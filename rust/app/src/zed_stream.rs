use std::time::Duration;

use axum::body::Bytes;
use axum::http::header;
use futures_util::stream;
use reqwest::{Body, Response, ResponseBuilderExt};
use serde_json::{Value, json};

use super::{Provider, safe_text, sanitized, text};
use crate::anthropic_upstream::ClaudeToChat;
use crate::gemini_upstream::GeminiToChat;
use crate::provider_connections::truthy;
use crate::responses_to_chat::ResponsesToChat;
use crate::sse_reader::{ReadLine, SseReader};

const MAX_LINE: usize = 4 * 1024 * 1024;
const DONE: &str = "data: [DONE]\n\n";

enum Converter {
    Claude(ClaudeToChat),
    Google(GeminiToChat),
    OpenAi(ResponsesToChat),
    Chat,
}

impl Converter {
    fn new(provider: Provider, model: &str) -> Self {
        match provider {
            Provider::Claude => {
                let mut state = ClaudeToChat::new();
                state.set_model(model);
                Self::Claude(state)
            }
            Provider::Google => Self::Google(GeminiToChat::with_model(model)),
            Provider::OpenAi => Self::OpenAi(ResponsesToChat::with_model(model)),
            Provider::Xai => Self::Chat,
        }
    }

    fn push(&mut self, event: Value) -> Vec<Value> {
        match self {
            Self::Claude(state) => state.push(None, &event),
            Self::Google(state) => state.push(None, &event),
            Self::OpenAi(state) => state.push(None, &event),
            Self::Chat => event.is_object().then_some(event).into_iter().collect(),
        }
    }

    fn finish(&mut self) -> Vec<Value> {
        match self {
            Self::Claude(state) => state.finish(),
            Self::Google(state) => state.finish(),
            Self::OpenAi(state) => state.finish(),
            Self::Chat => Vec::new(),
        }
    }
}

struct Stream {
    reader: SseReader,
    converter: Converter,
    model: String,
    private: [String; 5],
    stall: Duration,
    done: bool,
}

pub(super) fn adapt(
    response: Response,
    provider: Provider,
    model: &str,
    private: [String; 5],
) -> Response {
    let origin = response.url().clone();
    let status = response.status();
    let reader = Stream {
        reader: SseReader::new(response).with_line_limit(MAX_LINE),
        converter: Converter::new(provider, model),
        model: model.to_owned(),
        private,
        stall: crate::chat_output::stall_timeout("zed"),
        done: false,
    };
    let stream = stream::unfold(reader, |mut state| async move {
        let bytes = state.next().await?;
        Some((Ok::<_, std::convert::Infallible>(bytes), state))
    });
    axum::http::Response::builder()
        .status(status)
        .url(origin)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::wrap_stream(stream))
        .expect("Zed event stream")
        .into()
}

impl Stream {
    async fn next(&mut self) -> Option<Bytes> {
        if self.done {
            return None;
        }
        loop {
            let line = match self.reader.next(self.stall, None).await {
                ReadLine::Line(line, _) => line,
                ReadLine::End => return Some(self.finish(None)),
                ReadLine::Timeout => return Some(self.finish(Some("Response stream timed out"))),
                ReadLine::Disconnected => {
                    return Some(self.finish(Some("Response stream disconnected")));
                }
            };
            let line = line.trim();
            let line = line.strip_prefix("data:").map_or(line, str::trim_start);
            if line == "[DONE]" {
                return Some(self.finish(None));
            }
            let Ok(mut event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(value) = event.get_mut("event") {
                event = value.take();
            } else if let Some(status) = event.get("status").filter(|status| truthy(status)) {
                if let Some(failed) = status
                    .get("failed")
                    .filter(|failure| truthy(failure))
                    .or_else(|| {
                        (status == "failed" || status["type"] == "failed").then_some(status)
                    })
                {
                    let message = text(&failed["message"])
                        .or_else(|| text(&failed["error"]))
                        .or_else(|| text(&failed["code"]))
                        .unwrap_or("request failed");
                    return Some(self.finish(Some(message)));
                }
                if status == "stream_ended"
                    || status["type"] == "stream_ended"
                    || status.get("stream_ended").is_some()
                {
                    return Some(self.finish(None));
                }
                continue;
            }
            let values = self.converter.push(event);
            if !values.is_empty() {
                return Some(Bytes::from(self.encode(values)));
            }
        }
    }

    fn finish(&mut self, error: Option<&str>) -> Bytes {
        self.done = true;
        let mut wire = String::new();
        if let Some(error) = error {
            let now = chrono::Utc::now();
            let secrets = self.private.each_ref().map(String::as_str);
            let message: String = safe_text(error, &secrets).chars().take(2000).collect();
            wire.push_str(&self.encode(vec![json!({
                "id":format!("chatcmpl-zed-error-{}",now.timestamp_millis()),
                "object":"chat.completion.chunk","created":now.timestamp(),"model":self.model,
                "choices":[{"index":0,"delta":{"content":format!("[Zed error] {message}")},"finish_reason":"stop"}],
            })]));
        }
        let final_chunks = self.converter.finish();
        wire.push_str(&self.encode(final_chunks));
        wire.push_str(DONE);
        Bytes::from(wire)
    }

    fn encode(&self, chunks: Vec<Value>) -> String {
        let private = self.private.each_ref().map(String::as_str);
        let mut wire = String::new();
        for chunk in chunks {
            wire.push_str("data: ");
            wire.push_str(&sanitized(&chunk, &private).to_string());
            wire.push_str("\n\n");
        }
        wire
    }
}
