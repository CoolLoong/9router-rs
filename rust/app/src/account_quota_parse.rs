use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use serde_json::{Map, Value, json};

use crate::provider_connections::truthy;

pub(super) fn codex_usage(data: &Value) -> Value {
    let normal = first([
        &data["rate_limit"],
        &data["rate_limits"],
        &data["rate_limits_by_limit_id"]["codex"],
    ]);
    let review = extra_codex(data, false);
    let spark = extra_codex(data, true);
    let mut quotas = Map::new();
    for (prefix, snapshot) in [("", normal), ("review_", review), ("spark_", spark)] {
        if !snapshot.is_object() {
            continue;
        }
        let rate = rate_body(snapshot);
        for (name, key, fallback) in [
            ("session", "primary_window", "primary"),
            ("weekly", "secondary_window", "secondary"),
        ] {
            let window = first([
                &rate[key],
                &rate[fallback],
                &snapshot[key],
                &snapshot[fallback],
            ]);
            if truthy(window) {
                let used = finite(coalesce(&window["used_percent"], &window["percent_used"]))
                    .clamp(0.0, 100.0);
                let reset = [
                    &window["reset_at"],
                    &window["resets_at"],
                    &window["resetAt"],
                ]
                .into_iter()
                .find(|value| !value.is_null())
                .unwrap_or(&Value::Null);
                quotas.insert(
                    format!("{prefix}{name}"),
                    json!({
                        "used": number(used), "total": 100, "remaining": number(100.0 - used),
                        "resetAt": reset_date(reset, true), "unlimited": false,
                    }),
                );
            }
        }
    }
    json!({
        "plan": value_or(first([&data["plan_type"], &data["summary"]["plan"]]), json!("unknown")),
        "limitReached": limit_reached(normal),
        "reviewLimitReached": limit_reached(review),
        "sparkLimitReached": limit_reached(spark),
        "resetCredits": {
            "availableCount": number(finite(&data["rate_limit_reset_credits"]["available_count"]).max(0.0))
        },
        "quotas": quotas,
    })
}

fn rate_body(snapshot: &Value) -> &Value {
    if snapshot["rate_limit"].is_object() || snapshot["rate_limit"].is_array() {
        &snapshot["rate_limit"]
    } else {
        snapshot
    }
}

fn limit_reached(snapshot: &Value) -> Value {
    value_or(&rate_body(snapshot)["limit_reached"], json!(false))
}

fn extra_codex(data: &Value, spark: bool) -> &Value {
    let direct = if spark {
        first([
            &data["spark_rate_limit"],
            &data["gpt_5_3_codex_spark_rate_limit"],
        ])
    } else {
        first([&data["code_review_rate_limit"], &data["review_rate_limit"]])
    };
    if truthy(direct) {
        return direct;
    }
    let by_id = &data["rate_limits_by_limit_id"];
    if by_id.is_object() {
        return if spark {
            first([
                &by_id["gpt-5.3-codex-spark"],
                &by_id["gpt_5_3_codex_spark"],
                &by_id["spark"],
            ])
        } else {
            first([
                &by_id["code_review"],
                &by_id["codex_review"],
                &by_id["review"],
            ])
        };
    }
    data["additional_rate_limits"]
        .as_array()
        .and_then(|limits| {
            limits.iter().find(|limit| {
                let name = first([
                    &limit["limit_name"],
                    &limit["metered_feature"],
                    &limit["id"],
                ]);
                let name = name.as_str().unwrap_or_default().to_ascii_lowercase();
                name.contains(if spark { "spark" } else { "review" })
            })
        })
        .unwrap_or(&Value::Null)
}

pub(super) fn codex_credits(data: &Value) -> Value {
    let credits = data["credits"].as_array().into_iter().flatten().map(|credit| {
        let status = &credit["status"];
        json!({
            "status": if truthy(status) { string(status) } else { "unknown".into() },
            "grantedAt": reset_date(coalesce(&credit["granted_at"], &credit["grantedAt"]), false),
            "expiresAt": reset_date(coalesce(&credit["expires_at"], &credit["expiresAt"]), false),
        })
    }).collect::<Vec<_>>();
    json!({
        "availableCount": number(finite(coalesce(&data["available_count"], &data["availableCount"])).max(0.0)),
        "credits": credits,
    })
}

pub(super) fn claude_usage(data: &Value) -> Value {
    let mut quotas = Map::new();
    for (key, label) in [("five_hour", "session (5h)"), ("seven_day", "weekly (7d)")] {
        if let Some(used) = data[key]["utilization"].as_f64() {
            quotas.insert(label.into(), claude_window(used, &data[key]["resets_at"]));
        }
    }
    for (key, value) in data.as_object().into_iter().flatten() {
        if let Some(model) = key.strip_prefix("seven_day_")
            && let Some(used) = value["utilization"].as_f64()
        {
            quotas.insert(
                format!("weekly {model} (7d)"),
                claude_window(used, &value["resets_at"]),
            );
        }
    }
    for limit in data["limits"].as_array().into_iter().flatten() {
        if limit["kind"] == "weekly_scoped"
            && let Some(used) = limit["percent"].as_f64()
        {
            let name = limit["scope"]["model"]["display_name"]
                .as_str()
                .unwrap_or_default();
            let name = name.trim().to_lowercase();
            if !name.is_empty() {
                quotas.insert(
                    format!("weekly {name} (7d)"),
                    claude_window(used.clamp(0.0, 100.0), &limit["resets_at"]),
                );
            }
        }
    }
    json!({
        "plan": "Claude Code", "extraUsage": data["extra_usage"],
        "resetCredits": claude_grants(&data["cedar_ember"]), "quotas": quotas,
    })
}

