use std::borrow::Cow;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::post;
use serde_json::{Value, json};
use url::{Host, Url};

use crate::chat_accounts::{self, Retry, Selection};
use crate::chat_output::{error, json_response};
use crate::cli_video::redact_bearer;
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::provider_index::{self, js_trim};
use crate::proxy_transport::{self, Route, Selection as ProxySelection};
use crate::storage::Storage;
use crate::{catalog_guard, combo_chat, key_policy, model_resolution};

#[path = "web_fetch.rs"]
mod fetch;
#[path = "web_search.rs"]
mod search;

const BODY_LIMIT: usize = 128 * 1024 * 1024;

#[derive(Clone)]
struct WebState {
    storage: Storage,
    client: reqwest::Client,
    protected: reqwest::Client,
    fixture: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Search,
    Fetch,
}

impl Kind {
    fn service(self) -> &'static str {
        match self {
            Self::Search => "webSearch",
            Self::Fetch => "webFetch",
        }
    }

    fn lock_key(self, provider: &str) -> String {
        format!("{}:{provider}", self.service().to_ascii_lowercase())
    }
}

pub fn router(storage: Storage) -> Router {
    let state = WebState {
        storage,
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(crate::upstream::redirects())
            .build()
            .expect("web gateway HTTP client"),
        protected: protected_client()
            .build()
            .expect("protected web HTTP client"),
        fixture: std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"),
    };
    let mut router = Router::new();
    for prefix in ["/v1", "/api/v1", "/v1/v1"] {
        for (suffix, handler) in [("search", post(search)), ("web/fetch", post(fetch))] {
            router = router.route(&format!("{prefix}/{suffix}"), handler.options(options));
        }
    }
    router.with_state(state)
}

fn protected_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
}

impl WebState {
    async fn route_request(
        &self,
        builder: reqwest::RequestBuilder,
        fixture_prefix: &str,
        settings: &Value,
        account: &Value,
        expected: Option<&ProxySelection>,
    ) -> Result<(reqwest::RequestBuilder, WebDelivery), Failure> {
        let unavailable = || {
            Failure::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Web provider proxy is unavailable",
            )
            .terminal()
        };
        let (client, request) = builder.build_split();
        let mut request = request.map_err(|_| unavailable())?;
        let route =
            crate::provider_http::select(&self.storage, settings, account, request.url().as_str())
                .await
                .map_err(|_| unavailable())?;
        if expected.is_some_and(|expected| expected != &route) {
            return Err(unavailable());
        }
        let direct = |mut request: reqwest::Request| {
            if self.fixture {
                let old = request.url();
                let mut target = Url::parse(&format!(
                    "http://127.0.0.1:52989{fixture_prefix}{}",
                    old.path()
                ))
                .map_err(|_| unavailable())?;
                target.set_query(old.query());
                *request.url_mut() = target;
            }
            Ok(request)
        };
        let (client, request, protected) = match &route {
            ProxySelection::Routed(Route::Direct) if !proxy_configured(settings, account) => {
                (client, direct(request)?, false)
            }
            ProxySelection::ExplicitBypass => (self.protected.clone(), direct(request)?, true),
            ProxySelection::Routed(Route::Proxy { url, .. }) => {
                let proxy = proxy_transport::tunnel_proxy_url(url).ok_or_else(unavailable)?;
                let client = proxy_transport::vendor_metadata_client(proxy, protected_client())
                    .ok_or_else(unavailable)?;
                (client, request, true)
            }
            ProxySelection::Routed(Route::Relay {
                url,
                authorization: Some(_),
                ..
            }) => {
                if request
                    .url()
                    .query_pairs()
                    .any(|(name, _)| matches!(name.as_ref(), "key" | "api_key"))
                {
                    return Err(Failure::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Web credentials in the URL require a tunnel proxy",
                    )
                    .terminal());
                }
                *request.url_mut() = Url::parse(url).map_err(|_| unavailable())?;
                (self.protected.clone(), request, true)
            }
            _ => return Err(unavailable()),
        };
        let builder = reqwest::RequestBuilder::from_parts(client, request);
        let (builder, relay_secret) = match &route {
            ProxySelection::Routed(relay @ Route::Relay { authorization, .. }) => {
                (relay.relay(builder), authorization.clone())
            }
            _ => (builder, None),
        };
        Ok((
            builder,
            WebDelivery {
                protected,
                relay_secret,
            },
        ))
    }
}

