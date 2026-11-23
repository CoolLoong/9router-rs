use std::collections::HashMap;

use chrono::{DateTime, Utc};
use reqwest::{StatusCode, header};
use serde_json::{Map, Value, json};

use super::super::ReadScope;
use super::{QuotaDelivery, coerce_quantity, parse, redact, supplied};

pub(in super::super) const CN_USAGE: &str =
    "https://copilot.tencent.com/v2/billing/meter/get-user-resource";
pub(in super::super) const INTL_USAGE: &str =
    "https://www.codebuddy.ai/v2/billing/meter/get-user-resource";
const LABEL: &str = "CodeBuddy CN";
const MAX_BODY: usize = 1024 * 1024;
const DAY_MS: f64 = 86_400_000.0;

pub(super) async fn usage(read: &QuotaDelivery, account: &Value, international: bool) -> Value {
    let (provider, target, region) = if international {
        ("codebuddy-intl", INTL_USAGE, "intl")
    } else {
        ("codebuddy-cn", CN_USAGE, "cn")
    };
    let Some(token) = supplied(&account["accessToken"]).or_else(|| supplied(&account["apiKey"]))
    else {
        return json!({"message":format!("CodeBuddy ({provider}) credential not available.")});
    };
    let local;
    let endpoint = if matches!(read.scope, ReadScope::Direct | ReadScope::Bypass)
        && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
    {
        local = format!(
            "http://127.0.0.1:52989/codebuddy-quota/{region}/v2/billing/meter/get-user-resource"
        );
        &local
    } else {
        target
    };
    let request = read
        .client
        .post(endpoint)
        .header(header::ACCEPT, "application/json")
        .bearer_auth(token)
        .json(&json!({}));
    let request = match crate::static_chat::primary(provider).filter(|_| international) {
        Some(endpoint) => endpoint.builtin_headers(request),
        None => request.header(header::USER_AGENT, "9Router"),
    };
    let response = read.send(request).await;
    let Ok(mut response) = response else {
        return failure(provider, "fetch failed");
    };
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return json!({"message":format!("{LABEL} credential invalid or expired.")});
    }
    if !status.is_success() {
        return json!({"message":format!("{LABEL} quota API error ({}).", status.as_u16())});
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if chunk.len() <= MAX_BODY - bytes.len() => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => return failure(provider, "invalid provider response"),
        }
    }
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return failure(provider, "invalid provider response");
    };
    normalized(&body, account)
}

fn failure(provider: &str, reason: &str) -> Value {
    json!({"message":format!("CodeBuddy ({provider}) error: {reason}")})
}

fn normalized(body: &Value, account: &Value) -> Value {
    if body["code"].as_f64() != Some(0.0) {
        let message = redact(
            parse::string(&parse::value_or(&body["msg"], json!("unknown"))),
            account,
        );
        return json!({"message":format!("{LABEL} quota error: {message}")});
    }
    let Some(accounts) = body["data"]["Response"]["Data"]["Accounts"]
        .as_array()
        .filter(|items| !items.is_empty())
    else {
        return json!({"message":format!("{LABEL} connected. No credit package found.")});
    };
    let (mut refills, mut bonuses): (Vec<_>, Vec<_>) = accounts
        .iter()
        .map(Package::new)
        .partition(Package::recurring);
    refills.sort_by_key(Package::sort_key);
    bonuses.sort_by_key(Package::sort_key);

    let mut quotas = Map::new();
    let mut cadence_count = HashMap::<_, usize>::new();
    for package in &refills {
        let cadence = package.cadence();
        let count = cadence_count.entry(cadence).or_default();
        *count += 1;
        let label = if *count == 1 {
            cadence.into()
        } else {
            format!("{cadence} {count}")
        };
        quotas.insert(label, package.row(true));
    }
    for (index, package) in bonuses.iter().enumerate() {
        quotas.insert(format!("Bonus Pack {}", index + 1), package.row(false));
    }
    let base = refills.first().map_or(&accounts[0], |package| package.data);
    let plan = parse::value_or(
        &base["PackageName"],
        parse::value_or(&base["SubProductName"], json!("CodeBuddy")),
    );
    let plan = redact(parse::string(&plan), account);
    json!({"plan":plan,"quotas":quotas})
}

struct Package<'a> {
    data: &'a Value,
    reset: Option<DateTime<Utc>>,
}

impl<'a> Package<'a> {
    fn new(data: &'a Value) -> Self {
        Self {
            data,
            reset: parse::reset_time(&data["CycleEndTime"], true),
        }
    }

    fn sort_key(&self) -> i64 {
        self.reset.map_or(i64::MAX, |date| date.timestamp_millis())
    }

    fn recurring(&self) -> bool {
        let deduction_end = coerce_quantity(&self.data["DeductionEndTime"]);
        self.reset
            .zip(deduction_end)
            .is_some_and(|(cycle, deduction)| {
                deduction - cycle.timestamp_millis() as f64 > 2.0 * DAY_MS
            })
    }

    fn cadence(&self) -> &'static str {
        let start = parse::reset_time(&self.data["CycleStartTime"], true);
        let days = start.zip(self.reset).map(|(start, end)| {
            (end.timestamp_millis() - start.timestamp_millis()) as f64 / DAY_MS
        });
        match days {
            Some(days) if days <= 1.5 => "Daily",
            Some(days) if days <= 10.0 => "Weekly",
            _ => "Monthly",
        }
    }

    fn row(&self, recurring: bool) -> Value {
        let (used, total) = if recurring {
            (
                self.quantity("CycleCapacityUsedPrecise", "CycleCapacityUsed"),
                self.quantity("CycleCapacitySizePrecise", "CycleCapacitySize"),
            )
        } else {
            (
                self.quantity("CapacityUsedPrecise", "CapacityUsed"),
                self.quantity("CapacitySizePrecise", "CapacitySize"),
            )
        };
        let reset = self
            .reset
            .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        json!({"used":used,"total":total,"resetAt":reset,"unlimited":false,"recurring":recurring})
    }

    fn quantity(&self, precise: &str, plain: &str) -> Value {
        let value = self
            .data
            .get(precise)
            .filter(|value| !value.is_null())
            .unwrap_or(&self.data[plain]);
        parse::number(coerce_quantity(value).unwrap_or_default())
    }
}
