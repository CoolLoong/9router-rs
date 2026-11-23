use std::fmt;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use crate::dashboard_session;
use crate::storage::Storage;

#[path = "antigravity_oauth.rs"]
mod antigravity;
#[path = "provider_oauth_cline.rs"]
mod cline;
#[path = "codex_oauth_callback.rs"]
mod codex_callback;
#[path = "oauth_device_attempts.rs"]
pub(crate) mod device_attempts;
#[path = "provider_oauth_flow.rs"]
mod flow;
#[path = "grok_cli_refresh.rs"]
mod grok;
#[path = "grok_cli_device.rs"]
mod grok_device;
#[path = "kimi_refresh.rs"]
pub(crate) mod kimi;
#[path = "kimi_portal.rs"]
pub(crate) mod kimi_portal;
#[path = "kiro_refresh.rs"]
mod kiro;
#[path = "minimax_code_oauth.rs"]
pub(crate) mod minimax_code;
#[path = "native_oauth_callback.rs"]
mod native_callback;
#[path = "nous_refresh.rs"]
mod nous;
#[path = "nous_portal.rs"]
pub(crate) mod nous_portal;
#[path = "provider_oauth_refresh.rs"]
mod refresh;
#[path = "provider_oauth_store.rs"]
mod store;
#[path = "trae_oauth.rs"]
mod trae;
#[path = "windsurf_import.rs"]
pub(crate) mod windsurf;
#[path = "xai_oauth.rs"]
mod xai;
#[path = "zed_oauth.rs"]
mod zed;

pub(crate) use flow::token_claims;
pub(crate) use grok::ATTEMPT as GROK_CLI_REFRESH_ATTEMPT;
pub(crate) use kiro::ATTEMPT as KIRO_REFRESH_ATTEMPT;
pub(crate) use kiro::SOCIAL as KIRO_SOCIAL_REFRESH;
pub(crate) use kiro::{
    supports as kiro_refresh_supported, supports_relay as kiro_relay_refresh_supported,
    test_expiry as test_kiro_expiry, token_endpoint as kiro_token_endpoint,
};
pub(crate) use nous::{
    ATTEMPT as NOUS_REFRESH_ATTEMPT, can_refresh as nous_refresh_supported,
    compatible as nous_connection_compatible, inference_token as nous_inference_token,
    redact as redact_nous_error,
};
pub use refresh::{RefreshMode, refresh_account};
pub(crate) use refresh::{
    background_candidate, nous_refresh_due, refresh_due_for_provider, refresh_xai_inference,
    refresh_xai_routed, test_token_url,
};

pub(crate) fn supports_chat_credentials(provider: &str, account: &Value) -> bool {
    let access = if provider == "github" {
        crate::github_executor::token(account).is_some()
    } else {
        account["accessToken"]
            .as_str()
            .is_some_and(|token| !token.is_empty())
    };
    match (provider, account["authType"].as_str()) {
        ("codex" | "gemini-cli" | "github", Some("oauth")) => {
            access
                || account["refreshToken"]
                    .as_str()
                    .is_some_and(|token| !token.is_empty())
        }
        ("codex", Some("access_token")) => access,
        _ => false,
    }
}

pub(crate) async fn save_imported(
    storage: &Storage,
    provider: &'static str,
    fields: serde_json::Map<String, Value>,
) -> Result<Value, ()> {
    store::save_connection(storage, provider, None, fields, chrono::Utc::now())
        .await
        .map_err(|_| ())
}

use flow::{ApiError, Config, OAuth, Operation, Provider};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshError {
    Unsupported,
    AccountUnavailable,
    Storage,
    ProviderUnavailable,
    ReauthenticationRequired,
}

impl fmt::Display for RefreshError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(match self {
            Self::Unsupported => "This account type does not support native refresh",
            Self::AccountUnavailable => "The provider account is unavailable",
            Self::Storage => "Could not update stored provider credentials",
            Self::ProviderUnavailable => "Provider credential refresh failed",
            Self::ReauthenticationRequired => "The provider account must sign in again",
        })
    }
}

impl std::error::Error for RefreshError {}

#[derive(Clone)]
struct OAuthState {
    storage: Storage,
    flow: OAuth,
    callback: codex_callback::Proxy,
    xai_callback: codex_callback::Proxy,
}

pub fn router(storage: Storage) -> Router {
    let config = Config::pinned().expect("pinned provider OAuth configuration");
    build_router(
        storage,
        OAuth::new(config).expect("provider OAuth HTTP client"),
    )
}

