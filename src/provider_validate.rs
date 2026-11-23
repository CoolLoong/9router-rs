use std::sync::LazyLock;
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use reqwest::RequestBuilder;
use serde_json::{Value, json};
use url::Url;

use super::{
    ProviderTools, anthropic, discovery_url, error, fal_catalog_status, fal_catalog_unverified,
    reply, test_api_key,
};
use crate::chat_format::ChatFormat;
use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_http::{self, FetchError};
use crate::provider_index::{self, Compatible, compatible, js_trim};

const BODY_LIMIT: usize = 1024 * 1024;
const INVALID_KEY: &str = "Invalid API key";
const PATH_COMPONENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'?')
    .add(b'\\')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'{')
    .add(b'}');
static VALIDATION: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("provider-validation.json"))
        .expect("embedded provider probe metadata")
});

pub(super) async fn run(State(state): State<ProviderTools>, request: Request) -> Response {
    let (_, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, BODY_LIMIT).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Validation failed");
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Validation failed");
    };
    if body.is_null() {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Validation failed");
    }
    let provider_value = &body["provider"];
    let provider = provider_value.as_str().map(provider_index::normalize);
    let metadata = provider
        .as_deref()
        .and_then(provider_index::find)
        .unwrap_or(&Value::Null);
    let key = &body["apiKey"];
    let details = &body["providerSpecificData"];
    let substitute = metadata["apiKeyOptionalWith"]
        .as_str()
        .is_some_and(|name| truthy(&details[name]));
    if (provider.as_ref().is_some_and(String::is_empty) || !truthy(provider_value))
        || (!truthy(key)
            && provider.as_deref() != Some("ollama-local")
            && metadata["noAuth"] != true
            && !substitute)
    {
        return error(StatusCode::BAD_REQUEST, "Provider and API key required");
    }
    let Some(provider) = provider else {
        return error(StatusCode::BAD_REQUEST, "Provider validation not supported");
    };
    if provider == "fal-ai" {
        let answer = fal_catalog_status(
            &state.storage,
            &Value::Null,
            &js_text(key),
            Duration::from_secs(8),
        )
        .await;
        return match answer {
            Ok(status) if status.is_success() => result(true, INVALID_KEY),
            Ok(status) if auth_failure(status) => result(false, INVALID_KEY),
            Ok(status) => result(false, &fal_catalog_unverified(status)),
            Err(error) => result(false, error),
        };
    }
    if provider == "nous" {
        let problem =
            crate::nous_management::probe(&state.storage, &Value::Null, &js_text(key)).await;
        return result(problem.is_none(), problem.as_deref().unwrap_or_default());
    }
    if crate::bedrock_transport::is_provider(&provider) {
        let problem =
            crate::bedrock_transport::credential_probe(&state.storage, &body, false).await;
        return result(problem.is_none(), problem.as_deref().unwrap_or_default());
    }
    if let Some(response) = local_result(&provider, metadata, key, details) {
        return response;
    }
    let Some(settings) = state.storage.read_settings().await else {
        return result(false, "fetch failed");
    };
    let validation = Validation {
        state: &state,
        settings,
        account: &Value::Null,
    };
    if provider == "grok-web" {
        use crate::providers::web_login::identity::{Identity, inspect_grok};
        return match inspect_grok(
            &state.storage,
            &validation.settings,
            validation.account,
            key.as_str().unwrap_or_default(),
        )
        .await
        {
            Identity::Valid(_) => result(true, INVALID_KEY),
            Identity::Invalid(reason) => result(false, reason),
            Identity::Unknown => result(false, "Could not verify the Grok session"),
        };
    }
    if provider == "perplexity-web" {
        return match crate::perplexity_web_chat::validate_cookie(
            &state.storage,
            &validation.settings,
            key,
        )
        .await
        {
            Ok(()) => result(true, INVALID_KEY),
            Err(error) => result(false, error),
        };
    }
    if crate::qoder_models::supports(&provider) {
        let account = json!({
            "provider":provider,"apiKey":key,"providerSpecificData":details,
        });
        let detected =
            crate::qoder_models::models(&state.storage, &validation.settings, &account, true)
                .await
                .is_some_and(|models| !models.is_empty());
        return result(detected, INVALID_KEY);
    }
    if let Some(kind) = compatible(&provider) {
        return compatible_probe(&validation, &provider, kind, &js_text(key)).await;
    }
    if let Some(response) = probe(&validation, &provider, key, details).await {
        return response;
    }
    error(StatusCode::BAD_REQUEST, "Provider validation not supported")
}