struct WebDelivery {
    protected: bool,
    relay_secret: Option<HeaderValue>,
}

impl WebDelivery {
    fn redact<'a>(&self, message: &'a str) -> Cow<'a, str> {
        match self
            .relay_secret
            .as_ref()
            .and_then(|secret| secret.to_str().ok())
        {
            Some(secret) if !secret.is_empty() && message.contains(secret) => {
                Cow::Owned(message.replace(secret, "[redacted]"))
            }
            _ => Cow::Borrowed(message),
        }
    }

    fn failure(&self, mut failure: Failure) -> Failure {
        if let Cow::Owned(message) = self.redact(&failure.message) {
            failure.message = message;
        }
        if self.protected {
            failure.terminal()
        } else {
            failure
        }
    }
}

fn proxy_configured(settings: &Value, account: &Value) -> bool {
    truthy(&settings["outboundProxyEnabled"]) || proxy_transport::account_requires_tunnel(account)
}

async fn options() -> Response {
    into_http(Ok(json!({
        "status": 200,
        "headers": {
            "access-control-allow-origin": "*",
            "access-control-allow-methods": "POST, OPTIONS",
            "access-control-allow-headers": "*",
        },
        "body": "",
    })))
}

async fn search(State(state): State<WebState>, request: Request) -> Response {
    web(state, request, Kind::Search).await
}

async fn fetch(State(state): State<WebState>, request: Request) -> Response {
    web(state, request, Kind::Fetch).await
}

enum Plan {
    Denied,
    Combo(Value),
    Single,
}

async fn web(state: WebState, request: Request, kind: Kind) -> Response {
    let policy_key = catalog_guard::api_key(&request).filter(|key| !key.is_empty());
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, BODY_LIMIT).await else {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    let raw = String::from_utf8_lossy(&bytes);
    let Ok(body) = serde_json::from_str::<Value>(raw.strip_prefix('\u{feff}').unwrap_or(&raw))
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    if body.is_null() {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    }
    let custom_search_endpoint =
        kind == Kind::Search && setting(&body["provider_options"], "baseUrl").is_some();
    let Some(settings) = state.storage.read_settings().await else {
        if custom_search_endpoint {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not verify search provider configuration",
            );
        }
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to read settings");
    };
    if settings.get("requireApiKey").is_none_or(truthy) {
        let key = parts
            .headers
            .get("authorization")
            .and_then(|header| header.to_str().ok())
            .and_then(|text| text.strip_prefix("Bearer "))
            .or_else(|| {
                parts
                    .headers
                    .get("x-api-key")
                    .and_then(|header| header.to_str().ok())
            });
        let Some(key) = key.filter(|key| !key.is_empty()) else {
            return error(StatusCode::UNAUTHORIZED, "Missing API key");
        };
        if !state.storage.api_key_is_active(key.to_owned()).await {
            return error(StatusCode::UNAUTHORIZED, "Invalid API key");
        }
    }
    let provider_input = or_else(&body["provider"], &body["model"]);
    let Some(requested) = provider_input.as_str().filter(|name| !name.is_empty()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Missing required field: provider (or model)",
        );
    };
    let input = match kind {
        Kind::Search => {
            let Some(query) = body["query"]
                .as_str()
                .map(js_trim)
                .filter(|value| !value.is_empty())
            else {
                return error(StatusCode::BAD_REQUEST, "Missing required field: query");
            };
            query
        }
        Kind::Fetch => {
            let Some(url) = body["url"].as_str().filter(|url| !url.is_empty()) else {
                return error(StatusCode::BAD_REQUEST, "Missing required field: url");
            };
            if let Err(message) = public_url(url).await {
                return error(StatusCode::BAD_REQUEST, message);
            }
            url
        }
    };
    let entry = web_provider(requested);
    let provider = entry
        .and_then(|entry| entry["id"].as_str())
        .unwrap_or(requested);
    let requested_lookup = requested.to_owned();
    let provider_lookup = provider.to_owned();
    let plan = state
        .storage
        .run(false, move |db| {
            let combo = model_resolution::get_combo_models(db, &requested_lookup)?;
            if let Some(key) = &policy_key
                && !key_policy::permits_provider(
                    db,
                    key,
                    &requested_lookup,
                    &provider_lookup,
                    combo.is_some(),
                )?
            {
                return Ok(Plan::Denied);
            }
            Ok(combo.map_or(Plan::Single, Plan::Combo))
        })
        .await;
    let combo = match plan {
        Ok(Plan::Denied) => {
            return error(
                StatusCode::FORBIDDEN,
                &format!("This API key is not allowed to use model \"{requested}\""),
            );
        }
        Ok(Plan::Combo(models)) => Some(models),
        Ok(Plan::Single) => None,
        Err(_) if custom_search_endpoint => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not verify search provider configuration",
            );
        }
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to resolve web provider",
            );
        }
    };
    if custom_search_endpoint
        && (combo.is_some()
            || provider == "searxng"
            || entry.is_some_and(|entry| entry["authType"] != "none"))
    {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":"Search base URL must be configured on the provider connection","code":400}}),
            None,
        );
    }
    let run = |member| run_member(&state, kind, input, &body, &settings, member);
    if let Some(combo) = combo {
        let Some(models) = combo.as_array() else {
            return error(StatusCode::BAD_REQUEST, "Invalid web combo provider list");
        };
        let members = models
            .iter()
            .map(|model| {
                model.as_str().map_or_else(
                    || Member::Invalid("Invalid provider in web combo".into()),
                    |model| prepare_member(kind, model),
                )
            })
            .collect();
        let sticky = combo_chat::sticky_for(combo_chat::strategy(requested, &settings), &settings);
        return combo_chat::Combo {
            name: requested.into(),
            members,
            sticky,
        }
        .execute(
            &HashSet::new(),
            |member| (member.provider().map(str::to_owned), true, None),
            run,
            |_| {},
        )
        .await;
    }
    run(prepare_member(kind, requested)).await
}

