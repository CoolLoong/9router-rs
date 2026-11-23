use std::time::Duration;

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use reqwest::Response;
use serde_json::{Value, json};
use url::Url;

use crate::chat_output::js_text;
use crate::grok_web::{Failure, get_asset, post_conversation};
use crate::provider_connections::truthy;
use crate::storage::Storage;

const ASSETS: &str = "https://assets.grok.com/";
const STREAM_LIMIT: usize = 12 * 1024 * 1024;
const IMAGE_LIMIT: usize = 16 * 1024 * 1024;
const VIDEO_LIMIT: usize = 96 * 1024 * 1024;

pub(crate) async fn image(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    body: &Value,
) -> Result<Value, Failure> {
    let prompt = prompt(body)?;
    let message = if truthy(&body["size"]) {
        format!("Generate an image ({}): {prompt}", js_text(&body["size"]))
    } else {
        format!("Generate an image: {prompt}")
    };
    let payload = json!({
        "temporary":true,"modelName":"grok-4-1-thinking-1129","modelMode":"MODEL_MODE_FAST",
        "message":message,"fileAttachments":[],"imageAttachments":[],"disableSearch":false,
        "enableImageGeneration":true,"returnImageBytes":false,"returnRawGrokInXaiRequest":false,
        "enableImageStreaming":true,"imageGenerationCount":1,"forceConcise":false,
        "toolOverrides":{},"enableSideBySide":true,"sendFinalMetadata":true,"isReasoning":false,
        "disableTextFollowUps":false,"disableMemory":true,"forceSideBySide":false,
        "isAsyncChat":false,"disableSelfHarmShortCircuit":false,
    });
    let count = number(&body["n"]).unwrap_or(1.0).clamp(1.0, 4.0).ceil() as usize;
    let mut data = Vec::with_capacity(count);
    for _ in 0..count {
        let response = post_conversation(
            storage,
            settings,
            account,
            &payload,
            Duration::from_secs(170),
        )
        .await?;
        let frames = bounded(response, STREAM_LIMIT).await?;
        let target = image_url(&frames)
            .ok_or_else(|| upstream("Grok Web returned no completed image asset"))?;
        let response =
            get_asset(storage, settings, account, &target, Duration::from_secs(60)).await?;
        let bytes = bounded(response, IMAGE_LIMIT).await?;
        let mime = crate::chat_prefetch::mime(&bytes)
            .ok_or_else(|| upstream("Grok Web returned invalid image bytes"))?;
        data.push(json!({"b64_json":STANDARD.encode(bytes),"mime_type":mime}));
    }
    Ok(json!({"created":Utc::now().timestamp(),"data":data}))
}

