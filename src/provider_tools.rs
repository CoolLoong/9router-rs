use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rusqlite::TransactionBehavior;
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_connections::{self as accounts, truthy};
use crate::provider_index::{Compatible, compatible, js_trim};
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

#[path = "provider_oauth_catalog.rs"]
mod oauth_catalog;
#[path = "provider_oauth_probe.rs"]
mod oauth_probe;
#[path = "provider_model_probes.rs"]
mod probes;
#[path = "provider_test_batch.rs"]
mod test_batch;
#[path = "provider_validate.rs"]
mod validate;

static DISCOVERY: OnceLock<Value> = OnceLock::new();

#[derive(Clone)]
struct ProviderTools {
    storage: Storage,
    http: reqwest::Client,
    proxy_http: proxy_transport::Clients,
    gateway: String,
}

pub fn router(storage: Storage, listener: SocketAddr) -> Router {
    let peer = match listener.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), listener.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), listener.port())
        }
        _ => listener,
    };
    let state = ProviderTools {
        storage,
        gateway: format!("http://{peer}"),
        proxy_http: proxy_transport::Clients::default(),
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(crate::upstream::redirects())
            .timeout(Duration::from_secs(15))
            .build()
            .expect("provider testing HTTP client"),
    };
    Router::new()
        .route("/api/providers/{id}/models", get(models))
        .route("/api/providers/{id}/test", post(test))
        .route("/api/providers/test-batch", post(test_batch::run))
        .route("/api/providers/validate", post(validate::run))
        .route("/api/providers/{id}/test-models", post(probes::models))
        .route("/api/models/test", post(probes::model))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<ProviderTools>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({"error":"Unauthorized"})),
    )
        .into_response()
}

fn reply(status: StatusCode, body: Value) -> Response {
    into_http(Ok(json!({
        "status":status.as_u16(),"headers":{"content-type":"application/json"},"body":body.to_string(),
    })))
}

fn error(status: StatusCode, text: impl Into<String>) -> Response {
    reply(status, json!({"error":text.into()}))
}

fn missing() -> Response {
    error(StatusCode::NOT_FOUND, "Connection not found")
}

async fn account(state: &ProviderTools, id: String) -> Result<Option<Value>, String> {
    state
        .storage
        .run(false, move |db| accounts::by_id(db, &id))
        .await
}

