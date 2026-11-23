use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::{SecondsFormat, TimeDelta, Utc};
use reqwest::{Client, Url};
use serde_json::{Map, Value, json};

use crate::dashboard_session;
use crate::provider_connections::{object, truthy};
use crate::provider_index::js_trim;
use crate::provider_oauth::{save_imported, token_claims};
use crate::proxy_transport::{self, Route};
use crate::settings::reply;
use crate::storage::Storage;

#[path = "commandcode_import.rs"]
mod commandcode;
#[path = "cursor_auto_import.rs"]
mod cursor;
#[path = "gitlab_oauth.rs"]
mod gitlab_oauth;
#[path = "iflow_cookie_import.rs"]
mod iflow;
#[path = "kilocode_device.rs"]
mod kilocode_device;
pub(crate) use kilocode_device::{
    CHAT_ENDPOINT as KILOCODE_CHAT_ENDPOINT, chat_headers as kilocode_chat_headers,
};
#[path = "kiro_auto_import.rs"]
mod kiro;
#[path = "kiro_external_import.rs"]
mod kiro_external;
#[path = "kiro_social.rs"]
mod kiro_social;
#[path = "provider_xiaomi_mimo_import.rs"]
mod xiaomi_mimo;
pub(crate) use xiaomi_mimo::save_authorized as save_xiaomi_authorized;
pub(crate) use xiaomi_mimo::save_web_session as save_xiaomi_web_session;
#[path = "zed_import.rs"]
pub(crate) mod zed;
#[cfg(target_os = "linux")]
#[path = "zed_auto_import.rs"]
mod zed_auto;

const GITLAB_DEFAULT: &str = "https://gitlab.com";
const USER_LIMIT: usize = 64 * 1024;

pub(crate) fn xiaomi_cloud_key(account: &Value) -> Option<&str> {
    if account["provider"] != "xiaomi-mimo"
        || !matches!(account["authType"].as_str(), Some("apikey" | "api_key"))
            && !(account["authType"] == "oauth"
                && account["providerSpecificData"]["authMethod"] == "oauth")
    {
        return None;
    }
    [&account["apiKey"], &account["accessToken"]]
        .into_iter()
        .find_map(nonempty)
        .filter(|key| {
            key.strip_prefix("sk-")
                .is_some_and(|secret| !secret.is_empty())
                && key.bytes().all(|byte| byte.is_ascii_graphic())
        })
}

pub fn router(storage: Storage) -> Router {
    let router = Router::new()
        .route("/api/oauth/gitlab/pat", post(gitlab_pat))
        .route("/api/oauth/gitlab/authorize", get(gitlab_oauth::authorize))
        .route("/api/oauth/gitlab/exchange", post(gitlab_oauth::exchange))
        .route("/api/oauth/grok-cli/bulk-import", post(grok_bulk))
        .route("/api/oauth/iflow/cookie", post(iflow::import))
        .route(
            "/api/oauth/kilocode/device-code",
            get(kilocode_device::authorize),
        )
        .route("/api/oauth/kilocode/poll", post(kilocode_device::poll))
        .route("/api/oauth/xiaomi-mimo/api-key", post(xiaomi_mimo::import))
        .route(
            "/api/oauth/xiaomi-mimo/auto-import",
            get(xiaomi_mimo::read_local),
        )
        .route("/api/oauth/commandcode/auto-import", get(commandcode::read))
        .route("/api/oauth/cursor/auto-import", get(cursor::read))
        .route(
            "/api/oauth/cursor/import",
            get(cursor::instructions).post(cursor::import),
        )
        .route("/api/oauth/kiro/auto-import", get(kiro::read))
        .route(
            "/api/oauth/kiro/import-cli-proxy",
            post(kiro_external::import),
        )
        .route(
            "/api/oauth/kiro/social-authorize",
            get(kiro_social::authorize),
        )
        .route(
            "/api/oauth/kiro/social-exchange",
            post(kiro_social::exchange),
        )
        .route("/api/oauth/zed/import", post(zed::import));
    #[cfg(target_os = "linux")]
    let router = router.route("/api/oauth/zed/auto-import", get(zed_auto::read));
    router.with_state(storage)
}

