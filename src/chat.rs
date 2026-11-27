use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use serde_json::{Value, json};

use crate::anthropic_request;
use crate::chat_accounts::{self, Selection};
use crate::chat_client::Client;
use crate::chat_format::{ChatFormat, is_compact_path};
use crate::chat_output::{self, ResponseWire};
use crate::chat_prefetch;
use crate::chat_usage;
use crate::combo_chat;
use crate::commandcode_transport;
use crate::gemini_cli_transport;
use crate::gemini_public;
use crate::gemini_request;
use crate::gemini_signatures::{GeminiSignatures, SignatureContext};
use crate::github_claude;
use crate::github_executor;
use crate::github_responses;
use crate::google_code_assist;
use crate::headroom;
use crate::key_policy;
use crate::kiro_chat;
use crate::observability;
use crate::observed_response::into_http;
use crate::ollama_public;
use crate::opencode_chat;
use crate::provider_connections::truthy;
use crate::proxy_transport;
use crate::pxpipe_runtime::{Mode as PxpipeMode, Runtime as Pxpipe};
use crate::static_chat;
use crate::storage::Storage;
use crate::system_prompts;
use crate::{chat_input, chat_protocol_input};
use crate::{claude_passthrough, codex_executor};
use crate::{responses_request, responses_upstream};

#[path = "chat_vision.rs"]
mod chat_vision;

const BODY_LIMIT: usize = 128 * 1024 * 1024;
const DEFAULT_UPSTREAM: &str = "https://api.openai.com/v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const TOKEN_SAVER: &str = "x-9router-token-saver";
pub(crate) const MIMO_FREE_RETIRED: &str = "MiMo free API service has ended";

#[derive(Clone)]
struct ChatState {
    storage: Storage,
    client: reqwest::Client,
    reconnect: reqwest::Client,
    gemini_signatures: GeminiSignatures,
    proxy_clients: proxy_transport::Clients,
    recorder: observability::Recorder,
    codex_sessions: codex_executor::Sessions,
    antigravity_sessions: codex_executor::Sessions,
    github_routes: github_responses::Routes,
    pxpipe: Pxpipe,
}

pub fn router(storage: Storage, recorder: observability::Recorder, pxpipe: Pxpipe) -> Router {
    let build = || {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(4))
            .redirect(crate::upstream::redirects())
            .no_proxy()
    };
    let state = ChatState {
        storage,
        client: build().build().expect("chat HTTP client"),
        reconnect: build()
            .pool_max_idle_per_host(0)
            .build()
            .expect("chat reconnect client"),
        gemini_signatures: GeminiSignatures::default(),
        proxy_clients: proxy_transport::Clients::default(),
        recorder,
        codex_sessions: codex_executor::Sessions::default(),
        antigravity_sessions: codex_executor::Sessions::default(),
        github_routes: github_responses::Routes::default(),
        pxpipe,
    };
    let mut router = Router::new();
    for base in ["/v1", "/api/v1", "/v1/v1"] {
        for suffix in [
            "/chat/completions",
            "/messages",
            "/responses",
            "/responses/compact",
        ] {
            router = router.route(&format!("{base}{suffix}"), post(chat).options(options));
        }
    }
    for path in ["/responses", "/codex", "/codex/{*path}"] {
        router = router.route(path, post(chat).options(options));
    }
    for path in ollama_public::PATHS {
        router = router.route(path, post(chat).options(ollama_public::options));
    }
    for prefix in gemini_public::PATHS {
        router = router.route(&format!("{prefix}{{*path}}"), post(chat).options(options));
    }
    router.with_state(state)
}

async fn options() -> Response {
    into_http(Ok(json!({
        "status": 200,
        "headers": {
            "access-control-allow-origin": "*",
            "access-control-allow-methods": "GET, POST, OPTIONS",
            "access-control-allow-headers": "*",
        },
        "body": "",
    })))
}

enum Plan {
    Denied,
    Bypass,
    Native(combo_chat::Route),
    Invalid,
}

async fn chat(State(state): State<ChatState>, request: Request) -> Response {
    let policy_key = crate::catalog_guard::api_key(&request);
    let (parts, incoming) = request.into_parts();
    let gemini_path = gemini_public::PATHS
        .iter()
        .any(|prefix| parts.uri.path().starts_with(prefix));
    let ollama = ollama_public::PATHS.contains(&parts.uri.path());
    let Some(bytes) = to_bytes(incoming, BODY_LIMIT).await.ok() else {
        return invalid_chat_json(ollama);
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return invalid_chat_json(ollama);
    };
    let model = ollama.then(|| ollama_public::requested_model(&body));
    let response = chat_body(state, parts, policy_key, body, gemini_path).await;
    if let Some(model) = model {
        return ollama_public::transform(response, model);
    }
    response
}

fn invalid_chat_json(ollama: bool) -> Response {
    let response = error(
        StatusCode::BAD_REQUEST,
        "Invalid JSON body",
        HeaderMap::new(),
    );
    if ollama {
        ollama_public::transform(response, ollama_public::requested_model(&Value::Null))
    } else {
        response
    }
}

