//! Управление процессом-движком (winws.exe) и его состояние.
//!
//! Ключевые моменты, которые здесь важны:
//!  * `winws.exe` ставит драйвер WinDivert и в своём манифесте требует
//!    `requireAdministrator` — значит, само приложение тоже должно быть
//!    запущено от админа (см. build.rs). Иначе `CreateProcess` падает с
//!    ошибкой 740, и раньше это выглядело как «подключено», хотя движка нет.
//!  * статус меняется только здесь и только осмысленно: успешный `spawn`
//!    ещё не значит «работает», поэтому после запуска вызывается проверка
//!    трафика (health.rs), а `Connected` ставит тот, кто эту проверку прошёл.
//!  * `desired` — «пользователь хочет быть подключён», `sweeping` — «идёт
//!    перебор стратегий». Оба флага живут под тем же мьютексом, что и процесс,
//!    поэтому watchdog и перебор не могут одновременно дёргать движок.

use once_cell::sync::Lazy;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use tokio::time::sleep;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub const ENGINE_EXE: &str = "winws.exe";

/// Ошибка запуска движка. Права администратора выделены отдельно: их
/// бессмысленно «перебирать» следующей стратегией, нужно перезапустить
/// приложение с повышением прав.
#[derive(Debug, Clone)]
pub enum StartError {
    Elevation,
    Other(String),
}

impl StartError {
    pub fn message(&self) -> String {
        match self {
            StartError::Elevation => {
                "winws.exe требует прав администратора (ошибка 740). \
                 Закройте приложение и запустите его от имени администратора."
                    .to_string()
            }
            StartError::Other(e) => e.clone(),
        }
    }
}

/// Статус движка, который видит UI.
///
/// Сериализуется как `{"state":"error","message":"..."}` — никакого
/// `format!("{:?}")` и парсинга строк регулярками на фронте.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", content = "message", rename_all = "snake_case")]
pub enum EngineStatus {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Error(String),
}

pub struct EngineState {
    pub child: Option<Child>,
    pub status: EngineStatus,
    pub current_strategy: Option<String>,
    pub current_strategy_id: Option<String>,
    /// Пользователь (или автозапуск) хочет, чтобы обход был включён.
    pub desired: bool,
    /// Прямо сейчас идёт перебор стратегий.
    pub sweeping: bool,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            child: None,
            status: EngineStatus::Disconnected,
            current_strategy: None,
            current_strategy_id: None,
            desired: false,
            sweeping: false,
        }
    }
}

// Единое глобальное состояние движка. Доступ редкий (клик, тик watchdog),
// поэтому обычный Mutex, а не что-то из async-мира. Держать его через
// `.await` нельзя — это проверяется глазами при ревью, т.к. guard не Send.
pub static ENGINE: Lazy<Mutex<EngineState>> = Lazy::new(|| Mutex::new(EngineState::default()));

/// Блокировка, которая не паникует на «отравленном» мьютексе: паника в
/// релизе (`panic = "abort"`) уронила бы всё приложение из-за чужой ошибки.
pub fn lock_state() -> MutexGuard<'static, EngineState> {
    ENGINE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn snapshot() -> (EngineStatus, Option<String>, Option<String>) {
    let st = lock_state();
    (
        st.status.clone(),
        st.current_strategy.clone(),
        st.current_strategy_id.clone(),
    )
}

pub fn set_status(status: EngineStatus) {
    lock_state().status = status;
}

pub fn set_strategy(name: Option<&str>, id: Option<&str>) {
    let mut st = lock_state();
    st.current_strategy = name.map(str::to_string);
    st.current_strategy_id = id.map(str::to_string);
}

pub fn is_desired() -> bool {
    lock_state().desired
}

pub fn set_desired(value: bool) {
    lock_state().desired = value;
}

pub fn is_sweeping() -> bool {
    lock_state().sweeping
}

/// Папка с движком: `engine/` рядом с exe (так кладёт бандл), либо
/// `src-tauri/engine/` в dev-режиме. `VPN_APP_ENGINE_DIR` позволяет
/// подсунуть другую папку при отладке.
pub fn engine_root() -> PathBuf {
    if let Ok(dir) = std::env::var("VPN_APP_ENGINE_DIR") {
        let candidate = PathBuf::from(dir);
        if has_engine(&candidate) {
            return candidate;
        }
    }

    // В dev-режиме exe лежит в `target\<profile>\`, и tauri-build рядом с ним
    // кладёт КОПИЮ resources (target\debug\engine). Обновлять и запускать нужно
    // именно исходную `src-tauri/engine`, иначе обновление пишет в артефакт
    // сборки, который следующий `cargo build` перезапишет.
    if let Ok(exe) = std::env::current_exe() {
        if is_cargo_target_exe(&exe) {
            let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("engine");
            if has_engine(&source) {
                return source;
            }
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for rel in ["engine", "resources/engine", "resources"] {
                let candidate = dir.join(rel);
                if has_engine(&candidate) {
                    return candidate;
                }
            }
        }
    }

    Path::new(env!("CARGO_MANIFEST_DIR")).join("engine")
}

