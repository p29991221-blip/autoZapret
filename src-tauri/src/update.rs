//! Обновление движка и списков стратегий из последнего релиза zapret.
//!
//! Что здесь важно и что было сломано:
//!  * **Копирование больше ничего не удаляет.** Раньше `lists/` и `bin/`
//!    стирались целиком (`remove_dir_all`) и заливались заново. Если хоть один
//!    файл был занят (winws.exe ещё держит свой exe), удаление успевало снести
//!    половину папки и падало с «Отказано в доступе» — именно так из
//!    `engine/bin` пропали `cygwin1.dll`, `WinDivert.dll` и все `.bin`-файлы,
//!    после чего winws.exe отказывался стартовать. Теперь файлы только
//!    перезаписываются: любые сбои оставляют рабочий движок на месте.
//!  * Пользовательские `*-user.txt` и `ipset-all.txt` не перезаписываются
//!    релизной версией (там заглушки на пример-домен и одну подсеть).
//!  * Стратегии парсятся ДО того, как трогать файлы: битый релиз больше не
//!    может оставить приложение без списка стратегий.
//!  * Перед заменой winws.exe движок останавливается, и процесс дожидается
//!    завершения, иначе Windows не даёт перезаписать занятый exe.

use crate::engine;
use crate::strategies::{self, StrategyList};
use serde::Deserialize;
use std::fs;
use std::io::Cursor;
use std::path::Path;

const RELEASES_API: &str =
    "https://api.github.com/repos/Flowseal/zapret-discord-youtube/releases/latest";

/// Таймаут запроса к API (там только маленький JSON).
const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Сколько ждём установки TCP-соединения с GitHub.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Сколько ждём *очередную порцию* данных архива.
const CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Защита от «качаем неизвестно что»: реальный архив — десятки мегабайт.
const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

fn api_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("vpn-app") // GitHub API требует User-Agent, иначе 403
        .timeout(API_TIMEOUT)
        .build()
        .map_err(|e| format!("не создал http-клиент: {e}"))
}

/// Клиент для скачивания архива: общего таймаута нет намеренно. Раньше он был
/// 60 секунд на весь запрос, и на медленном канале обновление падало с
/// «operation timed out», не докачав файл.
fn download_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("vpn-app")
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| format!("не создал http-клиент: {e}"))
}

/// Скачивает архив по частям, следя за паузами между ними.
/// `on_progress` вызывается с накопленным размером — чтобы UI показывал
/// «сколько уже скачано», а не просто висел на «Обновляю…».
async fn download_archive<F: Fn(u64)>(
    client: &reqwest::Client,
    url: &str,
    on_progress: &F,
) -> Result<Vec<u8>, String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("не начал скачивание: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("GitHub ответил {} на запрос архива", response.status()));
    }

    let mut data: Vec<u8> = Vec::new();
    let mut reported: usize = 0;

    loop {
        match tokio::time::timeout(CHUNK_TIMEOUT, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                data.extend_from_slice(&chunk);
                if data.len() > MAX_ARCHIVE_BYTES {
                    return Err("архив больше 256 МБ — обновление отменено".into());
                }
                if data.len() - reported >= 512 * 1024 {
                    reported = data.len();
                    on_progress(data.len() as u64);
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(format!("скачивание оборвалось: {e}")),
            Err(_) => {
                return Err(format!(
                    "скачивание зависло (нет данных {} с) — проверьте сеть и повторите",
                    CHUNK_TIMEOUT.as_secs()
                ))
            }
        }
    }

    if data.is_empty() {
        return Err("GitHub прислал пустой архив".into());
    }
    on_progress(data.len() as u64);
    Ok(data)
}