async fn chat_body(
    state: ChatState,
    parts: axum::http::request::Parts,
    policy_key: Option<String>,
    mut body: Value,
    gemini_path: bool,
) -> Response {
    let inference_key = inference_key(&parts.headers);
    if gemini_path && let Some(native) = crate::gemini_tts::Native::parse(parts.uri.path(), &body) {
        return match native
            .forward(
                &state.storage,
                &parts.uri,
                &parts.headers,
                policy_key.as_deref(),
                &body,
            )
            .await
        {
            Some(response) => response,
            None => error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to prepare Gemini audio request",
                HeaderMap::new(),
            ),
        };
    }
    let gemini_client = if gemini_path {
        let Some(client) = gemini_public::Client::parse(parts.uri.path(), &body) else {
            return error(
                StatusCode::BAD_REQUEST,
                "Invalid Gemini request",
                HeaderMap::new(),
            );
        };
        let Some(converted) = client.convert_request(&body) else {
            return error(
                StatusCode::BAD_REQUEST,
                "Invalid Gemini contents",
                HeaderMap::new(),
            );
        };
        body = converted;
        Some(client)
    } else {
        None
    };
    if body.is_null() {
        return invalid_chat_json(false);
    }
    let Some(settings) = state.storage.read_settings().await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read settings",
            HeaderMap::new(),
        );
    };
    if settings.get("requireApiKey").is_none_or(truthy) {
        let Some(key) = inference_key.as_ref().filter(|s| !s.is_empty()) else {
            return error(
                StatusCode::UNAUTHORIZED,
                "Missing API key",
                HeaderMap::new(),
            );
        };
        if !state.storage.api_key_is_active(key.clone()).await {
            return error(
                StatusCode::UNAUTHORIZED,
                "Invalid API key",
                HeaderMap::new(),
            );
        }
    }

    let model = body["model"].clone();
    if !truthy(&model) {
        return error(StatusCode::BAD_REQUEST, "Missing model", HeaderMap::new());
    }
    let parsed = model_markers_rs::port::strip_model_context_marker(model).ok();
    let Some((parsed, model)) = parsed
        .as_ref()
        .and_then(|parsed| parsed["model"].as_str().map(|model| (parsed, model)))
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "Invalid model format",
            HeaderMap::new(),
        );
    };
    if model.is_empty() {
        return error(StatusCode::BAD_REQUEST, "Missing model", HeaderMap::new());
    }
    let model = crate::model_resolution::strip_gateway_prefix(model).to_owned();
    let marked_model = parsed["contextMarker"].as_str().map(|marker| {
        let id = model.split_once('/').map_or(model.as_str(), |(_, id)| id);
        format!("{id}[{marker}]")
    });
    body["model"] = json!(model);
    let direct_loopback =
        crate::catalog_guard::is_direct_loopback_parts(&parts.headers, &parts.extensions);
    let devin_key = policy_key.clone();
    if let Some(devin_model) = crate::devin_acp::requested_model(&model) {
        return crate::devin_acp::respond(
            &state.storage,
            &settings,
            crate::devin_acp::Caller {
                direct_loopback,
                key: devin_key.as_deref(),
                format: ChatFormat::from_path(parts.uri.path()),
                accept: header_text(&parts.headers, "accept"),
            },
            body,
            devin_model,
        )
        .await;
    }

    let requested = model.clone();
    let response = gemini_client
        .is_none()
        .then(|| {
            crate::chat_bypass::respond(
                &body,
                &model,
                header_text(&parts.headers, "user-agent"),
                truthy(&settings["ccFilterNaming"]),
            )
        })
        .flatten();
    let bypass = response.is_some();
    let resolution_settings = settings.clone();
    let capabilities = combo_chat::capabilities(&body);
    let routing_capabilities = capabilities.clone();
    let catalog = state.storage.model_catalog();
    let routing_catalog = catalog.clone();
    let plan = state
        .storage
        .run(false, move |db| {
            if let Some(key) = policy_key.as_ref().filter(|s| !s.is_empty())
                && !key_policy::permits_chat(db, key, &requested)?
            {
                return Ok(Plan::Denied);
            }
            if bypass {
                return Ok(Plan::Bypass);
            }
            let adapters = crate::capacity_adapter::Routing {
                required: &routing_capabilities,
                key: policy_key.as_deref().filter(|key| !key.is_empty()),
                catalog: routing_catalog.as_deref(),
            };
            if let Some(route) =
                combo_chat::resolve(db, &requested, &resolution_settings, &adapters)?
            {
                return Ok(Plan::Native(route));
            }
            Ok(Plan::Invalid)
        })
        .await;
    let route = match plan {
        Ok(Plan::Bypass) => return response.expect("prepared local CLI response"),
        Ok(Plan::Native(route)) => route,
        Ok(Plan::Denied) => {
            return error(
                StatusCode::FORBIDDEN,
                &format!("This API key is not allowed to use model \"{model}\""),
                HeaderMap::new(),
            );
        }
        Ok(Plan::Invalid) => {
            return error(
                StatusCode::BAD_REQUEST,
                "Invalid model or combo configuration",
                HeaderMap::new(),
            );
        }
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to resolve model",
                HeaderMap::new(),
            );
        }
    };

    let prefetch_status = AtomicU8::new(0);
    let kiro_tunnel_guard = AtomicBool::new(false);
    let bedrock_tunnel_guard = AtomicBool::new(false);
    let mimo_tunnel_guard = AtomicBool::new(false);
    let vertex_tunnel_guard = AtomicBool::new(
        (route_uses_provider(&route, "vertex") || route_uses_provider(&route, "vertex-partner"))
            && crate::vertex_transport::requires_tunnel(&settings, &Value::Null),
    );
    let zed_tunnel_guard = route_uses_provider(&route, "zed")
        && crate::zed_transport::protects_chat(&state.storage, &settings).await;
    let grok_proxy_guard = route_uses_provider(&route, "grok-cli")
        && crate::grok_cli_chat::protects_chat(&state.storage, &settings).await;
    let grok_web_route = route_uses_provider(&route, "grok-web");
    let deepseek_web_route = route_uses_provider(&route, "deepseek-web");
    let trae_proxy_guard = route_uses_provider(&route, "trae")
        && crate::trae_chat::protects_chat(&state.storage, &settings).await;
    let windsurf_proxy_guard = route_uses_provider(&route, "windsurf")
        && crate::windsurf_chat::protects_chat(&state.storage, &settings).await;
    let perplexity_proxy_guard = route_uses_provider(&route, "perplexity-web")
        && crate::perplexity_web_chat::protects_chat(&state.storage, &settings).await;
    let chatgpt_web_route = route_uses_provider(&route, "chatgpt-web");
    let gemini_web_route = route_uses_provider(&route, "gemini-web");
    let muse_proxy_guard = route_uses_provider(&route, "muse-web")
        && (crate::provider_http::requires_protection(&settings, &Value::Null)
            || chat_accounts::active_connections(&state.storage, "muse-web")
                .await
                .map_or(true, |accounts| {
                    accounts.iter().any(|account| {
                        crate::provider_http::requires_protection(&settings, account)
                    })
                }));
    let antigravity_proxy_guard = route_uses_provider(&route, "antigravity")
        && crate::antigravity_transport::protects_chat(&state.storage, &settings).await;
    let cursor_proxy_guard = route_uses_provider(&route, "cursor")
        && (crate::provider_http::requires_protection(&settings, &Value::Null)
            || chat_accounts::active_connections(&state.storage, "cursor")
                .await
                .map_or(true, |accounts| {
                    accounts.iter().any(|account| {
                        crate::provider_http::requires_protection(&settings, account)
                    })
                }));
    let mut qoder_proxy_guard = false;
    for provider in ["qoder", "qoder-cn"] {
        if route_uses_provider(&route, provider)
            && crate::qoder_chat::protects_chat(&state.storage, &settings, provider).await
        {
            qoder_proxy_guard = true;
            break;
        }
    }
    let mut cline_tunnel_guard = false;
    for provider in ["cline", "clinepass"] {
        if route_uses_provider(&route, provider)
            && crate::cline_transport::protects_chat(&state.storage, &settings, provider).await
        {
            cline_tunnel_guard = true;
            break;
        }
    }
    let context = ChatRequest {
        settings: &settings,
        catalog: catalog.as_deref(),
        prefetch_status: &prefetch_status,
        kiro_tunnel_guard: &kiro_tunnel_guard,
        bedrock_tunnel_guard: &bedrock_tunnel_guard,
        mimo_tunnel_guard: &mimo_tunnel_guard,
        vertex_tunnel_guard: &vertex_tunnel_guard,
        headers: &parts.headers,
        format: ChatFormat::from_path(parts.uri.path()),
        client_stream: body["stream"] == true,
        endpoint: parts.uri.path(),
        key: inference_key.as_deref(),
        requested_model: marked_model
            .as_deref()
            .filter(|_| matches!(&route, combo_chat::Route::Single(_, _))),
        direct_loopback,
        devin_key: devin_key.as_deref(),
    };
    let Some(prepared) = prepare_route(&state, context, body, route).await else {
        if deepseek_web_route {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "DeepSeek Web requires a supported saved session and text request",
                HeaderMap::new(),
            );
        }
        if grok_web_route {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Grok Web request requires a supported saved session and text request",
                HeaderMap::new(),
            );
        }
        if chatgpt_web_route {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "This ChatGPT Web request requires a supported native session and text request",
                HeaderMap::new(),
            );
        }
        if gemini_web_route {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Gemini Web requires a supported native session and text request",
                HeaderMap::new(),
            );
        }
        if muse_proxy_guard {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Muse Web request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if cursor_proxy_guard {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Cursor request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if antigravity_proxy_guard {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Antigravity request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if perplexity_proxy_guard {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Perplexity request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if windsurf_proxy_guard {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Windsurf request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if qoder_proxy_guard {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Qoder request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if trae_proxy_guard {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Trae request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if grok_proxy_guard {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Grok CLI request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if zed_tunnel_guard {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Zed request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if cline_tunnel_guard {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Cline request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if bedrock_tunnel_guard.load(Ordering::Relaxed) {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Bedrock request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if mimo_tunnel_guard.load(Ordering::Relaxed) {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This MiMo Desktop request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if vertex_tunnel_guard.load(Ordering::Relaxed) {
            return protected_proxy_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Vertex request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        if kiro_tunnel_guard.load(Ordering::Relaxed) {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "This Kiro request cannot use the configured proxy",
                HeaderMap::new(),
            );
        }
        match prefetch_status.load(Ordering::Relaxed) {
            2 => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "Image URL must resolve to a public HTTP address",
                    HeaderMap::new(),
                );
            }
            1 => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Could not prepare linked image for the selected provider",
                    HeaderMap::new(),
                );
            }
            _ => {}
        }
        return error(
            StatusCode::BAD_REQUEST,
            "The selected provider could not prepare this request",
            HeaderMap::new(),
        );
    };
    let response = run_route(&state, context, prepared, &capabilities).await;
    if let Some(client) = gemini_client {
        client.convert_response(response).await
    } else {
        response
    }
}

pub fn native_provider(provider: &str) -> bool {
    provider.starts_with("openai-compatible-")
        || provider.starts_with("anthropic-compatible-")
        || provider == "anthropic"
        || provider == "claude"
        || provider == "azure"
        || provider == "codex"
        || provider == "grok-cli"
        || provider == "grok-web"
        || provider == "deepseek-web"
        || provider == "devin-cli"
        || provider == "trae"
        || provider == "windsurf"
        || provider == "perplexity-web"
        || provider == "chatgpt-web"
        || provider == "gemini-web"
        || provider == "muse-web"
        || media_chat_message(provider).is_some()
        || matches!(provider, "mimo-free" | "mmf")
        || provider == "antigravity"
        || provider == "cursor"
        || crate::qoder_models::supports(provider)
        || provider == "gemini"
        || provider == "gemini-cli"
        || provider == "github"
        || provider == "gitlab"
        || provider == "kilocode"
        || provider == "iflow"
        || provider == "zed"
        || matches!(provider, "ollama" | "ollama-local")
        || provider == "commandcode"
        || provider == "kiro"
        || crate::cline_transport::is_provider(provider)
        || opencode_chat::is_provider(provider)
        || provider == "opencode"
        || crate::bedrock_transport::is_provider(provider)
        || vertex_provider(provider)
        || static_chat::available(provider)
}

pub(crate) fn media_chat_message(provider: &str) -> Option<&'static str> {
    match provider {
        "veoaifree-web" | "veo-free" => Some(
            "veoaifree-web is a media provider — use /v1/images/generations or /v1/videos/generations",
        ),
        "uc-web" => Some(
            "uc-web is a media provider — use /v1/images/generations or /v1/videos/generations",
        ),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct ChatRequest<'a> {
    settings: &'a Value,
    catalog: Option<&'a Value>,
    prefetch_status: &'a AtomicU8,
    kiro_tunnel_guard: &'a AtomicBool,
    bedrock_tunnel_guard: &'a AtomicBool,
    mimo_tunnel_guard: &'a AtomicBool,
    vertex_tunnel_guard: &'a AtomicBool,
    headers: &'a HeaderMap,
    format: ChatFormat,
    client_stream: bool,
    endpoint: &'a str,
    key: Option<&'a str>,
    requested_model: Option<&'a str>,
    direct_loopback: bool,
    devin_key: Option<&'a str>,
}

enum PreparedRoute {
    Single(Box<PreparedCall>),
    Devin {
        model: String,
        body: Value,
    },
    Unavailable {
        provider: Option<String>,
        status: StatusCode,
        message: Cow<'static, str>,
    },
    Combo(combo_chat::Combo<PreparedRoute>),
    Fusion(Box<PreparedFusion>),
    Smart(Box<PreparedSmart>),
}

#[derive(Clone)]
pub struct Fusion<T> {
    pub members: Vec<T>,
    pub judge: Option<Box<T>>,
    pub tuning: crate::fusion::Tuning,
}

struct PreparedFusion {
    panel: Vec<PreparedRoute>,
    direct: Vec<Option<PreparedRoute>>,
    judge: Option<Box<combo_chat::Route>>,
    tuning: crate::fusion::Tuning,
    body: Value,
}

#[derive(Clone)]
pub struct Smart<T> {
    pub name: String,
    pub members: Vec<(String, T)>,
    pub router: Option<(String, String)>,
    pub config: crate::smart_router::Config,
}

struct PreparedSmart {
    route: Smart<PreparedRoute>,
    body: Value,
}

struct PreparedCall {
    provider: String,
    model: String,
    prepared: chat_input::PreparedChat,
    source_body: Option<Value>,
    target: Transport,
    signature: Option<SignatureContext>,
    catalog_endpoint: Option<Endpoint>,
    headroom: Option<headroom::Config>,
    pxpipe: Option<PxpipeMode>,
}

async fn prepare_route(
    state: &ChatState,
    context: ChatRequest<'_>,
    body: Value,
    route: combo_chat::Route,
) -> Option<PreparedRoute> {
    match route {
        combo_chat::Route::Unavailable {
            provider,
            status,
            message,
        } => Some(PreparedRoute::Unavailable {
            provider,
            status,
            message,
        }),
        combo_chat::Route::Adapter(route, window) => {
            let body = crate::capacity_adapter::strip_history(body, window);
            Box::pin(prepare_route(state, context, body, *route)).await
        }
        combo_chat::Route::Single(provider, _)
            if matches!(provider.as_str(), "mimo-free" | "mmf") =>
        {
            Some(PreparedRoute::Unavailable {
                provider: Some("mimo-free".to_owned()),
                status: StatusCode::GONE,
                message: MIMO_FREE_RETIRED.into(),
            })
        }
        combo_chat::Route::Single(provider, model)
            if opencode_chat::requires_official_client(&provider, &model) =>
        {
            Some(PreparedRoute::Unavailable {
                provider: Some(provider),
                status: StatusCode::FORBIDDEN,
                message: opencode_chat::FREE_CLIENT_REQUIRED.into(),
            })
        }
        combo_chat::Route::Single(provider, model) => {
            if provider == "devin-cli" {
                return Some(PreparedRoute::Devin { model, body });
            }
            if let Some(message) = media_chat_message(&provider) {
                let (status, message) =
                    match chat_accounts::active_connections(&state.storage, &provider).await {
                        Ok(accounts) if accounts.is_empty() => (
                            StatusCode::NOT_FOUND,
                            format!("No active credentials for provider: {provider}").into(),
                        ),
                        Ok(_) => (StatusCode::BAD_REQUEST, format!("[400]: {message}").into()),
                        Err(_) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Failed to select provider credentials".into(),
                        ),
                    };
                return Some(PreparedRoute::Unavailable {
                    provider: Some(provider),
                    status,
                    message,
                });
            }
            let call = if provider.starts_with("openai-compatible-") {
                let (first, other) = if context.format == ChatFormat::Responses {
                    (Transport::Responses, Transport::Chat)
                } else {
                    (Transport::Chat, Transport::Responses)
                };
                match prepare_call(
                    state,
                    context,
                    body.clone(),
                    provider.clone(),
                    model.clone(),
                    Some(first),
                )
                .await
                {
                    Some(call) => Some(call),
                    None => prepare_call(state, context, body, provider, model, Some(other)).await,
                }
            } else {
                prepare_call(state, context, body, provider, model, None).await
            };
            call.map(|call| PreparedRoute::Single(Box::new(call)))
        }
        combo_chat::Route::Combo(combo) => {
            let mut members = Vec::new();
            for route in combo.members {
                let member_body = route_body(body.clone(), &route);
                members.push(Box::pin(prepare_route(state, context, member_body, route)).await?);
            }
            Some(PreparedRoute::Combo(combo_chat::Combo {
                name: combo.name,
                sticky: combo.sticky,
                members,
            }))
        }
        combo_chat::Route::Fusion(fusion) => {
            let panel_context = ChatRequest {
                client_stream: false,
                ..context
            };
            let mut panel = Vec::new();
            let mut direct = Vec::new();
            let panel_body = crate::fusion::panel_body(body.clone())?;
            for route in fusion.members {
                let panel_body = route_body(panel_body.clone(), &route);
                let direct_body = route_body(body.clone(), &route);
                panel.push(
                    Box::pin(prepare_route(
                        state,
                        panel_context,
                        panel_body,
                        route.clone(),
                    ))
                    .await?,
                );
                direct.push(Some(
                    Box::pin(prepare_route(state, context, direct_body, route)).await?,
                ));
            }
            if let Some(judge) = fusion.judge.as_ref() {
                let preflight = crate::fusion::judge_body(body.clone(), &[]);
                let preflight = route_body(preflight, judge);
                Box::pin(prepare_route(
                    state,
                    context,
                    preflight,
                    judge.as_ref().clone(),
                ))
                .await?;
            }
            Some(PreparedRoute::Fusion(Box::new(PreparedFusion {
                panel,
                direct,
                judge: fusion.judge,
                tuning: fusion.tuning,
                body,
            })))
        }
        combo_chat::Route::Smart(smart) => {
            let mut members = Vec::new();
            for (name, route) in smart.members {
                let input = route_body(body.clone(), &route);
                members.push((
                    name,
                    Box::pin(prepare_route(state, context, input, route)).await?,
                ));
            }
            Some(PreparedRoute::Smart(Box::new(PreparedSmart {
                route: Smart {
                    name: smart.name,
                    members,
                    router: smart.router,
                    config: smart.config,
                },
                body,
            })))
        }
    }
}

fn route_body(mut body: Value, route: &combo_chat::Route) -> Value {
    if let combo_chat::Route::Adapter(route, _) = route {
        return route_body(body, route);
    }
    if let combo_chat::Route::Single(provider, model) = route {
        body["model"] = json!(format!("{provider}/{model}"));
    }
    body
}

fn route_uses_provider(route: &combo_chat::Route, expected: &str) -> bool {
    match route {
        combo_chat::Route::Unavailable { .. } => false,
        combo_chat::Route::Single(provider, _) => provider == expected,
        combo_chat::Route::Adapter(member, _) => route_uses_provider(member, expected),
        combo_chat::Route::Combo(combo) => combo
            .members
            .iter()
            .any(|member| route_uses_provider(member, expected)),
        combo_chat::Route::Fusion(fusion) => {
            fusion
                .members
                .iter()
                .any(|member| route_uses_provider(member, expected))
                || fusion
                    .judge
                    .as_deref()
                    .is_some_and(|member| route_uses_provider(member, expected))
        }
        combo_chat::Route::Smart(smart) => {
            smart
                .members
                .iter()
                .any(|(_, member)| route_uses_provider(member, expected))
                || smart
                    .router
                    .as_ref()
                    .is_some_and(|(provider, _)| provider == expected)
        }
    }
}

impl PreparedRoute {
    fn capability_fit(&self, required: &HashSet<&str>, catalog: Option<&Value>) -> bool {
        match self {
            Self::Single(call) => {
                let caps = provider_capabilities_rs::port::model_capabilities(
                    Some(&call.provider),
                    &call.model,
                    catalog,
                );
                required.iter().all(|capability| caps[*capability] == true)
            }
            Self::Devin { .. }
            | Self::Unavailable { .. }
            | Self::Combo(_)
            | Self::Fusion(_)
            | Self::Smart(_) => false,
        }
    }

    fn provider(&self) -> Option<String> {
        match self {
            Self::Single(call) => Some(call.provider.clone()),
            Self::Devin { .. } => Some("devin-cli".into()),
            Self::Unavailable { provider, .. } => provider.clone(),
            Self::Combo(_) | Self::Fusion(_) | Self::Smart(_) => None,
        }
    }
}

fn run_route<'a>(
    state: &'a ChatState,
    context: ChatRequest<'a>,
    prepared: PreparedRoute,
    capabilities: &'a HashSet<&'static str>,
) -> futures_util::future::BoxFuture<'a, Response> {
    Box::pin(async move {
        match prepared {
            PreparedRoute::Single(call) => execute(state, context, *call).await,
            PreparedRoute::Devin { model, body } => {
                crate::devin_acp::respond(
                    &state.storage,
                    context.settings,
                    crate::devin_acp::Caller {
                        direct_loopback: context.direct_loopback,
                        key: context.devin_key,
                        format: context.format,
                        accept: header_text(context.headers, "accept"),
                    },
                    body,
                    &model,
                )
                .await
            }
            PreparedRoute::Unavailable {
                status, message, ..
            } => {
                if status == StatusCode::FORBIDDEN {
                    json_response(
                        status,
                        json!({"error":{"message":message,"type":"permission_error","code":"official_client_required"}}),
                        HeaderMap::new(),
                        true,
                    )
                } else {
                    error(status, &message, HeaderMap::new())
                }
            }
            PreparedRoute::Combo(combo) => {
                combo
                    .execute(
                        capabilities,
                        |route| {
                            (
                                route.provider(),
                                route.capability_fit(capabilities, context.catalog),
                                None,
                            )
                        },
                        |route| run_route(state, context, route, capabilities),
                        |_| {},
                    )
                    .await
            }
            PreparedRoute::Fusion(fusion) => {
                run_fusion(state, context, *fusion, capabilities).await
            }
            PreparedRoute::Smart(smart) => run_smart(state, context, *smart, capabilities).await,
        }
    })
}

async fn run_smart(
    state: &ChatState,
    context: ChatRequest<'_>,
    prepared: PreparedSmart,
    capabilities: &HashSet<&'static str>,
) -> Response {
    let Smart {
        name,
        mut members,
        router,
        config,
    } = prepared.route;
    let models: Vec<_> = members.iter().map(|(model, _)| model.clone()).collect();
    let session = config
        .sticky
        .then(|| crate::smart_router::session(context.headers, &prepared.body))
        .flatten();
    let mut sticky = session
        .as_ref()
        .and_then(|session| crate::smart_router::get(&name, session, &models));
    let mut chosen = false;
    if !(config.strict() && sticky.is_some())
        && let Some((provider, model)) = router
    {
        let prompt = crate::smart_router::prompt(&prepared.body);
        let criteria = crate::smart_router::criteria(&models);
        if !prompt.is_empty()
            && !criteria.is_empty()
            && let Some(raw) = crate::systemone::evaluate_route(
                &state.storage,
                &state.client,
                &provider,
                &model,
                prompt,
                criteria,
            )
            .await
            && let Some(choice) = crate::smart_router::Choice::parse(&raw, &models)
        {
            let change = if let Some(sticky) = sticky.as_ref() {
                sticky != &choice.model
                    && choice
                        .confidence
                        .is_some_and(|value| value >= config.threshold)
            } else {
                choice
                    .confidence
                    .is_none_or(|value| value >= config.threshold)
            };
            if change {
                choice.prioritize(&mut members);
                if let Some(session) = session.as_ref() {
                    crate::smart_router::assign(&name, session, &choice.model);
                    sticky = Some(choice.model);
                }
                chosen = true;
            }
        }
    }
    if !chosen && let Some(model) = sticky.as_deref() {
        crate::smart_router::prioritize_sticky(model, &mut members);
    }
    let combo_name = name.clone();
    combo_chat::Combo {
        name,
        members,
        sticky: None,
    }
    .execute(
        capabilities,
        |(name, route)| {
            (
                route.provider(),
                route.capability_fit(capabilities, context.catalog),
                Some(name.clone()),
            )
        },
        |(_, route)| run_route(state, context, route, capabilities),
        |failed| {
            if sticky.as_deref() == Some(failed)
                && let Some(session) = session.as_ref()
            {
                crate::smart_router::clear(Some(&combo_name), Some(session));
            }
        },
    )
    .await
}

async fn run_fusion(
    state: &ChatState,
    context: ChatRequest<'_>,
    mut fusion: PreparedFusion,
    capabilities: &HashSet<&'static str>,
) -> Response {
    if fusion.panel.is_empty() {
        return combo_chat::combo_error(
            StatusCode::BAD_REQUEST,
            "Fusion combo has no models",
            None,
        );
    }
    let catalog = state.storage.model_catalog();
    let panel: Vec<_> = fusion
        .panel
        .into_iter()
        .map(|route| {
            let state = state.clone();
            let settings = context.settings.clone();
            let catalog = catalog.clone();
            let headers = context.headers.clone();
            let format = context.format;
            let endpoint = context.endpoint.to_owned();
            let key = context.key.map(str::to_owned);
            let direct_loopback = context.direct_loopback;
            let devin_key = context.devin_key.map(str::to_owned);
            let capabilities = capabilities.clone();
            tokio::spawn(async move {
                let prefetch_status = AtomicU8::new(0);
                let kiro_tunnel_guard = AtomicBool::new(false);
                let bedrock_tunnel_guard = AtomicBool::new(false);
                let mimo_tunnel_guard = AtomicBool::new(false);
                let vertex_tunnel_guard = AtomicBool::new(false);
                let context = ChatRequest {
                    settings: &settings,
                    catalog: catalog.as_deref(),
                    prefetch_status: &prefetch_status,
                    kiro_tunnel_guard: &kiro_tunnel_guard,
                    bedrock_tunnel_guard: &bedrock_tunnel_guard,
                    mimo_tunnel_guard: &mimo_tunnel_guard,
                    vertex_tunnel_guard: &vertex_tunnel_guard,
                    headers: &headers,
                    format,
                    client_stream: false,
                    endpoint: &endpoint,
                    key: key.as_deref(),
                    requested_model: None,
                    direct_loopback,
                    devin_key: devin_key.as_deref(),
                };
                run_route(&state, context, route, &capabilities).await
            })
        })
        .collect();
    let answers = crate::fusion::collect(panel, fusion.tuning).await;
    match answers.as_slice() {
        [] => combo_chat::combo_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "All fusion panel models failed",
            None,
        ),
        [(index, _)] => {
            let Some(call) = fusion.direct.get_mut(*index).and_then(Option::take) else {
                return combo_chat::combo_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Fusion model unavailable",
                    None,
                );
            };
            run_route(state, context, call, capabilities).await
        }
        _ => {
            let Some(judge) = fusion.judge else {
                return combo_chat::combo_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Fusion judge unavailable",
                    None,
                );
            };
            let body = route_body(crate::fusion::judge_body(fusion.body, &answers), &judge);
            let Some(judge) = prepare_route(state, context, body, *judge).await else {
                return combo_chat::combo_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Fusion judge unavailable",
                    None,
                );
            };
            run_route(state, context, judge, capabilities).await
        }
    }
}

