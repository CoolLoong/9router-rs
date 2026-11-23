use std::sync::LazyLock;
use std::time::Duration;

use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::{Client, Method, RequestBuilder, Response, header, multipart::Form};
use serde_json::{Map, Value, json};

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_http;
use crate::provider_index::js_trim;
use crate::proxy_transport::{Route, Selection};
use crate::storage::Storage;
use crate::thinking_intent::numeric;

use super::{
    Adapter, ImageError, QueuedImageError, decode_image, finish_generation, proxy_error,
    proxy_required,
};

const BASE: &str = "https://api.cloudflare.com/client/v4/accounts";
const FIXTURE_BASE: &str = "http://127.0.0.1:52989/cloudflare-image/client/v4/accounts";
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const OPTIONAL: &[&str] = &[
    "negative_prompt",
    "guidance",
    "seed",
    "num_steps",
    "steps",
    "strength",
];
static HTTP: LazyLock<Result<Client, reqwest::Error>> = LazyLock::new(|| {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()
});

pub(super) async fn generate(
    storage: &Storage,
    settings: &Value,
    model: &str,
    account: &Value,
    key: Option<&str>,
    body: &Value,
) -> Result<(Value, bool), QueuedImageError> {
    let account_id = account["providerSpecificData"]["accountId"]
        .as_str()
        .map(js_trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| bad("cloudflare-ai requires accountId in providerSpecificData"))?;
    if account_id.len() != 32 || !account_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(bad("Invalid Cloudflare account ID").into());
    }
    if model.len() > 256
        || !model.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'.' | b'_' | b'-')
                })
        })
    {
        return Err(bad("Invalid Cloudflare image model ID").into());
    }
    let client = HTTP
        .as_ref()
        .map_err(|_| ImageError::bad_gateway("[502]: fetch failed"))?;
    let path = format!("/{account_id}/ai/run/{model}");
    let endpoint = format!("{BASE}{path}");
    let fixture = format!("{FIXTURE_BASE}{path}");
    let unavailable = || {
        QueuedImageError::Protected(proxy_error(
            "Cloudflare image connection proxy is unavailable",
        ))
    };
    let route = provider_http::select(storage, settings, account, &endpoint)
        .await
        .map_err(|_| unavailable())?;
    if matches!(&route, Selection::Routed(Route::Direct)) && proxy_required(settings, account)
        || matches!(&route, Selection::Routed(relay @ Route::Relay { .. }) if relay.relay_secret().is_none())
    {
        return Err(unavailable());
    }
    let delivery = provider_http::bind_selection(
        route,
        &endpoint,
        &fixture,
        client,
        Duration::from_secs(120),
        None,
    )
    .ok_or_else(unavailable)?;
    let mut request = delivery.request(Method::POST);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let request = payload(request, model, body).await?;
    let value = finish_generation(
        request,
        Adapter::Cloudflare,
        "cloudflare-ai",
        body,
        delivery.protected,
        &[key, delivery.relay_secret()],
    )
    .await
    .map_err(|failure| {
        if delivery.protected {
            QueuedImageError::Protected(failure)
        } else {
            failure.into()
        }
    })?;
    Ok((value, delivery.protected))
}

async fn payload(
    request: RequestBuilder,
    model: &str,
    body: &Value,
) -> Result<RequestBuilder, ImageError> {
    let mut payload = Map::from_iter([("prompt".into(), body["prompt"].clone())]);
    if let Some((width, height)) = body["size"].as_str().and_then(|size| size.split_once('x'))
        && let (Ok(width), Ok(height)) = (width.parse::<u32>(), height.parse::<u32>())
    {
        payload.insert("width".into(), json!(width));
        payload.insert("height".into(), json!(height));
    }
    for name in ["width", "height"] {
        if let Some(value) = body
            .get(name)
            .and_then(numeric)
            .filter(|value| value.is_finite())
        {
            payload.insert(
                name.into(),
                if value.fract() == 0.0 && value.abs() <= i64::MAX as f64 {
                    json!(value as i64)
                } else {
                    json!(value)
                },
            );
        }
    }
    for &name in OPTIONAL {
        if let Some(value) = body
            .get(name)
            .filter(|value| !value.is_null() && *value != "")
        {
            payload.insert(name.into(), value.clone());
        }
    }
    if matches!(
        model,
        "@cf/black-forest-labs/flux-2-dev"
            | "@cf/black-forest-labs/flux-2-klein-4b"
            | "@cf/black-forest-labs/flux-2-klein-9b"
    ) {
        let mut form = Form::new();
        for (name, value) in payload {
            form = form.text(name, js_text(&value));
        }
        return Ok(request.multipart(form));
    }
    if let Some((bytes, encoded)) = image_input(&body["image"]).await? {
        payload.insert("image_b64".into(), json!(encoded));
        payload.insert("image".into(), json!(bytes));
    }
    let mask = [&body["mask_image"], &body["maskImage"], &body["mask"]]
        .into_iter()
        .find(|value| truthy(value));
    if let Some(mask) = mask
        && let Some((bytes, encoded)) = image_input(mask).await?
    {
        payload.insert("mask_b64".into(), json!(encoded));
        let bytes = json!(bytes);
        payload.insert("mask".into(), bytes.clone());
        payload.insert("mask_image".into(), bytes);
    }
    Ok(request.json(&payload))
}

