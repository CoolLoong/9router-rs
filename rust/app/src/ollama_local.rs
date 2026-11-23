use std::collections::HashMap;
use std::io;
use std::time::Duration;

use axum::http::header;
use futures_util::stream;
use reqwest::{Body, RequestBuilder, Response, ResponseBuilderExt};
use serde_json::{Map, Value, json};

use crate::chat_input::{js_json, js_space, preprocess_tool_history, truthy};
use crate::sse_reader::{ReadLine, SseReader};

pub const DEFAULT_HOST: &str = "http://localhost:11434";
pub(crate) const CLOUD_CHAT: &str = "https://ollama.com/api/chat";

pub(crate) fn cloud_fixture() -> Option<&'static str> {
    crate::provider_http::fixture().then_some("http://127.0.0.1:52989/ollama/api/chat")
}

pub(crate) fn cloud_credentials(account: &Value) -> bool {
    matches!(account["authType"].as_str(), Some("apikey" | "api_key"))
        && ["apiKey", "accessToken"]
            .into_iter()
            .any(|key| account[key].as_str().is_some_and(|token| !token.is_empty()))
}

pub(crate) fn host(account: &Value) -> Option<&str> {
    let saved = &account["providerSpecificData"]["baseUrl"];
    if !saved.is_null() && !saved.is_string() {
        return None;
    }
    let base = saved
        .as_str()
        .map(|url| url.trim_matches(js_space))
        .filter(|url| !url.is_empty())
        .unwrap_or(DEFAULT_HOST);
    Some(base.strip_suffix('/').unwrap_or(base))
}

pub fn url(account: &Value) -> Option<String> {
    let base = host(account)?;
    let endpoint = format!("{base}/api/chat");
    let parsed = url::Url::parse(&endpoint).ok()?;
    (matches!(parsed.scheme(), "http" | "https") && parsed.has_host()).then_some(endpoint)
}

pub fn headers(
    request: RequestBuilder,
    account: &Value,
    settings: &Value,
    provider: &str,
    streaming: bool,
) -> RequestBuilder {
    let access = account
        .get("apiKey")
        .filter(|value| truthy(value))
        .or_else(|| account.get("accessToken"));
    let credential = access.map(js_text).unwrap_or_else(|| "undefined".into());
    let mut fields = vec![("Content-Type", "application/json".to_owned())];
    if streaming {
        fields.push(("Accept", "text/event-stream".into()));
    }
    request
        .bearer_auth(credential)
        .headers(crate::github_executor::configured_headers(
            fields, settings, provider,
        ))
}

pub fn prepare(mut body: Value, model: &str, streaming: bool) -> Option<Value> {
    body.as_object()?;
    let mut result = json!({"model": model});
    if let Some(messages) = body.as_object_mut()?.get_mut("messages") {
        if let Some(messages) = messages.as_array_mut() {
            preprocess_tool_history(messages)?;
            result["messages"] = Value::Array(normalize_messages(messages)?);
        } else {
            result["messages"] = messages.clone();
        }
    }
    result["stream"] = json!(streaming);
    let mut options = Map::new();
    for (source, target) in [
        ("temperature", "temperature"),
        ("max_tokens", "num_predict"),
        ("top_p", "top_p"),
    ] {
        if let Some(value) = body.get(source) {
            options.insert(target.into(), value.clone());
        }
    }
    if !options.is_empty() {
        result["options"] = Value::Object(options);
    }
    if body["tools"].is_array() {
        result["tools"] = body["tools"].clone();
    }
    if truthy(&body["tool_choice"]) {
        result["tool_choice"] = body["tool_choice"].clone();
    }
    Some(result)
}

