use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::header;
use base64::Engine;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use futures_util::{Stream, StreamExt, stream};
use reqwest::{Body, Response, ResponseBuilderExt};
use serde_json::{Value, json};

use crate::chat_format::ChatFormat;
use crate::chat_input::truthy;

const MAX_FRAME: usize = 24 * 1024 * 1024;
const MAX_HEADERS: usize = 128 * 1024;
const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

type Input = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

pub(super) fn adapt(response: Response, wire: ChatFormat, secrets: Arc<[String]>) -> Response {
    let status = response.status();
    let url = response.url().clone();
    let decoder = Decoder {
        upstream: Box::pin(response.bytes_stream()),
        wire,
        secrets,
        buffer: Vec::new(),
        offset: 0,
        finished: false,
        saw_terminal: false,
    };
    let frames = stream::unfold(decoder, |mut state| async move {
        state
            .next()
            .await
            .map(|chunk| (Ok::<_, std::convert::Infallible>(chunk), state))
    });
    axum::http::Response::builder()
        .status(status)
        .url(url)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::wrap_stream(frames))
        .expect("Bedrock SSE response")
        .into()
}

struct Decoder {
    upstream: Input,
    wire: ChatFormat,
    secrets: Arc<[String]>,
    buffer: Vec<u8>,
    offset: usize,
    finished: bool,
    saw_terminal: bool,
}

impl Decoder {
    async fn next(&mut self) -> Option<Bytes> {
        if self.finished {
            return None;
        }
        loop {
            let pending = &self.buffer[self.offset..];
            if pending.len() >= 12 {
                let length = match frame_length(pending) {
                    Ok(length) => length,
                    Err(fault) => return Some(self.fail(fault)),
                };
                if pending.len() >= length {
                    let event = decode(&pending[..length]);
                    self.offset += length;
                    match event {
                        Ok(None) => continue,
                        Ok(Some(value)) => return Some(self.emit(value)),
                        Err(fault) => return Some(self.fail(fault)),
                    }
                }
            }
            if self.offset > 0 {
                self.buffer.drain(..self.offset);
                self.offset = 0;
            }
            match self.upstream.next().await {
                Some(Ok(bytes)) => self.buffer.extend_from_slice(&bytes),
                Some(Err(_)) => {
                    return Some(self.fail(Fault::protocol("Bedrock stream read failed")));
                }
                None => return self.end(),
            }
        }
    }

    fn emit(&mut self, value: Value) -> Bytes {
        if self.wire == ChatFormat::Messages {
            let kind = value["type"].as_str().filter(|kind| !kind.is_empty());
            let Some(kind) = kind.filter(|kind| !kind.contains(['\r', '\n'])) else {
                return self.fail(Fault::protocol(
                    "Bedrock chunk decoded to an event with no type",
                ));
            };
            self.saw_terminal |= kind == "message_stop";
            frame(Some(kind), value.to_string())
        } else {
            let Some(choices) = value["choices"].as_array() else {
                return self.fail(Fault::protocol(
                    "Bedrock chunk decoded without a `choices` array",
                ));
            };
            self.saw_terminal |= choices
                .iter()
                .any(|choice| truthy(&choice["finish_reason"]));
            frame(None, value.to_string())
        }
    }

    fn fail(&mut self, fault: Fault) -> Bytes {
        self.finished = true;
        let safe = super::redact_values(&fault.message, self.secrets.iter().map(String::as_str));
        let kind = super::redact_values(&fault.kind, self.secrets.iter().map(String::as_str));
        if self.wire == ChatFormat::Messages {
            frame(
                Some("error"),
                json!({"type":"error","error":{"type":kind,"message":safe}}).to_string(),
            )
        } else {
            frame(
                None,
                json!({"error":{"type":kind,"message":safe,"code":kind}}).to_string(),
            )
        }
    }

    fn end(&mut self) -> Option<Bytes> {
        self.finished = true;
        if !self.buffer.is_empty() {
            return Some(self.fail(Fault::protocol("Bedrock stream ended mid-frame")));
        }
        if !self.saw_terminal {
            let message = if self.wire == ChatFormat::Messages {
                "Bedrock stream ended before message_stop"
            } else {
                "Bedrock stream ended before any choice reported a finish_reason"
            };
            return Some(self.fail(Fault::protocol(message)));
        }
        (self.wire == ChatFormat::Chat).then_some(Bytes::from_static(b"data: [DONE]\n\n"))
    }
}

