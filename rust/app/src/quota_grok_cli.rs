use std::sync::LazyLock;

use chrono::{DateTime, SecondsFormat, Utc};
use regex::{Captures, Regex};
use reqwest::{StatusCode, header};
use serde_json::{Map, Value, json};

use super::{QuotaDelivery, parse, redact, supplied, truthy};
use crate::grok_cli_http::identity_headers;
use crate::provider_oauth::token_claims;

pub(in super::super) const BILLING: &str =
    "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const USER: &str = "https://cli-chat-proxy.grok.com/v1/user?include=subscription";
pub(in super::super) const ENDPOINT: &str =
    "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";
const FIXTURE: &str = "http://127.0.0.1:52989/grok-build-weekly";
const MEDIA: &str = "application/grpc-web+proto";
const LIMIT: usize = 256 * 1024;
const AUTH_EXPIRED: &str = "Grok CLI authentication expired. Please re-authorize.";
const UNAVAILABLE: &str = "Grok Build weekly usage is unavailable for this connection.";

static TIER_LABEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b\w)").expect("Grok CLI subscription label pattern"));
static TIER_SEPARATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[_-]+").expect("Grok CLI subscription separator pattern"));

pub(super) async fn usage(read: &QuotaDelivery, account: &Value) -> Value {
    let Some(token) = supplied(&account["accessToken"]) else {
        return json!({"message":"Grok CLI access token not available."});
    };
    if [&account["expiresAt"], &account["tokenExpiresAt"]]
        .into_iter()
        .find_map(|value| parse::reset_time(value, true))
        .is_some_and(|expiry| expiry <= Utc::now())
    {
        return json!({"message":AUTH_EXPIRED});
    }
    let details = &account["providerSpecificData"];
    if crate::provider_http::fixture() && read.direct() {
        let plan = account_plan(token, details);
        return weekly_quota(read, token, FIXTURE, &plan)
            .await
            .unwrap_or_else(|| json!({"plan":plan,"message":UNAVAILABLE,"quotas":{}}));
    }
    let request = |url: &str| {
        identity_headers(
            read.client.get(url),
            token,
            supplied(&details["email"]),
            supplied(&details["userId"]).or_else(|| supplied(&details["principalId"])),
        )
        .header(header::ACCEPT, "application/json")
    };
    let (billing, user) = tokio::join!(read.send(request(BILLING)), read.send(request(USER)));
    let Ok(billing) = billing else {
        return json!({"message":"Grok CLI usage error: fetch failed"});
    };
    let status = billing.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return json!({"message":AUTH_EXPIRED});
    }
    if !status.is_success() {
        let raw = redact(
            read.redact_relay(billing.text().await.unwrap_or_default()),
            account,
        );
        let detail = raw
            .chars()
            .filter(|character| !character.is_control())
            .take(200)
            .collect::<String>();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return json!({"message":format!("Grok CLI billing API error ({}){suffix}",status.as_u16())});
    }
    let Ok(root) = billing.json::<Value>().await else {
        return json!({"message":"Grok CLI billing response was not JSON."});
    };
    if !root.is_object() && !root.is_array() {
        return json!({"message":"Grok CLI billing response was not JSON."});
    }
    let user = match user {
        Ok(response) if response.status().is_success() => {
            response.json::<Value>().await.unwrap_or_default()
        }
        _ => Value::Null,
    };
    let config = root
        .get("config")
        .filter(|value| value.is_object())
        .unwrap_or(&root);
    let tier = [
        &user["subscriptionTier"],
        &user["subscription_tier"],
        &user["subscription"]["tier"],
        &config["subscriptionTier"],
        &config["subscription_tier"],
    ]
    .into_iter()
    .find(|value| !value.is_null())
    .and_then(supplied);
    let subscribed = tier.is_some_and(|tier| {
        !["free", "none", "null"]
            .iter()
            .any(|free| tier.eq_ignore_ascii_case(free))
    });
    let plan = token_plan(token)
        .map(str::to_owned)
        .or_else(|| tier.map(tier_label))
        .unwrap_or_else(|| {
            if user["hasGrokCodeAccess"] == true {
                "Grok Code".into()
            } else {
                "Grok Build".into()
            }
        });
    let plan = redact(read.redact_relay(plan), account);
    let quotas = billing_quotas(&root, config, subscribed);
    if !quotas.is_empty() {
        return json!({"plan":plan,"quotas":quotas});
    }
    if let Some(weekly) = weekly_quota(read, token, ENDPOINT, &plan).await {
        return weekly;
    }
    let message = if subscribed {
        "Subscription access is active; Grok does not expose a numeric included quota."
    } else {
        "Grok Build connected, but no credit allotment was returned. Free promo may be exhausted."
    };
    json!({"plan":plan,"message":message,"quotas":{}})
}