fn error(status: StatusCode, text: impl Into<String>) -> Response {
    reply(status, json!({"error":text.into()}), true)
}

async fn body(storage: &Storage, request: Request) -> Result<Value, (StatusCode, &'static str)> {
    if !dashboard_session::authenticated(storage, request.headers()).await {
        return Err((StatusCode::UNAUTHORIZED, "Unauthorized"));
    }
    let decoded = to_bytes(request.into_body(), 8 * 1024 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    decoded.ok_or((StatusCode::BAD_REQUEST, "Invalid request body"))
}

async fn gitlab_pat(State(storage): State<Storage>, request: Request) -> Response {
    let input = match body(&storage, request).await {
        Ok(input) => input,
        Err((status, message)) => return error(status, message),
    };
    let token = input["token"]
        .as_str()
        .map(js_trim)
        .filter(|token| !token.is_empty());
    let Some(token) = token else {
        return error(StatusCode::BAD_REQUEST, "Personal Access Token is required");
    };
    let base = input["baseUrl"]
        .as_str()
        .map(js_trim)
        .filter(|base| !base.is_empty())
        .unwrap_or(GITLAB_DEFAULT)
        .trim_end_matches('/');
    if !Url::parse(base).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    }) {
        return error(StatusCode::BAD_REQUEST, "Invalid GitLab base URL");
    }
    let endpoint = format!("{base}/api/v4/user");
    let Some(settings) = storage.read_settings().await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read proxy configuration",
        );
    };
    let Ok(route) = proxy_transport::select(&storage, &settings, &Value::Null, &endpoint).await
    else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read proxy configuration",
        );
    };
    let user = match gitlab_user(&route, &endpoint, token).await {
        Ok(user) => user,
        Err((status, reason)) => return error(status, reason),
    };
    let email = nonempty(&user["email"])
        .or_else(|| nonempty(&user["public_email"]))
        .unwrap_or_default();
    let username = nonempty(&user["username"]).unwrap_or_default();
    let name = nonempty(&user["name"]).unwrap_or_default();
    let display = [name, username, email]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or_default();
    let record = json!({
        "accessToken":token,"refreshToken":null,"expiresAt":null,
        "email":email,"displayName":display,
        "providerSpecificData":{"username":username,"email":email,"name":name,"baseUrl":base,"authKind":"personal_access_token"},
    });
    match save_imported(&storage, "gitlab", object(record)).await {
        Ok(_) => reply(StatusCode::OK, json!({"success":true}), true),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save the provider connection",
        ),
    }
}

type GitlabError = (StatusCode, String);

async fn gitlab_user(route: &Route, endpoint: &str, token: &str) -> Result<Value, GitlabError> {
    let failure = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "GitLab token verification failed".to_owned(),
        )
    };
    let mut token_header = HeaderValue::from_str(token)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid GitLab token".into()))?;
    token_header.set_sensitive(true);
    let client = metadata_client(route, Duration::from_secs(15)).ok_or_else(failure)?;
    let target = match route {
        Route::Relay { url, .. } => url,
        _ => endpoint,
    };
    let mut response = route
        .relay(
            client
                .get(target)
                .header("Private-Token", token_header)
                .header(reqwest::header::ACCEPT, "application/json"),
        )
        .send()
        .await
        .map_err(|_| failure())?;
    let status = response.status();
    let bytes = metadata_bytes(&mut response).await.ok_or_else(failure)?;
    if !status.is_success() {
        let detail = String::from_utf8_lossy(&bytes).replace(token, "[redacted]");
        let detail: String = detail
            .chars()
            .filter(|ch| !ch.is_control())
            .take(256)
            .collect();
        return Err((
            StatusCode::UNAUTHORIZED,
            format!("GitLab token verification failed: {detail}"),
        ));
    }
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(failure)
}

