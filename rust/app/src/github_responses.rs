use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::header;
use futures_util::stream;
use reqwest::{Body, Response, ResponseBuilderExt};
use serde_json::{Value, json};

use crate::chat_input::{js_json, js_space, truthy};
use crate::responses_request;
use crate::sse_reader::{ReadLine, SseReader};

const MAX_ROUTES: usize = 512;

#[derive(Clone, Default)]
pub struct Routes(Arc<Mutex<VecDeque<String>>>);

impl Routes {
    pub fn has(&self, model: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .any(|known| known == model)
    }

    pub fn remember(&self, model: &str) {
        let mut routes = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if routes.iter().any(|known| known == model) {
            return;
        }
        if routes.len() >= MAX_ROUTES {
            routes.pop_front();
        }
        routes.push_back(model.to_owned());
    }
}

pub fn url() -> &'static str {
    crate::github_executor::responses_url()
}

pub fn fixture_url() -> Option<String> {
    (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"))
        .then(|| "http://127.0.0.1:52989/github/responses".into())
}

pub fn headers(
    request: reqwest::RequestBuilder,
    account: &Value,
    streaming: bool,
) -> reqwest::RequestBuilder {
    crate::github_executor::headers(request, account, &Value::Null, streaming)
}

pub fn prepare(mut body: Value, model: &str) -> Option<Value> {
    let fields = body.as_object_mut()?;
    if fields.get("input").is_some_and(truthy) {
        fields.insert("model".into(), json!(model));
        fields.insert("stream".into(), json!(true));
        if !fields.contains_key("max_output_tokens")
            && let Some(limit) = fields
                .get("max_completion_tokens")
                .or_else(|| fields.get("max_tokens"))
                .cloned()
        {
            fields.insert("max_output_tokens".into(), limit);
        }
        fields.shift_remove("max_tokens");
        fields.shift_remove("max_completion_tokens");
        return Some(body);
    }
    let reasoning = fields
        .shift_remove("reasoning_effort")
        .map(|effort| json!({"effort":effort,"summary":"auto"}))
        .or_else(|| fields.shift_remove("reasoning"));
    let mut converted = responses_request::from_chat(body, model)?;
    if let Some(reasoning) = reasoning {
        converted["reasoning"] = reasoning;
    }
    Some(converted)
}

pub fn adapt(response: Response, model: &str, streaming: bool, stall: Duration) -> Response {
    let mut state = Chunks::new(model);
    adapt_events(response, streaming, stall, move |event| {
        state.convert(event).into_iter().collect()
    })
}

pub(super) fn adapt_events<F>(
    response: Response,
    streaming: bool,
    stall: Duration,
    transform: F,
) -> Response
where
    F: FnMut(&Value) -> Vec<Value> + Send + 'static,
{
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
    let decoder = Decoder {
        reader: SseReader::new(response),
        transform,
        streaming,
        stall,
        done: false,
    };
    let body = Body::wrap_stream(stream::unfold(decoder, |mut state| async move {
        state.next().await.map(|chunk| (chunk, state))
    }));
    let mut output = builder.body(body).expect("valid adapted GitHub response");
    *output.headers_mut() = headers;
    output.into()
}

struct Decoder<F> {
    reader: SseReader,
    transform: F,
    streaming: bool,
    stall: Duration,
    done: bool,
}

impl<F> Decoder<F>
where
    F: FnMut(&Value) -> Vec<Value>,
{
    async fn next(&mut self) -> Option<Result<Vec<u8>, io::Error>> {
        if self.done {
            return None;
        }
        loop {
            let (line, terminated) = match self.reader.next(self.stall, None).await {
                ReadLine::Line(line, terminated) => (line, terminated),
                ReadLine::End => return None,
                ReadLine::Timeout => {
                    self.done = true;
                    return Some(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "GitHub response timed out",
                    )));
                }
                ReadLine::Disconnected => {
                    self.done = true;
                    return Some(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "GitHub response disconnected",
                    )));
                }
            };
            let Some(data) = line
                .trim_matches(js_space)
                .strip_prefix("data:")
                .map(|data| data.trim_matches(js_space))
            else {
                continue;
            };
            if data == "[DONE]" {
                if terminated && self.streaming {
                    return Some(Ok(b"data: [DONE]\n\n".to_vec()));
                }
                continue;
            }
            if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                let converted = (self.transform)(&chunk);
                if !converted.is_empty() {
                    let mut output = String::new();
                    for chunk in converted {
                        output.push_str("data: ");
                        output.push_str(&chunk.to_string());
                        output.push_str("\n\n");
                    }
                    return Some(Ok(output.into_bytes()));
                }
            }
        }
    }
}

struct Chunks {
    id: String,
    created: i64,
    model: String,
    indexes: HashMap<String, usize>,
    args_sent: HashSet<usize>,
    next_index: usize,
    finished: bool,
}

impl Chunks {
    fn new(model: &str) -> Self {
        Self {
            id: String::new(),
            created: 0,
            model: model.to_owned(),
            indexes: HashMap::new(),
            args_sent: HashSet::new(),
            next_index: 0,
            finished: false,
        }
    }

