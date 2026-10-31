@echo off
rem Запуск в dev-режиме. Приложение требует прав администратора (движку
rem winws.exe нужен драйвер WinDivert), поэтому батник сам перезапускается
rem через UAC, если открыт не от админа.

net session >nul 2>&1
if %errorlevel% neq 0 (
  echo Запрашиваю права администратора...
  powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
  exit /b
)

cd /d "%~dp0"
cargo tauri dev
pause
