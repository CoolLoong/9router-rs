use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header as http_header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use reqwest::{Client, Method, RequestBuilder, Response as UpstreamResponse, Url, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::dashboard_session;
use crate::provider_connections::{self, truthy};
use crate::provider_http::{self, Bound};
use crate::provider_oauth::{RefreshError, RefreshMode, refresh_account};
use crate::proxy_transport::{self, Route, Selection as ProxySelection};
use crate::storage::Storage;

#[path = "quota_antigravity.rs"]
mod antigravity;
pub(crate) use antigravity::read as read_antigravity;
#[path = "quota_extra.rs"]
mod extra;
#[path = "quota_xiaomi_mimo.rs"]
mod mimo;
#[path = "account_quota_parse.rs"]
mod parse;
#[path = "quota_reset.rs"]
mod reset;

const MAX_CACHE: usize = 256;
const CACHE_TTL: Duration = Duration::from_secs(300);
const COOLDOWN: Duration = Duration::from_secs(180);
const QUOTA_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub(crate) struct QuotaState {
    pub(crate) storage: Storage,
    pub(crate) reader: Reader,
}

pub fn router(storage: Storage, start_auto_ping: impl FnOnce(QuotaState)) -> Router {
    let endpoints = Endpoints::pinned().expect("pinned usage provider configuration");
    let reader = Reader::new(endpoints).expect("account usage HTTP client");
    start_auto_ping(QuotaState {
        storage: storage.clone(),
        reader: reader.clone(),
    });
    build_router(storage, reader)
}

fn build_router(storage: Storage, reader: Reader) -> Router {
    let state = QuotaState { storage, reader };
    Router::new()
        .route(
            "/api/usage/{connection_id}",
            get(quota)
                .layer(axum::middleware::map_response(|mut response: Response| async move {
                    response.headers_mut().entry(http_header::VARY).or_insert(
                        http_header::HeaderValue::from_static(
                            "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding",
                        ),
                    );
                    response
                })),
        )
        .route(
            "/api/usage/{connection_id}/codex-reset-credits",
            get(reset_status).post(reset::codex),
        )
        .route(
            "/api/usage/{connection_id}/claude-reset",
            axum::routing::post(reset::claude),
        )
        .with_state(state)
}

async fn quota(
    State(state): State<QuotaState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return unauthorized();
    }
    let mut account = match load(&state.storage, id).await {
        Ok(account) => account,
        Err(error) => return error.into_response(),
    };
    if account["provider"]
        .as_str()
        .is_some_and(crate::provider_oauth::minimax_code::usage::supports)
    {
        let (status, body) =
            crate::provider_oauth::minimax_code::usage::read(&state.storage, account).await;
        return (status, Json(body)).into_response();
    }
    let oauth = account["authType"] == "oauth";
    let api_key = matches!(account["authType"].as_str(), Some("apikey" | "api_key"))
        && account["provider"]
            .as_str()
            .and_then(crate::provider_index::find)
            .is_some_and(|provider| provider["usageApiKey"] == true);
    if !oauth && !api_key && account["authType"] != "mcode-lease" {
        return Json(json!({"message": "Usage not available for this connection"})).into_response();
    }
    if account["provider"] == "xiaomi-mimo" {
        return match mimo::read(&state.storage, &account).await {
            Ok(usage) => Json(usage).into_response(),
            Err(error) => error.into_response(),
        };
    }
    if account["provider"] == "antigravity" {
        return match antigravity::read(&state.storage, &account).await {
            Ok(usage) => Json(usage).into_response(),
            Err(error) => error.into_response(),
        };
    }
    let provider = match account["provider"].as_str() {
        Some("codex") => Provider::Codex,
        Some("claude") => Provider::Claude,
        Some(provider) if let Some(provider) = extra::Provider::from_id(provider) => {
            Provider::Extra(provider)
        }
        provider => {
            let provider = provider.unwrap_or("undefined");
            return Json(json!({"message":format!("Usage API not implemented for {provider}")}))
                .into_response();
        }
    };
    let force = request
        .uri()
        .query()
        .and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find_map(|(name, value)| (name == "force").then_some(value))
        })
        .is_some_and(|value| value == "1");
    match protected_read(&state, &account, provider).await {
        Ok(Some(read)) => {
            if oauth
                && matches!(
                    provider,
                    Provider::Codex
                        | Provider::Claude
                        | Provider::Extra(
                            extra::Provider::GeminiCli
                                | extra::Provider::CodeBuddyCn
                                | extra::Provider::CodeBuddyIntl
                                | extra::Provider::Kimi
                                | extra::Provider::Kiro
                                | extra::Provider::GrokCli
                                | extra::Provider::GitHub
                                | extra::Provider::ClinePass
                        )
                )
            {
                return match protected_oauth_usage(&state, account, provider, read, force).await {
                    Ok(usage) => Json(usage).into_response(),
                    Err(error) => error.into_response(),
                };
            }
            if !api_key
                && extra::supplied(&account["apiKey"]).is_none()
                && let Err(error) = protected_access(&account)
            {
                return error.into_response();
            }
            if api_key
                && matches!(provider, Provider::Extra(extra) if !extra.supports_credentials(&account))
            {
                return Failure::new(
                    StatusCode::UNAUTHORIZED,
                    "The saved quota API key is invalid",
                )
                .into_response();
            }
            return usage_response(
                state
                    .reader
                    .usage_with(&read, provider, &account, force)
                    .await,
            );
        }
        Ok(None) => (),
        Err(error) => return error.into_response(),
    }

    let refresh = match provider {
        _ if !oauth => None,
        Provider::Extra(
            extra::Provider::Zed
            | extra::Provider::CodeBuddyCn
            | extra::Provider::CommandCode
            | extra::Provider::Cursor
            | extra::Provider::DeepSeek
            | extra::Provider::Glm
            | extra::Provider::GlmCn
            | extra::Provider::Groq
            | extra::Provider::MiniMax
            | extra::Provider::MiniMaxCn
            | extra::Provider::Ollama
            | extra::Provider::OpenCodeGo
            | extra::Provider::OpenCodeZen
            | extra::Provider::Qoder
            | extra::Provider::QoderCn
            | extra::Provider::Synthetic
            | extra::Provider::VercelAiGateway,
        ) => None,
        Provider::Extra(extra::Provider::GitHub) => {
            extra::Provider::github_needs_refresh(&account).then_some(RefreshMode::Rejected)
        }
        Provider::Extra(extra::Provider::GrokCli) => (active_refreshable(&account)
            && crate::provider_oauth::refresh_due_for_provider("grok-cli", &account))
        .then_some(RefreshMode::Rejected),
        Provider::Extra(extra::Provider::IFlow) => {
            Some(RefreshMode::QuotaRouted { rejected: false })
        }
        _ => Some(RefreshMode::Proactive),
    };
    let refreshed = match refresh {
        Some(mode) => proactive(&state, &account, mode).await,
        None => Ok(None),
    };
    match refreshed {
        Ok(Some(current)) => {
            if !matches!(protected_read(&state, &current, provider).await, Ok(None)) {
                return proxy_unavailable().into_response();
            }
            account = current;
        }
        Ok(None) => (),
        Err(error) => return error.into_response(),
    }
    let result = state.reader.usage(provider, &account, force).await;
    let mut usage = match result {
        Ok(usage) => usage,
        Err(error) => {
            return Failure::new(StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
        }
    };
    if oauth
        && usage["message"].as_str().is_some_and(auth_expired)
        && active_refreshable(&account)
        && let Ok(current) = refresh_account(&state.storage, &account, RefreshMode::Rejected).await
    {
        if !matches!(protected_read(&state, &current, provider).await, Ok(None)) {
            return proxy_unavailable().into_response();
        }
        if let Ok(retried) = state.reader.usage(provider, &current, force).await {
            usage = retried;
        }
    }
    Json(usage).into_response()
}

