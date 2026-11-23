use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde_json::{Map, Value, json};

use crate::chat_format::ChatFormat;
use crate::gemini_signatures::SignatureContext;
use crate::gemini_upstream;
use crate::sse_reader::{ReadLine, SseReader};

#[path = "translated_sse.rs"]
mod translated_sse;

#[path = "chat_json.rs"]
mod chat_json;

pub type UsageCallback = Box<dyn FnOnce(Value) + Send + 'static>;

pub enum ResponseWire {
    Chat,
    Messages,
    Responses,
    Gemini(SignatureContext),
}

impl ResponseWire {
    fn matches_client(&self, format: ChatFormat) -> bool {
        matches!(
            (self, format),
            (Self::Chat, ChatFormat::Chat)
                | (Self::Messages, ChatFormat::Messages)
                | (Self::Responses, ChatFormat::Responses)
        )
    }
}

const VARY: &str = "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding";
const DONE: &str = "data: [DONE]\n\n";
const BUFFER_TOKENS: usize = 2000;
const USAGE_FIELDS: &[&str] = &[
    "prompt_tokens",
    "completion_tokens",
    "total_tokens",
    "cached_tokens",
    "reasoning_tokens",
    "prompt_tokens_details",
    "completion_tokens_details",
    "estimated",
];

const MESSAGES_USAGE_FIELDS: &[&str] = &[
    "input_tokens",
    "output_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
    "estimated",
];

const RESPONSES_USAGE_FIELDS: &[&str] = &[
    "input_tokens",
    "output_tokens",
    "input_tokens_details",
    "output_tokens_details",
    "estimated",
];

fn usage_fields(format: ChatFormat) -> &'static [&'static str] {
    match format {
        ChatFormat::Chat => USAGE_FIELDS,
        ChatFormat::Messages => MESSAGES_USAGE_FIELDS,
        ChatFormat::Responses => RESPONSES_USAGE_FIELDS,
    }
}

pub(crate) struct Output<'a> {
    pub request_body: Value,
    pub source_body: Option<Value>,
    pub provider: &'a str,
    pub model: &'a str,
    pub wire: ResponseWire,
    pub format: ChatFormat,
}

pub async fn respond(
    upstream: reqwest::Response,
    output: Output<'_>,
    stream: bool,
    force_json_from_sse: bool,
    on_usage: Option<UsageCallback>,
    trace: Option<&crate::observability::Trace>,
) -> Response {
    let upstream_headers = upstream.headers().clone();
    let content_type = upstream_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let event_stream = content_type.contains("text/event-stream");
    let force_json_from_sse = force_json_from_sse && event_stream;
    if stream && !force_json_from_sse {
        return stream_response(upstream, output, on_usage).await;
    }
    let Output {
        request_body,
        source_body,
        provider,
        model,
        wire,
        format,
    } = output;
    let context = source_body.as_ref().unwrap_or(&request_body);

    let mut value = if event_stream {
        let Ok(raw) = upstream.text().await else {
            return error(
                StatusCode::BAD_GATEWAY,
                "Invalid SSE response for non-streaming request",
            );
        };
        match collect_sse(&raw, model) {
            Ok(Some(value)) => value,
            Ok(None) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Invalid SSE response for non-streaming request",
                );
            }
            Err(entry) => {
                let numeric = number(&entry["status"]);
                let status = if numeric.fract() == 0.0 && (400.0..=599.0).contains(&numeric) {
                    StatusCode::from_u16(numeric as u16).unwrap_or(StatusCode::BAD_GATEWAY)
                } else {
                    StatusCode::BAD_GATEWAY
                };
                let message = entry["message"]
                    .as_str()
                    .filter(|message| !message.is_empty())
                    .unwrap_or("Upstream SSE stream failed");
                return error(status, message);
            }
        }
    } else {
        match upstream.json::<Value>().await {
            Ok(value) if value.is_object() => value,
            _ => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    &format!("Invalid JSON response from {provider}"),
                );
            }
        }
    };
    if matches!(provider, "cline" | "clinepass")
        && value["success"] == true
        && value["data"].is_object()
    {
        value = value["data"].take();
    }
    if let Some(trace) = trace {
        trace.raw_response(&value);
    }
    let usage = if matches!(wire, ResponseWire::Gemini(_)) {
        gemini_upstream::raw_usage(&value)
    } else {
        response_usage(&value)
    };
    if let (Some(callback), Some(usage)) = (on_usage, usage) {
        callback(usage);
    }
    if force_json_from_sse {
        if let Some(choices) = value["choices"].as_array_mut() {
            for choice in choices {
                if truthy(&choice["message"]["content"])
                    && let Some(message) = choice["message"].as_object_mut()
                {
                    message.shift_remove("reasoning_content");
                }
            }
        }
        let output = chat_json::translate(value, context, format);
        return json_response(StatusCode::OK, output, None);
    }
    let mut output = match wire {
        ResponseWire::Messages if format != ChatFormat::Messages => {
            chat_json::translate(crate::anthropic_upstream::to_chat(value), context, format)
        }
        ResponseWire::Messages => value,
        ResponseWire::Gemini(_) => {
            chat_json::translate(gemini_upstream::to_chat(value), context, format)
        }
        _ => chat_json::translate(value, context, format),
    };
    normalize_json(&mut output, format);
    json_response(StatusCode::OK, output, Some(&upstream_headers))
}

