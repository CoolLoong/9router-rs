use std::collections::HashMap;
use std::time::Duration;

use axum::http::HeaderMap;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::TransactionBehavior;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::account_quota::{
    Provider, QuotaState, access, active_refreshable, auth_expired, proactive, supports_direct,
};
use crate::chat_format::ChatFormat;
use crate::codex_executor::{self, Sessions};
use crate::provider_connections;
use crate::provider_oauth::{RefreshMode, refresh_account};
use crate::proxy_transport::{self, Route};
use crate::quota_ping_schedule::Schedule;
use crate::storage::Storage;

const TICK: Duration = Duration::from_secs(60);
const LEAD_MS: i64 = 5_000;
const AHEAD_MS: i64 = 300_000;
const RETRY_MS: i64 = 900_000;
const CODEX_DRIFT_MS: i64 = 30_000;
const CODEX_INTERVAL_MS: i64 = 600_000;
const PING_TIMEOUT: Duration = Duration::from_secs(45);
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
const CLAUDE_URL: &str = "https://api.anthropic.com/v1/messages?beta=true";
const FIXTURE: &str = "http://127.0.0.1:52989/quota-autoping";
pub(super) const SETTING_KEYS: &[&str] = &["claudeAutoPing", "codexAutoPing"];
static SETTINGS_CHANGED: Notify = Notify::const_new();

pub(super) fn wake() {
    SETTINGS_CHANGED.notify_one();
}

