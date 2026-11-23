use axum::body::{Body, Bytes, to_bytes};
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures_util::StreamExt;
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};

use crate::chat_input::{js_space, truthy};
use crate::chat_output::js_text;

pub const PATHS: &[&str] = &["/v1beta/models/", "/api/v1beta/models/"];
const BODY_LIMIT: usize = 128 * 1024 * 1024;

pub struct Client {
    model: String,
    stream: bool,
}

impl Client {
    pub fn parse(path: &str, body: &Value) -> Option<Self> {
        let (model, stream) = model_action(path)?;
        if is_native_tts(&model, body) || !body.is_object() {
            return None;
        }
        Some(Self { model, stream })
    }

    pub fn convert_request(&self, body: &Value) -> Option<Value> {
        let mut messages = Vec::new();
        if truthy(&body["systemInstruction"]) {
            let text = join_parts(&body["systemInstruction"]["parts"])?;
            if !text.is_empty() {
                messages.push(json!({ "role": "system", "content": text }));
            }
        }
        if truthy(&body["contents"]) {
            for content in body["contents"].as_array()? {
                if content.is_null() {
                    return None;
                }
                let role = if content["role"] == "model" {
                    "assistant"
                } else {
                    "user"
                };
                messages.push(json!({ "role": role, "content": join_parts(&content["parts"])? }));
            }
        }
        let mut result =
            json!({ "model": self.model, "messages": messages, "stream": self.stream });
        for (from, to) in [
            ("maxOutputTokens", "max_tokens"),
            ("temperature", "temperature"),
            ("topP", "top_p"),
        ] {
            if let Some(value) = body["generationConfig"].get(from) {
                result[to] = value.clone();
            }
        }
        Some(result)
    }

    pub async fn convert_response(self, response: Response) -> Response {
        if !response.status().is_success() {
            return response;
        }
        if self.stream {
            return self.stream_response(response);
        }
        let (parts, body) = response.into_parts();
        let Ok(bytes) = to_bytes(body, BODY_LIMIT).await else {
            return Response::from_parts(parts, Body::empty());
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            return Response::from_parts(parts, Body::from(bytes));
        };
        let body = if truthy(&value["candidates"])
            || truthy(&value["error"])
            || !truthy(&value["choices"][0])
        {
            value
        } else {
            let choice = &value["choices"][0];
            let message = &choice["message"];
            let mut parts = text_parts(message);
            let text = message
                .get("content")
                .filter(|value| truthy(value))
                .cloned()
                .unwrap_or(json!(""));
            parts.push(json!({ "text": text }));
            let mut payload = json!({
                "candidates": [{
                    "content": { "role": "model", "parts": parts },
                    "finishReason": finish_reason(&choice["finish_reason"]), "index": 0,
                }],
                "modelVersion": model_version(&value, &self.model),
            });
            if truthy(&value["usage"]) {
                payload["usageMetadata"] = usage(&value["usage"]);
            }
            payload
        };
        let mut response = Response::new(Body::from(body.to_string()));
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            header::HeaderValue::from_static("*"),
        );
        if let Some(vary) = parts.headers.get(header::VARY) {
            headers.insert(header::VARY, vary.clone());
        }
        response
    }

    fn stream_response(self, response: Response) -> Response {
        let mut pending = Vec::new();
        let model = self.model;
        let vary = response.headers().get(header::VARY).cloned();
        let stream = response
            .into_body()
            .into_data_stream()
            .filter_map(move |chunk| {
                let result = chunk.map(|bytes| {
                    pending.extend_from_slice(&bytes);
                    let complete = pending
                        .iter()
                        .rposition(|&byte| byte == b'\n')
                        .map_or(0, |index| index + 1);
                    let mut output = Vec::new();
                    for line in pending[..complete].split(|&byte| byte == b'\n') {
                        if let Some(event) = stream_chunk(line, &model) {
                            output.extend_from_slice(b"data: ");
                            output.extend_from_slice(event.to_string().as_bytes());
                            output.extend_from_slice(b"\r\n\r\n");
                        }
                    }
                    pending.drain(..complete);
                    Bytes::from(output)
                });
                let result = match result {
                    Ok(bytes) if bytes.is_empty() => None,
                    result => Some(result),
                };
                std::future::ready(result)
            });
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = StatusCode::OK;
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-cache"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            header::HeaderValue::from_static("*"),
        );
        if let Some(vary) = vary {
            headers.insert(header::VARY, vary);
        }
        response
    }
}

