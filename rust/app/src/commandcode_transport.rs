use std::collections::HashMap;
use std::io;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode, header};
use futures_util::stream;
use reqwest::{Body, RequestBuilder, Response, ResponseBuilderExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use uuid::Uuid;

use crate::chat_input::{PreparedChat, js_json, js_space, preprocess_tool_history, truthy};
use crate::chat_output::{js_add, js_text};
use crate::commandcode_identity::{IDENTITY, client_headers, session_headers};
use crate::gemini_signatures;
use crate::proxy_transport::{self, Route};
use crate::sse_reader::{ReadLine, SseReader};
use crate::storage::Storage;

const ENDPOINT: &str = "https://api.commandcode.ai/alpha/generate";
const CATALOG_ENDPOINT: &str = "https://api.commandcode.ai/provider/v1/models?view=client";
const SESSION_TTL: Duration = Duration::from_secs(7200);
const MAX_PRELUDE_LINES: usize = 1024;
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

pub fn supports(account: &Value) -> bool {
    account["authType"] == "apikey" && authorization(account).is_some()
}

pub fn url() -> &'static str {
    ENDPOINT
}

pub fn fixture_url() -> Option<&'static str> {
    (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"))
        .then_some("http://127.0.0.1:52989/commandcode/alpha/generate")
}

pub async fn models(storage: &Storage, settings: &Value) -> Option<Vec<Value>> {
    const LIMIT: usize = 4 * 1024 * 1024;
    let route = proxy_transport::select(storage, settings, &Value::Null, CATALOG_ENDPOINT)
        .await
        .ok()?;
    let client = crate::provider_imports::metadata_client(&route, Duration::from_secs(10))?;
    let endpoint = match &route {
        Route::Relay { url, .. } => url,
        Route::Direct if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") => {
            "http://127.0.0.1:52989/commandcode/provider/v1/models?view=client"
        }
        _ => CATALOG_ENDPOINT,
    };
    let request = client_headers(client.get(endpoint));
    let mut response = route.relay(request).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > LIMIT - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let data: Value = serde_json::from_slice(&bytes).ok()?;
    let models = data["data"]
        .as_array()?
        .iter()
        .filter_map(|entry| {
            let id = entry["id"]
                .as_str()
                .filter(|id| !id.is_empty() && !truthy(&entry["hidden"]))?;
            let name = [&entry["display"]["label"], &entry["name"]]
                .into_iter()
                .find_map(|name| name.as_str().filter(|name| !name.is_empty()))
                .unwrap_or(id);
            let desc = entry["display"]
                .get("description")
                .filter(|value| truthy(value));
            let vision = entry["inputModalities"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item == "image"));
            let reasoning = entry["reasoningEfforts"]
                .as_array()
                .is_some_and(|items| !items.is_empty());
            let mut model = json!({"id":id,"name":name,"description":desc});
            if let Some(context) = entry.get("context_length") {
                model["contextLength"] = context.clone();
            }
            if let Some(output) = entry
                .get("maxOutputTokens")
                .filter(|value| !value.is_null())
            {
                model["maxOutputTokens"] = output.clone();
            }
            model["isVL"] = json!(vision);
            model["isReasoning"] = json!(reasoning);
            Some(model)
        })
        .collect::<Vec<_>>();
    (!models.is_empty()).then_some(models)
}

fn token(account: &Value) -> Option<&str> {
    present(&account["apiKey"], &account["accessToken"])
        .as_str()
        .filter(|token| !token.is_empty())
}

fn authorization(account: &Value) -> Option<header::HeaderValue> {
    let mut header = header::HeaderValue::from_str(&format!("Bearer {}", token(account)?)).ok()?;
    header.set_sensitive(true);
    Some(header)
}

pub fn headers(request: RequestBuilder, account: &Value, envelope: &Value) -> RequestBuilder {
    let request = session_headers(
        request.header(header::ACCEPT, "text/event-stream"),
        envelope["threadId"].as_str().unwrap_or(""),
    );
    match authorization(account) {
        Some(auth) => request.header(header::AUTHORIZATION, auth),
        None => request,
    }
}