async fn protected_oauth_usage(
    state: &QuotaState,
    mut account: Value,
    provider: Provider,
    mut read: QuotaDelivery,
    force: bool,
) -> Result<Value, Failure> {
    if let Some(mode) = protected_quota_refresh(provider, &account, read.scope, false)
        && let Some(current) = proactive(state, &account, mode).await?
    {
        read = protected_read(state, &current, provider)
            .await?
            .filter(|current| current.scope == read.scope)
            .ok_or_else(proxy_unavailable)?;
        account = current;
    }
    if !matches!(
        provider,
        Provider::Extra(extra::Provider::Kimi | extra::Provider::ClinePass)
    ) || extra::supplied(&account["apiKey"]).is_none()
    {
        protected_access(&account)?;
    }
    let mut usage = state
        .reader
        .usage_with(&read, provider, &account, force)
        .await
        .map_err(|message| Failure::new(StatusCode::INTERNAL_SERVER_ERROR, message))?;
    if usage["message"].as_str().is_some_and(auth_expired)
        && active_refreshable(&account)
        && let Some(mode) = protected_quota_refresh(provider, &account, read.scope, true)
        && let Ok(current) = refresh_account(&state.storage, &account, mode).await
        && access(&current) != access(&account)
    {
        let delivery = protected_read(state, &current, provider)
            .await?
            .filter(|current| current.scope == read.scope)
            .ok_or_else(proxy_unavailable)?;
        if let Ok(retried) = state
            .reader
            .usage_with(&delivery, provider, &current, force)
            .await
        {
            usage = retried;
        }
    }
    Ok(usage)
}

