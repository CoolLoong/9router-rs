use std::fs;
use std::path::Path;

use chrono::{Duration, Local, TimeZone, Utc};
use serde_json::{Value, json};

use crate::chat_output::{js_add, js_text};
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

const DAY: i64 = 86_400_000;

pub(super) fn limit(query: Option<&str>) -> f64 {
    let raw = query.and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(key, value)| (key == "limit").then_some(value))
    });
    let value = raw.map_or(0.0, |raw| number(&json!(raw)));
    if value == 0.0 || value.is_nan() {
        100.0
    } else {
        value.min(500.0)
    }
}

fn number(value: &Value) -> f64 {
    match value {
        Value::Null | Value::Bool(false) => 0.0,
        Value::Bool(true) => 1.0,
        Value::Number(value) => value.as_f64().unwrap_or(f64::NAN),
        _ => {
            let text = js_text(value);
            let text = js_trim(&text);
            if text.is_empty() {
                return 0.0;
            }
            for (prefix, base) in [("0x", 16), ("0b", 2), ("0o", 8)] {
                if text.to_ascii_lowercase().starts_with(prefix) {
                    return u64::from_str_radix(&text[2..], base)
                        .map_or(f64::NAN, |value| value as f64);
                }
            }
            text.parse().unwrap_or(f64::NAN)
        }
    }
}