enum Member {
    Invalid(String),
    Ready {
        provider: &'static str,
        credential_fallback: Option<&'static str>,
        adapter: Adapter,
    },
}

impl Member {
    fn provider(&self) -> Option<&'static str> {
        match self {
            Self::Ready { provider, .. } => Some(provider),
            Self::Invalid(_) => None,
        }
    }
}

fn web_provider(requested: &str) -> Option<&'static Value> {
    provider_index::entries()
        .iter()
        .find(|entry| entry["alias"] == requested || entry["id"] == requested)
        .or_else(|| {
            let id = gateway_model_resolver_rs::port::known_provider_id(requested)
                .ok()
                .flatten()?;
            (id == "perplexity-agent")
                .then(|| provider_index::find(id))
                .flatten()
        })
}

fn prepare_member(kind: Kind, requested: &str) -> Member {
    let Some(entry) = web_provider(requested) else {
        return Member::Invalid(format!("Unknown provider: {requested}"));
    };
    let Some(provider) = entry["id"].as_str() else {
        return Member::Invalid(format!("Unknown provider: {requested}"));
    };
    let feature = if kind == Kind::Search {
        "search"
    } else {
        "fetch"
    };
    if !entry["serviceKinds"]
        .as_array()
        .is_some_and(|kinds| kinds.iter().any(|entry| entry == kind.service()))
    {
        return Member::Invalid(format!(
            "Provider {provider} does not support web {feature}"
        ));
    }
    let adapter = match kind {
        Kind::Search => search::Adapter::resolve(provider).map(Adapter::Search),
        Kind::Fetch => fetch::Adapter::resolve(provider).map(Adapter::Fetch),
    };
    let Some(adapter) = adapter else {
        return Member::Invalid(if kind == Kind::Search && provider == "vercel-ai-gateway" {
            format!("Unsupported chat-search provider: {provider}")
        } else {
            format!("Provider {provider} does not support web {feature}")
        });
    };
    Member::Ready {
        provider,
        credential_fallback: (kind == Kind::Search && provider == "ollama-search")
            .then_some("ollama"),
        adapter,
    }
}