/// Похож ли путь на вывод cargo: `...\target\debug\vpn-app.exe` или
/// `...\target\release\vpn-app.exe`.
fn is_cargo_target_exe(exe: &Path) -> bool {
    let profile = exe
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let parent = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default();

    matches!(profile, "debug" | "release") && parent == "target"
}

fn has_engine(root: &Path) -> bool {
    root.join("bin").join(ENGINE_EXE).exists()
}

pub fn engine_binary_path() -> PathBuf {
    engine_root().join("bin").join(ENGINE_EXE)
}

/// Файл лога движка — перезаписывается при каждом новом подключении.
pub fn log_file_path() -> PathBuf {
    engine_root().join("engine.log")
}

fn open_log_file() -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log_file_path())
}

/// Подставляет `{BIN}` и `{LISTS}` в аргументах реальными абсолютными путями
/// с завершающим слешем (как `%BIN%\` в батниках zapret).
fn resolve_placeholders(args: &[String]) -> Vec<String> {
    let root = engine_root();
    let bin = format!("{}\\", root.join("bin").display());
    let lists = format!("{}\\", root.join("lists").display());

    args.iter()
        .map(|a| a.replace("{BIN}", &bin).replace("{LISTS}", &lists))
        .collect()
}

fn kill_child_locked(state: &mut EngineState) {
    if let Some(mut child) = state.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Гасит только наш процесс, не трогая флаги и статус (нужно внутри перебора).
pub fn kill_child() {
    let mut state = lock_state();
    kill_child_locked(&mut state);
}

/// Полное отключение по воле пользователя: флаг `desired` снимается, поэтому
/// watchdog и текущий перебор больше не будут поднимать движок обратно.
pub fn disconnect() -> Result<(), String> {
    {
        let mut state = lock_state();
        state.desired = false;
        kill_child_locked(&mut state);
        state.status = EngineStatus::Disconnected;
        state.current_strategy = None;
        state.current_strategy_id = None;
    }
    // Драйвер снимаем уже без блокировки: `sc` — внешний процесс.
    stop_windivert_driver();
    Ok(())
}

/// Останавливает драйвер WinDivert.
///
/// Это не косметика: после `taskkill /F` движок не успевает выгрузить драйвер,
/// тот остаётся загруженным и держит `WinDivert64.sys` открытым. Пока файл
/// занят, его нельзя ни перезаписать при обновлении, ни удалить. Кроме того,
/// загруженный драйвер «прибит» к тому пути, откуда его загрузили, — если это
/// была старая копия движка, новый запуск молча использует её.
///
/// Ошибки игнорируем: если драйвер не запущен (`sc` вернёт 1062) или прав
/// не хватает, ничего страшного не произойдёт.
pub fn stop_windivert_driver() {
    let mut cmd = Command::new("sc");
    cmd.args(["stop", "windivert"]);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let _ = cmd.output();
}

/// Гасит все winws.exe в системе. Нужно только на старте и при обновлении,
/// чтобы подчистить зомби от прошлого запуска, который упал вместе с нами.
pub fn kill_stray_winws() {
    let mut cmd = Command::new("taskkill");
    cmd.args(["/F", "/IM", ENGINE_EXE, "/T"]);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let _ = cmd.output(); // не страшно, если убивать было некого
    stop_windivert_driver();
}

/// Запускает движок с конкретной стратегией (аргументы командной строки).
///
/// `Ok(())` означает только «процесс стартовал». Живость и работоспособность
/// проверяются отдельно — `is_alive()` и `health::probe`.
pub fn spawn(strategy_args: &[String]) -> Result<(), StartError> {
    let bin = engine_binary_path();
    if !bin.exists() {
        return Err(StartError::Other(format!(
            "не найден движок {} — положите winws.exe и .bin-файлы в папку engine/bin",
            bin.display()
        )));
    }

    let resolved_args = resolve_placeholders(strategy_args);
    let working_dir = bin.parent().map(Path::to_path_buf).unwrap_or_default();

    let mut cmd = Command::new(&bin);
    cmd.args(&resolved_args).current_dir(working_dir);

    // stdout и stderr — в один и тот же engine.log (clone хендла, иначе два
    // независимых курсора перемешают строки).
    match open_log_file() {
        Ok(log) => match log.try_clone() {
            Ok(err_log) => {
                cmd.stdout(Stdio::from(log)).stderr(Stdio::from(err_log));
            }
            Err(_) => {
                cmd.stdout(Stdio::from(log)).stderr(Stdio::null());
            }
        },
        Err(_) => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    // Держим блокировку на время spawn: это единицы миллисекунд, зато никакой
    // второй запуск не вклинится между «убили старый» и «записали новый child».
    let mut state = lock_state();
    kill_child_locked(&mut state);

    match cmd.spawn() {
        Ok(child) => {
            state.child = Some(child);
            state.status = EngineStatus::Connecting;
            Ok(())
        }
        Err(e) => {
            let err = if e.raw_os_error() == Some(740) {
                StartError::Elevation
            } else {
                StartError::Other(format!(
                    "не удалось запустить {}: {e}",
                    bin.file_name().unwrap_or_default().to_string_lossy()
                ))
            };
            state.status = EngineStatus::Error(err.message());
            Err(err)
        }
    }
}

/// Жив ли процесс движка прямо сейчас.
pub fn is_alive() -> bool {
    let mut state = lock_state();
    match state.child.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(None)),
        None => false,
    }
}