pub(crate) async fn stream_response(
    upstream: reqwest::Response,
    output: Output<'_>,
    on_usage: Option<UsageCallback>,
) -> Response {
    let Output {
        request_body,
        source_body,
        provider,
        model,
        wire,
        format,
    } = output;
    let upstream_headers = upstream.headers().clone();
    let content_type = upstream_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let content_type = content_type.to_ascii_lowercase();
    if !content_type.is_empty()
        && !content_type.contains("text/event-stream")
        && !content_type.contains("application/json")
    {
        let status = upstream.status();
        let body = upstream.text().await.unwrap_or_default();
        let reason = non_stream_reason(&body, &content_type);
        return json_response(
            status,
            json!({"error":{"message": format!("[{status}]: {reason}", status = status.as_u16())}}),
            None,
        );
    }
    let body = if wire.matches_client(format) {
        let stream = OpenAiStream::new(upstream, request_body, provider, format, on_usage);
        Body::from_stream(stream::unfold(stream, |mut state| async move {
            let chunk = state.next().await?;
            Some((Ok::<_, std::convert::Infallible>(chunk), state))
        }))
    } else {
        let stream = translated_sse::TranslatedStream::new(
            upstream,
            source_body.unwrap_or(request_body),
            provider,
            model,
            wire,
            format,
            on_usage,
        );
        Body::from_stream(stream::unfold(stream, |mut state| async move {
            let chunk = state.next().await?;
            Some((Ok::<_, std::convert::Infallible>(chunk), state))
        }))
    };
    reply(StatusCode::OK, body, true, Some(&upstream_headers))
}

pub(crate) fn chat_to_messages(value: Value) -> Value {
    chat_json::translate(value, &Value::Null, ChatFormat::Messages)
}