#[derive(Default)]
struct SessionStore {
    accounts: HashMap<String, Session>,
    conversations: HashMap<(String, [u8; 32]), Session>,
}

struct Session {
    uuid: Uuid,
    used: Instant,
}

fn session_uuid(headers: &HeaderMap, body: &Value, account: &Value) -> Uuid {
    if let Some(client) = gemini_signatures::session_id(headers, body) {
        let hash = Sha256::digest(client.as_bytes());
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&hash[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        return Uuid::from_bytes(bytes);
    }
    static STORE: LazyLock<Mutex<SessionStore>> = LazyLock::new(Mutex::default);
    let id = account["id"].as_str().filter(|id| !id.is_empty());
    let conversation = assistant_prefix(body);
    let mut slots = STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(conversation) = conversation {
        return session(
            &mut slots.conversations,
            (id.unwrap_or_default().to_owned(), conversation),
            5000,
        );
    }
    id.map_or_else(Uuid::new_v4, |id| {
        session(&mut slots.accounts, id.to_owned(), 1000)
    })
}

fn session<K: Eq + std::hash::Hash + Clone>(
    slots: &mut HashMap<K, Session>,
    key: K,
    limit: usize,
) -> Uuid {
    let now = Instant::now();
    if let Some(previous) = slots.get_mut(&key)
        && now.duration_since(previous.used) < SESSION_TTL
    {
        previous.used = now;
        return previous.uuid;
    }
    if slots.len() >= limit
        && let Some(oldest) = slots.iter().min_by_key(|(_, value)| value.used)
    {
        let oldest = oldest.0.clone();
        slots.remove(&oldest);
    }
    let uuid = Uuid::new_v4();
    slots.insert(key, Session { uuid, used: now });
    uuid
}

fn assistant_prefix(body: &Value) -> Option<[u8; 32]> {
    let mut prefix = Vec::with_capacity(50);
    let turns = body["messages"]
        .as_array()
        .or_else(|| body["input"].as_array())?;
    for turn in turns.iter().filter(|turn| turn["role"] == "assistant") {
        if let Some(text) = turn["content"].as_str() {
            prefix.extend(text.encode_utf16().take(50 - prefix.len()));
        } else if let Some(parts) = turn["content"].as_array() {
            for part in parts {
                let value = present(&part["text"], &part["output"]);
                if truthy(value) {
                    prefix.extend(js_text(value).encode_utf16().take(50 - prefix.len()));
                }
                if prefix.len() == 50 {
                    break;
                }
            }
        }
        if prefix.len() == 50 {
            return Some(Sha256::digest(String::from_utf16_lossy(&prefix)).into());
        }
    }
    None
}

pub fn prepare_chat(mut body: Value, model: &str, translated: bool) -> Option<PreparedChat> {
    if body["input"].is_array()
        || body["contents"].is_array()
        || body["userAgent"] == "antigravity"
        || truthy(&body["conversationState"])
    {
        return None;
    }
    let messages = body["messages"].as_array_mut()?;
    if !translated {
        messages.retain(|message| !message.is_null());
        preprocess_tool_history(messages)?;
    }
    if messages.iter().any(|message| {
        message["content"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                part["type"] == "image_url"
                    && image_url(part).is_some_and(|url| {
                        url.starts_with("http://") || url.starts_with("https://")
                    })
            })
        })
    }) {
        return None;
    }
    body["model"] = json!(model);
    Some(PreparedChat { body, stream: true })
}

