//! Сборка приложения.
//!
//! Главное, что здесь делается — в exe вшивается манифест Windows с
//! `requireAdministrator` (фича `elevate`, включена по умолчанию).
//! Без этого приложение запускается от обычного пользователя, а `winws.exe`
//! (движок) сам требует админа: `CreateProcess` возвращает ошибку 740
//! (ERROR_ELEVATION_REQUIRED), движок не стартует, и обход DPI не работает
//! вообще. UAC-запрос при запуске — плата за это.
//!
//! Фичу можно выключить (`cargo test --no-default-features`), чтобы тестовый
//! бинарник не требовал UAC: сами тесты движок не запускают.

/// Манифест заменяет дефолтный из tauri-build, поэтому Common Controls v6
/// (нужен WebView2/диалогам) прописываем руками.
#[cfg(windows)]
const MANIFEST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"
      />
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">permonitorv2,permonitor</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
</assembly>"#;

fn main() {
    #[cfg(windows)]
    {
        let elevate = std::env::var_os("CARGO_FEATURE_ELEVATE").is_some();
        let mut windows = tauri_build::WindowsAttributes::new();

        if elevate {
            windows = windows.app_manifest(MANIFEST);
        }

        let attrs = tauri_build::Attributes::new().windows_attributes(windows);

        // Подсказка на самый частый сбой сборки: tauri-build копирует engine/bin
        // в target, а WinDivert64.sys бывает занят загруженным драйвером.
        if let Err(err) = tauri_build::try_build(attrs) {
            let text = format!("{err:#}");
            if text.contains("os error 32") || text.contains("WinDivert64.sys") {
                eprintln!(
                    "\n[сборка] Файл engine/bin/WinDivert64.sys занят загруженным драйвером.\n\
                     [сборка] Остановите драйвер в терминале от администратора:\n\
                     [сборка]     sc stop windivert\n\
                     [сборка] (или перезагрузите компьютер) и повторите сборку.\n"
                );
            }
            panic!("не удалось выполнить tauri-build: {text}");
        }
    }

    #[cfg(not(windows))]
    tauri_build::build();
}