pub(super) fn start(state: QuotaState) {
    let fixture = std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1");
    tokio::spawn(async move {
        let period = if fixture {
            Duration::from_millis(250)
        } else {
            TICK
        };
        let mut interval = interval_at(Instant::now() + Duration::from_secs(3), period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut accounts = HashMap::new();
        let sessions = Sessions::default();
        let mut running = false;
        loop {
            let scheduled = tokio::select! {
                _ = interval.tick() => true,
                _ = SETTINGS_CHANGED.notified() => false,
            };
            let Some(settings) = state.storage.read_settings().await else {
                continue;
            };
            let enabled = Service::ALL
                .iter()
                .any(|service| service.enabled(&settings));
            if enabled && (scheduled || !running) {
                if !running {
                    interval.reset();
                }
                tick(&state, &sessions, &mut accounts, &settings, fixture).await;
            } else if !enabled {
                accounts.clear();
            }
            running = enabled;
        }
    });
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum Service {
    Claude,
    Codex,
}

impl Service {
    const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    fn provider(self) -> Provider {
        match self {
            Self::Claude => Provider::Claude,
            Self::Codex => Provider::Codex,
        }
    }

    fn config(self, settings: &Value) -> &Value {
        &settings[match self {
            Self::Claude => SETTING_KEYS[0],
            Self::Codex => SETTING_KEYS[1],
        }]
    }

    fn enabled(self, settings: &Value) -> bool {
        let config = self.config(settings);
        config["connections"]
            .as_object()
            .is_some_and(|accounts| accounts.values().any(|value| value == true))
            || config["schedules"]
                .as_object()
                .is_some_and(|accounts| accounts.values().any(|value| value["enabled"] == true))
    }

    fn quota_key(self) -> &'static str {
        match self {
            Self::Claude => "session (5h)",
            Self::Codex => "session",
        }
    }

    fn eligible_reset(self, cached: Option<&str>, reset: &str, now: i64) -> bool {
        let Some(reset) = millis_text(reset) else {
            return false;
        };
        match self {
            Self::Claude => now >= reset.saturating_sub(LEAD_MS),
            Self::Codex => cached
                .and_then(millis_text)
                .is_some_and(|before| reset.saturating_sub(before) >= CODEX_DRIFT_MS),
        }
    }
}

struct OptIn {
    reactive: bool,
    schedule: Option<Schedule>,
}

impl OptIn {
    fn current(settings: &Value, service: Service, id: &str) -> Option<Self> {
        let opts = service.config(settings);
        let reactive = opts["connections"][id] == true;
        let schedule = Schedule::from_value(&opts["schedules"][id]);
        (reactive || schedule.is_some()).then_some(Self { reactive, schedule })
    }
}

#[derive(Default)]
struct Cached {
    reset: Option<String>,
    failed: Option<i64>,
}

#[derive(PartialEq)]
struct Due {
    reset: Option<String>,
    reset_key: Option<String>,
    slot: Option<String>,
    catch_up: bool,
}

impl Cached {
    fn skip(&self, service: Service, opt_in: &OptIn, now: i64) -> bool {
        self.failed
            .is_some_and(|failed| now.saturating_sub(failed) < RETRY_MS)
            || service == Service::Claude
                && opt_in.schedule.is_none()
                && self
                    .reset
                    .as_deref()
                    .and_then(millis_text)
                    .is_some_and(|reset| now < reset.saturating_sub(AHEAD_MS))
    }

    fn due(
        &mut self,
        service: Service,
        account: &Value,
        opt_in: &OptIn,
        quotas: &Value,
        now: i64,
    ) -> Option<Due> {
        let session = &quotas[service.quota_key()];
        let reset = session["resetAt"]
            .as_str()
            .filter(|value| !value.is_empty());
        let cached = self.reset.clone();
        if let Some(reset) = reset {
            self.reset = Some(reset.to_owned());
        }
        if exhausted(session)
            || service == Service::Codex
                && quotas.as_object().is_some_and(|quotas| {
                    quotas.iter().any(|(name, quota)| {
                        !name.to_ascii_lowercase().contains("session") && exhausted(quota)
                    })
                })
        {
            return None;
        }
        if service == Service::Codex
            && millis(&account["lastPingAt"])
                .is_some_and(|prior| now.saturating_sub(prior) < CODEX_INTERVAL_MS)
        {
            return None;
        }
        let slot = opt_in
            .schedule
            .as_ref()
            .and_then(|schedule| schedule.due_slot(now))
            .filter(|slot| account["lastPingedSlotKey"] != *slot);
        let mut due = Due {
            reset: reset.map(str::to_owned),
            reset_key: reset.map(reset_key),
            slot,
            catch_up: false,
        };
        if due.slot.is_some() {
            return Some(due);
        }
        let reset = reset.filter(|_| opt_in.reactive)?;
        let previous = account["lastPingedResetKey"]
            .as_str()
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .or_else(|| account["lastPingedResetAt"].as_str().map(reset_key));
        if service == Service::Claude
            && let Some(old) = cached.as_deref()
            && previous.as_deref() != Some(reset_key(old).as_str())
            && service.eligible_reset(Some(old), old, now)
        {
            due.reset = Some(old.to_owned());
            due.reset_key = Some(reset_key(old));
            due.catch_up = reset != old;
        }
        let candidate = due.reset.as_deref()?;
        (due.reset_key != previous && service.eligible_reset(cached.as_deref(), candidate, now))
            .then_some(due)
    }
}

fn millis(value: &Value) -> Option<i64> {
    value
        .as_str()
        .and_then(millis_text)
        .or_else(|| value.as_i64())
}

fn millis_text(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date| date.timestamp_millis())
}

fn reset_key(value: &str) -> String {
    millis_text(value)
        .and_then(|time| DateTime::<Utc>::from_timestamp_millis(time.div_euclid(60_000) * 60_000))
        .map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| value.to_owned())
}

fn metric(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite())
}

fn exhausted(quota: &Value) -> bool {
    if quota["unlimited"] == true {
        return false;
    }
    if let Some(remaining) = metric(&quota["remaining"]) {
        return remaining <= 0.0;
    }
    metric(&quota["total"])
        .zip(metric(&quota["used"]))
        .is_some_and(|(total, used)| total > 0.0 && used >= total)
}