async fn models(State(state): State<ProviderTools>, Path(id): Path<String>) -> Response {
    let connection = match account(&state, id.clone()).await {
        Ok(Some(connection)) => connection,
        Ok(None) => return missing(),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch models"),
    };
    let provider = connection["provider"].as_str().unwrap_or_default();
    if crate::openai_models::media_models_endpoint(provider).is_some() {
        let mut result = json!({"provider":provider,"connectionId":id,"models":[]});
        if let Some(models) = crate::openai_models::media_models(&state.storage, &connection)
            .await
            .filter(|models| !models.is_empty())
        {
            result["models"] = json!(models);
        } else {
            let registry = crate::openai_models::data();
            let alias = registry["transportAlias"][provider]
                .as_str()
                .unwrap_or(provider);
            if registry["models"][alias].is_array() {
                result["models"] = registry["models"][alias].clone();
            }
            result["warning"] = json!("No live models returned; showing the stored catalog.");
        }
        return reply(StatusCode::OK, result);
    }
    match provider {
        "gemini-cli" => {
            return oauth_catalog::models(
                &state,
                &connection,
                &id,
                oauth_catalog::Catalog::GeminiCli,
            )
            .await;
        }
        "codex" => {
            return oauth_catalog::models(&state, &connection, &id, oauth_catalog::Catalog::Codex)
                .await;
        }
        _ => {}
    }
    if provider == "nous" {
        let token = [
            &connection["providerSpecificData"]["copilotToken"],
            &connection["accessToken"],
            &connection["apiKey"],
        ]
        .into_iter()
        .find(|token| truthy(token))
        .map(js_text);
        let Some(token) = token else {
            return error(StatusCode::UNAUTHORIZED, "No valid token found");
        };
        return match crate::nous_management::models(&state.storage, &token).await {
            Ok(models) => {
                let mut models = Value::Array(models);
                scrub_payload(&mut models, &secrets(&connection));
                reply(
                    StatusCode::OK,
                    json!({"provider":provider,"connectionId":id,"models":models}),
                )
            }
            Err(None) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch models"),
            Err(Some(status)) => error(
                status,
                format!("Failed to fetch models: {}", status.as_u16()),
            ),
        };
    }
    if crate::qoder_models::supports(provider) {
        let models = match state.storage.read_settings().await {
            Some(settings) => {
                crate::qoder_models::models(&state.storage, &settings, &connection, true)
                    .await
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };
        let mut output = json!({"provider":provider,"connectionId":id,"models":models});
        if models.is_empty() {
            output["warning"] = json!("Qoder returned no models; falling back to static catalog.");
        }
        scrub_payload(&mut output["models"], &secrets(&connection));
        return reply(StatusCode::OK, output);
    }
    if provider == "kimchi" {
        let live = match state.storage.read_settings().await {
            Some(settings) => {
                crate::kimchi_models::models(&state.storage, &settings, &connection, true).await
            }
            None => None,
        };
        let mut output = json!({"provider":provider,"connectionId":id,"models":[]});
        if let Some(live) = live {
            output["models"] = json!(live);
        } else {
            let registry = crate::openai_models::data();
            let alias = registry["transportAlias"][provider]
                .as_str()
                .unwrap_or(provider);
            output["models"] = registry["models"][alias].clone();
            output["warning"] =
                json!("Kimchi returned no live models; falling back to static catalog.");
        }
        scrub_payload(&mut output["models"], &secrets(&connection));
        return reply(StatusCode::OK, output);
    }
    if crate::bedrock_transport::is_provider(provider) {
        return error(
            StatusCode::BAD_REQUEST,
            format!("Provider {provider} does not support models listing"),
        );
    }
    if provider == "grok-cli" {
        use crate::grok_cli_models::Discovery;
        match crate::grok_cli_models::discover(&state.storage, &connection).await {
            Discovery::Models(models) => {
                return reply(
                    StatusCode::OK,
                    json!({"provider":provider,"connectionId":id,"models":models}),
                );
            }
            Discovery::Unavailable(warning) => {
                let registry = crate::openai_models::data();
                let alias = registry["transportAlias"][provider]
                    .as_str()
                    .unwrap_or(provider);
                return reply(
                    StatusCode::OK,
                    json!({"provider":provider,"connectionId":id,
                    "models":registry["models"][alias],"warning":warning}),
                );
            }
        }
    }
    if provider == "commandcode" {
        if !truthy(&connection["apiKey"]) && !truthy(&connection["accessToken"]) {
            return error(StatusCode::UNAUTHORIZED, "No valid token found");
        }
        let live = match state.storage.read_settings().await {
            Some(settings) => crate::commandcode_transport::models(&state.storage, &settings).await,
            None => None,
        };
        let mut output = json!({"provider":provider,"connectionId":id,"models":[]});
        if let Some(live) = live {
            output["models"] = json!(live);
        } else {
            let registry = crate::openai_models::data();
            let alias = registry["transportAlias"][provider]
                .as_str()
                .unwrap_or(provider);
            output["models"] = registry["models"][alias].clone();
            output["warning"] =
                json!("CommandCode returned no live models; falling back to static catalog.");
        }
        return reply(StatusCode::OK, output);
    }
    if provider == "cursor" {
        let live = match state.storage.read_settings().await {
            Some(settings) => {
                crate::openai_models::cursor_live(&state.storage, &settings, &connection, true)
                    .await
            }
            None => None,
        };
        let mut output = json!({"provider":provider,"connectionId":id,"models":[]});
        if let Some(live) = live {
            output["models"] = json!(live);
        } else {
            output["models"] = crate::openai_models::data()["models"]["cu"].clone();
            output["warning"] =
                json!("Cursor returned no live models; falling back to static catalog.");
        }
        scrub_payload(&mut output["models"], &secrets(&connection));
        return reply(StatusCode::OK, output);
    }
    if provider == "zed" {
        let mut output = json!({"provider":provider,"connectionId":id,"models":[]});
        let Some(settings) = state.storage.read_settings().await else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load Zed settings",
            );
        };
        match crate::zed_transport::models(&state.storage, &settings, &connection, true).await {
            Ok(models) if !models.is_empty() => output["models"] = json!(models),
            Ok(_) => output["warning"] = json!("Zed returned no live models."),
            Err(error) => output["warning"] = json!(format!("Failed to fetch Zed models: {error}")),
        }
        let private = secrets(&connection);
        scrub_payload(&mut output["models"], &private);
        if let Some(warning) = output.get_mut("warning") {
            scrub_payload(warning, &private);
        }
        return reply(StatusCode::OK, output);
    }
    if provider == "kiro" {
        use crate::kiro_models::Discovery;
        let discovery = if let Some(settings) = state.storage.read_settings().await {
            crate::kiro_models::discover(&state.storage, &settings, &connection).await
        } else {
            Discovery::Unavailable
        };
        let mut output = json!({"provider":provider,"connectionId":id,"models":[]});
        match discovery {
            Discovery::Models(models) => output["models"] = json!(models),
            Discovery::Unavailable => {
                output["warning"] =
                    json!("Kiro returned no models; falling back to static catalog.");
            }
        }
        scrub_payload(&mut output["models"], &secrets(&connection));
        return reply(StatusCode::OK, output);
    }
    if crate::cline_transport::is_provider(provider) {
        let live =
            crate::cline_transport::routed_models(&state.storage, provider, &connection).await;
        let mut output = json!({"provider":provider,"connectionId":id,"models":[]});
        if let Some(models) = live {
            output["models"] = json!(models);
        } else {
            let data = crate::openai_models::data();
            let alias = data["transportAlias"][provider]
                .as_str()
                .unwrap_or(provider);
            output["models"] = data["models"][alias].clone();
            let label = if provider == "clinepass" {
                "ClinePass"
            } else {
                "Cline"
            };
            output["warning"] = json!(format!(
                "{label} returned no live models; falling back to static catalog."
            ));
        }
        let mut private = secrets(&connection);
        if let Some(token) = connection["accessToken"].as_str() {
            let trimmed = js_trim(token);
            if !trimmed.is_empty() && trimmed != token {
                private.push(trimmed.to_owned());
            }
        }
        scrub_payload(&mut output["models"], &private);
        return reply(StatusCode::OK, output);
    }
    match discover(&state, &connection).await {
        Ok(models) => reply(
            StatusCode::OK,
            json!({
                "provider":provider,"connectionId":id,"models":models,
            }),
        ),
        Err((status, text)) => error(status, text),
    }
}

