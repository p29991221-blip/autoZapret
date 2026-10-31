use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    pub autoconnect: bool,
    /// Стратегия, которая подключилась в прошлый раз. Её пробуем первой,
    /// чтобы не перебирать заново все 20+ вариантов.
    #[serde(default)]
    pub last_strategy_id: Option<String>,
}

fn path() -> PathBuf {
    crate::paths::config_dir().join("settings.json")
}

pub fn load() -> Settings {
    fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) -> Result<(), String> {
    crate::paths::ensure_config_dir()?;
    let content = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    fs::write(path(), content).map_err(|e| format!("не сохранил настройки: {e}"))
}

pub fn remember_strategy(id: &str) {
    let mut settings = load();
    if settings.last_strategy_id.as_deref() == Some(id) {
        return;
    }
    settings.last_strategy_id = Some(id.to_string());
    let _ = save(&settings);
}