fn protected_quota_refresh(
    provider: Provider,
    account: &Value,
    scope: ReadScope,
    rejected: bool,
) -> Option<RefreshMode> {
    if matches!(provider, Provider::Extra(extra::Provider::GitHub)) {
        let relay = match scope {
            ReadScope::Relay(identity) => Some(identity),
            _ => None,
        };
        return (rejected || extra::Provider::github_needs_refresh(account))
            .then_some(RefreshMode::github(true, relay));
    }
    let (id, mode) = match provider {
        Provider::Extra(extra::Provider::GrokCli) => ("grok-cli", RefreshMode::GrokProtected),
        Provider::Extra(extra::Provider::Kiro) => ("kiro", RefreshMode::KiroProtected),
        _ => return Some(RefreshMode::QuotaRouted { rejected }),
    };
    if !active_refreshable(account)
        || !(rejected || crate::provider_oauth::refresh_due_for_provider(id, account))
    {
        return None;
    }
    Some(match scope {
        ReadScope::Relay(identity) => RefreshMode::RelayRouted(identity),
        _ => mode,
    })
}

async fn reset_status(
    State(state): State<QuotaState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return unauthorized();
    }
    let mut account = match load(&state.storage, id).await {
        Ok(account) => account,
        Err(error) => return error.into_response(),
    };
    if account["provider"] != "codex" {
        return Failure::new(
            StatusCode::BAD_REQUEST,
            "Codex reset credits are only available for Codex connections.",
        )
        .into_response();
    }
    if account["authType"] != "oauth" && account["authType"] != "access_token" {
        return Failure::new(
            StatusCode::BAD_REQUEST,
            "Codex reset credits require an OAuth or access-token connection.",
        )
        .into_response();
    }
    let endpoint = &state.reader.config.codex_credits;
    let oauth = account["authType"] == "oauth";
    let mut binding = match reset::bind(&state, &account, endpoint, "/codex/credits").await {
        Ok(binding) => binding,
        Err(error) => return error.into_response(),
    };
    if oauth {
        match proactive(
            &state,
            &account,
            RefreshMode::QuotaRouted { rejected: false },
        )
        .await
        {
            Ok(Some(current)) => {
                binding = match reset::bind(&state, &current, endpoint, "/codex/credits").await {
                    Ok(route)
                        if route.route == binding.route && route.protected == binding.protected =>
                    {
                        route
                    }
                    _ => return proxy_unavailable().into_response(),
                };
                account = current;
            }
            Ok(None) => (),
            Err(error) => return error.into_response(),
        }
    }
    if binding.protected
        && let Err(error) = protected_access(&account)
    {
        return error.into_response();
    }
    let mut result = state.reader.credits(&binding, &account).await;
    if result.as_ref().is_err_and(|error| auth_expired(error))
        && oauth
        && active_refreshable(&account)
    {
        result = match refresh_account(
            &state.storage,
            &account,
            RefreshMode::QuotaRouted { rejected: true },
        )
        .await
        {
            Ok(current) => {
                let route = match reset::bind(&state, &current, endpoint, "/codex/credits").await {
                    Ok(route)
                        if route.route == binding.route && route.protected == binding.protected =>
                    {
                        route
                    }
                    _ => return proxy_unavailable().into_response(),
                };
                state.reader.credits(&route, &current).await
            }
            Err(error) => Err(refresh_reason(error).into()),
        };
    }
    usage_response(result)
}