pub(crate) fn reply(
    status: StatusCode,
    body: Body,
    stream: bool,
    upstream: Option<&HeaderMap>,
) -> Response {
    let mut response = Response::builder()
        .status(status)
        .header(header::VARY, VARY)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(
            header::CONTENT_TYPE,
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        );
    if stream {
        response = response
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive");
    }
    if let Some(headers) = upstream {
        for (name, value) in headers {
            if matches!(name.as_str(), "retry-after" | "x-should-retry")
                || name.as_str().starts_with("anthropic-ratelimit-")
            {
                response = response.header(name, value);
            }
        }
    }
    response
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub(crate) fn json_response(
    status: StatusCode,
    mut value: Value,
    upstream: Option<&HeaderMap>,
) -> Response {
    normalize_numbers(&mut value);
    reply(status, Body::from(value.to_string()), false, upstream)
}

fn error_payload(status: StatusCode, message: &str) -> Value {
    let (kind, code) = match status.as_u16() {
        400 => ("invalid_request_error", "bad_request"),
        401 => ("authentication_error", "invalid_api_key"),
        402 => ("billing_error", "payment_required"),
        403 => ("permission_error", "insufficient_quota"),
        404 => ("invalid_request_error", "model_not_found"),
        406 => ("invalid_request_error", "model_not_supported"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        502 => ("server_error", "bad_gateway"),
        503 => ("server_error", "service_unavailable"),
        504 => ("server_error", "gateway_timeout"),
        status if status >= 500 => ("server_error", "internal_server_error"),
        _ => ("invalid_request_error", ""),
    };
    json!({"error": {"message": message, "type": kind, "code": code}})
}

pub(crate) fn error(status: StatusCode, message: &str) -> Response {
    json_response(status, error_payload(status, message), None)
}

fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

fn strip_tags(text: &str) -> String {
    let mut output = String::new();
    let mut tail = text;
    while let Some(start) = tail.find('<') {
        let Some(end) = tail[start..].find('>') else {
            break;
        };
        output.push_str(&tail[..start]);
        tail = &tail[start + end + 1..];
    }
    output.push_str(tail);
    output
}

fn short_text(text: &str) -> String {
    String::from_utf16_lossy(&js_trim(text).encode_utf16().take(160).collect::<Vec<_>>())
}

pub(crate) fn non_stream_reason(body: &str, content_type: &str) -> String {
    let lower = body.to_ascii_lowercase();
    let title = lower.find("<title>").and_then(|start| {
        let text = &lower[start + 7..];
        let end = text.find('<')?;
        (end > 0 && text[end..].starts_with("</title>")).then(|| &body[start + 7..start + 7 + end])
    });
    if let Some(title) = title {
        let mut cleaned = String::new();
        let mut line_break = false;
        for c in strip_tags(title).chars() {
            if matches!(c, '\r' | '\n') {
                if !line_break {
                    cleaned.push(' ');
                }
                line_break = true;
            } else {
                line_break = false;
                cleaned.push(c);
            }
        }
        let cleaned = short_text(&cleaned);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    if js_len(body) < 200 {
        return short_text(&strip_tags(body));
    }
    format!("Upstream returned non-SSE response ({content_type})")
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

pub(crate) fn js_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    js_text(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), |n| n.to_string()),
        value => value.to_string(),
    }
}

fn number(value: &Value) -> f64 {
    match value {
        Value::Null | Value::Bool(false) => 0.0,
        Value::Bool(true) => 1.0,
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(_) | Value::Array(_) => {
            let mut text = js_text(value);
            text = js_trim(&text).to_string();
            if text.is_empty() {
                0.0
            } else if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                u64::from_str_radix(hex, 16)
                    .map(|n| n as f64)
                    .unwrap_or(f64::NAN)
            } else {
                text.parse().unwrap_or(f64::NAN)
            }
        }
        _ => f64::NAN,
    }
}

