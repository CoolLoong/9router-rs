use std::sync::LazyLock;

use regex::Regex;
use reqwest::{Response, StatusCode, header};
use serde_json::{Map, Value, json};

use super::{QuotaDelivery, parse, redact, supplied, truthy};
use crate::provider_oauth::kimi_portal::{device_headers, saved_device_id};

const PLAN: &str = "Kimi Coding";
const MAX_BODY: usize = 1024 * 1024;
static PERMISSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("(?i)permission_denied|do not have permission|subscribe")
        .expect("Kimi permission message pattern")
});

pub(super) async fn usage(delivery: &QuotaDelivery, url: &str, account: &Value) -> Value {
    let request = delivery.client.get(url);
    let request = if let Some(key) = supplied(&account["apiKey"]) {
        request.header("x-api-key", key)
    } else if let Some((token, headers)) =
        supplied(&account["accessToken"]).zip(saved_device_id(account).and_then(device_headers))
    {
        request.bearer_auth(token).headers(headers)
    } else {
        return json!({"message":"Kimi access token or API key not available."});
    };
    let request = request
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json");
    let response = match delivery.send(request).await {
        Ok(response) => response,
        Err(reason) => {
            return json!({"message":format!("Kimi Coding connected. Unable to fetch usage: {reason}")});
        }
    };
    let status = response.status();
    let raw = delivery.redact_relay(read(response).await.unwrap_or_default());
    let data = if raw.is_empty() {
        Ok(json!({}))
    } else {
        serde_json::from_str::<Value>(&raw)
    };
    if !status.is_success() {
        let message = error(status, data.as_ref().unwrap_or(&Value::Null), &raw, account);
        return json!({"plan":PLAN,"message":redact(message,account)});
    }
    match data {
        Ok(data) => normalized(&data),
        Err(_) => {
            json!({"plan":PLAN,"message":"Kimi Coding connected. Invalid JSON response from API."})
        }
    }
}

fn error(status: StatusCode, data: &Value, raw: &str, account: &Value) -> String {
    if status == StatusCode::UNAUTHORIZED {
        return "Kimi authentication expired. Please re-authorize.".into();
    }
    let detail = &data["details"][0];
    let debug = present([&detail["debug"], &data["debug"]]);
    let reason = present([&debug["reason"], &data["reason"]]);
    let localized = present([
        &debug["localizedMessage"]["message"],
        &detail["localizedMessage"]["message"],
        &data["message"],
    ]);
    let text = localized.as_str().unwrap_or_default();
    if status == StatusCode::FORBIDDEN
        && (reason == "REASON_FEATURE_NO_PERMISSION"
            || PERMISSION.is_match(raw)
            || PERMISSION.is_match(text))
    {
        if !text.is_empty() {
            return text.into();
        }
        return "Kimi connected, but this account has no permission to view usage. Subscribe to Kimi Code to access quota.".into();
    }
    let source = if text.is_empty() { raw } else { text };
    let snippet: String = redact(source.to_owned(), account)
        .chars()
        .take(100)
        .collect();
    let mut message = format!("Kimi Coding connected. API Error {}", status.as_u16());
    if !snippet.is_empty() {
        message.push_str(": ");
        message.push_str(&snippet);
    }
    message
}

fn normalized(data: &Value) -> Value {
    let mut rows = Map::new();
    let usage = &data["usage"];
    let limit = parse::finite(nonnull(usage, "limit", "Limit"));
    if limit > 0.0 {
        let used = parse::finite(nonnull(usage, "used", "Used")).max(0.0);
        let remaining = parse::finite_number(nonnull(usage, "remaining", "Remaining"));
        let reset = present([
            &usage["resetTime"],
            &usage["ResetTime"],
            &usage["reset_at"],
            &usage["resetAt"],
        ]);
        rows.insert("Weekly".into(), row(used, limit, remaining, reset));
    }
    for item in data["limits"].as_array().into_iter().flatten() {
        let detail = &item["detail"];
        let limit = parse::finite(nonnull(detail, "limit", "Limit"));
        if limit <= 0.0 {
            continue;
        }
        let remaining =
            parse::finite_number(nonnull(detail, "remaining", "Remaining")).unwrap_or(limit);
        let reset = present([
            &detail["resetTime"],
            &detail["reset_at"],
            &detail["resetAt"],
        ]);
        rows.insert(
            "Ratelimit".into(),
            row((limit - remaining).max(0.0), limit, Some(remaining), reset),
        );
    }
    let level = &data["user"]["membership"]["level"];
    let level = if truthy(level) {
        parse::string(level)
    } else {
        String::new()
    };
    let plan = match level.as_str() {
        "LEVEL_BASIC" => "Moderato".into(),
        "LEVEL_INTERMEDIATE" => "Allegretto".into(),
        "LEVEL_ADVANCED" => "Allegro".into(),
        "LEVEL_STANDARD" => "Vivace".into(),
        name => name.strip_prefix("LEVEL_").unwrap_or(name).to_lowercase(),
    };
    let plan = if plan.is_empty() { PLAN } else { &plan };
    if rows.is_empty() {
        json!({"plan":plan,"message":"Kimi Coding connected. Usage tracked per request."})
    } else {
        json!({"plan":plan,"quotas":rows})
    }
}

fn row(used: f64, total: f64, remaining: Option<f64>, reset: &Value) -> Value {
    let percentage = remaining.unwrap_or(total - used).max(0.0) / total * 100.0;
    json!({"used":parse::number(used),"total":parse::number(total),
        "remainingPercentage":parse::number(percentage),"resetAt":parse::reset_date(reset,true),"unlimited":false})
}

fn present<'a>(values: impl IntoIterator<Item = &'a Value>) -> &'a Value {
    values
        .into_iter()
        .find(|value| truthy(value))
        .unwrap_or(&Value::Null)
}

fn nonnull<'a>(value: &'a Value, first: &str, second: &str) -> &'a Value {
    value
        .get(first)
        .filter(|value| !value.is_null())
        .unwrap_or(&value[second])
}

async fn read(mut response: Response) -> Option<String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_BODY - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}
