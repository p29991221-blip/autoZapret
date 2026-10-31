//! Реальная проверка «стратегия работает».
//!
//! `winws.exe` запускается и не падает даже тогда, когда обход не помогает:
//! он просто не может пролезть через DPI конкретного провайдера. Поэтому
//! после старта мы дёргаем HTTPS к доменам, которые в России режутся, и
//! смотрим, проходит ли TLS-хендшейк целиком.
//!
//! Проверка идёт через тот же сетевой стек, что и обычный трафик, поэтому
//! winws перехватывает её так же, как трафик браузера.
//!
//! Почему домены разделены на обязательные и дополнительные: раньше хватало
//! любого из двух, и стратегия, чинившая только Discord, объявлялась рабочей —
//! при этом YouTube не открывался, а приложение показывало «Подключено».
//! Теперь вердикт зависит только от YouTube.

use std::time::Duration;

/// Домены, которые обязаны открываться. Если YouTube не открывается —
/// стратегия не рабочая, что бы ни происходило с остальными.
pub const REQUIRED_HOSTS: [&str; 2] = ["www.youtube.com", "youtube.com"];

/// Дополнительные домены: на вердикт не влияют, но попадают в текст ошибки,
/// чтобы было видно, что именно не открылось.
pub const EXTRA_HOSTS: [&str; 1] = ["discord.com"];

#[derive(Debug, Default)]
pub struct ProbeReport {
    pub ok: Vec<&'static str>,
    pub failed: Vec<&'static str>,
}

impl ProbeReport {
    /// Стратегия рабочая, только если открылись все обязательные домены.
    pub fn is_working(&self) -> bool {
        !self
            .failed
            .iter()
            .any(|host| REQUIRED_HOSTS.contains(host))
    }

    pub fn failed_list(&self) -> String {
        self.failed.join(", ")
    }

    fn all_failed() -> Self {
        let mut report = Self::default();
        report.failed.extend(REQUIRED_HOSTS);
        report.failed.extend(EXTRA_HOSTS);
        report
    }
}

pub async fn probe(timeout: Duration) -> ProbeReport {
    let client = match reqwest::Client::builder()
        // Some сервера отдают 403 без нормального UA, а нам важен сам факт
        // установленного TLS-соединения, а не тело ответа.
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
        )
        // Редирект уводит на другой домен, которого может не быть в списках
        // обхода — тогда получим ложное «не работает». Нам хватает любого
        // ответа, включая 3xx.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
    {
        Ok(client) => client,
        Err(_) => return ProbeReport::all_failed(),
    };

    let mut tasks = Vec::new();

    for host in REQUIRED_HOSTS.iter().chain(EXTRA_HOSTS.iter()) {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            (*host, check_host(&client, host, timeout).await)
        }));
    }

    let mut report = ProbeReport::default();
    for task in tasks {
        match task.await {
            Ok((host, true)) => report.ok.push(host),
            Ok((host, false)) => report.failed.push(host),
            // Задача упала — считаем проверку неудачной.
            Err(_) => {}
        }
    }

    // Если часть задач не отдала результат, вердикт всё равно должен быть
    // консервативным: обязательные домены без подтверждения = провал.
    if report.ok.len() + report.failed.len() < REQUIRED_HOSTS.len() + EXTRA_HOSTS.len() {
        for host in REQUIRED_HOSTS {
            if !report.ok.contains(&host) && !report.failed.contains(&host) {
                report.failed.push(host);
            }
        }
    }

    report
}

/// Одна проверка домена: получить ответ и прочитать первые данные.
/// Две попытки в рамках общего бюджета: с `--dpi-desync` первое соединение
/// иногда убивается DPI, а второе проходит — ровно поэтому браузеры сами
/// повторяют запрос. Попытки дешёвые, потому что провал по RST приходит
/// за доли секунды.
async fn check_host(client: &reqwest::Client, host: &str, budget: Duration) -> bool {
    let started = tokio::time::Instant::now();

    for _ in 0..2 {
        let left = budget.saturating_sub(started.elapsed());
        if left.is_zero() {
            break;
        }

        let attempt = async {
            let Ok(mut response) = client.get(format!("https://{host}/")).send().await else {
                return false;
            };

            // Одних заголовков мало: DPI нередко пропускает TLS-хендшейк и
            // ответ, а потом рвёт соединение на первых данных. Поэтому
            // обязательно читаем первый кусок тела.
            matches!(response.chunk().await, Ok(Some(_)) | Ok(None))
        };

        if tokio::time::timeout(left, attempt).await.unwrap_or(false) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_is_required_discord_is_not() {
        let mut report = ProbeReport::default();
        report.ok = vec!["www.youtube.com", "youtube.com"];
        report.failed = vec!["discord.com"];
        assert!(report.is_working(), "YouTube открылся — стратегия рабочая");

        let mut only_discord = ProbeReport::default();
        only_discord.ok = vec!["discord.com"];
        only_discord.failed = vec!["www.youtube.com", "youtube.com"];
        assert!(
            !only_discord.is_working(),
            "Discord без YouTube — это ещё не рабочий обход"
        );
        assert_eq!(only_discord.failed_list(), "www.youtube.com, youtube.com");
    }

    #[test]
    fn every_probe_host_is_covered_by_engine_lists() {
        // Домены проверки должны быть в списках, иначе winws их не обработает
        // и «рабочая» стратегия будет определяться случайно.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("engine/lists");

        let mut lists = String::new();
        for name in ["list-general.txt", "list-google.txt"] {
            if let Ok(text) = std::fs::read_to_string(root.join(name)) {
                lists.push_str(&text.to_lowercase());
            }
        }

        for host in REQUIRED_HOSTS.iter().chain(EXTRA_HOSTS.iter()) {
            let needle = host.to_lowercase();
            let covered = lists.lines().any(|line| {
                let line = line.trim();
                !line.is_empty() && (needle.ends_with(line) || line.ends_with(&needle))
            });
            assert!(covered, "{host} не покрыт списками доменов движка");
        }
    }

    /// Дымовая проверка на живой сети (по умолчанию отключена, ходит в интернет):
    ///   cargo test --no-default-features -- --ignored
    #[test]
    #[ignore = "ходит в сеть; запускать вручную через --ignored"]
    fn probe_works_against_real_network() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let report = runtime.block_on(probe(Duration::from_secs(6)));

        println!(
            "ok={:?} failed={:?} is_working={}",
            report.ok,
            report.failed,
            report.is_working()
        );

        // Главное — проверка отрабатывает и не виснет: каждый домен получает
        // либо успех, либо провал.
        assert_eq!(
            report.ok.len() + report.failed.len(),
            REQUIRED_HOSTS.len() + EXTRA_HOSTS.len()
        );
    }
}