async fn run_member(
    state: &WebState,
    kind: Kind,
    input: &str,
    body: &Value,
    settings: &Value,
    member: Member,
) -> Response {
    let (provider, credential_fallback, adapter) = match member {
        Member::Invalid(message) => return error(StatusCode::BAD_REQUEST, &message),
        Member::Ready {
            provider,
            credential_fallback,
            adapter,
        } => (provider, credential_fallback, adapter),
    };
    let lock = kind.lock_key(provider);
    if let Adapter::Search(search::Adapter::Searxng) = adapter {
        let Ok(account) =
            proxy_transport::public_proxy_account(&state.storage, settings, provider).await
        else {
            return Failure::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not select a SearXNG proxy pool",
            )
            .response(kind);
        };
        return match search::request(
            state,
            search::Adapter::Searxng,
            input,
            body,
            settings,
            &account,
        )
        .await
        {
            Ok(value) => json_response(StatusCode::OK, value, None),
            Err(failure) => {
                let terminal = failure.terminal;
                let mut response = failure.response(kind);
                if terminal {
                    response.extensions_mut().insert(combo_chat::StopFallback);
                }
                response
            }
        };
    }
    let mut excluded = HashSet::new();
    let mut last = None;
    loop {
        let mut credential_provider = provider;
        let mut selection =
            chat_accounts::select(&state.storage, provider, &lock, &excluded, None).await;
        if matches!(selection, Ok(Selection::Empty))
            && let Some(fallback) = credential_fallback
        {
            credential_provider = fallback;
            selection =
                chat_accounts::select(&state.storage, fallback, &lock, &excluded, None).await;
        }
        let account = match selection {
            Ok(Selection::Ready(account)) => account,
            Ok(Selection::CoolingDown(retry)) => {
                let mut accounts = Vec::new();
                for candidate in std::iter::once(provider).chain(credential_fallback) {
                    let Ok(list) =
                        chat_accounts::active_connections(&state.storage, candidate).await
                    else {
                        return error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Failed to read provider credentials",
                        );
                    };
                    accounts.extend(list);
                }
                return unavailable(provider, retry, last, &accounts);
            }
            Ok(Selection::Empty) => {
                return last.map_or_else(
                    || {
                        error(
                            StatusCode::BAD_REQUEST,
                            &format!("No credentials for provider: {provider}"),
                        )
                    },
                    |failure: Failure| error(failure.status, &failure.message),
                );
            }
            Err(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to select provider credentials",
                );
            }
        };
        let Some(connection) = account["id"].as_str() else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid provider connection",
            );
        };
        let result = if supports_account(&account, kind) {
            match adapter {
                Adapter::Search(adapter) => {
                    search::request(state, adapter, input, body, settings, &account).await
                }
                Adapter::Fetch(adapter) => {
                    fetch::request(state, adapter, input, body, settings, &account).await
                }
            }
        } else {
            Err(Failure::new(
                StatusCode::UNAUTHORIZED,
                format!("Unsupported credentials for provider: {provider}"),
            ))
        };
        match result {
            Ok(mut value) => {
                if kind == Kind::Fetch {
                    let _ = chat_accounts::mark_success(&state.storage, connection, &lock).await;
                }
                redact_payload(&mut value, &account);
                return json_response(StatusCode::OK, value, None);
            }
            Err(mut failure) => {
                failure.message = redact(&failure.message, &account);
                if failure.terminal {
                    let mut response = failure.response(kind);
                    response.extensions_mut().insert(combo_chat::StopFallback);
                    return response;
                }
                let retry = chat_accounts::mark_unavailable(
                    &state.storage,
                    credential_provider,
                    connection,
                    &lock,
                    failure.status.as_u16(),
                    &failure.message,
                    None,
                )
                .await;
                if !retry.is_ok_and(|result| result.should_fallback) {
                    return failure.response(kind);
                }
                last = Some(failure);
                excluded.insert(connection.to_owned());
            }
        }
    }
}

fn supports_account(account: &Value, kind: Kind) -> bool {
    if kind == Kind::Search
        && matches!(
            account["provider"].as_str(),
            Some("antigravity" | "xai" | "kimi")
        )
    {
        return true;
    }
    let details = &account["providerSpecificData"];
    let glm_coding_key = kind == Kind::Search
        && account["provider"] == "glm"
        && account["authType"] == "oauth"
        && details["authMethod"] == "cli_poll"
        && upstream_token(account, &["accessToken"]).is_some();
    matches!(account["authType"].as_str(), Some("apikey" | "api_key"))
        || glm_coding_key
        || (account["provider"] != "glm"
            && upstream_token(account, &["apiKey", "accessToken"]).is_some())
}

