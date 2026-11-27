use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};

use crate::chat_format::ChatFormat;
use crate::chat_input::js_json;
use crate::chat_output::{
    UsageCallback, chat_to_messages, error, js_add, js_text, json_response, non_stream_reason,
    normalize_json, reply, response_usage, stall_timeout,
};
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::sse_reader::{ReadLine, SseReader};

const DONE: &str = "data: [DONE]\n\n";
const MAX_OUTPUT_ITEMS: usize = 16_384;

pub(crate) struct Output<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub client: ChatFormat,
    pub client_stream: bool,
    pub executor_stream: bool,
    pub clean_stream_usage: bool,
}

pub async fn respond(
    upstream: reqwest::Response,
    output: Output<'_>,
    on_usage: Option<UsageCallback>,
    trace: Option<&crate::observability::Trace>,
) -> Response {
    let Output {
        provider,
        model,
        client,
        client_stream,
        executor_stream,
        clean_stream_usage,
    } = output;
    let headers = upstream.headers().clone();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if !client_stream
        && (content_type.contains("text/event-stream") || content_type.is_empty())
    {
        let Some(value) = collect(upstream, stall_timeout(provider)).await else {
            return error(
                StatusCode::BAD_GATEWAY,
                "Failed to convert streaming response to JSON",
            );
        };
        if let Some(trace) = trace {
            trace.raw_response(&value);
        }
        if let Some(callback) = on_usage {
            callback(value["usage"].clone());
        }
        let value = match client {
            ChatFormat::Responses => value,
            ChatFormat::Chat => to_chat(&value, model),
            ChatFormat::Messages => chat_to_messages(to_chat(&value, model)),
        };
        return json_response(StatusCode::OK, value, None);
    }

    if executor_stream && (client_stream || !content_type.contains("application/json")) {
        if !content_type.is_empty()
            && !content_type.contains("text/event-stream")
            && !content_type.contains("application/json")
        {
            let status = upstream.status();
            let body = upstream.text().await.unwrap_or_default();
            let reason = non_stream_reason(&body, &content_type);
            return json_response(
                status,
                json!({ "error": { "message": format!("[{}]: {reason}", status.as_u16()) } }),
                None,
            );
        }
        let body = if client == ChatFormat::Responses {
            let passthrough = ResponsesStream::new(
                upstream,
                stall_timeout(provider),
                on_usage,
                clean_stream_usage,
            );
            Body::from_stream(stream::unfold(passthrough, |mut state| async move {
                let chunk = state.next().await?;
                Some((Ok::<_, Infallible>(chunk), state))
            }))
        } else {
            Body::from_stream(
                upstream
                    .bytes_stream()
                    .filter_map(|_| std::future::ready(None::<Result<Bytes, reqwest::Error>>)),
            )
        };
        return reply(StatusCode::OK, body, true, Some(&headers));
    }

    let Ok(mut value) = upstream.json::<Value>().await else {
        return error(
            StatusCode::BAD_GATEWAY,
            &format!("Invalid JSON response from {provider}"),
        );
    };
    if let Some(trace) = trace {
        trace.raw_response(&value);
    }
    if let (Some(callback), Some(usage)) = (on_usage, response_usage(&value)) {
        callback(usage);
    }
    normalize_json(&mut value, client);
    json_response(StatusCode::OK, value, Some(&headers))
}

#[derive(Default)]
struct Event {
    name: Option<String>,
    data: Option<String>,
}

impl Event {
    fn read(&mut self, line: &str) {
        if let Some(name) = line.strip_prefix("event:").map(js_trim) {
            if self.name.is_none() && !name.is_empty() {
                self.name = Some(name.into());
            }
        } else if let Some(data) = line.strip_prefix("data:").map(js_trim)
            && self.data.is_none()
            && !data.is_empty()
        {
            self.data = Some(data.into());
        }
    }
}

struct Aggregate {
    response_id: Value,
    created: Value,
    status: &'static str,
    items: BTreeMap<usize, Value>,
    usage: Value,
}

impl Aggregate {
    fn new() -> Self {
        Self {
            response_id: Value::Null,
            created: json!(chrono::Utc::now().timestamp()),
            status: "in_progress",
            items: BTreeMap::new(),
            usage: json!({ "input_tokens": 0, "output_tokens": 0, "total_tokens": 0 }),
        }
    }

