use std::borrow::Cow;
use std::collections::HashSet;
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use serde_json::{Value, json};
use url::Url;

use crate::chat_accounts::{self, Selection};
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::proxy_transport::{self, Route, Selection as Egress};
use crate::storage::Storage;
use crate::{catalog_guard, chat_usage, key_policy, model_resolution, ollama_local};

#[path = "embeddings_github.rs"]
mod github;

const BODY_LIMIT: usize = 128 * 1024 * 1024;
const OPENAI_BASE: &str = "https://api.openai.com/v1";
const GEMINI: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const GEMINI_FIXTURE: &str = "http://127.0.0.1:52989/gemini/v1beta/models";
const MISSING_SELF_HOSTED_BASE: &str = concat!(
    "Self-hosted Embedding needs an endpoint: set this connection's baseUrl to ",
    "the OpenAI base URL of your server, e.g. http://host:8080/v1 (note the /v1 — ",
    "\"/embeddings\" is appended to it). Refusing to fall back to api.openai.com, ",
    "which would send your input and API key to OpenAI."
);

#[derive(Clone)]
struct EmbeddingState {
    storage: Storage,
    client: reqwest::Client,
    protected: reqwest::Client,
    proxy_clients: proxy_transport::Clients,
    timeout: Duration,
}

pub fn router(storage: Storage) -> Router {
    let timeout = fetch_timeout();
    let state = EmbeddingState {
        storage,
        client: reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(timeout)
            .timeout(timeout)
            .redirect(crate::upstream::redirects())
            .build()
            .expect("embeddings HTTP client"),
        protected: crate::provider_http::http_client(timeout)
            .build()
            .expect("protected embeddings HTTP client"),
        proxy_clients: proxy_transport::Clients::default(),
        timeout,
    };
    let mut router = Router::new();
    for path in ["/v1/embeddings", "/api/v1/embeddings", "/v1/v1/embeddings"] {
        router = router.route(path, post(embeddings).options(options));
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

fn inference_key(headers: &HeaderMap) -> Option<String> {
    if let Some(key) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    {
        return Some(key.to_owned());
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
}

enum Plan {
    Invalid,
    Denied,
    Ready(String, String, Adapter),
}

async fn embeddings(State(state): State<EmbeddingState>, request: Request) -> Response {
    let policy_key = catalog_guard::api_key(&request);
    let client_key = inference_key(request.headers());
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, BODY_LIMIT).await else {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    if body.is_null() {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    }
    let Some(settings) = state.storage.read_settings().await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to read settings");
    };
    let require_key = settings.get("requireApiKey").is_none_or(truthy);
    if require_key {
        let Some(key) = client_key.as_ref().filter(|key| !key.is_empty()) else {
            return error(StatusCode::UNAUTHORIZED, "Missing API key");
        };
        if !state.storage.api_key_is_active(key.clone()).await {
            return error(StatusCode::UNAUTHORIZED, "Invalid API key");
        }
    }
    if !truthy(&body["model"]) {
        return error(StatusCode::BAD_REQUEST, "Missing model");
    }
    if !truthy(&body["input"]) {
        return error(StatusCode::BAD_REQUEST, "Missing required field: input");
    }
    let Some(requested) = body["model"].as_str().map(str::to_owned) else {
        return error(StatusCode::BAD_REQUEST, "Invalid model format");
    };
    let lookup = requested.clone();
    let plan = state
        .storage
        .run(false, move |db| {
            let Some((provider, model)) = model_resolution::get_model_info(db, &lookup)? else {
                return Ok(Plan::Invalid);
            };
            if provider.is_empty() {
                return Ok(Plan::Invalid);
            }
            if let Some(key) = policy_key.as_ref().filter(|key| !key.is_empty())
                && !key_policy::permits_resolved(db, key, &provider, &model)?
            {
                return Ok(Plan::Denied);
            }
            let selected = adapter(&provider);
            Ok(Plan::Ready(provider, model, selected))
        })
        .await;
    let (provider, model, adapter) = match plan {
        Ok(Plan::Invalid) => return error(StatusCode::BAD_REQUEST, "Invalid model format"),
        Ok(Plan::Denied) => {
            return error(
                StatusCode::FORBIDDEN,
                &format!("This API key is not allowed to use model \"{requested}\""),
            );
        }
        Ok(Plan::Ready(provider, model, adapter)) => (provider, model, adapter),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to resolve model"),
    };
    let mut excluded = HashSet::new();
    let mut last: Option<UpstreamError> = None;
    loop {
        let selected =
            match chat_accounts::select(&state.storage, &provider, &model, &excluded, None).await {
                Ok(selected) => selected,
                Err(_) => {
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to select provider credentials",
                    );
                }
            };
        let account = match selected {
            Selection::Empty => {
                if let Some(last) = last {
                    return error(last.status, &last.message);
                }
                return error(
                    StatusCode::BAD_REQUEST,
                    &format!("No credentials for provider: {provider}"),
                );
            }
            Selection::CoolingDown(retry) => {
                let status = last
                    .as_ref()
                    .map(|failure| failure.status)
                    .or_else(|| {
                        retry
                            .last_error_code
                            .and_then(|code| StatusCode::from_u16(code).ok())
                    })
                    .unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
                let reason = last
                    .as_ref()
                    .map(|failure| failure.message.as_str())
                    .or(retry.last_error.as_deref())
                    .unwrap_or("Unavailable");
                let message = format!(
                    "[{provider}/{model}] {reason} ({})",
                    retry.retry_after_human
                );
                let mut reply = json_response(status, json!({"error":{"message":message}}), false);
                if let Ok(value) = HeaderValue::from_str(&retry.retry_after_seconds.to_string()) {
                    reply.headers_mut().insert(header::RETRY_AFTER, value);
                }
                return reply;
            }
            Selection::Ready(account) => account,
        };
        let Some(connection) = account["id"].as_str() else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid provider connection",
            );
        };
        let response = fetch_embeddings(
            &state, &settings, adapter, &provider, &model, &account, &body,
        )
        .await;
        match response {
            Ok(value) => {
                let _ = chat_accounts::mark_success(&state.storage, connection, &model).await;
                if let Some(usage) = exact_usage(&value["usage"]) {
                    let callback = chat_usage::exact_callback(
                        state.storage.clone(),
                        provider.clone(),
                        model.clone(),
                        connection.to_owned(),
                        client_key,
                        parts.uri.path().to_owned(),
                    );
                    callback(usage);
                }
                return json_response(StatusCode::OK, value, true);
            }
            Err(failure) => {
                if failure.terminal {
                    return error(failure.status, &failure.message);
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
                    return error(failure.status, &failure.message);
                }
                excluded.insert(connection.to_owned());
                last = Some(failure);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Adapter {
    OpenAi(&'static str),
    Compatible,
    SelfHosted,
    Gemini,
    GitHub,
    OllamaLocal,
    Unsupported,
}

fn adapter(provider: &str) -> Adapter {
    if provider.starts_with("openai-compatible-") || provider.starts_with("custom-embedding-") {
        return Adapter::Compatible;
    }
    match provider {
        "gemini" | "google_ai_studio" => Adapter::Gemini,
        "ollama-local" => Adapter::OllamaLocal,
        "selfhosted-embedding" => Adapter::SelfHosted,
        "openai" => Adapter::OpenAi("https://api.openai.com/v1/embeddings"),
        "openrouter" => Adapter::OpenAi("https://openrouter.ai/api/v1/embeddings"),
        "mistral" => Adapter::OpenAi("https://api.mistral.ai/v1/embeddings"),
        "voyage-ai" => Adapter::OpenAi("https://api.voyageai.com/v1/embeddings"),
        "fireworks" => Adapter::OpenAi("https://api.fireworks.ai/inference/v1/embeddings"),
        "together" => Adapter::OpenAi("https://api.together.xyz/v1/embeddings"),
        "nebius" => Adapter::OpenAi("https://api.tokenfactory.nebius.com/v1/embeddings"),
        "nvidia" => Adapter::OpenAi("https://integrate.api.nvidia.com/v1/embeddings"),
        "jina-ai" => Adapter::OpenAi("https://api.jina.ai/v1/embeddings"),
        "vercel-ai-gateway" => Adapter::OpenAi("https://ai-gateway.vercel.sh/v1/embeddings"),
        "venice" => Adapter::OpenAi("https://api.venice.ai/api/v1/embeddings"),
        "tokenrouter" => Adapter::OpenAi("https://api.tokenrouter.com/v1/embeddings"),
        "github" => Adapter::GitHub,
        _ => Adapter::Unsupported,
    }
}

fn credential(account: &Value) -> Option<&str> {
    ["apiKey", "accessToken"]
        .iter()
        .find_map(|key| account[*key].as_str().filter(|value| !value.is_empty()))
}

struct UpstreamError {
    status: StatusCode,
    message: String,
    terminal: bool,
}

impl UpstreamError {
    fn bad_input(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            terminal: false,
        }
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
            terminal: false,
        }
    }

    fn fetch(error: reqwest::Error, protected: bool) -> Self {
        Self::bad_gateway(if error.is_timeout() {
            "[502]: The operation was aborted due to timeout"
        } else {
            "[502]: fetch failed"
        })
        .protected(protected)
    }

    fn protected(mut self, protected: bool) -> Self {
        self.terminal = protected;
        self
    }

    fn proxy() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Gemini embedding connection proxy is unavailable".into(),
            terminal: true,
        }
    }
}