fn unavailable(
    provider: &str,
    retry: Retry,
    last: Option<Failure>,
    accounts: &[Value],
) -> Response {
    let status = last
        .as_ref()
        .map(|failure| failure.status.as_u16())
        .or(retry.last_error_code)
        .unwrap_or(503);
    let reason = last
        .as_ref()
        .map(|failure| failure.message.as_str())
        .or(retry.last_error.as_deref())
        .unwrap_or("Unavailable");
    let reason = accounts
        .iter()
        .fold(redact_bearer(reason), |text, account| {
            redact_secrets(text, account)
        });
    let message = format!("[{provider}] {reason} ({})", retry.retry_after_human);
    into_http(Ok(json!({
        "status": status,
        "headers": {"content-type": "application/json", "retry-after": retry.retry_after_seconds.to_string()},
        "body": json!({"error": {"message": message}}).to_string(),
    })))
}

#[derive(Clone, Copy)]
enum Adapter {
    Search(search::Adapter),
    Fetch(fetch::Adapter),
}

struct Failure {
    status: StatusCode,
    message: String,
    terminal: bool,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            terminal: false,
        }
    }

    fn terminal(mut self) -> Self {
        self.terminal = true;
        self
    }

    fn response(self, kind: Kind) -> Response {
        match kind {
            Kind::Fetch => error(self.status, &self.message),
            Kind::Search => json_response(
                self.status,
                json!({"error":{"message": self.message, "code": self.status.as_u16()}}),
                None,
            ),
        }
    }
}

fn setting<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key]
        .as_str()
        .map(js_trim)
        .filter(|text| !text.is_empty())
}

fn or_else<'a>(first: &'a Value, fallback: &'a Value) -> &'a Value {
    if truthy(first) { first } else { fallback }
}

fn fit_text(text: &str, size: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(size).collect::<Vec<_>>())
}

fn secrets(account: &Value) -> impl Iterator<Item = &str> {
    ["apiKey", "accessToken", "refreshToken", "key", "token"]
        .into_iter()
        .filter_map(|field| {
            account[field]
                .as_str()
                .filter(|value| value.encode_utf16().count() >= 8)
        })
}

fn redact(message: &str, account: &Value) -> String {
    redact_secrets(redact_bearer(message), account)
}

fn redact_secrets(message: String, account: &Value) -> String {
    secrets(account).fold(message, |out, secret| {
        if out.contains(secret) {
            out.replace(secret, "[redacted]")
        } else {
            out
        }
    })
}

fn redact_payload(value: &mut Value, account: &Value) {
    match value {
        Value::String(text) => {
            for secret in secrets(account) {
                if text.contains(secret) {
                    *text = text.replace(secret, "[redacted]");
                }
            }
        }
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| redact_payload(value, account)),
        Value::Object(values) => values
            .values_mut()
            .for_each(|value| redact_payload(value, account)),
        _ => {}
    }
}

fn upstream_token<'a>(account: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|field| account[*field].as_str().filter(|value| !value.is_empty()))
}

async fn public_url(input: &str) -> Result<(), &'static str> {
    let url = Url::parse(input).map_err(|_| "Invalid URL format")?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err("Blocked URL: unsupported protocol");
    }
    let host = match url.host() {
        Some(Host::Ipv4(address)) if blocked(IpAddr::V4(address)) => {
            return Err("Blocked URL: internal host");
        }
        Some(Host::Ipv6(address)) if blocked(IpAddr::V6(address)) => {
            return Err("Blocked URL: internal host");
        }
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => return Ok(()),
        Some(Host::Domain(host)) => host.trim_end_matches('.'),
        None => return Err("Invalid URL format"),
    };
    if matches!(host, "localhost" | "ip6-localhost" | "ip6-loopback")
        || [".internal", ".local", ".localhost"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
    {
        return Err("Blocked URL: internal host");
    }
    if let Ok(addresses) =
        tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(80))).await
        && addresses.into_iter().any(|address| blocked(address.ip()))
    {
        return Err("Blocked URL: hostname resolves to an internal host");
    }
    Ok(())
}

fn blocked(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => blocked_v4(address),
        IpAddr::V6(address) => blocked_v6(address),
    }
}

fn blocked_v4(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    first == 0
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || (first == 100 && (64..=127).contains(&second))
}

fn blocked_v6(address: Ipv6Addr) -> bool {
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_unique_local()
        || address.is_unicast_link_local()
    {
        return true;
    }
    if let Some(mapped) = address.to_ipv4() {
        return blocked_v4(mapped);
    }
    let [start, group, a, b, c, d, high, low] = address.segments();
    start == 0x64
        && group == 0xff9b
        && [a, b, c, d] == [0; 4]
        && blocked_v4(Ipv4Addr::from((u32::from(high) << 16) | u32::from(low)))
}
