use std::time::Duration;

use axum::http::{HeaderValue, StatusCode, header};
use base64::{Engine, engine::general_purpose::STANDARD};
use percent_encoding::percent_decode_str;
use reqwest::{Method, Response};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

use super::image_queue_egress::{Delivery, Endpoint};
use super::{IMAGE_BASE64, ImageError, QueuedImageError, default_error, proxy_required};
use crate::chat_prefetch::{fetch_image_bytes, mime};
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::proxy_pools::managed_relay::public_target;
use crate::storage::Storage;

const ORIGIN: &str = "https://api.dev.runwayml.com";
const FIXTURE_ORIGIN: &str = "http://127.0.0.1:52989/runway";
const VERSION: &str = "2024-11-06";
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
const DATA_URI_LIMIT: usize = 5 * 1024 * 1024;
const IMAGE_RATIOS: &[&str] = &[
    "1024:1024",
    "1080:1080",
    "1168:880",
    "1360:768",
    "1440:1080",
    "1080:1440",
    "1808:768",
    "1920:1080",
    "1080:1920",
    "2112:912",
    "1280:720",
    "720:1280",
    "720:720",
    "960:720",
    "720:960",
    "1680:720",
];
const VIDEO_RATIOS: &[&str] = &[
    "1280:720", "720:1280", "1104:832", "832:1104", "960:960", "1584:672",
];
pub(super) async fn generate(
    storage: &Storage,
    settings: &Value,
    model: &str,
    account: &Value,
    body: &Value,
) -> Result<Value, QueuedImageError> {
    use QueuedImageError::{BeforeSubmit, PossiblyAccepted, Protected};

    let before_submit = |failure| {
        if proxy_required(settings, account) {
            Protected(failure)
        } else {
            BeforeSubmit(failure)
        }
    };

    let secret = ["apiKey", "accessToken"]
        .into_iter()
        .find_map(|field| account[field].as_str().filter(|key| !key.is_empty()))
        .filter(|key| key.len() <= 512 && key.bytes().all(|byte| byte.is_ascii_graphic()))
        .ok_or_else(|| {
            before_submit(ImageError {
                status: StatusCode::UNAUTHORIZED,
                message: "Invalid Runway API key".into(),
            })
        })?;
    let mut key = HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| {
        before_submit(ImageError {
            status: StatusCode::UNAUTHORIZED,
            message: "Invalid Runway API key".into(),
        })
    })?;
    key.set_sensitive(true);
    let (path, payload) = prepare(model, body, secret).await.map_err(before_submit)?;
    let endpoint = Endpoint {
        name: "Runway",
        origin: ORIGIN,
        direct: if fixture() { FIXTURE_ORIGIN } else { ORIGIN },
    };
    let delivery = Delivery::select(storage, settings, account, endpoint, path).await?;
    let response = delivery
        .request(Method::POST, path, &key)
        .header(header::ACCEPT, "application/json")
        .header("x-runway-version", VERSION)
        .json(&payload)
        .send()
        .await
        .map_err(|_| PossiblyAccepted(ImageError::bad_gateway("[502]: fetch failed")))?;
    let status = response.status();
    if !status.is_success() {
        let (failure, reports_task) = submit_error(response, secret, &delivery).await;
        return Err(if delivery.protected {
            Protected(failure)
        } else if !reports_task
            && matches!(
                status.as_u16(),
                400 | 401 | 402 | 403 | 404 | 413 | 422 | 429
            )
        {
            BeforeSubmit(failure)
        } else {
            PossiblyAccepted(failure)
        });
    }
    let submitted = read_json(response).await.map_err(PossiblyAccepted)?;
    let id = submitted["id"]
        .as_str()
        .filter(|id| {
            id.len() == 36
                && uuid::Uuid::parse_str(id).is_ok()
                && (!delivery.protected || !delivery.contains_secret(id, secret))
        })
        .ok_or_else(|| {
            PossiblyAccepted(ImageError::bad_gateway(
                "Runway returned an invalid task ID",
            ))
        })?;
    let result = poll(&delivery, &key, secret, id)
        .await
        .map_err(PossiblyAccepted)?;
    let outputs = result["output"].as_array().ok_or_else(|| {
        PossiblyAccepted(ImageError::bad_gateway("Runway returned no media outputs"))
    })?;
    let mut data = Vec::with_capacity(outputs.len());
    for item in outputs {
        let Some(url) = item.as_str().filter(|value| {
            https_asset(value)
                && !delivery.contains_secret(&percent_decode_str(value).decode_utf8_lossy(), secret)
        }) else {
            return Err(PossiblyAccepted(ImageError::bad_gateway(
                "Runway returned an invalid media URL",
            )));
        };
        data.push(json!({"url":url}));
    }
    Ok(json!({"created":chrono::Utc::now().timestamp(),"data":data}))
}

