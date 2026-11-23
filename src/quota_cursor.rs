use reqwest::{Response, StatusCode, header};
use serde_json::{Map, Value, json};

use super::{QuotaDelivery, parse, supplied};
use crate::cursor_identity::Account;

const MAX_BODY: usize = 4 * 1024 * 1024;

pub(in super::super) struct Endpoints {
    pub(in super::super) period: String,
    pub(in super::super) plan: String,
    pub(in super::super) legacy: String,
}

impl Endpoints {
    pub(super) fn pinned() -> Self {
        let base = "https://api2.cursor.sh/aiserver.v1.DashboardService";
        Self {
            period: format!("{base}/GetCurrentPeriodUsage"),
            plan: format!("{base}/GetPlanInfo"),
            legacy: "https://api2.cursor.sh/auth/usage".into(),
        }
    }

    pub(super) async fn read(&self, read: &QuotaDelivery, connection: &Value) -> Value {
        let Some(account) = Account::from_connection(connection) else {
            return json!({"message":"Cursor access token not available. Reconnect Cursor to view quota."});
        };
        let Some(period) = connect(read, &self.period, &account).await else {
            return json!({"message":"Cursor error: fetch failed"});
        };
        let status = period.status();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return json!({"plan":"Cursor","message":"Cursor authentication failed. Reconnect the Cursor provider."});
        }
        let raw = read_body(period).await.unwrap_or_default();
        let data: Value = if status.is_success() {
            serde_json::from_slice(&raw).unwrap_or_default()
        } else {
            Value::Null
        };
        let mut quotas = current_period(&data);
        if quotas.is_empty() {
            let request = read
                .client
                .get(&self.legacy)
                .bearer_auth(account.token)
                .header(header::ACCEPT, "application/json");
            let Ok(fallback) = read.send(request).await else {
                return json!({"message":"Cursor error: fetch failed"});
            };
            if fallback.status().is_success() {
                let value = read_json(fallback).await;
                quotas = request_buckets(&value, &account);
            } else if !status.is_success() {
                let detail: String = read
                    .redact_relay(clean(&String::from_utf8_lossy(&raw), &account))
                    .chars()
                    .take(120)
                    .collect();
                let suffix = if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                };
                return json!({"plan":"Cursor","message":format!("Cursor usage API error ({}){suffix}",status.as_u16())});
            }
        }
        if quotas.is_empty() {
            let message = data["displayMessage"]
                .as_str()
                .filter(|message| !message.is_empty())
                .map_or_else(
                    || "Cursor connected. No quota data returned for this plan.".into(),
                    |message| read.redact_relay(clean(message, &account)),
                );
            return json!({"plan":"Cursor","message":message});
        }
        let mut plan = "Cursor".to_owned();
        if let Some(response) = connect(read, &self.plan, &account)
            .await
            .filter(|response| response.status().is_success())
        {
            let info = read_json(response).await;
            if let Some(label) = supplied(&info["planInfo"]["planName"]) {
                plan = format!("Cursor {}", read.redact_relay(clean(label, &account)));
            }
        }
        json!({"plan":plan,"quotas":quotas})
    }
}

async fn connect(read: &QuotaDelivery, target: &str, account: &Account<'_>) -> Option<Response> {
    let request = read
        .client
        .post(target)
        .bearer_auth(account.token)
        .header(header::USER_AGENT, "9Router")
        .header(header::ACCEPT, "application/json")
        .header("connect-protocol-version", "1")
        .header("x-cursor-checksum", account.checksum()?)
        .header("x-ghost-mode", "true")
        .json(&json!({}));
    read.send(request).await.ok()
}