async fn tick(
    state: &QuotaState,
    sessions: &Sessions,
    caches: &mut HashMap<(Service, String), Cached>,
    settings: &Value,
    fixture: bool,
) {
    let mut current = HashMap::new();
    for service in Service::ALL {
        if !service.enabled(settings) {
            continue;
        }
        let Ok(accounts) = state
            .storage
            .run(false, move |db| {
                provider_connections::read_all(db, Some(service.name()))
            })
            .await
        else {
            continue;
        };
        for account in accounts {
            let Some(id) = account["id"]
                .as_str()
                .filter(|_| account["authType"] == "oauth" && account["isActive"] == true)
            else {
                continue;
            };
            let Some(opt_in) = OptIn::current(settings, service, id) else {
                continue;
            };
            let key = (service, id.to_owned());
            let mut cached = caches.remove(&key).unwrap_or_default();
            if !cached.skip(service, &opt_in, Utc::now().timestamp_millis())
                && supports_direct(state, &account, service.provider()).await
                && run_account(
                    state,
                    sessions,
                    service,
                    &account,
                    opt_in,
                    &mut cached,
                    fixture,
                )
                .await
                .is_err()
            {
                cached.failed = Some(Utc::now().timestamp_millis());
                crate::console_logs::record(
                    "WARN",
                    &format!("{} quota auto-ping failed", service.name()),
                );
            }
            current.insert(key, cached);
        }
    }
    *caches = current;
}

async fn run_account(
    state: &QuotaState,
    sessions: &Sessions,
    service: Service,
    observed: &Value,
    opt_in: OptIn,
    cached: &mut Cached,
    fixture: bool,
) -> Result<(), ()> {
    let mut account = proactive(state, observed, RefreshMode::Proactive)
        .await
        .map_err(|_| ())?
        .unwrap_or_else(|| observed.clone());
    if account["providerSpecificData"] != observed["providerSpecificData"]
        && !supports_direct(state, &account, service.provider()).await
    {
        return Ok(());
    }
    if access(&account).is_none() {
        return Err(());
    }
    let mut usage = observe(state, service, &account, fixture).await?;
    if usage["message"].as_str().is_some_and(auth_expired) && active_refreshable(&account) {
        account = refresh_account(&state.storage, &account, RefreshMode::Rejected)
            .await
            .map_err(|_| ())?;
        if !supports_direct(state, &account, service.provider()).await {
            return Ok(());
        }
        usage = observe(state, service, &account, fixture).await?;
    }
    let now = Utc::now().timestamp_millis();
    let mut trial = Cached {
        reset: cached.reset.clone(),
        failed: cached.failed,
    };
    let Some(due) = trial.due(service, &account, &opt_in, &usage["quotas"], now) else {
        cached.reset = trial.reset;
        return Ok(());
    };
    let id = account["id"].as_str().ok_or(())?;
    let Some((current, settings)) = reload(&state.storage, service, id).await? else {
        return Ok(());
    };
    if current["accessToken"] != account["accessToken"]
        || current["providerSpecificData"] != account["providerSpecificData"]
        || current["lastPingAt"] != account["lastPingAt"]
        || current["lastPingedSlotKey"] != account["lastPingedSlotKey"]
        || current["lastPingedResetKey"] != account["lastPingedResetKey"]
    {
        return Ok(());
    }
    let Some(current_opt_in) = OptIn::current(&settings, service, id) else {
        return Ok(());
    };
    if !supports_direct(state, &current, service.provider()).await {
        return Ok(());
    }
    if cached
        .due(service, &current, &current_opt_in, &usage["quotas"], now)
        .as_ref()
        != Some(&due)
    {
        return Ok(());
    }
    if due.catch_up {
        cached.reset = due.reset.clone();
    }
    if !send(state, sessions, service, &current, &settings, fixture).await? {
        return Err(());
    }
    persist_mark(&state.storage, service, id.to_owned(), due).await?;
    cached.reset = trial.reset;
    cached.failed = None;
    crate::console_logs::record(
        "INFO",
        &format!("{} quota auto-ping completed", service.name()),
    );
    Ok(())
}