async fn prepare(
    model: &str,
    body: &Value,
    secret: &str,
) -> Result<(&'static str, Value), ImageError> {
    let (path, require_image) = match model {
        "gen4_image" => ("/v1/text_to_image", false),
        "gen4_image_turbo" => ("/v1/text_to_image", true),
        "gen4_turbo" => ("/v1/image_to_video", true),
        "gen3a_turbo" => {
            return Err(invalid(
                "Runway retired gen3a_turbo. Select gen4_turbo with a source image.",
            ));
        }
        _ => return Err(invalid("Unsupported Runway model")),
    };
    let prompt = body["prompt"]
        .as_str()
        .filter(|prompt| !js_trim(prompt).is_empty() && prompt.encode_utf16().count() <= 1000)
        .ok_or_else(|| {
            invalid("Runway prompt must be a nonempty string of up to 1000 UTF-16 code units")
        })?;
    let video = path == "/v1/image_to_video";
    let mut payload = json!({"model":model,"promptText":prompt,"ratio":ratio(&body["size"],video)});
    let source = if truthy(&body["image"]) {
        let raw = body["image"]
            .as_str()
            .ok_or_else(|| invalid("Runway reference image must be a string"))?;
        let raw = js_trim(raw);
        if raw.starts_with("https://")
            && percent_decode_str(raw).decode_utf8_lossy().contains(secret)
        {
            return Err(invalid("Invalid Runway reference image URL"));
        }
        Some(
            reference_image(raw)
                .await
                .ok_or_else(|| invalid("Invalid or unavailable Runway reference image"))?,
        )
    } else if require_image {
        return Err(invalid("This Runway model requires a source image"));
    } else {
        None
    };
    if video {
        payload["promptImage"] = json!(source);
        payload["duration"] = json!(5);
    } else if let Some(image) = source {
        payload["referenceImages"] = json!([{"uri":image}]);
    }
    Ok((path, payload))
}

fn ratio(size: &Value, video: bool) -> String {
    let raw = size
        .as_str()
        .filter(|value| value.len() <= 20)
        .unwrap_or_default();
    let dimensions = raw.replace('x', ":");
    let supported = if video { VIDEO_RATIOS } else { IMAGE_RATIOS };
    if supported.contains(&dimensions.as_str()) {
        return dimensions;
    }
    match (video, raw) {
        (false, "1792x1024") => "1920:1080",
        (false, "1024x1792") => "1080:1920",
        (false, "1536x1024") => "1440:1080",
        (false, "1024x1536") => "1080:1440",
        (true, "1792x1024") => "1280:720",
        (true, "1024x1792") => "720:1280",
        (true, "1536x1024") => "1104:832",
        (true, "1024x1536") => "832:1104",
        (true, _) => "960:960",
        (false, _) => "1024:1024",
    }
    .to_owned()
}

async fn reference_image(value: &str) -> Option<String> {
    if let Some(opaque) = value.strip_prefix("runway://") {
        return (13..=5000)
            .contains(&value.len())
            .then_some(opaque)
            .filter(|opaque| {
                opaque
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'\\' | b'#'))
            })
            .map(|_| value.to_owned());
    }
    let bytes = if value.starts_with("https://") && value.len() <= 2048 && https_asset(value) {
        fetch_image_bytes(value).await?
    } else {
        if value.len() > DATA_URI_LIMIT {
            return None;
        }
        let (prefix, data) = value.split_once(',')?;
        if !matches!(
            prefix,
            "data:image/png;base64"
                | "data:image/jpeg;base64"
                | "data:image/jpg;base64"
                | "data:image/webp;base64"
        ) {
            return None;
        }
        IMAGE_BASE64.decode(data).ok()?
    };
    let content_type =
        mime(&bytes).filter(|mime| matches!(*mime, "image/png" | "image/jpeg" | "image/webp"))?;
    if base64::encoded_len(bytes.len(), true)? + content_type.len() + 13 > DATA_URI_LIMIT {
        return None;
    }
    Some(format!(
        "data:{content_type};base64,{}",
        STANDARD.encode(bytes)
    ))
}

