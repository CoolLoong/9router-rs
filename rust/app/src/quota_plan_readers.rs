use std::sync::LazyLock;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use regex::Regex;
use reqwest::{RequestBuilder, StatusCode, header};
use serde_json::{Map, Value, json};

use super::{QuotaDelivery, coerce_quantity, parse, redact, supplied, truthy};

static MINIMAX_AUTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("(?i)token plan|coding plan|invalid api key|invalid key|unauthorized|inactive")
        .expect("MiniMax authentication error pattern")
});
static WORD_INITIAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b\w)").expect("ASCII word start"));

pub(super) async fn glm(request: RequestBuilder, account: &Value, read: &QuotaDelivery) -> Value {
    let Some(key) = supplied(&account["apiKey"]).or_else(|| supplied(&account["accessToken"]))
    else {
        return json!({"message":"GLM API key not available."});
    };
    let request = request
        .bearer_auth(key)
        .header(header::ACCEPT, "application/json");
    let Ok(response) = read.send(request).await else {
        return json!({"message":"GLM error: fetch failed"});
    };
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return json!({"message":"GLM API key invalid or expired."});
    }
    if !status.is_success() {
        return json!({"message":format!("GLM quota API error ({}).",status.as_u16())});
    }
    let Ok(body) = response.json::<Value>().await else {
        return json!({"message":"GLM error: Invalid quota response"});
    };
    let data = &body["data"];
    let quotas: Map<String, Value> = data["limits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|limit| {
            let quota_type = limit["type"].as_str()?;
            if !matches!(quota_type, "TOKENS_LIMIT" | "CREDIT_LIMIT") {
                return None;
            }
            let count = limit
                .get("number")
                .map_or_else(|| "undefined".into(), parse::string);
            let label = match limit["unit"].as_f64() {
                Some(3.0) => format!("Session ({count}h)"),
                Some(6.0) => "Weekly (7d)".into(),
                _ if quota_type == "TOKENS_LIMIT" => "Tokens".into(),
                _ => format!("Limit ({count})"),
            };
            let used = quantity(&limit["percentage"]);
            let remaining = parse::number((100.0 - used).max(0.0));
            let reset = quantity(&limit["nextResetTime"]);
            let reset = iso((reset > 0.0 && reset <= 8.64e15)
                .then(|| DateTime::from_timestamp_millis(reset as i64))
                .flatten());
            Some((
                label,
                json!({"used":parse::number(used),"total":100,"remaining":remaining,
            "remainingPercentage":remaining,"resetAt":reset,"unlimited":false}),
            ))
        })
        .collect();
    let plan = data["level"]
        .as_str()
        .filter(|level| !level.is_empty())
        .map_or_else(
            || "Unknown".to_owned(),
            |level| {
                let mut chars = level.chars();
                let head = chars.next().expect("non-empty level");
                head.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
            },
        );
    json!({"plan":plan,"quotas":quotas})
}

pub(super) async fn minimax(read: &QuotaDelivery, targets: &[String; 2], account: &Value) -> Value {
    let Some(key) = supplied(&account["apiKey"]) else {
        return json!({"message":"MiniMax API key not available."});
    };
    let mut failed = false;
    for (index, url) in targets.iter().enumerate() {
        let request = read
            .client
            .get(url)
            .bearer_auth(key)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/json");
        let response = match read.send(request).await {
            Ok(response) => response,
            Err(reason) if !read.direct() => {
                return json!({"message":format!("MiniMax connected. Unable to fetch usage: {reason}")});
            }
            Err(_) => {
                failed = true;
                continue;
            }
        };
        let status = response.status();
        let Ok(raw) = response.text().await else {
            if !read.direct() {
                return json!({"message":"MiniMax connected. Unable to fetch usage: fetch failed"});
            }
            failed = true;
            continue;
        };
        let body = serde_json::from_str::<Value>(&raw).unwrap_or_default();
        let base = nonnull(&body, "base_resp", "baseResp");
        let code = quantity(nonnull(base, "status_code", "statusCode"));
        let message = nonnull(base, "status_msg", "statusMsg");
        let message = if message.is_null() {
            String::new()
        } else {
            parse::string(message)
        };
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
            || code == 1004.0
            || MINIMAX_AUTH.is_match(&message)
            || MINIMAX_AUTH.is_match(&raw)
        {
            return json!({"message":"MiniMax API key invalid or inactive. Use an active Token/Coding Plan key."});
        }
        if !status.is_success() {
            if index + 1 < targets.len()
                && (status.is_server_error()
                    || matches!(
                        status,
                        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                    ))
            {
                continue;
            }
            return json!({"message":format!("MiniMax connected. MiniMax usage endpoint error ({})",status.as_u16())});
        }
        if code != 0.0 {
            let message = if message.trim().is_empty() {
                "Upstream quota API error".into()
            } else {
                redact(read.redact_relay(message.trim().to_owned()), account)
            };
            return json!({"message":format!("MiniMax connected. {message}")});
        }
        return minimax_quotas(&body, url.contains("/coding_plan/remains"));
    }
    if failed {
        json!({"message":"MiniMax connected. Unable to fetch usage: fetch failed"})
    } else {
        json!({"message":"MiniMax connected. Unable to fetch usage."})
    }
}