    fn record(&mut self, event: Event) -> Option<()> {
        let (Some(name), Some(data)) = (event.name, event.data) else {
            return Some(());
        };
        let Ok(data) = serde_json::from_str::<Value>(&data) else {
            return Some(());
        };
        match name.as_str() {
            "response.created" => {
                if truthy(&data["response"]["id"]) {
                    self.response_id = data["response"]["id"].clone();
                }
                if truthy(&data["response"]["created_at"]) {
                    self.created = data["response"]["created_at"].clone();
                }
            }
            "response.output_item.done" => {
                let index = match &data["output_index"] {
                    Value::Null => 0,
                    value => {
                        let Some(index) = value
                            .as_f64()
                            .filter(|value| value.fract() == 0.0 && *value >= 0.0)
                        else {
                            return Some(());
                        };
                        if index >= MAX_OUTPUT_ITEMS as f64 {
                            return None;
                        }
                        index as usize
                    }
                };
                self.items.insert(index, data["item"].clone());
            }
            "response.completed" | "response.done" => {
                self.status = "completed";
                let usage = &data["response"]["usage"];
                if truthy(usage) {
                    self.usage = json!({
                        "input_tokens": value_or(&usage["input_tokens"], json!(0)),
                        "output_tokens": value_or(&usage["output_tokens"], json!(0)),
                        "total_tokens": value_or(&usage["total_tokens"], json!(0)),
                    });
                }
            }
            "response.failed" => self.status = "failed",
            _ => (),
        }
        Some(())
    }

    fn into_value(mut self) -> Value {
        let mut output = Vec::new();
        if let Some((&last_index, _)) = self.items.last_key_value() {
            for index in 0..=last_index {
                output.push(self.items.remove(&index).filter(truthy).unwrap_or_else(
                    || json!({ "type": "message", "content": [], "role": "assistant" }),
                ));
            }
        }
        let id = if truthy(&self.response_id) {
            self.response_id
        } else {
            let randomness = uuid::Uuid::new_v4().simple().to_string();
            json!(format!(
                "resp_{}_{}",
                chrono::Utc::now().timestamp_millis(),
                &randomness[..6]
            ))
        };
        json!({
            "id": id,
            "object": "response",
            "created_at": self.created,
            "status": self.status,
            "output": output,
            "usage": self.usage,
        })
    }
}

async fn collect(upstream: reqwest::Response, stall: Duration) -> Option<Value> {
    let mut reader = SseReader::new(upstream);
    let mut aggregate = Aggregate::new();
    let mut event = Event::default();
    loop {
        match reader.next(stall, None).await {
            ReadLine::Line(line, true) if line.is_empty() => {
                aggregate.record(std::mem::take(&mut event))?;
            }
            ReadLine::Line(line, _) => event.read(&line),
            ReadLine::End => {
                aggregate.record(event)?;
                return Some(aggregate.into_value());
            }
            ReadLine::Timeout | ReadLine::Disconnected => return None,
        }
    }
}

fn value_or(value: &Value, fallback: Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        fallback
    }
}

fn message_text(item: &Value) -> &str {
    let Some(content) = item["content"].as_array() else {
        return "";
    };
    content
        .iter()
        .find(|part| part["type"] == "output_text")
        .and_then(|part| part["text"].as_str())
        .or_else(|| content.iter().find_map(|part| part["text"].as_str()))
        .unwrap_or_default()
}

fn tool_call(item: &Value, index: usize) -> Value {
    let now = chrono::Utc::now().timestamp_millis();
    let name = item
        .get("name")
        .map(js_text)
        .unwrap_or_else(|| "undefined".to_owned());
    let id = if truthy(&item["call_id"]) {
        item["call_id"].clone()
    } else {
        json!(format!("call_{name}_{now}_{index}"))
    };
    let arguments = if let Some(text) = item["arguments"].as_str() {
        text.to_owned()
    } else {
        let value = value_or(&item["arguments"], json!({}));
        js_json(&value, false).unwrap_or_else(|| value.to_string())
    };
    let mut function = Map::new();
    if let Some(name) = item.get("name") {
        function.insert("name".into(), name.clone());
    }
    function.insert("arguments".into(), json!(arguments));
    json!({ "id": id, "type": "function", "function": function })
}

fn to_chat(response: &Value, model: &str) -> Value {
    let output = response["output"].as_array().map_or(&[][..], Vec::as_slice);
    let answer = output
        .iter()
        .rev()
        .filter(|item| item["type"] == "message")
        .map(message_text)
        .find(|text| !text.is_empty());
    let tools: Vec<_> = output
        .iter()
        .filter(|item| item["type"] == "function_call")
        .enumerate()
        .map(|(index, item)| tool_call(item, index))
        .collect();
    let has_tools = !tools.is_empty();
    let content = answer.map_or_else(
        || if has_tools { Value::Null } else { json!("") },
        |answer| json!(answer),
    );
    let mut message = json!({ "role": "assistant", "content": content });
    if has_tools {
        message["tool_calls"] = json!(tools);
    }
    let status = &response["status"];
    let finish = if has_tools {
        json!("tool_calls")
    } else if status == "completed" || status == "done" {
        json!("stop")
    } else {
        value_or(status, json!("stop"))
    };

    let usage = &response["usage"];
    let prompt = value_or(&usage["input_tokens"], json!(0));
    let completion = value_or(&usage["output_tokens"], json!(0));
    let total = js_add(&prompt, &completion);
    let now = chrono::Utc::now();
    json!({
        "id": value_or(&response["id"], json!(format!("chatcmpl-{}", now.timestamp_millis()))),
        "object": "chat.completion",
        "created": value_or(&response["created_at"], json!(now.timestamp())),
        "model": value_or(&response["model"], json!(model)),
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": { "prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": total },
    })
}