async fn prepare_call(
    state: &ChatState,
    context: ChatRequest<'_>,
    mut body: Value,
    provider: String,
    model_id: String,
    requested_transport: Option<Transport>,
) -> Option<PreparedCall> {
    let qoder = crate::qoder_models::supports(&provider);
    let windsurf = provider == "windsurf";
    let perplexity = provider == "perplexity-web";
    let grok_web = provider == "grok-web";
    let deepseek_web = provider == "deepseek-web";
    let chatgpt_web = provider == "chatgpt-web";
    let github_claude = provider == "github" && github_claude::matches_model(&model_id);
    let gemini_web = provider == "gemini-web";
    let muse_web = provider == "muse-web";
    let antigravity = provider == "antigravity";
    let cursor = provider == "cursor";
    let format = context.format;
    let accept = header_text(context.headers, "accept");
    let user_agent = header_text(context.headers, "user-agent");
    let client = Client::detect(context.headers, &body);
    let token_saver = header_text(context.headers, TOKEN_SAVER);
    let prompts = system_prompts::selected(context.settings, token_saver);
    let headroom = headroom::config(context.settings, token_saver);
    let kiro_headroom = provider == "kiro" && headroom.is_some();
    let pxpipe = truthy(&context.settings["pxpipeEnabled"]).then(|| state.pxpipe.mode());
    let zed_messages = provider == "zed" && format == ChatFormat::Messages;
    let grok_messages = provider == "grok-cli" && format == ChatFormat::Messages;
    let zed_responses = provider == "zed" && format == ChatFormat::Responses;
    let compress_tools = !token_saver.eq_ignore_ascii_case("off")
        && context.settings.get("rtkEnabled").is_none_or(truthy);
    let Ok(accounts) = chat_accounts::active_connections(&state.storage, &provider).await else {
        return None;
    };
    let mimo_desktop_model = provider == "xiaomi-mimo"
        && crate::mimo_account::account_model(crate::thinking_intent::model_suffix(&model_id).0);
    let mimo_desktop = mimo_desktop_model && accounts.iter().any(crate::mimo_account::has_session);
    if mimo_desktop
        && accounts.iter().any(|account| {
            crate::mimo_account::has_session(account)
                && crate::provider_http::requires_protection(context.settings, account)
        })
    {
        context.mimo_tunnel_guard.store(true, Ordering::Relaxed);
    }
    if crate::bedrock_transport::is_provider(&provider)
        && (crate::bedrock_transport::requires_tunnel(context.settings, &Value::Null)
            || accounts.iter().any(|account| {
                crate::bedrock_transport::requires_tunnel(context.settings, account)
            }))
    {
        context.bedrock_tunnel_guard.store(true, Ordering::Relaxed);
    }
    if vertex_provider(&provider)
        && accounts
            .iter()
            .any(|account| crate::vertex_transport::requires_tunnel(context.settings, account))
    {
        context.vertex_tunnel_guard.store(true, Ordering::Relaxed);
    }
    if provider == "kiro" {
        let mut supported = accounts.is_empty();
        for account in &accounts {
            if account["authType"] == "oauth" {
                match kiro_chat::OAuthRoute::select(&state.storage, context.settings, account).await
                {
                    Ok(route) if route.protected() => {
                        context.kiro_tunnel_guard.store(true, Ordering::Relaxed);
                        supported |= route.supports_protected(account);
                    }
                    Ok(_) => supported |= !unsupported_connection(account, &provider, &model_id),
                    Err(()) => {
                        context.kiro_tunnel_guard.store(true, Ordering::Relaxed);
                    }
                }
            } else {
                if kiro_chat::Connection::api_key(account).is_some()
                    && crate::provider_http::requires_protection(context.settings, account)
                {
                    context.kiro_tunnel_guard.store(true, Ordering::Relaxed);
                }
                supported |= !unsupported_connection(account, &provider, &model_id);
            }
        }
        if !supported {
            return None;
        }
    }
    let catalog_endpoint = if opencode_chat::is_provider(&provider) {
        Some(Endpoint::OpenCode(opencode_chat::select(
            &provider, &model_id, format,
        )?))
    } else if static_chat::available(&provider) {
        Some(Endpoint::Default(static_chat::select(
            &provider, &model_id, format,
        )?))
    } else {
        None
    };
    let target = if let Some(endpoint) = catalog_endpoint {
        match endpoint.wire() {
            ChatFormat::Chat => Transport::Chat,
            ChatFormat::Messages => Transport::Anthropic,
            ChatFormat::Responses => Transport::Responses,
        }
    } else if let Some(requested) = requested_transport {
        requested
    } else {
        transport(accounts.first().unwrap_or(&Value::Null), &provider)
    };
    if github_claude && format == ChatFormat::Chat && !github_claude::supported_body(&body) {
        return None;
    }
    let source_body = (matches!(
        provider.as_str(),
        "codex" | "commandcode" | "grok-cli" | "zed"
    ) || mimo_desktop
        || provider.starts_with("openai-compatible-")
        || target.client_wire() != Some(format)
        || observability::configured(context.settings).is_some())
    .then(|| {
        let mut source = body.clone();
        source["model"] = json!(format!("{provider}/{model_id}"));
        source
    });
    if client == Client::DeepSeekTui && body["stream"] != true {
        body["stream"] = json!(false);
    }
    crate::thinking_intent::apply_provider_mode(
        &mut body,
        &context.settings["providerThinking"][&provider],
    );
    let codex_client = provider == "codex"
        && ["droid", "codex-cli"]
            .iter()
            .any(|vendor| user_agent.to_ascii_lowercase().contains(vendor));
    let claude_native = target == Transport::Anthropic
        && format == ChatFormat::Messages
        && client == Client::Claude
        && claude_passthrough::matches(&provider, context.headers);
    if !accounts.is_empty()
        && accounts.iter().all(|account| {
            (provider != "kiro" && unsupported_connection(account, &provider, &model_id))
                || catalog_endpoint.map_or_else(
                    || transport(account, &provider) != target,
                    |endpoint| endpoint.url(account).is_none(),
                )
        })
    {
        return None;
    }
    if provider == "codex" {
        codex_executor::normalize_images(&mut body, format);
    }
    let normalize_vision = !codex_client
        && !claude_native
        && !cursor
        && !antigravity
        && !muse_web
        && !grok_web
        && !deepseek_web
        && !chatgpt_web
        && !gemini_web
        && provider != "zed";
    if normalize_vision {
        chat_vision::strip_unsupported(&mut body, format, &provider, &model_id, context.catalog);
    }
    if provider == "kiro" {
        kiro_chat::strip_model_media(&mut body, &model_id);
    }
    if (normalize_vision || provider == "codex")
        && !qoder
        && !accounts.is_empty()
        && chat_prefetch::needs_inline(&provider)
    {
        let report = chat_prefetch::apply(&mut body, format.into()).await;
        if report.refused {
            context.prefetch_status.store(2, Ordering::Relaxed);
            return None;
        }
        if report.saw_remote {
            context.prefetch_status.fetch_max(1, Ordering::Relaxed);
        }
        if provider == "codex" && report.unavailable {
            return None;
        }
    }
    if !claude_native
        && !grok_messages
        && !qoder
        && !windsurf
        && !perplexity
        && !grok_web
        && !deepseek_web
        && !chatgpt_web
        && !gemini_web
        && !muse_web
        && !antigravity
        && !cursor
        && provider != "zed"
        && !chat_vision::supported(
            &body,
            format,
            target.client_wire(),
            &provider,
            &model_id,
            context.catalog,
        )
    {
        return None;
    }

    let signature = (target == Transport::Gemini).then(|| {
        state
            .gemini_signatures
            .context(state.storage.clone(), context.headers, &body)
    });
    let upstream_model = catalog_endpoint.map_or(model_id.as_str(), |endpoint| {
        endpoint.upstream_model(&model_id)
    });
    let (upstream_model, thinking) = if matches!(target, Transport::Chat | Transport::Responses)
        && !matches!(provider.as_str(), "codex" | "kiro" | "grok-cli")
        && !github_claude
        && !perplexity
        && !cursor
        && !qoder
        && !deepseek_web
        && !zed_messages
        && !zed_responses
    {
        crate::openai_thinking::capture(
            &mut body,
            &provider,
            upstream_model,
            catalog_endpoint.and_then(Endpoint::thinking_format),
            target == Transport::Responses,
        )
    } else {
        (upstream_model, None)
    };
    let prepared = match target {
        Transport::Chat => {
            let preserve_cache_control =
                catalog_endpoint.is_some_and(Endpoint::preserve_cache_control);
            let converted = if zed_messages {
                crate::zed_transport::messages_to_chat(body, accept)
            } else if zed_responses {
                crate::zed_transport::responses_to_chat(body, accept)
            } else if provider == "kiro" {
                chat_protocol_input::for_kiro(format, body, accept)
            } else if chatgpt_web {
                chat_protocol_input::chatgpt_web(format, body, accept, false)
            } else if github_claude {
                github_claude::to_chat(format, body, accept)
            } else if (qoder || cursor) && format == ChatFormat::Messages {
                chat_protocol_input::anthropic_messages_for_responses(body, accept)
            } else if (qoder || cursor) && format == ChatFormat::Responses {
                chat_protocol_input::responses_preview(body)
            } else if preserve_cache_control && format == ChatFormat::Messages {
                chat_protocol_input::anthropic_messages(body, accept, true)
            } else {
                chat_protocol_input::translate(format, body, accept)
            }?;
            let mut converted = if cursor {
                crate::cursor_chat::prepare_openai(converted)
            } else {
                converted
            };
            if provider == "zed" {
                let fields = converted.as_object_mut()?;
                for field in [
                    "metadata",
                    "prompt_cache_key",
                    "session_id",
                    "conversation_id",
                ] {
                    fields.shift_remove(field);
                }
            }
            if converted["userAgent"] == "antigravity" && !accounts.is_empty() {
                return None;
            }
            let github_claude_controls = github_claude
                && (format != ChatFormat::Chat
                    || chat_protocol_input::unsupported_thinking(&converted)
                    || crate::thinking_intent::model_suffix(upstream_model)
                        .1
                        .is_some());
            match (catalog_endpoint, format) {
                (None, _)
                    if provider == "kiro"
                        || qoder
                        || windsurf
                        || perplexity
                        || grok_web
                        || deepseek_web
                        || chatgpt_web
                        || github_claude_controls
                        || gemini_web
                        || muse_web
                        || cursor =>
                {
                    Some(chat_input::PreparedChat {
                        stream: chat_input::stream_requested(&converted, accept),
                        body: converted,
                    })
                }
                (None, _) if provider == "zed" => crate::zed_transport::supports_body(
                    &state.storage,
                    context.settings,
                    &accounts,
                    upstream_model,
                    &converted,
                    format,
                )
                .await
                .then(|| chat_input::PreparedChat {
                    stream: chat_input::stream_requested(&converted, accept),
                    body: converted,
                }),
                (None, _) if provider == "commandcode" => commandcode_transport::prepare_chat(
                    converted,
                    upstream_model,
                    format != ChatFormat::Chat,
                ),
                (Some(endpoint), _) => chat_input::prepare_static(
                    converted,
                    upstream_model,
                    accept,
                    format != ChatFormat::Chat,
                    endpoint.force_stream(),
                    preserve_cache_control,
                ),
                (None, _)
                    if matches!(provider.as_str(), "azure" | "ollama" | "ollama-local")
                        || headroom.is_some()
                        || compress_tools
                        || !prompts.is_empty() =>
                {
                    chat_input::prepare_static(
                        converted,
                        upstream_model,
                        accept,
                        format != ChatFormat::Chat,
                        false,
                        false,
                    )
                }
                (None, ChatFormat::Chat) => chat_input::prepare(converted, upstream_model, accept),
                (None, _) => chat_input::prepare_translated(converted, upstream_model, accept),
            }
        }
        Transport::Responses => {
            if body["userAgent"] == "antigravity" && !accounts.is_empty() {
                return None;
            }
            if provider == "codex" {
                codex_executor::prepare(
                    format,
                    body,
                    upstream_model,
                    accept,
                    client == Client::Codex,
                )
            } else if provider == "grok-cli" {
                crate::grok_cli_body::prepare(format, body, upstream_model, accept)
            } else {
                responses_request::prepare(format, body, upstream_model, accept)
            }
        }
        Transport::Anthropic => {
            let prepared = if claude_native {
                claude_passthrough::prepare(body, &model_id, accept)
            } else if let Some(Endpoint::Default(endpoint)) = catalog_endpoint {
                endpoint.prepare_messages(format, body, &provider, &model_id, accept)
            } else if matches!(provider.as_str(), "anthropic" | "claude") {
                anthropic_request::prepare_for_provider(format, body, &model_id, accept, true)
            } else {
                anthropic_request::prepare(format, body, &model_id, accept)
            };
            if !accounts.is_empty()
                && prepared
                    .as_ref()
                    .is_some_and(|prepared| prepared.body["userAgent"] == "antigravity")
            {
                return None;
            }
            prepared
        }
        Transport::Gemini => {
            let Some(signature) = &signature else {
                return None;
            };
            if antigravity {
                crate::antigravity_transport::prepare(format, body, &model_id, accept, |id| {
                    signature.lookup(id, &model_id)
                })
            } else {
                let fallback = if provider == "gemini-cli" {
                    gemini_cli_transport::signature()
                } else {
                    gemini_request::DEFAULT_SIGNATURE
                };
                gemini_request::prepare(
                    format,
                    body,
                    &model_id,
                    accept,
                    |id| signature.lookup(id, &model_id),
                    fallback,
                )
            }
        }
    };
    let mut prepared = prepared?;
    if format == ChatFormat::Responses && target == Transport::Chat {
        chat_input::strip_responses_translation_fields(&mut prepared.body);
    }
    client.dedupe_tools(&mut prepared.body, &model_id);
    if let Some(thinking) = thinking {
        thinking.write(&mut prepared.body);
    }
    if compress_tools && !kiro_headroom {
        crate::rtk_compression::apply(&mut prepared.body);
    }
    if headroom.is_none() {
        target.apply_prompts(&mut prepared, &prompts);
        if target == Transport::Chat
            && catalog_endpoint.is_none()
            && !matches!(
                provider.as_str(),
                "azure" | "ollama" | "ollama-local" | "kiro"
            )
            && (compress_tools || !prompts.is_empty())
        {
            chat_input::apply_schema_fallback(&mut prepared.body)?;
        }
        if let Some(endpoint) = catalog_endpoint {
            endpoint.prepare(&provider, &model_id, &mut prepared)?;
        }
        if claude_native && pxpipe != Some(PxpipeMode::Native) {
            claude_passthrough::anchor(&mut prepared.body);
        }
    }
    if provider == "vertex" {
        crate::vertex_transport::prepare_body(&mut prepared.body);
    }
    if provider == "kiro" {
        prepared.body =
            kiro_chat::prepare(&prepared.body, &model_id, context.headers, context.key)?;
        if compress_tools && kiro_headroom {
            crate::rtk_compression::apply(&mut prepared.body);
        }
    }
    if provider == "iflow" {
        crate::iflow_transport::prepare(&mut prepared);
    }
    Some(PreparedCall {
        provider,
        model: model_id,
        prepared,
        source_body,
        target,
        signature,
        catalog_endpoint,
        headroom,
        pxpipe,
    })
}

fn pxpipe_wire(target: Transport, provider: &str, model: &str) -> &'static str {
    match provider {
        "github" if github_claude::matches_model(model) => "claude",
        "github" => "openai-responses",
        "gemini-cli" => "gemini-cli",
        "antigravity" => "antigravity",
        "commandcode" => "commandcode",
        "kiro" => "kiro",
        "ollama" | "ollama-local" => "ollama",
        "vertex" => "vertex",
        _ => match target {
            Transport::Chat => "openai",
            Transport::Responses => "openai-responses",
            Transport::Anthropic => "claude",
            Transport::Gemini => "gemini",
        },
    }
}

fn inference_key(headers: &HeaderMap) -> Option<String> {
    if let Some(key) = header_text(headers, "authorization").strip_prefix("Bearer ") {
        return Some(key.to_owned());
    }
    let key = header_text(headers, "x-api-key");
    (!key.is_empty()).then(|| key.to_owned())
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

fn base_url(account: &Value, target: Transport) -> &str {
    if target == Transport::Gemini {
        return if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
            "http://127.0.0.1:52989/gemini/v1beta/models"
        } else {
            gemini_request::BASE_URL
        };
    }
    account["providerSpecificData"]["baseUrl"]
        .as_str()
        .filter(|url| !url.is_empty())
        .unwrap_or(if target == Transport::Anthropic {
            "https://api.anthropic.com/v1"
        } else {
            DEFAULT_UPSTREAM
        })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Transport {
    Chat,
    Responses,
    Anthropic,
    Gemini,
}

#[derive(Clone, Copy)]
enum Endpoint {
    Default(static_chat::Endpoint),
    OpenCode(opencode_chat::Endpoint),
}

impl Endpoint {
    fn wire(self) -> ChatFormat {
        match self {
            Self::Default(endpoint) => endpoint.wire,
            Self::OpenCode(endpoint) => endpoint.wire,
        }
    }

    fn url(self, account: &Value) -> Option<String> {
        match self {
            Self::Default(endpoint) => endpoint.url(account),
            Self::OpenCode(endpoint) => {
                opencode_chat::supports(account).then(|| endpoint.url().to_owned())
            }
        }
    }

    fn fixture_url(self, provider: &str, account: &Value) -> Option<String> {
        match self {
            Self::Default(endpoint) => endpoint.fixture_url(provider, account),
            Self::OpenCode(endpoint) => endpoint.fixture_url(),
        }
    }

    fn upstream_model(self, model: &str) -> &str {
        match self {
            Self::Default(endpoint) => endpoint.upstream_model(model),
            Self::OpenCode(_) => model,
        }
    }

    fn thinking_format(self) -> Option<&'static str> {
        match self {
            Self::Default(endpoint) => endpoint.thinking_format(),
            Self::OpenCode(endpoint) => endpoint.thinking_format(),
        }
    }

    fn force_stream(self) -> bool {
        match self {
            Self::Default(endpoint) => endpoint.force_stream(),
            Self::OpenCode(endpoint) => endpoint.force_stream(),
        }
    }

    fn preserve_cache_control(self) -> bool {
        matches!(self, Self::Default(endpoint) if endpoint.preserve_cache_control())
    }

    fn timeout(self, default: Duration) -> Duration {
        match self {
            Self::Default(endpoint) => endpoint.timeout(default),
            Self::OpenCode(_) => default,
        }
    }

    fn prepare(
        self,
        provider: &str,
        model: &str,
        request: &mut chat_input::PreparedChat,
    ) -> Option<()> {
        match self {
            Self::Default(endpoint) => endpoint.prepare(provider, model, request),
            Self::OpenCode(endpoint) => {
                endpoint.prepare(request)?;
                request.stream |= endpoint.force_stream();
            }
        }
        Some(())
    }

    fn auth(
        self,
        request: reqwest::RequestBuilder,
        key: &str,
        account: &Value,
        headers: &HeaderMap,
        body: &Value,
    ) -> reqwest::RequestBuilder {
        match self {
            Self::Default(endpoint) => endpoint.auth(request, key, account),
            Self::OpenCode(endpoint) => endpoint.headers(request, account, headers, body),
        }
    }
}

