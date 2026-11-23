use anthropic_token_count_rs::port::estimate_anthropic_input_tokens;
use axum::Router;
use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use serde_json::{Value, json};

use crate::observed_response::into_http;

const BODY_LIMIT: usize = 128 * 1024 * 1024;

pub fn router() -> Router {
    [
        "/v1/messages/count_tokens",
        "/v1/v1/messages/count_tokens",
        "/api/v1/messages/count_tokens",
    ]
    .into_iter()
    .fold(Router::new(), |router, path| {
        router.route(path, post(count).options(options))
    })
}

async fn options() -> Response {
    reply(StatusCode::OK, None)
}

async fn count(request: Request) -> Response {
    let bytes = to_bytes(request.into_body(), BODY_LIMIT).await.ok();
    let Some(body) = bytes.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) else {
        return reply(
            StatusCode::BAD_REQUEST,
            Some(json!({ "error": "Invalid JSON body" })),
        );
    };
    match estimate_anthropic_input_tokens(body) {
        Ok(tokens) => reply(StatusCode::OK, Some(json!({ "input_tokens": tokens }))),
        Err(_) => into_http(Ok(json!({"status": 500, "headers": {}, "body": ""}))),
    }
}

fn reply(status: StatusCode, body: Option<Value>) -> Response {
    let mut headers = json!({
        "access-control-allow-origin": "*",
        "access-control-allow-methods": "POST, OPTIONS",
        "access-control-allow-headers": "*",
    });
    let body = body.map_or(String::new(), |value| {
        headers["content-type"] = json!("application/json");
        value.to_string()
    });
    into_http(Ok(
        json!({"status": status.as_u16(), "headers": headers, "body": body}),
    ))
}
