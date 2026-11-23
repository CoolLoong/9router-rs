use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes, to_bytes};
use axum::http::header;
use axum::response::Response;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::chat_input::{js_space, truthy};
use crate::storage::Storage;

const JSON_LIMIT: usize = 128 * 1024 * 1024;
const REQUEST_FIELDS: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "max_tokens",
    "max_completion_tokens",
    "thinking",
    "reasoning",
    "enable_thinking",
    "presence_penalty",
    "frequency_penalty",
    "seed",
    "stop",
    "tools",
    "tool_choice",
    "response_format",
    "prediction",
    "store",
    "metadata",
    "n",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "user",
    "parallel_tool_calls",
];

#[derive(Clone, Copy)]
pub struct Config {
    max_records: u64,
    batch_size: usize,
    flush: Duration,
    max_json: usize,
}

pub fn configured(settings: &Value) -> Option<Config> {
    let enabled = std::env::var("ENABLE_REQUEST_LOGS")
        .ok()
        .map(|value| value.eq_ignore_ascii_case("true"))
        .or_else(|| settings["enableObservability"].as_bool())
        .unwrap_or(false);
    if !enabled {
        return None;
    }
    let read = |key: &str, env: &str, default: usize| {
        let value = &settings[key];
        value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .or_else(|| value.as_str().and_then(js_trim_number))
            .filter(|n| *n > 0)
            .or_else(|| {
                std::env::var(env)
                    .ok()
                    .and_then(|text| js_trim_number(&text))
                    .filter(|n| *n > 0)
            })
            .unwrap_or(default)
    };
    Some(Config {
        max_records: read("observabilityMaxRecords", "OBSERVABILITY_MAX_RECORDS", 200) as u64,
        batch_size: read("observabilityBatchSize", "OBSERVABILITY_BATCH_SIZE", 20),
        flush: Duration::from_millis(read(
            "observabilityFlushIntervalMs",
            "OBSERVABILITY_FLUSH_INTERVAL_MS",
            5000,
        ) as u64),
        max_json: read("observabilityMaxJsonSize", "OBSERVABILITY_MAX_JSON_SIZE", 5)
            .saturating_mul(1024),
    })
}

fn js_trim_number(value: &str) -> Option<usize> {
    let value = value
        .trim_start_matches(js_space)
        .strip_prefix('+')
        .unwrap_or(value.trim_start_matches(js_space));
    let end = value
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(value.len());
    value[..end].parse().ok()
}

#[derive(Clone)]
pub struct Recorder {
    queue: mpsc::UnboundedSender<Work>,
}

struct Record {
    config: Config,
    data: Value,
}

enum Work {
    Write(Record),
    Flush(oneshot::Sender<()>),
}

impl Recorder {
    pub fn new(storage: Storage) -> Self {
        let (queue, receiver) = mpsc::unbounded_channel();
        tokio::spawn(writer(storage, receiver));
        Self { queue }
    }

    pub async fn flush(&self) {
        let (send, receive) = oneshot::channel();
        if self.queue.send(Work::Flush(send)).is_ok() {
            let _ = receive.await;
        }
    }
}

async fn writer(storage: Storage, mut queue: mpsc::UnboundedReceiver<Work>) {
    let mut pending: Vec<Record> = Vec::new();
    let mut deadline = tokio::time::Instant::now();
    loop {
        let job = if pending.is_empty() {
            queue.recv().await
        } else {
            tokio::select! {
                job = queue.recv() => job,
                _ = tokio::time::sleep_until(deadline) => {
                    commit(&storage, std::mem::take(&mut pending)).await;
                    continue;
                }
            }
        };
        match job {
            Some(Work::Flush(reply)) => {
                commit(&storage, std::mem::take(&mut pending)).await;
                let _ = reply.send(());
            }
            Some(Work::Write(record)) => {
                let config = record.config;
                if pending.is_empty() {
                    deadline = tokio::time::Instant::now()
                        + config.flush.min(Duration::from_secs(u32::MAX.into()));
                }
                pending.push(record);
                if pending.len() >= config.batch_size {
                    commit(&storage, std::mem::take(&mut pending)).await;
                }
            }
            None => {
                commit(&storage, pending).await;
                return;
            }
        }
    }
}