async fn fetch_embeddings(
    state: &EmbeddingState,
    settings: &Value,
    adapter: Adapter,
    provider: &str,
    model: &str,
    account: &Value,
    body: &Value,
) -> Result<Value, UpstreamError> {
    let input = &body["input"];
    if !input.is_string() && !input.is_array() {
        return Err(UpstreamError::bad_input(
            "input must be a string or array of strings",
        ));
    }
    if provider == "venice"
        && input
            .as_array()
            .is_some_and(|items| items.iter().any(|item| !item.is_string()))
    {
        return Err(UpstreamError::bad_input(
            "Venice embeddings require text input",
        ));
    }
    if matches!(adapter, Adapter::GitHub) {
        return github::fetch(state, settings, account, &openai_body(model, body)).await;
    }
    if matches!(adapter, Adapter::Unsupported) {
        return Err(UpstreamError::bad_input(&format!(
            "Provider '{provider}' does not support embeddings."
        )));
    }
    let ollama = matches!(adapter, Adapter::OllamaLocal);
    let key = if ollama {
        account.get("apiKey").filter(|key| truthy(key)).map(|key| {
            key.as_str()
                .map_or_else(|| Cow::Owned(js_text(key)), Cow::Borrowed)
        })
    } else {
        Some(Cow::Borrowed(credential(account).ok_or_else(|| {
            UpstreamError {
                status: StatusCode::UNAUTHORIZED,
                message: format!("Missing embedding credentials for provider: {provider}"),
                terminal: false,
            }
        })?))
    };
    let (mut request, payload, protected, relay_secret) = if matches!(adapter, Adapter::Gemini) {
        let operation = if input.is_array() {
            "batchEmbedContents"
        } else {
            "embedContent"
        };
        let (request, protected, relay_secret) = gemini_request(
            state,
            settings,
            account,
            model.strip_prefix("models/").unwrap_or(model),
            operation,
            key.as_deref().expect("Gemini embedding credential"),
        )
        .await?;
        (request, gemini_body(model, body), protected, relay_secret)
    } else {
        let url = match adapter {
            Adapter::OpenAi(endpoint) => endpoint.to_owned(),
            Adapter::OllamaLocal => ollama_embedding_url(account, model)?,
            Adapter::Compatible | Adapter::SelfHosted => {
                let compatible = matches!(adapter, Adapter::Compatible);
                let (fallback, paths, message) = if compatible {
                    (
                        Some(OPENAI_BASE),
                        crate::upstream::OPENAI_NODE_PATHS,
                        "Invalid embeddings baseUrl",
                    )
                } else {
                    (
                        None,
                        crate::upstream::EMBEDDING_NODE_PATHS,
                        MISSING_SELF_HOSTED_BASE,
                    )
                };
                let base =
                    crate::upstream::connection_base(account, fallback).ok_or_else(|| {
                        UpstreamError::bad_input(&format!("[{provider}/{model}] {message}"))
                    })?;
                crate::upstream::node_url(base, "/embeddings", paths)
                    .ok_or_else(|| {
                        UpstreamError::bad_input(&format!(
                            "[{provider}/{model}] Invalid embeddings baseUrl"
                        ))
                    })?
                    .to_string()
            }
            Adapter::Unsupported => unreachable!("unsupported embedding route rejected above"),
            Adapter::Gemini => unreachable!("Gemini uses its own embedding payload"),
            Adapter::GitHub => unreachable!("GitHub uses its own embedding transport"),
        };
        let destination = Url::parse(&url)
            .ok()
            .filter(|endpoint| {
                matches!(endpoint.scheme(), "http" | "https")
                    && endpoint.has_host()
                    && endpoint.username().is_empty()
                    && endpoint.password().is_none()
            })
            .ok_or_else(|| UpstreamError::bad_gateway("[502]: fetch failed"))?;
        let route =
            proxy_transport::select(&state.storage, settings, account, destination.as_str())
                .await
                .map_err(|_| UpstreamError::bad_gateway("[502]: fetch failed"))?;
        let protected = matches!(route, Route::Relay { .. });
        if truthy(&account["providerSpecificData"]["vercelRelayUrl"]) && !protected {
            return Err(
                UpstreamError::bad_gateway("Embedding relay route is unavailable").protected(true),
            );
        }
        let request = match &route {
            Route::Direct if account["providerSpecificData"]["strictProxy"] == true => {
                return Err(UpstreamError::bad_gateway("[502]: fetch failed"));
            }
            Route::Direct => state.client.post(destination),
            Route::Proxy { url, .. } => state
                .proxy_clients
                .get(url, false, state.timeout)
                .map_err(|_| UpstreamError::bad_gateway("[502]: fetch failed"))?
                .post(destination),
            Route::Relay { url, .. } => state.protected.post(url),
            Route::MissingStrict => return Err(UpstreamError::bad_gateway("[502]: fetch failed")),
        };
        let relay_secret = match &route {
            Route::Relay { authorization, .. } => authorization.clone(),
            _ => None,
        };
        (
            route.relay(request),
            openai_body(model, body),
            protected,
            relay_secret,
        )
    };
    request = request.timeout(state.timeout).json(&payload);
    if !matches!(adapter, Adapter::Gemini)
        && let Some(key) = &key
    {
        request = request.bearer_auth(key);
    }
    if provider == "openrouter" {
        request = request
            .header("HTTP-Referer", "https://endpoint-proxy.local")
            .header("X-Title", "Endpoint Proxy");
    }
    let response = request
        .send()
        .await
        .map_err(|error| UpstreamError::fetch(error, protected))?;
    let secrets = key
        .as_deref()
        .into_iter()
        .chain(relay_secret.as_ref().and_then(|value| value.to_str().ok()));
    let terminal_response =
        protected && (matches!(adapter, Adapter::Gemini) || response.status().is_redirection());
    let value = read_response(
        response,
        provider,
        terminal_response,
        if matches!(adapter, Adapter::Gemini) {
            "Gemini embedding endpoint returned an unexpected redirect"
        } else {
            "Embedding relay returned an unexpected redirect"
        },
        secrets,
        ollama,
    )
    .await?;
    Ok(if matches!(adapter, Adapter::Gemini) {
        normalize_gemini(value, model)
    } else {
        value
    })
}

