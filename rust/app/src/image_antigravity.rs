use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use super::{BODY_LIMIT, inline_image, normalize_gemini};
use crate::antigravity_transport::{Failure, complete, image_model, prepare};
use crate::chat_format::ChatFormat;
use crate::chat_output::js_text;
use crate::codex_executor::Sessions;
use crate::provider_connections::truthy;
use crate::storage::Storage;

pub(super) async fn generate(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    input: &Value,
    sessions: &Sessions,
) -> Result<Value, Failure> {
    let model = if image_model(model) {
        model
    } else {
        "gemini-3.1-flash-image"
    };
    let ratio = aspect_ratio(input).map(|ratio| ratio.replace(':', "x"));
    let model = ratio
        .filter(|suffix| !model.contains(suffix))
        .map_or_else(|| model.to_owned(), |suffix| format!("{model}-{suffix}"));
    let mut parts = ["reference_images", "images", "reference_image", "image"]
        .into_iter()
        .map(|field| &input[field])
        .filter(|value| truthy(value))
        .flat_map(|value| {
            value
                .as_array()
                .map_or_else(|| std::slice::from_ref(value), Vec::as_slice)
        })
        .take(10)
        .filter_map(|image| {
            let image = if image.is_object() {
                [
                    &image["image_url"]["url"],
                    &image["url"],
                    &image["data"],
                    &image["b64_json"],
                ]
                .into_iter()
                .find(|value| truthy(value))?
            } else {
                image
            };
            inline_image(image.as_str()?)
        })
        .collect::<Vec<_>>();
    let prompt = if truthy(&input["negative_prompt"]) {
        json!(format!(
            "{}\n\nNegative prompt: {}",
            js_text(&input["prompt"]),
            js_text(&input["negative_prompt"])
        ))
    } else {
        input["prompt"].clone()
    };
    parts.push(json!({"text":prompt}));
    let body = json!({"contents":[{"role":"user","parts":parts}]});
    let prepared = prepare(ChatFormat::Chat, body, &model, "application/json", |_| None)
        .ok_or_else(|| {
            Failure::new(StatusCode::BAD_REQUEST, "Invalid Antigravity image request")
        })?;
    let (mut upstream, _) = complete(
        storage,
        settings,
        account,
        &Value::Null,
        &prepared,
        &HeaderMap::new(),
        sessions,
    )
    .await?;
    let invalid = || {
        Failure::new(
            StatusCode::BAD_GATEWAY,
            "Invalid Antigravity image response",
        )
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = upstream.chunk().await.map_err(|_| invalid())? {
        if chunk.len() > BODY_LIMIT - bytes.len() {
            return Err(invalid());
        }
        bytes.extend_from_slice(&chunk);
    }
    let result: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let output = if truthy(&result["candidates"]) {
        &result
    } else {
        &result["response"]
    };
    Ok(normalize_gemini(output, &input["prompt"]))
}

fn aspect_ratio(input: &Value) -> Option<&str> {
    if let Some(value) = input["aspect_ratio"].as_str()
        && let Some((width, height)) = value.split_once(':')
        && [width, height].into_iter().all(|part| {
            (1..=2).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Some(value);
    }
    match input["size"].as_str()? {
        "" => None,
        "1024x1792" => Some("9:16"),
        "1792x1024" => Some("16:9"),
        "1024x1536" => Some("2:3"),
        "1536x1024" => Some("3:2"),
        _ => Some("1:1"),
    }
}