pub fn prepare(
    body: &Value,
    source: &Value,
    headers: &HeaderMap,
    model: &str,
    account: &Value,
) -> Result<Value, String> {
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| "Invalid CommandCode chat request body".to_owned())?;
    let (messages, system) = convert_messages(messages);
    let or_default = |value: &Value, default: Value| {
        if value.is_null() {
            default
        } else {
            value.clone()
        }
    };
    let mut params = json!({
        "model":model,
        "messages":messages,
        "stream":true,
        "max_tokens":or_default(not_null(&body["max_tokens"], &body["max_output_tokens"]), json!(64000)),
        "temperature":or_default(&body["temperature"], json!(0.3)),
    });
    if !system.is_empty() {
        params["system"] = json!(system);
    }
    let tools = convert_tools(&body["tools"]);
    if !tools.is_empty() {
        params["tools"] = json!(tools);
    }
    if !body["top_p"].is_null() {
        params["top_p"] = body["top_p"].clone();
    }
    let effort = [
        &body["params"]["reasoning_effort"],
        &body["reasoning_effort"],
        &body["output_config"]["effort"],
        &body["reasoning"]["effort"],
    ]
    .into_iter()
    .find(|value| !value.is_null())
    .and_then(Value::as_str)
    .filter(|value| !value.is_empty());
    if let Some(effort) = effort {
        params["reasoning_effort"] = json!(effort);
    }
    Ok(json!({
        "threadId":session_uuid(headers, source, account).to_string(),
        "memory":null,
        "taste":null,
        "skills":null,
        "permissionMode":"standard",
        "config":{
            "workingDir":IDENTITY.directory,
            "date":chrono::Utc::now().date_naive().to_string(),
            "environment":match std::env::consts::OS {"macos"=>"darwin","windows"=>"win32",other=>other},
            "structure":[],
            "isGitRepo":false,
            "currentBranch":"",
            "mainBranch":"",
            "gitStatus":"",
            "recentCommits":[],
        },
        "params":params,
        "model":model,
    }))
}

fn flatten_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.as_str().or_else(|| part["text"].as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        value => js_text(value),
    }
}

fn text_block(text: &str) -> Value {
    json!({"type":"text","text":text})
}

fn image_url(part: &Value) -> Option<&str> {
    part["image_url"]
        .as_str()
        .or_else(|| part["image_url"]["url"].as_str())
}

fn data_uri_mime(url: &str) -> Option<&str> {
    let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    (!mime.is_empty() && !mime.contains(';') && !data.is_empty()).then_some(mime)
}

fn image_block(part: &Value) -> Option<Value> {
    let block = |image: &str, mime: &str| json!({"type":"image","image":image,"mimeType":mime,"mediaType":mime});
    if part["type"] == "image_url" {
        let url = image_url(part)?;
        return Some(block(url, data_uri_mime(url)?));
    }
    if part["type"] != "image" {
        return None;
    }
    if let Some(image) = part["image"]
        .as_str()
        .filter(|image| image.starts_with("data:"))
    {
        let mime = part["mimeType"]
            .as_str()
            .filter(|mime| !mime.is_empty())
            .or_else(|| data_uri_mime(image))
            .unwrap_or("image/png");
        return Some(block(image, mime));
    }
    let source = &part["source"];
    if source["type"] != "base64" {
        return None;
    }
    let data = source["data"].as_str()?;
    let mime = source["media_type"]
        .as_str()
        .filter(|mime| !mime.is_empty())
        .unwrap_or("image/png");
    Some(block(&format!("data:{mime};base64,{data}"), mime))
}

fn user_blocks(content: &Value) -> Vec<Value> {
    let Some(parts) = content.as_array() else {
        return vec![text_block(&flatten_text(content))];
    };
    let blocks = parts
        .iter()
        .filter_map(|part| {
            part.as_str()
                .map(text_block)
                .or_else(|| image_block(part))
                .or_else(|| part["text"].as_str().map(text_block))
        })
        .collect::<Vec<_>>();
    with_empty_text(blocks)
}

fn with_empty_text(mut blocks: Vec<Value>) -> Vec<Value> {
    if blocks.is_empty() {
        blocks.push(text_block(""));
    }
    blocks
}