impl Transport {
    fn apply_prompts(self, prepared: &mut chat_input::PreparedChat, prompts: &[&str]) {
        let wire = match self {
            Self::Chat | Self::Responses => system_prompts::Wire::OpenAi,
            Self::Anthropic => system_prompts::Wire::Claude,
            Self::Gemini => system_prompts::Wire::Gemini,
        };
        for prompt in prompts {
            system_prompts::inject(&mut prepared.body, wire, prompt);
        }
    }

    fn headroom_wire(self) -> headroom::Wire {
        match self {
            Self::Chat => headroom::Wire::Chat,
            Self::Responses => headroom::Wire::Responses,
            Self::Anthropic => headroom::Wire::Claude,
            Self::Gemini => headroom::Wire::Gemini,
        }
    }

    fn client_wire(self) -> Option<ChatFormat> {
        match self {
            Self::Chat => Some(ChatFormat::Chat),
            Self::Responses => Some(ChatFormat::Responses),
            Self::Anthropic => Some(ChatFormat::Messages),
            Self::Gemini => None,
        }
    }
}

fn transport(account: &Value, provider: &str) -> Transport {
    if let Some(wire) = crate::bedrock_transport::wire(provider) {
        return match wire {
            ChatFormat::Chat => Transport::Chat,
            ChatFormat::Messages => Transport::Anthropic,
            ChatFormat::Responses => Transport::Responses,
        };
    }
    if matches!(provider, "codex" | "grok-cli") {
        return Transport::Responses;
    }
    if matches!(provider, "gemini" | "gemini-cli" | "vertex" | "antigravity") {
        return Transport::Gemini;
    }
    if matches!(
        provider,
        "azure"
            | "vertex-partner"
            | "ollama"
            | "ollama-local"
            | "commandcode"
            | "kiro"
            | "trae"
            | "windsurf"
            | "perplexity-web"
            | "grok-web"
            | "deepseek-web"
            | "chatgpt-web"
            | "gemini-web"
            | "muse-web"
            | "cursor"
            | "iflow"
            | "gitlab"
            | "kilocode"
    ) || crate::cline_transport::is_provider(provider)
        || crate::qoder_models::supports(provider)
    {
        return Transport::Chat;
    }
    if matches!(provider, "anthropic" | "claude") || provider.starts_with("anthropic-compatible-") {
        return Transport::Anthropic;
    }
    match account["providerSpecificData"]["apiType"].as_str() {
        Some("chat") => Transport::Chat,
        Some("responses") => Transport::Responses,
        _ if provider.contains("responses") => Transport::Responses,
        _ => Transport::Chat,
    }
}

fn native_oauth(provider: &str) -> bool {
    matches!(provider, "codex" | "gemini-cli" | "github" | "kimi")
        || crate::cline_transport::is_provider(provider)
        || static_chat::codebuddy::is_provider(provider)
}

fn minimax_code_provider(provider: &str) -> bool {
    matches!(provider, "minimax-code" | "minimax-code-global")
}

fn vertex_provider(provider: &str) -> bool {
    matches!(provider, "vertex" | "vertex-partner")
}

pub(crate) async fn refresh_kiro_chat(
    storage: &Storage,
    account: &Value,
    route: &kiro_chat::OAuthRoute,
    mode: crate::provider_oauth::RefreshMode,
) -> Result<Value, crate::provider_oauth::RefreshError> {
    use crate::provider_oauth::{RefreshError, refresh_account};
    if !route.verify(storage, account).await {
        return Err(RefreshError::Unsupported);
    }
    let current = refresh_account(storage, account, route.refresh_mode(mode)).await?;
    if !route.verify(storage, &current).await {
        return Err(RefreshError::Unsupported);
    }
    route
        .connection(&current)
        .is_some()
        .then_some(current)
        .ok_or(RefreshError::ReauthenticationRequired)
}

fn unsupported_connection(account: &Value, provider: &str, model: &str) -> bool {
    if provider.starts_with("openai-compatible-") || provider.starts_with("anthropic-compatible-") {
        return ["apiKey", "accessToken"]
            .iter()
            .find_map(|field| account[*field].as_str().filter(|token| !token.is_empty()))
            .is_none_or(|token| token.contains("sk-ant-oat"));
    }
    if provider == "grok-cli" {
        return !crate::grok_cli_chat::supports(account);
    }
    if provider == "grok-web" {
        return !crate::grok_web::supports(account);
    }
    if provider == "deepseek-web" {
        return !crate::deepseek_web::supports(account);
    }
    if provider == "trae" {
        return !crate::trae_chat::supports(account);
    }
    if provider == "windsurf" {
        return !crate::windsurf_chat::supports(account);
    }
    if provider == "perplexity-web" {
        return !crate::perplexity_web_chat::supports(account);
    }
    if provider == "chatgpt-web" {
        return !crate::chatgpt_web::supports(account);
    }
    if provider == "gemini-web" {
        return !crate::gemini_web::supports(account);
    }
    if provider == "muse-web" {
        return !crate::muse_web::supports(account);
    }
    if provider == "antigravity" {
        return !crate::antigravity_transport::supports(account);
    }
    if provider == "cursor" {
        return !crate::cursor_chat::supports_account(account);
    }
    if crate::qoder_models::supports(provider) {
        return !crate::qoder_chat::supports(account);
    }
    if minimax_code_provider(provider) {
        return account["authType"] != "oauth" && account["authType"] != "mcode-lease";
    }
    if provider == "zed" {
        return !crate::zed_transport::supports(account);
    }
    if provider == "gitlab" {
        return false;
    }
    if provider == "glm" && account["authType"] == "oauth" {
        return account["providerSpecificData"]["authMethod"] != "cli_poll"
            || account["accessToken"].as_str().is_none_or(str::is_empty);
    }
    if provider == "muse" && account["authType"] == "oauth" {
        return !static_chat::muse_subscription(account);
    }
    if provider == "xiaomi-mimo" {
        return !crate::mimo_account::has_session(account)
            && crate::provider_imports::xiaomi_cloud_key(account).is_none();
    }
    if provider == "kimi" && account["authType"] == "oauth" {
        return crate::provider_oauth::kimi_portal::saved_device_id(account).is_none()
            || account["accessToken"].as_str().is_none_or(str::is_empty);
    }
    if provider == "nous" {
        return !crate::provider_oauth::nous_connection_compatible(account);
    }
    if provider == "kimchi" {
        return !crate::kimchi_chat::supports(account);
    }
    if provider == "kilocode" {
        return [&account["apiKey"], &account["accessToken"]]
            .into_iter()
            .all(|value| value.as_str().is_none_or(str::is_empty));
    }
    if provider == "claude" {
        return !crate::claude_transport::supports(account);
    }
    if provider == "xai" && account["authType"] == "oauth" {
        return [&account["accessToken"], &account["refreshToken"]]
            .iter()
            .all(|value| value.as_str().is_none_or(str::is_empty));
    }
    if provider == "iflow" {
        return !(crate::iflow_transport::supports(account)
            || crate::iflow_transport::can_refresh(account));
    }
    if provider == "kiro" {
        return !(kiro_chat::supports(account)
            || kiro_chat::needs_refresh(account, false)
                && crate::provider_oauth::kiro_refresh_supported(account, false));
    }
    if provider == "commandcode" {
        return !commandcode_transport::supports(account);
    }
    if crate::cline_transport::is_provider(provider) {
        return !crate::cline_transport::typed_account(provider, account);
    }
    if crate::bedrock_transport::is_provider(provider) {
        return !crate::bedrock_transport::supports(provider, model, account);
    }
    if vertex_provider(provider) {
        return !crate::vertex_transport::supports(provider, account, model);
    }
    if provider == "ollama" {
        return !crate::ollama_local::cloud_credentials(account);
    }
    if provider == "ollama-local" && crate::ollama_local::url(account).is_none() {
        return true;
    }
    if static_chat::codebuddy::is_provider(provider) {
        return !static_chat::codebuddy::supports_account(account);
    }
    if matches!(provider, "codex" | "gemini-cli" | "github") {
        return !crate::provider_oauth::supports_chat_credentials(provider, account);
    }
    if native_oauth(provider) && !(provider == "kimi" && account["authType"] == "apikey") {
        return account["authType"] != "oauth"
            || account["accessToken"].as_str().is_none_or(str::is_empty);
    }
    !matches!(account["authType"].as_str(), Some("apikey" | "api_key"))
        || account["apiKey"]
            .as_str()
            .is_some_and(|key| key.contains("sk-ant-oat"))
}

struct UpstreamError {
    status: StatusCode,
    message: String,
    headers: Box<HeaderMap>,
}

struct BoundChatRoute {
    name: &'static str,
    destination: String,
    route: proxy_transport::Selection,
}

impl BoundChatRoute {
    fn relay(&self) -> Option<&proxy_transport::Route> {
        match &self.route {
            proxy_transport::Selection::Routed(route @ proxy_transport::Route::Relay { .. }) => {
                Some(route)
            }
            _ => None,
        }
    }

    async fn select(
        storage: &Storage,
        settings: &Value,
        account: &Value,
        name: &'static str,
        destination: String,
        allow_relay: bool,
    ) -> Option<Self> {
        use proxy_transport::{Route, Selection};
        let route = crate::provider_http::select(storage, settings, account, &destination)
            .await
            .ok()?;
        match route {
            Selection::Routed(Route::MissingStrict) => None,
            Selection::Routed(Route::Relay { .. }) if !allow_relay => None,
            _ => Some(Self {
                name,
                destination,
                route,
            }),
        }
    }
}

fn nous_credentials_unavailable() -> UpstreamError {
    UpstreamError {
        status: StatusCode::UNAUTHORIZED,
        message: "The Nous provider credentials are unavailable".into(),
        headers: Box::default(),
    }
}

async fn refresh_xai_chat(
    storage: &Storage,
    account: &Value,
    destination: &str,
    expected: &proxy_transport::Selection,
) -> Result<Value, UpstreamError> {
    crate::provider_oauth::refresh_xai_inference(storage, account, destination, expected)
        .await
        .map_err(|error| match error {
            crate::provider_oauth::RefreshError::Unsupported
            | crate::provider_oauth::RefreshError::Storage => {
                transport_failure("[502]: xAI outbound route changed during credential refresh")
            }
            _ => UpstreamError {
                status: StatusCode::UNAUTHORIZED,
                message: "xAI OAuth credential refresh failed".into(),
                headers: Box::default(),
            },
        })
}

