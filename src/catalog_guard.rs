use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{Extensions, HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use subtle::ConstantTimeEq;
use url::Url;

use crate::storage::Storage;

pub async fn enforce_gateway_access(
    State(storage): State<Storage>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let catalog = matches!(
        path,
        "/v1beta/models"
            | "/api/v1beta/models"
            | "/v1"
            | "/api/v1"
            | "/v1/v1"
            | "/v1/models"
            | "/api/v1/models"
            | "/v1/v1/models"
            | "/v1/chat/completions"
            | "/api/v1/chat/completions"
            | "/v1/v1/chat/completions"
            | "/v1/messages"
            | "/api/v1/messages"
            | "/v1/v1/messages"
            | "/v1/messages/count_tokens"
            | "/api/v1/messages/count_tokens"
            | "/v1/v1/messages/count_tokens"
            | "/v1/responses"
            | "/api/v1/responses"
            | "/v1/v1/responses"
            | "/responses"
            | "/codex"
            | "/v1/responses/compact"
            | "/api/v1/responses/compact"
            | "/v1/v1/responses/compact"
            | "/v1/embeddings"
            | "/api/v1/embeddings"
            | "/v1/v1/embeddings"
            | "/v1/audio/speech"
            | "/api/v1/audio/speech"
            | "/v1/v1/audio/speech"
            | "/v1/audio/voices"
            | "/api/v1/audio/voices"
            | "/v1/v1/audio/voices"
            | "/v1/audio/transcriptions"
            | "/api/v1/audio/transcriptions"
            | "/v1/v1/audio/transcriptions"
            | "/v1/images/generations"
            | "/api/v1/images/generations"
            | "/v1/v1/images/generations"
            | "/v1/search"
            | "/api/v1/search"
            | "/v1/v1/search"
            | "/v1/web/fetch"
            | "/api/v1/web/fetch"
            | "/v1/v1/web/fetch"
            | "/systemone"
            | "/v1/systemone"
            | "/api/v1/systemone"
            | "/v1/v1/systemone"
    ) || crate::ollama_public::PATHS.contains(&path)
        || path.starts_with("/v1/models/")
        || path.starts_with("/api/v1/models/")
        || path.starts_with("/v1/v1/models/")
        || path.starts_with("/codex/")
        || ["/v1/videos/", "/api/v1/videos/", "/v1/v1/videos/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
        || crate::gemini_public::PATHS
            .iter()
            .any(|prefix| path.starts_with(prefix));
    if !catalog || is_direct_loopback(&req) || valid_cli_token(&storage, req.headers()) {
        return next.run(req).await;
    }

    if let Some(key) = api_key(&req)
        && storage.api_key_is_active(key).await
    {
        return next.run(req).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        Json(json!({ "error": "API key required for remote API access" })),
    )
        .into_response()
}

pub(crate) fn is_direct_loopback(req: &Request) -> bool {
    is_direct_loopback_parts(req.headers(), req.extensions())
}

pub(crate) fn is_direct_loopback_parts(headers: &HeaderMap, extensions: &Extensions) -> bool {
    let peer = extensions.get::<ConnectInfo<SocketAddr>>();
    if !peer.is_some_and(|peer| peer.0.ip().to_canonical().is_loopback()) {
        return false;
    }
    if ["x-forwarded-for", "x-real-ip"].iter().any(|name| {
        headers
            .get_all(*name)
            .iter()
            .any(|value| !value.as_bytes().is_empty())
    }) {
        return false;
    }

    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    if origin.as_bytes().is_empty() {
        return true;
    }
    origin
        .to_str()
        .ok()
        .and_then(|origin| Url::parse(origin).ok())
        .is_some_and(|url| matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
}

pub fn api_key(req: &Request) -> Option<String> {
    let headers = req.headers();
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    {
        return Some(token.to_owned());
    }
    for name in ["x-api-key", "x-goog-api-key"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok())
            && !value.is_empty()
        {
            return Some(value.to_owned());
        }
    }
    url::form_urlencoded::parse(req.uri().query()?.as_bytes())
        .find_map(|(name, value)| (name == "key").then(|| value.into_owned()))
}

pub fn valid_cli_token(storage: &Storage, headers: &HeaderMap) -> bool {
    let Some(token) = headers.get("x-9r-cli-token") else {
        return false;
    };
    crate::key_identity::cli_token(storage.root())
        .is_some_and(|expected| bool::from(expected.as_bytes().ct_eq(token.as_bytes())))
}