type DiscoveryResult = Result<Value, (StatusCode, String)>;

async fn gemini_models(state: &ProviderTools, connection: &Value) -> DiscoveryResult {
    let auth = connection["apiKey"]
        .as_str()
        .filter(|key| connection["authType"] == "apikey" && !key.is_empty())
        .and_then(|key| header::HeaderValue::from_str(key).ok());
    let Some(mut auth) = auth else {
        return Err((StatusCode::UNAUTHORIZED, "No valid token found".into()));
    };
    auth.set_sensitive(true);
    let endpoint = if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
        "http://127.0.0.1:52989/gemini/v1beta/models"
    } else {
        "https://generativelanguage.googleapis.com/v1beta/models"
    };
    let request = state
        .http
        .get(endpoint)
        .header("x-goog-api-key", auth)
        .header(header::CONTENT_TYPE, "application/json");
    let mut body = fetch_catalog_json(state, connection, request).await?;
    let mut models = if truthy(&body["models"]) {
        body["models"].take()
    } else {
        json!([])
    };
    scrub_payload(&mut models, &secrets(connection));
    Ok(models)
}

async fn antigravity_models(state: &ProviderTools, connection: &Value) -> DiscoveryResult {
    let Some(token) = discovery_token(connection) else {
        return Err((StatusCode::UNAUTHORIZED, "No valid token found".into()));
    };
    let endpoint = if crate::provider_http::fixture() {
        "http://127.0.0.1:52989/provider-catalog/antigravity"
    } else {
        "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:models"
    };
    let request = state
        .http
        .post(endpoint)
        .bearer_auth(token)
        .json(&json!({}));
    let mut catalog = fetch_catalog_json(state, connection, request).await?;
    let mut models = if truthy(&catalog["models"]) {
        catalog["models"].take()
    } else {
        json!([])
    };
    scrub_payload(&mut models, &secrets(connection));
    Ok(models)
}

async fn claude_models(state: &ProviderTools, connection: &Value) -> DiscoveryResult {
    let Some(token) = discovery_token(connection) else {
        return Err((StatusCode::UNAUTHORIZED, "No valid token found".into()));
    };
    let endpoint = if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
        "http://127.0.0.1:52989/provider-catalog/claude?limit=1000"
    } else {
        "https://api.anthropic.com/v1/models?limit=1000"
    };
    let request = state
        .http
        .get(endpoint)
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header(header::CONTENT_TYPE, "application/json");
    let body = fetch_catalog_json(state, connection, request).await?;
    let models = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let id = row["id"].as_str().filter(|id| !id.is_empty())?;
            let name = row["display_name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            Some(json!({"id":id,"name":name}))
        })
        .collect();
    let mut models = Value::Array(models);
    scrub_payload(&mut models, &secrets(connection));
    Ok(models)
}