fn join_parts(parts: &Value) -> Option<String> {
    if parts.is_null() {
        return Some(String::new());
    }
    let mut strings = Vec::new();
    for part in parts.as_array()? {
        if part.is_null() {
            return None;
        }
        strings.push(
            part.get("text")
                .filter(|text| !text.is_null())
                .map_or_else(String::new, js_text),
        );
    }
    Some(strings.join("\n"))
}

pub(super) fn model_action(path: &str) -> Option<(String, bool)> {
    let rest = PATHS.iter().find_map(|prefix| path.strip_prefix(prefix))?;
    let mut segments = rest
        .split('/')
        .map(|raw| percent_decode_str(raw).decode_utf8());
    let first = segments.next()?.ok()?;
    let second = segments.next().transpose().ok()?;
    let (prefix, operation) = second.as_ref().map_or((None, first.as_ref()), |operation| {
        (Some(first.as_ref()), operation.as_ref())
    });
    let stream = operation.contains(":streamGenerateContent");
    let name = operation
        .replacen(":streamGenerateContent", "", 1)
        .replacen(":generateContent", "", 1);
    let model = match prefix {
        Some(provider) => format!("{provider}/{name}"),
        None => name,
    };
    Some((model, stream))
}

pub(super) fn native_model_id(model: &str) -> &str {
    let model = model.strip_prefix("models/").unwrap_or(model);
    model.strip_prefix("gemini/").unwrap_or(model)
}

pub(super) fn is_native_tts(model: &str, body: &Value) -> bool {
    if model.contains('/') && !model.starts_with("gemini/") && !model.starts_with("models/") {
        return false;
    }
    if body["generationConfig"]["responseModalities"]
        .as_array()
        .is_some_and(|modalities| {
            modalities
                .iter()
                .any(|value| js_text(value).eq_ignore_ascii_case("audio"))
        })
    {
        return true;
    }
    let model = native_model_id(model);
    let models = &crate::openai_models::data()["models"];
    models["gemini"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|entry| entry["id"] == model && (entry["kind"] == "tts" || entry["type"] == "tts"))
        || models["gemini-tts-models"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|entry| entry["id"] == model)
}

fn stream_chunk(line: &[u8], model: &str) -> Option<Value> {
    let data = std::str::from_utf8(line)
        .ok()?
        .strip_prefix("data:")?
        .trim_matches(js_space);
    let parsed: Value = serde_json::from_str(data).ok()?;
    let choice = parsed["choices"].get(0)?;
    let delta = &choice["delta"];
    let mut parts = text_parts(delta);
    if truthy(&delta["content"]) {
        parts.push(json!({ "text": delta["content"] }));
    }
    let reason = &choice["finish_reason"];
    if parts.is_empty() && !truthy(reason) {
        return None;
    }
    if parts.is_empty() {
        parts.push(json!({ "text": "" }));
    }
    let mut result = json!({
        "candidates": [{ "content": { "role": "model", "parts": parts }, "index": 0 }],
    });
    if truthy(reason) {
        result["candidates"][0]["finishReason"] = json!(finish_reason(reason));
        if truthy(&parsed["usage"]) {
            result["usageMetadata"] = usage(&parsed["usage"]);
            result["modelVersion"] = model_version(&parsed, model);
        }
    }
    Some(result)
}

fn text_parts(message: &Value) -> Vec<Value> {
    if truthy(&message["reasoning_content"]) {
        vec![json!({ "text": message["reasoning_content"], "thought": true })]
    } else {
        Vec::new()
    }
}

fn finish_reason(reason: &Value) -> &'static str {
    match reason.as_str() {
        Some("length") => "MAX_TOKENS",
        Some("content_filter") => "SAFETY",
        _ => "STOP",
    }
}

fn model_version(value: &Value, model: &str) -> Value {
    value
        .get("model")
        .filter(|model| truthy(model))
        .cloned()
        .unwrap_or_else(|| json!(model))
}

fn usage(value: &Value) -> Value {
    let mut metadata = json!({});
    for (from, to) in [
        ("prompt_tokens", "promptTokenCount"),
        ("completion_tokens", "candidatesTokenCount"),
        ("total_tokens", "totalTokenCount"),
    ] {
        metadata[to] = value
            .get(from)
            .filter(|value| truthy(value))
            .cloned()
            .unwrap_or(json!(0));
    }
    let thought = &value["completion_tokens_details"]["reasoning_tokens"];
    if truthy(thought) {
        metadata["thoughtsTokenCount"] = thought.clone();
    }
    metadata
}