fn json_number(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

pub(crate) fn js_add(a: &Value, b: &Value) -> Value {
    if a.is_string()
        || b.is_string()
        || a.is_array()
        || b.is_array()
        || a.is_object()
        || b.is_object()
    {
        json!(js_text(a) + &js_text(b))
    } else {
        json_number(number(a) + number(b))
    }
}

fn pad_usage(value: &Value, usage_fields: &[&str]) -> Value {
    let Some(usage) = value.as_object() else {
        return if value.is_array() {
            json!({})
        } else {
            value.clone()
        };
    };
    let mut buffered = usage.clone();
    let buffer = json!(BUFFER_TOKENS);
    for key in ["input_tokens", "prompt_tokens", "total_tokens"] {
        if let Some(current) = buffered.get_mut(key) {
            *current = js_add(current, &buffer);
        }
    }
    if !buffered.contains_key("total_tokens")
        && let (Some(prompt), Some(output)) = (
            buffered.get("prompt_tokens"),
            buffered.get("completion_tokens"),
        )
    {
        buffered.insert("total_tokens".into(), js_add(prompt, output));
    }
    let fields: Map<String, Value> = usage_fields
        .iter()
        .filter_map(|&key| buffered.get(key).map(|value| (key.into(), value.clone())))
        .collect();
    Value::Object(fields)
}

pub(crate) fn response_usage(chunk: &Value) -> Option<Value> {
    let usage = chunk.get("usage")?;
    let (input, output, cached, reasoning) = if let Some(input) = usage.get("input_tokens") {
        (
            input,
            &usage["output_tokens"],
            usage
                .get("cached_tokens")
                .filter(|v| !v.is_null())
                .or_else(|| usage["input_tokens_details"].get("cached_tokens")),
            None,
        )
    } else {
        (
            usage.get("prompt_tokens")?,
            &usage["completion_tokens"],
            usage
                .get("cached_tokens")
                .filter(|v| !v.is_null())
                .or_else(|| usage["prompt_tokens_details"].get("cached_tokens")),
            usage["completion_tokens_details"].get("reasoning_tokens"),
        )
    };
    let mut result = Map::new();
    for (key, value) in [("prompt_tokens", input), ("completion_tokens", output)] {
        result.insert(
            key.into(),
            if truthy(value) {
                value.clone()
            } else {
                json!(0)
            },
        );
    }
    for (key, value) in [("cached_tokens", cached), ("reasoning_tokens", reasoning)] {
        if let Some(value) = value {
            result.insert(key.into(), value.clone());
        }
    }
    if usage.get("input_tokens").is_some() {
        for key in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
            if let Some(value) = usage.get(key) {
                result.insert(key.into(), value.clone());
            }
        }
    }
    Some(Value::Object(result))
}

