use std::path::PathBuf;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::{Value, json};

use super::{CliTools, in_config, persist, read, reply, resolve_key};
use crate::cli::without_trailing_commas;
use crate::provider_connections::truthy;

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router.route(
        "/api/cli-tools/copilot-settings",
        get(status).post(apply).delete(reset),
    )
}

pub(super) async fn host_status() -> Option<Value> {
    tokio::task::spawn_blocking(check).await.ok()?.ok()
}

fn config_path() -> Result<PathBuf, String> {
    let home = std::env::home_dir().ok_or("Unable to locate the home directory")?;
    let user = match std::env::consts::OS {
        "windows" => std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or(home),
        "macos" => home.join("Library/Application Support"),
        _ => home.join(".config"),
    };
    Ok(user.join("Code/User/chatLanguageModels.json"))
}

fn parse(bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(&without_trailing_commas(bytes))
        .map_err(|_| "Unable to parse Copilot model settings".into())
}

fn check() -> Result<Value, String> {
    let path = config_path()?;
    let mut config = read(&path)
        .ok()
        .flatten()
        .and_then(|bytes| parse(&bytes).ok())
        .unwrap_or_default();
    if let Some(entries) = config.as_array_mut() {
        for entry in entries {
            if let Some(fields) = entry.as_object_mut() {
                fields.shift_remove("apiKey");
            }
        }
    }
    let entry = config
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["name"] == "9Router"));
    let model = entry.and_then(|entry| entry["models"].get(0));
    Ok(json!({
        "installed":true,
        "config":config,
        "has9Router":entry.is_some(),
        "configPath":path,
        "currentModel":model.map(|model| &model["id"]).filter(|id| truthy(id)),
        "currentUrl":model.map(|model| &model["url"]).filter(|url| truthy(url)),
    }))
}

async fn status(State(state): State<CliTools>) -> Response {
    answer(
        in_config(state.editing, check).await,
        "Failed to check copilot settings",
    )
}

async fn apply(State(state): State<CliTools>, request: Request) -> Response {
    let failure = "Failed to update copilot settings";
    let Some(body) = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|body| !body.is_null())
    else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, failure);
    };
    let Some((base, models)) = body["baseUrl"]
        .as_str()
        .filter(|base| !base.is_empty())
        .zip(
            body["models"]
                .as_array()
                .filter(|models| !models.is_empty()),
        )
    else {
        return error(StatusCode::BAD_REQUEST, "baseUrl and models are required");
    };
    let url = format!("{base}/chat/completions#models.ai.azure.com");
    let models: Vec<_> = models
        .iter()
        .map(|id| {
            json!({
                "id":id,"name":id,"url":url,"toolCalling":true,"vision":false,
                "maxInputTokens":128000,"maxOutputTokens":16000,
            })
        })
        .collect();
    let key = resolve_key(&state.storage, body["apiKey"].as_str()).await;
    answer(
        in_config(state.editing, move || {
            let path = config_path()?;
            let mut entries = match read(&path)? {
                Some(bytes) => parse(&bytes)?
                    .as_array()
                    .cloned()
                    .ok_or("Expected Copilot model list")?,
                None => Vec::new(),
            };
            let replacement =
                json!({"name":"9Router","vendor":"azure","apiKey":key,"models":models});
            if let Some(prior) = entries.iter_mut().find(|entry| entry["name"] == "9Router") {
                *prior = replacement;
            } else {
                entries.push(replacement);
            }
            write(&path, &entries)?;
            Ok(json!({
                "success":true,
                "message":"Copilot settings applied! Reload VS Code to take effect.",
                "configPath":path,
            }))
        })
        .await,
        failure,
    )
}

async fn reset(State(state): State<CliTools>) -> Response {
    answer(
        in_config(state.editing, || {
            let path = config_path()?;
            let Some(bytes) = read(&path)? else {
                return Ok(json!({"success":true,"message":"No config file to reset"}));
            };
            let mut entries = parse(&bytes)?
                .as_array()
                .cloned()
                .ok_or("Expected Copilot model list")?;
            let count = entries.len();
            entries.retain(|entry| entry["name"] != "9Router");
            if entries.len() != count {
                write(&path, &entries)?;
            }
            Ok(json!({"success":true,"message":"9Router removed from Copilot config"}))
        })
        .await,
        "Failed to reset copilot settings",
    )
}

fn write(path: &std::path::Path, entries: &[Value]) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(entries).map_err(|error| error.to_string())?;
    persist(path, Some(&bytes))
}

fn answer(result: Result<Value, String>, message: &str) -> Response {
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}))
}