async fn execute(state: &ChatState, context: ChatRequest<'_>, call: PreparedCall) -> Response {
    let PreparedCall {
        provider,
        model,
        prepared,
        source_body,
        target,
        signature,
        catalog_endpoint,
        headroom,
        pxpipe,
    } = call;
    let template = prepared;
    let ChatRequest {
        settings,
        catalog: _,
        prefetch_status: _,
        kiro_tunnel_guard: _,
        bedrock_tunnel_guard: _,
        mimo_tunnel_guard: _,
        vertex_tunnel_guard: _,
        format,
        client_stream,
        headers,
        key,
        endpoint,
        requested_model,
        direct_loopback: _,
        devin_key: _,
    } = context;
    let endpoint = endpoint.to_owned();
    let key = key.map(str::to_owned);
    let user_agent = header_text(headers, "user-agent").to_ascii_lowercase();
    let mut excluded = HashSet::new();
    let mut attempted_kiro_protected = false;
    let mut last: Option<UpstreamError> = None;
    loop {
        let selected = match chat_accounts::select_for_request(
            &state.storage,
            &provider,
            &model,
            requested_model.unwrap_or(&model),
            &excluded,
            None,
        )
        .await
        {
            Ok(selected) => selected,
            Err(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to select provider credentials",
                    HeaderMap::new(),
                );
            }
        };
        let mut account = match selected {
            Selection::CoolingDown(retry) => {
                let reason = last
                    .as_ref()
                    .map(|failure| failure.message.as_str())
                    .or(retry.last_error.as_deref())
                    .unwrap_or("Unavailable");
                let text = format!(
                    "[{provider}/{model}] {reason} ({})",
                    retry.retry_after_human
                );
                let mut headers = last.map(|failure| *failure.headers).unwrap_or_default();
                if let Ok(value) = HeaderValue::from_str(&retry.retry_after_seconds.to_string()) {
                    headers.insert(header::RETRY_AFTER, value);
                }
                return json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({"error": { "message": text }}),
                    headers,
                    false,
                );
            }
            Selection::Empty => {
                if let Some(failure) = last {
                    return error(failure.status, &failure.message, *failure.headers);
                }
                return error(
                    StatusCode::NOT_FOUND,
                    &format!("No active credentials for provider: {provider}"),
                    HeaderMap::new(),
                );
            }
            Selection::Ready(account) => account,
        };
        let Some(id) = account["id"].as_str().map(str::to_owned) else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid provider connection",
                HeaderMap::new(),
            );
        };
        let selected_transport = transport(&account, &provider);
        let alternative =
            if provider.starts_with("openai-compatible-") && selected_transport != target {
                let Some(request) = source_body.as_ref() else {
                    unreachable!("compatible requests retain the original request body");
                };
                let Some(call) = prepare_call(
                    state,
                    context,
                    request.clone(),
                    provider.clone(),
                    model.clone(),
                    Some(selected_transport),
                )
                .await
                else {
                    last = Some(UpstreamError {
                        status: StatusCode::BAD_REQUEST,
                        message:
                            "This connection cannot represent the request in its configured format"
                                .into(),
                        headers: Box::default(),
                    });
                    excluded.insert(id);
                    continue;
                };
                Some(call)
            } else {
                None
            };
        let (template, source_body, target) = alternative
            .as_ref()
            .map_or((&template, &source_body, target), |call| {
                (&call.prepared, &call.source_body, call.target)
            });
        let mimo_desktop = provider == "xiaomi-mimo"
            && crate::mimo_account::has_session(&account)
            && crate::mimo_account::account_model(crate::thinking_intent::model_suffix(&model).0);
        if provider == "xiaomi-mimo"
            && !mimo_desktop
            && crate::provider_imports::xiaomi_cloud_key(&account).is_none()
        {
            excluded.insert(id);
            last.get_or_insert_with(|| UpstreamError {
                status: StatusCode::UNAUTHORIZED,
                message: "This MiMo model requires a cloud API key".into(),
                headers: Box::default(),
            });
            continue;
        }
        let mimo_protected =
            mimo_desktop && crate::provider_http::requires_protection(settings, &account);
        let xai_oauth = provider == "xai" && account["authType"] == "oauth";
        let routed_name = match (provider.as_str(), account["authType"].as_str()) {
            ("codex", Some("oauth" | "access_token")) => Some("Codex OAuth"),
            ("gemini-cli", Some("oauth")) => Some("Gemini CLI OAuth"),
            ("kimi", Some("oauth")) => Some("Kimi OAuth"),
            ("xai", Some("oauth")) => Some("xAI OAuth"),
            ("iflow", Some("oauth")) => Some("iFlow OAuth"),
            ("kiro", Some("api_key" | "apikey")) => Some("Kiro API key"),
            _ => None,
        };
        let bound_route = if let Some(name) = routed_name {
            let destination = match provider.as_str() {
                "codex" => Some(codex_executor::endpoint(
                    is_compact_path(&endpoint) || truthy(&template.body["_compact"]),
                )),
                "gemini-cli" => Some(gemini_cli_transport::url(template.stream)),
                "iflow" => Some(crate::iflow_transport::ENDPOINT.to_owned()),
                "kiro" => kiro_chat::Connection::api_key(&account)
                    .map(|connection| connection.upstream_url()),
                _ => catalog_endpoint.and_then(|endpoint| endpoint.url(&account)),
            };
            let Some(destination) = destination else {
                last = Some(transport_failure(format!("[502]: Invalid {name} endpoint")));
                excluded.insert(id);
                continue;
            };
            let Some(binding) = BoundChatRoute::select(
                &state.storage,
                settings,
                &account,
                name,
                destination,
                xai_oauth || matches!(provider.as_str(), "kiro" | "iflow" | "codex" | "gemini-cli"),
            )
            .await
            else {
                last = Some(transport_failure(format!(
                    "[502]: {name} outbound route is unavailable",
                )));
                excluded.insert(id);
                continue;
            };
            Some(binding)
        } else {
            None
        };
        let mut github_oauth: HashMap<GithubRoute, BoundChatRoute> = HashMap::new();
        let mut github_relay = None;
        if provider == "github" {
            let preferred = GithubRoute::preferred(&model, &state.github_routes);
            for mode in std::iter::once(preferred)
                .chain((preferred == GithubRoute::Chat).then_some(GithubRoute::Responses))
            {
                if let Ok(route) = crate::provider_http::select_authenticated(
                    &state.storage,
                    settings,
                    &account,
                    mode.endpoint(),
                )
                .await
                    && github_oauth
                        .get(&preferred)
                        .is_none_or(|first| crate::provider_http::same_egress(&first.route, &route))
                {
                    github_oauth.insert(
                        mode,
                        BoundChatRoute {
                            name: "GitHub Copilot OAuth",
                            destination: mode.endpoint().to_owned(),
                            route,
                        },
                    );
                }
            }
            let Some(binding) = github_oauth.get(&preferred) else {
                last = Some(transport_failure(
                    "[502]: GitHub Copilot OAuth outbound route is unavailable",
                ));
                excluded.insert(id);
                continue;
            };
            github_relay = binding
                .relay()
                .and_then(proxy_transport::Route::relay_identity);
        }
        let kiro_oauth = if provider == "kiro" && account["authType"] == "oauth" {
            match kiro_chat::OAuthRoute::select(&state.storage, settings, &account).await {
                Ok(route) => Some(route),
                Err(()) => {
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "[502]: Kiro OAuth route is unavailable",
                        HeaderMap::new(),
                    );
                }
            }
        } else {
            None
        };
        let kiro_protected = kiro_oauth
            .as_ref()
            .is_some_and(kiro_chat::OAuthRoute::protected);
        if attempted_kiro_protected && !kiro_protected {
            excluded.insert(id);
            continue;
        }
        attempted_kiro_protected |= kiro_protected;
        let cline_egress = if crate::cline_transport::is_provider(&provider) {
            match crate::cline_transport::egress(&state.storage, settings, &account).await {
                Ok(proxy) => proxy,
                Err(message) => {
                    return protected_proxy_failure(
                        StatusCode::SERVICE_UNAVAILABLE,
                        message,
                        HeaderMap::new(),
                    );
                }
            }
        } else {
            None
        };
        let cline_protected = cline_egress.is_some();
        let trae_protected =
            provider == "trae" && crate::provider_http::requires_protection(settings, &account);
        let windsurf_protected =
            provider == "windsurf" && crate::provider_http::requires_protection(settings, &account);
        let perplexity_protected = provider == "perplexity-web"
            && crate::provider_http::requires_protection(settings, &account);
        let muse_protected =
            provider == "muse-web" && crate::provider_http::requires_protection(settings, &account);
        let antigravity_protected = provider == "antigravity"
            && crate::provider_http::requires_protection(settings, &account);
        let cursor_protected =
            provider == "cursor" && crate::provider_http::requires_protection(settings, &account);
        let qoder = crate::qoder_models::supports(&provider);
        let qoder_protected =
            qoder && crate::provider_http::requires_protection(settings, &account);
        let vertex_protected = vertex_provider(&provider)
            && crate::vertex_transport::requires_tunnel(settings, &account);
        let unsupported = if let Some(route) = &kiro_oauth
            && route.protected()
        {
            !route.supports_protected(&account)
        } else {
            unsupported_connection(&account, &provider, &model)
        };
        if unsupported
            || catalog_endpoint.map_or_else(
                || transport(&account, &provider) != target,
                |endpoint| endpoint.url(&account).is_none(),
            )
        {
            if perplexity_protected {
                return protected_proxy_failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Perplexity connection changed during selection",
                    HeaderMap::new(),
                );
            }
            if windsurf_protected {
                return protected_proxy_failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Windsurf connection changed during selection",
                    HeaderMap::new(),
                );
            }
            if qoder_protected {
                return protected_proxy_failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Qoder connection changed during selection",
                    HeaderMap::new(),
                );
            }
            if trae_protected {
                return protected_proxy_failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Trae connection changed during selection",
                    HeaderMap::new(),
                );
            }
            if vertex_protected {
                return protected_proxy_failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "This Vertex request cannot use the configured proxy",
                    HeaderMap::new(),
                );
            }
            if cline_protected {
                return protected_proxy_failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Cline connection changed during selection",
                    HeaderMap::new(),
                );
            }
            last = Some(UpstreamError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "Provider connection credentials or transport are unavailable".into(),
                headers: Box::default(),
            });
            excluded.insert(id);
            continue;
        }
        let zed_egress = if provider == "zed" {
            match crate::zed_transport::egress(&state.storage, settings, &account).await {
                Ok(egress) => Some(egress),
                Err(message) => {
                    return protected_proxy_failure(
                        StatusCode::SERVICE_UNAVAILABLE,
                        message,
                        HeaderMap::new(),
                    );
                }
            }
        } else {
            None
        };
        let zed_protected = zed_egress.as_ref().is_some_and(Option::is_some);
        let grok_delivery = if provider == "grok-cli" {
            match crate::grok_cli_chat::bind(&state.storage, &account).await {
                Some(delivery) => Some(delivery),
                None => {
                    return protected_proxy_failure(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Grok CLI configured proxy is unavailable",
                        HeaderMap::new(),
                    );
                }
            }
        } else {
            None
        };
        let grok_protected = grok_delivery
            .as_ref()
            .is_some_and(|binding| binding.protected);
        let bedrock_egress = if crate::bedrock_transport::is_provider(&provider) {
            match crate::bedrock_transport::select_egress(
                &state.storage,
                settings,
                &account,
                &model,
                template.stream,
            )
            .await
            {
                Ok(egress) => Some(egress),
                Err(message) => {
                    return protected_proxy_failure(
                        StatusCode::SERVICE_UNAVAILABLE,
                        message,
                        HeaderMap::new(),
                    );
                }
            }
        } else {
            None
        };
        let bedrock_protected = bedrock_egress
            .as_ref()
            .is_some_and(crate::bedrock_transport::Egress::protected);
        let vertex_route = if vertex_protected {
            match crate::vertex_transport::Protected::select(
                &state.storage,
                settings,
                &provider,
                &model,
                template.stream,
                &account,
            )
            .await
            {
                Ok(egress) => Some(egress),
                Err(message) => {
                    return protected_proxy_failure(
                        StatusCode::SERVICE_UNAVAILABLE,
                        message,
                        HeaderMap::new(),
                    );
                }
            }
        } else {
            None
        };
        let kiro_refresh = kiro_oauth
            .as_ref()
            .is_some_and(|route| route.supports_refresh(&account));
        let renewing_kiro = kiro_refresh && kiro_chat::needs_refresh(&account, kiro_protected);
        let nous_refresh =
            provider == "nous" && crate::provider_oauth::nous_refresh_supported(&account);
        let renewing_nous = nous_refresh && crate::provider_oauth::nous_refresh_due(&account);
        let minimax_code = minimax_code_provider(&provider);
        let xai_refresh = xai_oauth
            && bound_route.is_some()
            && account["refreshToken"]
                .as_str()
                .is_some_and(|token| !token.is_empty());
        let iflow_refresh = provider == "iflow" && crate::iflow_transport::can_refresh(&account);
        let chat_routed_refresh =
            iflow_refresh || matches!(provider.as_str(), "codex" | "gemini-cli");
        let chat_relay = matches!(provider.as_str(), "codex" | "gemini-cli")
            .then(|| {
                bound_route
                    .as_ref()
                    .and_then(BoundChatRoute::relay)
                    .and_then(proxy_transport::Route::relay_identity)
            })
            .flatten();
        let mut renewing_iflow = false;
        let mut renewing_xai = false;
        let mut renewed_chat_relay = false;
        if renewing_kiro && let Some(route) = &kiro_oauth {
            let result = refresh_kiro_chat(
                &state.storage,
                &account,
                route,
                crate::provider_oauth::RefreshMode::Proactive,
            )
            .await;
            match result {
                Ok(current) => {
                    account = current;
                }
                Err(failure) => {
                    let unavailable =
                        failure == crate::provider_oauth::RefreshError::ProviderUnavailable;
                    last = Some(UpstreamError {
                        status: if unavailable {
                            StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            StatusCode::UNAUTHORIZED
                        },
                        message: format!("Kiro credential refresh failed: {failure}"),
                        headers: Box::default(),
                    });
                    excluded.insert(id);
                    continue;
                }
            }
        } else if !cline_protected
            && (renewing_nous || native_oauth(&provider) || minimax_code || iflow_refresh)
            && account["authType"] == "oauth"
            && (chat_relay.is_none()
                || (truthy(&account["refreshToken"])
                    && crate::provider_oauth::refresh_due_for_provider(&provider, &account)))
        {
            match crate::provider_oauth::refresh_account(
                &state.storage,
                &account,
                if provider == "github" {
                    crate::provider_oauth::RefreshMode::github(false, github_relay)
                } else if chat_routed_refresh {
                    chat_relay.map_or(
                        crate::provider_oauth::RefreshMode::ChatRouted { rejected: false },
                        crate::provider_oauth::RefreshMode::RelayRouted,
                    )
                } else {
                    crate::provider_oauth::RefreshMode::Proactive
                },
            )
            .await
            {
                Ok(current) => {
                    renewing_iflow =
                        iflow_refresh && current["lastRefreshAt"] != account["lastRefreshAt"];
                    renewed_chat_relay = chat_relay.is_some()
                        && (current["lastRefreshAt"] != account["lastRefreshAt"]
                            || current["accessToken"] != account["accessToken"]);
                    account = current;
                }
                Err(crate::provider_oauth::RefreshError::AccountUnavailable) => {
                    excluded.insert(id);
                    continue;
                }
                Err(_) => (),
            }
        }
        if xai_refresh
            && let Some(binding) = &bound_route
            && (account["accessToken"].as_str().is_none_or(str::is_empty)
                || crate::provider_oauth::refresh_due_for_provider("xai", &account))
        {
            match refresh_xai_chat(
                &state.storage,
                &account,
                &binding.destination,
                &binding.route,
            )
            .await
            {
                Ok(current) => {
                    account = current;
                    renewing_xai = true;
                }
                Err(failure)
                    if failure.status == StatusCode::BAD_GATEWAY
                        || account["accessToken"].as_str().is_none_or(str::is_empty) =>
                {
                    last = Some(failure);
                    excluded.insert(id);
                    continue;
                }
                Err(_) => (),
            }
        }
        if provider == "iflow" && !crate::iflow_transport::supports(&account) {
            last = Some(UpstreamError {
                status: StatusCode::UNAUTHORIZED,
                message: "iFlow provider credentials have expired".into(),
                headers: Box::default(),
            });
            excluded.insert(id);
            continue;
        }
        if provider == "nous" && crate::provider_oauth::nous_inference_token(&account).is_none() {
            last = Some(nous_credentials_unavailable());
            excluded.insert(id);
            continue;
        }
        let missing_token = match provider.as_str() {
            "codex" | "gemini-cli" => account["accessToken"].as_str().is_none_or(str::is_empty),
            "github" => github_executor::token(&account).is_none(),
            _ => false,
        };
        if missing_token {
            last = Some(UpstreamError {
                status: StatusCode::UNAUTHORIZED,
                message: "Provider OAuth credentials could not be renewed".into(),
                headers: Box::default(),
            });
            excluded.insert(id);
            continue;
        }
        if provider == "gemini-cli" && google_code_assist::saved_project(&account).is_none() {
            use google_code_assist::ProjectError;
            let mut discovered =
                google_code_assist::discover_project(&state.storage, &account, chat_relay).await;
            if matches!(discovered, Err(ProjectError::Rejected))
                && !renewed_chat_relay
                && let Ok(updated) = crate::provider_oauth::refresh_account(
                    &state.storage,
                    &account,
                    chat_relay.map_or(
                        crate::provider_oauth::RefreshMode::ChatRouted { rejected: true },
                        crate::provider_oauth::RefreshMode::RelayRouted,
                    ),
                )
                .await
            {
                renewed_chat_relay = chat_relay.is_some()
                    && (updated["lastRefreshAt"] != account["lastRefreshAt"]
                        || updated["accessToken"] != account["accessToken"]);
                discovered =
                    google_code_assist::discover_project(&state.storage, &updated, chat_relay)
                        .await;
            }
            match discovered {
                Ok(current) => account = current,
                Err(failure) => {
                    let (status, message) = match failure {
                        ProjectError::Missing => (
                            StatusCode::BAD_REQUEST,
                            "Missing Gemini CLI project configuration",
                        ),
                        ProjectError::Rejected => (
                            StatusCode::UNAUTHORIZED,
                            "Gemini CLI project discovery rejected the OAuth credentials",
                        ),
                        ProjectError::AccountChanged => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            "Gemini CLI account changed during project discovery",
                        ),
                        ProjectError::Route => (
                            StatusCode::BAD_GATEWAY,
                            "Gemini CLI project discovery proxy is unavailable",
                        ),
                    };
                    last = Some(UpstreamError {
                        status,
                        message: message.into(),
                        headers: Box::default(),
                    });
                    excluded.insert(id);
                    continue;
                }
            }
        }
        let minimax_lease = minimax_code && account["authType"] == "mcode-lease";
        if minimax_lease {
            match crate::provider_oauth::minimax_code::lease::credentials(&account, None).await {
                Ok(current) => account = current,
                Err(message) => {
                    last = Some(UpstreamError {
                        status: StatusCode::SERVICE_UNAVAILABLE,
                        message: message.into(),
                        headers: Box::default(),
                    });
                    excluded.insert(id);
                    continue;
                }
            }
        }
        let pending = crate::usage_activity::begin(&account, &provider, &model);
        let pxpipe_wire = pxpipe.map(|_| pxpipe_wire(target, &provider, &model));
        let pxpipe_native = pxpipe == Some(PxpipeMode::Native) && pxpipe_wire == Some("claude");
        let mut rewritten = (headroom.is_some()
            || native_oauth(&provider)
            || pxpipe_native
            || provider == "kimchi")
            .then(|| chat_input::PreparedChat {
                body: template.body.clone(),
                stream: template.stream,
            });
        if let (Some(config), Some(rewritten)) = (&headroom, rewritten.as_mut()) {
            if provider != "commandcode" {
                let upstream_model = rewritten.body["model"]
                    .as_str()
                    .unwrap_or(&model)
                    .to_owned();
                let wire = if provider == "kiro" {
                    headroom::Wire::Kiro
                } else {
                    target.headroom_wire()
                };
                headroom::apply(
                    &state.client,
                    config,
                    &mut rewritten.body,
                    wire,
                    &upstream_model,
                )
                .await;
                target.apply_prompts(
                    rewritten,
                    &system_prompts::selected(settings, header_text(headers, TOKEN_SAVER)),
                );
            }
            if target == Transport::Anthropic
                && !pxpipe_native
                && format == ChatFormat::Messages
                && claude_passthrough::matches(&provider, headers)
            {
                claude_passthrough::anchor(&mut rewritten.body);
            }
            if let Some(endpoint) = catalog_endpoint {
                if endpoint.prepare(&provider, &model, rewritten).is_none() {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "Invalid provider request body",
                        HeaderMap::new(),
                    );
                }
            } else if target == Transport::Chat
                && !matches!(
                    provider.as_str(),
                    "azure" | "ollama" | "ollama-local" | "kiro"
                )
                && chat_input::apply_schema_fallback(&mut rewritten.body).is_none()
            {
                return error(
                    StatusCode::BAD_REQUEST,
                    "Invalid response format",
                    HeaderMap::new(),
                );
            }
        }
        let mut codex = if provider == "codex" {
            let exchange = rewritten.as_mut().and_then(|body| {
                let session = state.codex_sessions.pick(
                    source_body.as_ref().unwrap_or(&body.body),
                    &account,
                    headers,
                    key.as_deref(),
                );
                codex_executor::finish(
                    &mut body.body,
                    &model,
                    &account,
                    session,
                    format,
                    is_compact_path(&endpoint),
                )
            });
            if exchange.is_none() {
                return error(
                    StatusCode::BAD_REQUEST,
                    "Invalid Codex request body",
                    HeaderMap::new(),
                );
            }
            exchange
        } else {
            None
        };
        if provider == "gemini-cli"
            && rewritten
                .as_mut()
                .and_then(|prepared| {
                    let session = state.codex_sessions.pick(
                        source_body.as_ref().unwrap_or(&prepared.body),
                        &account,
                        headers,
                        key.as_deref(),
                    );
                    gemini_cli_transport::envelope(&mut prepared.body, &account, session)
                })
                .is_none()
        {
            return error(
                StatusCode::BAD_REQUEST,
                "Missing Gemini CLI project configuration",
                HeaderMap::new(),
            );
        }
        let github_mode = if provider != "github" {
            None
        } else if github_claude::matches_model(&model) {
            Some(GithubRoute::Messages)
        } else {
            Some(GithubRoute::Responses)
        };
        let mut github_request = rewritten
            .as_ref()
            .and_then(|prepared| {
                let mode = github_mode?;
                let body = match mode {
                    GithubRoute::Messages => github_claude::prepare(prepared.body.clone(), &model),
                    GithubRoute::Responses => {
                        github_responses::prepare(prepared.body.clone(), &model)
                    }
                    GithubRoute::Chat => None,
                }?;
                Some(chat_input::PreparedChat {
                    body,
                    stream: prepared.stream,
                })
            })
            .zip(github_mode)
            .map(|(request, route)| (route, request));
        if (github_mode == Some(GithubRoute::Messages) && github_request.is_none())
            || github_mode == Some(GithubRoute::Responses)
                && rewritten
                    .as_mut()
                    .and_then(|prepared| github_executor::prepare(&mut prepared.body, &model))
                    .is_none()
        {
            return error(
                StatusCode::BAD_REQUEST,
                "Invalid GitHub Copilot request body",
                HeaderMap::new(),
            );
        }
        let pxpipe_summary = if let Some(wire) = pxpipe_wire {
            let outgoing = github_request
                .as_mut()
                .map(|(_, call)| call)
                .or(rewritten.as_mut());
            let outgoing_body = outgoing.as_ref().map_or(&template.body, |call| &call.body);
            let upstream_model = outgoing_body["model"].as_str().unwrap_or(&model);
            let result = state
                .pxpipe
                .compress(settings, outgoing_body, wire, upstream_model)
                .await;
            if let Some(call) = outgoing {
                if let Some(body) = result.body {
                    call.body = body;
                }
                if target == Transport::Anthropic
                    && pxpipe_native
                    && format == ChatFormat::Messages
                    && claude_passthrough::matches(&provider, headers)
                {
                    claude_passthrough::anchor(&mut call.body);
                }
            }
            state
                .pxpipe
                .record(&provider, &model, result.summary.clone());
            Some(result.summary)
        } else {
            None
        };
        if provider == "kimchi"
            && let Some(prepared) = rewritten.as_mut()
        {
            crate::kimchi_chat::prepare(&state.storage, &account, &model, &mut prepared.body);
        }
        let prepared = rewritten.as_ref().unwrap_or(&template);
        let trace = observability::configured(settings).map(|config| {
            observability::Trace::new(
                state.recorder.clone(),
                config,
                observability::TraceRequest {
                    provider: &provider,
                    model: &model,
                    connection: &id,
                    request: source_body.as_ref().unwrap_or(&prepared.body),
                    upstream: &prepared.body,
                    stream: prepared.stream,
                    pxpipe: pxpipe_summary,
                },
            )
        });
        let upstream_request = UpstreamRequest {
            state,
            source: source_body.as_ref().unwrap_or(&prepared.body),
            target,
            model: &model,
            incoming: headers,
            provider: &provider,
            catalog_endpoint,
            settings,
            headroom: headroom.as_ref(),
            kiro_oauth: kiro_oauth.as_ref(),
            bound_route: bound_route.as_ref(),
            github_oauth: &github_oauth,
        };
        let mut cline_renewed = None;
        let mut upstream = if let Some(delivery) = grok_delivery {
            let exchange = crate::grok_cli_chat::Exchange::new(
                delivery,
                &account,
                &prepared.body,
                source_body.as_ref().unwrap_or(&prepared.body),
                headers,
                key.as_deref(),
            );
            match exchange
                .complete(&state.storage, &account, &prepared.body)
                .await
            {
                Ok((response, _)) if response.status().is_success() => Ok(response),
                Ok((response, renewed)) => {
                    let mut failure = parse_failure(response).await;
                    let mut secrets = crate::grok_cli_models::secrets(&account);
                    secrets.extend(exchange.relay_secret());
                    if let Some(current) = &renewed {
                        secrets.extend(crate::grok_cli_models::secrets(current));
                    }
                    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
                    failure.message = crate::grok_cli_models::redact(&failure.message, &secrets);
                    Err(failure)
                }
                Err(failure) => Err(UpstreamError {
                    status: failure.status,
                    message: failure.message.into(),
                    headers: Box::default(),
                }),
            }
        } else if let Some(egress) = &vertex_route {
            egress
                .send(
                    &prepared.body,
                    crate::provider_overrides::transport_headers(settings, &provider),
                    CONNECT_TIMEOUT,
                )
                .await
                .map_err(transport_failure)
        } else if provider == "claude" {
            match crate::claude_transport::complete(
                &state.storage,
                settings,
                &account,
                &model,
                prepared,
                header_text(headers, "anthropic-beta"),
            )
            .await
            {
                Ok((response, _)) if response.status().is_success() => Ok(response),
                Ok((response, mut secrets)) => {
                    let mut failure = parse_failure(response).await;
                    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
                    for secret in secrets
                        .iter()
                        .filter_map(|secret| std::str::from_utf8(secret).ok())
                    {
                        failure.message = failure.message.replace(secret, "[redacted]");
                    }
                    Err(failure)
                }
                Err(error) => Err(UpstreamError {
                    status: error.status,
                    message: error.message.to_owned(),
                    headers: Box::default(),
                }),
            }
        } else if provider == "antigravity" {
            crate::antigravity_transport::complete(
                &state.storage,
                settings,
                &account,
                source_body.as_ref().unwrap_or(&prepared.body),
                prepared,
                headers,
                &state.antigravity_sessions,
            )
            .await
            .map(|(response, _)| response)
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: failure.message,
                headers: failure.headers,
            })
        } else if provider == "cursor" {
            crate::cursor_chat::complete(
                &state.storage,
                settings,
                &account,
                &model,
                headers,
                &prepared.body,
                prepared.stream,
            )
            .await
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: failure.message,
                headers: Box::default(),
            })
        } else if mimo_desktop {
            let mut body = prepared.body.clone();
            prepare_mimo_desktop(&mut body, source_body.as_ref());
            crate::mimo_account::send(
                &state.storage,
                settings,
                &account,
                crate::mimo_account::Request::Chat {
                    body: &body,
                    stream: prepared.stream,
                },
            )
            .await
            .map_err(|error| {
                use crate::mimo_account::Error;
                UpstreamError {
                    status: match error {
                        Error::Missing | Error::Rejected => StatusCode::UNAUTHORIZED,
                        Error::Route | Error::Unavailable => StatusCode::BAD_GATEWAY,
                    },
                    message: error.message().into(),
                    headers: Box::default(),
                }
            })
        } else if provider == "deepseek-web" {
            crate::deepseek_web::complete(
                &state.storage,
                settings,
                &account,
                key.as_deref(),
                &model,
                &prepared.body,
                prepared.stream,
            )
            .await
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: failure.message,
                headers: Box::default(),
            })
        } else if provider == "grok-web" {
            crate::grok_web::complete(
                &state.storage,
                settings,
                &account,
                &model,
                &prepared.body,
                prepared.stream,
            )
            .await
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: format!("[{}]: {}", failure.status.as_u16(), failure.message),
                headers: Box::default(),
            })
        } else if provider == "gemini-web" {
            crate::gemini_web::complete(
                &state.storage,
                settings,
                &account,
                &model,
                &prepared.body,
                prepared.stream,
            )
            .await
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: failure.message,
                headers: Box::default(),
            })
        } else if provider == "chatgpt-web" {
            crate::chatgpt_web::complete(
                &state.storage,
                settings,
                &account,
                &model,
                &prepared.body,
                prepared.stream,
            )
            .await
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: format!("[{}]: {}", failure.status.as_u16(), failure.message),
                headers: Box::default(),
            })
        } else if provider == "muse-web" {
            crate::muse_web::complete(
                &state.storage,
                settings,
                &account,
                &model,
                &prepared.body,
                prepared.stream,
            )
            .await
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: format!("[{}]: {}", failure.status.as_u16(), failure.message),
                headers: Box::default(),
            })
        } else if provider == "perplexity-web" {
            crate::perplexity_web_chat::complete(
                &state.storage,
                settings,
                &account,
                key.as_deref(),
                &model,
                &prepared.body,
                prepared.stream,
            )
            .await
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: format!("[{}]: {}", failure.status.as_u16(), failure.message),
                headers: Box::default(),
            })
        } else if provider == "windsurf" {
            crate::windsurf_chat::complete(
                &state.storage,
                settings,
                &account,
                &model,
                &prepared.body,
            )
            .await
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: format!("[{}]: {}", failure.status.as_u16(), failure.message),
                headers: Box::default(),
            })
        } else if qoder {
            crate::qoder_chat::complete(&state.storage, settings, &account, &model, &prepared.body)
                .await
                .map_err(|failure| UpstreamError {
                    status: failure.status,
                    message: format!("[{}]: {}", failure.status.as_u16(), failure.message),
                    headers: Box::default(),
                })
        } else if provider == "trae" {
            crate::trae_chat::complete(
                &state.storage,
                settings,
                &account,
                &model,
                &prepared.body,
                prepared.stream,
            )
            .await
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: format!("[{}]: {}", failure.status.as_u16(), failure.message),
                headers: Box::default(),
            })
        } else if let Some(egress) = bedrock_egress {
            fetch_bedrock(
                &provider,
                &model,
                &account,
                prepared,
                egress,
                &state.storage,
                settings,
            )
            .await
        } else if let Some(egress) = zed_egress {
            let thread = crate::zed_transport::thread_id(
                source_body.as_ref().unwrap_or(&prepared.body),
                headers,
                &id,
                key.as_deref(),
            );
            crate::zed_transport::complete(
                &state.storage,
                &account,
                &model,
                &prepared.body,
                &thread,
                format,
                egress,
            )
            .await
            .map_err(|failure| UpstreamError {
                status: failure.status,
                message: format!("[{}]: {}", failure.status.as_u16(), failure.message),
                headers: Box::default(),
            })
        } else if let Some(route) = &cline_egress {
            crate::cline_transport::protected_chat(
                &state.storage,
                &provider,
                route,
                &account,
                (&prepared.body, prepared.stream),
                crate::provider_overrides::transport_headers(settings, &provider),
                CONNECT_TIMEOUT,
            )
            .await
            .map(|(response, renewed)| {
                cline_renewed = renewed;
                response
            })
            .map_err(|(status, message)| UpstreamError {
                status,
                message: format!("[{}]: {message}", status.as_u16()),
                headers: Box::default(),
            })
        } else {
            fetch_account(
                upstream_request,
                &account,
                prepared,
                codex.as_ref(),
                github_request.as_ref(),
            )
            .await
        };
        let rejected = |status| matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
        if minimax_lease
            && upstream
                .as_ref()
                .is_err_and(|failure| rejected(failure.status))
            && let Ok(updated) = crate::provider_oauth::minimax_code::lease::credentials(
                &account,
                account["accessToken"].as_str(),
            )
            .await
        {
            upstream = fetch_account(
                upstream_request,
                &updated,
                prepared,
                codex.as_ref(),
                github_request.as_ref(),
            )
            .await;
        }
        if ((!cline_protected
            && !renewed_chat_relay
            && (native_oauth(&provider) || nous_refresh && !renewing_nous)
            && upstream
                .as_ref()
                .is_ok_and(|response| rejected(response.status())))
            || kiro_refresh
                && !renewing_kiro
                && upstream
                    .as_ref()
                    .is_err_and(|failure| rejected(failure.status))
            || minimax_code
                && upstream
                    .as_ref()
                    .is_err_and(|failure| rejected(failure.status))
            || xai_refresh
                && !renewing_xai
                && upstream
                    .as_ref()
                    .is_err_and(|failure| rejected(failure.status))
            || iflow_refresh
                && !renewing_iflow
                && upstream
                    .as_ref()
                    .is_err_and(|failure| rejected(failure.status)))
            && account["authType"] == "oauth"
            && let Some(updated) = (if xai_oauth && let Some(binding) = &bound_route {
                refresh_xai_chat(
                    &state.storage,
                    &account,
                    &binding.destination,
                    &binding.route,
                )
                .await
                .ok()
            } else if let Some(route) = &kiro_oauth {
                refresh_kiro_chat(
                    &state.storage,
                    &account,
                    route,
                    crate::provider_oauth::RefreshMode::Rejected,
                )
                .await
                .ok()
            } else {
                crate::provider_oauth::refresh_account(
                    &state.storage,
                    &account,
                    if provider == "github" {
                        crate::provider_oauth::RefreshMode::github(true, github_relay)
                    } else if chat_routed_refresh {
                        chat_relay.map_or(
                            crate::provider_oauth::RefreshMode::ChatRouted { rejected: true },
                            crate::provider_oauth::RefreshMode::RelayRouted,
                        )
                    } else {
                        crate::provider_oauth::RefreshMode::Rejected
                    },
                )
                .await
                .ok()
            })
            && if provider == "github" {
                github_executor::token(&updated)
                    .is_some_and(|token| Some(token) != github_executor::token(&account))
            } else if provider == "nous" {
                crate::provider_oauth::nous_inference_token(&updated).is_some_and(|token| {
                    Some(token) != crate::provider_oauth::nous_inference_token(&account)
                })
            } else if iflow_refresh {
                crate::iflow_transport::supports(&updated)
                    && (updated["accessToken"] != account["accessToken"]
                        || updated["apiKey"] != account["apiKey"])
            } else {
                updated["accessToken"] != account["accessToken"]
            }
        {
            if let Some(exchange) = codex.as_mut() {
                exchange.refresh_binding(&updated);
            }
            let retry = fetch_account(
                upstream_request,
                &updated,
                prepared,
                codex.as_ref(),
                github_request.as_ref(),
            )
            .await;
            if retry
                .as_ref()
                .is_ok_and(|response| response.status().is_success())
            {
                upstream = retry;
            }
        }
        let mut failure = match upstream {
            Ok(upstream) if upstream.status().is_success() => {
                let usage = chat_usage::callback(
                    state.storage.clone(),
                    provider.clone(),
                    model.clone(),
                    id.clone(),
                    key.clone(),
                    endpoint.clone(),
                );
                let watched = trace.clone();
                let usage: chat_output::UsageCallback = Box::new(move |tokens| {
                    if let Some(trace) = &watched {
                        trace.usage(&tokens);
                    }
                    usage(tokens);
                });
                let outgoing = match target {
                    Transport::Chat | Transport::Anthropic | Transport::Gemini => {
                        let wire = if let Some(signature) = signature.clone() {
                            ResponseWire::Gemini(signature)
                        } else if target == Transport::Anthropic {
                            ResponseWire::Messages
                        } else {
                            ResponseWire::Chat
                        };
                        chat_output::respond(
                            upstream,
                            chat_output::Output {
                                request_body: prepared.body.clone(),
                                source_body: source_body.clone(),
                                provider: &provider,
                                model: &model,
                                wire,
                                format,
                            },
                            prepared.stream,
                            (catalog_endpoint.is_some_and(Endpoint::force_stream)
                                || matches!(provider.as_str(), "commandcode" | "zed"))
                                && !client_stream,
                            Some(usage),
                            trace.as_ref(),
                        )
                        .await
                    }
                    Transport::Responses if client_stream && format != ChatFormat::Responses => {
                        chat_output::stream_response(
                            upstream,
                            chat_output::Output {
                                request_body: prepared.body.clone(),
                                source_body: source_body.clone(),
                                provider: &provider,
                                model: &model,
                                wire: ResponseWire::Responses,
                                format,
                            },
                            Some(usage),
                        )
                        .await
                    }
                    Transport::Responses => {
                        responses_upstream::respond(
                            upstream,
                            responses_upstream::Output {
                                provider: &provider,
                                model: &model,
                                client: format,
                                client_stream,
                                executor_stream: prepared.stream,
                                clean_stream_usage: provider == "codex"
                                    && !user_agent.contains("droid")
                                    && !user_agent.contains("codex-cli"),
                            },
                            Some(usage),
                            trace.as_ref(),
                        )
                        .await
                    }
                };
                if outgoing.status().is_success() {
                    let _ = chat_accounts::mark_success(&state.storage, &id, &model).await;
                    if provider == "antigravity" {
                        crate::antigravity_quota_cache::success(&state.storage, &id, &model);
                    }
                    let mut outgoing = if let Some(trace) = trace {
                        trace.observe(outgoing).await
                    } else {
                        outgoing
                    };
                    if vertex_protected
                        || bedrock_protected
                        || zed_protected
                        || cline_protected
                        || grok_protected
                        || trae_protected
                        || qoder_protected
                        || windsurf_protected
                        || perplexity_protected
                        || provider == "grok-web"
                        || provider == "deepseek-web"
                        || provider == "chatgpt-web"
                        || provider == "gemini-web"
                        || muse_protected
                        || mimo_protected
                        || antigravity_protected
                        || cursor_protected
                        || github_relay.is_some()
                    {
                        outgoing.extensions_mut().insert(combo_chat::StopFallback);
                    }
                    return pending.respond(outgoing);
                }
                parse_output_failure(outgoing).await
            }
            Ok(response) => parse_failure(response).await,
            Err(failure) => failure,
        };
        let mut lock_model = model.as_str();
        let codebuddy = static_chat::codebuddy::is_provider(&provider);
        let codebuddy_reset = if codebuddy {
            static_chat::codebuddy::rate_limit(failure.status, &failure.message).and_then(
                |(message, reset)| {
                    failure.status = StatusCode::TOO_MANY_REQUESTS;
                    failure.message = message;
                    reset
                },
            )
        } else {
            None
        };
        if let Some(egress) = &vertex_route {
            failure.message = egress.redact_error(&failure.message);
        }
        if let Some(secret) = bound_route
            .as_ref()
            .and_then(BoundChatRoute::relay)
            .and_then(proxy_transport::Route::relay_secret)
        {
            failure.message = failure.message.replace(secret, "[redacted]");
        }
        if provider == "nous" {
            failure.message = crate::provider_oauth::redact_nous_error(&account, &failure.message);
        }
        if crate::cline_transport::is_provider(&provider) {
            if let Some(current) = &cline_renewed {
                failure.message = crate::cline_transport::redact_error(current, &failure.message);
            }
            failure.message = crate::cline_transport::redact_error(&account, &failure.message);
            if let Some(secret) = cline_egress
                .as_ref()
                .and_then(crate::provider_http::ProtectedEgress::relay_secret)
            {
                failure.message = failure.message.replace(secret, "[redacted]");
            }
        }
        if codebuddy
            || matches!(
                provider.as_str(),
                "codex" | "gemini-cli" | "github" | "kimi" | "muse" | "xiaomi-mimo"
            )
        {
            for secret in ["apiKey", "accessToken", "refreshToken"]
                .into_iter()
                .filter_map(|field| account[field].as_str())
                .filter(|secret| !secret.is_empty())
            {
                failure.message = failure.message.replace(secret, "[redacted]");
            }
        }
        if provider == "github"
            && let Some(token) = github_executor::token(&account)
        {
            failure.message = failure.message.replace(token, "[redacted]");
        }
        if let Some(secret) = github_oauth
            .values()
            .filter_map(BoundChatRoute::relay)
            .find_map(proxy_transport::Route::relay_secret)
        {
            failure.message = failure.message.replace(secret, "[redacted]");
        }
        if mimo_desktop
            && let Some(secret) = account["providerSpecificData"]["mimoPassToken"]
                .as_str()
                .filter(|secret| !secret.is_empty())
        {
            failure.message = failure.message.replace(secret, "[redacted]");
        }
        let reset = if provider == "codex" {
            codex_executor::quota_reset(failure.status, &mut failure.message)
        } else if provider == "github" {
            github_executor::monthly_reset(
                failure.status.as_u16(),
                &failure.message,
                chrono::Utc::now().timestamp_millis(),
            )
            .inspect(|_| lock_model = "")
        } else {
            codebuddy_reset
        };
        pending.fail();
        if let Some(trace) = trace {
            trace.fail(failure.status.as_u16(), &failure.message);
        }
        let quota_reset = if provider == "antigravity"
            && matches!(
                failure.status,
                StatusCode::CONFLICT | StatusCode::TOO_MANY_REQUESTS
            ) {
            crate::antigravity_quota_cache::limited(&state.storage, &id, &model).await
        } else {
            None
        };
        if vertex_protected
            || bedrock_protected
            || zed_protected
            || cline_protected
            || grok_protected
            || trae_protected
            || qoder_protected
            || windsurf_protected
            || perplexity_protected
            || provider == "grok-web"
            || provider == "deepseek-web"
            || provider == "chatgpt-web"
            || provider == "gemini-web"
            || muse_protected
            || mimo_protected
            || antigravity_protected
            || cursor_protected
            || github_relay.is_some()
            || provider == "antigravity" && failure.status == StatusCode::FORBIDDEN
        {
            if let Some(reset) = quota_reset {
                let seconds = reset
                    .abs_diff(chrono::Utc::now().timestamp_millis())
                    .div_ceil(1000)
                    .max(1);
                if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                    failure.headers.entry(header::RETRY_AFTER).or_insert(value);
                }
            }
            return protected_proxy_failure(failure.status, &failure.message, *failure.headers);
        }
        if quota_reset.is_some() {
            excluded.insert(id);
            last = Some(failure);
            continue;
        }
        let retry = chat_accounts::mark_unavailable(
            &state.storage,
            &provider,
            &id,
            lock_model,
            failure.status.as_u16(),
            &failure.message,
            reset,
        )
        .await;
        if !retry.is_ok_and(|retry| retry.should_fallback) {
            return error(failure.status, &failure.message, *failure.headers);
        }
        excluded.insert(id);
        last = Some(failure);
    }
}

