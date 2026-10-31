use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Strategy {
    pub id: String,
    pub name: String,
    /// аргументы командной строки для движка (winws/zapret-подобного)
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StrategyList {
    pub version: u32,
    pub strategies: Vec<Strategy>,
    /// Тег релиза GitHub, из которого приехал список (например "1.10.2").
    /// `None` — либо это встроенный набор, либо обновление было до появления
    /// поля; serde-дефолт держит старые кэши читаемыми.
    #[serde(default)]
    pub source_tag: Option<String>,
}

fn cache_path() -> PathBuf {
    // Раньше тут был `app_config_dir(&Config::default())` — у дефолтного
    // конфига пустой identifier, и кэш падал прямо в %APPDATA%\strategies.json.
    crate::paths::config_dir().join("strategies.json")
}

/// Читает локально закэшированный список стратегий (чтобы приложение
/// работало сразу после установки, даже без интернета до первого коннекта)
pub fn load_local() -> StrategyList {
    let path = cache_path();
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => StrategyList::default(),
    }
}

pub fn save_local(list: &StrategyList) -> Result<(), String> {
    crate::paths::ensure_config_dir()?;
    let content = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
    fs::write(cache_path(), content).map_err(|e| e.to_string())
}

/// Разбирает один .bat файл вида "general (ALTxx).bat" в стратегию.
///
/// Что тут важно (всё это раньше ломалось):
///  * в батниках zapret встречается и `"%BIN%\winws.exe"`, и `%BIN%\winws.exe`,
///    поэтому ищем просто `winws.exe`, а кавычку после него съедаем, если она есть;
///  * `%BIN%\` и `%LISTS%\` заменяем вместе со слешем, иначе в путях получался
///    двойной `\\`;
///  * опции с неизвестными переменными (`%GameFilterTCP%` и т.п.) выбрасываем,
///    а блоки `--new`, оставшиеся без фильтра, — целиком: winws такой профиль
///    не примет и упадёт на старте.
pub fn parse_bat_strategy(file_name: &str, content: &str) -> Option<Strategy> {
    let cmd = extract_command_line(content)?;
    let args = build_args(&cmd);
    if args.is_empty() {
        return None;
    }

    let name = file_name
        .strip_suffix(".bat")
        .or_else(|| file_name.strip_suffix(".BAT"))
        .unwrap_or(file_name)
        .trim()
        .to_string();

    if name.is_empty() {
        return None;
    }

    let id = name
        .to_lowercase()
        .replace(' ', "_")
        .replace(['(', ')'], "");

    Some(Strategy { id, name, args })
}

/// Достаёт из батника командную строку запуска winws.exe вместе с
/// продолжениями строк через `^`. Первое вхождение `winws.exe`, после которого
/// реально идут опции (`--...`), считается запуском; `taskkill ... winws.exe`
/// в начале файла так отсеивается.
fn extract_command_line(content: &str) -> Option<String> {
    let marker = "winws.exe";
    let mut from = 0;

    while let Some(rel) = content[from..].find(marker) {
        let after_name = from + rel + marker.len();
        let rest = &content[after_name..];
        let rest = rest.strip_prefix('"').unwrap_or(rest);

        let cmd = join_continuations(rest);
        if cmd.contains("--") {
            return Some(cmd);
        }
        from = after_name;
    }
    None
}

fn join_continuations(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        let continues = trimmed.ends_with('^');
        let piece = trimmed.trim_end_matches('^').trim();
        if !piece.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(piece);
        }
        if !continues {
            break;
        }
    }
    out
}

fn substitute(cmd: &str) -> String {
    // Именно в таком порядке: сначала вариант со слешем, иначе получим `\\`.
    cmd.replace("%BIN%\\", "{BIN}")
        .replace("%LISTS%\\", "{LISTS}")
        .replace("%BIN%", "{BIN}")
        .replace("%LISTS%", "{LISTS}")
}

