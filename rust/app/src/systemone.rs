use std::collections::HashSet;
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use serde_json::{Value, json};
use url::Url;

use crate::chat_accounts::{self, Retry, Selection};
use crate::chat_output::{error, js_text, json_response};
use crate::cli_video::redact_bearer;
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::provider_http::{self, Bound};
use crate::provider_index::js_trim;
use crate::provider_oauth::{self, RefreshMode};
use crate::proxy_transport::{self, Route, Selection as Egress};
use crate::storage::Storage;
use crate::{catalog_guard, chat_usage, key_policy, model_resolution};

#[path = "opencode_systemone.rs"]
mod opencode;

const BODY_LIMIT: usize = 128 * 1024 * 1024;

#[derive(Clone)]
struct SystemOneState {
    storage: Storage,
    client: reqwest::Client,
}

pub fn router(storage: Storage) -> Router {
    let state = SystemOneState {
        storage,
        client: reqwest::Client::builder()
            .no_proxy()
            .timeout(request_timeout())
            .redirect(crate::upstream::redirects())
            .build()
            .expect("System One HTTP client"),
    };
    let mut router = Router::new();
    for path in [
        "/systemone",
        "/v1/systemone",
        "/api/v1/systemone",
        "/v1/v1/systemone",
    ] {
        router = router.route(path, post(decisions).options(options));
    }
    router.with_state(state)
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

fn inference_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
        })
        .filter(|value| !value.is_empty())
}

enum Plan {
    Ready { provider: String, model: String },
    Denied,
    Missing,
}