pub(crate) fn prepare_mimo_desktop(body: &mut Value, source: Option<&Value>) {
    let source = source.unwrap_or(&Value::Null);
    let effort = [
        &body["reasoning_effort"],
        &source["reasoning_effort"],
        &body["output_config"]["effort"],
        &source["output_config"]["effort"],
    ]
    .into_iter()
    .find(|value| truthy(value))
    .map(|value| crate::chat_output::js_text(value).to_lowercase());
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    if let Some(effort) = effort {
        fields.shift_remove("reasoning_effort");
        let mut output = fields
            .get("output_config")
            .and_then(Value::as_object)
            .or_else(|| source["output_config"].as_object())
            .cloned()
            .unwrap_or_default();
        output.insert(
            "effort".into(),
            json!(if effort == "xhigh" { "high" } else { &effort }),
        );
        fields.insert("output_config".into(), Value::Object(output));
    } else if let Some(output) = source.get("output_config") {
        fields
            .entry("output_config")
            .or_insert_with(|| output.clone());
    }
    for (field, default) in [("temperature", 1.0), ("top_p", 0.95)] {
        if fields.get(field).is_none_or(Value::is_null) {
            fields.insert(field.into(), json!(default));
        }
    }
}

fn protected_proxy_failure(status: StatusCode, message: &str, headers: HeaderMap) -> Response {
    let mut response = error(status, message, headers);
    response.extensions_mut().insert(combo_chat::StopFallback);
    response
}