fn build_args(command: &str) -> Vec<String> {
    let command = substitute(command);
    let mut result: Vec<String> = Vec::new();

    for (index, segment) in command.split("--new").enumerate() {
        let mut args: Vec<String> = split_args(segment)
            .into_iter()
            // Сначала чистим глобальные `--wf-*` (там %GameFilter*% дописывал
            // порты через запятую), и только потом выбрасываем всё, где
            // остались неизвестные переменные.
            .map(|a| normalize_wf_ports(&a))
            .filter(|a| !a.is_empty() && !a.contains('%'))
            .collect();

        if args.is_empty() {
            continue;
        }

        // Профиль без своего фильтра (`--filter-*`) winws не примет. Нулевой
        // блок — глобальный (--wf-tcp/--wf-udp), у него фильтра и не должно быть.
        if index > 0 && !args.iter().any(|a| a.starts_with("--filter")) {
            continue;
        }

        if !result.is_empty() {
            result.push("--new".to_string());
        }
        result.append(&mut args);
    }

    result
}

/// Чистит список портов в глобальном фильтре: убирает хвосты от
/// `%GameFilterTCP%`/`%GameFilterUDP%` и пустые элементы.
/// `--wf-tcp=80,443,%GameFilterTCP%` -> `--wf-tcp=80,443`.
fn normalize_wf_ports(arg: &str) -> String {
    let Some(value) = arg
        .strip_prefix("--wf-tcp=")
        .or_else(|| arg.strip_prefix("--wf-udp="))
    else {
        return arg.to_string();
    };

    let prefix = &arg[..arg.len() - value.len()];
    let value = value
        .replace("%GameFilterTCP%", "")
        .replace("%GameFilterUDP%", "");
    let ports: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();

    if ports.is_empty() {
        String::new()
    } else {
        format!("{prefix}{}", ports.join(","))
    }
}

