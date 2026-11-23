use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use reqwest::header::{self, HeaderValue};
use reqwest::{Client, ClientBuilder, Method, Response, StatusCode};
use serde_json::{Map, Value, json};

use crate::provider_connections::truthy;
use crate::provider_http::{self, ProtectedEgress};
use crate::provider_index::js_trim;
use crate::provider_oauth::{RefreshMode, refresh_account, refresh_due_for_provider};
use crate::proxy_transport;
use crate::storage::Storage;

const ORIGIN: &str = "https://api.cline.bot";
const CHAT: &str = "/api/v1/chat/completions";
const MODELS: &str = "/api/v1/models";
const RECOMMENDED: &str = "/api/v1/ai/cline/recommended-models";
const MODEL_TIMEOUT: Duration = Duration::from_secs(5);
const MODEL_LIMIT: usize = 2 * 1024 * 1024;

static VERSION: LazyLock<String> = LazyLock::new(|| {
    serde_json::from_str::<Value>(include_str!("../../../package.json"))
        .ok()
        .and_then(|package| package["version"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "0.0.0".into())
});

static JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+").expect("Cline token pattern")
});

static MODEL_CLIENT: LazyLock<Option<reqwest::Client>> =
    LazyLock::new(|| client_builder().timeout(MODEL_TIMEOUT).build().ok());

fn client_builder() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

pub fn is_provider(provider: &str) -> bool {
    matches!(provider, "cline" | "clinepass")
}

pub(super) fn typed_account(provider: &str, account: &Value) -> bool {
    is_provider(provider)
        && (account["authType"] == "oauth"
            || provider == "clinepass" && account["authType"] == "apikey")
        && saved_token(account).and_then(authorization).is_some()
}

pub(super) async fn egress(
    storage: &Storage,
    settings: &Value,
    account: &Value,
) -> Result<Option<ProtectedEgress>, &'static str> {
    provider_http::protected_egress(storage, settings, account, ORIGIN)
        .await
        .map_err(|_| "Cline proxy is unavailable")
}

pub(super) async fn protects_chat(storage: &Storage, settings: &Value, provider: &str) -> bool {
    let Ok(accounts) = crate::chat_accounts::active_connections(storage, provider).await else {
        return true;
    };
    accounts
        .iter()
        .any(proxy_transport::account_requires_tunnel)
        || !matches!(egress(storage, settings, &Value::Null).await, Ok(None))
}

pub fn url() -> String {
    format!("{ORIGIN}{CHAT}")
}

pub fn fixture_url() -> Option<String> {
    fixture(CHAT)
}

fn fixture(path: &str) -> Option<String> {
    (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"))
        .then(|| format!("http://127.0.0.1:52989/cline{path}"))
}

fn saved_token(account: &Value) -> Option<&str> {
    if truthy(&account["apiKey"]) {
        account["apiKey"].as_str()
    } else {
        account["accessToken"].as_str()
    }
}

fn normalize(token: &str) -> Cow<'_, str> {
    let token = js_trim(token);
    if token
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("workos:"))
        || !JWT.is_match(token)
    {
        Cow::Borrowed(token)
    } else {
        Cow::Owned(format!("workos:{token}"))
    }
}

fn authorization(token: &str) -> Option<HeaderValue> {
    bearer(normalize(token).as_ref())
}

fn bearer(token: &str) -> Option<HeaderValue> {
    if token.is_empty() {
        return None;
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    value.set_sensitive(true);
    Some(value)
}

pub fn headers(
    mut request: reqwest::RequestBuilder,
    account: &Value,
    stream: bool,
) -> reqwest::RequestBuilder {
    if stream {
        request = request.header(header::ACCEPT, "text/event-stream");
    }
    oauth_headers(request, saved_token(account))
}

pub(crate) fn oauth_headers(
    mut request: reqwest::RequestBuilder,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    request = identity(request);
    if let Some(auth) = token.and_then(authorization) {
        request = request.header(header::AUTHORIZATION, auth);
    }
    request
}

fn identity(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        platform => platform,
    };
    request
        .header("HTTP-Referer", "https://cline.bot")
        .header("X-Title", "Cline")
        .header(header::USER_AGENT, format!("9Router/{}", *VERSION))
        .header("X-PLATFORM", platform)
        .header("X-PLATFORM-VERSION", "unknown")
        .header("X-CLIENT-TYPE", "9router")
        .header("X-CLIENT-VERSION", VERSION.as_str())
        .header("X-CORE-VERSION", VERSION.as_str())
        .header("X-IS-MULTIROOT", "false")
}

fn account_authorization(provider: &str, account: &Value, tunneled: bool) -> Option<HeaderValue> {
    if account["provider"] != provider {
        return None;
    }
    match account["authType"].as_str()? {
        "oauth" if !truthy(&account["apiKey"]) && (!tunneled || !expired(account)) => {
            authorization(account["accessToken"].as_str()?)
        }
        "apikey" if provider == "clinepass" => bearer(account["apiKey"].as_str()?),
        _ => None,
    }
}