fn usage_response(result: Result<Value, String>) -> Response {
    match result {
        Ok(credits) => Json(credits).into_response(),
        Err(error) => Failure::new(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

async fn protected_read(
    state: &QuotaState,
    account: &Value,
    provider: Provider,
) -> Result<Option<QuotaDelivery>, Failure> {
    let config = &state.reader.config;
    let (targets, token_target): ([Option<&str>; 3], Option<&str>) = match provider {
        Provider::Extra(extra::Provider::IFlow) => return Ok(None),
        Provider::Codex => ([Some(&config.codex), None, None], Some(&config.codex_token)),
        Provider::Claude => (
            [Some(&config.claude), None, None],
            Some(&config.claude_token),
        ),
        Provider::Extra(extra::Provider::GeminiCli) => (
            [
                Some(&config.extra.gemini_quota),
                Some(&config.extra.gemini_assist),
                None,
            ],
            Some("https://oauth2.googleapis.com/token"),
        ),
        Provider::Extra(extra::Provider::CodeBuddyCn) => (
            [Some(extra::CN_USAGE), None, None],
            (account["authType"] == "oauth")
                .then_some(crate::codebuddy_device::Edition::China.refresh_url()),
        ),
        Provider::Extra(extra::Provider::CodeBuddyIntl) => (
            [Some(extra::CODEBUDDY_INTL_USAGE), None, None],
            (account["authType"] == "oauth").then_some(crate::codebuddy_device::INTL_REFRESH_URL),
        ),
        Provider::Extra(extra::Provider::GitHub) => {
            ([Some(&config.extra.github), None, None], None)
        }
        Provider::Extra(extra::Provider::ClinePass) => (
            [Some(&config.extra.clinepass), None, None],
            (account["authType"] == "oauth").then_some(config.extra.clinepass_refresh.as_str()),
        ),
        Provider::Extra(extra::Provider::Ollama) => (
            [
                Some(
                    config
                        .extra
                        .ollama
                        .shared_destination()
                        .ok_or_else(proxy_unavailable)?,
                ),
                None,
                None,
            ],
            None,
        ),
        Provider::Extra(provider) => (config.extra.targets(provider, false), None),
    };
    let settings = state
        .storage
        .read_settings()
        .await
        .ok_or_else(proxy_unavailable)?;
    let mut targets = targets.into_iter().flatten();
    let Some(target) = targets.next() else {
        return Ok(None);
    };
    let selected = read_egress(&state.storage, &settings, account, target).await?;
    let mut relays = Vec::new();
    for target in targets {
        let alternate = read_egress(&state.storage, &settings, account, target).await?;
        match (&selected, alternate) {
            (
                ReadEgress::Relay { identity, .. },
                ReadEgress::Relay {
                    route,
                    identity: alternate,
                },
            ) if *identity == alternate => relays.push(route),
            (selected, alternate) if selected == &alternate => (),
            _ => return Err(proxy_unavailable()),
        }
    }
    let delivery = match selected {
        ReadEgress::Bypass => QuotaDelivery {
            client: state.reader.bypass_client.clone(),
            scope: ReadScope::Bypass,
            relays: Vec::new(),
        },
        ReadEgress::Tunnel(proxy) => {
            let scope = ReadScope::Tunnel(Sha256::digest(proxy.as_str().as_bytes()).into());
            let client = proxy_transport::vendor_metadata_client(
                proxy,
                quota_client().retry(reqwest::retry::never()),
            )
            .ok_or_else(proxy_unavailable)?;
            QuotaDelivery {
                client,
                scope,
                relays: Vec::new(),
            }
        }
        ReadEgress::Relay { route, identity } => {
            relays.insert(0, route);
            QuotaDelivery {
                client: state.reader.bypass_client.clone(),
                scope: ReadScope::Relay(identity),
                relays,
            }
        }
        ReadEgress::Direct => {
            if let Some(target) = token_target
                && !matches!(
                    proxy_transport::select(&state.storage, &settings, account, target).await,
                    Ok(Route::Direct)
                )
            {
                return Err(proxy_unavailable());
            }
            return Ok(None);
        }
    };
    Ok(Some(delivery))
}

#[derive(PartialEq, Eq)]
enum ReadEgress {
    Direct,
    Bypass,
    Tunnel(Url),
    Relay { route: Route, identity: [u8; 32] },
}

async fn read_egress(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    target: &str,
) -> Result<ReadEgress, Failure> {
    let https = Url::parse(target).is_ok_and(|url| url.scheme() == "https");
    match provider_http::select(storage, settings, account, target).await {
        Ok(ProxySelection::ExplicitBypass) if https => Ok(ReadEgress::Bypass),
        Ok(ProxySelection::Routed(Route::Proxy { url, .. })) if https => {
            proxy_transport::tunnel_proxy_url(&url)
                .map(ReadEgress::Tunnel)
                .ok_or_else(proxy_unavailable)
        }
        Ok(ProxySelection::Routed(route @ Route::Relay { .. })) if https => {
            let identity = route.relay_identity().ok_or_else(proxy_unavailable)?;
            Ok(ReadEgress::Relay { route, identity })
        }
        Ok(ProxySelection::Routed(Route::Direct))
            if !proxy_transport::account_requires_tunnel(account)
                && !truthy(&settings["outboundProxyEnabled"])
                && !proxy_environment(target) =>
        {
            Ok(ReadEgress::Direct)
        }
        _ => Err(proxy_unavailable()),
    }
}

fn proxy_environment(target: &str) -> bool {
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
        return false;
    }
    let variables = if Url::parse(target).is_ok_and(|url| url.scheme() == "http") {
        ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    };
    variables
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|proxy| !proxy.is_empty()))
}