async fn read_response<'a>(
    response: reqwest::Response,
    provider: &str,
    protected: bool,
    redirect_error: &str,
    secrets: impl IntoIterator<Item = &'a str>,
    redact_bearer: bool,
) -> Result<Value, UpstreamError> {
    let status = response.status();
    if protected && status.is_redirection() {
        return Err(UpstreamError::bad_gateway(redirect_error).protected(true));
    }
    if !status.is_success() {
        let raw = response.text().await.unwrap_or_default();
        let value = serde_json::from_str::<Value>(&raw).ok();
        let message = value
            .as_ref()
            .and_then(|value| {
                [
                    &value["error"]["message"],
                    &value["message"],
                    &value["error"],
                ]
                .into_iter()
                .find(|value| truthy(value))
            })
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string())
            })
            .unwrap_or_else(|| {
                if raw.is_empty() {
                    default_error_message(status)
                } else {
                    raw
                }
            });
        let mut message = if redact_bearer {
            crate::cli_video::redact_bearer(&message)
        } else {
            message
        };
        for secret in secrets.into_iter().filter(|secret| !secret.is_empty()) {
            message = message.replace(secret, "[redacted]");
        }
        return Err(UpstreamError {
            status,
            message: format!("[{}]: {message}", status.as_u16()),
            terminal: protected,
        });
    }
    response.json::<Value>().await.map_err(|_| {
        UpstreamError::bad_gateway(format!("Invalid JSON response from {provider}"))
            .protected(protected)
    })
}

