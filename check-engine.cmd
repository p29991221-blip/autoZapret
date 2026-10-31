@echo off
rem Двойной клик — покажет, какая версия движка и стратегий установлена.
rem Отчёт дублируется в check-engine.txt рядом с этим файлом.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0check-engine.ps1"
