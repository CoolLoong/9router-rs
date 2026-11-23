use std::convert::Infallible;
use std::sync::LazyLock;

use axum::body::Bytes;
use chrono::Utc;
use futures_util::stream;
use regex::Regex;
use reqwest::{Body, Response, ResponseBuilderExt, StatusCode, Url, header};
use serde_json::{Value, json};

use crate::chat_input::truthy;
use crate::provider_index::js_trim;
use crate::sse_reader::{ReadLine, SseReader};
use crate::thinking_intent::numeric;

use super::{Failure, TIMEOUT, payload::number_value, redact};

const MAX_LINE: usize = 4 * 1024 * 1024;
const DONE: &str = "data: [DONE]\n\n";
static LEGACY_BILLING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""code"\s*:\s*"112""#).expect("Qoder quota pattern"));

pub(super) struct Rejection {
    pub(super) status: f64,
    pub(super) body: String,
}

pub(super) struct Stream {
    upstream: Option<SseReader>,
    first: Option<String>,
    pending_finish: Value,
    pending_usage: Option<Value>,
    finish_forwarded: bool,
    id: Value,
    created: Value,
    model: String,
    error_model: String,
    secrets: Vec<String>,
}

impl Stream {
    pub(super) async fn open(
        upstream: Response,
        model: String,
        secrets: Vec<String>,
    ) -> Result<(Self, Option<Rejection>), Failure> {
        let mut reader = SseReader::new(upstream).with_line_limit(MAX_LINE);
        let first = loop {
            match reader.next(TIMEOUT, None).await {
                ReadLine::Line(line, _) => {
                    if js_trim(&line).starts_with("data:") {
                        break Some(line);
                    }
                }
                ReadLine::End => break None,
                ReadLine::Timeout => {
                    return Err(Failure::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "Qoder upstream timed out",
                    ));
                }
                ReadLine::Disconnected => {
                    return Err(Failure::upstream("Qoder response disconnected"));
                }
            }
        };
        let rejection = first
            .as_deref()
            .and_then(data)
            .and_then(envelope)
            .filter(|(status, _)| *status != 200.0)
            .map(|(status, inner)| Rejection {
                status,
                body: error_message(status, inner),
            });
        Ok((
            Self {
                upstream: Some(reader),
                first,
                pending_finish: Value::Null,
                pending_usage: None,
                finish_forwarded: false,
                id: Value::Null,
                created: Value::Null,
                model: model.clone(),
                error_model: model,
                secrets,
            },
            rejection,
        ))
    }

    pub(super) fn into_response(self, url: Url) -> Response {
        let stream = stream::unfold(self, |mut state| async move {
            let bytes = state.next().await?;
            Some((Ok::<_, Infallible>(bytes), state))
        });
        axum::http::Response::builder()
            .status(StatusCode::OK)
            .url(url)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::wrap_stream(stream))
            .expect("Qoder event stream")
            .into()
    }

    async fn next(&mut self) -> Option<Bytes> {
        let mut output = String::new();
        let mut upstream = self.upstream.take()?;
        loop {
            let line = if let Some(first) = self.first.take() {
                ReadLine::Line(first, true)
            } else {
                upstream.next(TIMEOUT, None).await
            };
            match line {
                ReadLine::Line(line, _) => {
                    if let Some(data) = data(&line) {
                        let terminal = self.push(data, &mut output);
                        if !output.is_empty() {
                            if !terminal {
                                self.upstream = Some(upstream);
                            }
                            break;
                        }
                    }
                }
                _ => {
                    self.flush(&mut output);
                    break;
                }
            }
        }
        Some(Bytes::from(output))
    }

    fn push(&mut self, raw: &str, output: &mut String) -> bool {
        if raw == "[DONE]" {
            self.flush(output);
            return true;
        }
        let Some((status, body)) = envelope(raw) else {
            return false;
        };
        if status != 0.0 && status != 200.0 {
            let message = redact(&error_message(status, body.clone()), &self.secrets);
            if billing(&body) {
                emit_json(
                    output,
                    &json!({"error":{
                        "message":message,"code":"qoder_billing_block","status":403,"type":"quota_error",
                    }}),
                );
            } else {
                let msg: String = message.chars().take(200).collect();
                let suffix = if message.chars().count() > 200 {
                    "..."
                } else {
                    ""
                };
                let now = Utc::now();
                emit_json(
                    output,
                    &json!({
                        "id":format!("qoder-error-{}",now.timestamp_millis()),
                        "object":"chat.completion.chunk","created":now.timestamp(),"model":self.error_model,
                        "choices":[{"index":0,"delta":{"content":format!("\n[qoder error {status}: {msg}{suffix}]")},"finish_reason":"stop"}],
                    }),
                );
            }
            output.push_str(DONE);
            return true;
        }
        if body.is_empty() {
            return false;
        }
        if body == "[DONE]" {
            self.flush(output);
            return true;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(&body) else {
            emit(output, &body);
            return false;
        };
        if !parsed.is_object() {
            return false;
        }
        if parsed["id"].as_str().is_some_and(|id| !id.is_empty()) {
            self.id = parsed["id"].clone();
        }
        if parsed["created"].is_number() {
            self.created = parsed["created"].clone();
        }
        if let Some(model) = parsed["model"].as_str().filter(|model| !model.is_empty()) {
            self.model = model.to_owned();
        }
        if let Some(usage) = canonical_usage(&parsed["usage"]) {
            self.pending_usage = Some(usage);
        }
        let choice = &parsed["choices"][0];
        let delta = &choice["delta"];
        let finish = [
            &choice["finish_reason"],
            &delta["finish_reason"],
            &parsed["finish_reason"],
        ]
        .into_iter()
        .find(|value| truthy(value));
        let valuable = delta.is_object()
            && (["content", "reasoning_content"]
                .into_iter()
                .any(|field| delta[field].as_str().is_some_and(|text| !text.is_empty()))
                || delta["tool_calls"]
                    .as_array()
                    .is_some_and(|tools| !tools.is_empty())
                || truthy(&delta["role"]));
        if valuable {
            emit(output, &body);
            if let Some(finish) = finish {
                self.finish_forwarded = true;
                self.pending_finish = if self.pending_usage.is_some() {
                    finish.clone()
                } else {
                    Value::Null
                };
            }
            if truthy(&self.pending_finish) && self.pending_usage.is_some() {
                self.terminal(output);
                output.push_str(DONE);
                return true;
            }
        } else {
            if let Some(finish) = finish {
                self.pending_finish = finish.clone();
            }
            if (truthy(&self.pending_finish) || self.finish_forwarded)
                && self.pending_usage.is_some()
            {
                self.terminal(output);
                output.push_str(DONE);
                return true;
            }
        }
        false
    }

    fn flush(&mut self, output: &mut String) {
        if self.pending_usage.is_some() || (truthy(&self.pending_finish) && !self.finish_forwarded)
        {
            self.terminal(output);
        }
        output.push_str(DONE);
    }

    fn terminal(&mut self, output: &mut String) {
        let now = Utc::now();
        let mut value = json!({
            "id": if truthy(&self.id) {self.id.clone()} else {json!(format!("qoder-{}",now.timestamp_millis()))},
            "object":"chat.completion.chunk",
            "created":if truthy(&self.created) {self.created.clone()} else {json!(now.timestamp())},
            "model":self.model,
            "choices":[{"index":0,"delta":{},"finish_reason":if truthy(&self.pending_finish) {self.pending_finish.take()} else {json!("stop")}}],
        });
        if let Some(usage) = self.pending_usage.take() {
            value["usage"] = usage;
        }
        emit_json(output, &value);
    }
}