pub(crate) async fn video(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
) -> Result<Value, Failure> {
    let prompt = prompt(body)?;
    if ["image", "image_url", "images", "image_urls"]
        .into_iter()
        .any(|key| {
            body[key].as_str().is_some_and(|value| !value.is_empty())
                || body[key].as_array().is_some_and(|images| {
                    images
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|value| !value.is_empty())
                })
        })
    {
        return Err(Failure {
            status: StatusCode::BAD_REQUEST,
            message: "Grok Web Imagine supports text-to-video only".into(),
        });
    }
    let ratio = body["size"]
        .as_str()
        .filter(|ratio| {
            ratio.split_once(':').is_some_and(|(left, right)| {
                [left, right].into_iter().all(|side| {
                    (1..=2).contains(&side.len()) && side.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
        })
        .unwrap_or("16:9");
    let duration = body
        .get("seconds")
        .filter(|value| !value.is_null())
        .unwrap_or(&body["duration"]);
    let duration = number(duration)
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(6.0);
    let payload = json!({
        "modelName":"imagine-video-gen","message":format!("{prompt} --mode=custom"),
        "enableImageStreaming":true,"enableSideBySide":true,"sendFinalMetadata":true,
        "responseMetadata":{"experiments":[],"modelConfigOverride":{"modelMap":{}}},
        "mediaGenInput":{"textToVideo":{
            "prompt":prompt,"aspectRatio":ratio,"duration":duration,"resolutionName":"480p",
        }},
        "kind":"CONVERSATION_KIND_IMAGINE",
    });
    let response = post_conversation(
        storage,
        settings,
        account,
        &payload,
        Duration::from_secs(300),
    )
    .await?;
    let frames = bounded(response, STREAM_LIMIT).await?;
    let target =
        video_url(&frames).ok_or_else(|| upstream("Grok Web returned no completed video asset"))?;
    let response = get_asset(
        storage,
        settings,
        account,
        &target,
        Duration::from_secs(120),
    )
    .await?;
    let bytes = bounded(response, VIDEO_LIMIT).await?;
    if bytes.get(4..8) != Some(b"ftyp") {
        return Err(upstream("Grok Web returned invalid MP4 bytes"));
    }
    let time = Utc::now();
    let model = if model.is_empty() { "grok-web" } else { model };
    Ok(json!({
        "id":format!("video-grok-web-{}",time.timestamp_millis()),"object":"video.generation",
        "created":time.timestamp(),"model":model,"status":"completed",
        "video":{"url":format!("data:video/mp4;base64,{}",STANDARD.encode(bytes))},
    }))
}

fn prompt(body: &Value) -> Result<&str, Failure> {
    body["prompt"]
        .as_str()
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or_else(|| Failure {
            status: StatusCode::BAD_REQUEST,
            message: "Grok Web media generation requires a prompt".into(),
        })
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| match value {
            Value::Bool(value) => Some(f64::from(u8::from(*value))),
            Value::String(value) => value.trim().parse().ok(),
            _ => None,
        })
        .filter(|value| !value.is_nan())
}

fn asset(value: &str) -> Option<Url> {
    let base = Url::parse(ASSETS).ok()?;
    let target = base.join(value.trim()).ok()?;
    (target.scheme() == "https"
        && target.origin() == base.origin()
        && target.username().is_empty()
        && target.password().is_none()
        && target.path() != "/"
        && target.fragment().is_none())
    .then_some(target)
}

fn frames(bytes: &[u8]) -> impl DoubleEndedIterator<Item = Value> + '_ {
    bytes.split(|byte| *byte == b'\n').filter_map(|line| {
        let line = std::str::from_utf8(line).ok()?.trim();
        let event = line.strip_prefix("data:").map_or(line, str::trim);
        serde_json::from_str(event).ok()
    })
}

fn image_url(bytes: &[u8]) -> Option<Url> {
    frames(bytes).rev().find_map(|frame| {
        let card = frame["result"]["response"]["cardAttachment"]["jsonData"].as_str()?;
        let data: Value = serde_json::from_str(card).ok()?;
        let chunk = &data["image_chunk"];
        if number(&chunk["progress"])? < 100.0 {
            return None;
        }
        asset(chunk["imageUrl"].as_str()?)
    })
}

fn video_url(bytes: &[u8]) -> Option<Url> {
    frames(bytes).rev().find_map(|frame| mp4_asset(&frame, 0))
}

fn mp4_asset(value: &Value, depth: usize) -> Option<Url> {
    if depth >= 24 {
        return None;
    }
    match value {
        Value::Array(items) => items
            .iter()
            .rev()
            .find_map(|item| mp4_asset(item, depth + 1)),
        Value::Object(items) => items
            .values()
            .rev()
            .find_map(|item| mp4_asset(item, depth + 1)),
        Value::String(text) => {
            let text = text.trim();
            if text.starts_with(['{', '[']) {
                let embedded = serde_json::from_str(text).ok()?;
                return mp4_asset(&embedded, depth + 1);
            }
            if text.bytes().any(|byte| byte.is_ascii_whitespace()) {
                return None;
            }
            let target = asset(text)?;
            target
                .path()
                .to_ascii_lowercase()
                .ends_with(".mp4")
                .then_some(target)
        }
        _ => None,
    }
}

async fn bounded(mut response: Response, limit: usize) -> Result<Vec<u8>, Failure> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(upstream("Grok Web media response exceeds the byte limit"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| upstream("Failed to read Grok Web media response"))?
    {
        if chunk.len() > limit - bytes.len() {
            return Err(upstream("Grok Web media response exceeds the byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn upstream(message: &str) -> Failure {
    Failure {
        status: StatusCode::BAD_GATEWAY,
        message: message.into(),
    }
}