pub(crate) fn normalize_json(value: &mut Value, format: ChatFormat) {
    let native = match format {
        ChatFormat::Chat => false,
        ChatFormat::Messages => value["type"] == "message",
        ChatFormat::Responses => value["object"] == "response",
    };
    let Some(fields) = value.as_object_mut() else {
        return;
    };
    if let Some(first) = fields
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .and_then(|choices| choices.first_mut())
        && first["message"]["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty())
        && let Some(choice) = first.as_object_mut()
    {
        choice.insert("finish_reason".into(), json!("tool_calls"));
    }
    if !native {
        for (name, default) in [
            ("object", json!("chat.completion")),
            ("created", json!(chrono::Utc::now().timestamp())),
        ] {
            if !fields.get(name).is_some_and(truthy) {
                fields.insert(name.into(), default);
            }
        }
        fields.shift_remove("prompt_filter_results");
        if let Some(choices) = fields.get_mut("choices").and_then(Value::as_array_mut) {
            for choice in choices.iter_mut().filter_map(Value::as_object_mut) {
                choice.shift_remove("content_filter_results");
                if let Some(message) = choice.get_mut("message").and_then(Value::as_object_mut)
                    && message.get("reasoning_content").is_some_and(truthy)
                    && message.get("content").is_some_and(truthy)
                {
                    message.shift_remove("reasoning_content");
                }
            }
        }
    }
    if let Some(usage) = fields.get_mut("usage").filter(|value| truthy(value)) {
        *usage = pad_usage(usage, usage_fields(format));
    }
}

pub(crate) fn reasoning_text(delta: &Value) -> String {
    for key in ["reasoning_content", "reasoning"] {
        if let Some(text) = delta[key].as_str().filter(|text| !text.is_empty()) {
            return text.to_string();
        }
    }
    delta["reasoning_details"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|detail| {
            if let Some(text) = detail.as_str() {
                text.to_string()
            } else if truthy(&detail["text"]) {
                js_text(&detail["text"])
            } else if truthy(&detail["content"]) {
                js_text(&detail["content"])
            } else {
                String::new()
            }
        })
        .collect()
}

#[derive(Default)]
struct ToolDelta {
    id: Value,
    name: String,
    arguments: String,
}

fn collect_sse(body: &str, fallback_model: &str) -> Result<Option<Value>, Value> {
    let mut first = None;
    let mut stream_error = None;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason = json!("stop");
    let mut usage = None;
    let mut tools: BTreeMap<i64, ToolDelta> = BTreeMap::new();
    for line in body.split('\n') {
        let Some(data) = js_trim(line).strip_prefix("data:").map(js_trim) else {
            continue;
        };
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if truthy(&chunk["error"]) {
            stream_error = Some(chunk["error"].clone());
            continue;
        }
        first.get_or_insert_with(|| chunk.clone());
        let choice = &chunk["choices"][0];
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str() {
            content.push_str(text);
        }
        reasoning.push_str(&reasoning_text(delta));
        if truthy(&choice["finish_reason"]) {
            finish_reason = choice["finish_reason"].clone();
        }
        if chunk["usage"].is_object() || chunk["usage"].is_array() {
            usage = Some(chunk["usage"].clone());
        }
        for fragment in delta["tool_calls"].as_array().into_iter().flatten() {
            let entry = tools
                .entry(number(&fragment["index"]) as i64)
                .or_insert_with(|| ToolDelta {
                    id: json!(""),
                    ..ToolDelta::default()
                });
            if truthy(&fragment["id"]) {
                entry.id = fragment["id"].clone();
            }
            for (field, dst) in [
                ("name", &mut entry.name),
                ("arguments", &mut entry.arguments),
            ] {
                if truthy(&fragment["function"][field]) {
                    dst.push_str(&js_text(&fragment["function"][field]));
                }
            }
        }
    }
    if let Some(error) = stream_error {
        return Err(error);
    }
    let Some(first) = first else {
        return Ok(None);
    };
    let id = if truthy(&first["id"]) {
        first["id"].clone()
    } else {
        json!(format!(
            "chatcmpl-{}",
            chrono::Utc::now().timestamp_millis()
        ))
    };
    let created = if truthy(&first["created"]) {
        first["created"].clone()
    } else {
        json!(chrono::Utc::now().timestamp())
    };
    let model = if truthy(&first["model"]) {
        first["model"].clone()
    } else {
        json!(if fallback_model.is_empty() {
            "unknown"
        } else {
            fallback_model
        })
    };
    let mut msg = json!({
        "role":"assistant",
        "content": if content.is_empty() && !tools.is_empty() { Value::Null } else { json!(content) }
    });
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning);
    }
    if !tools.is_empty() {
        msg["tool_calls"] = json!(tools.into_values().map(|call| json!({
            "id": call.id, "type": "function", "function": {"name": call.name, "arguments": call.arguments}
        })).collect::<Vec<_>>());
    }
    let mut result = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{"index":0,"message":msg,"finish_reason":finish_reason}]
    });
    if let Some(usage) = usage {
        result["usage"] = usage;
    }
    Ok(Some(result))
}

fn has_usage(value: Option<&Value>) -> bool {
    value.is_some_and(|usage| {
        [
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "input_tokens",
            "output_tokens",
            "promptTokenCount",
            "candidatesTokenCount",
        ]
        .iter()
        .any(|key| usage[*key].as_f64().is_some_and(|n| n > 0.0))
    })
}

fn claude_stream_usage(chunk: &Value) -> Option<Map<String, Value>> {
    let usage = match chunk["type"].as_str()? {
        "message_start" => chunk["message"].get("usage")?,
        "message_delta" => chunk.get("usage")?,
        _ => return None,
    };
    if !usage.is_object() {
        return None;
    }
    let mut result = Map::new();
    result.insert(
        "prompt_tokens".into(),
        json_number(number(&usage["input_tokens"])),
    );
    result.insert(
        "completion_tokens".into(),
        json_number(number(&usage["output_tokens"])),
    );
    for key in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
        if let Some(value) = usage.get(key).filter(|v| !v.is_null()) {
            let n = number(value);
            if n.is_finite() {
                result.insert(key.into(), json_number(n));
            }
        }
    }
    Some(result)
}