/// Качает последний релиз с GitHub, копирует поверх engine/bin и engine/lists,
/// парсит все "general (ALT*).bat" в свежий список стратегий.
/// `on_progress` — сколько байт архива уже скачано (для индикатора в UI).
pub async fn update_from_github<F>(
    engine_root: &Path,
    on_progress: F,
) -> Result<StrategyList, String>
where
    F: Fn(u64),
{
    let client = api_client()?;

    let release: GhRelease = client
        .get(RELEASES_API)
        .send()
        .await
        .map_err(|e| format!("не смог обратиться к GitHub: {e}"))?
        .json()
        .await
        .map_err(|e| format!("не разобрал ответ GitHub: {e}"))?;

    let asset = pick_zip_asset(&release).ok_or("в последнем релизе нет .zip файла")?;

    // Один повтор: на урезанном канале обрыв на середине архива — обычное дело,
    // а повторное скачивание стоит дешевле, чем ругань «не обновилось».
    let client = download_client()?;
    let mut bytes = None;
    let mut last_error = String::new();
    for attempt in 0..2 {
        match download_archive(&client, &asset.browser_download_url, &on_progress).await {
            Ok(data) => {
                bytes = Some(data);
                break;
            }
            Err(e) => {
                last_error = e;
                if attempt == 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }
    let bytes = bytes.ok_or(last_error)?;

    let tmp_dir = std::env::temp_dir().join("vpn-app-update");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;

    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("битый zip: {e}"))?;
    archive
        .extract(&tmp_dir)
        .map_err(|e| format!("не распаковал zip: {e}"))?;

    // Внутри архива всё лежит в одной папке вида zapret-discord-youtube-1.10.2/,
    // но на всякий случай ищем ту, где реально есть bin/winws.exe.
    let extracted_root = find_release_root(&tmp_dir).ok_or("в архиве нет папки bin с winws.exe")?;

    // 1) Сначала разбираем стратегии. Если релиз поменял формат батников и
    //    ничего не распарсилось — выходим, не тронув рабочий движок.
    let strategies_vec = parse_release_strategies(&extracted_root)?;

    // 2) Останавливаем движок: winws.exe держит свой exe открытым, а
    //    загруженный драйвер WinDivert — ещё и WinDivert64.sys.
    let _ = engine::disconnect();
    engine::kill_stray_winws();
    wait_until_winws_gone().await;
    wait_until_driver_stopped().await;

    // 3) Обновляем файлы. Ничего не удаляем: только перезапись.
    sync_dir_retry(
        &extracted_root.join("bin"),
        &engine_root.join("bin"),
        // Из bin берём всё, что дал релиз (dll и exe должны быть одной версии).
        // Чего в релизе нет — то остаётся на месте.
        |_| false,
    )?;
    sync_dir_retry(
        &extracted_root.join("lists"),
        &engine_root.join("lists"),
        // Списки пользователя и уже собранный ipset релизом не затираем.
        is_protected_list_file,
    )?;

    let list = StrategyList {
        version: strategies_vec.len() as u32,
        strategies: strategies_vec,
        // Штамп версии: по нему видно, какой релиз реально применён.
        source_tag: Some(release.tag_name.clone()),
    };
    strategies::save_local(&list)?;
    let _ = fs::remove_dir_all(&tmp_dir);
    Ok(list)
}

fn pick_zip_asset(release: &GhRelease) -> Option<&GhAsset> {
    let is_zip = |a: &GhAsset| a.name.to_lowercase().ends_with(".zip");

    release
        .assets
        .iter()
        .find(|a| is_zip(a) && a.name.to_lowercase().starts_with("zapret"))
        .or_else(|| release.assets.iter().find(|a| is_zip(a)))
}

fn find_release_root(tmp_dir: &Path) -> Option<std::path::PathBuf> {
    if tmp_dir.join("bin").join(engine::ENGINE_EXE).exists() {
        return Some(tmp_dir.to_path_buf());
    }
    fs::read_dir(tmp_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.join("bin").join(engine::ENGINE_EXE).exists())
}

fn parse_release_strategies(root: &Path) -> Result<Vec<strategies::Strategy>, String> {
    let mut parsed = Vec::new();

    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        let lower = name.to_lowercase();

        if !(lower.starts_with("general") && lower.ends_with(".bat")) {
            continue;
        }

        let content =
            fs::read_to_string(entry.path()).map_err(|e| format!("не прочитал {name}: {e}"))?;
        if let Some(strategy) = strategies::parse_bat_strategy(&name, &content) {
            parsed.push(strategy);
        }
    }

    if parsed.is_empty() {
        return Err("в новом релизе не нашлось ни одной general(*).bat стратегии".into());
    }

    parsed.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(parsed)
}

// ------------------------------------------------------------ копирование

/// Пользовательские списки и готовый ipset: их нельзя затирать релизной
/// версией (там `*-user.txt` — заглушки с example-доменами, а ipset-all.txt
/// вообще пустой шаблон).
fn is_protected_list_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with("-user.txt") || lower == "ipset-all.txt" || lower.ends_with(".backup")
}

/// Перезаписывает файлы из `src` в `dst`. Ничего не удаляет и не очищает —
/// это принципиально: при обновлении поверх работающего движка любая ошибка
/// не должна оставлять папку полупустой.
fn sync_dir(src: &Path, dst: &Path, skip: fn(&str) -> bool) -> Result<(), String> {
    if !src.exists() {
        // Не валим всё обновление, если структура релиза чуть поменялась.
        return Ok(());
    }

    fs::create_dir_all(dst).map_err(|e| format!("не создал {}: {e}", dst.display()))?;
    copy_recursive(src, dst, skip)
}