async fn fetch_catalog_json(
    state: &ProviderTools,
    connection: &Value,
    request: reqwest::RequestBuilder,
) -> DiscoveryResult {
    const CATALOG_LIMIT: usize = 2 * 1024 * 1024;
    let invalid = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch models".into(),
        )
    };
    let mut response = catalog_fetch(state, connection, request)
        .await
        .map_err(|_| invalid())?;
    if !response.status().is_success() {
        let status = response.status();
        return Err((
            status,
            format!("Failed to fetch models: {}", status.as_u16()),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| invalid())? {
        if chunk.len() > CATALOG_LIMIT - bytes.len() {
            return Err(invalid());
        }
        bytes.extend_from_slice(&chunk);
    }
    let body: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if body.is_null() {
        return Err(invalid());
    }
    Ok(body)
}

async fn catalog_fetch(
    state: &ProviderTools,
    connection: &Value,
    builder: reqwest::RequestBuilder,
) -> Result<reqwest::Response, ()> {
    let settings = state.storage.read_settings().await.ok_or(())?;
    crate::provider_http::fetch(
        &state.storage,
        &settings,
        connection,
        &state.proxy_http,
        builder,
        Duration::from_secs(15),
    )
    .await
    .map_err(|_| ())
}

fn discovery_url(provider: &str) -> Option<&'static str> {
    if let Some((_, address, fixture)) = [
        (
            "alicode",
            "https://coding.dashscope.aliyuncs.com/v1/models",
            "http://127.0.0.1:52989/provider-catalog/alicode",
        ),
        (
            "alicode-intl",
            "https://coding-intl.dashscope.aliyuncs.com/v1/models",
            "http://127.0.0.1:52989/provider-catalog/alicode-intl",
        ),
        (
            "alims-intl",
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/models",
            "http://127.0.0.1:52989/provider-catalog/alims-intl",
        ),
    ]
    .into_iter()
    .find(|(name, _, _)| *name == provider)
    {
        return Some(
            if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
                fixture
            } else {
                address
            },
        );
    }
    if provider == "muse" {
        return Some(
            if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
                "http://127.0.0.1:52989/muse-chat/v1/models"
            } else {
                "https://api.meta.ai/v1/models"
            },
        );
    }
    DISCOVERY.get_or_init(|| {
        serde_json::from_str(include_str!("provider-discovery.json"))
            .expect("embedded provider discovery URLs")
    })[provider]
        .as_str()
}

fn js_field(value: &Value, field: &str) -> String {
    value.get(field).map_or_else(|| "undefined".into(), js_text)
}

fn ollama_host(connection: &Value) -> &str {
    let host = connection["providerSpecificData"]["baseUrl"]
        .as_str()
        .map(js_trim)
        .filter(|url| !url.is_empty())
        .unwrap_or("http://localhost:11434");
    host.strip_suffix('/').unwrap_or(host)
}

fn discovery_token(connection: &Value) -> Option<String> {
    [
        &connection["providerSpecificData"]["copilotToken"],
        &connection["accessToken"],
        &connection["apiKey"],
    ]
    .into_iter()
    .find(|token| truthy(token))
    .map(js_text)
}

async fn discover(state: &ProviderTools, connection: &Value) -> DiscoveryResult {
    let provider = connection["provider"].as_str().unwrap_or_default();
    if provider == "gemini" {
        return gemini_models(state, connection).await;
    }
    if provider == "antigravity" {
        return antigravity_models(state, connection).await;
    }
    if provider == "claude" {
        return claude_models(state, connection).await;
    }
    if provider == "github" {
        let Some(settings) = state.storage.read_settings().await else {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch models".into(),
            ));
        };
        return crate::copilot_models::for_management(&state.storage, &settings, connection)
            .await
            .map(|models| {
                let mut value = Value::Array(models);
                scrub_payload(&mut value, &secrets(connection));
                value
            });
    }
    let kind = compatible(provider);
    let configured_base = &connection["providerSpecificData"]["baseUrl"];
    if truthy(configured_base)
        && !configured_base.is_string()
        && (provider == "ollama-local" || kind.is_some())
    {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch models".into(),
        ));
    }
    let key = if truthy(&connection["apiKey"]) {
        js_field(connection, "apiKey")
    } else {
        js_field(connection, "accessToken")
    };
    let selected = match kind {
        Some(kind @ (Compatible::OpenAi | Compatible::Anthropic)) => {
            let Some(base) = crate::upstream::connection_base(connection, None) else {
                let style = if kind == Compatible::OpenAi {
                    "OpenAI"
                } else {
                    "Anthropic"
                };
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("No base URL configured for {style} compatible provider"),
                ));
            };
            let Some(endpoint) =
                crate::upstream::node_url(base, "/models", crate::upstream::compatible_paths(kind))
            else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Invalid compatible provider base URL".into(),
                ));
            };
            let mut request = state.http.get(endpoint).bearer_auth(&key);
            if kind == Compatible::Anthropic {
                request = request
                    .header("x-api-key", &key)
                    .header("anthropic-version", "2023-06-01");
            }
            request
        }
        _ if provider == "ollama-local" => state
            .http
            .get(format!("{}/api/tags", ollama_host(connection))),
        _ => {
            let Some(url) = discovery_url(provider).or_else(|| {
                (provider == "anthropic").then_some("https://api.anthropic.com/v1/models")
            }) else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("Provider {provider} does not support models listing"),
                ));
            };
            let Some(token) = discovery_token(connection) else {
                return Err((StatusCode::UNAUTHORIZED, "No valid token found".into()));
            };
            if provider == "anthropic" {
                state
                    .http
                    .get(url)
                    .header("x-api-key", token)
                    .header("anthropic-version", "2023-06-01")
            } else if provider == "muse" {
                state
                    .http
                    .get(url)
                    .bearer_auth(token)
                    .header("x-api-version", "1.0.0")
            } else {
                state.http.get(url).bearer_auth(token)
            }
        }
    };
    let internal_error = (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Failed to fetch models".to_owned(),
    );
    async {
        let request = selected.header("content-type", "application/json");
        let response = catalog_fetch(state, connection, request)
            .await
            .map_err(|_| internal_error.clone())?;
        if !response.status().is_success() {
            return Err((
                response.status(),
                format!("Failed to fetch models: {}", response.status().as_u16()),
            ));
        }
        let body: Value = response.json().await.map_err(|_| internal_error)?;
        if body.is_null() {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch models".into(),
            ));
        }
        let fields: &[&str] = if matches!(kind, Some(Compatible::OpenAi | Compatible::Anthropic)) {
            &["data", "models"]
        } else if matches!(
            provider,
            "anthropic" | "muse" | "alicode" | "alicode-intl" | "alims-intl"
        ) {
            &["data"]
        } else {
            &["data", "models", "results"]
        };
        let mut models = if fields.contains(&"results") && body.is_array() {
            body
        } else {
            fields
                .iter()
                .find_map(|field| body.get(*field).filter(|value| truthy(value)))
                .cloned()
                .unwrap_or(json!([]))
        };
        scrub_payload(&mut models, &secrets(connection));
        Ok(models)
    }
    .await
}