async fn decisions(State(state): State<SystemOneState>, request: Request) -> Response {
    let policy_key = catalog_guard::api_key(&request).filter(|key| !key.is_empty());
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, BODY_LIMIT).await else {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    let text = String::from_utf8_lossy(&bytes);
    let Ok(body) = serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(&text))
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    if body.is_null() {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    }
    let Some(settings) = state.storage.read_settings().await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not load System One settings",
        );
    };
    let client_key = inference_key(&parts.headers);
    if settings.get("requireApiKey").is_none_or(truthy) {
        let Some(key) = client_key else {
            return error(StatusCode::UNAUTHORIZED, "Missing API key");
        };
        if !state.storage.api_key_is_active(key.to_owned()).await {
            return error(StatusCode::UNAUTHORIZED, "Invalid API key");
        }
    }
    if !truthy(&body["model"]) {
        return error(StatusCode::BAD_REQUEST, "Missing model");
    }
    if body["state"].is_null() {
        return error(StatusCode::BAD_REQUEST, "Missing required field: state");
    }
    if !body["questions"].is_object() {
        return error(StatusCode::BAD_REQUEST, "Missing required field: questions");
    }
    let Some(requested) = body["model"].as_str() else {
        return error(StatusCode::BAD_REQUEST, "Invalid model format");
    };
    let lookup = requested.to_owned();
    let plan = state
        .storage
        .run(false, move |db| {
            let Some((provider, model)) = model_resolution::get_model_info(db, &lookup)?
                .filter(|(provider, _)| !provider.is_empty())
            else {
                return Ok(Plan::Missing);
            };
            if let Some(key) = policy_key
                && !key_policy::permits_resolved(db, &key, &provider, &model)?
            {
                return Ok(Plan::Denied);
            }
            Ok(Plan::Ready { provider, model })
        })
        .await;
    let (provider, model) = match plan {
        Ok(Plan::Ready { provider, model }) => (provider, model),
        Ok(Plan::Denied) => {
            return error(
                StatusCode::FORBIDDEN,
                &format!("This API key is not allowed to use model \"{requested}\""),
            );
        }
        Ok(Plan::Missing) => return error(StatusCode::BAD_REQUEST, "Invalid model format"),
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to resolve the System One model",
            );
        }
    };
    let adapter = Adapter::resolve(&provider);
    let anonymous = provider == "opencode";
    let accounts = if anonymous {
        Vec::new()
    } else {
        let Ok(accounts) = chat_accounts::active_connections(&state.storage, &provider).await
        else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load provider credentials",
            );
        };
        accounts
    };

    let mut last = None;
    let mut excluded = HashSet::new();
    loop {
        let account = if anonymous {
            match opencode::public_account(&state.storage, &settings).await {
                Ok(account) => account,
                Err(()) => {
                    return error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Could not select a proxy pool for OpenCode System One",
                    );
                }
            }
        } else {
            match chat_accounts::select(&state.storage, &provider, &model, &excluded, None).await {
                Ok(Selection::Ready(account)) => account,
                Ok(Selection::CoolingDown(retry)) => {
                    return unavailable(retry, last, &provider, &model, &accounts);
                }
                Ok(Selection::Empty) => {
                    return last.map_or_else(
                        || {
                            error(
                                StatusCode::BAD_REQUEST,
                                &format!("No credentials for provider: {provider}"),
                            )
                        },
                        Failure::response,
                    );
                }
                Err(_) => {
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to select provider credentials",
                    );
                }
            }
        };
        let Some(connection) = account["id"].as_str() else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid provider connection",
            );
        };
        let decision = Decision {
            adapter,
            provider: &provider,
            model: &model,
            account: &account,
            body: &body,
        };
        match decision
            .send(&state.storage, &settings, &state.client)
            .await
        {
            Ok(value) => {
                let _ = chat_accounts::mark_success(&state.storage, connection, &model).await;
                if let Some(usage) = exact_usage(&value["usage"]) {
                    chat_usage::exact_callback(
                        state.storage.clone(),
                        provider,
                        model,
                        connection.to_owned(),
                        client_key.map(str::to_owned),
                        parts.uri.path().to_owned(),
                    )(usage);
                }
                return json_response(StatusCode::OK, value, None);
            }
            Err(failure) => {
                if failure.terminal {
                    return failure.response();
                }
                let retry = chat_accounts::mark_unavailable(
                    &state.storage,
                    &provider,
                    connection,
                    &model,
                    failure.status.as_u16(),
                    &failure.message,
                    None,
                )
                .await;
                if !retry.is_ok_and(|outcome| outcome.should_fallback) {
                    return failure.response();
                }
                excluded.insert(connection.to_owned());
                last = Some(failure);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Adapter {
    endpoints: Option<(&'static str, &'static str)>,
    path: Option<&'static str>,
}

impl Adapter {
    fn resolve(provider: &str) -> Self {
        let (endpoint, fixture, path) = match provider {
            "v1m" => ("https://v1m.ir/v1/systemone", "/v1m/v1/systemone", None),
            "openrouter" => (
                "https://openrouter.ai/api/v1/systemone",
                "/openrouter/api/v1/systemone",
                None,
            ),
            "ollama" => (
                "https://ollama.com/v1/systemone",
                "/ollama/v1/systemone",
                None,
            ),
            "ollama-local" => (
                "http://localhost:11434/v1/systemone",
                "/ollama-local/v1/systemone",
                Some("/v1/systemone"),
            ),
            "nace" => (
                "https://console.nace.ai/v1/systemone",
                "/nace/v1/systemone",
                Some("/v1/systemone"),
            ),
            "cloudflare-ai" => (
                "https://api.cloudflare.com/client/v4/accounts/{accountId}/ai/run/{model}",
                "/cloudflare/client/v4/accounts/{accountId}/ai/run/{model}",
                None,
            ),
            "opencode" | "opencode-zen" => (opencode::ENDPOINT, opencode::FIXTURE, None),
            _ => {
                return Self {
                    endpoints: None,
                    path: None,
                };
            }
        };
        Self {
            endpoints: Some((endpoint, fixture)),
            path,
        }
    }

    fn url(
        self,
        fixture: bool,
        provider: &str,
        model: &str,
        details: &Value,
    ) -> Result<String, Failure> {
        if truthy(&details["baseUrl"]) && !details["baseUrl"].is_string() {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "Invalid System One endpoint",
            ));
        }
        let mut url = match details["baseUrl"]
            .as_str()
            .map(js_trim)
            .filter(|url| !url.is_empty())
        {
            Some(base) => with_path(base, self.path),
            None => {
                let (endpoint, local) = self.endpoints.ok_or_else(|| {
                    Failure::new(
                        StatusCode::BAD_REQUEST,
                        format!("Provider '{provider}' does not support System One."),
                    )
                })?;
                if fixture {
                    format!("http://127.0.0.1:52989{local}")
                } else {
                    endpoint.to_owned()
                }
            }
        };
        if url.contains("{accountId}") {
            let account = details["accountId"].as_str().filter(|id| !id.is_empty());
            let Some(account) = account else {
                return Err(Failure::new(
                    StatusCode::BAD_REQUEST,
                    format!("Provider '{provider}' requires accountId in providerSpecificData."),
                ));
            };
            if !account
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
            {
                return Err(Failure::new(
                    StatusCode::BAD_REQUEST,
                    "Invalid provider account ID",
                ));
            }
            url = url.replacen("{accountId}", account, 1);
        }
        if url.contains("{model}") {
            if !model.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:@".contains(&byte))
            }) {
                return Err(Failure::new(
                    StatusCode::BAD_REQUEST,
                    "Invalid model format",
                ));
            }
            url = url.replace("{model}", model);
        }
        if protected_url(&url).is_none() {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "Invalid System One endpoint",
            ));
        }
        Ok(url)
    }
}

