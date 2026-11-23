use std::collections::VecDeque;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::anthropic_upstream::ClaudeToChat;
use crate::chat_format::ChatFormat;
use crate::claude_sse::ClaudeSse;
use crate::gemini_signatures::SignatureContext;
use crate::gemini_upstream::{self, GeminiToChat};
use crate::responses_sse::ResponsesSse;
use crate::responses_to_chat::ResponsesToChat;
use crate::sse_reader::{ReadLine, SseReader};

use super::{
    DONE, MESSAGES_USAGE_FIELDS, ResponseWire, USAGE_FIELDS, UsageCallback, error_payload,
    estimated_usage, extract_stream_usage, has_usage, js_len, js_trim, merge_usage,
    normalize_numbers, pad_usage, stall_timeout, valuable_chunk,
};

const TRAILER_DEADLINE: Duration = Duration::from_secs(3);

enum Decoder {
    Chat,
    Claude(ClaudeToChat),
    Responses(ResponsesToChat),
    Gemini(GeminiToChat, SignatureContext),
}

enum Translator {
    Chat,
    Claude(ClaudeSse),
    Responses(ResponsesSse),
}

pub(super) struct TranslatedStream {
    input: SseReader,
    decoder: Decoder,
    event: Option<String>,
    translator: Translator,
    output: VecDeque<Bytes>,
    cutoff: Option<Instant>,
    stall: Duration,
    terminal: bool,
    prompt_tokens: usize,
    content_chars: usize,
    usage: Option<Value>,
    on_usage: Option<UsageCallback>,
}

impl TranslatedStream {
    pub(super) fn new(
        upstream: reqwest::Response,
        mut body: Value,
        provider: &str,
        model: &str,
        wire: ResponseWire,
        format: ChatFormat,
        on_usage: Option<UsageCallback>,
    ) -> Self {
        normalize_numbers(&mut body);
        let decoder = match wire {
            ResponseWire::Chat => Decoder::Chat,
            ResponseWire::Messages => Decoder::Claude(ClaudeToChat::new()),
            ResponseWire::Responses => Decoder::Responses(ResponsesToChat::with_model(model)),
            ResponseWire::Gemini(context) => {
                Decoder::Gemini(GeminiToChat::with_model(model), context)
            }
        };
        let translator = match format {
            ChatFormat::Messages => Translator::Claude(ClaudeSse::new()),
            ChatFormat::Responses => {
                let names = crate::chat_protocol_input::responses_custom_tool_names(&body);
                let responses = ResponsesSse::with_custom_tools(names.map(str::to_owned));
                Translator::Responses(if matches!(decoder, Decoder::Chat) {
                    responses
                } else {
                    responses.without_usage_wait()
                })
            }
            ChatFormat::Chat => Translator::Chat,
        };
        Self {
            input: SseReader::new(upstream),
            decoder,
            event: None,
            translator,
            output: VecDeque::new(),
            cutoff: None,
            stall: stall_timeout(provider),
            terminal: false,
            prompt_tokens: js_len(&body.to_string()).div_ceil(4),
            content_chars: 0,
            usage: None,
            on_usage,
        }
    }

    pub(super) async fn next(&mut self) -> Option<Bytes> {
        loop {
            if let Some(frame) = self.output.pop_front() {
                return Some(frame);
            }
            if self.terminal {
                return None;
            }

            let remaining = self
                .cutoff
                .map(|end| end.saturating_duration_since(Instant::now()));
            if remaining == Some(Duration::ZERO) {
                self.finish();
                continue;
            }
            match self.input.next(self.stall, self.cutoff).await {
                ReadLine::Line(line, terminated) => self.push(&line, terminated),
                ReadLine::End => self.finish(),
                ReadLine::Timeout if self.cutoff.is_some_and(|end| end <= Instant::now()) => {
                    self.finish()
                }
                ReadLine::Timeout => self.abort("stream stall timeout"),
                ReadLine::Disconnected => self.abort("upstream connection lost"),
            }
        }
    }

    fn push(&mut self, line: &str, terminated: bool) {
        let line = js_trim(line);
        if let Some(event) = line.strip_prefix("event:").map(js_trim) {
            self.event = Some(event.to_owned());
            return;
        }
        let Some(data) = line.strip_prefix("data:").map(js_trim) else {
            return;
        };
        if data == "[DONE]" {
            if matches!(self.decoder, Decoder::Chat)
                && let Translator::Responses(state) = &self.translator
                && (state.completion_pending() || state.is_completed())
            {
                self.finish();
            }
            return;
        }
        let event = self.event.take();
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if chunk.is_null() {
            return;
        }
        if terminated {
            let delta = match self.decoder {
                Decoder::Chat => &chunk["choices"][0]["delta"],
                _ => &chunk["delta"],
            };
            let fields = match self.decoder {
                Decoder::Chat => ["content", "reasoning_content"],
                _ => ["text", "thinking"],
            };
            for key in fields {
                if let Some(text) = delta[key].as_str() {
                    self.content_chars += js_len(text);
                }
            }
            if matches!(self.decoder, Decoder::Gemini(..))
                && let Some(parts) = chunk["candidates"][0]["content"]["parts"].as_array()
            {
                for part in parts {
                    if let Some(text) = part["text"].as_str() {
                        self.content_chars += js_len(text);
                    }
                }
            }
        }
        let pad =
            terminated && !matches!(chunk["type"].as_str(), Some("error" | "response.failed"));
        let frames = match &mut self.decoder {
            Decoder::Chat => {
                if let Some(usage) = extract_stream_usage(&chunk) {
                    merge_usage(&mut self.usage, usage);
                }
                vec![chunk]
            }
            Decoder::Claude(input) => {
                let frames = input.push(event.as_deref(), &chunk);
                if let Some(usage) = input.usage() {
                    self.usage = Some(usage.clone());
                }
                frames
            }
            Decoder::Responses(input) => {
                let frames = input.push(event.as_deref(), &chunk);
                if let Some(usage) = input.usage() {
                    self.usage = Some(usage.clone());
                }
                frames
            }
            Decoder::Gemini(input, context) => {
                if let Some(usage) = gemini_upstream::raw_usage(&chunk)
                    && let Value::Object(usage) = usage
                {
                    merge_usage(&mut self.usage, usage);
                }
                let frames = input.push(event.as_deref(), &chunk);
                if let Some(usage) = input.usage()
                    && !frames.is_empty()
                {
                    self.usage = Some(usage.clone());
                }
                context.store(input.take_thought_signatures());
                frames
            }
        };
        for frame in frames {
            self.emit(frame, pad);
        }
    }

