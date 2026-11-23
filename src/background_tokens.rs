use std::time::Duration;

use chrono::Utc;

use crate::provider_oauth::{RefreshMode, background_candidate, refresh_account};
use crate::{provider_connections, storage::Storage};

const STARTUP: Duration = Duration::from_secs(10);
const INTERVAL: Duration = Duration::from_secs(300);

pub fn start(storage: Storage) {
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        || std::env::var("DISABLE_BACKGROUND_TOKEN_REFRESH").is_ok_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    {
        return;
    }
    tokio::spawn(async move {
        let first = tokio::time::Instant::now() + STARTUP;
        let mut clock = tokio::time::interval_at(first, INTERVAL);
        clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            clock.tick().await;
            refresh_due(&storage).await;
        }
    });
}

async fn refresh_due(storage: &Storage) {
    let now = Utc::now().timestamp_millis();
    let Ok(accounts) = storage
        .run(false, move |db| {
            let mut accounts = provider_connections::read_all(db, None)?;
            accounts.retain(|account| background_candidate(account, now));
            Ok(accounts)
        })
        .await
    else {
        return;
    };
    let mut accounts = accounts.into_iter().peekable();
    while let Some(account) = accounts.next() {
        let provider = account["provider"].as_str().unwrap_or_default();
        match refresh_account(storage, &account, RefreshMode::Background).await {
            Ok(_) => crate::console_logs::record(
                "INFO",
                &format!("Checked {provider} credential expiry"),
            ),
            Err(error) => crate::console_logs::record(
                "WARN",
                &format!("{provider} credential refresh failed: {error}"),
            ),
        }
        if accounts.peek().is_some() {
            tokio::time::sleep(delay(provider)).await;
        }
    }
}

fn delay(provider: &str) -> Duration {
    let sensitive = matches!(provider, "antigravity" | "gemini-cli");
    let (name, default) = if sensitive {
        ("BG_REFRESH_GOOGLE_DELAY_MS", 12_000)
    } else {
        ("BG_REFRESH_DELAY_MS", 1_500)
    };
    let base = std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default);
    let mut bytes = [0u8; 2];
    let jitter = if sensitive && getrandom::fill(&mut bytes).is_ok() {
        u64::from(u16::from_le_bytes(bytes) % 4000)
    } else {
        200
    };
    Duration::from_millis(base.saturating_add(jitter))
}