async fn weekly_quota(
    read: &QuotaDelivery,
    token: &str,
    target: &str,
    plan: &str,
) -> Option<Value> {
    let request = read
        .client
        .post(target)
        .bearer_auth(token)
        .header(header::USER_AGENT, "9Router")
        .header(header::CONTENT_TYPE, MEDIA)
        .header(header::ACCEPT, MEDIA)
        .header("x-grpc-web", "1")
        .body(vec![0; 5]);
    let mut response = read.send(request).await.ok()?;
    if matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) || response
        .headers()
        .get("grpc-status")
        .is_some_and(|status| status == "16")
    {
        return (target == FIXTURE).then(|| json!({"message":AUTH_EXPIRED}));
    }
    if !response.status().is_success()
        || response
            .headers()
            .get("grpc-status")
            .is_some_and(|status| status != "0")
        || response
            .content_length()
            .is_some_and(|length| length > LIMIT as u64)
    {
        return None;
    }
    let mut raw = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if chunk.len() <= LIMIT - raw.len() => raw.extend_from_slice(&chunk),
            Ok(None) => break,
            _ => return None,
        }
    }
    let (used, reset) = weekly(&raw)?;
    Some(json!({"plan":plan,"quotas":{"Weekly SuperGrok": quota(used, 100.0, &json!(reset))}}))
}

fn account_plan(token: &str, details: &Value) -> String {
    if let Some(plan) = token_plan(token) {
        return plan.into();
    }
    if let Some(tier) = supplied(&details["subscriptionTier"]) {
        return tier_label(tier);
    }
    if details["hasGrokCodeAccess"] == true {
        "Grok Code".into()
    } else {
        "Grok Build".into()
    }
}

fn token_plan(token: &str) -> Option<&'static str> {
    let claims = token_claims(token);
    let names = [
        "Free",
        "SuperGrok",
        "X Basic",
        "X Premium",
        "X Premium Plus",
        "SuperGrok Heavy",
        "SuperGrok Lite",
    ];
    let index = match &claims["tier"] {
        Value::Number(number) => number
            .as_f64()
            .filter(|number| (0.0..=6.0).contains(number) && number.fract() == 0.0)
            .map(|number| number as usize),
        Value::String(value) if let [number @ b'0'..=b'6'] = value.as_bytes() => {
            Some(usize::from(*number - b'0'))
        }
        _ => None,
    };
    index.and_then(|index| names.get(index).copied())
}

fn tier_label(tier: &str) -> String {
    let spaced = TIER_SEPARATOR.replace_all(tier, " ");
    TIER_LABEL
        .replace_all(&spaced, |captures: &Captures<'_>| {
            captures[0].to_ascii_uppercase()
        })
        .into_owned()
}

