//! The Rust port of `src/app/api/v1beta/models/`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{Value, json};

const CATALOG: &str = include_str!("../catalog.json");
static MODELS_BODY: OnceLock<Result<String, String>> = OnceLock::new();

#[derive(Deserialize)]
struct Model {
    id: String,
    name: Option<String>,
}

/// Port of `OPTIONS` (`src/app/api/v1beta/models/route.js:6`).
pub async fn options() -> Result<serde_json::Value, String> {
    Ok(json!({
        "status": 200,
        "headers": {
            "access-control-allow-headers": "*",
            "access-control-allow-methods": "GET, OPTIONS",
            "access-control-allow-origin": "*",
        },
        "body": "",
    }))
}

/// Port of `GET` (`src/app/api/v1beta/models/route.js:20`).
pub async fn get() -> Result<serde_json::Value, String> {
    let body = MODELS_BODY
        .get_or_init(models_body)
        .as_ref()
        .map_err(Clone::clone)?;
    Ok(json!({
        "status": 200,
        "headers": { "content-type": "application/json" },
        "body": body,
    }))
}

fn models_body() -> Result<String, String> {
    let catalog: Vec<(String, Vec<Model>)> =
        serde_json::from_str(CATALOG).map_err(|error| error.to_string())?;
    let mut models = Vec::new();
    let mut seen = HashSet::new();

    for (provider, entries) in &catalog {
        for model in entries {
            let label = model
                .name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or(&model.id);
            add_model(
                &mut models,
                &mut seen,
                format!("models/{provider}/{}", model.id),
                label,
                format!("{provider} model: {label}"),
                &["generateContent"],
            );
            if provider == "gemini" {
                add_model(
                    &mut models,
                    &mut seen,
                    format!("models/{}", model.id),
                    label,
                    format!("Gemini model: {label}"),
                    &["generateContent", "streamGenerateContent"],
                );
            }
        }
    }
    Ok(json!({ "models": models }).to_string())
}

fn add_model(
    models: &mut Vec<Value>,
    seen: &mut HashSet<String>,
    name: String,
    display_name: &str,
    description: String,
    methods: &[&str],
) {
    if seen.insert(name.clone()) {
        models.push(json!({
            "name": name,
            "displayName": display_name,
            "description": description,
            "supportedGenerationMethods": methods,
            "inputTokenLimit": 128000,
            "outputTokenLimit": 8192,
        }));
    }
}