async fn commit(storage: &Storage, batch: Vec<Record>) {
    let Some(limit) = batch.last().map(|record| record.config.max_records) else {
        return;
    };
    let result = storage
        .run(true, move |db| {
            let transaction =
                db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            for record in batch {
                let data = record.data;
                transaction.execute(
                "INSERT INTO requestDetails (id,timestamp,provider,model,connectionId,status,data)
                 VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET
                 timestamp=excluded.timestamp,provider=excluded.provider,model=excluded.model,
                 connectionId=excluded.connectionId,status=excluded.status,data=excluded.data",
                rusqlite::params![
                    data["id"].as_str(), data["timestamp"].as_str(), data["provider"].as_str(),
                    data["model"].as_str(), data["connectionId"].as_str(), data["status"].as_str(),
                    data.to_string(),
                ],
            )?;
            }
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            transaction.execute(
                "DELETE FROM requestDetails WHERE id IN (
                SELECT id FROM requestDetails ORDER BY timestamp DESC, rowid DESC LIMIT -1 OFFSET ?1
            )",
                [limit],
            )?;
            transaction.commit()
        })
        .await;
    if result.is_err() {
        eprintln!("Failed to save request details");
    }
}

#[derive(Clone)]
pub struct Trace(Arc<Call>);

struct Call {
    recorder: Recorder,
    config: Config,
    id: String,
    provider: String,
    model: String,
    connection: String,
    request: Value,
    upstream_request: Value,
    pxpipe: Option<Value>,
    started: Instant,
    usage: Mutex<Option<Value>>,
    upstream_response: Mutex<Option<Value>>,
}

pub struct TraceRequest<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub connection: &'a str,
    pub request: &'a Value,
    pub upstream: &'a Value,
    pub stream: bool,
    pub pxpipe: Option<Value>,
}

impl Trace {
    pub fn new(recorder: Recorder, config: Config, data: TraceRequest<'_>) -> Self {
        let TraceRequest {
            provider,
            model,
            connection,
            request,
            upstream,
            stream,
            pxpipe,
        } = data;
        let mut body = json!({
            "messages": if truthy(&request["messages"]) { request["messages"].clone() } else { json!([]) },
            "model": request["model"],
            "stream": stream,
        });
        for field in REQUEST_FIELDS {
            if let Some(value) = request.get(*field) {
                body[*field] = value.clone();
            }
        }
        let id = format!(
            "{}-{}",
            chrono::Utc::now().timestamp_millis(),
            uuid::Uuid::new_v4().simple()
        );
        Self(Arc::new(Call {
            recorder,
            config,
            id,
            provider: provider.into(),
            model: model.into(),
            connection: connection.into(),
            request: limited(body, config.max_json),
            upstream_request: limited(upstream.clone(), config.max_json),
            pxpipe,
            started: Instant::now(),
            usage: Mutex::new(None),
            upstream_response: Mutex::new(None),
        }))
    }

    pub fn usage(&self, data: &Value) {
        if let Ok(mut usage) = self.0.usage.lock() {
            *usage = Some(data.clone());
        }
    }

    pub fn raw_response(&self, data: &Value) {
        if let Ok(mut response) = self.0.upstream_response.lock() {
            *response = Some(limited(data.clone(), self.0.config.max_json));
        }
    }

    pub fn fail(&self, status: u16, error: &str) {
        let prefix = format!("[{status}]: ");
        let error = error.strip_prefix(&prefix).unwrap_or(error);
        self.record(
            "error",
            json!({ "error": error, "status": status, "thinking": null }),
            json!({}),
            Some(0),
        );
    }