fn secrets(connection: &Value) -> Vec<String> {
    [
        "apiKey",
        "accessToken",
        "refreshToken",
        "idToken",
        "copilotToken",
        "oauthAccessToken",
        "firebaseIdToken",
    ]
    .iter()
    .flat_map(|name| {
        [
            connection.get(*name),
            connection["providerSpecificData"].get(*name),
        ]
    })
    .flatten()
    .filter_map(Value::as_str)
    .filter(|value| !value.is_empty())
    .map(ToOwned::to_owned)
    .collect()
}

fn redact(mut text: String, secrets: &[String]) -> String {
    for secret in secrets {
        if text.contains(secret) {
            text = text.replace(secret, "[redacted]");
        }
    }
    text
}

pub(super) fn scrub_payload(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => scrub_text(text, secrets),
        Value::Array(rows) => {
            for row in rows {
                scrub_payload(row, secrets);
            }
        }
        Value::Object(object) => {
            for (mut name, mut value) in std::mem::take(object) {
                scrub_text(&mut name, secrets);
                scrub_payload(&mut value, secrets);
                object.insert(name, value);
            }
        }
        _ => {}
    }
}

fn scrub_text(text: &mut String, secrets: &[String]) {
    for secret in secrets {
        if secret == text || secret.len() >= 6 && text.contains(secret) {
            *text = text.replace(secret, "[redacted]");
        }
    }
}

async fn test(State(state): State<ProviderTools>, Path(id): Path<String>) -> Response {
    match test_connection(&state, id).await {
        ConnectionTest::Complete(test) => reply(
            StatusCode::OK,
            json!({"valid":test.valid,"error":test.error,"refreshed":test.refreshed}),
        ),
        ConnectionTest::Missing => missing(),
        ConnectionTest::Failed => self::error(StatusCode::INTERNAL_SERVER_ERROR, "Test failed"),
    }
}

enum ConnectionTest {
    Complete(TestObservation),
    Missing,
    Failed,
}

struct TestObservation {
    valid: bool,
    error: Option<String>,
    latency_ms: u128,
    refreshed: bool,
}

