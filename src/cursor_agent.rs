use std::collections::{HashSet, VecDeque};
use std::convert::Infallible;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header};
use futures_util::stream;
use reqwest::{Response, ResponseBuilderExt, Version};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::request::array;
use super::{Failure, TIMEOUT, headers};
use crate::chat_input::{js_json, js_space};
use crate::chat_output::{estimated_usage, js_text};
use crate::cursor_identity::Account;
use crate::provider_http::{Delivery, http_client};
use crate::storage::Storage;

#[path = "cursor_agent_codec.rs"]
mod codec;

const ENDPOINT: &str = "https://agent.api5.cursor.sh/agent.v1.AgentService/Run";
const FIXTURE: &str = "http://127.0.0.1:52989/cursor/agent.v1.AgentService/Run";
const EMPTY: &str = "Cursor AgentService returned an empty turn";
const UNSUPPORTED: &str = "Cursor AgentService requested an unsupported IDE tool";
const ERROR_BODY_LIMIT: usize = 256 * 1024;

pub(super) async fn complete(
    storage: &Storage,
    settings: &Value,
    connection: &Value,
    account: Account<'_>,
    model: &str,
    body: &Value,
    stream: bool,
) -> Result<Response, Failure> {
    let delivery = Delivery::select(storage, settings, connection, ENDPOINT)
        .await
        .map_err(|_| Failure::unavailable())?;
    let endpoint = if delivery.use_fixture() {
        FIXTURE
    } else {
        ENDPOINT
    };
    let resolved = if matches!(model, "auto" | "default") {
        crate::cursor_models::models_routed(connection, false, &delivery)
            .await
            .and_then(|models| {
                let mut eligible = models.into_iter().filter(|model| {
                    model["supportsAgent"] == true
                        && model["isChatOnly"] != true
                        && model["isHidden"] != true
                });
                let first = eligible.next()?;
                let chosen = if first["defaultOn"] == true {
                    first
                } else {
                    eligible
                        .find(|row| row["defaultOn"] == true)
                        .unwrap_or(first)
                };
                chosen["id"].as_str().map(str::to_owned)
            })
            .unwrap_or_else(|| model.to_owned())
    } else {
        model.to_owned()
    };
    let remote = Remote {
        delivery,
        endpoint,
        headers: headers(&account)?,
        model: resolved,
        secrets: [account.token.to_owned(), account.machine.to_owned()],
    };
    let turn = remote.open(body).await?;
    let destination = turn.response.url().clone();
    let now = chrono::Utc::now();
    let output = Output {
        id: format!("chatcmpl-msg_{}", now.timestamp_millis()),
        created: now.timestamp(),
        model: model.to_owned(),
    };
    let suffix = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    let mut agent = Agent {
        remote,
        turn: Some(turn),
        declared: array(&body["tools"])
            .iter()
            .filter_map(codec::tool_name)
            .map(str::to_owned)
            .collect(),
        retry: Retry::new(body),
        composer: suffix == "composer" || suffix.starts_with("composer-"),
        thinking: String::new(),
        visible: 0,
        emitted: false,
        ready: VecDeque::new(),
    };
    if stream {
        let source = stream::unfold((agent, output), |(mut agent, output)| async move {
            let event = agent.next().await?;
            let data = match event {
                Item::Text(text) => encode(output.chunk(json!({"content":text}), None)),
                Item::Tool(value) => {
                    let mut value = value;
                    value["index"] = json!(0);
                    encode(output.chunk(json!({"tool_calls":[value]}), None))
                }
                Item::Finish(reason) => {
                    encode(output.chunk(json!({}), Some(reason))) + "data: [DONE]\n\n"
                }
                Item::Error(message) => {
                    encode(json!({"error":{"message":message,"type":"api_error"}}))
                        + "data: [DONE]\n\n"
                }
            };
            Some((Ok::<_, Infallible>(Bytes::from(data)), (agent, output)))
        });
        return axum::http::Response::builder()
            .url(destination)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(reqwest::Body::wrap_stream(source))
            .map(Into::into)
            .map_err(|_| Failure::unavailable());
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut reason = "stop";
    while let Some(item) = agent.next().await {
        match item {
            Item::Text(value) => text.push_str(&value),
            Item::Tool(value) => calls.push(value),
            Item::Finish(finish) => reason = finish,
            Item::Error(message) => return Err(Failure::new(StatusCode::BAD_REQUEST, message)),
        }
    }
    let prompt = js_json(body, false)
        .unwrap_or_else(|| body.to_string())
        .encode_utf16()
        .count()
        .div_ceil(4);
    let usage = estimated_usage(prompt, text.encode_utf16().count());
    let mut message = json!({"role":"assistant","content":(!text.is_empty()).then_some(text)});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    let body = json!({"id":output.id,"object":"chat.completion","created":output.created,"model":output.model,
        "choices":[{"index":0,"message":message,"finish_reason":reason}],"usage":usage});
    axum::http::Response::builder()
        .url(destination)
        .header(header::CONTENT_TYPE, "application/json")
        .body(reqwest::Body::from(body.to_string()))
        .map(Into::into)
        .map_err(|_| Failure::unavailable())
}

struct Remote {
    delivery: Delivery,
    endpoint: &'static str,
    headers: HeaderMap,
    model: String,
    secrets: [String; 2],
}

impl Remote {
    async fn open(&self, body: &Value) -> Result<Turn, Failure> {
        let frame = codec::run(body, &self.model).ok_or_else(|| {
            Failure::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Cursor AgentService request exceeds the Connect frame limit",
            )
        })?;
        let (writer, receiver) = mpsc::channel::<Bytes>(8);
        writer
            .try_send(Bytes::from(frame))
            .map_err(|_| Failure::unavailable())?;
        let outbound = stream::unfold(receiver, |mut receive| async move {
            receive
                .recv()
                .await
                .map(|frame| (Ok::<_, Infallible>(frame), receive))
        });
        let direct = http_client(TIMEOUT)
            .http2_prior_knowledge()
            .build()
            .map_err(|_| Failure::unavailable())?;
        let client = self
            .delivery
            .client(&direct, TIMEOUT)
            .ok_or_else(Failure::unavailable)?;
        let mut response = client
            .post(self.endpoint)
            .version(Version::HTTP_2)
            .headers(self.headers.clone())
            .body(reqwest::Body::wrap_stream(outbound))
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    Failure::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "Cursor AgentService request timed out",
                    )
                } else {
                    Failure::unavailable()
                }
            })?;
        if response.status().is_redirection() || response.version() != Version::HTTP_2 {
            return Err(Failure::new(
                StatusCode::BAD_GATEWAY,
                "Cursor AgentService rejected the HTTP/2 connection",
            ));
        }
        if response.status() != StatusCode::OK {
            drop(writer);
            let status = response.status();
            let mut body = Vec::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                let len = chunk.len().min(ERROR_BODY_LIMIT - body.len());
                body.extend_from_slice(&chunk[..len]);
                if body.len() == ERROR_BODY_LIMIT {
                    break;
                }
            }
            let detail = String::from_utf8_lossy(&body);
            let detail = if detail.is_empty() {
                "request failed"
            } else {
                &detail
            };
            return Err(Failure::new(
                status,
                format!(
                    "Cursor AgentService {}: {}",
                    status.as_u16(),
                    self.redact(detail)
                ),
            ));
        }
        Ok(Turn {
            response,
            writer,
            buffer: Vec::new(),
            cursor: 0,
            ready: VecDeque::new(),
        })
    }

    fn redact(&self, input: &str) -> String {
        self.secrets.iter().fold(input.to_owned(), |text, secret| {
            text.replace(secret, "[redacted]")
        })
    }
}

