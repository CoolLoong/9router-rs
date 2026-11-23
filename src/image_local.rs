use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt;
use serde_json::{Value, json};
use url::Url;

use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

const MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy)]
pub enum Adapter {
    SdWebui,
    ComfyUi,
}

impl Adapter {
    pub fn resolve(provider: &str) -> Option<Self> {
        match provider {
            "sdwebui" => Some(Self::SdWebui),
            "comfyui" => Some(Self::ComfyUi),
            _ => None,
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::SdWebui => "http://localhost:7860/sdapi/v1/txt2img",
            Self::ComfyUi => "http://localhost:8188/",
        }
    }

    fn path(self, upstream: &Url) -> String {
        if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() != Ok("1") {
            return upstream.to_string();
        }
        let provider = match self {
            Self::SdWebui => "sdwebui",
            Self::ComfyUi => "comfyui",
        };
        let mut url = format!("http://127.0.0.1:52989/{provider}{}", upstream.path());
        if let Some(query) = upstream.query() {
            url.push('?');
            url.push_str(query);
        }
        url
    }
}

pub struct Failure {
    pub status: StatusCode,
    pub message: String,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }
}

pub async fn generate(
    client: &reqwest::Client,
    adapter: Adapter,
    body: &Value,
    binary: bool,
) -> Result<Value, Failure> {
    let target = Url::parse(adapter.endpoint()).expect("pinned local image endpoint");
    let payload = match adapter {
        Adapter::SdWebui => sd_payload(body)?,
        Adapter::ComfyUi => json!({"prompt":body["prompt"]}),
    };
    let response = client
        .post(adapter.path(&target))
        .json(&payload)
        .send()
        .await
        .map_err(|_| Failure::gateway("[502]: fetch failed"))?;
    let status = response.status();
    if !status.is_success() {
        let raw = response.text().await.unwrap_or_default();
        let message = upstream_error(status, &raw);
        return Err(Failure::new(
            status,
            format!("[{}]: {message}", status.as_u16()),
        ));
    }
    let raw = response
        .bytes()
        .await
        .map_err(|_| Failure::gateway("terminated"))?;
    let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw);
    let data: Value = serde_json::from_slice(raw).map_err(|error| {
        Failure::gateway(if error.is_eof() {
            "Unexpected end of JSON input"
        } else {
            "Invalid image provider JSON"
        })
    })?;
    let mut value = match adapter {
        Adapter::SdWebui => {
            let images: Vec<_> = data["images"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|image| json!({"b64_json":image}))
                .collect();
            json!({"created":chrono::Utc::now().timestamp(),"data":images})
        }
        Adapter::ComfyUi => data,
    };
    if binary
        && !truthy(&value["data"][0]["b64_json"])
        && let Some(url) = value["data"][0]["url"]
            .as_str()
            .filter(|value| !value.is_empty())
        && let Some(encoded) = artifact(client, adapter, &target, url).await
    {
        value["data"][0]["b64_json"] = json!(encoded);
    }
    Ok(value)
}

fn sd_payload(body: &Value) -> Result<Value, Failure> {
    let size = match body.get("size") {
        None => "1024x1024",
        Some(Value::String(size)) => size,
        Some(Value::Null) => {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "Cannot read properties of null (reading 'split')",
            ));
        }
        _ => {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "size.split is not a function",
            ));
        }
    };
    let mut dimensions = size.split('x');
    let width = dimension(dimensions.next());
    let height = dimension(dimensions.next());
    let batch = body.get("n").cloned().unwrap_or_else(|| json!(1));
    Ok(json!({"prompt":body["prompt"],"width":width,"height":height,"steps":20,"batch_size":batch}))
}

fn dimension(text: Option<&str>) -> Value {
    let number = match text.map(js_trim) {
        Some("Infinity" | "+Infinity" | "-Infinity") => return Value::Null,
        Some(text) => {
            let radix = match text.get(..2).map(str::to_ascii_lowercase).as_deref() {
                Some("0x") => Some(16),
                Some("0o") => Some(8),
                Some("0b") => Some(2),
                _ => None,
            };
            if let Some(radix) = radix {
                u64::from_str_radix(&text[2..], radix)
                    .ok()
                    .map(|value| value as f64)
            } else if text.bytes().any(|byte| byte.is_ascii_digit()) {
                text.parse::<f64>().ok()
            } else {
                None
            }
        }
        None => None,
    };
    let value = number
        .filter(|value| *value != 0.0 && !value.is_nan())
        .unwrap_or(512.0);
    if value.fract() == 0.0 && value.abs() < i64::MAX as f64 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

async fn artifact(
    client: &reqwest::Client,
    adapter: Adapter,
    endpoint: &Url,
    raw: &str,
) -> Option<String> {
    let image = Url::parse(raw).ok()?;
    if image.origin() != endpoint.origin()
        || !image.username().is_empty()
        || image.password().is_some()
    {
        return None;
    }
    let response = client
        .get(adapter.path(&image))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_IMAGE_BYTES as u64)
    {
        return None;
    }
    let mut input = response.bytes_stream();
    let mut image = Vec::new();
    while let Some(chunk) = input.next().await {
        let chunk = chunk.ok()?;
        if image.len().saturating_add(chunk.len()) > MAX_IMAGE_BYTES {
            return None;
        }
        image.extend_from_slice(&chunk);
    }
    (!image.is_empty()).then(|| STANDARD.encode(image))
}

fn upstream_error(status: StatusCode, raw: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(raw)
        && let Some(message) = [
            &value["error"]["message"],
            &value["message"],
            &value["error"],
        ]
        .into_iter()
        .find(|message| truthy(message))
    {
        return message
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| message.to_string());
    }
    if !raw.is_empty() {
        return raw.to_owned();
    }
    match status.as_u16() {
        400 => "Bad request",
        401 => "Invalid API key provided",
        402 => "Payment required",
        403 => "You exceeded your current quota",
        404 => "Model not found",
        406 => "Model not supported",
        429 => "Rate limit exceeded",
        500 => "Internal server error",
        502 => "Bad gateway - upstream provider error",
        503 => "Service temporarily unavailable",
        504 => "Gateway timeout",
        code => return format!("Upstream error: {code}"),
    }
    .into()
}