pub(super) async fn protected_chat(
    storage: &Storage,
    provider: &str,
    route: &ProtectedEgress,
    account: &Value,
    request: (&Value, bool),
    mut overrides: header::HeaderMap,
    timeout: Duration,
) -> Result<(Response, Option<Value>), (StatusCode, &'static str)> {
    let (body, stream) = request;
    let unavailable = (
        StatusCode::UNAUTHORIZED,
        "Cline credentials are unavailable or expired",
    );
    let renewable = account["provider"] == provider
        && account["authType"] == "oauth"
        && !truthy(&account["apiKey"])
        && truthy(&account["refreshToken"]);
    let mut refreshed = None;
    if renewable && (expired(account) || refresh_due_for_provider(provider, account)) {
        refreshed = Some(
            refresh_protected(storage, provider, account, route)
                .await
                .ok_or(unavailable)?,
        );
    }
    let auth = account_authorization(provider, refreshed.as_ref().unwrap_or(account), true)
        .ok_or(unavailable)?;
    overrides.remove(header::PROXY_AUTHORIZATION);
    overrides.remove("x-relay-auth");
    overrides.remove("x-relay-target");
    overrides.remove("x-relay-path");
    let mut response =
        send_chat(Some(route), body, stream, auth, overrides.clone(), timeout).await?;
    if renewable
        && refreshed.is_none()
        && matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        )
        && let Some(current) = refresh_protected(storage, provider, account, route).await
        && current["accessToken"] != account["accessToken"]
        && let Some(auth) = account_authorization(provider, &current, true)
    {
        drop(response);
        response = send_chat(Some(route), body, stream, auth, overrides, timeout).await?;
        refreshed = Some(current);
    }
    Ok((response, refreshed))
}

async fn refresh_protected(
    storage: &Storage,
    provider: &str,
    account: &Value,
    route: &ProtectedEgress,
) -> Option<Value> {
    let settings = storage.read_settings().await?;
    if !matches!(egress(storage, &settings, account).await, Ok(Some(actual)) if &actual == route) {
        return None;
    }
    let mode = match route {
        ProtectedEgress::Relay(relay) => RefreshMode::RelayRouted(relay.relay_identity()?),
        _ => RefreshMode::Routed(true),
    };
    let current = refresh_account(storage, account, mode).await.ok()?;
    let settings = storage.read_settings().await?;
    if current["isActive"] == false
        || account_authorization(provider, &current, true).is_none()
        || !matches!(egress(storage, &settings, &current).await, Ok(Some(actual)) if &actual == route)
    {
        return None;
    }
    Some(current)
}

pub(super) async fn translator_reply(
    provider: &str,
    account: &Value,
    body: &Value,
    stream: bool,
    timeout: Duration,
) -> Result<(Response, Vec<u8>), (StatusCode, &'static str)> {
    let auth = account_authorization(provider, account, false).ok_or((
        StatusCode::UNAUTHORIZED,
        "Cline credentials are unavailable or expired",
    ))?;
    let credential = auth.as_bytes()[b"Bearer ".len()..].to_owned();
    let response = send_chat(None, body, stream, auth, header::HeaderMap::new(), timeout).await?;
    Ok((response, credential))
}

async fn send_chat(
    route: Option<&ProtectedEgress>,
    body: &Value,
    stream: bool,
    auth: HeaderValue,
    overrides: header::HeaderMap,
    timeout: Duration,
) -> Result<Response, (StatusCode, &'static str)> {
    let builder = client_builder().connect_timeout(timeout);
    let relay = match route {
        Some(ProtectedEgress::Relay(relay)) => Some(relay),
        _ => None,
    };
    let (client, destination) = if let Some(ProtectedEgress::Tunnel(proxy)) = route {
        (
            proxy_transport::vendor_metadata_client(proxy.clone(), builder)
                .ok_or((StatusCode::BAD_GATEWAY, "Could not use the Cline proxy"))?,
            url(),
        )
    } else {
        (
            builder.build().map_err(|_| {
                (
                    StatusCode::BAD_GATEWAY,
                    "Could not initialize the Cline client",
                )
            })?,
            if relay.is_some() {
                url()
            } else {
                fixture_url().unwrap_or_else(url)
            },
        )
    };
    let (failed, timed_out) = if route.is_some() {
        (
            "Could not reach Cline through the proxy",
            "Cline proxy timeout",
        )
    } else {
        ("Could not reach Cline upstream", "Cline connection timeout")
    };
    let mut request =
        identity(client.post(destination).json(body)).header(header::AUTHORIZATION, auth);
    if stream {
        request = request.header(header::ACCEPT, "text/event-stream");
    }
    let request = request.headers(overrides);
    let request = if let Some(relay) = relay {
        let (client, request) = request.build_split();
        let request = request.map_err(|_| (StatusCode::BAD_GATEWAY, failed))?;
        let relay = relay
            .relay_target(request.url())
            .ok_or((StatusCode::BAD_GATEWAY, failed))?;
        provider_http::relay_request(client, request, &relay)
            .ok_or((StatusCode::BAD_GATEWAY, failed))?
    } else {
        request
    };
    let response = tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, timed_out))?
        .map_err(|_| (StatusCode::BAD_GATEWAY, failed))?;
    if response.status().is_redirection() {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Cline upstream redirected the request",
        ));
    }
    Ok(response)
}