fn normalize_messages(messages: &[Value]) -> Option<Vec<Value>> {
    let mut names = HashMap::new();
    for message in messages {
        message.as_object()?;
        if message["role"] == "assistant" && truthy(&message["tool_calls"]) {
            for call in message["tool_calls"].as_array()? {
                if truthy(&call["id"]) && truthy(&call["function"]["name"]) {
                    names.insert(js_text(&call["id"]), call["function"]["name"].clone());
                }
            }
        }
    }

    let mut normalized = Vec::with_capacity(messages.len());
    for message in messages {
        let role = &message["role"];
        let content = normalize_content(&message["content"]);
        if role == "tool" {
            if !content.is_empty() {
                let name = names
                    .get(&js_text(&message["tool_call_id"]))
                    .filter(|value| truthy(value))
                    .or_else(|| message.get("name").filter(|value| truthy(value)))
                    .cloned()
                    .unwrap_or_else(|| json!("unknown_tool"));
                normalized.push(json!({"role":"tool","tool_name":name,"content":content}));
            }
            continue;
        }
        if role == "assistant" && truthy(&message["tool_calls"]) {
            let calls = message["tool_calls"]
                .as_array()?
                .iter()
                .map(|call| {
                    let function = &call["function"];
                    let arguments = if let Some(arguments) = function["arguments"].as_str() {
                        serde_json::from_str(arguments).unwrap_or_else(|_| json!({}))
                    } else {
                        fallback(&function["arguments"], json!({}))
                    };
                    json!({"type":"function","function":{
                        "index":fallback(&call["index"],json!(0)),
                        "name":fallback(&function["name"],json!("")),
                        "arguments":arguments,
                    }})
                })
                .collect::<Vec<_>>();
            normalized.push(json!({"role":"assistant","content":content,"tool_calls":calls}));
            continue;
        }
        if content.is_empty() && role != "assistant" {
            continue;
        }
        let mut outgoing = json!({"content":content});
        if let Some(role) = message.get("role") {
            outgoing["role"] = role.clone();
        }
        let images = message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "image_url")
            .filter_map(|block| {
                let image = &block["image_url"];
                let url = image.as_str().or_else(|| image["url"].as_str())?;
                let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
                (!mime.is_empty() && !mime.contains(';') && !data.is_empty()).then_some(data)
            })
            .collect::<Vec<_>>();
        if !images.is_empty() {
            outgoing["images"] = json!(images);
        }
        normalized.push(outgoing);
    }
    Some(normalized)
}

fn normalize_content(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "text" && truthy(&part["text"]))
        .map(|part| js_text(&part["text"]))
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn adapt(response: Response, model: &str, streaming: bool, stall: Duration) -> Response {
    if !response.status().is_success() {
        return response;
    }
    let status = response.status();
    let mut headers = response.headers().clone();
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::CONTENT_ENCODING);
    let builder = axum::http::Response::builder()
        .status(status)
        .url(response.url().clone());

    let body = if streaming {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !content_type.is_empty()
            && ![
                "application/x-ndjson",
                "application/ndjson",
                "application/json",
                "text/event-stream",
            ]
            .iter()
            .any(|mime| content_type.contains(mime))
        {
            return response;
        }
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/event-stream"),
        );
        let decoder = Decoder {
            reader: SseReader::new(response),
            state: StreamState::new(model),
            stall,
            ended: false,
        };
        Body::wrap_stream(stream::unfold(decoder, |mut decoder| async move {
            decoder.next().await.map(|chunk| (chunk, decoder))
        }))
    } else {
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        match response.json::<Value>().await {
            Ok(body) => {
                Body::from(to_chat(&body, chrono::Utc::now().timestamp_millis()).to_string())
            }
            Err(_) => Body::from(""),
        }
    };
    let mut output = builder.body(body).expect("valid adapted Ollama response");
    *output.headers_mut() = headers;
    output.into()
}

struct Decoder {
    reader: SseReader,
    state: StreamState,
    stall: Duration,
    ended: bool,
}

impl Decoder {
    async fn next(&mut self) -> Option<Result<Vec<u8>, io::Error>> {
        if self.ended {
            return None;
        }
        loop {
            match self.reader.next(self.stall, None).await {
                ReadLine::Line(line, _) => {
                    let line = line.trim_matches(js_space);
                    if !line.starts_with('{') {
                        continue;
                    }
                    if let Ok(event) = serde_json::from_str::<Value>(line)
                        && let Some(frame) = self
                            .state
                            .push(&event, chrono::Utc::now().timestamp_millis())
                    {
                        return Some(Ok(format!("data: {frame}\n\n").into_bytes()));
                    }
                }
                ReadLine::End => {
                    self.ended = true;
                    return None;
                }
                ReadLine::Timeout => {
                    self.ended = true;
                    return Some(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Ollama stream timed out",
                    )));
                }
                ReadLine::Disconnected => {
                    self.ended = true;
                    return Some(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Ollama stream disconnected",
                    )));
                }
            }
        }
    }
}