struct ResponsesStream {
    reader: SseReader,
    stall: Duration,
    sent_done: bool,
    closed: bool,
    usage: Option<Value>,
    on_usage: Option<UsageCallback>,
    clean_stream_usage: bool,
}

impl ResponsesStream {
    fn new(
        upstream: reqwest::Response,
        stall: Duration,
        on_usage: Option<UsageCallback>,
        clean_stream_usage: bool,
    ) -> Self {
        Self {
            reader: SseReader::new(upstream),
            stall,
            sent_done: false,
            closed: false,
            usage: None,
            on_usage,
            clean_stream_usage,
        }
    }

    async fn next(&mut self) -> Option<Bytes> {
        while !self.closed {
            match self.reader.next(self.stall, None).await {
                ReadLine::Line(line, terminated) => {
                    if let Some(frame) = self.line(line, terminated) {
                        return Some(frame);
                    }
                }
                ReadLine::End => {
                    self.closed = true;
                    self.save_usage();
                    if !self.sent_done {
                        return Some(Bytes::from_static(DONE.as_bytes()));
                    }
                }
                ReadLine::Timeout | ReadLine::Disconnected => {
                    self.closed = true;
                    return Some(aborted());
                }
            }
        }
        None
    }

    fn line(&mut self, mut line: String, terminated: bool) -> Option<Bytes> {
        if let Some(data) = js_trim(&line).strip_prefix("data:").map(js_trim) {
            if data == "[DONE]" {
                if !terminated {
                    return None;
                }
                self.sent_done = true;
            } else if terminated {
                let mut value = serde_json::from_str::<Value>(data).ok()?;
                if value.is_null() {
                    return None;
                }
                let kind = value["type"].as_str().unwrap_or_default();
                if matches!(kind, "response.completed" | "response.done") {
                    if let Some(usage) = nested_usage(&value) {
                        self.usage = Some(usage);
                    }
                } else if value["usage"].get("prompt_tokens").is_some()
                    && let Some(usage) = response_usage(&value)
                {
                    self.usage = Some(usage);
                }
                if matches!(
                    kind,
                    "response.completed" | "response.done" | "response.failed" | "error"
                ) || matches!(
                    value["response"]["status"].as_str(),
                    Some("completed" | "failed")
                ) {
                    self.save_usage();
                }
                if self.clean_stream_usage && clean_usage(&mut value) {
                    line = format!("data: {value}");
                }
            }
        }
        if line.starts_with("data:") && !line.starts_with("data: ") {
            line.insert(5, ' ');
        }
        if terminated {
            line.push('\n');
        }
        Some(Bytes::from(line))
    }

    fn save_usage(&mut self) {
        if let (Some(callback), Some(usage)) = (self.on_usage.take(), self.usage.take()) {
            callback(usage);
        }
    }
}

fn clean_usage(value: &mut Value) -> bool {
    let Some(fields) = value.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    if fields.get("usage").is_some_and(Value::is_null) {
        fields.shift_remove("usage");
        changed = true;
    } else if let Some(usage) = fields.get_mut("usage").and_then(Value::as_object_mut)
        && usage.get("perf_metrics").is_some_and(Value::is_null)
    {
        usage.shift_remove("perf_metrics");
        changed = true;
    }
    if let Some(response) = fields.get_mut("response") {
        changed |= clean_usage(response);
    }
    changed
}

fn nested_usage(value: &Value) -> Option<Value> {
    let usage = &value["response"]["usage"];
    usage.as_object()?;
    let input = if truthy(&usage["input_tokens"]) {
        &usage["input_tokens"]
    } else {
        &usage["prompt_tokens"]
    };
    let output = if truthy(&usage["output_tokens"]) {
        &usage["output_tokens"]
    } else {
        &usage["completion_tokens"]
    };
    let mut canonical = json!({
        "prompt_tokens": value_or(input, json!(0)),
        "completion_tokens": value_or(output, json!(0)),
    });
    if let Some(cache) = usage["input_tokens_details"].get("cached_tokens") {
        canonical["cached_tokens"] = cache.clone();
    }
    if let Some(reasoning) = usage["output_tokens_details"].get("reasoning_tokens") {
        canonical["reasoning_tokens"] = reasoning.clone();
    }
    Some(canonical)
}

fn aborted() -> Bytes {
    let event = json!({
        "type": "response.failed",
        "response": {
            "id": format!("resp_{}", chrono::Utc::now().timestamp_millis()),
            "status": "failed",
            "error": {
                "type": "stream_error",
                "code": "stream_disconnected",
                "message": "stream closed before response.completed",
            },
        },
    });
    Bytes::from(format!("event: response.failed\ndata: {event}\n\n{DONE}"))
}