struct Validation<'a> {
    state: &'a ProviderTools,
    settings: Value,
    account: &'a Value,
}

impl Validation<'_> {
    async fn send(
        &self,
        request: RequestBuilder,
        seconds: u64,
    ) -> Result<reqwest::Response, FetchError> {
        provider_http::fetch(
            &self.state.storage,
            &self.settings,
            self.account,
            &self.state.proxy_http,
            request,
            Duration::from_secs(seconds),
        )
        .await
    }

    async fn check(
        &self,
        request: RequestBuilder,
        seconds: u64,
        valid: impl FnOnce(StatusCode) -> bool,
    ) -> Probe {
        match probe_status(self.send(request, seconds).await) {
            Ok(status) => Probe::Verdict(valid(status)),
            Err(failure) => failure,
        }
    }
}

pub(super) async fn saved_api_key(
    state: &ProviderTools,
    account: &Value,
    provider: &str,
    key: &str,
) -> Option<String> {
    let Some(settings) = state.storage.read_settings().await else {
        return Some("fetch failed".into());
    };
    let validation = Validation {
        state,
        settings,
        account,
    };
    let details = &account["providerSpecificData"];
    if provider == "azure" {
        return azure(&validation, key, details)
            .await
            .problem("Invalid API key or Azure configuration")
            .map(str::to_owned);
    }
    if provider == "cloudflare-ai" {
        if !truthy(&details["accountId"]) {
            return Some("Missing Account ID".into());
        }
        return cloudflare(&validation, key, details)
            .await
            .problem("Invalid API token or Account ID")
            .map(str::to_owned);
    }

    let client = &state.http;
    let get = |url: &str| registry_endpoint(url).map(|url| client.get(url).bearer_auth(key));
    let chat = |url: &str, model: &str, anthropic_wire: bool| {
        registry_endpoint(url).map(|url| {
            if anthropic_wire {
                anthropic(client.post(url), key, model)
            } else {
                client.post(url).bearer_auth(key).json(&json!({
                    "model":model,"max_tokens":1,"messages":[{"role":"user","content":"test"}],
                }))
            }
        })
    };
    let request = match provider {
        "commandcode" => get("https://api.commandcode.ai/alpha/whoami?limits=1")
            .map(|request| request.header("accept", "application/json")),
        "blackbox" => get("https://api.blackbox.ai/v1/models"),
        "tokenharbor" => get("https://tokenharbor.ai/v1/models"),
        "dahl" => get("https://inference.dahl.global/v1/models"),
        "atria" => get("https://api.atria-asi.ai/v1/models"),
        "agnes" => get("https://apihub.agnes-ai.com/v1/models"),
        "bai" => get("https://api.b.ai/v1/models"),
        "runcrate" | "synthetic" | "xiaomi-mimo" => {
            VALIDATION[provider]["validateUrl"].as_str().and_then(get)
        }
        "xiaomi-tokenplan" => get("https://token-plan-sgp.xiaomimimo.com/v1/models"),
        "kimchi" => get("https://api.cast.ai/v1/llm/openai/supported-providers").map(|request| {
            request
                .header("accept", "application/json")
                .header("user-agent", "kimchi/0.1.40")
        }),
        "opencode" => registry_endpoint("https://opencode.ai/zen/v1/models").map(|url| {
            client
                .get(url)
                .bearer_auth("public")
                .header("user-agent", "opencode/1.18.31")
        }),
        "opencode-go" => opencode_go_request(&validation, key),
        "llm7" if truthy(&details["baseUrl"]) => {
            let Some(configured) = details["baseUrl"].as_str() else {
                return Some("baseUrl.replace is not a function".into());
            };
            let base = configured.strip_suffix('/').unwrap_or(configured);
            safe_endpoint(&format!("{base}/models")).map(|url| client.get(url).bearer_auth(key))
        }
        "llm7" => get("https://api.llm7.io/v1/models"),
        "glm" => chat(
            "https://api.z.ai/api/anthropic/v1/messages",
            "glm-4.7",
            true,
        ),
        "glm-cn" => chat(
            "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
            "glm-4.7",
            false,
        ),
        "minimax" => chat(
            "https://api.minimax.io/anthropic/v1/messages",
            "minimax-m2",
            true,
        ),
        "minimax-cn" => chat(
            "https://api.minimaxi.com/anthropic/v1/messages",
            "minimax-m2",
            true,
        ),
        "kimi" => chat(
            "https://api.kimi.com/coding/v1/messages",
            "kimi-latest",
            true,
        ),
        "volcengine-ark" | "byteplus" => {
            let catalog = crate::openai_models::data();
            let alias = catalog["transportAlias"][provider]
                .as_str()
                .unwrap_or(provider);
            catalog["models"][alias][0]["id"]
                .as_str()
                .and_then(|model| {
                    let endpoint =
                        crate::static_chat::default_endpoint(provider)?.url(&Value::Null)?;
                    chat(&endpoint, model, false)
                })
        }
        _ => return Some("Provider test not supported".into()),
    };
    let Some(request) = request else {
        return Some("fetch failed".into());
    };
    let status = match probe_status(validation.send(request, 15).await) {
        Ok(status) => status,
        Err(failure) => return failure.problem(INVALID_KEY).map(str::to_owned),
    };
    let accepts = matches!(
        provider,
        "glm"
            | "glm-cn"
            | "minimax"
            | "minimax-cn"
            | "kimi"
            | "volcengine-ark"
            | "byteplus"
            | "opencode-go"
    );
    if status.is_success() || accepts && !auth_failure(status) {
        return None;
    }
    Some(match provider {
        "commandcode" if !auth_failure(status) => {
            format!("Command Code API error ({})", status.as_u16())
        }
        "llm7" => "Invalid API key or base URL".into(),
        "opencode" => "OpenCode free tier unavailable".into(),
        _ => INVALID_KEY.into(),
    })
}