pub(crate) fn metadata_client(route: &Route, timeout: Duration) -> Option<Client> {
    let mut builder = Client::builder()
        .no_proxy()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    if let Route::Proxy { url, .. } = route {
        builder = builder.proxy(reqwest::Proxy::all(url).ok()?);
    } else if matches!(route, Route::MissingStrict) {
        return None;
    }
    builder.build().ok()
}

async fn metadata_bytes(response: &mut reqwest::Response) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > USER_LIMIT {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}

async fn grok_bulk(State(storage): State<Storage>, request: Request) -> Response {
    let input = match body(&storage, request).await {
        Ok(input) => input,
        Err((status, message)) => return error(status, message),
    };
    let accounts = if let Some(accounts) = input.as_array().or_else(|| input["accounts"].as_array())
    {
        accounts.as_slice()
    } else if input.is_object() {
        std::slice::from_ref(&input)
    } else {
        &[]
    };
    if accounts.is_empty() {
        return error(StatusCode::BAD_REQUEST, "No accounts provided");
    }
    let mut results = Vec::with_capacity(accounts.len());
    let mut success = 0;
    for (index, account) in accounts.iter().enumerate() {
        let result = match grok_account(account) {
            Ok(fields) => save_imported(&storage, "grok-cli", fields)
                .await
                .map_err(|_| "Could not save the provider connection"),
            Err(reason) => Err(reason),
        };
        results.push(match result {
            Ok(account) => {
                success += 1;
                json!({"index":index,"ok":true,"id":account["id"],"email":account["email"]})
            }
            Err(reason) => json!({"index":index,"ok":false,"error":reason}),
        });
    }
    reply(
        StatusCode::OK,
        json!({"total":accounts.len(),"success":success,"failed":accounts.len()-success,"results":results}),
        true,
    )
}

fn grok_account(input: &Value) -> Result<Map<String, Value>, &'static str> {
    if !input.is_object() {
        return Err("Item is not an object");
    }
    let pick = |first: &str, second: &str| {
        input
            .get(first)
            .filter(|value| truthy(value))
            .or_else(|| input.get(second).filter(|value| truthy(value)))
    };
    let access = pick("access_token", "accessToken")
        .and_then(nonempty)
        .ok_or("Missing access_token / accessToken")?;
    let refresh = pick("refresh_token", "refreshToken")
        .cloned()
        .unwrap_or_default();
    let id = pick("id_token", "idToken").cloned().unwrap_or_default();
    let email = nonempty(&input["email"])
        .map(str::to_owned)
        .or_else(|| inferred_email(id.as_str()).or_else(|| inferred_email(Some(access))));
    let expires = if let Some(expires) = pick("expires_at", "expiresAt") {
        expires.clone()
    } else if let Some(seconds) = pick("expires_in", "expiresIn")
        .and_then(Value::as_f64)
        .filter(|seconds| *seconds > 0.0)
    {
        let duration = TimeDelta::try_milliseconds((seconds * 1000.0).trunc() as i64)
            .ok_or("Invalid time value")?;
        let expires = Utc::now()
            .checked_add_signed(duration)
            .ok_or("Invalid time value")?;
        json!(expires.to_rfc3339_opts(SecondsFormat::Millis, true))
    } else {
        Value::Null
    };
    let mut details = object(json!({"authMethod":"device_code"}));
    if !id.is_null() {
        details.insert("idToken".into(), id);
    }
    if let Some(email) = &email {
        details.insert("email".into(), json!(email));
    }
    details.extend(object(input["providerSpecificData"].clone()));
    let mut fields = object(json!({
        "accessToken":access,"refreshToken":refresh,"expiresAt":expires,
        "email":email,"providerSpecificData":details,
    }));
    if let Some(display) = pick("displayName", "name") {
        fields.insert("displayName".into(), display.clone());
    }
    Ok(fields)
}

fn inferred_email(token: Option<&str>) -> Option<String> {
    let claims = token_claims(token?);
    ["email", "preferred_username", "sub"]
        .iter()
        .find_map(|key| nonempty(&claims[*key]).map(str::to_owned))
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}
