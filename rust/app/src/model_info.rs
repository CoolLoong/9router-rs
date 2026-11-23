use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use crate::chat_input::truthy;
use crate::observed_response::into_http;
use crate::openai_models::{data, model_kind, respond};
use crate::provider_index;

pub async fn options() -> Response {
    into_http(Ok(json!({
        "status": 200,
        "headers": {
            "access-control-allow-origin": "*",
            "access-control-allow-methods": "GET, OPTIONS",
        },
        "body": "",
    })))
}

pub async fn show(request: Request) -> Response {
    let query = request.uri().query().unwrap_or_default();
    let mut id = None;
    let mut kind = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "id" if id.is_none() => id = Some(value.into_owned()),
            "kind" if kind.is_none() => kind = Some(value.into_owned()),
            _ => (),
        }
    }
    let Some(id) = id.filter(|id| !id.is_empty()) else {
        return respond(
            StatusCode::BAD_REQUEST,
            json!({
                "error": {
                    "message": "Missing required query param: id (e.g. ?id=openai/dall-e-3)",
                    "type": "invalid_request_error",
                },
            }),
            true,
        );
    };
    if let Some(info) = lookup(&id, kind.as_deref().filter(|kind| !kind.is_empty())) {
        respond(StatusCode::OK, info, true)
    } else {
        respond(
            StatusCode::NOT_FOUND,
            json!({
                "error": {"message":format!("Model not found: {id}"), "type":"not_found"},
            }),
            true,
        )
    }
}

fn lookup(id: &str, wanted: Option<&str>) -> Option<Value> {
    let (alias, model_id) = id.split_once('/')?;
    let registry = data();
    let provider = registry["infoProviderByAlias"][alias]
        .as_str()
        .unwrap_or(alias);
    let models = &registry["models"];
    let catalog = models.get(alias).or_else(|| models.get(provider));
    if let Some(model) = catalog.and_then(Value::as_array).and_then(|models| {
        models.iter().find(|model| {
            model["id"] == model_id && wanted.is_none_or(|kind| model_kind(model) == kind)
        })
    }) {
        return Some(build(alias, provider, model, model_kind(model)));
    }
    let (kind, label, params) =
        if model_id == "search" && registry["searchModelInfo"].get(provider).is_some() {
            (
                "webSearch",
                "Search",
                json!([
                    "query",
                    "max_results",
                    "country",
                    "language",
                    "time_range",
                    "domain_filter",
                    "search_type"
                ]),
            )
        } else if model_id == "fetch"
            && registry["providersWithFetch"]
                .as_array()
                .is_some_and(|models| models.iter().any(|id| id == provider))
        {
            (
                "webFetch",
                "Fetch",
                json!(["url", "format", "max_characters"]),
            )
        } else {
            return None;
        };
    let display = provider_index::find(provider)?.get("name")?.as_str()?;
    let model = json!({"id": model_id, "name": format!("{display} {label}"), "params": params});
    Some(build(alias, provider, &model, kind))
}

fn build(alias: &str, provider: &str, model: &Value, kind: &str) -> Value {
    let endpoint = match kind {
        "llm" | "imageToText" => Some("/v1/chat/completions"),
        "image" => Some("/v1/images/generations"),
        "tts" => Some("/v1/audio/speech"),
        "stt" => Some("/v1/audio/transcriptions"),
        "embedding" => Some("/v1/embeddings"),
        "webSearch" => Some("/v1/search"),
        "webFetch" => Some("/v1/fetch"),
        _ => None,
    };
    let id = model["id"].as_str().unwrap_or("");
    let name = model["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .unwrap_or(id);
    let mut output = json!({
        "id": format!("{alias}/{id}"), "name": name, "kind": kind, "owned_by": alias, "endpoint": endpoint,
    });
    for field in [
        "params",
        "capabilities",
        "options",
        "dimensions",
        "contextWindow",
    ] {
        if truthy(&model[field]) {
            output[field] = model[field].clone();
        }
    }
    if kind == "tts"
        && matches!(
            provider,
            "elevenlabs" | "edge-tts" | "deepgram" | "inworld" | "local-device"
        )
    {
        output["voicesUrl"] = json!(format!("/v1/audio/voices?provider={provider}"));
    }
    if kind == "webSearch" {
        let config = &data()["searchModelInfo"][provider];
        for field in ["searchTypes", "maxResults", "required"] {
            if truthy(&config[field]) {
                output[field] = config[field].clone();
            }
        }
    }
    output
}