async fn test_connection(state: &ProviderTools, id: String) -> ConnectionTest {
    let connection = match account(state, id.clone()).await {
        Ok(Some(connection)) => connection,
        Ok(None) => return ConnectionTest::Missing,
        Err(_) => return ConnectionTest::Failed,
    };
    let provider = connection["provider"].as_str().unwrap_or_default();
    let oauth = !matches!(connection["authType"].as_str(), Some("apikey" | "cookie"));
    let kiro_oauth = provider == "kiro"
        && (matches!(connection["authType"].as_str(), Some("oauth" | "api_key"))
            || (!truthy(&connection["authType"]) && !truthy(&connection["accessToken"])));
    let proxy_problem =
        match crate::proxy_pools::connection_problem(&state.storage, &connection).await {
            Ok(problem) => problem,
            Err(_) => return ConnectionTest::Failed,
        };
    let proxy_failed = proxy_problem.is_some();
    let start = std::time::Instant::now();
    let (connection, problem, refreshed, soft) = if proxy_failed {
        (connection, proxy_problem, false, false)
    } else if matches!(connection["authType"].as_str(), Some("cookie" | "apikey"))
        && crate::providers::web_login::supports(provider)
    {
        use crate::providers::web_login::identity::{self, Identity};
        let token = connection["apiKey"].as_str().unwrap_or_default();
        match identity::inspect(&state.storage, &connection, provider, token).await {
            Identity::Valid(_) => (connection, None, false, false),
            Identity::Invalid(reason) => (connection, Some(reason.to_owned()), false, false),
            Identity::Unknown => (
                connection,
                Some("Could not verify the session: provider unavailable".into()),
                false,
                true,
            ),
        }
    } else if provider == "perplexity-web" && !oauth {
        let problem =
            crate::providers::web_login::identity::test_perplexity(&state.storage, &connection)
                .await;
        (connection, problem, false, false)
    } else if provider == "xiaomi-mimo" && crate::mimo_account::has_session(&connection) {
        let (problem, transient) = crate::mimo_account::test(&state.storage, &connection).await;
        if transient {
            return ConnectionTest::Complete(TestObservation {
                valid: false,
                error: problem,
                latency_ms: start.elapsed().as_millis(),
                refreshed: false,
            });
        }
        (connection, problem, false, false)
    } else if provider == "xiaomi-mimo"
        && let Some(key) = crate::provider_imports::xiaomi_cloud_key(&connection)
    {
        let problem = validate::saved_api_key(state, &connection, provider, key).await;
        (connection, problem, false, false)
    } else if provider == "grok-cli" && oauth {
        oauth_probe::grok(state, connection).await
    } else if oauth
        && matches!(
            provider,
            "claude" | "codex" | "gemini-cli" | "cline" | "antigravity"
        )
    {
        oauth_probe::official(state, connection).await
    } else if connection["provider"] == "nous" && connection["authType"] == "oauth" {
        let observation = crate::nous_management::test_oauth(&state.storage, connection).await;
        (
            observation.connection,
            observation.error,
            observation.refreshed,
            false,
        )
    } else if connection["provider"] == "kimi" && connection["authType"] == "oauth" {
        let test = crate::provider_oauth::kimi::test_expiry(&state.storage, connection).await;
        (test.0, test.1, test.2, false)
    } else if kiro_oauth {
        let (account, error, refreshed) =
            crate::provider_oauth::test_kiro_expiry(&state.storage, connection).await;
        (account, error, refreshed, false)
    } else if provider == "kiro"
        && matches!(connection["authType"].as_str(), Some("apikey" | "cookie"))
    {
        (
            connection,
            Some("Provider test not supported".into()),
            false,
            false,
        )
    } else {
        let problem = match connection["authType"].as_str() {
            Some("apikey" | "cookie") => test_api_key(state, &connection).await,
            _ if oauth => oauth_probe::test(state, &connection).await,
            _ => None,
        };
        let problem = problem.unwrap_or_else(|| Some("Provider test not supported".into()));
        (connection, problem, false, false)
    };
    let latency_ms = if proxy_failed {
        0
    } else {
        start.elapsed().as_millis()
    };
    let valid = problem.is_none() || soft;
    let error = problem.map(|text| redact(text, &secrets(&connection)));
    let problem = error.clone();
    let written = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(current) = accounts::by_id(&tx, &id)?
                && [
                    "provider",
                    "authType",
                    "apiKey",
                    "accessToken",
                    "defaultModel",
                    "providerSpecificData",
                ]
                .iter()
                .all(|key| current[*key] == connection[*key])
                && (!matches!(
                    connection["provider"].as_str(),
                    Some(
                        "nous"
                            | "kimi"
                            | "kiro"
                            | "grok-cli"
                            | "claude"
                            | "codex"
                            | "gemini-cli"
                            | "antigravity"
                            | "cline"
                    )
                ) || ["refreshToken", "expiresAt"]
                    .iter()
                    .all(|key| current[*key] == connection[*key]))
                && (connection["provider"] != "kiro"
                    || current["tokenExpiresAt"] == connection["tokenExpiresAt"])
            {
                let now = accounts::now();
                let patch = json!({
                    "testStatus": if valid { "active" } else { "error" },
                    "lastError": problem,
                    "lastErrorAt": problem.is_some().then_some(&now),
                });
                accounts::write(
                    &tx,
                    &accounts::patch(current, accounts::object(patch), &now),
                )?;
            }
            tx.commit()
        })
        .await;
    if written.is_err() {
        return ConnectionTest::Failed;
    }
    ConnectionTest::Complete(TestObservation {
        valid,
        error,
        latency_ms,
        refreshed,
    })
}