fn local_result(
    provider: &str,
    metadata: &Value,
    key: &Value,
    details: &Value,
) -> Option<Response> {
    if provider == "cloudflare-ai" && !truthy(&details["accountId"]) {
        return Some(result(false, "Missing Account ID"));
    }
    if matches!(provider, "vertex" | "vertex-partner")
        && let Some(account) = key
            .as_str()
            .and_then(|key| serde_json::from_str::<Value>(key).ok())
        && account["type"] == "service_account"
    {
        let valid = ["client_email", "private_key", "project_id"]
            .iter()
            .all(|field| truthy(&account[*field]));
        return Some(result(valid, INVALID_KEY));
    }
    if VALIDATION[provider]["metadataProbe"]["accept"] == true
        || VALIDATION[provider]["defaultProbe"]["noAuth"] == true
    {
        return Some(result(true, INVALID_KEY));
    }
    if metadata.is_null() && compatible(provider).is_none() && !matches!(provider, "vertex-partner")
    {
        return Some(error(
            StatusCode::BAD_REQUEST,
            "Provider validation not supported",
        ));
    }
    None
}

async fn probe(
    validation: &Validation<'_>,
    provider: &str,
    api_key: &Value,
    details: &Value,
) -> Option<Response> {
    let key = js_text(api_key);
    if let Some(metadata) = VALIDATION[provider].get("metadataProbe") {
        return Some(
            metadata_probe(validation, metadata, &key)
                .await?
                .response(INVALID_KEY),
        );
    }
    if let Some(config) = VALIDATION[provider].get("defaultProbe") {
        return Some(
            default_probe(validation, config, &key)
                .await?
                .response(INVALID_KEY),
        );
    }
    let client = &validation.state.http;
    let outcome = match provider {
        "openai" | "vercel-ai-gateway" | "anthropic" | "gemini" | "deepgram" |
        "alicode" | "alicode-intl" | "alims-intl" => {
            let unsaved = json!({"provider":provider,"apiKey":key});
            let issue = test_api_key(validation.state, &unsaved).await?;
            return Some(match issue {
                None => result(true, INVALID_KEY),
                Some(issue) => result(false, &issue),
            });
        }
        "openrouter" => validation.check(client.get(discovery_url(provider)?).bearer_auth(&key), 15, |status| status.is_success()).await,
        "glm" | "glm-cn" | "kimi" | "minimax" | "minimax-cn" | "volcengine-ark" | "byteplus" => {
            chat_probe(validation, provider, &key).await?
        }
        "azure" => azure(validation, &key, details).await,
        "cloudflare-ai" => cloudflare(validation, &key, details).await,
        "vertex" | "vertex-partner" => validation.check(client.post(
            "https://aiplatform.googleapis.com/v1/publishers/google/models/__probe__:generateContent")
            .query(&[("key", &key)]).json(&json!({})), 15, |status| !auth_failure(status)).await,
        "ollama-local" => {
            let shape = json!({"providerSpecificData":details});
            let Some(host) = crate::ollama_local::host(&shape) else {
                return Some(result(false, "fetch failed"));
            };
            let Some(target) = safe_endpoint(&format!("{host}/api/tags")) else {
                return Some(result(false, "fetch failed"));
            };
            let mut request = client.get(target);
            if truthy(api_key) {
                request = request.bearer_auth(&key);
            }
            validation.check(request, 8, |status| status.is_success()).await
        }
        "opencode-go" => validation.check(opencode_go_request(validation, &key)?, 15, |status| !auth_failure(status)).await,
        "commandcode" => commandcode_probe(validation, &key).await?,
        "blackbox" => validation.check(
            client.post(registry_endpoint("https://api.blackbox.ai/chat/completions")?)
                .bearer_auth(&key)
                .json(&json!({"model":"gpt-4o","messages":[{"role":"user","content":"test"}],"max_tokens":10})),
            15,
            |status| matches!(status, StatusCode::OK | StatusCode::BAD_REQUEST),
        ).await,
        other => {
            let config = &VALIDATION[other];
            let dest = if other == "xiaomi-tokenplan" {
                let regions = &config["regions"];
                let region = details["region"].as_str().unwrap_or_default();
                let base = regions[region].as_str().or_else(|| regions[config["defaultRegion"].as_str()?].as_str())?;
                format!("{base}/models")
            } else {
                config["validateUrl"].as_str()?.to_owned()
            };
            let dest = registry_endpoint(&dest)?;
            validation.check(client.get(dest).bearer_auth(&key), 8, |status| match other {
                "xai" => matches!(status, StatusCode::OK | StatusCode::FORBIDDEN),
                "xiaomi-tokenplan" => status != StatusCode::UNAUTHORIZED,
                _ => status.is_success(),
            }).await
        }
    };
    let failure = match provider {
        "azure" => "Invalid API key or Azure configuration",
        "cloudflare-ai" => "Invalid API token or Account ID",
        _ => INVALID_KEY,
    };
    Some(outcome.response(failure))
}