struct StreamState {
    model: Value,
    started: Option<i64>,
    tools: bool,
}

impl StreamState {
    fn new(model: &str) -> Self {
        Self {
            model: json!(model),
            started: None,
            tools: false,
        }
    }

    fn push(&mut self, event: &Value, now: i64) -> Option<Value> {
        let time = *self.started.get_or_insert_with(|| {
            if truthy(&event["model"]) {
                self.model = event["model"].clone();
            }
            now
        });
        let (delta, reason) = if truthy(&event["done"]) {
            (json!({}), Some(finish(&event["done_reason"], self.tools)))
        } else {
            let message = &event["message"];
            let content = message["content"].as_str().filter(|text| !text.is_empty());
            let thinking = message["thinking"].as_str().filter(|text| !text.is_empty());
            let calls = message["tool_calls"].as_array();
            if content.is_none() && thinking.is_none() && calls.is_none() {
                return None;
            }
            let mut delta = json!({});
            if let Some(text) = content {
                delta["content"] = json!(text);
            }
            if let Some(text) = thinking {
                delta["reasoning_content"] = json!(text);
            }
            if let Some(calls) = calls {
                self.tools = true;
                delta["tool_calls"] = json!(tool_calls(calls, now));
            }
            (delta, None)
        };
        let mut frame = json!({
            "id":format!("chatcmpl-{time}"),"object":"chat.completion.chunk","created":time.div_euclid(1000),
            "model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":reason}],
        });
        if reason.is_some() {
            frame["usage"] = usage(event);
        }
        Some(frame)
    }
}

fn to_chat(body: &Value, now: i64) -> Value {
    let upstream = &body["message"];
    let mut message = json!({"role":"assistant"});
    for (field, key) in [("content", "content"), ("thinking", "reasoning_content")] {
        if truthy(&upstream[field]) {
            message[key] = upstream[field].clone();
        }
    }
    let tools = upstream["tool_calls"]
        .as_array()
        .filter(|values| !values.is_empty());
    if let Some(calls) = tools {
        message["tool_calls"] = json!(tool_calls(calls, now));
    } else if message.get("content").is_none() {
        message["content"] = json!("");
    }
    json!({
        "id":format!("chatcmpl-{now}"),"object":"chat.completion","created":now.div_euclid(1000),
        "model":fallback(&body["model"],json!("ollama")),
        "choices":[{"index":0,"message":message,"finish_reason":finish(&body["done_reason"], tools.is_some())}],
        "usage":usage(body),
    })
}

fn tool_calls(calls: &[Value], now: i64) -> Vec<Value> {
    calls.iter().enumerate().map(|(index, call)| {
        let function = &call["function"];
        let arguments = function["arguments"].as_str().map(str::to_owned).unwrap_or_else(|| {
            let value = fallback(&function["arguments"],json!({}));
            js_json(&value,false).unwrap_or_else(|| value.to_string())
        });
        json!({
            "index":function.get("index").filter(|value| !value.is_null()).cloned().unwrap_or_else(|| json!(index)),
            "id":fallback(&call["id"],json!(format!("call_{index}_{now}"))),
            "type":"function","function":{"name":fallback(&function["name"],json!("")),"arguments":arguments},
        })
    }).collect()
}

fn finish(reason: &Value, tools: bool) -> &'static str {
    if tools || matches!(reason.as_str(), Some("tool_calls" | "tool_use")) {
        "tool_calls"
    } else if matches!(reason.as_str(), Some("length" | "max_tokens")) {
        "length"
    } else {
        "stop"
    }
}

fn usage(event: &Value) -> Value {
    let input = event["prompt_eval_count"].as_f64().unwrap_or_default();
    let output = event["eval_count"].as_f64().unwrap_or_default();
    let cache = event["prompt_eval_cached_count"]
        .as_f64()
        .unwrap_or_default();
    let mut result = json!({"prompt_tokens":number(input),"completion_tokens":number(output),"total_tokens":number(input+output)});
    if cache > 0.0 {
        result["prompt_tokens_details"] = json!({"cached_tokens":number(cache)});
    }
    result
}

fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn fallback(value: &Value, otherwise: Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        otherwise
    }
}

fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) => "[object Object]".into(),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    js_text(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => value.to_string(),
    }
}