fn with_path(base: &str, path: Option<&str>) -> String {
    let Some(path) = path else {
        return base.to_owned();
    };
    let base = base.trim_end_matches('/');
    if base.ends_with(path) {
        return base.to_owned();
    }
    let (prefix, tail) = path.rsplit_once('/').unwrap_or(("", path));
    if !prefix.is_empty() && base.ends_with(prefix) {
        format!("{base}/{tail}")
    } else {
        format!("{base}{path}")
    }
}

#[derive(Clone, Copy)]
struct Decision<'a> {
    adapter: Adapter,
    provider: &'a str,
    model: &'a str,
    account: &'a Value,
    body: &'a Value,
}

impl Decision<'_> {
    async fn send(
        self,
        storage: &Storage,
        settings: &Value,
        direct: &reqwest::Client,
    ) -> Result<Value, Failure> {
        let (endpoint, delivery) = self.binding(storage, settings, direct).await?;
        let mut refreshed = None;
        let missing = credential(self.account, "accessToken").is_none();
        if self.account["authType"] == "oauth"
            && credential(self.account, "apiKey").is_none()
            && credential(self.account, "refreshToken").is_some()
            && (missing || provider_oauth::refresh_due_for_provider(self.provider, self.account))
            && let Some(mode) = refresh_mode(self.provider, &delivery, missing)
        {
            match provider_oauth::refresh_account(storage, self.account, mode).await {
                Ok(current) => {
                    let settings = storage.read_settings().await.ok_or_else(Failure::proxy)?;
                    let decision = Decision {
                        account: &current,
                        ..self
                    };
                    let (destination, route) = decision
                        .binding(storage, &settings, direct)
                        .await
                        .map_err(|_| Failure::proxy())?;
                    if current["isActive"] == false
                        || endpoint != destination
                        || !delivery.same_route(&route)
                    {
                        return Err(Failure::proxy());
                    }
                    refreshed = Some(current);
                }
                Err(provider_oauth::RefreshError::AccountUnavailable) => {
                    return Err(Failure::proxy());
                }
                Err(_) => (),
            }
        }
        let decision = Decision {
            account: refreshed.as_ref().unwrap_or(self.account),
            ..self
        };
        decision
            .request(delivery.request(reqwest::Method::POST), delivery.protected)
            .await
            .map_err(|mut failure| {
                if let Some(secret) = delivery.relay_secret() {
                    failure.message = failure.message.replace(secret, "[redacted]");
                }
                failure.message = redact(failure.message, self.account);
                failure.terminal = delivery.protected;
                failure
            })
    }

    async fn binding(
        self,
        storage: &Storage,
        settings: &Value,
        direct: &reqwest::Client,
    ) -> Result<(String, Bound), Failure> {
        let details = &self.account["providerSpecificData"];
        let endpoint = self
            .adapter
            .url(false, self.provider, self.model, details)?;
        if truthy(&details["connectionProxyEnabled"]) && details["connectionProxyEnabled"] != true {
            return Err(Failure::proxy());
        }
        let selection =
            proxy_transport::select_with_bypass(storage, settings, self.account, &endpoint)
                .await
                .map_err(|_| Failure::proxy())?;
        let proxy_configured = proxy_transport::account_requires_tunnel(self.account)
            || truthy(&settings["outboundProxyEnabled"])
                && settings
                    .get("outboundProxyUrl")
                    .is_some_and(|value| !value.is_null() && !js_trim(&js_text(value)).is_empty());
        let https = protected_url(&endpoint).is_some_and(|url| url.scheme() == "https");
        match &selection {
            Egress::Routed(Route::Direct) if !proxy_configured => (),
            Egress::ExplicitBypass if !truthy(&details["vercelRelayUrl"]) => (),
            Egress::Routed(Route::Proxy { url, .. })
                if https
                    && !truthy(&details["vercelRelayUrl"])
                    && proxy_transport::tunnel_proxy_url(url).is_some() =>
            {
                ()
            }
            Egress::Routed(Route::Relay {
                authorization: Some(_),
                ..
            }) if https => (),
            _ => return Err(Failure::proxy()),
        }
        let strict;
        let client = if matches!(selection, Egress::Routed(Route::Direct)) {
            direct
        } else {
            strict = protected_client().build().map_err(|_| Failure::proxy())?;
            &strict
        };
        let fixture = self.adapter.url(true, self.provider, self.model, details)?;
        let delivery = provider_http::bind_selection(
            selection,
            &endpoint,
            &fixture,
            client,
            request_timeout(),
            None,
        )
        .ok_or_else(Failure::proxy)?;
        Ok((endpoint, delivery))
    }

    async fn request(
        self,
        request: reqwest::RequestBuilder,
        protected: bool,
    ) -> Result<Value, Failure> {
        let Self {
            provider,
            model,
            account,
            body,
            ..
        } = self;
        let mut payload = body.clone();
        payload["model"] = json!(match (provider, model) {
            ("cloudflare-ai", "@cf/cloudflare/clef-flash") => "clef-flash",
            _ => model,
        });
        let mut request = request.json(&payload);
        if let Some(token) =
            credential(account, "apiKey").or_else(|| credential(account, "accessToken"))
        {
            request = request.bearer_auth(token);
        }
        if provider == "openrouter" {
            request = request
                .header("HTTP-Referer", "https://endpoint-proxy.local")
                .header("X-Title", "Endpoint Proxy");
        }
        if opencode::is_lane(provider) {
            request = opencode::headers(request).map_err(|_| {
                Failure::new(
                    StatusCode::BAD_GATEWAY,
                    "[502]: Could not initialize OpenCode session",
                )
            })?;
        }
        let response = request.send().await.map_err(|error| {
            let message = if error.is_timeout() {
                "The operation was aborted due to timeout"
            } else {
                "fetch failed"
            };
            Failure::new(StatusCode::BAD_GATEWAY, format!("[502]: {message}"))
        })?;
        let status = response.status();
        if protected && status.is_redirection() {
            return Err(Failure::new(
                StatusCode::BAD_GATEWAY,
                "System One endpoint returned an unexpected redirect",
            ));
        }
        let bytes = response.bytes().await.unwrap_or_default();
        let raw = String::from_utf8_lossy(&bytes);
        let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        let data = serde_json::from_str::<Value>(raw);
        if !status.is_success() {
            let reason = data
                .ok()
                .and_then(|json| {
                    [&json["error"]["message"], &json["message"], &json["error"]]
                        .into_iter()
                        .find(|value| truthy(value))
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string())
                        })
                })
                .unwrap_or_else(|| {
                    if raw.is_empty() {
                        default_error(status)
                    } else {
                        raw.to_owned()
                    }
                });
            let safe = redact(reason, account);
            return Err(Failure::new(
                status,
                format!("[{}]: {safe}", status.as_u16()),
            ));
        }
        data.map_err(|_| {
            Failure::new(
                StatusCode::BAD_GATEWAY,
                format!("Invalid JSON response from {provider}"),
            )
        })
    }
}

