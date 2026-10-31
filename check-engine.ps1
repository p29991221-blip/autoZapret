param(
    # Запустить без ожидания Enter в конце (для вызова из другого скрипта).
    [switch]$NoPause
)

# Что реально установлено: версия движка, набор стратегий, хеши файлов.
#
# Двойной клик по check-engine.cmd — отчёт на экране и в check-engine.txt.
# Скрипт ничего не меняет и не требует прав администратора.

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$script:report = New-Object System.Collections.Generic.List[string]

function Say {
    param([string]$Text, [string]$Color = '')

    $script:report.Add($Text)
    if ($Color) { Write-Host $Text -ForegroundColor $Color } else { Write-Host $Text }
}

function Find-Engine {
    param([string]$Root)

    $candidates = @()
    if ($env:VPN_APP_ENGINE_DIR) { $candidates += $env:VPN_APP_ENGINE_DIR }
    $candidates += (Join-Path $Root 'src-tauri\engine')
    $candidates += (Join-Path $Root 'engine')
    $candidates += (Join-Path $Root 'src-tauri\target\release\engine')
    $candidates += (Join-Path $Root 'target\release\engine')

    foreach ($dir in $candidates) {
        if ($dir -and (Test-Path (Join-Path $dir 'bin\winws.exe'))) { return $dir }
    }
    return $null
}

$engine = Find-Engine -Root $root

Say ''
Say '=== Движок ===' 'Cyan'
if (-not $engine) {
    Say '  engine/ не найдена рядом со скриптом' 'Red'
} else {
    Say "  папка      : $engine"
    $winws = Join-Path $engine 'bin\winws.exe'
    $item = Get-Item $winws
    $hash = (Get-FileHash $winws -Algorithm SHA256).Hash.Substring(0, 16)
    Say "  winws.exe  : $($item.Length) байт, изменён $($item.LastWriteTime)"
    Say "  sha256     : $hash..."

    $log = Join-Path $engine 'engine.log'
    if (Test-Path $log) {
        $first = Get-Content $log -TotalCount 1
        Say "  последний запуск: $((Get-Item $log).LastWriteTime)"
        if ($first) { Say "  версия движка   : $first" }
    } else {
        Say '  engine.log нет — движок ещё не запускался'
    }

    $binCount = (Get-ChildItem (Join-Path $engine 'bin') -File | Measure-Object).Count
    $ipset = Join-Path $engine 'lists\ipset-all.txt'
    $ipsetCount = if (Test-Path $ipset) { (Get-Content $ipset | Measure-Object -Line).Lines } else { 0 }
    Say "  файлов в bin: $binCount, подсетей в ipset-all.txt: $ipsetCount"
}

Say ''
Say '=== Стратегии (кэш приложения) ===' 'Cyan'
$cache = Join-Path $env:APPDATA 'com.vpnapp.client\strategies.json'
if (-not (Test-Path $cache)) {
    Say '  кэша нет — приложение использует встроенный набор из 2 стратегий'
} else {
    $data = Get-Content $cache -Raw | ConvertFrom-Json
    Say "  файл      : $cache"
    Say "  изменён   : $((Get-Item $cache).LastWriteTime)"
    Say "  стратегий : $($data.strategies.Count)"
    if ($data.source_tag) {
        Say "  релиз     : $($data.source_tag)" 'Green'
    } else {
        Say '  релиз     : не записан (кэш создан до появления штампа — обновите стратегии)'
    }
}

Say ''
Say 'Как проверить, что обновление сработало:' 'Cyan'
Say '  1) нажмите «Обновить стратегии» в приложении;'
Say '  2) на кнопке появится тег релиза, а здесь — строка «релиз»;'
Say '  3) если тег/хеш/версия движка не изменились — значит новых релизов не было.'

# Тайминги последнего подключения: видно, сколько занял каждый шаг.
$connectLog = Join-Path $env:APPDATA 'com.vpnapp.client\connect.log'
Say ''
Say '=== Последнее подключение (connect.log) ===' 'Cyan'
if (Test-Path $connectLog) {
    Say "  файл: $connectLog (изменён $((Get-Item $connectLog).LastWriteTime))"
    Get-Content $connectLog -Tail 25 | ForEach-Object { Say "  $_" }
} else {
    Say '  пусто — приложение ещё не подключалось в этой сборке'
}

$outFile = Join-Path $root 'check-engine.txt'
try {
    Set-Content -Path $outFile -Value $script:report -Encoding UTF8
    Say ''
    Say "Отчёт сохранён: $outFile" 'Green'
} catch {
    Say ''
    Say "Не смог сохранить отчёт: $($_.Exception.Message)" 'Red'
}

if (-not $NoPause) {
    Write-Host ''
    [void](Read-Host 'Нажмите Enter, чтобы закрыть окно')
}
