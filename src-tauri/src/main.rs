#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Трей-клиент обхода DPI.
//!
//! Здесь живёт вся логика «подключения»: перебор стратегий, проверка того,
//! что стратегия реально работает, авто-переподключение и команды для UI.
//!
//! Раньше подключение считалось успешным, если процесс не упал за 1.5 сек.
//! На практике `winws.exe` при нехватке прав/битой стратегии либо вообще
//! не стартует (ошибка 740), либо стартует и молча не пробивает DPI — поэтому
//! UI показывал «Подключено» при полностью нерабочем обходе. Теперь успех —
//! это пройденный HTTPS-запрос через движок (см. health.rs).

mod autostart;
mod engine;
mod health;
mod paths;
mod settings;
mod strategies;
mod update;

use engine::{EngineStatus, StartError};
use once_cell::sync::Lazy;
use std::io::Write;
use std::time::{Duration, Instant};
use strategies::{fallback_strategies, load_local, Strategy, StrategyList};
use tauri::{
    CustomMenuItem, Manager, SystemTray, SystemTrayEvent, SystemTrayMenu, SystemTrayMenuItem,
};

// ------------------------------------------------------------- диагностика

/// Момент старта приложения — от него считаем тайминги в connect.log.
static T0: Lazy<Instant> = Lazy::new(Instant::now);

/// Лог подключения: пишем в файл, потому что «кнопка долго думает» иначе
/// не продиагностировать — консоли у релизной сборки нет.
///
/// Файл: `%APPDATA%\com.vpnapp.client\connect.log`.
fn connect_log(message: impl AsRef<str>) {
    let path = paths::config_dir().join("connect.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            file,
            "[+{:6.2}s] {}",
            T0.elapsed().as_secs_f64(),
            message.as_ref()
        );
    }
}

/// Сколько ждём, пока движок напишет в лог «capture is started».
/// Обычно это 0.2–0.6 с; 2 с — запас на большие списки.
const READY_BUDGET: Duration = Duration::from_secs(2);
/// Таймаут проверки одной стратегии. Именно он определяет, сколько ждать,
/// пока перебор дойдёт до рабочего варианта: заблокированный домен не отвечает
/// вообще, и запрос висит до таймаута. Рабочий обход после «capture is started»
/// отвечает за 0.3–1.0 с, так что 2 секунды — с двукратным запасом.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Как часто проверять, что уже подключённая стратегия всё ещё пробивает DPI.
const HEALTH_INTERVAL: Duration = Duration::from_secs(90);
/// Столько подряд неудачных проверок — и переключаемся на другую стратегию.
/// Больше одного нужно, чтобы не дёргаться из-за случайного таймаута.
const HEALTH_MAX_FAILURES: u32 = 3;

// ---------------------------------------------------------------- состояние

fn status_json() -> serde_json::Value {
    let (status, strategy, strategy_id) = engine::snapshot();
    serde_json::json!({
        "status": status,
        "strategy": strategy,
        "strategyId": strategy_id,
    })
}

/// Отправляет статус в UI. Единственное место, откуда фронт узнаёт о смене
/// состояния — опроса по таймеру нет.
fn emit(app: &tauri::AppHandle) {
    let _ = app.emit_all("engine-status", status_json());
}

fn publish(app: &tauri::AppHandle, status: EngineStatus, name: Option<&str>, id: Option<&str>) {
    engine::set_status(status);
    engine::set_strategy(name, id);
    emit(app);
}

/// Пока guard жив, `sweeping == true`, т.е. второй перебор стратегий не
/// начнётся. Drop снимает флаг даже при выходе по ошибке.
struct SweepGuard;

impl SweepGuard {
    fn begin() -> Option<Self> {
        let mut st = engine::lock_state();
        if st.sweeping {
            return None;
        }
        st.sweeping = true;
        st.desired = true;
        Some(SweepGuard)
    }
}

impl Drop for SweepGuard {
    fn drop(&mut self) {
        engine::lock_state().sweeping = false;
    }
}

// -------------------------------------------------------------- подключение

fn current_strategy_list() -> StrategyList {
    let local = load_local();
    if local.strategies.is_empty() {
        fallback_strategies()
    } else {
        local
    }
}

async fn connect(app: tauri::AppHandle) -> Result<(), String> {
    connect_skipping(app, None).await
}