async fn gemini_request(
    state: &EmbeddingState,
    settings: &Value,
    account: &Value,
    model: &str,
    operation: &str,
    key: &str,
) -> Result<(reqwest::RequestBuilder, bool, Option<HeaderValue>), UpstreamError> {
    let destination = |base| Url::parse(&format!("{base}/{model}:{operation}"));
    let endpoint =
        destination(GEMINI).map_err(|_| UpstreamError::bad_gateway("[502]: fetch failed"))?;
    let direct = || {
        if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
            destination(GEMINI_FIXTURE)
                .map_err(|_| UpstreamError::bad_gateway("[502]: fetch failed"))
        } else {
            Ok(endpoint.clone())
        }
    };
    let egress =
        proxy_transport::select_with_bypass(&state.storage, settings, account, endpoint.as_str())
            .await
            .map_err(|_| UpstreamError::proxy())?;
    if truthy(&account["providerSpecificData"]["vercelRelayUrl"])
        && !matches!(
            egress,
            Egress::Routed(Route::Relay {
                authorization: Some(_),
                ..
            })
        )
    {
        return Err(UpstreamError::proxy());
    }
    if matches!(egress, Egress::Routed(Route::Direct))
        && !gemini_proxy_configured(settings, account)
    {
        let mut url = direct()?;
        url.query_pairs_mut().append_pair("key", key);
        return Ok((state.client.post(url), false, None));
    }
    if model.is_empty()
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte))
    {
        return Err(UpstreamError::bad_input("Invalid Gemini embedding model").protected(true));
    }
    let client = || {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(state.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
    };
    let (request, secret) = match &egress {
        Egress::ExplicitBypass => (
            client()
                .build()
                .map_err(|_| UpstreamError::proxy())?
                .post(direct()?),
            None,
        ),
        Egress::Routed(Route::Proxy { url, .. }) => {
            let proxy = proxy_transport::tunnel_proxy_url(url).ok_or_else(UpstreamError::proxy)?;
            let client = proxy_transport::vendor_metadata_client(proxy, client())
                .ok_or_else(UpstreamError::proxy)?;
            (client.post(endpoint), None)
        }
        Egress::Routed(
            route @ Route::Relay {
                url,
                authorization: Some(secret),
                ..
            },
        ) => (route.relay(state.client.post(url)), Some(secret.clone())),
        _ => return Err(UpstreamError::proxy()),
    };
    let mut header = HeaderValue::from_str(key).map_err(|_| UpstreamError::proxy())?;
    header.set_sensitive(true);
    Ok((request.header("x-goog-api-key", header), true, secret))
}

