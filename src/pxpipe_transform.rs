use serde_json::{Number, Value, json};
use std::time::Duration;

const DEFAULT_MIN_CHARS: f64 = 25_000.0;
const DEFAULT_TIMEOUT_MS: f64 = 15_000.0;

#[derive(Clone, Copy)]
pub struct Prepared {
    pub original_chars: usize,
    pub min_chars: f64,
    pub timeout: Duration,
}

pub struct Compression {
    pub body: Option<Value>,
    pub summary: Value,
}

pub enum Transform {
    Skipped {
        reason: Option<String>,
        detail: Option<String>,
    },
    Applied {
        body: Vec<u8>,
        info: ImageInfo,
        owns_cache_control: bool,
    },
}

#[derive(Default)]
pub struct ImageInfo {
    pub compressed_chars: usize,
    pub count: usize,
    pub bytes: usize,
    pub tokens: usize,
    pub pixels: usize,
    pub baseline_tokens: usize,
}

pub fn prepare(
    body: &Value,
    enabled: bool,
    installed: bool,
    format: Option<&str>,
    min_chars: f64,
    timeout_ms: f64,
) -> Result<Prepared, Value> {
    let reason = if !enabled {
        "disabled"
    } else if !installed {
        "not_installed"
    } else if !crate::provider_connections::truthy(body) {
        "missing_body"
    } else if format != Some("claude") {
        return Err(skipped_summary(
            "unsupported_format",
            format.map(str::to_owned),
            json!({}),
        ));
    } else {
        let original_chars = body_chars(body);
        let min_chars = positive_or(min_chars, DEFAULT_MIN_CHARS);
        if (original_chars as f64) < min_chars {
            return Err(skipped_summary(
                "below_threshold",
                None,
                json!({
                    "originalChars": original_chars, "threshold": number_json(min_chars),
                }),
            ));
        }
        let timeout_ms = positive_or(timeout_ms, DEFAULT_TIMEOUT_MS);
        let timeout = if timeout_ms <= i32::MAX as f64 {
            timeout_ms.floor().max(1.0) as u64
        } else {
            1
        };
        return Ok(Prepared {
            original_chars,
            min_chars,
            timeout: Duration::from_millis(timeout),
        });
    };
    Err(skipped_summary(reason, None, json!({})))
}

impl Prepared {
    pub fn complete(
        &self,
        result: Result<Option<Transform>, String>,
        duration_ms: u64,
    ) -> Compression {
        let common = || json!({"originalChars": self.original_chars, "durationMs": duration_ms});
        match result {
            Err(detail) => skipped("transform_error", Some(detail), common()),
            Ok(None) => skipped("timeout", None, common()),
            Ok(Some(Transform::Skipped { reason, detail })) => skipped(
                reason
                    .as_deref()
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or("passthrough"),
                detail,
                common(),
            ),
            Ok(Some(Transform::Applied {
                body,
                info,
                owns_cache_control,
            })) => {
                let raw = body.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&body);
                let Ok(body) = serde_json::from_str::<Value>(&String::from_utf8_lossy(raw)) else {
                    return skipped(
                        "transform_error",
                        Some("Invalid PXPIPE response body".into()),
                        common(),
                    );
                };
                let image_tokens = if info.tokens > 0 {
                    info.tokens
                } else if info.pixels > 0 {
                    round_ratio(info.pixels, 750)
                } else {
                    info.count.saturating_mul(4761)
                };
                let chars_remaining = self.original_chars.saturating_sub(info.compressed_chars);
                let after = round_ratio(chars_remaining, 4).saturating_add(image_tokens);
                let before = if info.baseline_tokens > 0 {
                    info.baseline_tokens
                } else {
                    round_ratio(self.original_chars, 4)
                };
                let saved = before.saturating_sub(after);
                let pct = if before > 0 {
                    saved as f64 / before as f64 * 100.0
                } else {
                    0.0
                };
                Compression {
                    summary: json!({
                        "applied": true, "reason": "applied", "originalChars": self.original_chars,
                        "compressedBodyChars": body_chars(&body), "imagedChars": info.compressed_chars,
                        "imageCount": info.count, "imageBytes": info.bytes, "tokensBeforeEst": before,
                        "tokensAfterEst": after, "durationMs": duration_ms,
                        "cacheOwnsControl": owns_cache_control, "tokensSavedEst": saved,
                        "savedPct": number_json(round_hundredths(pct)),
                    }),
                    body: Some(body),
                }
            }
        }
    }
}

fn positive_or(value: f64, fallback: f64) -> f64 {
    if value > 0.0 { value } else { fallback }
}

fn skipped(reason: &str, detail: Option<String>, fields: Value) -> Compression {
    Compression {
        body: None,
        summary: skipped_summary(reason, detail, fields),
    }
}

fn skipped_summary(reason: &str, detail: Option<String>, fields: Value) -> Value {
    let mut summary = json!({"applied": false, "reason": reason});
    if let Some(detail) = detail {
        summary["detail"] = json!(detail);
    }
    if let Some(fields) = fields.as_object() {
        summary
            .as_object_mut()
            .expect("summary object")
            .extend(fields.clone());
    }
    summary
}

fn number_json(number: f64) -> Value {
    if number.fract() == 0.0 && number.abs() < 9_007_199_254_740_992.0 {
        json!(number as i64)
    } else {
        json!(number)
    }
}

fn round_ratio(value: usize, denominator: usize) -> usize {
    value / denominator + usize::from(value % denominator >= denominator.div_ceil(2))
}

fn round_hundredths(value: f64) -> f64 {
    let scaled = value * 100.0;
    if scaled.fract() == 0.5 && value.mul_add(100.0, -scaled) < 0.0 {
        scaled.floor() / 100.0
    } else {
        scaled.round() / 100.0
    }
}

fn body_chars(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => number_chars(number),
        Value::String(value) => string_chars(value),
        Value::Array(items) => {
            2 + items.len().saturating_sub(1) + items.iter().map(body_chars).sum::<usize>()
        }
        Value::Object(fields) => {
            2 + fields.len().saturating_sub(1)
                + fields
                    .iter()
                    .map(|(name, value)| string_chars(name) + 1 + body_chars(value))
                    .sum::<usize>()
        }
    }
}

fn string_chars(value: &str) -> usize {
    2 + value
        .chars()
        .map(|ch| match ch {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            '\u{0}'..='\u{1f}' => 6,
            ch => ch.len_utf16(),
        })
        .sum::<usize>()
}

fn number_chars(number: &Number) -> usize {
    const MAX_SAFE_INT: u64 = 9_007_199_254_740_991;
    if let Some(integer) = number.as_i64()
        && integer.unsigned_abs() <= MAX_SAFE_INT
    {
        return integer.to_string().len();
    }
    let number = number.as_f64().unwrap_or(f64::INFINITY);
    if !number.is_finite() {
        4
    } else if number == 0.0 {
        1
    } else if number.abs() >= 1e21 {
        format!("{number:e}").len() + 1
    } else if number.abs() < 1e-6 {
        format!("{number:e}").len()
    } else {
        number.to_string().len()
    }
}