struct Turn {
    response: Response,
    writer: mpsc::Sender<Bytes>,
    buffer: Vec<u8>,
    cursor: usize,
    ready: VecDeque<codec::Event>,
}

impl Turn {
    async fn next(&mut self) -> Result<Option<codec::Event>, &'static str> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(Some(event));
            }
            if let Some((consumed, events)) = codec::take(&self.buffer[self.cursor..])? {
                self.cursor += consumed;
                self.ready.extend(events);
                continue;
            }
            if self.cursor != 0 {
                self.buffer.drain(..self.cursor);
                self.cursor = 0;
            }
            let Some(chunk) = self
                .response
                .chunk()
                .await
                .map_err(|_| "Cursor AgentService stream interrupted")?
            else {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                return Err("Cursor AgentService returned an incomplete frame");
            };
            self.buffer.extend_from_slice(&chunk);
        }
    }
}

struct Retry {
    body: Option<Value>,
    remaining: usize,
    delay: Duration,
}

impl Retry {
    fn new(body: &Value) -> Self {
        let config = |key, fallback| {
            std::env::var(key)
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(fallback)
        };
        let remaining = config("CURSOR_EMPTY_TURN_MAX_RETRIES", 1);
        let delay = Duration::from_millis(config("CURSOR_EMPTY_TURN_RETRY_DELAY_MS", 400) as u64);
        let threshold = match config("CURSOR_TAIL_ONLY_THRESHOLD", 80) {
            0 => 80,
            count => count,
        };
        let messages = array(&body["messages"]);
        let mut retry = Self {
            body: None,
            remaining,
            delay,
        };
        if messages.len() < threshold || remaining == 0 {
            return retry;
        }
        let mut tail: Vec<_> = messages
            .iter()
            .filter(|message| {
                message["role"] == "system" && !js_text(&message["content"]).contains("[9router]")
            })
            .cloned()
            .collect();
        let non_system: Vec<_> = messages
            .iter()
            .filter(|message| message["role"] != "system")
            .collect();
        let anchor = non_system
            .iter()
            .rposition(|message| matches!(message["role"].as_str(), Some("user" | "tool")))
            .unwrap_or_else(|| non_system.len().saturating_sub(1));
        let start = if anchor > 0 && non_system[anchor - 1]["role"] == "assistant" {
            anchor - 1
        } else {
            anchor
        };
        tail.push(json!({"role":"system","content":format!("[9router] Long session: {} messages trimmed to the latest turn. Answer the current request; re-read files if needed.",messages.len())}));
        tail.extend(non_system[start..].iter().map(|value| (*value).clone()));
        let mut body = body.clone();
        body["messages"] = json!(tail);
        retry.body = Some(body);
        retry
    }
}