const MINIMAX_WINDOWS: [(&str, [(&str, &str); 5]); 2] = [
    (
        "5h",
        [
            ("current_interval_total_count", "currentIntervalTotalCount"),
            ("current_interval_usage_count", "currentIntervalUsageCount"),
            (
                "current_interval_remaining_percent",
                "currentIntervalRemainingPercent",
            ),
            ("remains_time", "remainsTime"),
            ("end_time", "endTime"),
        ],
    ),
    (
        "7d",
        [
            ("current_weekly_total_count", "currentWeeklyTotalCount"),
            ("current_weekly_usage_count", "currentWeeklyUsageCount"),
            (
                "current_weekly_remaining_percent",
                "currentWeeklyRemainingPercent",
            ),
            ("weekly_remains_time", "weeklyRemainsTime"),
            ("weekly_end_time", "weeklyEndTime"),
        ],
    ),
];

fn minimax_quotas(body: &Value, count_is_remaining: bool) -> Value {
    let mut quotas = Map::new();
    let now = Utc::now();
    for model in nonnull(body, "model_remains", "modelRemains")
        .as_array()
        .into_iter()
        .flatten()
    {
        let raw = nonnull(model, "model_name", "modelName");
        let name = if truthy(raw) {
            minimax_name(&parse::string(raw))
        } else {
            minimax_name("")
        };
        for (window, fields) in MINIMAX_WINDOWS {
            let [total, count, percent, delay, anchor] =
                fields.map(|(snake, camel)| nonnull(model, snake, camel));
            let total = quantity(total).max(0.0);
            let percent = (!percent.is_null())
                .then(|| coerce_quantity(percent))
                .flatten()
                .map(|value| value.clamp(0.0, 100.0));
            if total == 0.0 && percent.is_none() {
                continue;
            }
            let (total, count) = if total > 0.0 {
                (total, quantity(count).max(0.0))
            } else {
                let percent = percent.expect("a percent-only quota");
                let used = if count_is_remaining {
                    percent
                } else {
                    100.0 * (1.0 - percent / 100.0)
                };
                (100.0, used.round())
            };
            let used = if count_is_remaining {
                (total - count).max(0.0)
            } else {
                count.min(total)
            };
            let remaining = (total - used).max(0.0);
            let percent = percent.unwrap_or_else(|| (remaining / total * 100.0).clamp(0.0, 100.0));
            let delay = quantity(delay);
            let reset = if delay > 0.0 {
                iso((delay <= 8.64e15)
                    .then(|| TimeDelta::try_milliseconds(delay as i64))
                    .flatten()
                    .and_then(|offset| now.checked_add_signed(offset)))
            } else {
                iso(parse::reset_time(anchor, true))
            };
            quotas.insert(format!("{name} ({window})"),json!({"used":parse::number(used),"total":parse::number(total),
                "remaining":parse::number(remaining),"remainingPercentage":parse::number(percent),"resetAt":reset,"unlimited":false}));
        }
    }
    if quotas.is_empty() {
        json!({"message":"MiniMax connected. No quota data was returned."})
    } else {
        json!({"quotas":quotas})
    }
}

fn minimax_name(source: &str) -> String {
    match source.trim() {
        "" => "MiniMax".into(),
        "MiniMax-M*" | "general" => "M-series".into(),
        source => {
            let name = source
                .split(|ch: char| ch.is_whitespace() || matches!(ch, '_' | '-'))
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let name = WORD_INITIAL.replace_all(&name, |initial: &regex::Captures<'_>| {
                initial[0].to_ascii_uppercase()
            });
            name.split(' ')
                .map(|word| match word {
                    "To" => "to",
                    "Tts" => "TTS",
                    "Hd" => "HD",
                    word => word,
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

fn quantity(value: &Value) -> f64 {
    coerce_quantity(value).unwrap_or(0.0)
}

fn nonnull<'a>(value: &'a Value, first: &str, second: &str) -> &'a Value {
    value
        .get(first)
        .filter(|value| !value.is_null())
        .unwrap_or(&value[second])
}

fn iso(date: Option<DateTime<Utc>>) -> Option<String> {
    date.map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true))
}