async fn fetch_bedrock(
    provider: &str,
    model: &str,
    account: &Value,
    prepared: &chat_input::PreparedChat,
    egress: crate::bedrock_transport::Egress,
    storage: &Storage,
    settings: &Value,
) -> Result<reqwest::Response, UpstreamError> {
    let request = crate::bedrock_transport::Exchange::new(
        provider, model, account, prepared, egress, storage, settings,
    )
    .await
    .map_err(transport_failure)?;
    let (response, redactions) = request.send().await.map_err(transport_failure)?;
    if response.status().is_success() {
        return Ok(response);
    }
    let mut failure = parse_failure(response).await;
    failure.message = redactions.redact(&failure.message);
    Err(failure)
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) enum GithubRoute {
    Chat,
    Responses,
    Messages,
}

impl GithubRoute {
    pub(crate) fn preferred(model: &str, routes: &github_responses::Routes) -> Self {
        if github_claude::matches_model(model) {
            Self::Messages
        } else if routes.has(model) {
            Self::Responses
        } else {
            Self::Chat
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::Chat => github_executor::url(),
            Self::Messages => github_claude::url(),
            Self::Responses => github_responses::url(),
        }
    }
}

#[derive(Clone, Copy)]
struct UpstreamRequest<'a> {
    state: &'a ChatState,
    source: &'a Value,
    target: Transport,
    model: &'a str,
    incoming: &'a HeaderMap,
    provider: &'a str,
    catalog_endpoint: Option<Endpoint>,
    settings: &'a Value,
    headroom: Option<&'a headroom::Config>,
    kiro_oauth: Option<&'a kiro_chat::OAuthRoute>,
    bound_route: Option<&'a BoundChatRoute>,
    github_oauth: &'a HashMap<GithubRoute, BoundChatRoute>,
}

async fn fetch_account(
    upstream: UpstreamRequest<'_>,
    account: &Value,
    prepared: &chat_input::PreparedChat,
    codex: Option<&codex_executor::Exchange>,
    github: Option<&(GithubRoute, chat_input::PreparedChat)>,
) -> Result<reqwest::Response, UpstreamError> {
    let UpstreamRequest {
        state,
        model,
        provider,
        ..
    } = upstream;
    if github.is_none_or(|(route, _)| {
        *route == GithubRoute::Responses
            && upstream.github_oauth.contains_key(&GithubRoute::Chat)
            && (!state.github_routes.has(model) || !upstream.github_oauth.contains_key(route))
    }) {
        let result = send_account(upstream, account, prepared, codex, GithubRoute::Chat).await?;
        if github.is_none() || result.status() != StatusCode::BAD_REQUEST {
            return Ok(result);
        }
        let failure = parse_failure(result).await;
        if !upstream.github_oauth.contains_key(&GithubRoute::Responses)
            || !github_executor::needs_responses(model, failure.status.as_u16(), &failure.message)
        {
            return Err(failure);
        }
        state.github_routes.remember(model);
    }
    let (route, request) = github.expect("prepared GitHub alternate request");
    send_account(upstream, account, request, codex, *route)
        .await
        .map(|response| {
            let stall = chat_output::stall_timeout(provider);
            match route {
                GithubRoute::Messages => {
                    github_claude::adapt(response, model, request.stream, stall)
                }
                GithubRoute::Responses => {
                    github_responses::adapt(response, model, request.stream, stall)
                }
                GithubRoute::Chat => response,
            }
        })
}