fn gemini_proxy_configured(settings: &Value, account: &Value) -> bool {
    proxy_transport::account_requires_tunnel(account)
        || truthy(&settings["outboundProxyEnabled"]) && truthy(&settings["outboundProxyUrl"])
        || std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() != Ok("1")
            && ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
                .iter()
                .any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
}

fn ollama_embedding_url(account: &Value, model: &str) -> Result<String, UpstreamError> {
    let Some(base) = ollama_local::host(account) else {
        return Err(UpstreamError::bad_input(&format!(
            "[ollama-local/{model}] credentials?.providerSpecificData?.baseUrl?.trim is not a function"
        )));
    };
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix("/v1/embeddings").unwrap_or(base);
    let base = base.strip_suffix("/v1").unwrap_or(base);
    let endpoint = format!("{base}/v1/embeddings");
    if Url::parse(&endpoint).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
    }) {
        Ok(endpoint)
    } else {
        Err(UpstreamError::bad_gateway("[502]: fetch failed"))
    }
}

fn openai_body(model: &str, body: &Value) -> Value {
    let mut request = json!({"model":model,"input":body["input"]});
    request["encoding_format"] = if truthy(&body["encoding_format"]) {
        body["encoding_format"].clone()
    } else {
        json!("float")
    };
    if let Some(dimensions) = normalized_dimensions(&body["dimensions"]) {
        request["dimensions"] = dimensions;
    }
    request
}

fn gemini_body(model: &str, body: &Value) -> Value {
    let model = if model.starts_with("models/") {
        model.to_owned()
    } else {
        format!("models/{model}")
    };
    let dimensions = normalized_dimensions(&body["dimensions"]);
    let request = |text: &Value| {
        let mut object = json!({"model":model,"content":{"parts":[{"text":js_text(text)}]}});
        if let Some(dimensions) = &dimensions {
            object["outputDimensionality"] = dimensions.clone();
        }
        object
    };
    match &body["input"] {
        Value::Array(values) => json!({"requests":values.iter().map(request).collect::<Vec<_>>()}),
        value => request(value),
    }
}