fn opencode_go_request(validation: &Validation<'_>, key: &str) -> Option<RequestBuilder> {
    let url = registry_endpoint("https://opencode.ai/inference/go/openai/v1/chat/completions")?;
    Some(validation.state.http.post(url).bearer_auth(key).json(&json!({
        "model":VALIDATION["opencode-go"]["defaultModel"],"messages":[{"role":"user","content":"ping"}],
        "max_tokens":1,"stream":false,
    })))
}

async fn commandcode_probe(validation: &Validation<'_>, key: &str) -> Option<Probe> {
    let model = crate::openai_models::data()["models"]["commandcode"][0]["id"].as_str()?;
    let body = json!({"messages":[{"role":"user","content":"ping"}],"max_tokens":1});
    let mut envelope = crate::commandcode_transport::prepare(
        &body,
        &body,
        &axum::http::HeaderMap::new(),
        model,
        &Value::Null,
    )
    .ok()?;
    envelope["params"]["stream"] = Value::Bool(false);
    envelope.as_object_mut()?.shift_remove("model");
    let url = registry_endpoint(crate::commandcode_transport::url())?;
    let request = crate::commandcode_identity::session_headers(
        validation.state.http.post(url),
        envelope["threadId"].as_str()?,
    )
    .bearer_auth(key)
    .json(&envelope);
    Some(
        validation
            .check(request, 15, |status| !auth_failure(status))
            .await,
    )
}