fn tool_name(call: &Value) -> &Value {
    present(&call["function"]["name"], &call["name"])
}

fn tool_input(input: &Value) -> Value {
    if input.is_null() {
        return json!({});
    }
    if let Some(text) = input.as_str() {
        let clean = text.trim_matches(js_space);
        if clean.is_empty() {
            return json!({});
        }
        return match serde_json::from_str::<Value>(clean) {
            Ok(value) if value.is_object() || value.is_array() => value,
            Ok(value) => json!({"value":value}),
            Err(_) => json!({"raw":text}),
        };
    }
    input.clone()
}

fn convert_messages(messages: &[Value]) -> (Vec<Value>, String) {
    let mut names = HashMap::new();
    for message in messages
        .iter()
        .filter(|message| message["role"] == "assistant")
    {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            let name = tool_name(call);
            if truthy(&call["id"]) && truthy(name) {
                names.insert(js_text(&call["id"]), name);
            }
        }
    }
    let mut out = Vec::with_capacity(messages.len());
    let mut system = Vec::new();
    for message in messages.iter().filter(|message| !message.is_null()) {
        let content = &message["content"];
        match message["role"].as_str() {
            Some("system") => {
                let text = flatten_text(content);
                if !text.is_empty() {
                    system.push(text);
                }
            }
            Some("tool") => {
                let id = &message["tool_call_id"];
                let name = if truthy(&message["name"]) {
                    message["name"].clone()
                } else {
                    names
                        .get(&js_text(id))
                        .map(|name| (*name).clone())
                        .unwrap_or_else(|| json!("unknown"))
                };
                let is_error = truthy(&message["is_error"]) || message["status"] == "error";
                out.push(json!({"role":"tool","content":[{
                    "type":"tool-result", "toolCallId":present(id, &json!("")), "toolName":name,
                    "output":{"type":if is_error {"error-text"} else {"text"}, "value":flatten_text(content)},
                }]}));
            }
            Some("assistant") => {
                let mut blocks = Vec::new();
                let reasoning = present(
                    &message["reasoning_content"],
                    present(&message["thought"], &message["reasoning"]),
                );
                if let Some(text) = reasoning
                    .as_str()
                    .filter(|text| !text.trim_matches(js_space).is_empty())
                {
                    blocks.push(json!({"type":"reasoning","text":text}));
                }
                let text = flatten_text(content);
                if !text.is_empty() {
                    blocks.push(text_block(&text));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    blocks.push(json!({
                        "type":"tool-call",
                        "toolCallId":present(&call["id"], &json!("")),
                        "toolName":present(tool_name(call), &json!("")),
                        "input":tool_input(not_null(&call["function"]["arguments"], &call["input"])),
                    }));
                }
                out.push(json!({"role":"assistant","content":with_empty_text(blocks)}));
            }
            _ => out.push(json!({"role":"user","content":user_blocks(content)})),
        }
    }
    (out, system.join("\n\n"))
}

fn convert_tools(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let (definition, schema) = if tool["type"] == "function" && truthy(&tool["function"]) {
                (
                    &tool["function"],
                    present(&tool["function"]["parameters"], &Value::Null),
                )
            } else {
                let schema = present(&tool["input_schema"], &tool["parameters"]);
                if !truthy(&tool["name"]) || !truthy(schema) {
                    return None;
                }
                (tool, schema)
            };
            let mut output = json!({});
            for field in ["name", "description"] {
                if let Some(value) = definition.get(field) {
                    output[field] = value.clone();
                }
            }
            output["input_schema"] = if schema.is_null() {
                json!({"type":"object"})
            } else {
                schema.clone()
            };
            Some(output)
        })
        .collect()
}

fn present<'a>(value: &'a Value, default: &'a Value) -> &'a Value {
    if truthy(value) { value } else { default }
}

fn not_null<'a>(value: &'a Value, default: &'a Value) -> &'a Value {
    if value.is_null() { default } else { value }
}