struct Agent {
    remote: Remote,
    turn: Option<Turn>,
    declared: HashSet<String>,
    retry: Retry,
    composer: bool,
    thinking: String,
    visible: usize,
    emitted: bool,
    ready: VecDeque<Item>,
}

enum Item {
    Text(String),
    Tool(Value),
    Finish(&'static str),
    Error(String),
}

impl Agent {
    async fn next(&mut self) -> Option<Item> {
        loop {
            if let Some(item) = self.ready.pop_front() {
                return Some(item);
            }
            let turn = self.turn.as_mut()?;
            match turn.next().await {
                Ok(Some(codec::Event::Write(frame))) => {
                    if turn.writer.send(Bytes::from(frame)).await.is_err() {
                        self.turn = None;
                        return Some(Item::Error(
                            "Cursor AgentService stopped accepting replies".into(),
                        ));
                    }
                }
                Ok(Some(codec::Event::Tool {
                    id,
                    name,
                    arguments,
                })) => {
                    self.turn = None;
                    if !self.declared.contains(&name) {
                        return Some(Item::Error(UNSUPPORTED.into()));
                    }
                    self.ready.push_back(Item::Finish("tool_calls"));
                    return Some(Item::Tool(
                        json!({"id":self.remote.redact(&id),"type":"function","function":{
                            "name":name,"arguments":self.remote.redact(&arguments),
                        }}),
                    ));
                }
                Ok(Some(codec::Event::Text(text))) => {
                    self.emitted = true;
                    return Some(Item::Text(self.remote.redact(&text)));
                }
                Ok(Some(codec::Event::Thinking(text))) => {
                    self.thinking.push_str(&text);
                    if self.composer {
                        let chars: Vec<_> = visible(&self.thinking).encode_utf16().collect();
                        if chars.len() > self.visible {
                            let text = String::from_utf16_lossy(&chars[self.visible..]);
                            self.visible = chars.len();
                            self.emitted = true;
                            return Some(Item::Text(self.remote.redact(&text)));
                        }
                    }
                }
                Ok(Some(codec::Event::Finish)) | Ok(None) => {
                    self.turn = None;
                    if self.emitted {
                        return Some(Item::Finish("stop"));
                    }
                    let fallback = if self.composer {
                        visible(&self.thinking)
                    } else {
                        self.thinking.trim_matches(js_space)
                    };
                    if !fallback.is_empty() {
                        self.ready.push_back(Item::Finish("stop"));
                        return Some(Item::Text(self.remote.redact(fallback)));
                    }
                    if self.retry.remaining != 0
                        && let Some(body) = &self.retry.body
                    {
                        self.retry.remaining -= 1;
                        if let Ok(turn) = self.remote.open(body).await {
                            self.turn = Some(turn);
                            self.thinking.clear();
                            self.visible = 0;
                            tokio::time::sleep(self.retry.delay).await;
                            continue;
                        }
                    }
                    return Some(Item::Error(EMPTY.into()));
                }
                Ok(Some(codec::Event::Error(message))) => {
                    self.turn = None;
                    return Some(Item::Error(self.remote.redact(&message)));
                }
                Err(message) => {
                    self.turn = None;
                    return Some(Item::Error(message.into()));
                }
            }
        }
    }
}

fn visible(text: &str) -> &str {
    text.rsplit_once("</think>")
        .map_or("", |(_, text)| text.trim_start_matches(js_space))
}

struct Output {
    id: String,
    model: String,
    created: i64,
}

impl Output {
    fn chunk(&self, delta: Value, reason: Option<&str>) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,
               "choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    }
}

fn encode(value: Value) -> String {
    format!("data: {value}\n\n")
}
