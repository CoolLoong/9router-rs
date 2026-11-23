use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use crate::storage::Storage;

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const STRIKE_WINDOW: Duration = Duration::from_secs(60);
const BLOCK_MS: i64 = 15 * 60_000;
const CAPACITY: usize = 256;

type Key = (PathBuf, String);
type Cache = VecDeque<(Key, Arc<AccountQuota>)>;
static ACCOUNTS: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

#[derive(Default)]
struct AccountQuota {
    refreshed_at: AsyncMutex<Option<Instant>>,
    state: Mutex<AccountState>,
}

#[derive(Default)]
struct AccountState {
    quotas: Value,
    strikes: HashMap<String, Strike>,
}

struct Strike {
    first: Instant,
    count: u8,
    blocked_until: Option<i64>,
}

fn get(root: &Path, id: &str, create: bool) -> Option<Arc<AccountQuota>> {
    let mut cache = ACCOUNTS.lock().ok()?;
    if let Some(index) = cache
        .iter()
        .position(|((path, connection), _)| path == root && connection == id)
    {
        let entry = cache.remove(index)?;
        let quota = entry.1.clone();
        cache.push_back(entry);
        return Some(quota);
    }
    if !create {
        return None;
    }
    if cache.len() >= CAPACITY {
        cache.pop_front();
    }
    let quota = Arc::new(AccountQuota::default());
    cache.push_back(((root.to_owned(), id.to_owned()), quota.clone()));
    Some(quota)
}

pub(super) fn blocked_until(root: &Path, id: &str, model: &str, now: i64) -> Option<i64> {
    if model.is_empty() {
        return None;
    }
    let account = get(root, id, false)?;
    let state = account.state.lock().ok()?;
    let model = model_id(model);
    let strike = state
        .strikes
        .get(model)
        .and_then(|strike| strike.blocked_until)
        .filter(|deadline| *deadline > now);
    reading(&state.quotas, model, now)
        .1
        .into_iter()
        .chain(strike)
        .max()
}

pub(super) fn success(storage: &Storage, id: &str, model: &str) {
    if let Some(account) = get(storage.root(), id, false)
        && let Ok(mut state) = account.state.lock()
    {
        state.strikes.remove(model_id(model));
    }
}

pub(super) async fn limited(storage: &Storage, id: &str, model: &str) -> Option<i64> {
    let account = get(storage.root(), id, true)?;
    {
        let mut last = account.refreshed_at.lock().await;
        if last.is_none_or(|time| time.elapsed() >= REFRESH_INTERVAL) {
            *last = Some(Instant::now());
            let connection = id.to_owned();
            if let Ok(Some(current)) = storage
                .run(false, move |db| {
                    crate::provider_connections::by_id(db, &connection)
                })
                .await
                && current["provider"] == "antigravity"
                && current["isActive"] == true
                && let Ok(mut usage) =
                    crate::account_quota::read_antigravity(storage, &current).await
                && usage["message"].is_null()
                && usage["quotas"].is_object()
                && let Ok(mut state) = account.state.lock()
            {
                state.quotas = usage["quotas"].take();
            }
        }
    }
    let now = Utc::now().timestamp_millis();
    let mut state = account.state.lock().ok()?;
    let model = model_id(model);
    let (exhausted, reset) = reading(&state.quotas, model, now);
    if exhausted {
        let block = state
            .strikes
            .get(model)
            .and_then(|strike| strike.blocked_until)
            .filter(|deadline| *deadline > now);
        if block.is_none() {
            state.strikes.remove(model);
        }
        return reset.max(block);
    }
    state.strikes.retain(|_, strike| {
        strike.blocked_until.map_or_else(
            || strike.first.elapsed() <= STRIKE_WINDOW,
            |until| until > now,
        )
    });
    let strike = state
        .strikes
        .entry(model.to_owned())
        .or_insert_with(|| Strike {
            first: Instant::now(),
            count: 0,
            blocked_until: None,
        });
    if strike.blocked_until.is_none() {
        strike.count += 1;
        if strike.count >= 3 {
            strike.blocked_until = Some(now + BLOCK_MS);
        }
    }
    strike.blocked_until
}

fn model_id(model: &str) -> &str {
    let model = model.split_once('/').map_or(model, |(_, model)| model);
    let model = model.trim();
    model
        .strip_suffix(')')
        .and_then(|model| model.rsplit_once('('))
        .filter(|(_, suffix)| !suffix.is_empty() && !suffix.contains(')'))
        .map_or(model, |(id, _)| id.trim())
}

fn reading(quotas: &Value, model: &str, now: i64) -> (bool, Option<i64>) {
    let alias = match model {
        "gemini-4-argon" => "gemini-4-argon-high",
        "claude-sonnet-5-5" => "claude-sonnet-5-5-high",
        "claude-opus-5-5" => "claude-opus-5-5-high",
        "gemini-3.8-flash" => "gemini-3.8-flash-medium",
        "gemini-3.7-flash" => "gemini-3.7-flash-medium",
        "gemini-3.6-flash" => "gemini-3.6-flash-medium",
        _ => "",
    };
    let id = model.to_ascii_lowercase();
    let family = if id.starts_with("claude-") || id.starts_with("gpt-") || id == "claude" {
        Some(("claude_gpt_session", "claude_gpt_weekly"))
    } else if id.starts_with("gemini-") && id.contains("argon") || id == "gemini_argon" {
        if quotas["gemini_argon_session"].is_null() && quotas["gemini_argon_weekly"].is_null() {
            Some(("gemini_session", "gemini_weekly"))
        } else {
            Some(("gemini_argon_session", "gemini_argon_weekly"))
        }
    } else if id.starts_with("gemini-") && !id.contains("image") || id == "gemini" {
        Some(("gemini_session", "gemini_weekly"))
    } else {
        None
    };
    let (session, weekly) = family.unwrap_or(("", ""));
    let mut exhausted = false;
    let mut deadline = None;
    for id in [model, alias, session, weekly]
        .into_iter()
        .filter(|id| !id.is_empty())
    {
        let row = &quotas[id];
        if row["remainingPercentage"]
            .as_f64()
            .is_some_and(|remaining| remaining <= 0.0)
        {
            exhausted = true;
            if let Some(reset) =
                crate::chat_accounts::date_ms(&row["resetAt"]).filter(|reset| *reset > now)
            {
                deadline = deadline.max(Some(reset));
            }
        }
    }
    (exhausted, deadline)
}
