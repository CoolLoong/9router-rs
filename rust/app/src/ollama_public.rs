use std::collections::HashMap;

use axum::body::{Body, BodyDataStream, HttpBody};
use axum::http::header;
use axum::response::Response;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::provider_connections::truthy;

pub const PATHS: &[&str] = &["/api/v1/api/chat", "/v1/api/chat", "/v1/v1/api/chat"];
const VARY: &str = "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding";

pub fn requested_model(body: &Value) -> Value {
    let model = &body["model"];
    if truthy(model) {
        model.clone()
    } else {
        json!("llama3.2")
    }
}

pub async fn options() -> Response {
    Response::builder()
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "*")
        .header(header::VARY, VARY)
        .body(Body::empty())
        .expect("Ollama preflight response")
}

pub fn transform(response: Response, model: Value) -> Response {
    let mut reply = Response::builder()
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(header::VARY, VARY);
    let body = if response.body().is_end_stream() {
        reply = reply.status(response.status());
        Body::empty()
    } else {
        reply = reply.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
        let converter = Converter {
            model,
            input: response.into_body().into_data_stream(),
            pending: Vec::new(),
            tools: HashMap::new(),
            finished: false,
        };
        Body::from_stream(stream::unfold(converter, |mut state| async move {
            state.next().await.map(|frame| (frame, state))
        }))
    };
    reply.body(body).expect("Ollama NDJSON response")
}

struct Converter {
    model: Value,
    input: BodyDataStream,
    pending: Vec<u8>,
    tools: HashMap<String, PendingTool>,
    finished: bool,
}

struct PendingTool {
    order: usize,
    name: String,
    arguments: String,
}

impl Converter {
    async fn next(&mut self) -> Option<Result<Vec<u8>, axum::Error>> {
        if self.finished {
            return None;
        }
        loop {
            let bytes = match self.input.next().await {
                Some(Ok(bytes)) => bytes,
                Some(Err(error)) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                None => {
                    self.finished = true;
                    let mut out = Vec::new();
                    self.terminal(&mut out);
                    return Some(Ok(out));
                }
            };
            let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
                self.pending.extend_from_slice(&bytes);
                continue;
            };
            self.pending.extend_from_slice(&bytes[..last_newline]);
            let lines = std::mem::replace(&mut self.pending, bytes[last_newline + 1..].to_vec());
            let mut out = Vec::new();
            for line in lines.split(|byte| *byte == b'\n') {
                let Some(raw) = line.strip_prefix(b"data:") else {
                    continue;
                };
                let raw = String::from_utf8_lossy(raw);
                let data = crate::provider_index::js_trim(&raw);
                if data == "[DONE]" {
                    self.terminal(&mut out);
                    break;
                }
                if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                    self.chunk(&chunk, &mut out);
                }
            }
            if !out.is_empty() {
                return Some(Ok(out));
            }
        }
    }

    fn chunk(&mut self, chunk: &Value, out: &mut Vec<u8>) {
        let choice = &chunk["choices"][0];
        let delta = &choice["delta"];
        let calls = &delta["tool_calls"];
        if truthy(calls) {
            let Some(calls) = calls.as_array() else {
                return;
            };
            for call in calls {
                if call.is_null() {
                    return;
                }
                let index = call
                    .get("index")
                    .map_or_else(|| "undefined".into(), js_text);
                let order = self.tools.len();
                let row = self.tools.entry(index).or_insert_with(|| PendingTool {
                    order,
                    name: String::new(),
                    arguments: String::new(),
                });
                let function = &call["function"];
                if truthy(&function["name"]) {
                    row.name.push_str(&js_text(&function["name"]));
                }
                if truthy(&function["arguments"]) {
                    row.arguments.push_str(&js_text(&function["arguments"]));
                }
            }
        }

        if truthy(&delta["content"]) {
            self.frame(
                out,
                json!({"role":"assistant","content":delta["content"]}),
                false,
            );
        }

        let finish = choice["finish_reason"].as_str();
        if !matches!(finish, Some("stop" | "tool_calls")) {
            return;
        }
        if self.tools.is_empty() {
            if finish == Some("stop") {
                self.terminal(out);
            }
            return;
        }

        let mut tools = std::mem::take(&mut self.tools)
            .into_iter()
            .collect::<Vec<_>>();
        tools.sort_by_key(|(key, call)| (numeric_key(key), call.order));
        let calls = tools
            .into_iter()
            .map(|(_, call)| {
                let arguments =
                    serde_json::from_str::<Value>(&call.arguments).unwrap_or_else(|_| json!({}));
                json!({"function":{"name":call.name,"arguments":arguments}})
            })
            .collect::<Vec<_>>();
        self.frame(
            out,
            json!({"role":"assistant","content":"","tool_calls":calls}),
            true,
        );
    }

    fn terminal(&self, out: &mut Vec<u8>) {
        self.frame(out, json!({"role":"assistant","content":""}), true);
    }

    fn frame(&self, out: &mut Vec<u8>, message: Value, done: bool) {
        let row = json!({"model":self.model,"message":message,"done":done});
        serde_json::to_writer(&mut *out, &row).expect("Ollama NDJSON serialization");
        out.push(b'\n');
    }
}

fn numeric_key(key: &str) -> u32 {
    if key.starts_with('0') && key.len() > 1 || !key.bytes().all(|byte| byte.is_ascii_digit()) {
        return u32::MAX;
    }
    key.parse().unwrap_or(u32::MAX)
}