fn protected_access(account: &Value) -> Result<(), Failure> {
    let expiration = [&account["expiresAt"], &account["tokenExpiresAt"]]
        .into_iter()
        .find(|expiry| !expiry.is_null() && expiry.as_str() != Some(""));
    if access(account).is_some()
        && expiration.is_none_or(|expiry| {
            parse::reset_time(expiry, true).is_some_and(|expiry| expiry > chrono::Utc::now())
        })
    {
        Ok(())
    } else {
        Err(Failure::new(
            StatusCode::UNAUTHORIZED,
            "A current access token is required for quota through this proxy. Please re-authorize the connection.",
        ))
    }
}

fn proxy_unavailable() -> Failure {
    Failure::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "Account quota proxy is unavailable",
    )
}

async fn load(storage: &Storage, id: String) -> Result<Value, Failure> {
    storage
        .run(false, move |database| {
            provider_connections::by_id(database, &id)
        })
        .await
        .map_err(|_| {
            Failure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load provider connection",
            )
        })?
        .ok_or_else(|| Failure::new(StatusCode::NOT_FOUND, "Connection not found"))
}

pub(crate) async fn supports_direct(
    state: &QuotaState,
    account: &Value,
    provider: Provider,
) -> bool {
    let details = &account["providerSpecificData"];
    let pool_id = details["proxyPoolId"]
        .as_str()
        .map(str::trim)
        .unwrap_or_default();
    if (!pool_id.is_empty() && pool_id != "__none__")
        || details["connectionProxyEnabled"] == true
        || details["vercelRelayUrl"]
            .as_str()
            .is_some_and(|url| !url.trim().is_empty())
    {
        return false;
    }
    let Some(settings) = state.storage.read_settings().await else {
        return false;
    };
    if truthy(&settings["outboundProxyEnabled"]) {
        return false;
    }
    let config = &state.reader.config;
    let relevant = match provider {
        Provider::Codex => [
            Some(config.codex.as_str()),
            Some(config.codex_token.as_str()),
            None,
        ],
        Provider::Claude => [
            Some(config.claude.as_str()),
            Some(config.claude_token.as_str()),
            None,
        ],
        Provider::Extra(provider) => config
            .extra
            .targets(provider, account["authType"] == "oauth"),
    };
    for endpoint in relevant.into_iter().flatten() {
        if !matches!(
            proxy_transport::select(&state.storage, &settings, account, endpoint).await,
            Ok(Route::Direct)
        ) {
            return false;
        }
    }
    true
}

pub(crate) async fn proactive(
    state: &QuotaState,
    account: &Value,
    mode: RefreshMode,
) -> Result<Option<Value>, Failure> {
    if account["isActive"] == false {
        return Ok(None);
    }
    match refresh_account(&state.storage, account, mode).await {
        Ok(current) => Ok(Some(current)),
        Err(RefreshError::ProviderUnavailable) if access(account).is_some() => Ok(None),
        Err(RefreshError::AccountUnavailable) => {
            Err(Failure::new(StatusCode::NOT_FOUND, "Connection not found"))
        }
        Err(error) => Err(Failure::new(
            StatusCode::UNAUTHORIZED,
            format!("Credential refresh failed: {}", refresh_reason(error)),
        )),
    }
}

