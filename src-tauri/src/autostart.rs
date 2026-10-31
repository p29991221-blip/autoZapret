//! Автозапуск с Windows через ключ `HKCU\...\Run`.
//!
//! `reg add` умеет завершиться с ошибкой (например, политика запрещает запись),
//! а старый код молча возвращал Ok — тумблер в UI включался, но при следующей
//! загрузке ничего не запускалось. Теперь результат команды проверяется.

use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "vpn-app";

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("не определил путь к exe: {e}"))?;
        let exe_str = format!("\"{}\"", exe.display());
        run_reg(&[
            "add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &exe_str, "/f",
        ])
        .and_then(|ok| {
            if ok {
                Ok(())
            } else {
                Err("Windows не дал прописать автозапуск в реестр".to_string())
            }
        })
    } else if !is_enabled() {
        // Записи и так нет — выключать нечего, это не ошибка.
        Ok(())
    } else {
        run_reg(&["delete", RUN_KEY, "/v", VALUE_NAME, "/f"]).and_then(|ok| {
            if ok {
                Ok(())
            } else {
                Err("Windows не дал удалить запись автозапуска".to_string())
            }
        })
    }
}

pub fn is_enabled() -> bool {
    run_reg(&["query", RUN_KEY, "/v", VALUE_NAME]).unwrap_or(false)
}

/// Возвращает `true`, если `reg` завершился успешно.
fn run_reg(args: &[&str]) -> Result<bool, String> {
    let mut cmd = Command::new("reg");
    cmd.args(args);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = cmd
        .output()
        .map_err(|e| format!("не удалось вызвать reg: {e}"))?;
    Ok(output.status.success())
}
