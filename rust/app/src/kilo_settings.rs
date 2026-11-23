use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::{Map, Value, json};

use super::{CliTools, executable, in_config, persist, reply, serialize_json};
use crate::cli::Tool;
use crate::cli_connect::{
    read_settings,
    tools::{configure_kilo, reset_kilo},
};
use crate::provider_connections::{object, truthy};

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router.route(
        "/api/cli-tools/kilo-settings",
        get(status).post(apply).delete(reset),
    )
}

pub(super) async fn host_status() -> Option<Value> {
    tokio::task::spawn_blocking(check).await.ok()?.ok()
}

fn paths() -> Result<(PathBuf, PathBuf), String> {
    let home = std::env::home_dir().ok_or("Missing home directory")?;
    let file = Tool::Kilo
        .files(&home, None)
        .into_iter()
        .next()
        .ok_or("Missing Kilo settings path")?;
    Ok((file, home.join(".config/Code/User/settings.json")))
}

fn check() -> Result<Value, String> {
    let (path, _) = paths()?;
    let installed = path.exists()
        || executable("kilo")
        || cfg!(windows)
            && std::env::var_os("APPDATA")
                .is_some_and(|data| PathBuf::from(data).join("npm/kilo.cmd").is_file());
    if !installed {
        return Ok(
            json!({"installed":false,"settings":null,"message":"Kilo Code CLI is not installed"}),
        );
    }
    let config = read_settings(&path)
        .ok()
        .flatten()
        .map_or(Value::Null, |(_, config)| config);
    let keys = object(config.clone())
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    let entry = config
        .get("openai-compatible")
        .filter(|value| truthy(value))
        .unwrap_or(&config["9router"]);
    let base = entry
        .get("baseUrl")
        .filter(|value| truthy(value))
        .unwrap_or(&entry["baseURL"]);
    let configured = base.as_str().is_some_and(|base| {
        ["localhost", "127.0.0.1", "9router"]
            .iter()
            .any(|marker| base.contains(marker))
    });
    Ok(json!({
        "installed":true,"settings":{"auth":keys},"has9Router":configured,"authPath":path,
        "routerConfig":{"baseUrl":base.as_str(),"model":entry["model"].as_str()}
    }))
}

async fn status(State(state): State<CliTools>) -> Response {
    complete(
        in_config(state.editing, check).await,
        "Failed to check kilo settings",
    )
}

async fn apply(State(state): State<CliTools>, request: Request) -> Response {
    let failure = "Failed to update kilo settings";
    let body = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|value| !value.is_null());
    let Some(body) = body else {
        return problem(StatusCode::INTERNAL_SERVER_ERROR, failure);
    };
    if ["baseUrl", "apiKey", "model"]
        .iter()
        .any(|name| !truthy(&body[*name]))
    {
        return problem(
            StatusCode::BAD_REQUEST,
            "baseUrl, apiKey and model are required",
        );
    }
    let Some(base) = body["baseUrl"].as_str() else {
        return problem(StatusCode::INTERNAL_SERVER_ERROR, failure);
    };
    let base = if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    };
    let result = in_config(state.editing, move || {
        let (file, vscode) = paths()?;
        let mut config = read_settings(&file)?.map_or_else(Map::new, |(_, value)| object(value));
        configure_kilo(&mut config, &base, &body["apiKey"], &body["model"]);
        save(&file, config)?;
        if vscode.parent().is_some_and(Path::is_dir)
            && let Ok(existing) = read_settings(&vscode)
        {
            let mut settings = existing.map_or_else(Map::new, |(_, value)| object(value));
            settings.insert("kilocode.customProvider".into(), json!({
                "name":"9Router","baseURL":base,"apiKey":body["apiKey"],
            }));
            settings.insert("kilocode.defaultModel".into(), body["model"].clone());
            let _ = save(&vscode, settings);
        }
        Ok(json!({"success":true,"message":"Kilo Code settings applied successfully!","authPath":file}))
    }).await;
    complete(result, failure)
}

async fn reset(State(state): State<CliTools>) -> Response {
    let result = in_config(state.editing, || {
        let (file, vscode) = paths()?;
        let Some((_, config)) = read_settings(&file)
            .ok()
            .flatten()
            .filter(|(_, value)| truthy(value))
        else {
            return Ok(json!({"success":true,"message":"No settings file to reset"}));
        };
        let mut config = object(config);
        reset_kilo(&mut config);
        save(&file, config)?;
        if let Ok(Some((_, settings))) = read_settings(&vscode)
            && truthy(&settings)
        {
            let mut settings = object(settings);
            settings.shift_remove("kilocode.customProvider");
            settings.shift_remove("kilocode.defaultModel");
            let _ = save(&vscode, settings);
        }
        Ok(json!({"success":true,"message":"9Router settings removed from Kilo Code"}))
    })
    .await;
    complete(result, "Failed to reset kilo settings")
}

fn save(path: &Path, config: Map<String, Value>) -> Result<(), String> {
    persist(path, Some(serialize_json(config)?.as_bytes()))
}

fn complete(result: Result<Value, String>, message: &str) -> Response {
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

fn problem(status: StatusCode, error: &str) -> Response {
    reply(status, json!({"error":error}))
}