fn billing_quotas(root: &Value, config: &Value, subscribed: bool) -> Map<String, Value> {
    let reset = [
        &config["billingPeriodEnd"],
        &config["billing_period_end"],
        &config["currentPeriod"]["end"],
        reset_field(config),
        &root["billingPeriodEnd"],
        &root["billing_period_end"],
        reset_field(root),
    ]
    .into_iter()
    .map(|value| parse::reset_date(value, true))
    .find(|value| !value.is_null())
    .unwrap_or_default();
    let amount = |keys: &[&str]| {
        quantity(
            [config, root]
                .into_iter()
                .flat_map(|entry| keys.iter().map(move |key| &entry[*key])),
        )
    };
    let mut quotas = Map::new();
    if let Some(total) = amount(&["monthlyLimit", "monthly_limit"]).filter(|value| *value > 0.0) {
        let used = amount(&["includedUsed", "included_used"])
            .or_else(|| amount(&["totalUsed", "total_used"]))
            .unwrap_or_default();
        quotas.insert("Monthly included".into(), quota(used, total, &reset));
    }
    match (amount(&["onDemandCap"]), amount(&["onDemandUsed"])) {
        (Some(cap), used) if cap > 0.0 => {
            quotas.insert(
                "On-demand".into(),
                quota(used.unwrap_or_default(), cap, &reset),
            );
        }
        (Some(0.0), Some(_)) if !subscribed => {
            quotas.insert("On-demand".into(), quota(1.0, 1.0, &reset));
        }
        _ => (),
    }
    if let Some(balance) = amount(&["prepaidBalance"]).filter(|value| *value > 0.0) {
        quotas.insert("Prepaid".into(), quota(0.0, balance, &Value::Null));
    }
    if let Some(used) = quantity([
        &config["creditUsagePercent"],
        &config["credit_usage_percent"],
        &root["creditUsagePercent"],
    ])
    .filter(|value| *value >= 0.0)
    {
        quotas.insert(
            "Weekly SuperGrok".into(),
            quota(used.min(100.0), 100.0, &reset),
        );
    }
    for bag in [
        &root["credits"],
        &root["creditBalance"],
        &root["usage"],
        &config["credits"],
        &config["includedCredits"],
        &config["subscriptionCredits"],
    ]
    .into_iter()
    .filter(|value| value.is_object())
    {
        let remaining = quantity([&bag["remaining"], &bag["balance"], &bag["left"]]);
        if let Some(total) = quantity([
            &bag["total"],
            &bag["limit"],
            &bag["cap"],
            &bag["allocation"],
            &bag["amount"],
        ])
        .filter(|total| *total > 0.0)
        {
            let used = quantity([&bag["used"], &bag["spent"], &bag["consumed"]])
                .or_else(|| remaining.map(|balance| total - balance))
                .unwrap_or_default();
            let bag_reset = [&bag["resetAt"], &bag["resetsAt"], &bag["end"]]
                .into_iter()
                .find(|value| truthy(value))
                .map_or(Value::Null, |value| parse::reset_date(value, true));
            quotas.insert(
                "Credits".into(),
                quota(
                    used,
                    total,
                    if bag_reset.is_null() {
                        &reset
                    } else {
                        &bag_reset
                    },
                ),
            );
            break;
        }
        if let Some(balance) = remaining.filter(|value| *value >= 0.0) {
            let (used, total) = if balance > 0.0 {
                (0.0, balance)
            } else {
                (1.0, 1.0)
            };
            let mut row = quota(used, total, &reset);
            row["used"] = json!(0);
            quotas.insert("Credits".into(), row);
            break;
        }
    }
    quotas
}

fn quantity<'a>(values: impl IntoIterator<Item = &'a Value>) -> Option<f64> {
    let value = values.into_iter().find(|value| !value.is_null())?;
    parse::finite_number(value.get("val").unwrap_or(value))
}

fn reset_field(value: &Value) -> &Value {
    [&value["resetAt"], &value["resetsAt"], &value["periodEnd"]]
        .into_iter()
        .find(|value| truthy(value))
        .unwrap_or(&Value::Null)
}

fn quota(used: f64, total: f64, reset: &Value) -> Value {
    let used = used.max(0.0);
    json!({"used":parse::number(used),"total":parse::number(total),
        "remainingPercentage":parse::number((total-used).max(0.0)/total*100.0),
        "resetAt":reset,"unlimited":false})
}

