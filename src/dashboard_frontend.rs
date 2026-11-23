use axum::Json;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use percent_encoding::percent_decode_str;
use serde_json::json;

use super::{protected, redirect};
use crate::{key_identity, storage::Storage};

const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self' blob:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";
include!(concat!(env!("OUT_DIR"), "/dashboard_assets.rs"));

pub(super) fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/dashboard/context", get(context))
        .with_state(storage)
        .route("/__dashboard/{*asset}", get(serve_asset))
        .route("/providers/{*asset}", get(serve_asset))
        .route("/i18n/literals/{*asset}", get(serve_asset))
        .route("/icons/{*asset}", get(serve_asset))
        .route("/favicon.svg", get(serve_asset))
        .route("/favicon.ico", get(serve_asset))
        .route("/manifest.webmanifest", get(serve_asset))
}

fn asset(path: &str) -> Option<&'static [u8]> {
    FILES
        .binary_search_by(|(name, _)| name.cmp(&path))
        .ok()
        .map(|index| FILES[index].1)
}

async fn serve_asset(request: Request) -> Response {
    let Ok(path) = percent_decode_str(request.uri().path()).decode_utf8() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let staged = if let Some(path) = path.strip_prefix("/__dashboard/") {
        if !(path.starts_with("assets/") || path.starts_with("monaco/")) {
            return StatusCode::NOT_FOUND.into_response();
        }
        path.to_owned()
    } else {
        format!("public{path}")
    };
    let Some(body) = asset(&staged) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = mime_guess::from_path(path.as_ref()).first_or_octet_stream();
    let content_type = if path == "/manifest.webmanifest" {
        "application/manifest+json"
    } else {
        mime.essence_str()
    };
    let cache = if staged.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

async fn context(State(storage): State<Storage>, request: Request) -> Response {
    if protected(&storage, request.headers()).await.is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Unauthorized"})),
        )
            .into_response();
    }
    let Ok(machine_id) = key_identity::machine_id(storage.root()) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"Could not load the dashboard context"})),
        )
            .into_response();
    };
    (
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(json!({"machineId": machine_id})),
    )
        .into_response()
}

pub(super) async fn render(
    State(storage): State<Storage>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let native_page = matches!(path, "/dashboard/models" | "/dashboard/updates")
        || path.starts_with("/dashboard/providers/nodes/")
        || path.starts_with("/dashboard/providers/connections/");
    if request.method() != Method::GET
        || native_page
        || !(path == "/dashboard" || path.starts_with("/dashboard/"))
    {
        return next.run(request).await;
    }
    if let Err(path) = protected(&storage, request.headers()).await {
        return redirect(path);
    }
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, POLICY),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        asset("index.html").expect("embedded dashboard entry page"),
    )
        .into_response()
}