async fn chat_probe(validation: &Validation<'_>, provider: &str, key: &str) -> Option<Probe> {
    let endpoint = crate::static_chat::default_endpoint(provider)?;
    let mut url = safe_endpoint(&endpoint.url(&Value::Null)?)?;
    if endpoint.wire == ChatFormat::Messages && url.query() == Some("beta=true") {
        url.set_query(None);
    }
    let target = registry_endpoint(url.as_str())?;
    let data = crate::openai_models::data();
    let alias = data["transportAlias"][provider]
        .as_str()
        .unwrap_or(provider);
    let model = data["models"][alias][0]["id"].as_str()?;
    let request = endpoint
        .auth(validation.state.http.post(target), key, &Value::Null)
        .json(&json!({"model":model,"max_tokens":1,"messages":[{"role":"user","content":"test"}]}));
    Some(
        validation
            .check(request, 15, |status| !auth_failure(status))
            .await,
    )
}

async fn metadata_probe(validation: &Validation<'_>, config: &Value, key: &str) -> Option<Probe> {
    let endpoint = registry_endpoint(config["url"].as_str()?)?;
    let method = config["method"].as_str()?.parse::<reqwest::Method>().ok()?;
    let web = config["kind"] == "web";
    let mut request = validation
        .state
        .http
        .request(method.clone(), endpoint)
        .header("Content-Type", "application/json");
    for (name, value) in config["headers"].as_object().into_iter().flatten() {
        request = request.header(name, js_text(value));
    }
    request = match config["auth"].as_str() {
        Some("bearer") => request.bearer_auth(key),
        Some("key") if web => request.query(&[("key", key), ("q", "ping"), ("cx", "test")]),
        Some("api_key") if web => {
            request.query(&[("api_key", key), ("q", "ping"), ("engine", "google")])
        }
        Some("key") => request.header("Authorization", format!("Key {key}")),
        Some("token") => request.header("Authorization", format!("Token {key}")),
        Some("basic") => request.header("Authorization", format!("Basic {key}")),
        Some(name @ ("x-api-key" | "x-key" | "xi-api-key" | "x-subscription-token")) => {
            request.header(name, key)
        }
        _ if web => request,
        _ => return None,
    };
    if web && method == reqwest::Method::POST {
        request = request.json(&json!({"query":"ping","q":"ping","url":"https://example.com"}));
    } else if !web && method != reqwest::Method::GET {
        request = request.json(&json!({
            "input":"ping","text":"ping","prompt":"ping","model":config["model"],
        }));
    }
    Some(
        validation
            .check(request, 8, |status| !auth_failure(status))
            .await,
    )
}

