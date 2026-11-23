use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::header;
use axum::response::Response;
use futures_util::stream;
use serde_json::{Value, json};

use super::{Effects, IMAGE_LIMIT, normalize_usage, redact, text};
use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::sse_reader::{ReadLine, SseReader};

const STALL: Duration = Duration::from_secs(120);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const TEXT_LIMIT: usize = 2048;

pub(super) struct Parsed {
    pub(super) image: Value,
    pub(super) usage: Option<Value>,
}

pub(super) struct StreamError {
    pub(super) message: String,
    code: Option<String>,
    kind: Option<String>,
    param: Option<String>,
}

impl StreamError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            kind: None,
            param: None,
        }
    }

    fn payload(self) -> Value {
        let mut result = json!({"message":self.message});
        for (key, value) in [
            ("code", self.code),
            ("type", self.kind),
            ("param", self.param),
        ] {
            if let Some(value) = value {
                result[key] = json!(value);
            }
        }
        result
    }
}

struct Event {
    stage: String,
    data: Value,
}

pub(super) struct Images {
    reader: SseReader,
    image: Option<String>,
    problem: Option<String>,
    assistant: String,
    usage: Option<Value>,
    received: usize,
    last_progress: Option<Instant>,
    secrets: Arc<[String]>,
}

impl Images {
    pub(super) fn new(response: reqwest::Response, secrets: Arc<[String]>) -> Self {
        Self {
            reader: SseReader::new(response).with_line_limit(IMAGE_LIMIT),
            image: None,
            problem: None,
            assistant: String::new(),
            usage: None,
            received: 0,
            last_progress: None,
            secrets,
        }
    }

    pub(super) async fn collect(mut self) -> Result<Parsed, StreamError> {
        while let Some(event) = self.next_event().await? {
            self.apply(event, false)?;
        }
        self.finish()
    }