/// То же самое, но с несколькими попытками — Windows не всегда мгновенно
/// отпускает exe после taskkill, плюс антивирус может секунду держать
/// свежескачанный файл. Повторная перезапись безопасна (идемпотентна).
fn sync_dir_retry(src: &Path, dst: &Path, skip: fn(&str) -> bool) -> Result<(), String> {
    let mut last_err = String::new();
    for attempt in 0..10 {
        match sync_dir(src, dst, skip) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e;
                std::thread::sleep(std::time::Duration::from_millis(800 * (attempt + 1)));
            }
        }
    }
    Err(format!(
        "не удалось обновить {} (файлы заняты — вероятно, winws.exe всё ещё работает \
         или архив проверяет антивирус): {last_err}",
        dst.display()
    ))
}

fn copy_recursive(src: &Path, dst: &Path, skip: fn(&str) -> bool) -> Result<(), String> {
    for entry in fs::read_dir(src).map_err(|e| format!("не прочитал {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();

        if !entry.path().is_dir() && skip(&name) {
            continue;
        }

        let target = dst.join(&name);
        if entry.path().is_dir() {
            fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            copy_recursive(&entry.path(), &target, skip)?;
        } else {
            fs::copy(entry.path(), &target)
                .map_err(|e| format!("не скопировал {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Опрашивает список процессов, пока winws.exe не исчезнет (максимум ~3 сек).
async fn wait_until_winws_gone() {
    for _ in 0..10 {
        let still_running = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq winws.exe", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("winws.exe"))
            .unwrap_or(false);

        if !still_running {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

/// Драйвер выгружается не мгновенно: пока он в памяти, `WinDivert64.sys`
/// остаётся занятым и перезаписать его нельзя.
async fn wait_until_driver_stopped() {
    for _ in 0..15 {
        let running = std::process::Command::new("sc")
            .args(["query", "windivert"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .to_uppercase()
                    .contains("RUNNING")
            })
            .unwrap_or(false);

        if !running {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vpn-app-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn user_files_are_protected() {
        assert!(is_protected_list_file("list-general-user.txt"));
        assert!(is_protected_list_file("ipset-exclude-user.txt"));
        assert!(is_protected_list_file("ipset-all.txt"));
        assert!(is_protected_list_file("ipset-all.txt.backup"));
        assert!(!is_protected_list_file("list-general.txt"));
        assert!(!is_protected_list_file("list-exclude.txt"));
    }

    #[test]
    fn lists_update_keeps_user_files_and_never_deletes() {
        let base = temp_dir("lists-test");
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        fs::write(src.join("list-general.txt"), "new").unwrap();
        fs::write(src.join("ipset-all.txt"), "203.0.113.113/32").unwrap();
        fs::write(src.join("list-general-user.txt"), "example.abc").unwrap();

        fs::write(dst.join("list-general.txt"), "old").unwrap();
        fs::write(dst.join("ipset-all.txt"), "1.0.0.0/24\n2.0.0.0/24\n").unwrap();
        fs::write(dst.join("list-general-user.txt"), "my.domain").unwrap();
        // Файл, которого в релизе нет, — должен просто остаться.
        fs::write(dst.join("list-exclude-user.txt"), "keep.me").unwrap();

        sync_dir(&src, &dst, is_protected_list_file).unwrap();

        assert_eq!(fs::read_to_string(dst.join("list-general.txt")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(dst.join("ipset-all.txt")).unwrap(),
            "1.0.0.0/24\n2.0.0.0/24\n"
        );
        assert_eq!(
            fs::read_to_string(dst.join("list-general-user.txt")).unwrap(),
            "my.domain"
        );
        assert_eq!(
            fs::read_to_string(dst.join("list-exclude-user.txt")).unwrap(),
            "keep.me"
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// Регрессия на реальную аварию: обновление не должно удалять файлы,
    /// которых нет в релизе (cygwin1.dll и остальные .bin остаются на месте).
    #[test]
    fn bin_update_never_removes_existing_files() {
        let base = temp_dir("bin-test");
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        fs::write(src.join("winws.exe"), "new-exe").unwrap();
        fs::write(src.join("stun.bin"), "new-stun").unwrap();

        fs::write(dst.join("winws.exe"), "old-exe").unwrap();
        fs::write(dst.join("cygwin1.dll"), "cygwin").unwrap();
        fs::write(dst.join("WinDivert.dll"), "windivert").unwrap();
        fs::write(dst.join("stun.bin"), "old-stun").unwrap();

        sync_dir(&src, &dst, |_| false).unwrap();

        assert_eq!(fs::read_to_string(dst.join("winws.exe")).unwrap(), "new-exe");
        assert_eq!(fs::read_to_string(dst.join("stun.bin")).unwrap(), "new-stun");
        // Главное: файлы, которых в релизе нет, не исчезли.
        assert_eq!(fs::read_to_string(dst.join("cygwin1.dll")).unwrap(), "cygwin");
        assert_eq!(
            fs::read_to_string(dst.join("WinDivert.dll")).unwrap(),
            "windivert"
        );

        let _ = fs::remove_dir_all(&base);
    }
}