async fn default_probe(validation: &Validation<'_>, config: &Value, key: &str) -> Option<Probe> {
    let raw = config["url"].as_str()?;
    let chat = registry_endpoint(raw)?;
    let models = ["/chat/completions", "/chatbot"]
        .iter()
        .find_map(|suffix| raw.strip_suffix(suffix))
        .map_or_else(|| raw.to_owned(), |base| format!("{base}/models"));
    let models = registry_endpoint(&models)?;
    let headers = |request: RequestBuilder| {
        let mut request = request.header("Content-Type", "application/json");
        for (name, value) in config["headers"].as_object().into_iter().flatten() {
            request = request.header(name, js_text(value));
        }
        if config["auth"] == "x-api-key" {
            request.header("X-API-Key", key)
        } else {
            request.bearer_auth(key)
        }
    };
    let client = &validation.state.http;
    let first = validation.send(headers(client.get(models)), 8).await;
    if !matches!(&first, Err(FetchError::Direct(_))) {
        match probe_status(first) {
            Ok(status) if auth_failure(status) || status.is_success() => {
                return Some(Probe::Verdict(status.is_success()));
            }
            Ok(_) => (),
            Err(failure) => return Some(failure),
        }
    }
    let request = headers(client.post(chat)).json(&json!({
        "model":config["model"],"messages":[{"role":"user","content":"ping"}],"max_tokens":1,
    }));
    Some(
        validation
            .check(request, 10, |status| !auth_failure(status))
            .await,
    )
}

async fn compatible_probe(
    validation: &Validation<'_>,
    provider: &str,
    kind: Compatible,
    key: &str,
) -> Response {
    let state = validation.state;
    let missing = match kind {
        Compatible::OpenAi => "OpenAI Compatible node not found",
        Compatible::Anthropic => "Anthropic Compatible node not found",
        Compatible::Embedding => "Custom Embedding node not found",
    };
    let provider = provider.to_owned();
    let node = state
        .storage
        .run(false, move |db| crate::provider_nodes::by_id(db, &provider))
        .await;
    let node = match node {
        Ok(Some(node)) => Value::Object(node),
        Ok(None) => return error(StatusCode::NOT_FOUND, missing),
        Err(_) => return result(false, "Could not load provider node"),
    };
    let Some(base) = node["baseUrl"]
        .as_str()
        .map(js_trim)
        .filter(|base| !base.is_empty())
    else {
        return result(false, "fetch failed");
    };
    let paths = crate::upstream::compatible_paths(kind);
    if kind == Compatible::Anthropic {
        let Some(target) = crate::upstream::node_url(base, "/messages", paths) else {
            return result(false, "fetch failed");
        };
        let model = node["defaultModel"]
            .as_str()
            .filter(|model| !model.is_empty())
            .unwrap_or("claude-3-haiku-20240307");
        let request = if crate::upstream::anthropic_official(&target) {
            state.http.post(target)
        } else {
            state.http.post(target).bearer_auth(key)
        };
        return validation
            .check(anthropic(request, key, model), 15, |status| {
                !auth_failure(status) && status != StatusCode::NOT_FOUND && !status.is_redirection()
            })
            .await
            .response(INVALID_KEY);
    }
    let Some(target) = crate::upstream::node_url(base, "/models", paths) else {
        return result(false, "fetch failed");
    };
    let status = match probe_status(
        validation
            .send(state.http.get(target).bearer_auth(key), 15)
            .await,
    ) {
        Ok(status) => status,
        Err(failure) => return failure.response(INVALID_KEY),
    };
    if kind == Compatible::OpenAi || auth_failure(status) {
        return result(status.is_success(), INVALID_KEY);
    }
    if status.is_success() {
        return reply(StatusCode::OK, json!({"valid":true}));
    }
    let Some(target) = crate::upstream::node_url(base, "/embeddings", paths) else {
        return result(false, "fetch failed");
    };
    validation
        .check(
            state
                .http
                .post(target)
                .bearer_auth(key)
                .json(&json!({"model":"test","input":"ping"})),
            15,
            |status| !auth_failure(status),
        )
        .await
        .response(INVALID_KEY)
}

