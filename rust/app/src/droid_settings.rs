use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use crate::cli::{Tool, without_trailing_commas};
use crate::cli_connect::write_private;
use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::storage::Storage;

#[derive(Clone)]
struct DroidState {
    storage: Storage,
    editing: Arc<Mutex<()>>,
}

pub fn router(storage: Storage) -> Router {
    let state = DroidState {
        storage,
        editing: Arc::default(),
    };
    Router::new()
        .route(
            "/api/cli-tools/droid-settings",
            get(get_settings)
                .post(update_settings)
                .delete(remove_settings),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<DroidState>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({"error":"Unauthorized"})),
    )
        .into_response()
}

async fn get_settings(State(state): State<DroidState>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let _editor = state.editing.lock().map_err(|_| ())?;
        status(&settings_path()?)
    })
    .await;
    match result {
        Ok(Ok(value)) => reply(StatusCode::OK, value),
        _ => failed("Failed to check droid settings"),
    }
}

async fn update_settings(State(state): State<DroidState>, request: Request) -> Response {
    let Ok(bytes) = to_bytes(request.into_body(), 1024 * 1024).await else {
        return failed("Failed to update droid settings");
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return failed("Failed to update droid settings");
    };
    if body.is_null() {
        return failed("Failed to update droid settings");
    }
    let result = tokio::task::spawn_blocking(move || {
        let _editor = state.editing.lock().map_err(|_| ())?;
        update(&settings_path()?, &body)
    })
    .await;
    match result {
        Ok(Ok(Some(value))) => reply(StatusCode::OK, value),
        Ok(Ok(None)) => reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"baseUrl and at least one model are required"}),
        ),
        _ => failed("Failed to update droid settings"),
    }
}

async fn remove_settings(State(state): State<DroidState>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let _editor = state.editing.lock().map_err(|_| ())?;
        remove(&settings_path()?)
    })
    .await;
    match result {
        Ok(Ok(value)) => reply(StatusCode::OK, value),
        _ => failed("Failed to reset droid settings"),
    }
}

fn settings_path() -> Result<PathBuf, ()> {
    let home = std::env::home_dir().ok_or(())?;
    Tool::Droid.files(&home, None).into_iter().next().ok_or(())
}

pub(crate) async fn host_status() -> Option<Value> {
    tokio::task::spawn_blocking(|| status(&settings_path()?))
        .await
        .ok()?
        .ok()
}

fn status(path: &Path) -> Result<Value, ()> {
    let installed = crate::cli_tools::executable("droid");
    #[cfg(windows)]
    let installed = installed || npm_droid_installed();
    if !installed && !path.exists() {
        return Ok(
            json!({"installed": false,"settings":null,"message":"Factory Droid CLI is not installed"}),
        );
    }
    let settings = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&without_trailing_commas(&bytes)).ok())
        .unwrap_or_default();
    let has_router = if truthy(&settings["customModels"]) {
        let mut configured = false;
        for row in settings["customModels"].as_array().ok_or(())? {
            if managed(row)? {
                configured = true;
                break;
            }
        }
        configured
    } else {
        false
    };
    Ok(json!({"installed":true,"settings":settings,"has9Router":has_router,"settingsPath":path}))
}

#[cfg(windows)]
fn npm_droid_installed() -> bool {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return false;
    };
    let path = PathBuf::from(appdata).join("npm");
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    extensions
        .split(';')
        .any(|extension| path.join(format!("droid{extension}")).is_file())
}

fn update(path: &Path, body: &Value) -> Result<Option<Value>, ()> {
    let models: Vec<&Value> = body["models"]
        .as_array()
        .map(|rows| rows.iter().collect())
        .unwrap_or_else(|| {
            body["model"]
                .as_str()
                .map(|_| vec![&body["model"]])
                .unwrap_or_default()
        });
    if !truthy(&body["baseUrl"]) || models.is_empty() {
        return Ok(None);
    }
    let base = body["baseUrl"].as_str().ok_or(())?;
    let mut base = base.to_owned();
    if !base.ends_with("/v1") {
        base.push_str("/v1");
    }
    let key = if truthy(&body["apiKey"]) {
        body["apiKey"].clone()
    } else {
        json!("your_api_key")
    };
    let mut settings = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or_else(|| json!({}));
    let fields = settings.as_object_mut().ok_or(())?;
    let entries = fields.entry("customModels").or_insert(Value::Null);
    if !truthy(entries) {
        *entries = json!([]);
    }
    let entries = entries.as_array_mut().ok_or(())?;
    remove_managed(entries)?;
    let first = match body["activeModel"].as_str() {
        Some("") => None,
        Some(active) => Some(
            models
                .iter()
                .position(|model| *model == active)
                .unwrap_or(0),
        ),
        None => Some(0),
    };
    for (index, model) in models.iter().enumerate() {
        let Some(model) = model.as_str().filter(|name| !name.is_empty()) else {
            continue;
        };
        entries.push(json!({
            "model":model,"id":format!("custom:9Router-{index}"),"index":index,"baseUrl":base,"apiKey":key,
            "displayName":model,"maxOutputTokens":131072,"noImageSupport":false,"provider":"openai",
        }));
    }
    if let Some(first) = first.filter(|first| *first < entries.len()) {
        let chosen = entries.remove(first);
        entries.insert(0, chosen);
        for (index, entry) in entries.iter_mut().enumerate() {
            entry
                .as_object_mut()
                .ok_or(())?
                .insert("index".into(), json!(index));
        }
    }
    save(path, &settings)?;
    Ok(Some(
        json!({"success":true,"message":"Factory Droid settings applied successfully!","settingsPath":path}),
    ))
}

fn remove(path: &Path) -> Result<Value, ()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"success":true,"message":"No settings file to reset"}));
        }
        Err(_) => return Err(()),
    };
    let mut settings: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    let fields = settings.as_object_mut().ok_or(())?;
    if let Some(value) = fields.get_mut("customModels").filter(|value| truthy(value)) {
        let rows = value.as_array_mut().ok_or(())?;
        remove_managed(rows)?;
        if rows.is_empty() {
            fields.shift_remove("customModels");
        }
    }
    save(path, &settings)?;
    Ok(json!({"success":true,"message":"9Router settings removed successfully"}))
}

fn managed(row: &Value) -> Result<bool, ()> {
    if row.is_null() {
        return Err(());
    }
    let id = &row["id"];
    Ok(!id.is_null() && id.as_str().ok_or(())?.starts_with("custom:9Router"))
}

fn remove_managed(rows: &mut Vec<Value>) -> Result<(), ()> {
    let mut keep = Vec::with_capacity(rows.len());
    for row in std::mem::take(rows) {
        if !managed(&row)? {
            keep.push(row);
        }
    }
    *rows = keep;
    Ok(())
}

fn save(path: &Path, value: &Value) -> Result<(), ()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| ())?;
    write_private(path, &bytes, path.exists()).map_err(|_| ())
}

fn failed(message: &str) -> Response {
    reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":message}))
}

fn reply(status: StatusCode, value: Value) -> Response {
    into_http(Ok(json!({
        "status":status.as_u16(),"headers":{"content-type":"application/json"},"body":value.to_string(),
    })))
}