pub enum Probe {
    Response(Response),
    Rejected { status: StatusCode, message: String },
}

pub async fn probe(response: Response, model: &str, account: &Value, stall: Duration) -> Probe {
    if !response.status().is_success() || response.status() == StatusCode::NO_CONTENT {
        return Probe::Response(response);
    }
    let status = response.status();
    let mut headers = response.headers().clone();
    let url = response.url().clone();
    let mut decoder = Decoder {
        reader: SseReader::new(response).with_line_limit(MAX_EVENT_BYTES),
        converter: Convert::new(model),
        stall,
        first_frame: None,
        ended: false,
    };
    let deadline = Instant::now() + stall;
    for _ in 0..MAX_PRELUDE_LINES {
        match decoder.reader.next(stall, Some(deadline)).await {
            ReadLine::Line(line, _) => {
                let data = line_data(&line);
                if data == "[DONE]" {
                    break;
                }
                let Ok(event) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if event["type"] == "error" {
                    let (status, message) = parse_stream_error(&event);
                    return Probe::Rejected {
                        status,
                        message: format!("[CommandCode error: {}]", redact(account, &message)),
                    };
                }
                decoder.first_frame = decoder.converter.push(&event).map(frame);
                if decoder.first_frame.is_some()
                    || matches!(
                        event["type"].as_str(),
                        Some(
                            "finish-step"
                                | "tool-input-start"
                                | "tool-call"
                                | "text-delta"
                                | "reasoning-delta"
                                | "finish"
                        )
                    )
                {
                    break;
                }
            }
            ReadLine::End => break,
            ReadLine::Timeout => {
                return Probe::Rejected {
                    status: StatusCode::GATEWAY_TIMEOUT,
                    message: "stream stall timeout".into(),
                };
            }
            ReadLine::Disconnected => {
                return Probe::Rejected {
                    status: StatusCode::BAD_GATEWAY,
                    message: "upstream connection lost".into(),
                };
            }
        }
    }
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::CONTENT_ENCODING);
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    let body = Body::wrap_stream(stream::unfold(decoder, |mut decoder| async move {
        decoder.next().await.map(|frame| (frame, decoder))
    }));
    let mut adapted = axum::http::Response::builder()
        .status(status)
        .url(url)
        .body(body)
        .expect("CommandCode SSE response");
    *adapted.headers_mut() = headers;
    Probe::Response(adapted.into())
}

fn frame(chunk: Value) -> String {
    format!("data: {chunk}\n\n")
}

fn line_data(line: &str) -> &str {
    let line = line.trim_matches(js_space);
    line.strip_prefix("data:")
        .map(|data| data.trim_matches(js_space))
        .unwrap_or(line)
}

struct Decoder {
    reader: SseReader,
    converter: Convert,
    stall: Duration,
    first_frame: Option<String>,
    ended: bool,
}

impl Decoder {
    async fn next(&mut self) -> Option<Result<String, io::Error>> {
        if self.ended {
            return None;
        }
        if let Some(first) = self.first_frame.take() {
            return Some(Ok(first));
        }
        let error = loop {
            match self.reader.next(self.stall, None).await {
                ReadLine::Line(line, _) => {
                    let Ok(event) = serde_json::from_str::<Value>(line_data(&line)) else {
                        continue;
                    };
                    if event["type"] == "error" {
                        break "upstream connection lost";
                    }
                    if let Some(chunk) = self.converter.push(&event) {
                        return Some(Ok(frame(chunk)));
                    }
                }
                ReadLine::End => {
                    self.ended = true;
                    return None;
                }
                ReadLine::Timeout => break "stream stall timeout",
                ReadLine::Disconnected => break "upstream connection lost",
            }
        };
        self.ended = true;
        Some(Err(io::Error::other(error)))
    }
}

struct Convert {
    model: String,
    created: Option<i64>,
    began: bool,
    tool_indices: HashMap<String, usize>,
    finish: Option<Value>,
    usage: Value,
}