pub(super) fn redact_error(account: &Value, message: &str) -> String {
    ["apiKey", "accessToken", "refreshToken"]
        .into_iter()
        .filter_map(|key| account[key].as_str())
        .flat_map(|token| [token, js_trim(token)])
        .filter(|token| !token.is_empty())
        .fold(message.to_owned(), |message, token| {
            message.replace(token, "[redacted]")
        })
}

pub async fn routed_models(
    storage: &Storage,
    provider: &str,
    account: &Value,
) -> Option<Vec<Value>> {
    if account["provider"] != provider || !is_provider(provider) {
        return None;
    }
    let client = MODEL_CLIENT.as_ref()?;
    let bind = |path: &'static str| async move {
        let endpoint = format!("{ORIGIN}{path}");
        let local = fixture(path);
        provider_http::bind(
            storage,
            account,
            &endpoint,
            local.as_deref().unwrap_or(&endpoint),
            client,
            MODEL_TIMEOUT,
            None,
        )
        .await
    };
    let token = saved_token(account)?;
    let route = bind(MODELS).await?;
    let mut request = route
        .request(Method::GET)
        .header(header::ACCEPT, "application/json");
    let (auth, secret) = if truthy(&account["apiKey"]) {
        (bearer(token)?, token)
    } else {
        if route.protected && expired(account) {
            return None;
        }
        request = identity(request);
        (authorization(token)?, js_trim(token))
    };
    let raw = model_body(request.header(header::AUTHORIZATION, auth)).await?;
    let list = raw.as_array().or_else(|| raw["data"].as_array())?;
    let secrets = std::iter::once(secret)
        .chain(route.relay_secret())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let cline_pass = provider == "clinepass";
    if cline_pass {
        let models: Vec<_> = list
            .iter()
            .filter_map(|model| model_entry(model, cline_pass, &secrets).map(|(_, entry)| entry))
            .collect();
        return (!models.is_empty()).then_some(models);
    }
    let mut models = Map::new();
    models.extend(
        list.iter()
            .filter_map(|model| model_entry(model, false, &secrets)),
    );
    if let Some(recommendations) = bind(RECOMMENDED).await
        && (route.same_route(&recommendations)
            || route
                .route
                .relay_identity()
                .is_some_and(|identity| recommendations.route.relay_identity() == Some(identity)))
        && let Some(free) = model_body(
            recommendations
                .request(Method::GET)
                .header(header::ACCEPT, "application/json"),
        )
        .await
        && let Some(entries) = free["free"].as_array()
    {
        for (id, entry) in entries
            .iter()
            .filter_map(|model| model_entry(model, false, &secrets))
        {
            models.entry(id).or_insert(entry);
        }
    }
    (!models.is_empty()).then(|| models.into_values().collect())
}

fn expired(account: &Value) -> bool {
    ["expiresAt", "tokenExpiresAt"]
        .into_iter()
        .find_map(|field| {
            let value = &account[field];
            if let Some(epoch) = value.as_i64().or_else(|| value.as_str()?.parse().ok()) {
                return Some(if epoch < 1_000_000_000_000 {
                    epoch.saturating_mul(1_000)
                } else {
                    epoch
                });
            }
            chrono::DateTime::parse_from_rfc3339(value.as_str()?)
                .ok()
                .map(|date| date.timestamp_millis())
        })
        .is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp_millis())
}

fn model_entry(model: &Value, cline_pass: bool, secrets: &[String]) -> Option<(String, Value)> {
    let id = model["id"].as_str()?;
    if js_trim(id).is_empty()
        || cline_pass && !id.starts_with("cline-pass/")
        || secrets
            .iter()
            .any(|secret| id == secret || secret.len() >= 6 && id.contains(secret))
    {
        return None;
    }
    let mut name = if truthy(&model["name"]) {
        model["name"].clone()
    } else {
        json!(id)
    };
    crate::provider_tools::scrub_payload(&mut name, secrets);
    Some((id.to_owned(), json!({"id":id,"name":name})))
}

async fn model_body(request: reqwest::RequestBuilder) -> Option<Value> {
    let mut response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MODEL_LIMIT - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}