/// Перебирает стратегии по очереди, пока одна не пройдёт проверку трафика.
/// `skip_id` — стратегия, которую надо попробовать последней (её только что
/// признали нерабочей).
async fn connect_skipping(app: tauri::AppHandle, skip_id: Option<String>) -> Result<(), String> {
    let Some(_guard) = SweepGuard::begin() else {
        // Перебор уже идёт. Второй только мешал бы: два перебора убивали бы
        // процессы друг друга и оба считали бы стратегии битыми.
        return Ok(());
    };

    let list = current_strategy_list();
    if list.strategies.is_empty() {
        engine::set_desired(false);
        let msg = "Нет ни одной стратегии — нажмите «Обновить стратегии»".to_string();
        publish(&app, EngineStatus::Error(msg.clone()), None, None);
        return Err(msg);
    }

    publish(&app, EngineStatus::Connecting, None, None);

    let mut ordered: Vec<&Strategy> = list.strategies.iter().collect();
    let preferred = settings::load().last_strategy_id;

    // Сначала — стратегия, сработавшая в прошлый раз: перебор всех 20+ вариантов
    // по несколько секунд на каждый занимает больше минуты.
    if let Some(preferred) = preferred.as_deref() {
        ordered.sort_by_key(|s| s.id != preferred);
    }

    if let Some(skip) = skip_id.as_deref() {
        // Сортировка стабильная: отвалившаяся стратегия уедет в конец,
        // а «любимая» останется первой.
        ordered.sort_by_key(|s| s.id == skip);
    }

    let mut last_error = String::new();
    let total = ordered.len();
    let sweep_started = std::time::Instant::now();

    connect_log(format!(
        "перебор: {} стратегий, первая={:?}, пропускаем={:?}",
        total, preferred, skip_id
    ));

    for (index, strategy) in ordered.into_iter().enumerate() {
        // Пользователь мог нажать «Отключить» посреди перебора.
        if !engine::is_desired() {
            return Ok(());
        }

        // Счётчик и время в подписи: иначе перебор выглядит как зависшее окно.
        let label = format!(
            "{} · {}/{} · {} с",
            strategy.name,
            index + 1,
            total,
            sweep_started.elapsed().as_secs()
        );
        publish(
            &app,
            EngineStatus::Connecting,
            Some(&label),
            Some(&strategy.id),
        );

        let attempt_started = std::time::Instant::now();
        connect_log(format!("[{}/{}] {} — запуск", index + 1, total, strategy.id));

        match engine::spawn(&strategy.args) {
            Ok(()) => {}
            Err(StartError::Elevation) => {
                // Перебирать дальше бессмысленно: права нужны любой стратегии.
                let msg = StartError::Elevation.message();
                engine::set_desired(false);
                publish(&app, EngineStatus::Error(msg.clone()), None, None);
                return Err(msg);
            }
            Err(err) => {
                connect_log(format!("  spawn не удался: {}", err.message()));
                last_error = err.message();
                continue;
            }
        }

        // Ждём не «наугад 0.9 с», а строчку в логе движка о запущенном
        // перехвате: с большими списками он поднимается дольше, и проба,
        // ушедшая раньше, объявляла рабочую стратегию битой.
        let ready = engine::wait_until_capturing(READY_BUDGET).await;

        if !engine::is_desired() {
            engine::kill_child();
            return Ok(());
        }

        if !engine::is_alive() {
            // Код выхода подскажет, если из engine/bin пропала dll.
            let hint = engine::exit_hint();
            last_error = format!(
                "{}: движок завершился сразу после запуска{hint}",
                strategy.name
            );
            connect_log(format!(
                "  процесс умер за {:.2}s: {}",
                attempt_started.elapsed().as_secs_f64(),
                last_error
            ));
            engine::kill_child();
            continue;
        }

        if !ready {
            // Лог не подтвердил перехват (winws может буферизовать вывод).
            // Стратегию не отбрасываем: окончательно решает проверка трафика.
            connect_log("  перехват не подтверждён логом — проверяю трафик всё равно");
        } else {
            connect_log(format!(
                "  перехват поднят за {:.2}s, проверяю трафик",
                attempt_started.elapsed().as_secs_f64()
            ));
        }

        // Проверяем, что через движок реально открывается YouTube. Живого
        // процесса недостаточно: winws работает и тогда, когда не пробивает DPI.
        let report = health::probe(PROBE_TIMEOUT).await;

        if report.is_working() {
            // Запоминаем удачную стратегию, чтобы в следующий раз не перебирать.
            settings::remember_strategy(&strategy.id);
            connect_log(format!(
                "  УСПЕХ за {:.2}s (ok={:?})",
                attempt_started.elapsed().as_secs_f64(),
                report.ok
            ));
            publish(
                &app,
                EngineStatus::Connected,
                Some(&strategy.name),
                Some(&strategy.id),
            );
            return Ok(());
        }

        if !engine::is_desired() {
            engine::kill_child();
            return Ok(());
        }

        connect_log(format!(
            "  провал за {:.2}s, не открылось: {}",
            attempt_started.elapsed().as_secs_f64(),
            report.failed_list()
        ));

        last_error = format!(
            "{}: не открывается {} — DPI не обойдён",
            strategy.name,
            report.failed_list()
        );
        engine::kill_child();
    }

    engine::set_desired(false);
    connect_log(format!("перебор закончен без результата: {last_error}"));
    let msg = if last_error.is_empty() {
        "Ни одна стратегия не сработала".to_string()
    } else {
        format!("Ни одна стратегия не сработала. {last_error}")
    };
    publish(&app, EngineStatus::Error(msg.clone()), None, None);
    Err(msg)
}