async fn reload(
    storage: &Storage,
    service: Service,
    id: &str,
) -> Result<Option<(Value, Value)>, ()> {
    let settings = storage.read_settings().await.ok_or(())?;
    let id = id.to_owned();
    let account = storage
        .run(false, move |db| provider_connections::by_id(db, &id))
        .await
        .map_err(|_| ())?;
    Ok(account
        .filter(|account| {
            account["provider"] == service.name()
                && account["isActive"] == true
                && account["authType"] == "oauth"
        })
        .map(|account| (account, settings)))
}

async fn observe(
    state: &QuotaState,
    service: Service,
    account: &Value,
    fixture: bool,
) -> Result<Value, ()> {
    if fixture {
        let token = access(account).ok_or(())?;
        return state
            .reader
            .client
            .get(format!("{FIXTURE}/{}/usage", service.name()))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| ())?
            .error_for_status()
            .map_err(|_| ())?
            .json()
            .await
            .map_err(|_| ());
    }
    state
        .reader
        .usage(service.provider(), account, service == Service::Claude)
        .await
        .map_err(|_| ())
}

async fn send(
    state: &QuotaState,
    sessions: &Sessions,
    service: Service,
    account: &Value,
    settings: &Value,
    fixture: bool,
) -> Result<bool, ()> {
    let token = access(account).ok_or(())?;
    let client = &state.reader.client;
    let mut input = json!({
        "model":"gpt-5.6-terra",
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "instructions":"Reply with OK.",
        "reasoning":{"effort":"none","summary":"auto"},
        "store":false,"stream":true,
    });
    let exchange = if service == Service::Codex {
        let session = sessions.pick(&input, account, &HeaderMap::new(), None);
        Some(
            codex_executor::finish(
                &mut input,
                "gpt-5.6-terra",
                account,
                session,
                ChatFormat::Responses,
                false,
            )
            .ok_or(())?,
        )
    } else {
        None
    };
    let target = exchange
        .as_ref()
        .map_or_else(|| CLAUDE_URL.to_owned(), |exchange| exchange.url());
    if !matches!(
        proxy_transport::select(&state.storage, settings, account, &target).await,
        Ok(Route::Direct)
    ) {
        return Err(());
    }
    let url = if fixture {
        format!("{FIXTURE}/{}/ping", service.name())
    } else {
        target
    };
    let request = if let Some(exchange) = exchange {
        exchange.headers(
            client
                .post(&url)
                .json(&input)
                .header("accept", "text/event-stream"),
            token,
        )
    } else {
        client.post(&url)
            .bearer_auth(token)
            .header("anthropic-version", &state.reader.config.anthropic_version)
            .header("anthropic-beta", "oauth-2025-04-20,claude-code-20250219")
            .json(&json!({"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}))
    };
    let mut response = request.timeout(PING_TIMEOUT).send().await.map_err(|_| ())?;
    if !response.status().is_success() {
        return Ok(false);
    }
    if service == Service::Codex {
        let mut length = 0;
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            length += chunk.len();
            if length > RESPONSE_LIMIT {
                return Err(());
            }
        }
    }
    Ok(true)
}

async fn persist_mark(storage: &Storage, service: Service, id: String, due: Due) -> Result<(), ()> {
    storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(mut account) = provider_connections::by_id(&tx, &id)?
                && account["authType"] == "oauth"
                && account["provider"] == service.name()
            {
                let now = provider_connections::now();
                account["updatedAt"] = json!(now);
                account["lastPingAt"] = json!(now);
                if let Some(key) = due.reset_key {
                    account["lastPingedResetAt"] = json!(due.reset);
                    account["lastPingedResetKey"] = json!(key);
                }
                if let Some(slot) = due.slot {
                    account["lastPingedSlotKey"] = json!(slot);
                }
                provider_connections::write(&tx, &account)?;
            }
            tx.commit()
        })
        .await
        .map_err(|_| ())
}