/// Дефолтный набор стратегий "из коробки" — взято из твоих рабочих
/// general (ALT12)/(ALT13) батников. {BIN} и {LISTS} подставляются
/// в рантайме реальными путями (engine::resolve_placeholders).
///
/// Игровые куски (--filter-tcp/udp=%GameFilterTCP%/%GameFilterUDP%) пока
/// убраны — их значения задавал service.bat, которого у нас нет. Добавим
/// отдельной стратегией, когда разберём что он туда пишет.
pub fn fallback_strategies() -> StrategyList {
    StrategyList {
        version: 1,
        // Встроенный набор — не из релиза, штампа нет.
        source_tag: None,
        strategies: vec![
            Strategy {
                id: "alt12".into(),
                name: "ALT12 (общая)".into(),
                args: split_args(
                    r#"--wf-tcp=80,443,2053,2083,2087,2096,8443 --wf-udp=443,19294-19344,50000-50100 --filter-udp=443 --hostlist="{LISTS}list-general.txt" --hostlist="{LISTS}list-general-user.txt" --hostlist-exclude="{LISTS}list-exclude.txt" --hostlist-exclude="{LISTS}list-exclude-user.txt" --ipset-exclude="{LISTS}ipset-exclude.txt" --ipset-exclude="{LISTS}ipset-exclude-user.txt" --dpi-desync=fake --dpi-desync-repeats=11 --dpi-desync-fake-quic="{BIN}quic_initial_www_google_com.bin" --new --filter-udp=19294-19344,50000-50100 --filter-l7=discord,stun --dpi-desync=fake --dpi-desync-fake-discord="{BIN}stun.bin" --dpi-desync-fake-discord="{BIN}ACTIVE_DISCORD_UDP.bin" --dpi-desync-fake-stun="{BIN}ACTIVE_DISCORD_UDP.bin" --dpi-desync-repeats=3 --new --filter-tcp=2053,2083,2087,2096,8443 --hostlist-domains=discord.media --dpi-desync=fake,multisplit --dpi-desync-split-seqovl=681 --dpi-desync-split-pos=1 --dpi-desync-fooling=ts --dpi-desync-repeats=8 --dpi-desync-split-seqovl-pattern="{BIN}tls_clienthello_www_google_com.bin" --dpi-desync-fake-tls="{BIN}tls_clienthello_www_google_com.bin" --new --filter-tcp=443 --hostlist="{LISTS}list-google.txt" --ip-id=zero --dpi-desync=hostfakesplit --dpi-desync-fooling=ts --dpi-desync-hostfakesplit-mod=host=www.google.com --new --filter-tcp=80,443 --hostlist="{LISTS}list-general.txt" --hostlist="{LISTS}list-general-user.txt" --hostlist-exclude="{LISTS}list-exclude.txt" --hostlist-exclude="{LISTS}list-exclude-user.txt" --ipset-exclude="{LISTS}ipset-exclude.txt" --ipset-exclude="{LISTS}ipset-exclude-user.txt" --dpi-desync=fake,multisplit --dpi-desync-split-seqovl=664 --dpi-desync-split-pos=1 --dpi-desync-fooling=ts --dpi-desync-repeats=8 --dpi-desync-split-seqovl-pattern="{BIN}tls_clienthello_max_ru.bin" --dpi-desync-fake-tls="{BIN}stun.bin" --dpi-desync-fake-tls="{BIN}tls_clienthello_max_ru.bin" --dpi-desync-fake-http="{BIN}tls_clienthello_max_ru.bin" --new --filter-udp=443 --ipset="{LISTS}ipset-all.txt" --hostlist-exclude="{LISTS}list-exclude.txt" --hostlist-exclude="{LISTS}list-exclude-user.txt" --ipset-exclude="{LISTS}ipset-exclude.txt" --ipset-exclude="{LISTS}ipset-exclude-user.txt" --dpi-desync=fake --dpi-desync-repeats=11 --dpi-desync-fake-quic="{BIN}quic_initial_www_google_com.bin" --new --filter-tcp=80,443,8443 --ipset="{LISTS}ipset-all.txt" --hostlist-exclude="{LISTS}list-exclude.txt" --hostlist-exclude="{LISTS}list-exclude-user.txt" --ipset-exclude="{LISTS}ipset-exclude.txt" --ipset-exclude="{LISTS}ipset-exclude-user.txt" --dpi-desync=fake,multisplit --dpi-desync-split-seqovl=664 --dpi-desync-split-pos=1 --dpi-desync-fooling=ts --dpi-desync-repeats=8 --dpi-desync-split-seqovl-pattern="{BIN}tls_clienthello_max_ru.bin" --dpi-desync-fake-tls="{BIN}stun.bin" --dpi-desync-fake-tls="{BIN}tls_clienthello_max_ru.bin" --dpi-desync-fake-http="{BIN}tls_clienthello_max_ru.bin""#,
                ),
            },
            Strategy {
                id: "alt13".into(),
                name: "ALT13 (общая, альт.)".into(),
                args: split_args(
                    r#"--wf-tcp=80,443,2053,2083,2087,2096,8443 --wf-udp=443,19294-19344,50000-50100 --filter-udp=443 --hostlist="{LISTS}list-general.txt" --hostlist="{LISTS}list-general-user.txt" --hostlist-exclude="{LISTS}list-exclude.txt" --hostlist-exclude="{LISTS}list-exclude-user.txt" --ipset-exclude="{LISTS}ipset-exclude.txt" --ipset-exclude="{LISTS}ipset-exclude-user.txt" --dpi-desync=fake --dpi-desync-repeats=11 --dpi-desync-fake-quic="{BIN}quic_initial_www_google_com.bin" --new --filter-udp=19294-19344,50000-50100 --filter-l7=discord,stun --dpi-desync=fake --dpi-desync-fake-discord="{BIN}ACTIVE_DISCORD_UDP.bin" --dpi-desync-fake-stun="{BIN}ACTIVE_DISCORD_UDP.bin" --dpi-desync-repeats=5 --new --filter-tcp=2053,2083,2087,2096,8443 --hostlist-domains=discord.media --dpi-desync=fake,multisplit --dpi-desync-split-seqovl=681 --dpi-desync-split-pos=1 --dpi-desync-fooling=ts --dpi-desync-repeats=7 --dpi-desync-split-seqovl-pattern="{BIN}tls_clienthello_www_google_com.bin" --dpi-desync-fake-tls="{BIN}tls_clienthello_www_google_com.bin" --new --filter-tcp=443 --hostlist="{LISTS}list-google.txt" --dpi-desync=hostfakesplit --dpi-desync-fooling=ts --dpi-desync-hostfakesplit-mod=host=www.google.com --new --filter-tcp=80,443 --hostlist="{LISTS}list-general.txt" --hostlist="{LISTS}list-general-user.txt" --hostlist-exclude="{LISTS}list-exclude.txt" --hostlist-exclude="{LISTS}list-exclude-user.txt" --ipset-exclude="{LISTS}ipset-exclude.txt" --ipset-exclude="{LISTS}ipset-exclude-user.txt" --dpi-desync=fake,hostfakesplit --dpi-desync-fooling=ts --dpi-desync-hostfakesplit-mod=host=mail.ru,altorder=1 --dpi-desync-repeats=5 --dpi-desync-fake-tls="{BIN}tls_clienthello_sochi_park.bin" --dpi-desync-fake-tls="{BIN}stun2.bin" --dpi-desync-fake-http="{BIN}tls_clienthello_sochi_park.bin" --new --filter-udp=443 --ipset="{LISTS}ipset-all.txt" --hostlist-exclude="{LISTS}list-exclude.txt" --hostlist-exclude="{LISTS}list-exclude-user.txt" --ipset-exclude="{LISTS}ipset-exclude.txt" --ipset-exclude="{LISTS}ipset-exclude-user.txt" --dpi-desync=fake --dpi-desync-repeats=11 --dpi-desync-fake-quic="{BIN}quic_initial_www_google_com.bin" --new --filter-tcp=80,443,8443 --ipset="{LISTS}ipset-all.txt" --hostlist-exclude="{LISTS}list-exclude.txt" --hostlist-exclude="{LISTS}list-exclude-user.txt" --ipset-exclude="{LISTS}ipset-exclude.txt" --ipset-exclude="{LISTS}ipset-exclude-user.txt" --dpi-desync=fake,hostfakesplit --dpi-desync-fooling=ts --dpi-desync-hostfakesplit-mod=host=mail.ru,altorder=1 --dpi-desync-repeats=5 --dpi-desync-fake-tls="{BIN}tls_clienthello_sochi_park.bin" --dpi-desync-fake-tls="{BIN}stun2.bin" --dpi-desync-fake-http="{BIN}tls_clienthello_sochi_park.bin""#,
                ),
            },
        ],
    }
}