fn https_asset(value: &str) -> bool {
    value.len() <= 8192 && Url::parse(value).is_ok_and(|url| public_target(&url))
}

fn fixture() -> bool {
    cfg!(debug_assertions) && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

async fn poll(
    delivery: &Delivery,
    key: &HeaderValue,
    secret: &str,
    id: &str,
) -> Result<Value, ImageError> {
    let path = format!("/v1/tasks/{id}");
    let (interval, maximum) = if fixture() {
        (Duration::from_millis(25), Duration::from_millis(500))
    } else {
        (Duration::from_millis(1500), Duration::from_secs(120))
    };
    timeout_at(Instant::now() + maximum, async {
        loop {
            sleep(interval).await;
            let response = delivery
                .request(Method::GET, &path, key)
                .header(header::ACCEPT, "application/json")
                .header("x-runway-version", VERSION)
                .send()
                .await
                .map_err(|_| ImageError::bad_gateway("Runway polling request failed"))?;
            let status = response.status();
            if status.is_redirection() {
                return Err(ImageError::bad_gateway("Runway task redirect refused"));
            }
            if !status.is_success() {
                return Err(ImageError::bad_gateway(format!(
                    "Runway status {}",
                    status.as_u16()
                )));
            }
            let value = read_json(response).await?;
            if value["id"] != id {
                return Err(ImageError::bad_gateway("Runway returned a different task"));
            }
            match value["status"].as_str() {
                Some("SUCCEEDED") => return Ok(value),
                Some("PENDING" | "THROTTLED" | "RUNNING") => {}
                Some("FAILED" | "CANCELED" | "CANCELLED") => {
                    let message = value["failure"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("Runway task failed");
                    return Err(ImageError::bad_gateway(redacted(delivery, message, secret)));
                }
                _ => {
                    return Err(ImageError::bad_gateway(
                        "Runway returned an unknown task status",
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| ImageError {
        status: StatusCode::GATEWAY_TIMEOUT,
        message: "Runway polling timeout".into(),
    })?
}

async fn response_bytes(mut response: Response, limit: usize) -> Result<Vec<u8>, ImageError> {
    let invalid = || ImageError::bad_gateway("Invalid Runway response");
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(invalid());
    }
    let mut contents = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| invalid())? {
        if chunk.len() > limit - contents.len() {
            return Err(invalid());
        }
        contents.extend_from_slice(&chunk);
    }
    Ok(contents)
}

async fn read_json(response: Response) -> Result<Value, ImageError> {
    serde_json::from_slice(&response_bytes(response, RESPONSE_LIMIT).await?)
        .map_err(|_| ImageError::bad_gateway("Invalid Runway response"))
}

async fn submit_error(response: Response, secret: &str, delivery: &Delivery) -> (ImageError, bool) {
    let status = response.status();
    if status.is_redirection() {
        return (
            ImageError::bad_gateway("Runway submit redirect refused"),
            false,
        );
    }
    let raw = response_bytes(response, 64 * 1024)
        .await
        .unwrap_or_default();
    let value: Value = serde_json::from_slice(&raw).unwrap_or_default();
    let message = [
        &value["error"]["message"],
        &value["message"],
        &value["error"],
    ]
    .into_iter()
    .find_map(|value| value.as_str().filter(|message| !message.is_empty()))
    .map(str::to_owned)
    .unwrap_or_else(|| default_error(status));
    (
        ImageError {
            status,
            message: format!(
                "[{}]: {}",
                status.as_u16(),
                redacted(delivery, &message, secret)
            ),
        },
        truthy(&value["id"]) || truthy(&value["taskId"]),
    )
}

fn redacted(delivery: &Delivery, message: &str, secret: &str) -> String {
    delivery
        .redact(&percent_decode_str(message).decode_utf8_lossy(), secret)
        .chars()
        .take(2000)
        .collect()
}

fn invalid(message: &str) -> ImageError {
    ImageError {
        status: StatusCode::BAD_REQUEST,
        message: message.to_owned(),
    }
}