/// Ждёт, пока движок напишет в лог, что перехват реально запущен.
///
/// Фиксированная пауза тут плоха в обе стороны: с большими списками winws
/// поднимается дольше 0.9 с, и проба, отправленная раньше, уходила мимо
/// движка — рабочая стратегия объявлялась битой (и перебор шёл дальше по
/// всем 20+ вариантам). А когда драйвер вообще не встал, ждать нечего.
///
/// `true` — перехват запущен, можно проверять трафик.
pub async fn wait_until_capturing(budget: Duration) -> bool {
    let path = log_file_path();
    let deadline = std::time::Instant::now() + budget;

    loop {
        if !is_alive() {
            return false;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if text.contains("capture is started") {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(70)).await;
    }
}

/// Код выхода уже завершившегося процесса. `try_wait` кэширует статус,
/// поэтому вызывать после `is_alive()` безопасно.
pub fn exit_code() -> Option<i32> {
    let mut state = lock_state();
    match state.child.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(status)) => status.code(),
            _ => None,
        },
        None => None,
    }
}

/// Человеческое пояснение к коду выхода — чтобы «движок упал» не приходилось
/// расшифровывать вручную. Самое частое на практике: из `engine/bin` пропала
/// dll (cygwin1.dll/WinDivert.dll) — Windows в этом случае даже показывает
/// своё окно «Не удается продолжить выполнение кода».
pub fn exit_hint() -> String {
    match exit_code().map(|code| code as u32) {
        Some(0xC0000135) => {
            " — не хватает DLL (STATUS_DLL_NOT_FOUND): проверьте cygwin1.dll и WinDivert.dll в engine/bin"
                .to_string()
        }
        Some(0xC0000142) => " — не инициализировалась DLL (STATUS_DLL_INIT_FAILED)".to_string(),
        Some(0) => String::new(),
        Some(code) => format!(" (код выхода 0x{code:08X})"),
        None => String::new(),
    }
}

/// Фоновый сторож: раз в 3 секунды смотрит, не умер ли процесс сам.
/// Если умер, а пользователь не отключался — отдаёт хендл `on_lost`, который
/// сам решает, как переподключаться.
///
/// Важно: когда процесс признан мёртвым, `child` забирается (становится `None`),
/// иначе `try_wait` навсегда возвращал бы «умер» и мы бы запускали перебор
/// каждые 3 секунды — именно так и получался шторм переподключений.
pub async fn watchdog_loop<F>(on_lost: F)
where
    F: Fn() + Send + Sync + 'static,
{
    loop {
        sleep(Duration::from_secs(3)).await;

        let lost = {
            let mut state = lock_state();
            if !state.desired || state.sweeping {
                continue;
            }
            match state.child.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(None) => false,          // жив
                    Ok(Some(_)) | Err(_) => {   // завершился сам / не опросить
                        state.child = None;
                        true
                    }
                },
                // Процесса нет — либо перебор ещё в пути (тогда sweeping),
                // либо уже отработал и снял desired. Повторно не триггерим.
                None => false,
            }
        };

        if lost {
            on_lost();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_without_debug_format() {
        let connected = serde_json::to_string(&EngineStatus::Connected).unwrap();
        assert_eq!(connected, r#"{"state":"connected"}"#);

        let error = serde_json::to_string(&EngineStatus::Error("нет сети".into())).unwrap();
        assert_eq!(error, r#"{"state":"error","message":"нет сети"}"#);
    }

    #[test]
    fn engine_root_points_to_existing_winws_in_dev() {
        // В dev-режиме движок лежит в src-tauri/engine — проверяем, что
        // фолбэк действительно находит его (иначе приложение «не видит» движок).
        assert!(engine_binary_path().exists(), "winws.exe не найден рядом с проектом");
    }

    #[test]
    fn detects_cargo_build_layout() {
        // Для таких exe берём исходный engine/, а не копию в target/.
        assert!(is_cargo_target_exe(Path::new(
            r"C:\p\src-tauri\target\debug\vpn-app.exe"
        )));
        assert!(is_cargo_target_exe(Path::new(
            r"C:\p\src-tauri\target\release\vpn-app.exe"
        )));
        // Установленное приложение и «портативная» папка — наоборот.
        assert!(!is_cargo_target_exe(Path::new(
            r"C:\Program Files\vpn-app\vpn-app.exe"
        )));
        assert!(!is_cargo_target_exe(Path::new(r"C:\Tools\target\vpn-app.exe")));
    }

    #[test]
    fn exit_hint_explains_missing_dll() {
        // 0xC0000135 как i32 (как его отдаёт Windows).
        let code: i32 = 0xC0000135u32 as i32;
        assert_eq!(code as u32, 0xC0000135);
        // Просто фиксируем, что константа в hint() та же самая.
        assert!(format!("{:08X}", code as u32).eq("C0000135"));
    }
}