    pub async fn observe(self, response: Response) -> Response {
        let stream = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|text| text.to_str().ok())
            .is_some_and(|type_| type_.contains("text/event-stream"));
        let (parts, payload) = response.into_parts();
        if stream {
            self.record(
                "success",
                json!({
                    "content": "[Streaming in progress...]", "thinking": null, "type": "streaming",
                }),
                json!("[Streaming - raw response not captured]"),
                Some(0),
            );
            let state = Sse {
                trace: self,
                wire: payload.into_data_stream(),
                pending: Vec::new(),
                content: String::new(),
                thinking: String::new(),
                ttft: None,
                finished: false,
            };
            return Response::from_parts(
                parts,
                Body::from_stream(stream::unfold(state, |mut state| async {
                    if state.finished {
                        return None;
                    }
                    if let Some(chunk) = state.wire.next().await {
                        if let Ok(bytes) = &chunk {
                            state.read(bytes);
                        }
                        return Some((chunk, state));
                    }
                    state.finish();
                    None
                })),
            );
        }
        let Ok(bytes) = to_bytes(payload, JSON_LIMIT).await else {
            return Response::from_parts(parts, Body::empty());
        };
        let result: Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let choice = &result["choices"][0];
        let content = choice["message"]
            .get("content")
            .filter(|value| truthy(value))
            .or_else(|| result.get("content").filter(|value| truthy(value)));
        let reasoning = choice["message"]
            .get("reasoning_content")
            .filter(|value| truthy(value))
            .or_else(|| {
                result
                    .get("reasoning_content")
                    .filter(|value| truthy(value))
            });
        let fields = json!({
            "content": content, "thinking": reasoning,
            "finish_reason": choice["finish_reason"].as_str().unwrap_or("unknown"),
        });
        let upstream = self
            .0
            .upstream_response
            .lock()
            .ok()
            .and_then(|mut entry| entry.take())
            .unwrap_or(result);
        self.record("success", fields, upstream, None);
        Response::from_parts(parts, Body::from(bytes))
    }

    fn record(&self, status: &str, response: Value, upstream_response: Value, ttft: Option<u128>) {
        let call = &self.0;
        let total = call.started.elapsed().as_millis();
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let tokens = call
            .usage
            .lock()
            .ok()
            .and_then(|tokens| tokens.clone())
            .unwrap_or_else(|| json!({"prompt_tokens":0,"completion_tokens":0}));
        let mut data = json!({
            "id": call.id, "provider": call.provider, "model": call.model, "connectionId": call.connection,
            "timestamp": timestamp, "status": status,
            "latency": { "ttft":ttft.unwrap_or(total), "total":total },
            "tokens": tokens, "request": call.request, "providerRequest": call.upstream_request,
            "providerResponse": limited(upstream_response, call.config.max_json),
            "response": limited(response, call.config.max_json),
        });
        if let Some(summary) = &call.pxpipe {
            data["pxpipe"] = summary.clone();
        }
        let _ = call.recorder.queue.send(Work::Write(Record {
            config: call.config,
            data,
        }));
    }
}

struct Sse {
    trace: Trace,
    wire: axum::body::BodyDataStream,
    pending: Vec<u8>,
    content: String,
    thinking: String,
    ttft: Option<u128>,
    finished: bool,
}

impl Sse {
    fn read(&mut self, bytes: &Bytes) {
        self.pending.extend_from_slice(bytes);
        let Some(end) = self.pending.iter().rposition(|&byte| byte == b'\n') else {
            return;
        };
        let tail = self.pending.split_off(end + 1);
        for line in std::mem::replace(&mut self.pending, tail).split(|&byte| byte == b'\n') {
            self.line(line);
        }
    }

    fn line(&mut self, line: &[u8]) {
        let Some(data) = std::str::from_utf8(line)
            .ok()
            .and_then(|line| line.trim_matches(js_space).strip_prefix("data:"))
        else {
            return;
        };
        let Ok(event) = serde_json::from_str::<Value>(data.trim_matches(js_space)) else {
            return;
        };
        let choice = &event["choices"][0]["delta"];
        let (content, thinking) = match event["type"].as_str() {
            Some("content_block_delta") => (&event["delta"]["text"], &event["delta"]["thinking"]),
            Some("response.output_text.delta") => (&event["delta"], &Value::Null),
            Some("response.reasoning_summary_text.delta") => (&Value::Null, &event["delta"]),
            _ => (&choice["content"], &choice["reasoning_content"]),
        };
        for (target, source) in [(&mut self.content, content), (&mut self.thinking, thinking)] {
            if let Some(text) = source.as_str().filter(|text| !text.is_empty()) {
                self.ttft
                    .get_or_insert_with(|| self.trace.0.started.elapsed().as_millis());
                target.push_str(text);
            }
        }
    }

    fn finish(&mut self) {
        self.finished = true;
        let pending = std::mem::take(&mut self.pending);
        self.line(&pending);
        let content = if self.content.is_empty() {
            "[Empty streaming response]"
        } else {
            &self.content
        };
        let thinking = (!self.thinking.is_empty()).then_some(self.thinking.as_str());
        self.trace.record(
            "success",
            json!({ "content":content, "thinking":thinking, "type":"streaming" }),
            json!(content),
            self.ttft,
        );
    }
}

fn limited(data: Value, limit: usize) -> Value {
    if !truthy(&data) {
        return json!({});
    }
    let raw = data.to_string();
    let length = raw.encode_utf16().count();
    if length > limit {
        let preview = String::from_utf16_lossy(&raw.encode_utf16().take(200).collect::<Vec<_>>());
        json!({ "_truncated":true, "_originalSize":length, "_preview":preview })
    } else {
        data
    }
}