fn refresh_reason(error: RefreshError) -> &'static str {
    match error {
        RefreshError::ReauthenticationRequired => {
            "Refresh token invalid or reused. Please re-authorize the connection."
        }
        _ => "Failed to refresh credentials. Please re-authorize the connection.",
    }
}

pub(crate) fn active_refreshable(account: &Value) -> bool {
    account["isActive"] != false
        && account["refreshToken"]
            .as_str()
            .is_some_and(|token| !token.is_empty())
}

pub(crate) fn access(account: &Value) -> Option<&str> {
    account["accessToken"]
        .as_str()
        .filter(|token| !token.is_empty())
}

pub(crate) fn auth_expired(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "expired",
        "authentication",
        "unauthorized",
        "401",
        "re-authorize",
    ]
    .iter()
    .any(|hint| message.contains(hint))
}

fn unauthorized() -> Response {
    let mut response = Failure::new(StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    response.headers_mut().insert(
        http_header::VARY,
        http_header::HeaderValue::from_static("Accept-Encoding"),
    );
    response
}

pub(crate) struct Failure {
    status: StatusCode,
    reason: String,
}

impl Failure {
    fn new(status: StatusCode, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.reason}))).into_response()
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Provider {
    Codex,
    Claude,
    Extra(extra::Provider),
}

pub(crate) struct Endpoints {
    codex: String,
    codex_credits: String,
    codex_token: String,
    claude: String,
    claude_legacy: String,
    claude_settings: String,
    claude_token: String,
    pub(crate) anthropic_version: String,
    claude_agent: String,
    extra: extra::Endpoints,
}

impl Endpoints {
    fn pinned() -> Option<Self> {
        let codex = include_str!("../../../open-sse/providers/registry/codex.js");
        let claude = include_str!("../../../open-sse/providers/registry/claude.js");
        let shared = include_str!("../../../open-sse/providers/shared.js");
        let codex_usage = codex.split_once("\n    usage: {")?.1.split_once('}')?.0;
        let claude_usage = claude.split_once("\n    usage: {")?.1;
        Some(Self {
            codex: pinned_field(codex_usage, "url:")?,
            codex_credits: pinned_field(codex_usage, "resetCreditsUrl:")?,
            codex_token: pinned_field(codex.split_once("\n  oauth: {")?.1, "tokenUrl:")?,
            claude: pinned_field(claude_usage, "oauthUrl:")?,
            claude_legacy: pinned_field(claude_usage, "orgUrl:")?,
            claude_settings: pinned_field(claude_usage, "settingsUrl:")?,
            claude_token: pinned_field(claude.split_once("\n  oauth: {")?.1, "tokenUrl:")?,
            anthropic_version: pinned_field(shared, "export const ANTHROPIC_API_VERSION =")?,
            claude_agent: format!(
                "claude-cli/{} (external, cli)",
                pinned_field(shared, "export const CLAUDE_CLI_VERSION =")?
            ),
            extra: extra::Endpoints::pinned()?,
        })
    }
}

fn pinned_field(source: &str, prefix: &str) -> Option<String> {
    source.lines().find_map(|line| {
        Some(
            line.trim()
                .strip_prefix(prefix)?
                .trim()
                .strip_prefix('"')?
                .split_once('"')?
                .0
                .to_owned(),
        )
    })
}

#[derive(Clone)]
pub(crate) struct Reader {
    pub(crate) config: Arc<Endpoints>,
    pub(crate) client: Client,
    bypass_client: Client,
    cache: Arc<Mutex<Cache>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadScope {
    Direct,
    Bypass,
    Tunnel([u8; 32]),
    Relay([u8; 32]),
}

struct QuotaDelivery {
    client: Client,
    scope: ReadScope,
    relays: Vec<Route>,
}

impl QuotaDelivery {
    fn direct(&self) -> bool {
        self.scope == ReadScope::Direct
    }

    fn redact_relay(&self, mut message: String) -> String {
        if let Some(secret) = self
            .relays
            .iter()
            .find_map(|route| match route {
                Route::Relay {
                    authorization: Some(value),
                    ..
                } => value.to_str().ok(),
                _ => None,
            })
            .filter(|secret| !secret.is_empty())
        {
            message = message.replace(secret, "[redacted]");
        }
        message
    }