    pub(super) fn reply(self, effects: Effects) -> Response {
        let body = stream::unfold(Some((self, effects)), |state| async move {
            let (mut reader, effects) = state?;
            loop {
                let event = match reader.next_event().await {
                    Ok(Some(event)) => event,
                    Ok(None) => {
                        let chunk = match reader.finish() {
                            Ok(result) => {
                                effects.success(result.usage).await;
                                frame("done", &result.image)
                            }
                            Err(error) => frame("error", &error.payload()),
                        };
                        return Some((Ok::<_, Infallible>(Bytes::from(chunk)), None));
                    }
                    Err(error) => {
                        return Some((Ok(Bytes::from(frame("error", &error.payload()))), None));
                    }
                };
                let mut encoded = String::new();
                let now = Instant::now();
                if reader
                    .last_progress
                    .is_none_or(|last| now.duration_since(last) > PROGRESS_INTERVAL)
                {
                    reader.last_progress = Some(now);
                    encoded.push_str(&frame(
                        "progress",
                        &json!({"stage":event.stage,"bytesReceived":reader.received}),
                    ));
                }
                match reader.apply(event, true) {
                    Ok(Some(partial)) => encoded.push_str(&frame("partial_image", &partial)),
                    Ok(None) => (),
                    Err(error) => {
                        encoded.push_str(&frame("error", &error.payload()));
                        return Some((Ok(Bytes::from(encoded)), None));
                    }
                }
                if !encoded.is_empty() {
                    return Some((Ok(Bytes::from(encoded)), Some((reader, effects))));
                }
            }
        });
        let mut response = response(Body::from_stream(body));
        response.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("keep-alive"),
        );
        response
            .headers_mut()
            .insert("x-accel-buffering", header::HeaderValue::from_static("no"));
        response
    }

    async fn next_event(&mut self) -> Result<Option<Event>, StreamError> {
        let mut stage = String::new();
        let mut data = String::new();
        loop {
            match self.reader.next(STALL, None).await {
                ReadLine::Line(line, terminated) => {
                    self.received = self
                        .received
                        .saturating_add(line.len() + usize::from(terminated));
                    let line = line.trim_end_matches('\r');
                    if line.is_empty() {
                        if !stage.is_empty() {
                            return Ok(Some(Event {
                                stage,
                                data: serde_json::from_str(&data).unwrap_or_default(),
                            }));
                        }
                        if data == "[DONE]" {
                            return Ok(None);
                        }
                        data.clear();
                        continue;
                    }
                    if let Some(value) = line.strip_prefix("event:") {
                        stage = value.trim().to_owned();
                    } else if let Some(value) = line.strip_prefix("data:") {
                        let value = value.trim();
                        if value.len() > IMAGE_LIMIT - data.len() {
                            return Err(StreamError::message("Codex image event was too large"));
                        }
                        data.push_str(value);
                    }
                }
                ReadLine::End => {
                    return Ok((!stage.is_empty()).then(|| Event {
                        stage,
                        data: serde_json::from_str(&data).unwrap_or_default(),
                    }));
                }
                ReadLine::Timeout => {
                    return Err(StreamError::message("Codex image response timed out"));
                }
                ReadLine::Disconnected => {
                    return Err(StreamError::message("Codex image response disconnected"));
                }
            }
        }
    }

    fn apply(&mut self, event: Event, streaming: bool) -> Result<Option<Value>, StreamError> {
        let data = &event.data;
        match event.stage.as_str() {
            "error" | "response.failed" if !data.is_null() => {
                let error = [&data["error"], &data["response"]["error"], data]
                    .into_iter()
                    .find(|value| truthy(value))
                    .unwrap_or(data);
                let message = text(&error["message"])
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Codex upstream failed ({})", event.stage));
                let safe = |field| text(&error[field]).map(|value| redact(value, &self.secrets));
                return Err(StreamError {
                    message: redact(&message, &self.secrets),
                    code: safe("code"),
                    kind: safe("type"),
                    param: safe("param"),
                });
            }
            "response.image_generation_call.partial_image"
                if streaming && truthy(&data["partial_image_b64"]) =>
            {
                let mut part = json!({"b64_json":data["partial_image_b64"]});
                if let Some(index) = data.get("partial_image_index") {
                    part["index"] = index.clone();
                }
                return Ok(Some(part));
            }
            "response.output_text.delta" => {
                if let Some(delta) = text(&data["delta"]) {
                    let remaining = TEXT_LIMIT.saturating_sub(self.assistant.chars().count());
                    self.assistant.extend(delta.chars().take(remaining));
                }
            }
            "response.output_item.done" => {
                let item = &data["item"];
                if item["type"] == "image_generation_call" {
                    if let Some(image) = text(&item["result"]) {
                        self.image = Some(image.to_owned());
                    } else if let Some(error) = stream_error(item) {
                        self.problem = Some(redact(&error, &self.secrets));
                    }
                } else if item["type"] == "message" {
                    let full = item["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|part| {
                            text(part)
                                .or_else(|| text(&part["text"]))
                                .or_else(|| text(&part["output_text"]))
                        })
                        .take(64)
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !full.is_empty() {
                        self.assistant = full.chars().take(TEXT_LIMIT).collect();
                    }
                }
            }
            "response.completed" | "response.done" => {
                if let Some(usage) = normalize_usage(if data["response"]["usage"].is_null() {
                    &data["usage"]
                } else {
                    &data["response"]["usage"]
                }) {
                    self.usage = Some(usage);
                }
                let output = data["response"]["output"]
                    .as_array()
                    .or_else(|| data["output"].as_array());
                if let Some(image) = output
                    .into_iter()
                    .flatten()
                    .rev()
                    .filter(|item| item["type"] == "image_generation_call")
                    .find_map(|item| text(&item["result"]))
                {
                    self.image = Some(image.to_owned());
                }
            }
            _ => (),
        }
        Ok(None)
    }

    fn finish(self) -> Result<Parsed, StreamError> {
        if let Some(image) = self.image {
            return Ok(Parsed {
                image: json!({"created":chrono::Utc::now().timestamp(),"data":[{"b64_json":image}]}),
                usage: self.usage,
            });
        }
        let reason = self
            .problem
            .or_else(|| {
                let assistant = self.assistant.trim();
                (!assistant.is_empty()).then(|| {
                    format!(
                        "Codex returned text instead of an image: \"{}\"",
                        assistant.chars().take(300).collect::<String>()
                    )
                })
            })
            .unwrap_or_else(|| {
                "Codex completed without an image. No upstream image result was provided.".into()
            });
        Err(StreamError::message(redact(&reason, &self.secrets)))
    }
}

fn stream_error(value: &Value) -> Option<String> {
    if let Some(message) = text(value) {
        return Some(message.to_owned());
    }
    [
        &value["error"]["message"],
        &value["response"]["error"]["message"],
        &value["item"]["error"]["message"],
        &value["response"]["status_details"]["error"]["message"],
    ]
    .into_iter()
    .find(|value| truthy(value))
    .map(js_text)
    .or_else(|| {
        text(&value["error"])
            .or_else(|| text(&value["message"]))
            .map(str::to_owned)
    })
}

fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn response(body: Body) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(body)
        .expect("Codex image event stream")
}

pub(super) fn native(image: Value) -> Response {
    response(Body::from(frame("done", &image)))
}