fn extract_stream_usage(chunk: &Value) -> Option<Map<String, Value>> {
    let usage = &chunk["usage"];
    let prompt = usage.get("prompt_tokens")?;
    let completion = if truthy(&usage["completion_tokens"]) {
        usage["completion_tokens"].clone()
    } else {
        json!(0)
    };
    let cached = usage["prompt_tokens_details"]
        .get("cached_tokens")
        .filter(|value| truthy(value))
        .or_else(|| usage.get("prompt_cache_hit_tokens"));
    let mut result = Map::new();
    for (key, value) in [
        ("prompt_tokens", Some(prompt)),
        ("completion_tokens", Some(&completion)),
        ("cached_tokens", cached),
        (
            "reasoning_tokens",
            usage["completion_tokens_details"].get("reasoning_tokens"),
        ),
    ] {
        if let Some(value) = value.filter(|v| !v.is_null()) {
            let n = number(value);
            if n.is_finite() {
                result.insert(key.into(), json_number(n));
            }
        }
    }
    for key in ["prompt_tokens_details", "completion_tokens_details"] {
        let value = &usage[key];
        if value.is_object() || value.is_array() {
            result.insert(key.into(), value.clone());
        }
    }
    (!result.is_empty()).then_some(result)
}

fn merge_usage(usage: &mut Option<Value>, next: Map<String, Value>) {
    let Some(current) = usage.as_mut().and_then(Value::as_object_mut) else {
        *usage = Some(Value::Object(next));
        return;
    };
    for (key, value) in next {
        if let Some(n) = value.as_f64() {
            let previous = current.get(&key).and_then(Value::as_f64).unwrap_or(0.0);
            current.insert(key, json_number(previous.max(n)));
        } else if value.is_object() || value.is_array() {
            current.insert(key, value);
        }
    }
}

fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(n) if let Some(x) = n.as_f64() => *value = json_number(x),
        Value::Array(items) => items.iter_mut().for_each(normalize_numbers),
        Value::Object(items) => items.values_mut().for_each(normalize_numbers),
        _ => {}
    }
}

pub(crate) fn estimated_usage(prompt_tokens: usize, chars: usize) -> Value {
    let prompt_tokens = prompt_tokens + BUFFER_TOKENS;
    let completion_tokens = if chars == 0 { 0 } else { (chars / 4).max(1) };
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
        "estimated": true,
    })
}

fn base36(mut value: u64) -> String {
    let mut digits = Vec::new();
    loop {
        digits.push(char::from(
            b"0123456789abcdefghijklmnopqrstuvwxyz"[(value % 36) as usize],
        ));
        value /= 36;
        if value == 0 {
            return digits.iter().rev().collect();
        }
    }
}