async fn test_api_key(state: &ProviderTools, connection: &Value) -> Option<Option<String>> {
    let provider = connection["provider"].as_str()?;
    if crate::qoder_models::supports(provider) {
        let valid = if let Some(settings) = state.storage.read_settings().await {
            crate::qoder_models::test_key(&state.storage, &settings, connection).await
        } else {
            false
        };
        return Some((!valid).then(|| "Invalid Personal Access Token".into()));
    }
    if provider == "nous" {
        let key = js_field(connection, "apiKey");
        return Some(crate::nous_management::probe(&state.storage, connection, &key).await);
    }
    if provider == "fal-ai" {
        let key = js_field(connection, "apiKey");
        return Some(
            match fal_catalog_status(&state.storage, connection, &key, Duration::from_secs(15))
                .await
            {
                Ok(StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
                    Some("Invalid API key".into())
                }
                Ok(status) if status.is_redirection() => Some(fal_catalog_unverified(status)),
                Ok(_) => None,
                Err(error) => Some(error.into()),
            },
        );
    }
    if crate::bedrock_transport::is_provider(provider) {
        return Some(
            crate::bedrock_transport::credential_probe(&state.storage, connection, true).await,
        );
    }
    let kind = compatible(provider);
    let configured_base = &connection["providerSpecificData"]["baseUrl"];
    if truthy(configured_base) && !configured_base.is_string() {
        match kind {
            Some(Compatible::OpenAi | Compatible::Anthropic) => {
                return Some(Some("modelsBase.replace is not a function".into()));
            }
            _ if provider == "ollama-local" || kind.is_some() => return None,
            _ => (),
        }
    }
    let key = js_field(connection, "apiKey");
    let bad_key = "Invalid API key";
    let bad_compatible = "Invalid API key or base URL";
    let official_anthropic = provider == "anthropic";
    let dashscope = matches!(provider, "alicode" | "alicode-intl" | "alims-intl");
    let sensenova = matches!(provider, "sensenova" | "sensenova-intl");
    let (request, failure) = match kind {
        Some(Compatible::OpenAi) => {
            let Some(base) = crate::upstream::connection_base(connection, None) else {
                return Some(Some("Missing base URL".into()));
            };
            let Some(endpoint) =
                crate::upstream::node_url(base, "/models", crate::upstream::OPENAI_NODE_PATHS)
            else {
                return Some(Some(bad_compatible.into()));
            };
            (
                state.http.get(endpoint).bearer_auth(&key),
                bad_compatible.to_owned(),
            )
        }
        Some(Compatible::Anthropic) => {
            let Some(base) = crate::upstream::connection_base(connection, None) else {
                return Some(Some("Missing base URL".into()));
            };
            let Some(endpoint) =
                crate::upstream::node_url(base, "/messages", crate::upstream::ANTHROPIC_NODE_PATHS)
            else {
                return Some(Some(bad_compatible.into()));
            };
            let model = connection["defaultModel"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("claude-3-haiku-20240307");
            let request = if crate::upstream::anthropic_official(&endpoint) {
                state.http.post(endpoint)
            } else {
                state.http.post(endpoint).bearer_auth(&key)
            };
            (anthropic(request, &key, model), bad_compatible.to_owned())
        }
        _ if official_anthropic => (
            anthropic(
                state.http.post("https://api.anthropic.com/v1/messages"),
                &key,
                "claude-3-haiku-20240307",
            ),
            bad_key.to_owned(),
        ),
        _ if provider == "gemini" => (
            state
                .http
                .get("https://generativelanguage.googleapis.com/v1/models")
                .query(&[("key", &key)]),
            bad_key.to_owned(),
        ),
        _ if provider == "openrouter" => (
            state
                .http
                .get("https://openrouter.ai/api/v1/auth/key")
                .bearer_auth(&key),
            bad_key.to_owned(),
        ),
        _ if provider == "assemblyai" => (
            state
                .http
                .get("https://api.assemblyai.com/v1/account")
                .bearer_auth(&key),
            bad_key.to_owned(),
        ),
        _ if provider == "deepgram" => (
            state
                .http
                .get("https://api.deepgram.com/v1/projects")
                .header("authorization", format!("Token {key}")),
            bad_key.to_owned(),
        ),
        _ if provider == "ollama-local" => {
            let host = ollama_host(connection);
            (
                state.http.get(format!("{host}/api/tags")),
                format!("Ollama not reachable at {}", public_endpoint(host)),
            )
        }
        _ if dashscope => {
            let endpoint = crate::static_chat::primary(provider)?;
            let target = endpoint
                .fixture_url(provider, connection)
                .or_else(|| endpoint.url(connection))?;
            let catalog = crate::openai_models::data();
            let alias = catalog["transportAlias"][provider]
                .as_str()
                .unwrap_or(provider);
            let model = &catalog["models"][alias][0]["id"];
            (
                state
                    .http
                    .post(target)
                    .bearer_auth(&key)
                    .json(&json!({"model":model,
                "max_tokens":1,"messages":[{"role":"user","content":"test"}]})),
                bad_key.into(),
            )
        }
        _ if sensenova => {
            let address = if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
                format!("http://127.0.0.1:52989/provider-catalog/{provider}")
            } else if provider == "sensenova" {
                "https://token.sensenova.cn/v1/models".into()
            } else {
                "https://token.sensenova.ai/v1/models".into()
            };
            (
                state
                    .http
                    .get(address)
                    .bearer_auth(&key)
                    .header("accept", "application/json"),
                bad_key.into(),
            )
        }
        _ if matches!(
            provider,
            "openai"
                | "vercel-ai-gateway"
                | "deepseek"
                | "groq"
                | "mistral"
                | "xai"
                | "nvidia"
                | "perplexity"
                | "together"
                | "fireworks"
                | "cerebras"
                | "cohere"
                | "muse"
                | "nebius"
                | "siliconflow"
                | "hyperbolic"
                | "ollama"
                | "nanobanana"
                | "chutes"
        ) =>
        {
            (
                state.http.get(discovery_url(provider)?).bearer_auth(&key),
                bad_key.to_owned(),
            )
        }
        _ => return Some(validate::saved_api_key(state, connection, provider, &key).await),
    };
    let Some(settings) = state.storage.read_settings().await else {
        return Some(Some("fetch failed".into()));
    };
    let response = match crate::provider_http::fetch(
        &state.storage,
        &settings,
        connection,
        &state.proxy_http,
        request,
        Duration::from_secs(15),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            return Some(Some(if error.is_timeout() {
                "The operation was aborted due to timeout".into()
            } else {
                "fetch failed".into()
            }));
        }
    };
    let status = response.status();
    if status == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        return Some(Some("Proxy authentication required".into()));
    }
    let rejected = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
    if sensenova && !status.is_success() && !rejected {
        return Some(Some(format!("SenseNova API error ({})", status.as_u16())));
    }
    let valid = if kind == Some(Compatible::Anthropic) {
        !rejected && status != StatusCode::NOT_FOUND && !status.is_redirection()
    } else if official_anthropic {
        status != StatusCode::UNAUTHORIZED
    } else if dashscope {
        !rejected
    } else {
        status.is_success()
    };
    Some((!valid).then_some(failure))
}