fn build_router(storage: Storage, flow: OAuth) -> Router {
    let minimax_code = minimax_code::router(storage.clone());
    let trae = trae::router(storage.clone());
    let windsurf = windsurf::router(storage.clone());
    let zed = zed::router(storage.clone());
    let grok_device = grok_device::router(storage.clone());
    Router::new()
        .route(
            "/api/oauth/{provider}/{action}",
            get(dispatch).post(dispatch).options(oauth_options),
        )
        .merge(codex_callback::routes(codex_callback::Loopback::Codex))
        .merge(codex_callback::routes(codex_callback::Loopback::Xai))
        .route("/callback", get(callback))
        .route("/__9router-ui/oauth-callback.js", get(callback_script))
        .with_state(OAuthState {
            storage,
            flow,
            callback: codex_callback::Proxy::default(),
            xai_callback: codex_callback::Proxy::default(),
        })
        .merge(minimax_code)
        .merge(trae)
        .merge(windsurf)
        .merge(zed)
        .merge(grok_device)
}

async fn callback() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; script-src 'self'; base-uri 'none'; frame-ancestors 'none'",
            ),
        ],
        include_str!("../assets/native-web/oauth-callback.html"),
    )
        .into_response()
}

async fn callback_script() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/native-web/oauth-callback.js"),
    )
        .into_response()
}

async fn dispatch(
    State(state): State<OAuthState>,
    Path((provider, action)): Path<(String, String)>,
    request: Request,
) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return reply(StatusCode::UNAUTHORIZED, json!({"error": "Unauthorized"}));
    }
    let method = if request.method() == Method::HEAD {
        Method::GET
    } else {
        request.method().clone()
    };
    let operation = Operation::identify(&provider, &method, &action);
    let query = request.uri().query().unwrap_or_default().to_owned();
    let body = if method == Method::POST {
        let raw = to_bytes(request.into_body(), 64 * 1024).await.ok();
        let body = raw
            .as_ref()
            .and_then(|body| serde_json::from_slice::<Value>(body).ok())
            .filter(Value::is_object);
        let Some(body) = body else {
            return reply(
                StatusCode::BAD_REQUEST,
                json!({"error": "Invalid or empty request body"}),
            );
        };
        if matches!(operation, Some((Provider::Codex, Operation::Exchange)))
            && body["state"].is_null()
            && let Some(token) = body["code"]
                .as_str()
                .filter(|code| looks_like_compact_token(code))
        {
            return match crate::codex_import::exchange_access_token(&state.storage, token).await {
                Ok(result) => reply(StatusCode::OK, result),
                Err(message) => reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":message})),
            };
        }
        body
    } else {
        Value::Null
    };
    let Some((provider, operation)) = operation else {
        return other_action(&provider, &method, &action, &query, &body);
    };
    let result = handle(
        &state.storage,
        &state.flow,
        provider,
        operation,
        &query,
        &body,
    )
    .await;
    match result {
        Ok(body) => reply(StatusCode::OK, body),
        Err(error) => reply(error.status, json!({"error": error.message})),
    }
}

async fn oauth_options() -> Response {
    (
        StatusCode::NO_CONTENT,
        [(header::ALLOW, "GET, HEAD, OPTIONS, POST")],
    )
        .into_response()
}

fn device_provider(provider: &str) -> bool {
    matches!(
        provider,
        "github"
            | "grok-cli"
            | "muse"
            | "kiro"
            | "qoder"
            | "qoder-cn"
            | "kimi"
            | "kimi-coding"
            | "kilocode"
            | "codebuddy-cn"
            | "codebuddy-intl"
            | "glm"
            | "minimax-code"
            | "minimax-code-global"
            | "nous"
    )
}

fn redirect_uri(query: &str) -> std::borrow::Cow<'_, str> {
    url::form_urlencoded::parse(query.as_bytes())
        .find_map(|(name, value)| (name == "redirect_uri" && !value.is_empty()).then_some(value))
        .unwrap_or(std::borrow::Cow::Borrowed("http://localhost:8080/callback"))
}