async fn image_input(value: &Value) -> Result<Option<(Vec<u8>, String)>, ImageError> {
    if let Some(values) = value.as_array() {
        if values.len() > MAX_IMAGE_BYTES {
            return Err(bad("Cloudflare image input is too large"));
        }
        let bytes = values
            .iter()
            .map(|value| value.as_u64().and_then(|value| u8::try_from(value).ok()))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| bad("Invalid Cloudflare image byte array"))?;
        let encoded = STANDARD.encode(&bytes);
        return Ok(Some((bytes, encoded)));
    }
    let Some(text) = value.as_str().map(js_trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    if url::Url::parse(text).is_ok_and(|url| matches!(url.scheme(), "http" | "https")) {
        let bytes = crate::chat_prefetch::fetch_image_bytes(text)
            .await
            .ok_or_else(|| bad("Could not read a public Cloudflare image input"))?;
        let encoded = STANDARD.encode(&bytes);
        return Ok(Some((bytes, encoded)));
    }
    let encoded = inline_data(text).unwrap_or(text);
    if encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(bad("Cloudflare image input is too large"));
    }
    Ok(Some((decode_image(encoded), encoded.to_owned())))
}

fn inline_data(text: &str) -> Option<&str> {
    let (header, body) = text.split_once(',')?;
    let header = header.to_ascii_lowercase();
    let format = header
        .strip_prefix("data:image/")?
        .strip_suffix(";base64")?;
    (!format.is_empty() && !format.contains(';')).then_some(body)
}

pub(super) async fn response(mut response: Response) -> Result<Value, ImageError> {
    let binary = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("image/"));
    let invalid = || ImageError::bad_gateway("Invalid response from cloudflare-ai");
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
    {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| invalid())? {
        if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
            return Err(invalid());
        }
        bytes.extend_from_slice(&chunk);
    }
    if binary {
        return Ok(images(Some(json!({"b64_json":STANDARD.encode(bytes)}))));
    }

    let body = serde_json::from_slice::<Value>(&bytes).map_err(|_| invalid())?;
    let mut value = &body;
    loop {
        if truthy(&value["created"]) && value["data"].is_array() {
            return Ok(value.clone());
        }
        let result = value
            .get("result")
            .filter(|result| !result.is_null())
            .unwrap_or(value);
        let queued = result["responses"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["success"] != false))
            .and_then(|item| item.get("result"))
            .filter(|result| truthy(result));
        if let Some(nested) = queued {
            value = nested;
            continue;
        }
        let image = [
            result,
            &result["image"],
            &result["data"][0]["b64_json"],
            &result["data"][0]["url"],
        ]
        .into_iter()
        .filter_map(Value::as_str)
        .find(|image| !image.is_empty());
        return Ok(images(image.map(|image| {
            if let Some(data) = inline_data(image) {
                json!({"b64_json":data})
            } else if url::Url::parse(image)
                .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
            {
                json!({"url":image})
            } else {
                json!({"b64_json":image})
            }
        })));
    }
}

fn images(entry: Option<Value>) -> Value {
    json!({"created":chrono::Utc::now().timestamp(),"data":entry.into_iter().collect::<Vec<_>>()})
}

fn bad(message: &str) -> ImageError {
    ImageError {
        status: StatusCode::BAD_REQUEST,
        message: message.into(),
    }
}
