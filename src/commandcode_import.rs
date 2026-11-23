use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::PathBuf;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use crate::catalog_guard::{is_direct_loopback, valid_cli_token};
use crate::dashboard_session;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::settings::reply;
use crate::storage::Storage;

const LIMIT: u64 = 1024 * 1024;

pub(super) async fn read(State(storage): State<Storage>, request: Request) -> Response {
    if !local_authorized(&storage, &request) {
        return reply(
            StatusCode::FORBIDDEN,
            json!({"found":false,"error":"Local only: CLI token required"}),
            true,
        );
    }
    match tokio::task::spawn_blocking(find).await {
        Ok(Ok(value)) => reply(StatusCode::OK, value, true),
        _ => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"found":false,"error":"Failed to read Command Code credentials"}),
            true,
        ),
    }
}

pub(super) fn local_authorized(storage: &Storage, request: &Request) -> bool {
    valid_cli_token(storage, request.headers())
        || is_direct_loopback(request) && dashboard_session::has_session(storage, request.headers())
}

fn candidates() -> Option<Vec<PathBuf>> {
    let home = std::env::home_dir()?;
    let mut paths = vec![home.join(".commandcode/auth.json")];
    if cfg!(windows) {
        paths.push(
            std::env::var_os("APPDATA")
                .filter(|value| !value.is_empty())
                .map_or_else(|| home.join("AppData/Roaming"), PathBuf::from)
                .join("commandcode/auth.json"),
        );
    }
    Some(paths)
}

fn find() -> Result<Value, &'static str> {
    let candidates = candidates().ok_or("Missing home directory")?;
    for path in &candidates {
        if !std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let file = match File::open(path) {
            Ok(file) if file.metadata().is_ok_and(|metadata| metadata.is_file()) => file,
            Ok(_) => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(_) => return Err("Cannot read the Command Code credential file"),
        };
        let mut bytes = Vec::new();
        file.take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read the Command Code credential file")?;
        if bytes.len() as u64 > LIMIT {
            return Err("Command Code credential file is too large");
        }
        let Ok(auth) = serde_json::from_slice::<Value>(&bytes) else {
            return Ok(
                json!({"found":false,"error":"auth.json is not valid JSON. Please run 'commandcode login' again."}),
            );
        };
        let Some(api_key) = auth["apiKey"]
            .as_str()
            .map(js_trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(
                json!({"found":false,"error":"No apiKey found in ~/.commandcode/auth.json. Please run 'commandcode login'."}),
            );
        };
        let optional = |field: &str| auth.get(field).filter(|value| truthy(value));
        return Ok(json!({
            "found":true,"apiKey":api_key,
            "userId":optional("userId"),"userName":optional("userName"),
            "keyName":optional("keyName"),"authenticatedAt":optional("authenticatedAt"),
            "source":path,
        }));
    }
    let checked = candidates
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(json!({
        "found":false,
        "error":format!("Command Code auth file not found. Checked:\n{checked}\n\nRun 'commandcode login' in your terminal first."),
    }))
}