fn current_period(data: &Value) -> Map<String, Value> {
    let mut quotas = Map::new();
    let reset = parse::reset_date(&data["billingCycleEnd"], true);
    let usage = &data["planUsage"];
    let total = parse::finite(&usage["limit"]);
    if total > 0.0 {
        let including = usage
            .get("includedSpend")
            .filter(|value| !value.is_null())
            .unwrap_or(&usage["totalSpend"]);
        let used = parse::finite(including);
        let remaining =
            parse::finite_number(&usage["remaining"]).unwrap_or_else(|| (total - used).max(0.0));
        let balance = usage
            .get("totalPercentUsed")
            .filter(|value| !value.is_null())
            .map_or_else(
                || usd_ratio(remaining, total),
                |percent| 100.0 - parse::finite(percent).clamp(0.0, 100.0),
            );
        quotas.insert(
            "Plan (USD)".into(),
            row(usd(used), usd(total), balance, &reset),
        );
    } else {
        percent(
            &mut quotas,
            usage,
            "totalPercentUsed",
            "Included usage",
            &reset,
        );
    }
    percent(
        &mut quotas,
        usage,
        "autoPercentUsed",
        "Auto models (%)",
        &reset,
    );
    percent(
        &mut quotas,
        usage,
        "apiPercentUsed",
        "Named models (%)",
        &reset,
    );

    let demand = &data["spendLimitUsage"];
    for (label, limit, used, remaining) in [
        (
            "On-demand (USD)",
            "individualLimit",
            "individualUsed",
            "individualRemaining",
        ),
        (
            "Team on-demand (USD)",
            "pooledLimit",
            "pooledUsed",
            "pooledRemaining",
        ),
    ] {
        let limit = parse::finite(&demand[limit]);
        if limit <= 0.0 {
            continue;
        }
        let used = parse::finite(&demand[used]);
        let remaining = demand
            .get(remaining)
            .filter(|value| !value.is_null())
            .map_or_else(|| (limit - used).max(0.0), parse::finite);
        quotas.insert(
            label.into(),
            row(usd(used), usd(limit), usd_ratio(remaining, limit), &reset),
        );
        break;
    }
    quotas
}

fn percent(
    quotas: &mut Map<String, Value>,
    usage: &Value,
    field: &str,
    label: &str,
    reset: &Value,
) {
    if let Some(value) = usage.get(field).filter(|value| !value.is_null()) {
        let used = parse::finite(value);
        quotas.insert(
            label.into(),
            row(used, 100.0, 100.0 - used.clamp(0.0, 100.0), reset),
        );
    }
}

fn request_buckets(data: &Value, account: &Account<'_>) -> Map<String, Value> {
    let reset = parse::reset_date(&data["startOfMonth"], true);
    data.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, bucket)| {
            let total = parse::finite(&bucket["maxRequestUsage"]);
            let used = parse::finite(&bucket["numRequests"]);
            (total > 0.0).then(|| {
                (
                    format!("{} requests", clean(key, account)),
                    row(
                        used,
                        total,
                        ((total - used) / total * 100.0).max(0.0),
                        &reset,
                    ),
                )
            })
        })
        .collect()
}

fn clean(text: &str, account: &Account<'_>) -> String {
    text.replace(account.token, "[redacted]")
        .replace(account.machine, "[redacted]")
}

fn usd(cents: f64) -> f64 {
    cents.round() / 100.0
}

fn usd_ratio(remaining: f64, total: f64) -> f64 {
    let total = usd(total);
    if total > 0.0 {
        usd(remaining) / total * 100.0
    } else {
        0.0
    }
}

fn row(used: f64, total: f64, remaining: f64, reset: &Value) -> Value {
    json!({"used":parse::number(used),"total":parse::number(total),
        "remainingPercentage":parse::number(remaining),"resetAt":reset})
}

async fn read_json(response: Response) -> Value {
    read_body(response)
        .await
        .and_then(|body| serde_json::from_slice(&body).ok())
        .unwrap_or_default()
}

async fn read_body(mut response: Response) -> Option<Vec<u8>> {
    let mut raw = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_BODY - raw.len() {
            return None;
        }
        raw.extend_from_slice(&chunk);
    }
    Some(raw)
}