fn other_action(
    provider: &str,
    method: &Method,
    action: &str,
    query: &str,
    body: &Value,
) -> Response {
    use crate::provider_connections::truthy;
    let known = device_provider(provider)
        || Provider::from_id(provider).is_some()
        || matches!(
            provider,
            "cursor" | "gitlab" | "kimchi" | "zed" | "windsurf" | "xiaomi-mimo"
        );
    let query_has_state = || {
        url::form_urlencoded::parse(query.as_bytes())
            .any(|(key, value)| key == "state" && !value.is_empty())
    };
    let result = match (method, action) {
        (&Method::GET, "authorize") if device_provider(provider) => {
            flow::device_authorization(&redirect_uri(query))
        }
        (&Method::GET, "authorize") if provider == "cursor" => {
            Err(ApiError::bad("Cursor uses token import"))
        }
        (&Method::GET, "start-proxy") => Err(ApiError::bad(
            "Proxy only supported for codex/xai/trae/windsurf/zed",
        )),
        (&Method::GET, "poll-status") if !query_has_state() => Err(ApiError::bad("Missing state")),
        (&Method::GET, "poll-status") => Err(ApiError::bad(
            "Poll only supported for codex/xai/trae/windsurf/zed/xiaomi-mimo",
        )),
        (&Method::GET, "stop-proxy") => Err(ApiError::bad(
            "Proxy only supported for codex/xai/trae/windsurf/zed/xiaomi-mimo",
        )),
        (&Method::GET, "ide-status") => {
            Err(ApiError::bad("ide-status only supported for trae/windsurf"))
        }
        (&Method::GET, "lease-status") => Err(ApiError::bad(
            "lease-status only supported for minimax-code/minimax-code-global",
        )),
        (&Method::POST, "lease-connect") => Err(ApiError::bad(
            "lease-connect only supported for minimax-code/minimax-code-global",
        )),
        (&Method::POST, "register-session") if !query_has_state() && !truthy(&body["state"]) => {
            Err(ApiError::bad("Missing state"))
        }
        (&Method::POST, "register-session") => Err(ApiError::bad(
            "register-session only supported for trae/windsurf/zed",
        )),
        (&Method::POST, "manual-code") => Err(ApiError::bad("Manual code only supported for xai")),
        (&Method::POST, "poll") if !truthy(&body["deviceCode"]) => {
            Err(ApiError::bad("Missing device code"))
        }
        (&Method::POST, "poll") if !truthy(&body["codeVerifier"]) => {
            Err(ApiError::bad("Missing code verifier"))
        }
        (&Method::POST, "exchange")
            if !truthy(&body["code"])
                || !truthy(&body["redirectUri"])
                || !truthy(&body["codeVerifier"]) =>
        {
            Err(ApiError::bad("Missing required fields"))
        }
        (&Method::GET, "authorize" | "device-code") | (&Method::POST, "exchange" | "poll")
            if !known =>
        {
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":format!("Unknown provider: {provider}")}),
            );
        }
        (&Method::GET, "device-code") => {
            Err(ApiError::bad("Provider does not support device code flow"))
        }
        (&Method::POST, "poll") => Err(ApiError::failed(
            "Provider does not support device code flow",
        )),
        (&Method::POST, "exchange") => Err(ApiError::bad(
            "Provider does not support authorization code flow",
        )),
        _ => Err(ApiError::bad("Unknown action")),
    };
    match result {
        Ok(body) => reply(StatusCode::OK, body),
        Err(error) => reply(error.status, json!({"error":error.message})),
    }
}

async fn handle(
    storage: &Storage,
    flow: &OAuth,
    provider: Provider,
    operation: Operation,
    query: &str,
    body: &Value,
) -> Result<Value, ApiError> {
    match operation {
        Operation::Authorize => flow.authorize(provider, &redirect_uri(query)).await,
        Operation::DeviceCode if provider == Provider::GitHub => flow.device_code().await,
        Operation::DeviceCode => Err(ApiError::bad("Provider does not support device code flow")),
        Operation::Exchange => {
            let (credentials, onboarding) = flow.exchange(storage, provider, body).await?;
            let connection = store::save(storage, provider, credentials).await?;
            if let Some(tier) = onboarding {
                antigravity::onboard(
                    storage.clone(),
                    flow.antigravity_onboard().to_owned(),
                    &connection,
                    tier,
                );
            }
            let mut summary = json!({"id": connection["id"], "provider": provider.id()});
            for name in ["email", "displayName"] {
                if let Some(value) = connection.get(name) {
                    summary[name] = value.clone();
                }
            }
            Ok(json!({"success": true, "connection": summary}))
        }
        Operation::Poll => match flow.poll(body).await? {
            flow::Poll::Status(status) => Ok(status),
            flow::Poll::Complete(credentials) => {
                match store::save(storage, provider, credentials).await {
                    Ok(connection) => Ok(json!({
                        "success": true, "connection": {"id": connection["id"], "provider": provider.id()}
                    })),
                    Err(_) => Ok(flow::failed_device_exchange(
                        "Could not save the provider connection",
                    )),
                }
            }
        },
    }
}

fn looks_like_compact_token(code: &str) -> bool {
    code.starts_with("eyJ") && code.split('.').filter(|part| !part.is_empty()).count() == 3
}

fn reply(status: StatusCode, body: Value) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    headers.insert(
        header::VARY,
        header::HeaderValue::from_static("Accept-Encoding"),
    );
    response
}
