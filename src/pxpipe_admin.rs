use std::path::PathBuf;
use std::time::Instant;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use crate::dashboard_session;
use crate::provider_connections::truthy;
use crate::pxpipe_runtime::{self, Runtime};
use crate::settings::{effective, reply};
use crate::storage::Storage;

#[path = "pxpipe_events.rs"]
mod events;

#[derive(Clone)]
struct Pxpipe {
    storage: Storage,
    runtime: Runtime,
}

pub fn router(storage: Storage, runtime: Runtime) -> Router {
    let state = Pxpipe { storage, runtime };
    Router::new()
        .route("/api/pxpipe/status", get(status))
        .route("/api/pxpipe/stats", get(stats))
        .route("/api/pxpipe/logs", get(logs))
        .route("/api/pxpipe/health", get(health).post(health))
        .route("/api/pxpipe/start", post(start))
        .route("/api/pxpipe/stop", post(stop))
        .route("/api/pxpipe/restart", post(restart))
        .route("/api/pxpipe/install", post(install))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<Pxpipe>, request: Request, next: Next) -> Response {
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

fn npm_available() -> bool {
    let mut directories = if cfg!(windows) {
        [("ProgramFiles", "nodejs"), ("APPDATA", "npm")]
            .into_iter()
            .filter_map(|(variable, path)| {
                std::env::var_os(variable).map(|root| PathBuf::from(root).join(path))
            })
            .collect::<Vec<_>>()
    } else {
        let mut directories = vec![
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/opt/homebrew/bin"),
        ];
        directories.extend(std::env::home_dir().map(|home| home.join(".local/bin")));
        directories.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        directories
    };
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }
    let name = if cfg!(windows) { "npm.cmd" } else { "npm" };
    directories.iter().any(|directory| {
        let Ok(file) = std::fs::metadata(directory.join(name)) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.is_file() && file.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        file.is_file()
    })
}

fn runtime_status(runtime: &Runtime) -> Value {
    let installed = runtime.installed();
    let loaded = runtime.loaded_at();
    json!({
        "installed":installed,"installing":false,"version":installed.then_some(pxpipe_runtime::VERSION),
        "path":installed.then_some(runtime.directory()),
        "running":loaded.is_some(),"loadedAt":loaded,
        "uptimeMs":loaded.map_or(0, |then| (pxpipe_runtime::now() - then).max(0)),"npmAvailable":npm_available(),
        "mode":"library",
    })
}

fn unavailable(status: StatusCode) -> Response {
    reply(
        status,
        json!({"error":"PXPIPE is not installed","code":"NOT_INSTALLED"}),
        false,
    )
}

fn failed(message: &str) -> Response {
    reply(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":message}),
        false,
    )
}

async fn status(State(state): State<Pxpipe>) -> Response {
    let Some(settings) = state.storage.read_settings().await.map(effective) else {
        return failed("Failed to read PXPIPE settings");
    };
    let mut response = runtime_status(&state.runtime);
    response["enabled"] = json!(truthy(&settings["pxpipeEnabled"]));
    response["autoInstall"] = json!(truthy(&settings["pxpipeAutoInstall"]));
    response["minChars"] = settings["pxpipeMinChars"].clone();
    response["timeoutMs"] = settings["pxpipeTimeoutMs"].clone();
    reply(StatusCode::OK, response, false)
}

async fn health(State(state): State<Pxpipe>) -> Response {
    reply(StatusCode::OK, native_health(&state.runtime).await, false)
}

async fn native_health(runtime: &Runtime) -> Value {
    let installed = runtime.installed();
    let mut checks = vec![json!({
        "id":"installed","label":"PXPIPE installed","ok":installed,
        "detail":installed.then(|| format!("v{}", pxpipe_runtime::VERSION)),
    })];
    if !installed {
        return json!({"healthy":false,"checks":checks,"error":"pxpipe not installed"});
    }
    runtime.start();
    checks.push(json!({
        "id":"module","label":"Transform module loads","ok":true,
        "detail": format!("v{}", pxpipe_runtime::VERSION),
    }));
    let started = Instant::now();
    let healthy = tokio::task::spawn_blocking(|| {
        crate::pxpipe_render::render("PXPIPE health probe", &Default::default()).is_ok_and(
            |pages| {
                pages.len() == 1 && pages[0].dropped == 0 && pages[0].png.starts_with(b"\x89PNG")
            },
        )
    })
    .await
    .is_ok_and(|rendered| rendered);
    let detail = if healthy {
        format!("{}ms (rendered)", started.elapsed().as_millis())
    } else {
        "Native PXPIPE self-test failed".to_owned()
    };
    checks.push(
        json!({"id":"transform","label":"Test request transforms","ok":healthy,"detail":detail}),
    );
    json!({"healthy":healthy,"checks":checks,"error":(!healthy).then_some(detail)})
}

async fn native_install(runtime: Runtime) -> bool {
    tokio::task::spawn_blocking(move || runtime.install())
        .await
        .is_ok_and(|result| result.is_ok())
}

async fn install(State(state): State<Pxpipe>) -> Response {
    if !native_install(state.runtime.clone()).await {
        return failed("Unable to install native PXPIPE");
    }
    state.runtime.stop();
    let health = native_health(&state.runtime).await;
    reply(
        StatusCode::OK,
        json!({
            "installed":true,"version":pxpipe_runtime::VERSION,
            "path":state.runtime.directory(),"health":health,
        }),
        false,
    )
}

async fn start(State(state): State<Pxpipe>) -> Response {
    if !state.runtime.installed() {
        let Some(settings) = state.storage.read_settings().await.map(effective) else {
            return failed("Failed to read PXPIPE settings");
        };
        if !truthy(&settings["pxpipeAutoInstall"]) {
            return unavailable(StatusCode::CONFLICT);
        }
        if !native_install(state.runtime.clone()).await {
            return failed("Unable to install native PXPIPE");
        }
    }
    state.runtime.start();
    reply(StatusCode::OK, runtime_status(&state.runtime), false)
}

async fn stop(State(state): State<Pxpipe>) -> Response {
    let mut response = json!({"stopped":state.runtime.stop()});
    if let (Some(output), Some(status)) = (
        response.as_object_mut(),
        runtime_status(&state.runtime).as_object(),
    ) {
        output.extend(status.clone());
    }
    reply(StatusCode::OK, response, false)
}

async fn restart(State(state): State<Pxpipe>) -> Response {
    if !state.runtime.installed() {
        return unavailable(StatusCode::INTERNAL_SERVER_ERROR);
    }
    state.runtime.stop();
    state.runtime.start();
    reply(StatusCode::OK, runtime_status(&state.runtime), false)
}

async fn stats(State(state): State<Pxpipe>, request: Request) -> Response {
    let directory = state.runtime.directory().to_owned();
    let limit = events::limit(request.uri().query());
    match tokio::task::spawn_blocking(move || events::stats(&directory, limit)).await {
        Ok(Ok(value)) => reply(StatusCode::OK, value, false),
        Ok(Err(error)) => failed(error),
        Err(_) => failed("Failed to read PXPIPE statistics"),
    }
}

async fn logs(State(state): State<Pxpipe>, request: Request) -> Response {
    let directory = state.runtime.directory().to_owned();
    let limit = events::limit(request.uri().query());
    match tokio::task::spawn_blocking(move || events::logs(&directory, limit)).await {
        Ok(value) => reply(StatusCode::OK, value, false),
        Err(_) => failed("Failed to read PXPIPE logs"),
    }
}
