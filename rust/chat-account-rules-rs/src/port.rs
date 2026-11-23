//! The Rust port of `open-sse/services/accountFallback.js`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

use serde_json::{Value, json};

use crate::js::{as_number, is_undefined, number, string_units};

const LONG: u64 = 120_000;
const SHORT: u64 = 5_000;
const TRANSIENT: u64 = 30_000;
const MAX_RATE_LIMIT_MS: u64 = 1_800_000;

const BACKOFF_TEXT: &[&str] = &[
    "rate limit",
    "too many requests",
    "quota exceeded",
    "insufficient credit",
    "credit balance is too low",
    "capacity",
    "overloaded",
];

/// Port of `getQuotaCooldown` (`open-sse/services/accountFallback.js:9`).
pub fn get_quota_cooldown(backoff_level: Option<f64>) -> Result<Value, String> {
    Ok(number(quota_cooldown(backoff_level.unwrap_or_default())))
}

fn quota_cooldown(backoff_level: f64) -> f64 {
    if backoff_level.is_nan() {
        return f64::NAN;
    }
    let level = (backoff_level - 1.0).max(0.0);
    (2_000.0 * libm::pow(2.0, level)).min(300_000.0)
}

fn backoff(level: f64) -> Value {
    let new_level = if level.is_nan() {
        f64::NAN
    } else {
        (level + 1.0).min(15.0)
    };
    json!({
        "shouldFallback": true,
        "cooldownMs": number(quota_cooldown(new_level)),
        "newBackoffLevel": number(new_level),
    })
}

fn fixed(milliseconds: u64) -> Value {
    json!({ "shouldFallback": true, "cooldownMs": milliseconds })
}

fn text_match(text: &str, provider: Option<&str>, level: f64) -> Option<Value> {
    let (ms, scoped) = match provider {
        Some("codex")
            if text.contains("model is not supported when using codex with a chatgpt account") =>
        {
            (MAX_RATE_LIMIT_MS, false)
        }
        Some("codebuddy-intl")
            if text.contains("request illegal") || text.contains("conversation id missing") =>
        {
            return Some(backoff(level));
        }
        Some("nvidia") if text.contains("degraded") => (900_000, true),
        _ if text.contains("no credentials") => (LONG, false),
        _ if text.contains("request not allowed") => (SHORT, false),
        _ if text.contains("improperly formed request") => (LONG, false),
        _ if BACKOFF_TEXT.iter().any(|literal| text.contains(literal)) => {
            return Some(backoff(level));
        }
        _ => return None,
    };
    Some(json!({ "shouldFallback": true, "cooldownMs": ms, "modelScoped": scoped }))
}

/// Port of `checkFallbackError` (`open-sse/services/accountFallback.js:29`).
pub fn check_fallback_error(
    status: Value,
    error_text: Value,
    backoff_level: Option<f64>,
    provider: Option<Value>,
) -> Result<Value, String> {
    let level = backoff_level.unwrap_or_default();
    let provider = provider.as_ref().and_then(Value::as_str);
    let error = string_units(&error_text)
        .map(|units| String::from_utf16_lossy(&units))
        .unwrap_or_else(|| error_text.to_string());
    if let Some(hit) = text_match(&error.to_lowercase(), provider, level) {
        return Ok(hit);
    }

    let exact = as_number(&status);
    match exact {
        Some(401.0 | 402.0 | 403.0 | 404.0) => return Ok(fixed(LONG)),
        Some(429.0) => return Ok(backoff(level)),
        _ => {}
    }
    if (400.0..500.0).contains(&comparable_number(&status)?) {
        return Ok(json!({ "shouldFallback": false, "cooldownMs": 0, "requestScoped": true }));
    }
    Ok(fixed(TRANSIENT))
}

fn comparable_number(value: &Value) -> Result<f64, String> {
    if let Some(value) = as_number(value) {
        return Ok(value);
    }
    if let Some(text) = string_units(value) {
        return Ok(numeric_string(&String::from_utf16_lossy(&text)));
    }
    Ok(match value {
        Value::Null => 0.0,
        Value::Bool(value) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        Value::Array(items) => numeric_string(&array_to_string(items)?),
        Value::Object(fields) if fields.contains_key("toString") => {
            return Err("Cannot convert object to primitive value".into());
        }
        _ => f64::NAN,
    })
}

fn array_to_string(items: &[Value]) -> Result<String, String> {
    let mut parts = Vec::with_capacity(items.len());
    for value in items {
        let text = if value.is_null() || is_undefined(value) {
            String::new()
        } else if let Some(units) = string_units(value) {
            String::from_utf16_lossy(&units)
        } else if let Some(number) = as_number(value) {
            number.to_string()
        } else {
            match value {
                Value::Array(items) => array_to_string(items)?,
                Value::Bool(value) => value.to_string(),
                Value::Object(fields) if fields.contains_key("toString") => {
                    return Err("Cannot convert object to primitive value".into());
                }
                _ => "[object Object]".to_owned(),
            }
        };
        parts.push(text);
    }
    Ok(parts.join(","))
}

fn numeric_string(text: &str) -> f64 {
    let text = text.trim_matches(|c: char| c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'));
    if text.is_empty() {
        return 0.0;
    }
    for (prefixes, base) in [(["0x", "0X"], 16), (["0o", "0O"], 8), (["0b", "0B"], 2)] {
        if let Some(rest) = prefixes.iter().find_map(|prefix| text.strip_prefix(prefix)) {
            return u64::from_str_radix(rest, base)
                .map(|n| n as f64)
                .unwrap_or(f64::NAN);
        }
    }
    text.parse().unwrap_or(f64::NAN)
}