async fn azure(validation: &Validation<'_>, key: &str, details: &Value) -> Probe {
    let value = |name| details[name].as_str().filter(|value| !value.is_empty());
    let Some(endpoint) = value("azureEndpoint").and_then(safe_endpoint) else {
        return Probe::Failed("fetch failed");
    };
    let mut url = endpoint.as_str();
    url = url.strip_suffix('/').unwrap_or(url);
    let deployment = value("deployment").unwrap_or("gpt-4");
    if matches!(deployment, "." | "..") || endpoint.query().is_some() {
        return Probe::Failed("fetch failed");
    }
    let encoded = utf8_percent_encode(deployment, PATH_COMPONENT);
    let version = value("apiVersion").unwrap_or("2024-10-01-preview");
    let path = format!("{url}/openai/deployments/{encoded}/chat/completions");
    let max_tokens = if validation.account.is_null() {
        "max_tokens"
    } else {
        "max_completion_tokens"
    };
    let mut request = validation
        .state
        .http
        .post(path)
        .query(&[("api-version", version)])
        .header("api-key", key)
        .json(&json!({"messages":[{"role":"user","content":"test"}],(max_tokens):1}));
    if let Some(org) = value("organization") {
        request = request.header("OpenAI-Organization", org);
    }
    validation
        .check(request, 15, |status| !auth_failure(status))
        .await
}

async fn cloudflare(validation: &Validation<'_>, key: &str, details: &Value) -> Probe {
    let Some(account) = details["accountId"].as_str().filter(|account| {
        account
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }) else {
        return Probe::Verdict(false);
    };
    let target =
        format!("https://api.cloudflare.com/client/v4/accounts/{account}/ai/v1/chat/completions");
    let Some(target) = registry_endpoint(&target) else {
        return Probe::Failed("fetch failed");
    };
    validation
        .check(
            validation
                .state
                .http
                .post(target)
                .bearer_auth(key)
                .json(&json!({
                    "model":VALIDATION["cloudflare-ai"]["defaultModel"],
                    "messages":[{"role":"user","content":"test"}],"max_tokens":1,
                })),
            15,
            |status| !auth_failure(status) && status != StatusCode::NOT_FOUND,
        )
        .await
}

fn safe_endpoint(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

fn registry_endpoint(raw: &str) -> Option<Url> {
    let target = safe_endpoint(raw)?;
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
        let host = target.host_str()?;
        let mut local = Url::parse(&format!(
            "http://127.0.0.1:52979/validate/{host}{}",
            target.path(),
        ))
        .ok()?;
        local.set_query(target.query());
        return Some(local);
    }
    Some(target)
}

fn auth_failure(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

fn probe_status(response: Result<reqwest::Response, FetchError>) -> Result<StatusCode, Probe> {
    match response {
        Ok(response) if response.status() == StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
            Err(Probe::Failed("Proxy authentication required"))
        }
        Ok(response) => Ok(response.status()),
        Err(error) => Err(Probe::transport(error.is_timeout())),
    }
}

enum Probe {
    Verdict(bool),
    Failed(&'static str),
}

impl Probe {
    fn transport(timeout: bool) -> Self {
        Self::Failed(if timeout {
            "The operation was aborted due to timeout"
        } else {
            "fetch failed"
        })
    }

    fn response(self, invalid: &str) -> Response {
        let failure = self.problem(invalid);
        result(failure.is_none(), failure.unwrap_or_default())
    }

    fn problem(self, invalid: &str) -> Option<&str> {
        match self {
            Self::Verdict(valid) => (!valid).then_some(invalid),
            Self::Failed(error) => Some(error),
        }
    }
}

fn result(valid: bool, message: &str) -> Response {
    reply(
        StatusCode::OK,
        json!({"valid":valid,"error":(!valid).then_some(message)}),
    )
}