fn data(line: &str) -> Option<&str> {
    js_trim(line).strip_prefix("data:").map(str::trim_start)
}

fn envelope(data: &str) -> Option<(f64, String)> {
    let envelope = serde_json::from_str::<Value>(data).ok()?;
    let status = envelope
        .get("statusCodeValue")
        .and_then(numeric)
        .unwrap_or(200.0);
    let body = match &envelope["body"] {
        Value::String(body) => body.clone(),
        Value::Null => String::new(),
        value => value.to_string(),
    };
    Some((status, body))
}

fn error_message(status: f64, body: String) -> String {
    if body.is_empty() {
        format!("upstream status {status}")
    } else {
        body
    }
}

fn billing(body: &str) -> bool {
    body.to_ascii_lowercase().contains("pricingurl")
        || serde_json::from_str::<Value>(body)
            .ok()
            .is_some_and(|value| {
                matches!(value["code"].as_str(), Some("110" | "112"))
                    || matches!(value["code"].as_i64(), Some(110 | 112))
            })
        || LEGACY_BILLING.is_match(body)
}

pub(super) fn failure_status(status: f64, body: &str) -> StatusCode {
    if billing(body) {
        StatusCode::FORBIDDEN
    } else if status.fract() == 0.0 && (400.0..=599.0).contains(&status) {
        StatusCode::from_u16(status as u16).unwrap_or(StatusCode::BAD_GATEWAY)
    } else {
        StatusCode::BAD_GATEWAY
    }
}

fn emit(output: &mut String, value: &str) {
    output.push_str("data: ");
    output.extend(value.chars().filter(|ch| !matches!(ch, '\r' | '\n')));
    output.push_str("\n\n");
}

fn emit_json(output: &mut String, value: &Value) {
    emit(output, &value.to_string());
}

fn canonical_usage(usage: &Value) -> Option<Value> {
    usage.as_object()?;
    let find = |fields: &[&str]| {
        fields
            .iter()
            .find_map(|key| usage.get(*key).filter(|value| !value.is_null()))
            .and_then(numeric)
    };
    let prompt = find(&["prompt_tokens", "input_tokens"]);
    let completion = find(&["completion_tokens", "output_tokens"]);
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    let prompt = prompt.unwrap_or(0.0);
    let completion = completion.unwrap_or(0.0);
    let mut details = usage["prompt_tokens_details"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let cached = details
        .get("cached_tokens")
        .filter(|value| !value.is_null())
        .and_then(numeric)
        .or_else(|| {
            find(&[
                "cached_tokens",
                "prompt_cache_hit_tokens",
                "cache_read_input_tokens",
            ])
        });
    let creation = details
        .get("cache_creation_tokens")
        .filter(|value| !value.is_null())
        .and_then(numeric)
        .or_else(|| find(&["cache_creation_input_tokens"]));
    let mut out = json!({
        "prompt_tokens":number_value(prompt),
        "completion_tokens":number_value(completion),
        "total_tokens":number_value(usage.get("total_tokens").and_then(numeric).unwrap_or(prompt + completion)),
    });
    if let Some(cache) = cached {
        out["cached_tokens"] = number_value(cache);
        details.insert("cached_tokens".into(), number_value(cache));
    }
    if let Some(cache) = creation {
        details.insert("cache_creation_tokens".into(), number_value(cache));
    }
    if !details.is_empty() {
        out["prompt_tokens_details"] = Value::Object(details);
    }
    if usage["completion_tokens_details"].is_object() {
        out["completion_tokens_details"] = usage["completion_tokens_details"].clone();
    }
    if let Some(reason) = find(&["reasoning_tokens"]).or_else(|| {
        usage["completion_tokens_details"]
            .get("reasoning_tokens")
            .and_then(numeric)
    }) {
        out["reasoning_tokens"] = number_value(reason);
    }
    Some(out)
}