    fn convert(&mut self, event: &Value) -> Option<Value> {
        let kind = event["type"]
            .as_str()
            .filter(|kind| !kind.is_empty())
            .or_else(|| event["event"].as_str());
        let data = event
            .get("data")
            .filter(|value| truthy(value))
            .unwrap_or(event);
        if self.id.is_empty() {
            let now = chrono::Utc::now();
            self.id = format!("chatcmpl-{}", now.timestamp_millis());
            self.created = now.timestamp();
        }
        match kind? {
            "response.output_text.delta" => self.delta(data, "content"),
            "response.reasoning_summary_text.delta" => self.delta(data, "reasoning_content"),
            "response.output_item.added" if is_tool(&data["item"]) => {
                let item = &data["item"];
                let call_id = text(&item["call_id"])
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_{}", chrono::Utc::now().timestamp_millis()));
                let key = text(&item["id"])
                    .or_else(|| text(&data["item_id"]))
                    .unwrap_or(&call_id);
                let index = if let Some(&existing) = self.indexes.get(key) {
                    existing
                } else {
                    let new = self.next_index;
                    self.next_index += 1;
                    self.indexes.insert(key.to_owned(), new);
                    new
                };
                Some(self.chunk(
                    json!({"tool_calls":[{"index":index,"id":call_id,"type":"function",
                    "function":{"name":text(&item["name"]).unwrap_or_default(),"arguments":""}}]}),
                    None,
                ))
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let delta = data.get("delta").filter(|value| truthy(value))?;
                let index = self.index(text(&data["item_id"]));
                self.args_sent.insert(index);
                Some(self.chunk(
                    json!({"tool_calls":[{"index":index,"function":{"arguments":delta}}]}),
                    None,
                ))
            }
            "response.output_item.done" if is_tool(&data["item"]) => {
                let item = &data["item"];
                let arguments = text(&item["arguments"])?;
                let index = self.index(text(&item["id"]).or_else(|| text(&data["item_id"])));
                self.args_sent.insert(index).then(|| {
                    self.chunk(
                        json!({"tool_calls":[{"index":index,"function":{"arguments":arguments}}]}),
                        None,
                    )
                })
            }
            "response.completed" | "response.done" if !self.finished => {
                self.finished = true;
                let reason = if self.next_index > 0 {
                    "tool_calls"
                } else {
                    "stop"
                };
                let mut result = self.chunk(json!({}), Some(reason));
                let raw = &data["response"]["usage"];
                if raw.is_object() || raw.is_array() {
                    let input = first([&raw["input_tokens"], &raw["prompt_tokens"]], json!(0));
                    let output =
                        first([&raw["output_tokens"], &raw["completion_tokens"]], json!(0));
                    let cached = first(
                        [
                            &raw["input_tokens_details"]["cached_tokens"],
                            &raw["cache_read_input_tokens"],
                        ],
                        json!(0),
                    );
                    let mut usage = json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":add(&input, &output)});
                    if cached.as_f64().is_some_and(|tokens| tokens > 0.0) {
                        usage["prompt_tokens_details"] = json!({"cached_tokens":cached});
                    }
                    result["usage"] = usage;
                }
                Some(result)
            }
            "error" | "response.failed" if !self.finished => {
                let error = [&data["error"], &data["response"]["error"]]
                    .into_iter()
                    .find(|value| truthy(value))?;
                self.finished = true;
                let message = text(&error["message"])
                    .map(str::to_owned)
                    .or_else(|| js_json(error, false))
                    .unwrap_or_else(|| error.to_string());
                Some(self.chunk(
                    json!({"content":format!("[Error] {message}")}),
                    Some("stop"),
                ))
            }
            _ => None,
        }
    }

    fn delta(&self, data: &Value, key: &str) -> Option<Value> {
        let delta = data.get("delta").filter(|value| truthy(value))?;
        Some(self.chunk(json!({key:delta}), None))
    }

    fn index(&self, key: Option<&str>) -> usize {
        key.and_then(|key| self.indexes.get(key).copied())
            .unwrap_or(self.next_index.saturating_sub(1))
    }

    fn chunk(&self, delta: Value, reason: Option<&str>) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,
            "choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    }
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn is_tool(item: &Value) -> bool {
    matches!(
        item["type"].as_str(),
        Some("function_call" | "custom_tool_call")
    )
}

fn first<'a>(values: impl IntoIterator<Item = &'a Value>, fallback: Value) -> Value {
    values
        .into_iter()
        .find(|value| truthy(value))
        .cloned()
        .unwrap_or(fallback)
}

fn add(left: &Value, right: &Value) -> Value {
    if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
        return json!(left.saturating_add(right));
    }
    if left.is_string() || right.is_string() {
        return json!(format!(
            "{}{}",
            left.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| left.to_string()),
            right
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| right.to_string())
        ));
    }
    json!(left.as_f64().unwrap_or(0.0) + right.as_f64().unwrap_or(0.0))
}