    async fn send(&self, request: RequestBuilder) -> Result<UpstreamResponse, &'static str> {
        let response = if self.relays.is_empty() {
            request.send().await.map_err(|_| "fetch failed")?
        } else {
            let (client, request) = request.build_split();
            let mut request = request.map_err(|_| "fetch failed")?;
            request.timeout_mut().get_or_insert(QUOTA_TIMEOUT);
            let route = self
                .relays
                .iter()
                .find_map(|route| route.relay_target(request.url()))
                .ok_or("Quota relay origin changed")?;
            provider_http::relay_request(client, request, &route)
                .ok_or("fetch failed")?
                .send()
                .await
                .map_err(|_| "fetch failed")?
        };
        if !self.direct() && response.status().is_redirection() {
            return Err("Quota upstream redirect refused");
        }
        Ok(response)
    }
}

type Cache = VecDeque<(([u8; 32], ReadScope), Arc<AsyncMutex<ClaudeSlot>>)>;

#[derive(Default)]
struct ClaudeSlot {
    prior: Option<(Instant, Value)>,
    cooldown: Option<Instant>,
}

fn quota_client() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .timeout(QUOTA_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
}

impl Reader {
    fn new(config: Endpoints) -> Result<Self, reqwest::Error> {
        Ok(Self {
            config: Arc::new(config),
            client: quota_client().build()?,
            bypass_client: quota_client().retry(reqwest::retry::never()).build()?,
            cache: Arc::default(),
        })
    }

    fn direct(&self) -> QuotaDelivery {
        QuotaDelivery {
            client: self.client.clone(),
            scope: ReadScope::Direct,
            relays: Vec::new(),
        }
    }

    pub(crate) async fn usage(
        &self,
        provider: Provider,
        account: &Value,
        force: bool,
    ) -> Result<Value, String> {
        self.usage_with(&self.direct(), provider, account, force)
            .await
    }

    async fn usage_with(
        &self,
        read: &QuotaDelivery,
        provider: Provider,
        account: &Value,
        force: bool,
    ) -> Result<Value, String> {
        let token = access(account);
        match provider {
            Provider::Codex => {
                let request = bearer(read.client.get(&self.config.codex), token)
                    .header(header::ACCEPT, "application/json");
                let response = read
                    .send(request)
                    .await
                    .map_err(|error| format!("Failed to fetch Codex usage: {error}"))?;
                if !response.status().is_success() {
                    return Ok(
                        json!({"message": format!("Codex connected. Usage API temporarily unavailable ({}).", response.status().as_u16())}),
                    );
                }
                let body = response
                    .json::<Value>()
                    .await
                    .map_err(|_| "Failed to fetch Codex usage: invalid provider response")?;
                Ok(parse::codex_usage(&body))
            }
            Provider::Claude => Ok(self.claude(read, token, force).await),
            Provider::Extra(provider) => self.config.extra.usage(read, provider, account).await,
        }
    }

