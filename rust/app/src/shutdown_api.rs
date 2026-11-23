use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::sync::Notify;

use crate::catalog_guard::valid_cli_token;
use crate::dashboard_session::has_session;
use crate::{settings, storage::Storage};

pub fn router(storage: Storage, signal: Arc<Notify>) -> Router {
    Router::new()
        .route("/api/shutdown", post(developer_shutdown))
        .route("/api/version/shutdown", post(version_shutdown))
        .route_layer(middleware::from_fn_with_state(storage, authorize))
        .with_state(signal)
}

async fn authorize(State(storage): State<Storage>, req: Request, next: Next) -> Response {
    if valid_cli_token(&storage, req.headers()) || has_session(&storage, req.headers()) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::VARY, "Accept-Encoding")],
            Json(json!({"error":"Unauthorized"})),
        )
            .into_response()
    }
}

async fn developer_shutdown(State(signal): State<Arc<Notify>>, request: Request) -> Response {
    if std::env::var("NODE_ENV").as_deref() != Ok("development") {
        return reply(StatusCode::FORBIDDEN, false, "Not allowed in production");
    }
    let authorized = std::env::var("SHUTDOWN_SECRET").is_ok_and(|secret| {
        !secret.is_empty()
            && request
                .headers()
                .get(header::AUTHORIZATION)
                .is_some_and(|value| {
                    bool::from(
                        format!("Bearer {secret}")
                            .as_bytes()
                            .ct_eq(value.as_bytes()),
                    )
                })
    });
    if !authorized {
        return reply(StatusCode::UNAUTHORIZED, false, "Unauthorized");
    }
    shutdown(signal);
    reply(StatusCode::OK, true, "Shutting down...")
}

async fn version_shutdown(State(signal): State<Arc<Notify>>) -> Response {
    shutdown(signal);
    reply(StatusCode::OK, true, "Shutting down for manual update...")
}

fn shutdown(signal: Arc<Notify>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        signal.notify_one();
    });
}

fn reply(status: StatusCode, success: bool, message: &str) -> Response {
    settings::reply(status, json!({"success":success,"message":message}), false)
}