fn credential<'a>(account: &'a Value, field: &str) -> Option<&'a str> {
    account[field].as_str().filter(|value| !value.is_empty())
}

fn refresh_mode(provider: &str, delivery: &Bound, missing: bool) -> Option<RefreshMode> {
    if provider == "github" && delivery.protected {
        return Some(RefreshMode::github(
            missing,
            delivery.route.relay_identity(),
        ));
    }
    if let Some(identity) = delivery.route.relay_identity() {
        return matches!(
            provider,
            "kiro"
                | "grok-cli"
                | "cline"
                | "clinepass"
                | "codex"
                | "claude"
                | "gemini-cli"
                | "antigravity"
        )
        .then_some(RefreshMode::RelayRouted(identity));
    }
    Some(match (provider, delivery.protected) {
        ("kiro", true) => RefreshMode::KiroProtected,
        ("grok-cli", true) => RefreshMode::GrokProtected,
        ("kimi" | "iflow" | "codebuddy-cn" | "codebuddy-intl", true) => {
            RefreshMode::QuotaRouted { rejected: missing }
        }
        (_, true) => RefreshMode::Routed(true),
        ("grok-cli", false) => RefreshMode::Rejected,
        (_, false) if missing => RefreshMode::Rejected,
        _ => RefreshMode::Proactive,
    })
}