fn patch_chunk(
    value: &mut Value,
    tool_ids: &mut HashMap<String, String>,
    lift_aliases: bool,
    repeat_known_tool_ids: bool,
) -> bool {
    let mut changed = false;
    let Some(chunk) = value.as_object_mut() else {
        return false;
    };
    if let Some(id) = chunk.get("id").and_then(Value::as_str)
        && !id.is_empty()
        && (id == "chat" || id == "completion" || js_len(id) < 8)
    {
        let fallback = ["requestId", "traceId"]
            .into_iter()
            .filter_map(|key| {
                chunk
                    .get("extend_fields")?
                    .get(key)
                    .filter(|value| truthy(value))
            })
            .next()
            .map_or_else(
                || base36(chrono::Utc::now().timestamp_millis() as u64),
                js_text,
            );
        chunk.insert("id".into(), json!(format!("chatcmpl-{fallback}")));
        changed = true;
    }
    if let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices.iter_mut().filter_map(Value::as_object_mut) {
            changed |= choice.shift_remove("content_filter_results").is_some();
            let Some(delta) = choice.get_mut("delta") else {
                continue;
            };
            if lift_aliases {
                let lifted = reasoning_text(delta);
                if !lifted.is_empty()
                    && delta["reasoning_content"]
                        .as_str()
                        .is_none_or(str::is_empty)
                {
                    delta["reasoning_content"] = json!(lifted);
                    changed = true;
                }
            }
            if delta["tool_calls"].as_array().is_some_and(Vec::is_empty)
                && let Some(fields) = delta.as_object_mut()
            {
                fields.shift_remove("tool_calls");
                changed = true;
            }
            if let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for call in calls.iter_mut().filter_map(Value::as_object_mut) {
                    let index = call.get("index");
                    let key = index.map(|value| {
                        if let Some(n) = value.as_f64() {
                            json_number(n).to_string()
                        } else {
                            value.to_string()
                        }
                    });
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty());
                    if let Some(id) = id {
                        if let Some(key) = key {
                            tool_ids.insert(key, id.into());
                        }
                    } else {
                        let known = key.as_ref().and_then(|key| tool_ids.get(key));
                        if known.is_some() && !repeat_known_tool_ids {
                            continue;
                        }
                        let id = known.cloned().unwrap_or_else(|| {
                            let now = chrono::Utc::now().timestamp_millis();
                            index.map_or_else(
                                || format!("call_{now}"),
                                |i| format!("call_{}_{now}", js_text(i)),
                            )
                        });
                        call.insert("id".into(), json!(id));
                        if let Some(key) = key {
                            tool_ids.insert(key, id);
                        }
                        changed = true;
                    }
                }
            }
        }
    }
    if chunk.contains_key("choices") {
        for (key, default) in [
            ("object", json!("chat.completion.chunk")),
            ("created", json!(chrono::Utc::now().timestamp())),
        ] {
            if !chunk.get(key).is_some_and(truthy) {
                chunk.insert(key.into(), default);
                changed = true;
            }
        }
    }
    changed | chunk.shift_remove("prompt_filter_results").is_some()
}

