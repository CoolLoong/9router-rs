use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::{
    dashboard_session, observed_response::into_http, pricing_api, settings, storage::Storage,
};

const LOCALES: &str = include_str!("../../../src/i18n/config.js");
pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/pricing", pricing_api::routes())
        .route("/api/tags", get(tags).options(tags_options))
        .route_layer(middleware::from_fn_with_state(storage.clone(), authorize))
        .route("/api/locale", post(locale))
        .route("/api/init", get(init))
        .with_state(storage)
}

async fn authorize(State(storage): State<Storage>, req: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&storage, req.headers()).await {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::VARY, "Accept-Encoding")],
            Json(json!({"error": "Unauthorized"})),
        )
            .into_response()
    }
}

async fn init() -> Response {
    into_http(Ok(json!({
        "status": 200, "headers": {"content-type": "text/plain;charset=UTF-8"},
        "body": "Initialized",
    })))
}

async fn locale(request: Request) -> Response {
    let body = to_bytes(request.into_body(), 128 * 1024 * 1024).await;
    let Some(value) = body
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .filter(|value| !value.is_null())
    else {
        return settings::reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"Failed to set locale"}),
            false,
        );
    };
    let supported = LOCALES
        .split_once("export const LOCALES = [")
        .and_then(|(_, list)| list.split_once("];"))
        .map_or("", |(list, _)| list);
    let Some(locale) = value["locale"].as_str().filter(|locale| {
        supported.lines().any(|line| {
            line.trim()
                .strip_prefix('"')
                .and_then(|line| line.strip_suffix("\","))
                .is_some_and(|value| value == *locale)
        })
    }) else {
        return settings::reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"Invalid locale"}),
            false,
        );
    };
    let expiry =
        (chrono::Utc::now() + chrono::Duration::days(365)).format("%a, %d %b %Y %H:%M:%S GMT");
    let cookie = format!("locale={locale}; Path=/; Expires={expiry}; Max-Age=31536000");
    let mut output = settings::reply(
        StatusCode::OK,
        json!({"success":true,"locale":locale}),
        false,
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        output.headers_mut().append(header::SET_COOKIE, value);
    }
    output
}

async fn tags() -> Response {
    cors(Some(include_str!("ollama-tags.json").trim_end()))
}

async fn tags_options() -> Response {
    cors(None)
}

fn cors(body: Option<&'static str>) -> Response {
    let mut headers = json!({
        "access-control-allow-origin":"*", "access-control-allow-methods":"GET, OPTIONS",
        "access-control-allow-headers":"*",
    });
    if body.is_some() {
        headers["content-type"] = json!("application/json");
    }
    into_http(Ok(
        json!({"status":200, "headers": headers, "body":body.unwrap_or_default()}),
    ))
}