impl Convert {
    fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            created: None,
            began: false,
            tool_indices: HashMap::new(),
            finish: None,
            usage: Value::Null,
        }
    }

    fn push(&mut self, event: &Value) -> Option<Value> {
        if event["object"] == "chat.completion.chunk" {
            return Some(event.clone());
        }
        let kind = event["type"].as_str()?;
        let timestamp = *self
            .created
            .get_or_insert_with(|| chrono::Utc::now().timestamp_millis());
        let delta = match kind {
            "text-delta" => {
                let text = present(&event["text"], &event["delta"]);
                if !truthy(text) {
                    return None;
                }
                self.delta("content", text.clone())
            }
            "reasoning-delta" => {
                if !truthy(&event["text"]) {
                    return None;
                }
                self.delta("reasoning_content", event["text"].clone())
            }
            "tool-input-start" => {
                let id = present(&event["id"], &event["toolCallId"])
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_{}_{}", self.tool_indices.len(), timestamp));
                let next = self.tool_indices.len();
                let index = *self.tool_indices.entry(id.clone()).or_insert(next);
                self.tool(event, Some(id.as_str()), index, String::new())
            }
            "tool-input-delta" => {
                let id = present(&event["id"], &event["toolCallId"]).as_str()?;
                let index = self.tool_indices.get(id)?;
                let part = present(
                    &event["delta"],
                    present(&event["inputTextDelta"], &json!("")),
                )
                .clone();
                json!({"tool_calls":[{"index":index,"function":{"arguments":part}}]})
            }
            "tool-call" => {
                let id = event["toolCallId"].as_str();
                let index = self.tool_indices.len();
                let key = id.unwrap_or_default();
                if self.tool_indices.contains_key(key) {
                    return None;
                }
                self.tool_indices.insert(key.to_owned(), index);
                let input = &event["input"];
                let args = input
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| js_json(not_null(input, &json!({})), false))
                    .unwrap_or_else(|| "{}".to_owned());
                self.tool(event, id, index, args)
            }
            "finish-step" => {
                self.finish = Some(finish_reason(&event["finishReason"]));
                if truthy(&event["usage"]) {
                    self.usage = event["usage"].clone();
                }
                return None;
            }
            "finish" => {
                let reason = self
                    .finish
                    .clone()
                    .unwrap_or_else(|| finish_reason(&event["finishReason"]));
                let mut output = self.chunk(timestamp, json!({}), reason);
                if let Some(usage) = mapped_usage(present(&event["totalUsage"], &self.usage)) {
                    output["usage"] = usage;
                }
                return Some(output);
            }
            _ => return None,
        };
        Some(self.chunk(timestamp, delta, Value::Null))
    }

    fn delta(&mut self, key: &str, value: Value) -> Value {
        let mut delta = if self.began {
            json!({})
        } else {
            json!({"role":"assistant"})
        };
        self.began = true;
        delta[key] = value;
        delta
    }

    fn tool(&mut self, event: &Value, id: Option<&str>, index: usize, arguments: String) -> Value {
        let mut call = json!({"index":index});
        if let Some(id) = id {
            call["id"] = json!(id);
        }
        call["type"] = json!("function");
        call["function"] =
            json!({"name":present(&event["toolName"], &json!("")),"arguments":arguments});
        self.delta("tool_calls", json!([call]))
    }

    fn chunk(&self, timestamp: i64, delta: Value, finish: Value) -> Value {
        json!({
            "id":format!("chatcmpl-{timestamp}"),"object":"chat.completion.chunk",
            "created":timestamp.div_euclid(1000),"model":self.model,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}],
        })
    }
}

fn finish_reason(value: &Value) -> Value {
    match value.as_str() {
        Some("tool-calls" | "tool_use") => json!("tool_calls"),
        Some("content-filter") => json!("content_filter"),
        Some("error") => json!("stop"),
        _ if !truthy(value) => json!("stop"),
        _ => value.clone(),
    }
}