    async fn credits(&self, read: &Bound, account: &Value) -> Result<Value, String> {
        let token = access(account)
            .ok_or("No Codex access token available. Please re-authorize the connection.")?;
        let mut request = read
            .request(Method::GET)
            .bearer_auth(token)
            .header(header::ACCEPT, "application/json")
            .header("OpenAI-Beta", "codex-1")
            .header("originator", "codex_cli_rs");
        let details = &account["providerSpecificData"];
        if let Some(id) = ["workspaceId", "accountId", "chatgptAccountId"]
            .into_iter()
            .find_map(|key| details[key].as_str().filter(|id| !id.is_empty()))
        {
            request = request.header("ChatGPT-Account-ID", id);
        }
        let response = request.send().await.map_err(|_| "fetch failed")?;
        let status = response.status();
        if read.protected && status.is_redirection() {
            return Err("Quota upstream redirect refused".into());
        }
        let body = response.json::<Value>().await.unwrap_or_default();
        if !status.is_success() {
            let message = ["message", "error", "detail"]
                .into_iter()
                .find_map(|name| {
                    let value = &body[name];
                    if !truthy(value) {
                        return None;
                    }
                    Some(
                        value
                            .as_str()
                            .or_else(|| value["message"].as_str())
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string()),
                    )
                })
                .unwrap_or_else(|| {
                    format!("Codex reset credits API unavailable ({}).", status.as_u16())
                });
            let message: String = message
                .chars()
                .filter(|ch| !ch.is_control())
                .take(300)
                .collect();
            let message = message.replace(token, "[redacted]");
            return Err(message);
        }
        Ok(parse::codex_credits(&body))
    }

    fn invalidate_claude_usage(&self, token: &str) {
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|((key, _), _)| *key != hash);
    }

    fn scoped_slot(&self, token: &str, scope: ReadScope) -> Arc<AsyncMutex<ClaudeSlot>> {
        let key = (Sha256::digest(token.as_bytes()).into(), scope);
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(index) = cache.iter().position(|(hash, _)| *hash == key)
            && let Some(entry) = cache.remove(index)
        {
            let slot = entry.1.clone();
            cache.push_back(entry);
            return slot;
        }
        if cache.len() >= MAX_CACHE {
            cache.pop_front();
        }
        let slot = Arc::new(AsyncMutex::new(ClaudeSlot::default()));
        cache.push_back((key, slot.clone()));
        slot
    }

    async fn claude(&self, read: &QuotaDelivery, token: Option<&str>, force: bool) -> Value {
        let Some(token) = token else {
            return self
                .claude_uncached(read, None, &mut ClaudeSlot::default())
                .await;
        };
        let slot = self.scoped_slot(token, read.scope);
        let mut slot = slot.lock().await;
        if !force
            && let Some((expires, value)) = &slot.prior
            && *expires > Instant::now()
        {
            return value.clone();
        }
        let usage = self.claude_uncached(read, Some(token), &mut slot).await;
        if truthy(&usage["quotas"]) {
            slot.prior = Some((Instant::now() + CACHE_TTL, usage.clone()));
        } else if !force && let Some((_, stale)) = &slot.prior {
            return stale.clone();
        }
        usage
    }

    async fn claude_uncached(
        &self,
        read: &QuotaDelivery,
        token: Option<&str>,
        slot: &mut ClaudeSlot,
    ) -> Value {
        if slot.cooldown.is_none_or(|until| until <= Instant::now()) {
            let mut request = bearer(read.client.get(&self.config.claude), token)
                .header("anthropic-beta", "oauth-2025-04-20")
                .header("anthropic-version", &self.config.anthropic_version);
            if read.direct() || !read.relays.is_empty() {
                request = request
                    .query(&[("cedar_ember", "1")])
                    .header(header::USER_AGENT, &self.config.claude_agent);
            }
            let response = read.send(request).await;
            match response {
                Ok(result) if result.status().is_success() => {
                    return match result.json::<Value>().await {
                        Ok(body) => parse::claude_usage(&body),
                        Err(_) => claude_error("invalid provider response"),
                    };
                }
                Ok(result) if result.status() == StatusCode::TOO_MANY_REQUESTS => {
                    slot.cooldown = Some(Instant::now() + COOLDOWN);
                }
                Ok(_) => (),
                Err(error) => return claude_error(error),
            }
        }
        self.claude_legacy(read, token).await
    }

    async fn claude_legacy(&self, read: &QuotaDelivery, token: Option<&str>) -> Value {
        let request = bearer(read.client.get(&self.config.claude_settings), token)
            .header("anthropic-version", &self.config.anthropic_version);
        let response = match read.send(request).await {
            Ok(result) if result.status().is_success() => result,
            Ok(_) => {
                return json!({"message": "Claude connected. Usage API requires admin permissions."});
            }
            Err(error) => return claude_error(error),
        };
        let Ok(settings) = response.json::<Value>().await else {
            return claude_error("invalid provider response");
        };
        let mut result = json!({"plan": parse::value_or(&settings["plan"], json!("Unknown"))});
        if let Some(name) = settings.get("organization_name") {
            result["organization"] = name.clone();
        }
        if let Some(id) = settings["organization_id"]
            .as_str()
            .filter(|id| valid_org_id(id))
        {
            let url = self.config.claude_legacy.replace("{org_id}", id);
            let request = bearer(read.client.get(url), token)
                .header("anthropic-version", &self.config.anthropic_version);
            match read.send(request).await {
                Ok(usage) if usage.status().is_success() => {
                    let Ok(data) = usage.json::<Value>().await else {
                        return claude_error("invalid provider response");
                    };
                    result["quotas"] = data;
                    return result;
                }
                Ok(_) => (),
                Err(error) => return claude_error(error),
            }
        }
        result["message"] = json!("Claude connected. Usage details require admin access.");
        result
    }
}

fn bearer(request: RequestBuilder, token: Option<&str>) -> RequestBuilder {
    request.bearer_auth(token.unwrap_or("undefined"))
}

fn valid_org_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn claude_error(reason: &str) -> Value {
    json!({"message": format!("Claude connected. Unable to fetch usage: {reason}")})
}