async fn send_account(
    upstream: UpstreamRequest<'_>,
    account: &Value,
    prepared: &chat_input::PreparedChat,
    codex: Option<&codex_executor::Exchange>,
    github: GithubRoute,
) -> Result<reqwest::Response, UpstreamError> {
    let UpstreamRequest {
        state,
        source,
        target,
        model,
        incoming,
        provider,
        catalog_endpoint,
        settings,
        headroom,
        kiro_oauth,
        bound_route,
        github_oauth,
    } = upstream;
    let bound_route = if provider == "github" {
        Some(github_oauth.get(&github).ok_or_else(|| {
            transport_failure(
                "[502]: GitHub Copilot outbound route is unavailable for this endpoint",
            )
        })?)
    } else {
        bound_route
    };
    let base = base_url(account, target);
    let base = base.strip_suffix('/').unwrap_or(base);
    let vertex = if vertex_provider(provider) {
        Some(
            crate::vertex_transport::Exchange::resolve(
                &state.client,
                provider,
                model,
                prepared.stream,
                account,
            )
            .await
            .map_err(transport_failure)?,
        )
    } else {
        None
    };
    let ollama_body = if matches!(provider, "ollama" | "ollama-local") {
        Some(
            crate::ollama_local::prepare(prepared.body.clone(), model, prepared.stream)
                .ok_or_else(|| UpstreamError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Invalid Ollama chat request body".into(),
                    headers: Box::default(),
                })?,
        )
    } else {
        None
    };
    let commandcode = if provider == "commandcode" {
        let mut envelope =
            commandcode_transport::prepare(&prepared.body, source, incoming, model, account)
                .map_err(|message| UpstreamError {
                    status: StatusCode::BAD_REQUEST,
                    message,
                    headers: Box::default(),
                })?;
        if let Some(config) = headroom {
            headroom::apply(
                &state.client,
                config,
                &mut envelope,
                headroom::Wire::CommandCode,
                model,
            )
            .await;
            for prompt in system_prompts::selected(settings, header_text(incoming, TOKEN_SAVER)) {
                system_prompts::inject(&mut envelope, system_prompts::Wire::CommandCode, prompt);
            }
        }
        Some(envelope)
    } else {
        None
    };
    let kiro_tunnel = kiro_oauth.is_some_and(kiro_chat::OAuthRoute::tunneled);
    let kiro = if provider == "kiro" {
        let connection = if let Some(binding) = kiro_oauth {
            binding.connection(account)
        } else {
            kiro_chat::Connection::api_key(account)
                .or_else(|| kiro_chat::Connection::from_account(account))
        };
        Some(connection.ok_or_else(|| transport_failure("[502]: Invalid Kiro connection"))?)
    } else {
        None
    };
    let kiro_body = kiro
        .as_ref()
        .map(|connection| connection.body(&prepared.body, account["id"].as_str()));
    let path = match target {
        Transport::Chat => "chat/completions".to_owned(),
        Transport::Responses => "responses".to_owned(),
        Transport::Anthropic => "messages".to_owned(),
        Transport::Gemini => format!(
            "{}:{}",
            model,
            if prepared.stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            },
        ),
    };
    let url = if let Some(codex) = codex {
        codex.url()
    } else if provider == "github" {
        github.endpoint().to_owned()
    } else if provider == "iflow" {
        crate::iflow_transport::ENDPOINT.to_owned()
    } else if provider == "gitlab" {
        crate::gitlab_transport::endpoint(account)
            .ok_or_else(|| transport_failure("[502]: Invalid GitLab endpoint"))?
    } else if provider == "kilocode" {
        crate::provider_imports::KILOCODE_CHAT_ENDPOINT.to_owned()
    } else if provider == "azure" {
        crate::azure_transport::url(account, model)
    } else if let Some(connection) = &kiro {
        if kiro_oauth.is_some() || bound_route.is_some() {
            connection.upstream_url()
        } else {
            connection.url()
        }
    } else if commandcode.is_some() {
        commandcode_transport::url().to_owned()
    } else if provider == "ollama" {
        crate::ollama_local::CLOUD_CHAT.to_owned()
    } else if provider == "ollama-local" {
        crate::ollama_local::url(account)
            .ok_or_else(|| transport_failure("[502]: Invalid Ollama endpoint"))?
    } else if crate::cline_transport::is_provider(provider) {
        crate::cline_transport::url()
    } else if let Some(vertex) = &vertex {
        vertex.url().to_owned()
    } else if provider == "gemini-cli" {
        gemini_cli_transport::url(prepared.stream)
    } else if provider == "anthropic" {
        "https://api.anthropic.com/v1/messages".to_owned()
    } else if let Some(
        kind @ (crate::provider_index::Compatible::OpenAi
        | crate::provider_index::Compatible::Anthropic),
    ) = crate::provider_index::compatible(provider)
    {
        use crate::provider_index::Compatible;
        use crate::upstream::{ANTHROPIC_NODE_PATHS, OPENAI_NODE_PATHS};
        let (default, paths) = if kind == Compatible::Anthropic {
            ("https://api.anthropic.com/v1", ANTHROPIC_NODE_PATHS)
        } else {
            (DEFAULT_UPSTREAM, OPENAI_NODE_PATHS)
        };
        crate::upstream::connection_base(account, Some(default))
            .and_then(|base| crate::upstream::node_url(base, &format!("/{path}"), paths))
            .map(String::from)
            .ok_or_else(|| UpstreamError {
                status: StatusCode::BAD_REQUEST,
                message: "Invalid compatible provider base URL".into(),
                headers: Box::default(),
            })?
    } else if let Some(endpoint) = catalog_endpoint {
        let Some(url) = endpoint.url(account) else {
            return Err(UpstreamError {
                status: StatusCode::BAD_REQUEST,
                message: "Missing provider endpoint configuration".into(),
                headers: Box::default(),
            });
        };
        url
    } else {
        format!("{base}/{path}")
    };
    let official_anthropic = provider == "anthropic"
        || url::Url::parse(&url)
            .is_ok_and(|endpoint| crate::upstream::anthropic_official(&endpoint));
    let attributed_body = crate::provider_oauth::nous_portal::compatible_tags(
        provider,
        &url,
        account,
        &prepared.body,
    )
    .map(|tags| {
        let mut body = prepared.body.clone();
        body["tags"] = Value::Array(tags);
        body
    });
    let timeout = catalog_endpoint.map_or(CONNECT_TIMEOUT, |endpoint| {
        endpoint.timeout(CONNECT_TIMEOUT)
    });
    let route = if vertex.is_some() {
        proxy_transport::Route::Direct
    } else if crate::cline_transport::is_provider(provider) {
        if !matches!(
            crate::cline_transport::egress(&state.storage, settings, account).await,
            Ok(None)
        ) {
            return Err(transport_failure(
                "[502]: Cline direct route is unavailable",
            ));
        }
        proxy_transport::Route::Direct
    } else if let Some(binding) = kiro_oauth {
        if url != binding.endpoint || !binding.verify(&state.storage, account).await {
            return Err(transport_failure(
                "[502]: Kiro OAuth route changed before sending",
            ));
        }
        binding.request_route()
    } else if let Some(binding) = bound_route {
        let provider_name = binding.name;
        if url != binding.destination {
            return Err(transport_failure(format!(
                "[502]: {provider_name} endpoint changed before sending",
            )));
        }
        let current = state.storage.read_settings().await.ok_or_else(|| {
            transport_failure(format!(
                "[502]: Could not load {provider_name} outbound settings"
            ))
        })?;
        let actual = crate::provider_http::select(&state.storage, &current, account, &url)
            .await
            .map_err(|_| {
                transport_failure(format!(
                    "[502]: {provider_name} outbound route is unavailable"
                ))
            })?;
        if actual != binding.route {
            return Err(transport_failure(format!(
                "[502]: {provider_name} outbound route changed before sending",
            )));
        }
        match actual {
            proxy_transport::Selection::ExplicitBypass => proxy_transport::Route::Direct,
            proxy_transport::Selection::Routed(route) => route,
        }
    } else {
        proxy_transport::select(&state.storage, settings, account, &url)
            .await
            .map_err(|_| transport_failure("[502]: Could not resolve proxy route"))?
    };
    let kiro_proxy = if kiro.is_some()
        && let proxy_transport::Route::Proxy { url: proxy, .. } = &route
    {
        Some(
            kiro_chat::proxy_client(proxy, &url)
                .ok_or_else(|| transport_failure("[502]: Could not use the Kiro proxy"))?,
        )
    } else {
        None
    };
    if matches!(route, proxy_transport::Route::MissingStrict) {
        return Err(transport_failure(
            "[502]: [ProxyFetch] Proxy required but none resolved (strictProxy=true)",
        ));
    }
    let url = match &route {
        _ if let Some((_, target)) = &kiro_proxy => target.clone(),
        proxy_transport::Route::Relay { url, .. } => url.clone(),
        proxy_transport::Route::Direct if kiro.is_some() => {
            kiro.as_ref().map_or(url, kiro_chat::Connection::url)
        }
        proxy_transport::Route::Direct if vertex.is_some() => vertex
            .as_ref()
            .and_then(crate::vertex_transport::Exchange::fixture_url)
            .unwrap_or(url),
        proxy_transport::Route::Direct if provider == "gemini-cli" => {
            gemini_cli_transport::local_url(prepared.stream).unwrap_or(url)
        }
        proxy_transport::Route::Direct if provider == "ollama" => {
            crate::ollama_local::cloud_fixture()
                .map(str::to_owned)
                .unwrap_or(url)
        }
        proxy_transport::Route::Direct if github == GithubRoute::Messages => {
            github_claude::fixture_url().unwrap_or(url)
        }
        proxy_transport::Route::Direct if github == GithubRoute::Responses => {
            github_responses::fixture_url().unwrap_or(url)
        }
        proxy_transport::Route::Direct if provider == "github" => {
            github_executor::fixture_url().unwrap_or(url)
        }
        proxy_transport::Route::Direct if provider == "iflow" => {
            crate::iflow_transport::fixture_url().unwrap_or(url)
        }
        proxy_transport::Route::Direct if provider == "gitlab" => {
            crate::gitlab_transport::fixture_url(&url)
        }
        proxy_transport::Route::Direct
            if provider == "kilocode"
                && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") =>
        {
            "http://127.0.0.1:52989/kilocode-device/api/openrouter/chat/completions".into()
        }
        proxy_transport::Route::Direct if commandcode.is_some() => {
            commandcode_transport::fixture_url()
                .map(str::to_owned)
                .unwrap_or(url)
        }
        proxy_transport::Route::Direct if crate::cline_transport::is_provider(provider) => {
            crate::cline_transport::fixture_url().unwrap_or(url)
        }
        proxy_transport::Route::Direct if codex.is_some() => codex
            .and_then(codex_executor::Exchange::fixture_url)
            .unwrap_or(url),
        proxy_transport::Route::Direct
            if provider == "anthropic"
                && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") =>
        {
            "http://127.0.0.1:52989/anthropic/v1/messages".into()
        }
        proxy_transport::Route::Direct if catalog_endpoint.is_some() => catalog_endpoint
            .and_then(|endpoint| endpoint.fixture_url(provider, account))
            .unwrap_or(url),
        _ => url,
    };
    let key = if provider == "nous" {
        crate::provider_oauth::nous_inference_token(account)
            .ok_or_else(nous_credentials_unavailable)?
    } else if provider == "github" {
        github_executor::token(account).ok_or_else(|| UpstreamError {
            status: StatusCode::UNAUTHORIZED,
            message: "GitHub Copilot credentials are unavailable".into(),
            headers: Box::default(),
        })?
    } else if let Some(binding) = bound_route
        && provider != "kiro"
    {
        account["accessToken"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| UpstreamError {
                status: StatusCode::UNAUTHORIZED,
                message: format!("{} credentials are unavailable or expired", binding.name),
                headers: Box::default(),
            })?
    } else {
        account["apiKey"]
            .as_str()
            .filter(|key| !key.is_empty())
            .or_else(|| {
                account["accessToken"]
                    .as_str()
                    .filter(|token| !token.is_empty())
            })
            .unwrap_or("undefined")
    };
    let mut attempts = 0;
    let mut stream_attempts = 0;
    let mut fresh = false;
    let mut overrides = crate::provider_overrides::transport_headers(settings, provider);
    if bound_route.is_some() {
        overrides.remove(header::AUTHORIZATION);
    }
    if matches!(route, proxy_transport::Route::Relay { .. }) && bound_route.is_some() {
        for name in [
            "x-relay-target",
            "x-relay-path",
            "x-relay-auth",
            "proxy-authorization",
        ] {
            overrides.remove(name);
        }
    }
    if provider == "iflow" {
        for name in [
            "user-agent",
            "session-id",
            "x-iflow-timestamp",
            "x-iflow-signature",
        ] {
            overrides.remove(name);
        }
    }
    if kiro
        .as_ref()
        .is_some_and(|connection| connection.is_oauth())
    {
        for name in [
            "tokentype",
            "x-amz-sso-bearer",
            "x-amzn-codewhisperer-profile-arn",
            "x-amz-target",
        ] {
            overrides.remove(name);
        }
    }
    loop {
        let direct = if fresh {
            &state.reconnect
        } else {
            &state.client
        };
        let request = |client: &reqwest::Client| {
            let mut request = client.post(&url).json(
                commandcode
                    .as_ref()
                    .or(ollama_body.as_ref())
                    .or(kiro_body.as_deref())
                    .or(attributed_body.as_ref())
                    .unwrap_or(&prepared.body),
            );
            if let Some(exchange) = codex {
                request = exchange.headers(request, account["accessToken"].as_str().unwrap_or(key));
            } else if github == GithubRoute::Messages {
                request = github_claude::headers(request, account, prepared.stream);
            } else if github == GithubRoute::Responses {
                request = github_responses::headers(request, account, prepared.stream);
            } else if provider == "github" {
                request = github_executor::headers(request, account, settings, prepared.stream);
            } else if provider == "iflow" {
                request = crate::iflow_transport::headers(request, account, prepared.stream);
            } else if provider == "gitlab" {
                request = request
                    .bearer_auth(crate::gitlab_transport::token(account).unwrap_or_default());
            } else if provider == "azure" {
                request = crate::azure_transport::headers(request, account);
            } else if let Some(connection) = &kiro {
                request = connection.headers(request, false);
            } else if let Some(commandcode) = &commandcode {
                request = commandcode_transport::headers(request, account, commandcode);
            } else if ollama_body.is_some() {
                request = crate::ollama_local::headers(
                    request,
                    account,
                    settings,
                    provider,
                    prepared.stream,
                );
            } else if crate::cline_transport::is_provider(provider) {
                request = crate::cline_transport::headers(request, account, prepared.stream);
            } else if let Some(vertex) = &vertex {
                request = vertex.headers(request);
            } else if provider == "gemini-cli" {
                request = gemini_cli_transport::headers(
                    request,
                    account["accessToken"].as_str().unwrap_or(key),
                    model,
                    prepared.stream,
                );
            } else if provider == "kilocode" {
                request = crate::provider_imports::kilocode_chat_headers(request, key, account);
            } else if let Some(endpoint) = catalog_endpoint {
                request = endpoint.auth(request, key, account, incoming, &prepared.body);
            } else if target == Transport::Anthropic {
                let client_beta = header_text(incoming, "anthropic-beta");
                request = request
                    .header("anthropic-version", "2023-06-01")
                    .header("x-api-key", key);
                if !official_anthropic {
                    request = request.bearer_auth(key);
                }
                let beta = if provider == "anthropic" {
                    Some(merge_official_anthropic_beta(client_beta))
                } else {
                    anthropic_beta(model, &prepared.body, client_beta, official_anthropic)
                };
                if let Some(beta) = beta {
                    request = request.header("anthropic-beta", beta);
                }
            } else if target == Transport::Gemini {
                request = request.header("x-goog-api-key", key);
            } else {
                request = request.bearer_auth(key);
            }
            if prepared.stream
                && vertex.is_none()
                && commandcode.is_none()
                && !crate::cline_transport::is_provider(provider)
                && !matches!(
                    provider,
                    "gemini-cli" | "github" | "ollama" | "ollama-local" | "kiro" | "iflow"
                )
            {
                request = request.header(header::ACCEPT, "text/event-stream");
            }
            route.relay(request.headers(overrides.clone()))
        };
        let result = tokio::time::timeout(timeout, async {
            if let Some((client, _)) = &kiro_proxy {
                return request(client).send().await;
            }
            if let proxy_transport::Route::Proxy { url, strict } = &route {
                let response = match state.proxy_clients.get(url, fresh, timeout) {
                    Ok(proxy) => request(&proxy).send().await,
                    Err(error) => Err(error),
                };
                if *strict || bound_route.is_some() || response.is_ok() {
                    return response;
                }
            }
            let response = request(direct).send().await?;
            if let Some(connection) = &kiro
                && matches!(route, proxy_transport::Route::Direct)
                && let Some((endpoint, alternate)) = connection.alternate(response.status())
                && let Some(settings) = state.storage.read_settings().await
                && matches!(
                    crate::provider_http::Delivery::select(
                        &state.storage,
                        &settings,
                        account,
                        &endpoint
                    )
                    .await,
                    Ok(crate::provider_http::Delivery::Direct
                        | crate::provider_http::Delivery::Bypass)
                )
            {
                return connection
                    .headers(
                        direct
                            .post(alternate)
                            .json(kiro_body.as_deref().unwrap_or(&prepared.body)),
                        true,
                    )
                    .headers(overrides.clone())
                    .send()
                    .await;
            }
            Ok(response)
        })
        .await;
        let result = if codex.is_some() {
            match result {
                Ok(Ok(response)) => match codex_executor::probe(response).await {
                    codex_executor::Probe::Response(response) => Ok(Ok(response)),
                    codex_executor::Probe::Transient {
                        message,
                        account_capacity,
                    } => {
                        if account_capacity || stream_attempts == 3 {
                            return Err(UpstreamError {
                                status: StatusCode::SERVICE_UNAVAILABLE,
                                message: codex_executor::sse_failure(&message),
                                headers: Box::default(),
                            });
                        }
                        stream_attempts += 1;
                        attempts = 0;
                        tokio::time::sleep(Duration::from_millis(2000)).await;
                        continue;
                    }
                },
                other => other,
            }
        } else if commandcode.is_some() {
            match result {
                Ok(Ok(response)) if response.status().is_success() => {
                    match commandcode_transport::probe(
                        response,
                        model,
                        account,
                        chat_output::stall_timeout(provider),
                    )
                    .await
                    {
                        commandcode_transport::Probe::Response(response) => Ok(Ok(response)),
                        commandcode_transport::Probe::Rejected { status, message } => {
                            if matches!(status.as_u16(), 502..=504) && stream_attempts < 2 {
                                stream_attempts += 1;
                                attempts = 0;
                                tokio::time::sleep(Duration::from_secs(stream_attempts)).await;
                                continue;
                            }
                            return Err(UpstreamError {
                                status,
                                message: format!("[{}]: {message}", status.as_u16()),
                                headers: Box::default(),
                            });
                        }
                    }
                }
                other => other,
            }
        } else {
            result
        };
        let status = match &result {
            Ok(Ok(response)) => response.status().as_u16(),
            _ => 502,
        };
        let (retries, delay) = match (github, status, kiro_tunnel) {
            (GithubRoute::Chat, 502, false) if vertex.is_none() => (3, 3000),
            (GithubRoute::Chat, 503, false) if vertex.is_none() => (3, 2000),
            (GithubRoute::Chat, 504, false) if vertex.is_none() => (2, 3000),
            _ => (0, 0),
        };
        if attempts >= retries {
            return match result {
                Ok(Ok(response)) if provider == "kiro" && response.status().is_success() => {
                    Ok(kiro_chat::adapt(
                        response,
                        model,
                        &prepared.body,
                        chat_output::stall_timeout(provider),
                    ))
                }
                Ok(Ok(response)) if kiro_proxy.is_some() && response.status().is_redirection() => {
                    Err(transport_failure("[502]: Kiro proxy redirect refused"))
                }
                Ok(Ok(response)) if provider == "kiro" => {
                    let mut failure = parse_failure(response).await;
                    failure.message = kiro_chat::redact_error(account, &failure.message);
                    if let Some(secret) = route.relay_secret() {
                        failure.message = failure.message.replace(secret, "[redacted]");
                    }
                    Err(failure)
                }
                Ok(Ok(response))
                    if minimax_code_provider(provider) && !response.status().is_success() =>
                {
                    Err(minimax_code_failure(response, account).await)
                }
                Ok(Ok(response)) if provider == "iflow" && !response.status().is_success() => {
                    let mut failure = parse_failure(response).await;
                    failure.message =
                        crate::iflow_transport::redact_error(account, &failure.message);
                    if let Some(secret) = route.relay_secret() {
                        failure.message = failure.message.replace(secret, "[redacted]");
                    }
                    Err(failure)
                }
                Ok(Ok(response)) if provider == "gitlab" && !response.status().is_success() => {
                    let mut failure = parse_failure(response).await;
                    failure.message =
                        crate::gitlab_transport::redact_error(account, &failure.message);
                    Err(failure)
                }
                Ok(Ok(response))
                    if (matches!(provider, "kilocode" | "glm")
                        || provider == "xai" && bound_route.is_some())
                        && !response.status().is_success()
                        && !key.is_empty() =>
                {
                    let mut failure = parse_failure(response).await;
                    failure.message = failure.message.replace(key, "[redacted]");
                    Err(failure)
                }
                Ok(Ok(response)) if provider == "kimchi" && !response.status().is_success() => {
                    let mut failure = parse_failure(response).await;
                    failure.message = crate::kimchi_chat::redact_error(account, &failure.message);
                    Err(failure)
                }
                Ok(Ok(response)) if vertex.is_some() && !response.status().is_success() => {
                    let mut failure = parse_failure(response).await;
                    if let Some(vertex) = &vertex {
                        failure.message = vertex.redact_error(&failure.message);
                    }
                    Err(failure)
                }
                Ok(Ok(response)) if commandcode.is_some() && !response.status().is_success() => {
                    let status = response.status();
                    let headers = failure_headers(&response);
                    let raw = response.text().await.unwrap_or_default();
                    let (status, message) = commandcode_transport::parse_failure(
                        status,
                        status.canonical_reason().unwrap_or_default(),
                        &raw,
                        account,
                    );
                    Err(UpstreamError {
                        status,
                        message: format!("[{}]: {message}", status.as_u16()),
                        headers: headers.into(),
                    })
                }
                Ok(Ok(response)) if ollama_body.is_some() => Ok(crate::ollama_local::adapt(
                    response,
                    model,
                    prepared.stream,
                    chat_output::stall_timeout(provider),
                )
                .await),
                Ok(Ok(response)) => Ok(response),
                Ok(Err(_))
                    if matches!(route, proxy_transport::Route::Proxy { strict: true, .. }) =>
                {
                    Err(transport_failure(
                        "[502]: [ProxyFetch] Proxy required but failed (strictProxy=true): fetch failed",
                    ))
                }
                Ok(Err(_)) => Err(transport_failure("[502]: fetch failed")),
                Err(_) => Err(transport_failure("[502]: fetch connect timeout")),
            };
        }
        drop(result);
        attempts += 1;
        if status == 502 {
            fresh = true;
        }
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

fn transport_failure(message: impl Into<String>) -> UpstreamError {
    UpstreamError {
        status: StatusCode::BAD_GATEWAY,
        message: message.into(),
        headers: Box::default(),
    }
}

pub(crate) fn anthropic_beta(
    model: &str,
    body: &Value,
    client: &str,
    official: bool,
) -> Option<String> {
    if !model.starts_with("claude-") {
        return None;
    }
    let mut seen = HashSet::new();
    let base = [
        "claude-code-20250219",
        "oauth-2025-04-20",
        "interleaved-thinking-2025-05-14",
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
        "structured-outputs-2025-12-15",
        "fast-mode-2026-02-01",
        "redact-thinking-2026-02-12",
        "token-efficient-tools-2026-03-28",
    ];
    let heavy = ["advanced-tool-use-2025-11-20", "effort-2025-11-24"];
    let wants_summaries = body["thinking"]["display"] == "summarized";
    let flags = base
        .into_iter()
        .filter(|flag| *flag != "redact-thinking-2026-02-12" || !wants_summaries)
        .chain(
            heavy
                .into_iter()
                .filter(|_| model.starts_with("claude-opus") || model.starts_with("claude-sonnet")),
        )
        .chain(
            client
                .split(',')
                .map(str::trim)
                .filter(|flag| !flag.is_empty()),
        )
        .filter(|flag| (official || *flag != "claude-code-20250219") && seen.insert(*flag))
        .collect::<Vec<_>>();
    (!flags.is_empty()).then(|| flags.join(","))
}

fn merge_official_anthropic_beta(client: &str) -> String {
    let mut seen = HashSet::new();
    ["claude-code-20250219", "interleaved-thinking-2025-05-14"]
        .into_iter()
        .chain(
            client
                .split(',')
                .map(str::trim)
                .filter(|flag| !flag.is_empty()),
        )
        .filter(|flag| seen.insert(*flag))
        .collect::<Vec<_>>()
        .join(",")
}

async fn parse_failure(response: reqwest::Response) -> UpstreamError {
    let status = response.status();
    let headers = failure_headers(&response);
    let body = response.text().await.unwrap_or_default();
    let text = if body.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        body
    };
    UpstreamError {
        status,
        message: format!("[{}]: {text}", status.as_u16()),
        headers: headers.into(),
    }
}

async fn minimax_code_failure(response: reqwest::Response, account: &Value) -> UpstreamError {
    let mut status = response.status();
    let headers = failure_headers(&response);
    let raw = response.text().await.unwrap_or_default();
    let mut message = if raw.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        raw.clone()
    };
    if let Some(detail) = minimax_code_quota(status, &raw) {
        status = StatusCode::TOO_MANY_REQUESTS;
        message = detail;
    }
    for secret in ["accessToken", "refreshToken", "apiKey"]
        .into_iter()
        .filter_map(|field| account[field].as_str().filter(|token| !token.is_empty()))
    {
        message = message.replace(secret, "[redacted]");
    }
    UpstreamError {
        status,
        message: format!("[{}]: {message}", status.as_u16()),
        headers: headers.into(),
    }
}

pub(crate) fn minimax_code_quota(status: StatusCode, raw: &str) -> Option<String> {
    static QUOTA: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            "(?i)insufficient|balance|credit|quota|exhaust|limit|余额|积分|额度|不足|用完|上限",
        )
        .expect("MiniMax Code quota pattern")
    });
    if !matches!(status, StatusCode::PAYMENT_REQUIRED | StatusCode::FORBIDDEN)
        || !QUOTA.is_match(raw)
    {
        return None;
    }
    let body: Value = serde_json::from_str(raw).unwrap_or_default();
    let detail = [
        &body["error"]["message"],
        &body["message"],
        &body["msg"],
        &body["base_resp"]["status_msg"],
    ]
    .into_iter()
    .find(|value| !value.is_null())
    .map_or_else(|| raw.trim().to_owned(), chat_output::js_text);
    Some(format!("usage limit reached: {detail}"))
}

fn failure_headers(response: &reqwest::Response) -> HeaderMap {
    response
        .headers()
        .iter()
        .filter(|(name, _)| {
            matches!(name.as_str(), "retry-after" | "x-should-retry")
                || name.as_str().starts_with("anthropic-ratelimit-")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

async fn parse_output_failure(response: Response) -> UpstreamError {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), BODY_LIMIT)
        .await
        .unwrap_or_default();
    let message = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|error| error["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
    UpstreamError {
        status,
        message,
        headers: Box::default(),
    }
}

fn error(status: StatusCode, message: &str, headers: HeaderMap) -> Response {
    let (kind, code) = match status.as_u16() {
        400 => ("invalid_request_error", "bad_request"),
        401 => ("authentication_error", "invalid_api_key"),
        402 => ("billing_error", "payment_required"),
        403 => ("permission_error", "insufficient_quota"),
        404 => ("invalid_request_error", "model_not_found"),
        406 => ("invalid_request_error", "model_not_supported"),
        410 => ("invalid_request_error", "model_retired"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        502 => ("server_error", "bad_gateway"),
        503 => ("server_error", "service_unavailable"),
        504 => ("server_error", "gateway_timeout"),
        status if status >= 500 => ("server_error", "internal_server_error"),
        _ => ("invalid_request_error", ""),
    };
    json_response(
        status,
        json!({"error": { "message": message, "type": kind, "code": code }}),
        headers,
        true,
    )
}

fn json_response(status: StatusCode, body: Value, headers: HeaderMap, cors: bool) -> Response {
    let mut values = json!({ "content-type": "application/json" });
    if cors {
        values["access-control-allow-origin"] = json!("*");
    }
    let mut response = into_http(Ok(json!({
        "status": status.as_u16(), "headers": values, "body": body.to_string(),
    })));
    response.headers_mut().extend(headers);
    response
}