fn mapped_usage(raw: &Value) -> Option<Value> {
    if !raw.is_object() && !raw.is_array() {
        return None;
    }
    let numeric = |value: &Value| {
        if value.is_number() {
            value.clone()
        } else {
            json!(0)
        }
    };
    let prompt = numeric(&raw["inputTokens"]);
    let completion = numeric(&raw["outputTokens"]);
    let total = if raw["totalTokens"].is_number() {
        raw["totalTokens"].clone()
    } else {
        js_add(&prompt, &completion)
    };
    let mut usage =
        json!({"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":total});
    for (field, name) in [
        ("cacheReadTokens", "cached_tokens"),
        ("cacheWriteTokens", "cache_creation_tokens"),
    ] {
        let value = &raw["inputTokenDetails"][field];
        if value.as_f64().is_some_and(|value| value > 0.0) {
            if !usage["prompt_tokens_details"].is_object() {
                usage["prompt_tokens_details"] = json!({});
            }
            usage["prompt_tokens_details"][name] = value.clone();
        }
    }
    Some(usage)
}

fn status_code(value: &Value) -> Option<StatusCode> {
    let n = value
        .as_f64()
        .or_else(|| value.as_str()?.trim_matches(js_space).parse().ok())?;
    if n.fract() != 0.0 || !(400.0..=599.0).contains(&n) {
        return None;
    }
    StatusCode::from_u16(n as u16).ok()
}

fn parse_stream_error(event: &Value) -> (StatusCode, String) {
    let error = not_null(&event["error"], &event["message"]);
    let message = if error.is_null() {
        "unknown".to_owned()
    } else if error.is_object() {
        let value = present(&error["message"], &error["error"]);
        if truthy(value) {
            js_text(value)
        } else {
            error.to_string()
        }
    } else {
        error
            .as_str()
            .map_or_else(|| error.to_string(), str::to_owned)
    };
    let status = status_code(&event["statusCode"])
        .or_else(|| status_code(&error["statusCode"]))
        .or_else(|| status_code(&error["status"]))
        .unwrap_or_else(|| {
            let text = message.to_lowercase();
            const MESSAGES: &[(StatusCode, &[&str])] = &[
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    &["rate limit", "too many requests"],
                ),
                (
                    StatusCode::UNAUTHORIZED,
                    &["unauthorized", "invalid api key", "authentication"],
                ),
                (
                    StatusCode::PAYMENT_REQUIRED,
                    &["payment required", "billing"],
                ),
                (StatusCode::FORBIDDEN, &["quota", "forbidden", "permission"]),
                (StatusCode::NOT_FOUND, &["not found"]),
            ];
            MESSAGES
                .iter()
                .find(|(_, patterns)| patterns.iter().any(|pattern| text.contains(*pattern)))
                .map_or(StatusCode::SERVICE_UNAVAILABLE, |(status, _)| *status)
        });
    (status, message)
}

pub fn parse_failure(
    status: StatusCode,
    status_text: &str,
    raw: &str,
    account: &Value,
) -> (StatusCode, String) {
    let value = serde_json::from_str::<Value>(raw).unwrap_or_default();
    let error = present(&value["error"], &value);
    let code = status_code(present(&error["code"], &error["statusCode"])).unwrap_or(status);
    let message = present(&error["message"], &value["message"]);
    let message = if truthy(message) {
        js_text(message)
    } else if !raw.is_empty() {
        raw.to_owned()
    } else if !status_text.is_empty() {
        status_text.to_owned()
    } else {
        format!("CommandCode upstream error: {}", status.as_u16())
    };
    (code, redact(account, &message))
}

fn redact(account: &Value, message: &str) -> String {
    let mut text = message.to_owned();
    for field in ["apiKey", "accessToken"] {
        if let Some(secret) = account[field].as_str().filter(|secret| !secret.is_empty()) {
            text = text.replace(secret, "[redacted]");
        }
    }
    text
}