fn public_endpoint(host: &str) -> String {
    if let Ok(mut address) = url::Url::parse(host)
        && (!address.username().is_empty() || address.password().is_some())
    {
        let _ = address.set_username("");
        let _ = address.set_password(None);
        return address.to_string();
    }
    host.to_owned()
}

async fn fal_catalog_status(
    storage: &Storage,
    account: &Value,
    key: &str,
    timeout: Duration,
) -> Result<StatusCode, &'static str> {
    use proxy_transport::Selection::{ExplicitBypass, Routed};

    const CATALOG: &str = "https://api.fal.ai/v1/models";
    const FIXTURE: &str = "http://127.0.0.1:52979/validate/api.fal.ai/v1/models";
    const FAILED: &str = "fetch failed";
    let settings = storage.read_settings().await.ok_or(FAILED)?;
    if truthy(&account["providerSpecificData"]["vercelRelayUrl"]) {
        return Err(FAILED);
    }
    let route = proxy_transport::select_with_bypass(storage, &settings, account, CATALOG)
        .await
        .map_err(|_| FAILED)?;
    let proxy = match route {
        ExplicitBypass => None,
        Routed(Route::Direct)
            if !truthy(&settings["outboundProxyEnabled"])
                && !proxy_transport::account_requires_tunnel(account) =>
        {
            None
        }
        Routed(Route::Proxy { url, .. }) => {
            Some(proxy_transport::tunnel_proxy_url(&url).ok_or(FAILED)?)
        }
        _ => return Err(FAILED),
    };
    let builder = reqwest::Client::builder()
        .no_proxy()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never());
    let (client, endpoint) = if let Some(proxy) = proxy {
        (
            proxy_transport::vendor_metadata_client(proxy, builder),
            CATALOG,
        )
    } else {
        let fixture = std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1");
        (
            builder.build().ok(),
            if fixture { FIXTURE } else { CATALOG },
        )
    };
    let mut authorization =
        header::HeaderValue::from_str(&format!("Key {key}")).map_err(|_| FAILED)?;
    authorization.set_sensitive(true);
    client
        .ok_or(FAILED)?
        .get(endpoint)
        .query(&[("limit", "1")])
        .header(header::AUTHORIZATION, authorization)
        .send()
        .await
        .map(|response| response.status())
        .map_err(|error| {
            if error.is_timeout() {
                "The operation was aborted due to timeout"
            } else {
                FAILED
            }
        })
}

fn fal_catalog_unverified(status: StatusCode) -> String {
    format!(
        "Unable to verify API key (Fal returned {})",
        status.as_u16()
    )
}

fn anthropic(request: reqwest::RequestBuilder, key: &str, model: &str) -> reqwest::RequestBuilder {
    request
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model":model,"max_tokens":1,"messages":[{"role":"user","content":"test"}],
        }))
}
