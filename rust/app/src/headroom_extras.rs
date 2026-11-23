use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use super::{HeadroomAdmin, Package, denied, detect, inspect_packages, local_authorized, reply};
use crate::dashboard_session;

pub(super) const CODE: &[&str] = &["tree-sitter", "tree-sitter-language-pack"];
pub(super) const ML: &[&str] = &["torch", "huggingface-hub"];
const AVAILABLE: &[&str] = &["code", "ml"];
const LOG: &str = "install.log";
const TIMEOUT: Duration = Duration::from_secs(900);

pub(super) async fn status(State(state): State<HeadroomAdmin>, request: Request) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return denied(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let want_log = request.uri().query().is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(name, value)| (name == "log").then_some(value == "1"))
            .unwrap_or(false)
    });
    let body = if want_log {
        if !local_authorized(&state.storage, &request).await {
            return denied(StatusCode::FORBIDDEN, "Local only: CLI token required");
        }
        let path = state.storage.root().join("headroom").join(LOG);
        let log = tokio::task::spawn_blocking(move || log_tail(&path))
            .await
            .unwrap_or_default();
        json!({"log":log})
    } else {
        let package = tokio::task::spawn_blocking(|| detect().package)
            .await
            .unwrap_or_default();
        let mut body = json!({"available":AVAILABLE});
        add_status(&mut body, package);
        body
    };
    reply(StatusCode::OK, body, true)
}

pub(super) async fn install(State(state): State<HeadroomAdmin>, request: Request) -> Response {
    change(state, request, true).await
}

pub(super) async fn uninstall(State(state): State<HeadroomAdmin>, request: Request) -> Response {
    change(state, request, false).await
}

async fn change(state: HeadroomAdmin, request: Request, installing: bool) -> Response {
    if !local_authorized(&state.storage, &request).await {
        return denied(StatusCode::FORBIDDEN, "Local only: CLI token required");
    }
    let requested = to_bytes(request.into_body(), 32 * 1024)
        .await
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .unwrap_or_default();
    let mut extras = Vec::new();
    for requested in requested["extras"].as_array().into_iter().flatten() {
        if let Some(extra) = AVAILABLE.iter().copied().find(|name| requested == *name)
            && !extras.contains(&extra)
        {
            extras.push(extra);
        }
    }
    let Ok(guard) = state.extras.try_lock_owned() else {
        return error(
            StatusCode::CONFLICT,
            "Headroom package operation is already running",
            "BUSY",
        );
    };
    let directory = state.storage.root().join("headroom");
    tokio::spawn(async move {
        let _guard = guard;
        run(&directory, extras, installing).await
    })
    .await
    .unwrap_or_else(|_| operation_failed(installing))
}

async fn run(directory: &Path, extras: Vec<&'static str>, installing: bool) -> Response {
    let Ok(details) = tokio::task::spawn_blocking(detect).await else {
        return operation_failed(installing);
    };
    let Some(python) = details.python_bin else {
        return error(
            StatusCode::BAD_REQUEST,
            "Python >= 3.10 not found",
            "NO_PYTHON",
        );
    };
    if installing && details.path.is_none() {
        return error(
            StatusCode::BAD_REQUEST,
            "headroom-ai not installed (run `pip install headroom-ai[proxy]` first)",
            "NOT_INSTALLED",
        );
    }
    if !installing && extras.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "No valid extras to remove",
            "INVALID_EXTRAS",
        );
    }
    let mut args = vec!["-m", "pip"];
    let spec = format!(
        "headroom-ai[{}]",
        std::iter::once("proxy")
            .chain(extras.iter().copied())
            .collect::<Vec<_>>()
            .join(",")
    );
    let removed = extras
        .iter()
        .flat_map(|extra| if *extra == "code" { CODE } else { ML })
        .copied()
        .collect::<Vec<_>>();
    if installing {
        args.extend(["install", "--upgrade", &spec]);
    } else {
        args.extend(["uninstall", "-y"]);
        args.extend(removed.iter().copied());
    }
    let Ok(log) = install_log(directory) else {
        return operation_failed(installing);
    };
    let Ok(stderr) = log.try_clone() else {
        return operation_failed(installing);
    };
    let mut command = tokio::process::Command::new(&python);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(stderr)
        .env("PIP_NO_INPUT", "1")
        .kill_on_drop(true);
    let completed = tokio::time::timeout(TIMEOUT, command.status()).await;
    match completed {
        Ok(Ok(status)) if status.success() => (),
        Ok(Ok(status)) => {
            let action = if installing { "install" } else { "uninstall" };
            let code = status
                .code()
                .map_or_else(|| "null".into(), |code| code.to_string());
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("pip {action} exited with code={code} — see headroom/install.log"),
                failure_code(installing),
            );
        }
        _ => return operation_failed(installing),
    }
    let package = tokio::task::spawn_blocking(move || inspect_packages(&python))
        .await
        .unwrap_or_default();
    let mut body = json!({"success":true,"code":0});
    body[if installing { "spec" } else { "removed" }] = if installing {
        json!(spec)
    } else {
        json!(removed)
    };
    add_status(&mut body, package);
    reply(StatusCode::OK, body, true)
}

fn add_status(body: &mut Value, package: Package) {
    body["installed"] = json!(package.installed);
    body["version"] = json!(package.version);
    body["extras"] = json!({"code":package.code,"ml":package.ml});
}

fn install_log(directory: &Path) -> std::io::Result<fs::File> {
    fs::create_dir_all(directory)?;
    let temporary = tempfile::NamedTempFile::new_in(directory)?;
    Ok(temporary.persist(directory.join(LOG))?)
}

fn log_tail(path: &Path) -> String {
    const MAX_BYTES: u64 = 128 * 1024;
    let result = (|| -> std::io::Result<String> {
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_file() {
            return Ok(String::new());
        }
        let mut file = fs::File::open(path)?;
        let start = meta.len().saturating_sub(MAX_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut tail = Vec::new();
        file.take(MAX_BYTES).read_to_end(&mut tail)?;
        let tail = String::from_utf8_lossy(&tail);
        let lines = tail
            .lines()
            .filter(|line| !line.is_empty())
            .rev()
            .take(15)
            .collect::<Vec<_>>();
        Ok(lines.into_iter().rev().collect::<Vec<_>>().join("\n"))
    })();
    result.unwrap_or_default()
}

fn error(status: StatusCode, message: impl Into<String>, code: &str) -> Response {
    reply(status, json!({"error":message.into(),"code":code}), true)
}

fn failure_code(installing: bool) -> &'static str {
    if installing {
        "INSTALL_FAILED"
    } else {
        "UNINSTALL_FAILED"
    }
}

fn operation_failed(installing: bool) -> Response {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Headroom package operation failed — see headroom/install.log",
        failure_code(installing),
    )
}