    fn emit(&mut self, mut chunk: Value, terminated: bool) {
        match &mut self.translator {
            Translator::Chat => {
                if !valuable_chunk(&chunk) {
                    return;
                }
                if terminated
                    && chunk["choices"][0]["finish_reason"]
                        .as_str()
                        .is_some_and(|reason| !reason.is_empty())
                {
                    if !has_usage(chunk.get("usage")) && self.content_chars > 0 {
                        let usage = estimated_usage(self.prompt_tokens, self.content_chars);
                        chunk["usage"] = usage.clone();
                        self.usage = Some(usage);
                    } else if let Some(usage) = &self.usage {
                        chunk["usage"] = pad_usage(usage, USAGE_FIELDS);
                    }
                }
                normalize_numbers(&mut chunk);
                self.output
                    .push_back(Bytes::from(format!("data: {chunk}\n\n")));
            }
            Translator::Claude(claude) => {
                claude.set_usage(self.usage.clone());
                let events = claude.push(&chunk);
                self.usage = claude.usage().cloned();
                for mut event in events {
                    if terminated && event["type"] == "message_delta" && claude.finished() {
                        if !has_usage(event.get("usage")) && self.content_chars > 0 {
                            let usage =
                                estimated_client_usage(self.prompt_tokens, self.content_chars);
                            event["usage"] = usage.clone();
                            self.usage = Some(usage);
                        } else if let Some(usage) = &self.usage {
                            event["usage"] = pad_usage(usage, MESSAGES_USAGE_FIELDS);
                        }
                    }
                    self.output.push_back(frame(event));
                }
            }
            Translator::Responses(responses) => {
                self.output
                    .extend(responses.push(&chunk).into_iter().map(Bytes::from));
                if responses.completion_pending() {
                    self.cutoff
                        .get_or_insert_with(|| Instant::now() + TRAILER_DEADLINE);
                } else {
                    self.cutoff = None;
                }
            }
        }
    }

    fn finish(&mut self) {
        if self.terminal {
            return;
        }
        let remaining = match &mut self.decoder {
            Decoder::Chat => Vec::new(),
            Decoder::Claude(state) => state.finish(),
            Decoder::Responses(state) => state.finish(),
            Decoder::Gemini(state, _) => state.finish(),
        };
        for frame in remaining {
            self.emit(frame, false);
        }
        match &mut self.translator {
            Translator::Chat => (),
            Translator::Claude(state) => self.output.extend(state.finish().into_iter().map(frame)),
            Translator::Responses(state) => self
                .output
                .extend(state.finish().into_iter().map(Bytes::from)),
        }
        self.terminal = true;
        if !has_usage(self.usage.as_ref()) && self.content_chars > 0 {
            self.usage = Some(match self.translator {
                Translator::Chat => estimated_usage(self.prompt_tokens, self.content_chars),
                _ => estimated_client_usage(self.prompt_tokens, self.content_chars),
            });
        }
        if let (Some(callback), Some(usage)) = (self.on_usage.take(), self.usage.take()) {
            callback(usage);
        }
    }

    fn abort(&mut self, reason: &str) {
        self.terminal = true;
        self.on_usage = None;
        let error = error_payload(StatusCode::GATEWAY_TIMEOUT, reason);
        let event = match self.translator {
            Translator::Claude(_) => frame(json!({ "type": "error", "error": error["error"] })),
            Translator::Chat | Translator::Responses(_) => {
                Bytes::from(format!("data: {error}\n\n{DONE}"))
            }
        };
        self.output.push_back(event);
    }
}

fn estimated_client_usage(prompt_tokens: usize, chars: usize) -> Value {
    let usage = estimated_usage(prompt_tokens, chars);
    json!({ "input_tokens": usage["prompt_tokens"], "output_tokens": usage["completion_tokens"], "estimated": true })
}

fn frame(mut event: Value) -> Bytes {
    normalize_numbers(&mut event);
    let kind = event["type"].as_str().unwrap_or_default();
    Bytes::from(format!("event: {kind}\ndata: {event}\n\n"))
}
