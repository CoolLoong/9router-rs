use std::time::Duration;

use axum::http::{StatusCode, header::HeaderValue};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use reqwest::{Method, Response, Url, header};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use super::image_queue_egress::{Delivery, Endpoint};
use super::{ImageError, QueuedImageError, default_error, fixture, image_key, proxy_required};
use crate::provider_connections::truthy;
use crate::proxy_pools::managed_relay::public_target;
use crate::storage::Storage;
use QueuedImageError::{BeforeSubmit, PossiblyAccepted, Protected};

const ORIGIN: &str = "https://api.nanobananaapi.ai";
const FIXTURE: &str = "http://127.0.0.1:52989/nanobanana-image";
const SUBMIT: &str = "/api/v1/nanobanana/generate";
const POLL: &str = "/api/v1/nanobanana/record-info";
const RESPONSE_LIMIT: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(1500);
const POLL_TIMEOUT: Duration = Duration::from_secs(120);
const CALLBACK_ENV: &str = "NINEROUTER_NANOBANANA_CALLBACK_URL";

pub(super) async fn generate(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    body: &Value,
) -> Result<Value, QueuedImageError> {
    let callback = std::env::var(CALLBACK_ENV)
        .ok()
        .and_then(|value| Url::parse(value.trim()).ok())
        .filter(public_target)
        .ok_or_else(|| {
            Protected(ImageError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("Set {CALLBACK_ENV} to an authorized public HTTPS callback"),
            })
        })?;
    let credentials = || {
        let failure = ImageError {
            status: StatusCode::UNAUTHORIZED,
            message: "Invalid NanoBanana API key".into(),
        };
        if proxy_required(settings, account) {
            Protected(failure)
        } else {
            BeforeSubmit(failure)
        }
    };
    let secret = image_key(account).ok_or_else(credentials)?;
    let mut key = HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| credentials())?;
    key.set_sensitive(true);
    let endpoint = Endpoint {
        name: "NanoBanana",
        origin: ORIGIN,
        direct: if fixture() { FIXTURE } else { ORIGIN },
    };
    let delivery = Delivery::select(storage, settings, account, endpoint, SUBMIT).await?;
    let payload = payload(body, &callback);
    let response = delivery
        .request(Method::POST, SUBMIT, &key)
        .json(&payload)
        .send()
        .await
        .map_err(|_| PossiblyAccepted(ImageError::bad_gateway("NanoBanana submit failed")))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(PossiblyAccepted(ImageError::bad_gateway(
            "NanoBanana submit redirect refused",
        )));
    }
    let data = read_json(response).await;
    if !status.is_success() {
        let detail = data
            .as_ref()
            .ok()
            .and_then(|value| upstream_message(value, &["msg", "message"]));
        let message = detail.map_or_else(
            || default_error(status),
            |message| sanitized(&delivery, &callback, secret, message),
        );
        let failure = ImageError {
            status,
            message: format!("[{}]: {message}", status.as_u16()),
        };
        return Err(if delivery.protected {
            Protected(failure)
        } else if status.is_client_error()
            && status != StatusCode::REQUEST_TIMEOUT
            && data
                .as_ref()
                .ok()
                .is_none_or(|data| !truthy(&data["data"]["taskId"]))
        {
            BeforeSubmit(failure)
        } else {
            PossiblyAccepted(failure)
        });
    }
    let data = data.map_err(PossiblyAccepted)?;
    if data["code"].as_f64() != Some(200.0) {
        let message = upstream_message(&data, &["msg"]).unwrap_or("NanoBanana submit failed");
        return Err(PossiblyAccepted(ImageError::bad_gateway(sanitized(
            &delivery, &callback, secret, message,
        ))));
    }
    let id = data["data"]["taskId"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256 && !delivery.contains_secret(id, secret))
        .ok_or_else(|| {
            PossiblyAccepted(ImageError::bad_gateway(
                "NanoBanana: no valid taskId returned",
            ))
        })?;
    let path = format!(
        "{POLL}?taskId={}",
        utf8_percent_encode(id, NON_ALPHANUMERIC)
    );
    let status = timeout(POLL_TIMEOUT, async {
        let interval = if fixture() {
            Duration::from_millis(25)
        } else {
            POLL_INTERVAL
        };
        loop {
            sleep(interval).await;
            let response = delivery
                .request(Method::GET, &path, &key)
                .header(header::CONTENT_TYPE, "application/json")
                .send()
                .await
                .map_err(|_| ImageError::bad_gateway("NanoBanana status request failed"))?;
            if !response.status().is_success() {
                return Err(ImageError::bad_gateway(format!(
                    "NanoBanana status {}",
                    response.status().as_u16()
                )));
            }
            let state = read_json(response).await?;
            match state["data"]["successFlag"].as_f64() {
                Some(1.0) => return Ok(state),
                Some(2.0 | 3.0) => {
                    let message = upstream_message(&state["data"], &["errorMessage"])
                        .unwrap_or("NanoBanana generation failed");
                    return Err(ImageError::bad_gateway(sanitized(
                        &delivery, &callback, secret, message,
                    )));
                }
                _ => (),
            }
        }
    })
    .await
    .map_err(|_| PossiblyAccepted(ImageError::bad_gateway("NanoBanana polling timeout")))?
    .map_err(PossiblyAccepted)?;

    let image = upstream_message(
        &status["data"]["response"],
        &["resultImageUrl", "originImageUrl"],
    );
    let images = if let Some(image) = image {
        let decoded = percent_decode_str(image).decode_utf8_lossy();
        if image.len() > 8192
            || !Url::parse(image).is_ok_and(|url| public_target(&url))
            || delivery.contains_secret(&decoded, secret)
            || decoded.contains(callback.as_str())
        {
            return Err(PossiblyAccepted(ImageError::bad_gateway(
                "NanoBanana returned an invalid media URL",
            )));
        }
        vec![json!({"url":image,"revised_prompt":body["prompt"]})]
    } else {
        Vec::new()
    };
    Ok(json!({"created":chrono::Utc::now().timestamp(),"data":images}))
}

fn payload(body: &Value, callback: &Url) -> Value {
    let ratio = match body["size"].as_str() {
        Some("1024x1792") => "9:16",
        Some("1792x1024") => "16:9",
        Some("1024x1536") => "2:3",
        Some("1536x1024") => "3:2",
        _ => "1:1",
    };
    let count = body
        .get("n")
        .filter(|count| truthy(count))
        .cloned()
        .unwrap_or_else(|| json!(1));
    let input = body["images"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let edit = !input.is_empty() || truthy(&body["image"]);
    let mut payload = json!({
        "prompt":body["prompt"],"numImages":count,"image_size":ratio,"callBackUrl":callback.as_str(),
        "type":if edit { "IMAGETOIAMGE" } else { "TEXTTOIAMGE" },
    });
    if edit {
        let mut images: Vec<_> = input.iter().filter(|image| truthy(image)).collect();
        if truthy(&body["image"]) {
            images.push(&body["image"]);
        }
        payload["imageUrls"] = json!(images);
    }
    payload
}

fn upstream_message<'a>(data: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| data[*key].as_str().filter(|text| !text.is_empty()))
}

fn sanitized(delivery: &Delivery, callback: &Url, secret: &str, message: &str) -> String {
    delivery
        .redact(message, secret)
        .replace(callback.as_str(), "[redacted]")
}

async fn read_json(mut response: Response) -> Result<Value, ImageError> {
    let invalid = || ImageError::bad_gateway("Invalid NanoBanana response");
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| invalid())? {
        if chunk.len() > RESPONSE_LIMIT - bytes.len() {
            return Err(invalid());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}