/// Разбивает командную строку на аргументы, уважая кавычки
/// (простой парсер: между "..." пробелы не разделяют аргумент).
fn split_args(cmd: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for c in cmd.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ' ' if !in_quotes => {
                if !current.is_empty() {
                    args.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Типичный general (ALT1).bat из набора zapret: запуск через `start`,
    /// переносы строк через `^`, переменные %BIN%/%LISTS%, глобальный
    /// `%GameFilterTCP%` и отдельный профиль, целиком построенный на нём.
    const BAT: &str = r#"@echo off
chcp 65001 >nul
title zapret

:: на всякий случай гасим старый процесс
taskkill /F /IM winws.exe >nul 2>&1

call service.bat
set GameFilterTCP=,50000-50100
set GameFilterUDP=,50000-50100

start "zapret" /min "%BIN%\winws.exe" --wf-tcp=80,443%GameFilterTCP% --wf-udp=443%GameFilterUDP% ^
--filter-tcp=80,443 --hostlist="%LISTS%\list-general.txt" ^
--dpi-desync=fake --dpi-desync-fake-tls="%BIN%\stun.bin" ^
--new ^
--filter-udp=443 --hostlist="%LISTS%\list-google.txt" --dpi-desync=fake --dpi-desync-fake-quic="%BIN%\quic_initial_www_google_com.bin" ^
--new ^
--filter-tcp=%GameFilterTCP% --filter-udp=%GameFilterUDP% --dpi-desync=fake
"#;

    #[test]
    fn old_cache_without_source_tag_still_loads() {
        // Кэш, записанный до появления поля source_tag, обязан читаться.
        let old = r#"{"version":2,"strategies":[{"id":"a","name":"A","args":["--wf-tcp=443"]}]}"#;
        let list: StrategyList = serde_json::from_str(old).unwrap();

        assert_eq!(list.strategies.len(), 1);
        assert!(list.source_tag.is_none());
    }

    #[test]
    fn parses_start_line_with_continuations() {
        let strategy = parse_bat_strategy("general (ALT1).bat", BAT).expect("стратегия не разобралась");

        assert_eq!(strategy.name, "general (ALT1)");
        assert_eq!(strategy.id, "general_alt1");

        // Глобальные фильтры на месте, но без хвостовой запятой от GameFilter.
        assert!(strategy.args.contains(&"--wf-tcp=80,443".to_string()), "{:?}", strategy.args);
        assert!(strategy.args.contains(&"--wf-udp=443".to_string()), "{:?}", strategy.args);

        // Плейсхолдеры подставлены, двойных слешей нет.
        assert!(strategy
            .args
            .iter()
            .any(|a| a == "--hostlist={LISTS}list-general.txt"));
        assert!(strategy
            .args
            .iter()
            .any(|a| a == "--dpi-desync-fake-tls={BIN}stun.bin"));

        // Неизвестных переменных не осталось, и нет блока без фильтра.
        assert!(!strategy.args.iter().any(|a| a.contains('%')), "{:?}", strategy.args);
        let new_count = strategy.args.iter().filter(|a| *a == "--new").count();
        assert_eq!(new_count, 1, "ожидался один --new, args={:?}", strategy.args);

        // Ни одной «пустой» опции после выкидывания GameFilter-блока.
        assert!(!strategy.args.iter().any(|a| a == "--new--new"));
        assert!(strategy.args.iter().all(|a| !a.is_empty()));
    }

    #[test]
    fn parses_unquoted_engine_path() {
        // Встречается вариант без кавычек вокруг пути к exe.
        let bat = "start \"\" /min %BIN%\\winws.exe --wf-tcp=443 ^\n--filter-tcp=443 --dpi-desync=split2\n";
        let strategy = parse_bat_strategy("general (ALT2).bat", bat).expect("не разобрался");
        assert_eq!(
            strategy.args,
            vec!["--wf-tcp=443", "--filter-tcp=443", "--dpi-desync=split2"]
        );
    }

    #[test]
    fn ignores_taskkill_and_empty_files() {
        assert!(parse_bat_strategy("bad.bat", "taskkill /F /IM winws.exe >nul").is_none());
        assert!(parse_bat_strategy("empty.bat", "").is_none());
    }

    #[test]
    fn split_args_keeps_quoted_spaces() {
        let args = split_args(r#"--hostlist="C:\a b\list.txt" --new"#);
        assert_eq!(args, vec![r"--hostlist=C:\a b\list.txt", "--new"]);
    }

    #[test]
    fn fallback_strategies_are_consistent() {
        let list = fallback_strategies();
        assert!(!list.strategies.is_empty());
        for strategy in &list.strategies {
            assert!(strategy.args.len() > 3, "{}: пустые аргументы", strategy.id);
            assert!(strategy.args.iter().any(|a| a.starts_with("--wf-")), "{}", strategy.id);
            // Все плейсхолдеры должны быть известны engine::resolve_placeholders.
            for arg in &strategy.args {
                assert!(!arg.contains('%'), "{}: остался %VAR%: {arg}", strategy.id);
            }
        }
    }
}
