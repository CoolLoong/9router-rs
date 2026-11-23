use std::sync::LazyLock;

use chrono::{SecondsFormat, TimeDelta, Utc};
use regex::Regex;
use reqwest::{RequestBuilder, Response, StatusCode, header};
use serde_json::{Map, Value, json};

use super::{QuotaDelivery, coerce_quantity, parse, redact, supplied};

static GROQ_DURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([0-9]+(?:\.[0-9]+)?)(ms|s|m|h)").expect("Groq duration pattern")
});

pub(super) async fn groq(request: RequestBuilder, account: &Value, read: &QuotaDelivery) -> Value {
    let Some(key) = supplied(&account["apiKey"]) else {
        return json!({"message":"Groq API key not available. Add a key to view usage."});
    };
    let request = request
        .bearer_auth(key)
        .header(header::ACCEPT, "application/json");
    let Ok(response) = read.send(request).await else {
        return json!({"message":"Groq error: fetch failed"});
    };
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return json!({"plan":"Groq","message":"Groq authentication failed. Check the API key."});
    }
    if !status.is_success() {
        let detail = error_suffix(response, account, read, 120).await;
        return json!({"plan":"Groq","message":format!("Groq usage API error ({}){detail}",status.as_u16())});
    }
    let now = Utc::now();
    let quotas: Map<String, Value> = [("requests", "Requests"), ("tokens", "Tokens")]
        .into_iter()
        .filter_map(|(suffix, label)| {
            let headers = response.headers();
            let measure = |part: &str| {
                let value = headers
                    .get(format!("x-ratelimit-{part}-{suffix}"))?
                    .to_str()
                    .ok()?;
                coerce_quantity(&json!(value))
            };
            let limit = measure("limit")?;
            let remaining = measure("remaining")?;
            let reset = headers
                .get(format!("x-ratelimit-reset-{suffix}"))
                .and_then(|header| header.to_str().ok())
                .and_then(groq_duration)
                .and_then(|duration| now.checked_add_signed(duration))
                .map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true));
            Some((
                label.into(),
                json!({"used":parse::number((limit-remaining).max(0.0)),
                "total":parse::number(limit),"resetAt":reset,"unlimited":false}),
            ))
        })
        .collect();
    if quotas.is_empty() {
        return json!({"plan":"Groq","message":"Groq connected. No rate-limit data reported for this key yet.","quotas":{}});
    }
    json!({"plan":"Groq","quotas":quotas})
}

fn groq_duration(source: &str) -> Option<TimeDelta> {
    let mut parts = GROQ_DURATION.captures_iter(source).peekable();
    parts.peek()?;
    let milliseconds: f64 = parts
        .map(|capture| {
            let scale = match &capture[2] {
                "h" => 3_600_000.0,
                "m" => 60_000.0,
                "s" => 1_000.0,
                _ => 1.0,
            };
            capture[1].parse::<f64>().expect("matched decimal") * scale
        })
        .sum();
    if !milliseconds.is_finite() || milliseconds >= i64::MAX as f64 {
        return None;
    }
    TimeDelta::try_milliseconds(milliseconds as i64)
}

pub(super) async fn vercel(
    request: RequestBuilder,
    account: &Value,
    read: &QuotaDelivery,
) -> Value {
    let Some(key) = supplied(&account["apiKey"]) else {
        return json!({"message":"Vercel AI Gateway API key not available."});
    };
    let request = request
        .bearer_auth(key)
        .header(header::ACCEPT, "application/json");
    let Ok(response) = read.send(request).await else {
        return json!({"message":"Vercel AI Gateway error: fetch failed"});
    };
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return json!({"message":"Vercel AI Gateway API key invalid or expired."});
    }
    if !status.is_success() {
        let detail = error_suffix(response, account, read, 200).await;
        return json!({"message":format!("Vercel AI Gateway credits API error ({}){detail}",status.as_u16())});
    }
    let Ok(data) = response.json::<Value>().await else {
        return json!({"message":"Vercel AI Gateway error: Invalid credits response"});
    };
    let balance = coerce_quantity(&data["balance"]).unwrap_or(0.0);
    let used = coerce_quantity(&data["total_used"]).unwrap_or(0.0);
    if balance <= 0.0 && used <= 0.0 {
        return json!({"plan":"Pay-as-you-go","message":"Vercel AI Gateway connected. No credit allocation found (BYOK or unfunded account).","quotas":{}});
    }
    json!({"plan":"Pay-as-you-go","quotas":{
        "Used (USD)":{"used":parse::number(used),"total":0,"remaining":0,"remainingPercentage":100,"unlimited":true},
        "Remaining (USD)":{"used":parse::number(balance),"total":5,"remaining":parse::number(balance),
            "remainingPercentage":parse::number((balance/5.0)*100.0),"unlimited":false},
    }})
}

async fn error_suffix(
    mut response: Response,
    account: &Value,
    read: &QuotaDelivery,
    max: usize,
) -> String {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let remaining = (32 * 1024usize).saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..remaining.min(chunk.len())]);
        if bytes.len() >= 32 * 1024 {
            break;
        }
    }
    let raw = redact(
        read.redact_relay(String::from_utf8_lossy(&bytes).into_owned()),
        account,
    );
    let text: String = raw.chars().take(max).collect();
    if text.is_empty() {
        text
    } else {
        format!(": {text}")
    }
}