fn spawn_connect(app: &tauri::AppHandle, skip_id: Option<String>) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = connect_skipping(handle, skip_id).await;
    });
}

// ------------------------------------------------------------------- логи

fn open_logs() -> Result<(), String> {
    let path = engine::log_file_path();
    if !path.exists() {
        std::fs::write(&path, "Логов пока нет — движок ещё не запускался.\n")
            .map_err(|e| format!("не создал {}: {e}", path.display()))?;
    }
    std::process::Command::new("notepad")
        .arg(&path)
        .spawn()
        .map_err(|e| format!("не открыл лог: {e}"))?;
    Ok(())
}

// ------------------------------------------------------------------- трей

fn handle_tray(app: &tauri::AppHandle, id: &str) {
    match id {
        "show" => show_window(app),
        "toggle" => {
            if engine::is_desired() {
                let _ = engine::disconnect();
                publish(app, EngineStatus::Disconnected, None, None);
            } else {
                spawn_connect(app, None);
            }
        }
        "logs" => {
            let _ = open_logs();
        }
        "quit" => {
            // Гасим дочерний процесс, иначе он переживёт приложение.
            let _ = engine::disconnect();
            std::process::exit(0);
        }
        _ => {}
    }
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

// --------------------------------------------------------------- tauri API

#[tauri::command]
async fn cmd_connect(app: tauri::AppHandle) -> Result<(), String> {
    connect(app).await
}

#[tauri::command]
fn cmd_disconnect(app: tauri::AppHandle) -> Result<(), String> {
    engine::disconnect()?;
    publish(&app, EngineStatus::Disconnected, None, None);
    Ok(())
}

/// Ждёт, пока идущий перебор стратегий закончится (он может быть внутри
/// проверки трафика и удерживать движок).
async fn wait_sweep_idle(limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while engine::is_sweeping() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    true
}

#[tauri::command]
async fn cmd_update_strategies(
    app: tauri::AppHandle,
) -> Result<strategies::StrategyList, String> {
    let was_on = engine::is_desired();

    // 1) Гасим обход и ждём, пока перебор стратегий отпустит движок. Иначе он
    // успеет запустить новый winws.exe прямо посреди обновления — и Windows
    // не даст перезаписать занятые файлы.
    let _ = engine::disconnect();
    publish(&app, EngineStatus::Disconnected, None, None);

    if !wait_sweep_idle(Duration::from_secs(10)).await {
        if was_on {
            spawn_connect(&app, None);
        }
        return Err("идёт переподключение — повторите обновление через несколько секунд".into());
    }

    // 2) Держим флаг перебора на всё время обновления, чтобы сторож процесса
    // и сторож трафика не подняли движок обратно.
    let Some(guard) = SweepGuard::begin() else {
        if was_on {
            spawn_connect(&app, None);
        }
        return Err("идёт переподключение — повторите обновление через несколько секунд".into());
    };
    engine::set_desired(false);

    connect_log("обновление: старт");
    let progress_app = app.clone();
    let result = update::update_from_github(&engine::engine_root(), move |bytes| {
        // Прогресс скачивания в UI: без него кнопка «залипает» на минуты.
        let _ = progress_app.emit_all(
            "update-progress",
            serde_json::json!({ "bytes": bytes }),
        );
    })
    .await;

    match &result {
        Ok(list) => connect_log(format!(
            "обновление: готово, тег={:?}, стратегий={}",
            list.source_tag,
            list.strategies.len()
        )),
        Err(e) => connect_log(format!("обновление: ошибка {e}")),
    }

    // 3) Отпускаем флаг и, если обход был включён, поднимаем его заново.
    drop(guard);
    if was_on {
        spawn_connect(&app, None);
    }
    result
}

#[tauri::command]
fn cmd_get_status() -> serde_json::Value {
    status_json()
}

#[tauri::command]
fn cmd_open_logs() -> Result<(), String> {
    open_logs()
}

#[tauri::command]
fn cmd_get_autostart() -> bool {
    autostart::is_enabled()
}

#[tauri::command]
fn cmd_set_autostart(enabled: bool) -> Result<(), String> {
    autostart::set_enabled(enabled)
}

#[tauri::command]
fn cmd_get_autoconnect() -> bool {
    settings::load().autoconnect
}

#[tauri::command]
fn cmd_set_autoconnect(enabled: bool) -> Result<(), String> {
    // Читаем-меняем-пишем: в настройках есть ещё и запомненная стратегия.
    let mut current = settings::load();
    current.autoconnect = enabled;
    settings::save(&current)
}

fn main() {
    let tray_menu = SystemTrayMenu::new()
        .add_item(CustomMenuItem::new("show", "Открыть"))
        .add_item(CustomMenuItem::new("toggle", "Подключить / Отключить"))
        .add_item(CustomMenuItem::new("logs", "Показать логи"))
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(CustomMenuItem::new("quit", "Выход"));

    // Иконку трея Tauri берёт из `tauri.systemTray.iconPath` в tauri.conf.json
    // (она вшивается в exe на этапе сборки), поэтому тут её не задаём.
    let tray = SystemTray::new()
        .with_menu(tray_menu)
        .with_tooltip("vpn-app — обход DPI");

    tauri::Builder::default()
        .system_tray(tray)
        .on_system_tray_event(|app, event| match event {
            SystemTrayEvent::MenuItemClick { id, .. } => handle_tray(app, &id),
            SystemTrayEvent::LeftClick { .. } | SystemTrayEvent::DoubleClick { .. } => {
                show_window(app)
            }
            _ => {}
        })
        .on_window_event(|event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event.event() {
                // Приложение живёт в трее: крестик сворачивает окно, а не
                // убивает обход. Выход — через меню трея.
                api.prevent_close();
                let _ = event.window().hide();
            }
        })
        .setup(|app| {
            // Свежий диагностический лог на каждый запуск приложения.
            let _ = std::fs::write(paths::config_dir().join("connect.log"), "");
            connect_log("приложение запущено");

            // Подчищаем зомби winws.exe от предыдущего запуска: они держат
            // фильтр WinDivert, и новый процесс падает с «already running».
            engine::kill_stray_winws();

            if settings::load().autoconnect {
                spawn_connect(&app.handle(), None);
            }

            // Сторож процесса: движок умер сам (не по кнопке) — переподключаемся,
            // начав с другой стратегии.
            let watchdog_handle = app.handle();
            tauri::async_runtime::spawn(async move {
                let handle = watchdog_handle.clone();
                engine::watchdog_loop(move || {
                    if !engine::is_desired() {
                        return;
                    }
                    // lock_state() тут отпускается до вызова connect_skipping,
                    // поэтому дедлока нет.
                    let skip = engine::lock_state().current_strategy_id.clone();
                    publish(&handle, EngineStatus::Reconnecting, None, None);
                    spawn_connect(&handle, skip);
                })
                .await;
            });

            // Сторож трафика: процесс жив, но DPI перестал пробиваться — самое
            // частое на практике (провайдер поменял правила). Проверяем редко,
            // чтобы не грузить машину и не мешать играм.
            let health_handle = app.handle();
            tauri::async_runtime::spawn(async move {
                let mut failures = 0u32;
                loop {
                    tokio::time::sleep(HEALTH_INTERVAL).await;

                    let (status, _, skip) = engine::snapshot();
                    if !matches!(status, EngineStatus::Connected)
                        || engine::is_sweeping()
                        || !engine::is_desired()
                    {
                        failures = 0;
                        continue;
                    }

                    if health::probe(PROBE_TIMEOUT).await.is_working() {
                        failures = 0;
                        continue;
                    }

                    failures += 1;
                    if failures >= HEALTH_MAX_FAILURES {
                        failures = 0;
                        publish(&health_handle, EngineStatus::Reconnecting, None, None);
                        spawn_connect(&health_handle, skip);
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cmd_connect,
            cmd_disconnect,
            cmd_update_strategies,
            cmd_get_status,
            cmd_get_autostart,
            cmd_set_autostart,
            cmd_get_autoconnect,
            cmd_set_autoconnect,
            cmd_open_logs
        ])
        .run(tauri::generate_context!())
        .expect("ошибка запуска tauri-приложения");
}