fn valuable_chunk(chunk: &Value) -> bool {
    let first = &chunk["choices"][0];
    let delta = &first["delta"];
    !truthy(delta)
        || ["content", "reasoning_content", "role"]
            .iter()
            .any(|key| truthy(&delta[*key]))
        || delta["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty())
        || truthy(&first["finish_reason"])
}

struct OpenAiStream {
    input: SseReader,
    wire: ChatFormat,
    done_seen: bool,
    eof: bool,
    no_synthesized_done: bool,
    preserve_finish_usage: bool,
    lift_aliases: bool,
    repeat_known_tool_ids: bool,
    stall: Duration,
    tool_ids: HashMap<String, String>,
    prompt_tokens: usize,
    content_chars: usize,
    usage: Option<Value>,
    on_usage: Option<UsageCallback>,
}

impl OpenAiStream {
    fn new(
        upstream: reqwest::Response,
        mut body: Value,
        provider: &str,
        wire: ChatFormat,
        on_usage: Option<UsageCallback>,
    ) -> Self {
        normalize_numbers(&mut body);
        Self {
            input: SseReader::new(upstream),
            wire,
            done_seen: false,
            eof: false,
            no_synthesized_done: matches!(
                provider,
                "antigravity" | "gemini" | "vertex" | "ollama-local" | "commandcode"
            ),
            preserve_finish_usage: matches!(provider, "ollama-local" | "commandcode"),
            lift_aliases: matches!(provider, "opencode" | "sensenova" | "sensenova-intl"),
            repeat_known_tool_ids: provider != "commandcode",
            stall: stall_timeout(provider),
            tool_ids: HashMap::new(),
            prompt_tokens: if body.is_object() || body.is_array() {
                js_len(&body.to_string()).div_ceil(4)
            } else {
                0
            },
            content_chars: 0,
            usage: None,
            on_usage,
        }
    }

    fn line(&mut self, line: &str) -> Option<Bytes> {
        if let Some(data) = js_trim(line).strip_prefix("data:").map(js_trim) {
            if data == "[DONE]" {
                self.done_seen = true;
            } else {
                let mut chunk = serde_json::from_str::<Value>(data).ok()?;
                if chunk.is_null() {
                    return None;
                }
                if self.wire == ChatFormat::Messages {
                    if let Some(usage) = claude_stream_usage(&chunk) {
                        merge_usage(&mut self.usage, usage);
                    }
                    return Some(Bytes::from(format!("{}\n", normalized_frame(line))));
                }
                let mut changed = patch_chunk(
                    &mut chunk,
                    &mut self.tool_ids,
                    self.lift_aliases,
                    self.repeat_known_tool_ids,
                );
                if !valuable_chunk(&chunk) {
                    return None;
                }
                let delta = &chunk["choices"][0]["delta"];
                for key in ["content", "reasoning_content"] {
                    if let Some(text) = delta[key].as_str() {
                        self.content_chars += js_len(text);
                    }
                }
                if let Some(usage) = extract_stream_usage(&chunk) {
                    merge_usage(&mut self.usage, usage);
                }
                if !self.preserve_finish_usage && truthy(&chunk["choices"][0]["finish_reason"]) {
                    if !has_usage(chunk.get("usage")) {
                        let usage = estimated_usage(self.prompt_tokens, self.content_chars);
                        self.usage = Some(usage.clone());
                        chunk["usage"] = usage;
                        changed = true;
                    } else if let Some(usage) = &self.usage {
                        chunk["usage"] = pad_usage(usage, USAGE_FIELDS);
                        changed = true;
                    }
                }
                if changed {
                    normalize_numbers(&mut chunk);
                    return Some(Bytes::from(format!("data: {chunk}\n")));
                }
            }
        }
        Some(Bytes::from(format!("{}\n", normalized_frame(line))))
    }

    fn completed(&mut self) {
        if !has_usage(self.usage.as_ref()) && self.content_chars > 0 {
            self.usage = Some(estimated_usage(self.prompt_tokens, self.content_chars));
        }
        if let (Some(callback), Some(usage)) = (self.on_usage.take(), self.usage.take()) {
            callback(usage);
        }
    }

    async fn next(&mut self) -> Option<Bytes> {
        loop {
            if !self.eof {
                match self.input.next(self.stall, None).await {
                    ReadLine::Line(line, true) => {
                        if let Some(chunk) = self.line(&line) {
                            return Some(chunk);
                        }
                        continue;
                    }
                    ReadLine::Line(line, false) => {
                        self.eof = true;
                        let tail = js_trim(&line).strip_prefix("data:").map(js_trim);
                        if tail != Some("[DONE]") && !line.is_empty() {
                            return Some(Bytes::from(normalized_frame(&line)));
                        }
                    }
                    ReadLine::End => self.eof = true,
                    err => {
                        let reason = match err {
                            ReadLine::Timeout => "stream stall timeout",
                            _ => "upstream connection lost",
                        };
                        self.eof = true;
                        self.done_seen = true;
                        self.on_usage = None;
                        let frame = error_payload(StatusCode::GATEWAY_TIMEOUT, reason);
                        return Some(Bytes::from(format!("data: {frame}\n\n{DONE}")));
                    }
                }
            }
            self.completed();
            if !self.done_seen && !self.no_synthesized_done {
                self.done_seen = true;
                return Some(Bytes::from_static(DONE.as_bytes()));
            }
            return None;
        }
    }
}

fn normalized_frame(line: &str) -> String {
    if line.starts_with("data:") && !line.starts_with("data: ") {
        format!("data: {}", &line[5..])
    } else {
        line.to_owned()
    }
}

pub(crate) fn stall_timeout(provider: &str) -> Duration {
    if matches!(provider, "qoder" | "qoder-cn") {
        return Duration::from_millis(120_000);
    }
    let env = std::env::var("STREAM_STALL_TIMEOUT_MS").ok();
    let timeout = env
        .as_deref()
        .and_then(|text| {
            let text = js_trim(text);
            let text = text.strip_prefix('+').unwrap_or(text);
            let digits = text.bytes().take_while(u8::is_ascii_digit).count();
            text[..digits].parse::<u64>().ok().filter(|ms| *ms > 0)
        })
        .unwrap_or(360_000);
    Duration::from_millis(timeout)
}