fn normalize_gemini(body: Value, model: &str) -> Value {
    if body["object"] == "list" && body["data"].is_array() {
        return body;
    }
    let mut data = Vec::new();
    if let Some(embeddings) = body["embeddings"].as_array() {
        for (index, embed) in embeddings.iter().enumerate() {
            let values = if truthy(&embed["values"]) {
                embed["values"].clone()
            } else {
                json!([])
            };
            data.push(json!({"object":"embedding","index":index,"embedding":values}));
        }
    } else if truthy(&body["embedding"]["values"]) {
        data.push(json!({"object":"embedding","index":0,"embedding":body["embedding"]["values"]}));
    }
    json!({"object":"list","data":data,"model":model,"usage":{"prompt_tokens":0,"total_tokens":0}})
}

fn normalized_dimensions(value: &Value) -> Option<Value> {
    let n = if let Some(n) = value.as_f64() {
        n
    } else if let Some(flag) = value.as_bool() {
        if flag { 1.0 } else { 0.0 }
    } else if value.is_string() || value.is_array() {
        let string = js_text(value);
        let string = js_trim(&string);
        let radix = match string.get(..2) {
            Some("0x" | "0X") => Some(16),
            Some("0b" | "0B") => Some(2),
            Some("0o" | "0O") => Some(8),
            _ => None,
        };
        match radix {
            Some(radix) => u64::from_str_radix(&string[2..], radix).ok()? as f64,
            None => string.parse::<f64>().ok()?,
        }
    } else {
        return None;
    };
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    Some(if n.fract() == 0.0 && n < 9_007_199_254_740_992.0 {
        json!(n as i64)
    } else {
        json!(n)
    })
}

fn exact_usage(value: &Value) -> Option<Value> {
    if !value.is_object() || value["estimated"] == true {
        return None;
    }
    let prompt = value
        .get("prompt_tokens")
        .filter(|v| !v.is_null())
        .unwrap_or(&value["input_tokens"]);
    let n = prompt.as_f64()?;
    let completion = value
        .get("completion_tokens")
        .filter(|v| !v.is_null())
        .or_else(|| value.get("output_tokens").filter(|v| !v.is_null()));
    if n <= 0.0
        || n > 9_007_199_254_740_991.0
        || n.fract() != 0.0
        || completion.is_some_and(|v| v.as_f64() != Some(0.0))
        || value["total_tokens"].as_f64() != Some(n)
    {
        return None;
    }
    Some(json!({"prompt_tokens":n as u64,"completion_tokens":0,"total_tokens":n as u64}))
}

fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) => "[object Object]".into(),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    js_text(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Number(number) => number.as_f64().map_or_else(
            || number.to_string(),
            |n| {
                if n == 0.0 {
                    "0".into()
                } else if n.abs() >= 1e21 {
                    format!("{n:e}").replace('e', "e+")
                } else if n.abs() < 1e-6 {
                    format!("{n:e}")
                } else {
                    n.to_string()
                }
            },
        ),
        _ => value.to_string(),
    }
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

fn fetch_timeout() -> Duration {
    let raw = std::env::var("FETCH_CONNECT_TIMEOUT_MS").ok();
    let timeout = raw
        .as_deref()
        .and_then(|text| {
            let text = js_trim(text);
            let text = text.strip_prefix('+').unwrap_or(text);
            let end = text.bytes().take_while(u8::is_ascii_digit).count();
            text[..end].parse::<u64>().ok().filter(|ms| *ms > 0)
        })
        .unwrap_or(60_000);
    Duration::from_millis(timeout)
}

fn default_error_message(status: StatusCode) -> String {
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

fn error(status: StatusCode, message: &str) -> Response {
    let (kind, code) = match status.as_u16() {
        400 => ("invalid_request_error", "bad_request"),
        401 => ("authentication_error", "invalid_api_key"),
        402 => ("billing_error", "payment_required"),
        403 => ("permission_error", "insufficient_quota"),
        404 => ("invalid_request_error", "model_not_found"),
        406 => ("invalid_request_error", "model_not_supported"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        502 => ("server_error", "bad_gateway"),
        503 => ("server_error", "service_unavailable"),
        504 => ("server_error", "gateway_timeout"),
        code if code >= 500 => ("server_error", "internal_server_error"),
        _ => ("invalid_request_error", ""),
    };
    json_response(
        status,
        json!({"error":{"message":message,"type":kind,"code":code}}),
        true,
    )
}

fn json_response(status: StatusCode, body: Value, cors: bool) -> Response {
    let mut headers = json!({"content-type":"application/json"});
    if cors {
        headers["access-control-allow-origin"] = json!("*");
    }
    into_http(Ok(
        json!({"status":status.as_u16(),"headers":headers,"body":body.to_string()}),
    ))
}
