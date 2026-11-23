use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};

use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use super::super::ReadScope;
use super::{QuotaDelivery, coerce_quantity, parse, supplied, truthy};

const NO_RESET: f64 = 253_402_214_400_000.0;
const CACHE_LIMIT: usize = 128;
const REFRESH_MS: i64 = 300_000;
const DEFAULT_TTL_MS: i64 = 86_400_000;
type Slot = Arc<AsyncMutex<Option<Job>>>;
type Cache = VecDeque<([u8; 32], Slot)>;
static JOBS: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

struct Job {
    token: String,
    expires: i64,
}

pub(in super::super) struct Endpoints {
    pub(in super::super) usage: String,
    pub(in super::super) exchange: String,
    pub(in super::super) identity: String,
}

impl Endpoints {
    pub(super) fn new(base: &str) -> Self {
        Self {
            usage: format!("{base}/api/v2/quota/usage"),
            exchange: format!("{base}/api/v1/jobToken/exchange"),
            identity: format!("{base}/api/v1/userinfo"),
        }
    }

    pub(super) async fn usage(&self, read: &QuotaDelivery, account: &Value) -> Value {
        let access = supplied(&account["accessToken"]);
        let pat = supplied(&account["apiKey"])
            .or(access)
            .filter(|token| token.starts_with("pt-"));
        let job = match pat {
            Some(pat) => self.job(read, pat).await,
            None => None,
        };
        let Some(access) = job.as_deref().or(access) else {
            return json!({"message":"Qoder usage unavailable: no access token"});
        };
        let request = read
            .client
            .get(&self.usage)
            .bearer_auth(access)
            .header(header::ACCEPT, "application/json");
        let Ok(response) = read.send(request).await else {
            return json!({"message":"Qoder connected. Unable to fetch usage: fetch failed"});
        };
        if !response.status().is_success() {
            return json!({"message":format!("Qoder connected. Usage fetch returned {}.",response.status().as_u16())});
        }
        match response.json::<Value>().await {
            Ok(body) if truthy(&body) => quota(&body),
            _ => json!({"message":"Qoder connected. Usage response was not JSON."}),
        }
    }

    async fn job(&self, read: &QuotaDelivery, pat: &str) -> Option<String> {
        let hash = Sha256::new()
            .chain_update(&self.exchange)
            .chain_update([0])
            .chain_update(pat);
        let hash = match read.scope {
            ReadScope::Direct => hash.chain_update([0]),
            ReadScope::Bypass => hash.chain_update([1]),
            ReadScope::Tunnel(identity) => hash.chain_update([2]).chain_update(identity),
            ReadScope::Relay(identity) => hash.chain_update([3]).chain_update(identity),
        };
        let key: [u8; 32] = hash.finalize().into();
        let slot = {
            let mut cache = JOBS.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(position) = cache.iter().position(|(candidate, _)| *candidate == key) {
                let entry = cache.remove(position).expect("present cache entry");
                let slot = entry.1.clone();
                cache.push_front(entry);
                slot
            } else {
                if cache.len() >= CACHE_LIMIT {
                    let evict = cache
                        .iter()
                        .rposition(|(_, slot)| Arc::strong_count(slot) == 1)?;
                    cache.remove(evict);
                }
                let slot = Arc::new(AsyncMutex::new(None));
                cache.push_front((key, slot.clone()));
                slot
            }
        };
        let mut state = slot.lock().await;
        let now = Utc::now().timestamp_millis();
        if let Some(job) = state
            .as_ref()
            .filter(|job| job.expires.saturating_sub(now) > REFRESH_MS)
        {
            return Some(job.token.clone());
        }
        let request = read
            .client
            .post(&self.exchange)
            .header(header::ACCEPT, "application/json")
            .header(header::USER_AGENT, "9Router")
            .json(&json!({"personal_token":pat}));
        let response = read.send(request).await.ok()?.error_for_status().ok()?;
        let payload = response.json::<Value>().await.ok()?;
        let token = supplied(&payload["token"])
            .filter(|value| header::HeaderValue::from_str(value).is_ok())?;
        let now = Utc::now().timestamp_millis();
        let fallback = if truthy(&payload["expires_at"]) {
            DEFAULT_TTL_MS
        } else {
            payload["expires_in"]
                .as_f64()
                .filter(|ttl| *ttl > 0.0)
                .map_or(DEFAULT_TTL_MS, |ttl| ttl as i64)
        };
        let expires = payload["expires_at"]
            .as_str()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map_or(now.saturating_add(fallback), |time| time.timestamp_millis());
        let identity = read
            .client
            .get(&self.identity)
            .bearer_auth(token)
            .header(header::ACCEPT, "application/json")
            .header(header::USER_AGENT, "9Router");
        let _ = read.send(identity).await;
        *state = Some(Job {
            token: token.to_owned(),
            expires,
        });
        Some(token.to_owned())
    }
}

fn quota(body: &Value) -> Value {
    let expiration = coerce_quantity(&body["expiresAt"]).filter(|value| *value > 0.0);
    let open = expiration == Some(NO_RESET);
    let reset = expiration
        .filter(|value| !open && *value <= 8.64e15)
        .and_then(|value| DateTime::<Utc>::from_timestamp_millis(value as i64))
        .map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true));
    let mut quotas = json!({
        "user":row(&body["userQuota"], reset.as_deref(), open),
        "organization":row(&body["orgResourcePackage"], reset.as_deref(), open),
    });
    let addon = &body["addOnQuota"];
    if coerce_quantity(&addon["total"]).is_some_and(|total| total > 0.0) {
        let mut quota = row(addon, None, false);
        quota["detailUrl"] = parse::value_or(&addon["detailUrl"], Value::Null);
        quotas["addOn"] = quota;
    }
    json!({"quotas":quotas,"totalUsagePercentage":number(&body["totalUsagePercentage"]),
        "isQuotaExceeded":truthy(&body["isQuotaExceeded"]),"expiresAt":expiration.map(parse::number)})
}

fn row(details: &Value, reset: Option<&str>, open: bool) -> Value {
    json!({"total":number(&details["total"]),"used":number(&details["used"]),
        "remaining":number(&details["remaining"]),"unit":parse::value_or(&details["unit"],json!("credits")),
        "resetAt":reset,"neverExpires":open})
}

fn number(value: &Value) -> Value {
    parse::number(coerce_quantity(value).unwrap_or_default())
}