fn weekly(raw: &[u8]) -> Option<(f64, Option<String>)> {
    let message = grpc_data(raw)?;
    let Field::Chunk(credits) = field(message, 1).ok()?? else {
        return None;
    };
    let ratio = match field(credits, 1).ok()? {
        None => 0.0,
        Some(Field::Fixed32(bits)) => f32::from_bits(bits) as f64,
        Some(Field::Fixed64(bits)) => f64::from_bits(bits),
        _ => return None,
    };
    if !ratio.is_finite() || ratio < 0.0 {
        return None;
    }
    let reset = match field(credits, 5).ok()? {
        Some(Field::Chunk(timestamp)) => reset_at(timestamp),
        _ => None,
    };
    Some(((ratio * 100.0).min(100.0).round(), reset))
}

fn grpc_data(mut bytes: &[u8]) -> Option<&[u8]> {
    if !matches!(bytes.first(), Some(0 | 1 | 0x80 | 0x81)) {
        return Some(bytes);
    }
    let mut data = None;
    while !bytes.is_empty() {
        let ([flag, a, b, c, d], rest) = bytes.split_first_chunk::<5>()?;
        let size = u32::from_be_bytes([*a, *b, *c, *d]) as usize;
        let (content, remaining) = rest.split_at_checked(size)?;
        match flag {
            0 if data.is_none() => data = Some(content),
            0x80 if remaining.is_empty() && trailers_ok(content) => (),
            _ => return None,
        }
        bytes = remaining;
    }
    data
}

fn trailers_ok(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| {
        text.lines().all(|line| {
            line.split_once(':').is_none_or(|(name, value)| {
                !name.eq_ignore_ascii_case("grpc-status") || value.trim() == "0"
            })
        })
    })
}

fn reset_at(timestamp: &[u8]) -> Option<String> {
    let read = |tag| match field(timestamp, tag).ok()? {
        Some(Field::Varint(value)) => i64::try_from(value).ok(),
        _ => Some(0),
    };
    let milliseconds = read(1)?
        .checked_mul(1000)?
        .checked_add(read(2)?.checked_add(500_000)? / 1_000_000)?;
    DateTime::<Utc>::from_timestamp_millis(milliseconds)
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
}

enum Field<'a> {
    Varint(u64),
    Fixed32(u32),
    Fixed64(u64),
    Chunk(&'a [u8]),
}

fn field(mut bytes: &[u8], tag: u64) -> Result<Option<Field<'_>>, ()> {
    let mut matched = None;
    while !bytes.is_empty() {
        let key = varint(&mut bytes)?;
        let field = match key & 7 {
            0 => Field::Varint(varint(&mut bytes)?),
            1 => {
                let (raw, rest) = bytes.split_first_chunk::<8>().ok_or(())?;
                bytes = rest;
                Field::Fixed64(u64::from_le_bytes(*raw))
            }
            2 => {
                let length = usize::try_from(varint(&mut bytes)?).map_err(|_| ())?;
                let (raw, rest) = bytes.split_at_checked(length).ok_or(())?;
                bytes = rest;
                Field::Chunk(raw)
            }
            5 => {
                let (raw, rest) = bytes.split_first_chunk::<4>().ok_or(())?;
                bytes = rest;
                Field::Fixed32(u32::from_le_bytes(*raw))
            }
            _ => return Err(()),
        };
        if key >> 3 == 0 {
            return Err(());
        }
        if key >> 3 == tag {
            matched = Some(field);
        }
    }
    Ok(matched)
}

fn varint(bytes: &mut &[u8]) -> Result<u64, ()> {
    let mut value = 0;
    for shift in (0..=63).step_by(7) {
        let (byte, rest) = bytes.split_first().ok_or(())?;
        *bytes = rest;
        if shift == 63 && *byte > 1 {
            return Err(());
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(())
}