fn claude_window(used: f64, reset: &Value) -> Value {
    let remaining = number((100.0 - used).max(0.0));
    json!({
        "used": number(used), "total": 100, "remaining": remaining,
        "remainingPercentage": remaining, "resetAt": reset_date(reset, true),
        "unlimited": false,
    })
}

fn claude_grants(block: &Value) -> Value {
    let Some(all) = block["grants"]
        .as_array()
        .filter(|_| truthy(&block["eligible"]))
    else {
        return Value::Null;
    };
    let available = all
        .iter()
        .filter(|grant| {
            truthy(&grant["id"])
                && !truthy(&grant["paused"])
                && grant_number(&grant["resets_left"]) > 0.0
        })
        .collect::<Vec<_>>();
    let next = available
        .iter()
        .copied()
        .find(|grant| grant["id"] == block["next_grant_id"])
        .or_else(|| available.first().copied())
        .unwrap_or(&Value::Null);
    let grants = all
        .iter()
        .filter(|grant| truthy(&grant["id"]))
        .map(|grant| {
            json!({
                "id": grant["id"], "label": value_or(&grant["label"], json!("")),
                "resetsLeft": number(grant_number(&grant["resets_left"])),
                "resetsTotal": number(grant_number(&grant["resets_total"])),
                "startsAt": value_or(&grant["starts_at"], Value::Null),
                "endsAt": value_or(&grant["ends_at"], Value::Null),
                "clears": grant["clears"].as_array().cloned().unwrap_or_default(),
                "paused": grant["paused"] == true,
                "usableNow": grant["usable_now"] == true,
                "useRequiresLimit": grant["use_requires_limit"] != false,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "availableCount": number(available.iter().map(|grant| grant_number(&grant["resets_left"])).sum()),
        "nextGrantId": value_or(&next["id"], Value::Null),
        "expiresAt": value_or(&next["ends_at"], Value::Null),
        "clears": value_or(&next["clears"], json!([])),
        "cooldownUntil": value_or(&block["cooldown_until"], Value::Null),
        "weeklyResetsAt": value_or(&block["weekly_resets_at"], Value::Null),
        "grants": grants,
    })
}

fn grant_number(value: &Value) -> f64 {
    if value == true { 1.0 } else { finite(value) }
}

pub(super) fn finite(value: &Value) -> f64 {
    finite_number(value).unwrap_or(0.0)
}

pub(super) fn finite_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| {
            let text = value.as_str()?.trim();
            for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
                if let Some(digits) = text
                    .get(..2)
                    .filter(|value| value.eq_ignore_ascii_case(prefix))
                    .map(|_| &text[2..])
                {
                    return u64::from_str_radix(digits, radix)
                        .ok()
                        .map(|number| number as f64);
                }
            }
            text.parse().ok()
        })
        .filter(|value| value.is_finite())
}

pub(super) fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value < i64::MAX as f64 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

pub(super) fn reset_date(value: &Value, numeric_string: bool) -> Value {
    reset_time(value, numeric_string)
        .map(|date| json!(date.to_rfc3339_opts(SecondsFormat::Millis, true)))
        .unwrap_or_default()
}

pub(super) fn reset_time(value: &Value, numeric_string: bool) -> Option<DateTime<Utc>> {
    if !truthy(value) {
        return None;
    }
    let epoch = value.as_f64().or_else(|| {
        let string = value.as_str()?;
        (numeric_string && !string.is_empty() && string.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| string.parse::<f64>().ok())
            .flatten()
    });
    if let Some(epoch) = epoch {
        let ms = if epoch < 1e12 { epoch * 1000.0 } else { epoch };
        if !ms.is_finite() || ms.abs() > 8.64e15 {
            return None;
        }
        DateTime::<Utc>::from_timestamp_millis(ms as i64)
    } else {
        value.as_str().and_then(parse_date)
    }
}

fn parse_date(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|day| day.and_hms_opt(0, 0, 0))
                .map(|date| date.and_utc())
        })
        .or_else(|| {
            ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
                .into_iter()
                .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
                .and_then(|date| Local.from_local_datetime(&date).earliest())
                .map(|date| date.with_timezone(&Utc))
        })
        .or_else(|| {
            if value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            NaiveDate::from_ymd_opt(value.parse().ok()?, 1, 1)
                .and_then(|day| day.and_hms_opt(0, 0, 0))
                .map(|date| date.and_utc())
        })
}

fn first<'a>(values: impl IntoIterator<Item = &'a Value>) -> &'a Value {
    values
        .into_iter()
        .find(|value| truthy(value))
        .unwrap_or(&Value::Null)
}

fn coalesce<'a>(first: &'a Value, second: &'a Value) -> &'a Value {
    if first.is_null() { second } else { first }
}

pub(super) fn value_or(value: &Value, fallback: Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        fallback
    }
}

pub(super) fn string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) => "[object Object]".into(),
        _ => value.to_string(),
    }
}
