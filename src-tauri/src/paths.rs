//! Куда приложение кладёт свои файлы (кэш стратегий, настройки).
//!
//! Раньше здесь использовался `tauri::api::path::app_config_dir(&Config::default())`,
//! но у `Config::default()` пустой `bundle.identifier`, поэтому путь получался
//! `%APPDATA%\strategies.json` — файл валялся прямо в корне Roaming рядом с
//! чужими приложениями. Теперь папка всегда `%APPDATA%\<APP_ID>`.

use std::path::PathBuf;

/// Должно совпадать с `tauri.bundle.identifier` в tauri.conf.json.
pub const APP_ID: &str = "com.vpnapp.client";

pub fn config_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.trim().is_empty() {
            return PathBuf::from(appdata).join(APP_ID);
        }
    }
    // Фолбэк для нестандартного окружения (например, запуск из сервиса).
    std::env::temp_dir().join(APP_ID)
}

/// Создаёт папку настроек и возвращает её путь.
pub fn ensure_config_dir() -> Result<PathBuf, String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("не создал {}: {e}", dir.display()))?;
    Ok(dir)
}