fn request_timeout() -> Duration {
    let milliseconds = std::env::var("FETCH_CONNECT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(60_000);
    Duration::from_millis(milliseconds)
}

fn protected_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(request_timeout())
        .connect_timeout(Duration::from_secs(10))
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
}

fn protected_url(endpoint: &str) -> Option<Url> {
    Url::parse(endpoint).ok().filter(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}

fn default_error(status: StatusCode) -> String {
    match status.as_u16() {
        400 => "Bad request",
        401 => "Invalid API key provided",
        402 => "Payment required",
        403 => "You exceeded your current quota",
        404 => "Model not found",
        406 => "Model not supported",
        429 => "Rate limit exceeded",
        500 => "Internal server error",
        502 => "Bad gateway - upstream provider error",
        503 => "Service temporarily unavailable",
        504 => "Gateway timeout",
        code => return format!("Upstream error: {code}"),
    }
    .into()
}

fn exact_usage(value: &Value) -> Option<Value> {
    if !truthy(value) {
        return None;
    }
    let zero = json!(0);
    let count = |key| {
        value
            .get::<&str>(key)
            .filter(|value| truthy(value))
            .unwrap_or(&zero)
    };
    let input = count("input_tokens");
    let output = count("output_tokens");
    let total = if input.is_string() || output.is_string() {
        json!(format!("{}{}", js_text(input), js_text(output)))
    } else if let Some(sum) = input
        .as_i64()
        .zip(output.as_i64())
        .and_then(|(input, output)| input.checked_add(output))
    {
        json!(sum)
    } else {
        let number = |value: &Value| {
            value
                .as_f64()
                .or_else(|| value.as_bool().map(|value| f64::from(u8::from(value))))
                .unwrap_or(f64::NAN)
        };
        json!(number(input) + number(output))
    };
    Some(json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":total}))
}

fn redact(message: String, account: &Value) -> String {
    ["accessToken", "refreshToken", "apiKey"]
        .into_iter()
        .filter_map(|field| account[field].as_str())
        .filter(|secret| secret.encode_utf16().count() >= 8)
        .fold(redact_bearer(&message), |out, secret| {
            out.replace(secret, "[redacted]")
        })
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

    fn proxy() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "System One connection proxy is unavailable".into(),
            terminal: true,
        }
    }

    fn response(self) -> Response {
        error(self.status, &self.message)
    }
}

fn unavailable(
    retry: Retry,
    last: Option<Failure>,
    provider: &str,
    model: &str,
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
    let safe = accounts.iter().fold(redact_bearer(reason), redact);
    let message = format!("[{provider}/{model}] {safe} ({})", retry.retry_after_human);
    into_http(Ok(json!({
        "status": status,
        "headers": {"content-type": "application/json", "retry-after": retry.retry_after_seconds.to_string()},
        "body": json!({"error": {"message": message}}).to_string(),
    })))
}

pub(crate) async fn evaluate_route(
    storage: &Storage,
    client: &reqwest::Client,
    provider: &str,
    model: &str,
    state: String,
    criteria: serde_json::Map<String, Value>,
) -> Option<Value> {
    let adapter = Adapter::resolve(provider);
    let settings = storage.read_settings().await?;
    let account = if provider == "opencode" {
        opencode::public_account(storage, &settings).await.ok()?
    } else {
        let Selection::Ready(account) =
            chat_accounts::select(storage, provider, model, &HashSet::new(), None)
                .await
                .ok()?
        else {
            return None;
        };
        account
    };
    let payload = json!({
        "model": model, "state":state,
        "questions":{"route":{"type":"choice",
             "instructions":"Which of these configured models is the most appropriate for this request?",
             "criteria":criteria}},
    });
    let decision = Decision {
        adapter,
        provider,
        model,
        account: &account,
        body: &payload,
    };
    tokio::time::timeout(
        Duration::from_secs(8),
        decision.send(storage, &settings, client),
    )
    .await
    .ok()?
    .ok()
}