fn numeric(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn stamp(event: &Value) -> f64 {
    event.get("ts").map_or(f64::NAN, number)
}

fn timestamp(event: &Value) -> Option<i64> {
    let raw = event.get("ts")?;
    if let Some(time) = raw.as_str() {
        return chrono::DateTime::parse_from_rfc3339(time)
            .ok()
            .map(|time| time.timestamp_millis());
    }
    let date = number(raw);
    (date.is_finite() && date.abs() <= 8_640_000_000_000_000.0).then_some(date as i64)
}

fn read(directory: &Path) -> Vec<Value> {
    let mut rows: Vec<Value> = ["events.jsonl.1", "events.jsonl"]
        .into_iter()
        .filter_map(|file| fs::read(directory.join(file)).ok())
        .flat_map(|content| {
            String::from_utf8_lossy(&content)
                .split('\n')
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect::<Vec<_>>()
        })
        .collect();
    rows.sort_by(|left, right| {
        let key = |event| {
            let value = stamp(event);
            if value.is_nan() { f64::INFINITY } else { value }
        };
        key(left).total_cmp(&key(right))
    });
    rows
}

fn recent(rows: Vec<Value>, limit: f64) -> Vec<Value> {
    let count = if limit > 0.0 {
        let count = limit.trunc() as usize;
        if count == 0 { rows.len() } else { count }
    } else {
        rows.len().saturating_sub((-limit).trunc() as usize)
    };
    rows.into_iter().rev().take(count).collect()
}

pub(super) fn logs(directory: &Path, limit: f64) -> Value {
    let install_log =
        fs::read(directory.join("install.log"))
            .ok()
            .map_or_else(String::new, |raw| {
                let content = String::from_utf8_lossy(&raw);
                let mut final_lines: Vec<_> = content
                    .rsplit('\n')
                    .enumerate()
                    .map(|(index, line)| {
                        if index == 0 {
                            line
                        } else {
                            line.strip_suffix('\r').unwrap_or(line)
                        }
                    })
                    .filter(|line| !line.is_empty())
                    .take(200)
                    .collect();
                final_lines.reverse();
                final_lines.join("\n")
            });
    json!({"installLog":install_log,"events":recent(read(directory), limit)})
}

fn totals() -> Value {
    json!({
        "requests":0,"compressed":0,"bypassed":0,"errors":0,
        "tokensBeforeEst":0,"tokensAfterEst":0,"tokensSavedEst":0,"savedPct":0,
        "imagesGenerated":0,"compressionTimeMs":0,"avgCompressionMs":0,
    })
}

fn add(output: &mut Value, field: &str, value: &Value) {
    if truthy(value) {
        output[field] = js_add(&output[field], value);
    }
}

fn increment(output: &mut Value, field: &str) {
    output[field] = json!(output[field].as_u64().unwrap_or_default() + 1);
}

fn accumulate(output: &mut Value, event: &Value) {
    increment(output, "requests");
    if truthy(&event["applied"]) {
        increment(output, "compressed");
        for (destination, source) in [
            ("tokensBeforeEst", "tokensBeforeEst"),
            ("tokensAfterEst", "tokensAfterEst"),
            ("tokensSavedEst", "tokensSavedEst"),
            ("imagesGenerated", "imageCount"),
            ("compressionTimeMs", "durationMs"),
        ] {
            add(output, destination, &event[source]);
        }
    } else if matches!(
        event["reason"].as_str(),
        Some("transform_error" | "timeout")
    ) {
        increment(output, "errors");
    } else {
        increment(output, "bypassed");
    }
}

fn finalize(output: &mut Value) {
    let before = number(&output["tokensBeforeEst"]);
    if before > 0.0 {
        let percent = number(&output["tokensSavedEst"]) / before * 100.0;
        output["savedPct"] = numeric(format!("{percent:.2}").parse().unwrap_or(f64::NAN));
    }
    let compressed = number(&output["compressed"]);
    if compressed > 0.0 {
        output["avgCompressionMs"] =
            numeric((number(&output["compressionTimeMs"]) / compressed + 0.5).floor());
    }
}

fn utc_label(time: i64) -> Option<String> {
    chrono::DateTime::<Utc>::from_timestamp_millis(time)
        .map(|day| day.format("%Y-%m-%d").to_string())
}

pub(super) fn stats(directory: &Path, limit: f64) -> Result<Value, &'static str> {
    let events = read(directory);
    let now = Local::now();
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or("Invalid time value")?;
    let today = (0..180)
        .find_map(|offset| {
            Local
                .from_local_datetime(&(midnight + Duration::minutes(offset)))
                .earliest()
        })
        .ok_or("Invalid time value")?
        .timestamp_millis();
    let now = now.timestamp_millis();
    let mut windows = json!({
        "all":totals(),"today":totals(),"yesterday":totals(),"last7d":totals(),"last30d":totals(),
    });

    let mut timeline = serde_json::Map::new();
    for i in (0..30).rev() {
        let date = utc_label(today - i * DAY).ok_or("Invalid time value")?;
        timeline.insert(
            date.clone(),
            json!({"date":date,"tokensSavedEst":0,"compressed":0,"requests":0}),
        );
    }

    for event in &events {
        let timestamp = timestamp(event).ok_or("Invalid time value")?;
        let stamp = stamp(event);
        accumulate(&mut windows["all"], event);
        if stamp >= today as f64 {
            accumulate(&mut windows["today"], event);
        } else if stamp >= (today - DAY) as f64 {
            accumulate(&mut windows["yesterday"], event);
        }
        if stamp >= (now - 7 * DAY) as f64 {
            accumulate(&mut windows["last7d"], event);
        }
        if stamp >= (now - 30 * DAY) as f64 {
            accumulate(&mut windows["last30d"], event);
        }
        if let Some(bucket) = timeline.get_mut(&utc_label(timestamp).ok_or("Invalid time value")?) {
            increment(bucket, "requests");
            if truthy(&event["applied"]) {
                increment(bucket, "compressed");
                add(bucket, "tokensSavedEst", &event["tokensSavedEst"]);
            }
        }
    }

    for counters in windows
        .as_object_mut()
        .expect("PXPIPE stats windows")
        .values_mut()
    {
        finalize(counters);
    }

    Ok(
        json!({"windows":windows,"timeline":timeline.into_values().collect::<Vec<_>>(),"recent":recent(events, limit)}),
    )
}