fn frame(kind: Option<&str>, data: String) -> Bytes {
    Bytes::from(match kind {
        Some(kind) => format!("event: {kind}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    })
}

pub(crate) struct Fault {
    kind: String,
    message: String,
}

impl Fault {
    fn protocol(message: impl Into<String>) -> Self {
        Self {
            kind: "api_error".into(),
            message: message.into(),
        }
    }
}

pub(crate) fn frame_length(data: &[u8]) -> Result<usize, Fault> {
    if data.len() < 12 {
        return Err(Fault::protocol("Bedrock EventStream prelude is incomplete"));
    }
    if read_u32(&data[8..12]) != crc32(&data[..8]) {
        return Err(Fault::protocol("Bedrock EventStream prelude CRC mismatch"));
    }
    let length = read_u32(&data[..4]) as usize;
    let headers = read_u32(&data[4..8]) as usize;
    if !(16..=MAX_FRAME).contains(&length) || headers > MAX_HEADERS || headers > length - 16 {
        return Err(Fault::protocol(
            "Bedrock EventStream frame bounds are invalid",
        ));
    }
    Ok(length)
}

fn decode(frame: &[u8]) -> Result<Option<Value>, Fault> {
    let Frame { headers, payload } = decode_frame(frame)?;
    let message_type = headers
        .get(":message-type")
        .map(String::as_str)
        .unwrap_or_default();
    if matches!(message_type, "exception" | "error") {
        let kind = headers
            .get(":exception-type")
            .or_else(|| headers.get(":error-code"))
            .filter(|kind| !kind.is_empty())
            .cloned()
            .unwrap_or_else(|| "api_error".into());
        let message = [
            payload["message"].as_str(),
            payload["Message"].as_str(),
            headers.get(":error-message").map(String::as_str),
        ]
        .into_iter()
        .flatten()
        .find(|message| !message.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Bedrock returned an EventStream {message_type}"));
        return Err(Fault { kind, message });
    }
    if headers.get(":event-type").map(String::as_str) != Some("chunk") {
        return Ok(None);
    }
    let encoded = payload["bytes"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Fault::protocol("Bedrock chunk frame carried no payload bytes"))?;
    let bytes = B64
        .decode(encoded)
        .map_err(|_| Fault::protocol("Bedrock chunk was not valid base64"))?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let inner = serde_json::from_str(text)
        .map_err(|_| Fault::protocol("Bedrock chunk was not valid JSON"))?;
    Ok(Some(inner))
}

pub(crate) struct Frame {
    pub(crate) headers: HashMap<String, String>,
    pub(crate) payload: Value,
}

pub(crate) fn decode_frame(frame: &[u8]) -> Result<Frame, Fault> {
    if frame.len() < 16 {
        return Err(Fault::protocol("AWS EventStream frame is too short"));
    }
    let trailer = frame.len() - 4;
    if read_u32(&frame[trailer..]) != crc32(&frame[..trailer]) {
        return Err(Fault::protocol("AWS EventStream message CRC mismatch"));
    }
    let header_end = 12 + read_u32(&frame[4..8]) as usize;
    if header_end > trailer {
        return Err(Fault::protocol("AWS EventStream frame bounds are invalid"));
    }
    let headers = read_headers(&frame[12..header_end])?;
    let raw = String::from_utf8_lossy(&frame[header_end..trailer]);
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let payload: Value = if raw.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(raw)
            .map_err(|_| Fault::protocol("AWS EventStream payload is not valid JSON"))?
    };
    Ok(Frame { headers, payload })
}

fn read_headers(mut data: &[u8]) -> Result<HashMap<String, String>, Fault> {
    let mut headers = HashMap::new();
    let mut seen = HashSet::new();
    while !data.is_empty() {
        let name_length = take(&mut data, 1)?[0] as usize;
        let name = String::from_utf8_lossy(take(&mut data, name_length)?).into_owned();
        if !seen.insert(name.clone()) {
            return Err(Fault::protocol(format!(
                "AWS EventStream contains duplicate header: {name}"
            )));
        }
        let kind = take(&mut data, 1)?[0];
        let length = match kind {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            6 | 7 => {
                u16::from_be_bytes(take(&mut data, 2)?.try_into().expect("header length")) as usize
            }
            9 => 16,
            unknown => {
                return Err(Fault::protocol(format!(
                    "AWS EventStream header {name} has unknown type {unknown}"
                )));
            }
        };
        let recorded = take(&mut data, length)?;
        if kind == 7 {
            headers.insert(name, String::from_utf8_lossy(recorded).into_owned());
        }
    }
    Ok(headers)
}

fn take<'a>(data: &mut &'a [u8], length: usize) -> Result<&'a [u8], Fault> {
    let Some((head, tail)) = data.split_at_checked(length) else {
        return Err(Fault::protocol(
            "AWS EventStream header exceeds its declared bounds",
        ));
    };
    *data = tail;
    Ok(head)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum()
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(
        bytes
            .try_into()
            .expect("complete EventStream prelude field"),
    )
}
