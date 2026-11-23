use std::collections::HashSet;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use serde_json::{Map, Value, json};
use url::Url;

use crate::chat_accounts::{self, Selection};
use crate::chat_output::{error, json_response};
use crate::combo_chat::{self, Combo};
use crate::image_local;
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::provider_oauth;
use crate::proxy_transport::{self, Route, Selection as ProxySelection};
use crate::storage::Storage;
use crate::{catalog_guard, key_policy, model_resolution};

#[path = "cloudflare_image.rs"]
mod cloudflare_image;
#[path = "image_hosted.rs"]
mod hosted;
#[path = "image_antigravity.rs"]
mod image_antigravity;
#[path = "image_bfl.rs"]
mod image_bfl;
#[path = "image_codex.rs"]
mod image_codex;
#[path = "image_fal.rs"]
mod image_fal;
#[path = "image_nanobanana.rs"]
mod image_nanobanana;
#[path = "image_queue_egress.rs"]
mod image_queue_egress;
#[path = "image_runway.rs"]
mod image_runway;

const BODY_LIMIT: usize = 128 * 1024 * 1024;
const OPENAI: &str = "https://api.openai.com/v1/images/generations";
const OPENAI_FIXTURE: &str = "http://127.0.0.1:52989/openai/v1/images/generations";
const XAI: &str = "https://api.x.ai/v1/images/generations";
const XAI_FIXTURE: &str = "http://127.0.0.1:52989/xai/v1/images/generations";
const GEMINI: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const GEMINI_FIXTURE: &str = "http://127.0.0.1:52989/gemini/v1beta/models";
const VARY: &str =
    "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch";
const IMAGE_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);
const SENSENOVA_FIELDS: &[&str] = &[
    "model",
    "prompt",
    "n",
    "size",
    "output_format",
    "response_format",
    "watermark",
    "prompt_extend",
];

#[derive(Clone)]
struct ImageState {
    storage: Storage,
    client: reqwest::Client,
    protected: reqwest::Client,
    antigravity_sessions: crate::codex_executor::Sessions,
}

pub fn router(storage: Storage) -> Router {
    let state = ImageState {
        storage,
        client: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(60))
            .redirect(crate::upstream::redirects())
            .build()
            .expect("image generation HTTP client"),
        protected: protected_client()
            .build()
            .expect("protected image generation HTTP client"),
        antigravity_sessions: crate::codex_executor::Sessions::default(),
    };
    let mut router = Router::new();
    for path in [
        "/v1/images/generations",
        "/api/v1/images/generations",
        "/v1/v1/images/generations",
    ] {
        router = router.route(path, post(images).options(options));
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
    if let Some(key) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    {
        return Some(key);
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|key| !key.is_empty())
}

enum Plan {
    Denied,
    InvalidCombo,
    Single(Option<Member>),
    Combo(Combo<Option<Member>>),
}

struct Member {
    provider: String,
    model: String,
    adapter: Option<Adapter>,
}

#[derive(Clone, Copy)]
struct ImageOptions<'a> {
    binary: bool,
    stream: bool,
    preferred: Option<&'a str>,
    client_key: Option<&'a str>,
    endpoint: &'a str,
}

fn resolve_member(db: &rusqlite::Connection, requested: &str) -> rusqlite::Result<Option<Member>> {
    Ok(
        model_resolution::get_model_info(db, requested)?.and_then(|(provider, model)| {
            let adapter = Adapter::resolve(&provider);
            (!provider.is_empty() && !model.is_empty()).then_some(Member {
                provider,
                model,
                adapter,
            })
        }),
    )
}

async fn images(State(state): State<ImageState>, request: Request) -> Response {
    let policy_key = catalog_guard::api_key(&request);
    let (parts, raw) = request.into_parts();
    let Ok(bytes) = to_bytes(raw, BODY_LIMIT).await else {
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
        let Some(key) = inference_key(&parts.headers).filter(|key| !key.is_empty()) else {
            return error(StatusCode::UNAUTHORIZED, "Missing API key");
        };
        if !state.storage.api_key_is_active(key.to_owned()).await {
            return error(StatusCode::UNAUTHORIZED, "Invalid API key");
        }
    }
    if !truthy(&body["model"]) {
        return error(StatusCode::BAD_REQUEST, "Missing model");
    }
    if !truthy(&body["prompt"]) {
        return error(StatusCode::BAD_REQUEST, "Missing required field: prompt");
    }
    let requested = body["model"].as_str().unwrap_or_default();
    let lookup = requested.to_owned();
    let sticky = combo_chat::sticky_for(combo_chat::strategy(requested, &settings), &settings);
    let plan = state
        .storage
        .run(false, move |db| {
            if let Some(key) = policy_key.as_deref().filter(|key| !key.is_empty())
                && !key_policy::permits_chat(db, key, &lookup)?
            {
                return Ok(Plan::Denied);
            }
            if let Some(models) = model_resolution::get_combo_models(db, &lookup)? {
                let Some(models) = models.as_array() else {
                    return Ok(Plan::InvalidCombo);
                };
                let members = models
                    .iter()
                    .map(|model| resolve_member(db, model.as_str().unwrap_or_default()))
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                return Ok(Plan::Combo(Combo {
                    name: lookup,
                    members,
                    sticky,
                }));
            }
            Ok(Plan::Single(resolve_member(db, &lookup)?))
        })
        .await;
    let plan = match plan {
        Ok(Plan::Denied) => {
            let requested = crate::chat_output::js_text(&body["model"]);
            return error(
                StatusCode::FORBIDDEN,
                &format!("This API key is not allowed to use model \"{requested}\""),
            );
        }
        Ok(Plan::InvalidCombo) => {
            return error(StatusCode::BAD_REQUEST, "Invalid image combo model list");
        }
        Ok(plan) => plan,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to resolve model"),
    };
    let binary = parts
        .uri
        .query()
        .and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find_map(|(key, value)| (key == "response_format").then_some(value == "binary"))
        })
        .unwrap_or(false);
    if binary && truthy(&body["output_format"]) && !body["output_format"].is_string() {
        return error(StatusCode::BAD_REQUEST, "Invalid image output format");
    }
    let preferred = parts
        .headers
        .get("x-connection-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty());
    let options = ImageOptions {
        binary,
        preferred,
        stream: parts
            .headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream")),
        client_key: inference_key(&parts.headers).filter(|key| !key.is_empty()),
        endpoint: parts.uri.path(),
    };
    match plan {
        Plan::Single(member) => image(&state, &settings, member, &body, options).await,
        Plan::Combo(combo) => {
            let required = combo_chat::capabilities(&body);
            let catalog = state.storage.model_catalog();
            combo
                .execute(
                    &required,
                    |member| {
                        let Some(member) = member else {
                            return (None, false, None);
                        };
                        let fits = if required.is_empty() {
                            true
                        } else {
                            let capabilities = provider_capabilities_rs::port::model_capabilities(
                                Some(&member.provider),
                                &member.model,
                                catalog.as_deref(),
                            );
                            required
                                .iter()
                                .all(|capability| capabilities[*capability] == true)
                        };
                        (Some(member.provider.clone()), fits, None)
                    },
                    |member| image(&state, &settings, member, &body, options),
                    |_| {},
                )
                .await
        }
        Plan::Denied | Plan::InvalidCombo => unreachable!("rejected image plan"),
    }
}

async fn image(
    state: &ImageState,
    settings: &Value,
    member: Option<Member>,
    body: &Value,
    options: ImageOptions<'_>,
) -> Response {
    let Some(Member {
        provider,
        model,
        adapter,
    }) = member
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid model format");
    };
    let ImageOptions {
        binary, preferred, ..
    } = options;
    if matches!(adapter, Some(Adapter::VeoFree)) {
        return match crate::veoaifree_media::image(&state.storage, settings, body, binary).await {
            Ok(value) => image_response(value, body, binary).await,
            Err(failure) => protected_image_error(ImageError {
                status: failure.status,
                message: failure.message,
            }),
        };
    }
    if let Some(Adapter::Local(adapter)) = adapter {
        return match image_local::generate(&state.protected, adapter, body, binary).await {
            Ok(value) => image_response(value, body, binary).await,
            Err(failure) => error(failure.status, &failure.message),
        };
    }
    let mut excluded = HashSet::new();
    let mut last: Option<ImageError> = None;
    let mut prepared = None;
    loop {
        let selected =
            match chat_accounts::select(&state.storage, &provider, &model, &excluded, preferred)
                .await
            {
                Ok(account) => account,
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
                let mut reply = into_http(Ok(json!({
                    "status":status.as_u16(),
                    "headers":{"content-type":"application/json"},
                    "body":json!({"error":{"message":message}}).to_string(),
                })));
                if let Ok(value) = HeaderValue::from_str(&retry.retry_after_seconds.to_string()) {
                    reply.headers_mut().insert(header::RETRY_AFTER, value);
                }
                return reply;
            }
            Selection::Ready(account) => account,
        };
        let Some(adapter) = adapter else {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("Provider '{provider}' does not support image generation"),
            );
        };
        let Some(connection) = account["id"].as_str() else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid provider connection",
            );
        };
        let mut protected = false;
        let result = match adapter {
            Adapter::ChatGptWeb => {
                match crate::chatgpt_web::image(&state.storage, settings, &account, &model, body)
                    .await
                {
                    Ok(value) => {
                        protected = true;
                        Ok(value)
                    }
                    Err(failure) => {
                        return protected_image_error(ImageError {
                            status: failure.status,
                            message: failure.message,
                        });
                    }
                }
            }
            Adapter::GeminiWeb => {
                match crate::gemini_web::image(&state.storage, settings, &account, body).await {
                    Ok(value) => {
                        protected = true;
                        Ok(value)
                    }
                    Err(failure) => {
                        return protected_image_error(ImageError {
                            status: failure.status,
                            message: failure.message,
                        });
                    }
                }
            }
            Adapter::GrokWeb => {
                match crate::grok_web_media::image(&state.storage, settings, &account, body).await {
                    Ok(value) => {
                        protected = true;
                        Ok(value)
                    }
                    Err(failure) => {
                        return protected_image_error(ImageError {
                            status: failure.status,
                            message: failure.message,
                        });
                    }
                }
            }
            Adapter::MuseWeb => {
                match crate::muse_web::image(&state.storage, settings, &account, &model, body).await
                {
                    Ok(value) => {
                        protected = true;
                        Ok(value)
                    }
                    Err(failure) => {
                        return protected_image_error(ImageError {
                            status: failure.status,
                            message: failure.message,
                        });
                    }
                }
            }
            Adapter::Uc => {
                match crate::uc_media::image(&state.storage, settings, &account, &model, body).await
                {
                    Ok(value) => {
                        protected = true;
                        Ok(value)
                    }
                    Err(failure) => {
                        return protected_image_error(ImageError {
                            status: failure.status,
                            message: failure.message,
                        });
                    }
                }
            }
            Adapter::Antigravity => {
                match image_antigravity::generate(
                    &state.storage,
                    settings,
                    &account,
                    &model,
                    body,
                    &state.antigravity_sessions,
                )
                .await
                {
                    Ok(value) => {
                        protected = true;
                        Ok(value)
                    }
                    Err(failure) => {
                        let mut response = protected_image_error(ImageError {
                            status: failure.status,
                            message: failure.message,
                        });
                        response.headers_mut().extend(*failure.headers);
                        return response;
                    }
                }
            }
            Adapter::Codex => {
                match image_codex::generate(state, settings, &model, &account, body, options).await
                {
                    Ok(image_codex::ResultImage::Free) => {
                        excluded.insert(connection.to_owned());
                        last.get_or_insert_with(|| ImageError {
                            status: StatusCode::FORBIDDEN,
                            message: "No account entitled to image generation (ChatGPT Free plans are not supported)".into(),
                        });
                        continue;
                    }
                    Ok(image_codex::ResultImage::Ready(mut response)) => {
                        response.extensions_mut().insert(combo_chat::StopFallback);
                        return response;
                    }
                    Err(failure) => Err(failure),
                }
            }
            Adapter::Hosted(hosted) => {
                let request = match &prepared {
                    Some(request) => request,
                    None => match hosted.prepare(&model, body).await {
                        Ok(request) => prepared.insert(request),
                        Err(failure) => return error(failure.status, &failure.message),
                    },
                };
                match request.generate(&state.storage, settings, &account).await {
                    Ok((value, routed)) => {
                        protected = routed;
                        Ok(value)
                    }
                    Err(failure) => Err(failure),
                }
            }
            Adapter::Queued(queue) => {
                queue
                    .generate(&state.storage, settings, &model, &account, body)
                    .await
            }
            Adapter::OpenAi { .. } | Adapter::Gemini => {
                match generate_synchronous(
                    state, settings, adapter, &provider, &model, &account, body,
                )
                .await
                {
                    Ok((value, routed)) => {
                        protected = routed;
                        Ok(value)
                    }
                    Err(failure) => Err(failure),
                }
            }
            Adapter::Cloudflare => {
                match cloudflare_image::generate(
                    &state.storage,
                    settings,
                    &model,
                    &account,
                    image_key(&account),
                    body,
                )
                .await
                {
                    Ok((value, routed)) => {
                        protected = routed;
                        Ok(value)
                    }
                    Err(failure) => Err(failure),
                }
            }
            Adapter::Local(_) | Adapter::VeoFree => {
                unreachable!("accountless image adapters are dispatched separately")
            }
        };
        match result {
            Ok(value) => {
                let _ = chat_accounts::mark_success(&state.storage, connection, &model).await;
                let mut response = image_response(value, body, binary).await;
                if protected || matches!(adapter, Adapter::Queued(_)) {
                    response.extensions_mut().insert(combo_chat::StopFallback);
                }
                return response;
            }
            Err(
                QueuedImageError::PossiblyAccepted(failure) | QueuedImageError::Protected(failure),
            ) => {
                return protected_image_error(failure);
            }
            Err(QueuedImageError::BeforeSubmit(failure)) => {
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
    OpenAi {
        endpoint: &'static str,
        fields: Option<&'static [&'static str]>,
    },
    Gemini,
    Antigravity,
    GrokWeb,
    GeminiWeb,
    ChatGptWeb,
    VeoFree,
    Uc,
    MuseWeb,
    Codex,
    Cloudflare,
    Local(image_local::Adapter),
    Hosted(hosted::Adapter),
    Queued(QueueProvider),
}

#[derive(Clone, Copy)]
enum QueueProvider {
    BlackForestLabs,
    Fal,
    NanoBanana,
    Runway,
}

impl QueueProvider {
    async fn generate(
        self,
        storage: &Storage,
        settings: &Value,
        model: &str,
        account: &Value,
        body: &Value,
    ) -> Result<Value, QueuedImageError> {
        match self {
            Self::BlackForestLabs => {
                image_bfl::generate(storage, settings, model, account, body).await
            }
            Self::Fal => image_fal::generate(storage, settings, model, account, body).await,
            Self::NanoBanana => image_nanobanana::generate(storage, settings, account, body).await,
            Self::Runway => image_runway::generate(storage, settings, model, account, body).await,
        }
    }
}

impl Adapter {
    fn resolve(provider: &str) -> Option<Self> {
        let (endpoint, fields): (&'static str, Option<&'static [&'static str]>) = match provider {
            "openai" => (OPENAI, None),
            "minimax" => ("https://api.minimaxi.com/v1/images/generations", None),
            "openrouter" => ("https://openrouter.ai/api/v1/images/generations", None),
            "recraft" => (
                "https://external.api.recraft.ai/v1/images/generations",
                None,
            ),
            "vercel-ai-gateway" => ("https://ai-gateway.vercel.sh/v1/images/generations", None),
            "venice" => ("https://api.venice.ai/api/v1/images/generations", None),
            "tokenrouter" => ("https://api.tokenrouter.com/v1/images/generations", None),
            "xai" => (XAI, Some(&["model", "prompt", "n", "response_format"])),
            "sensenova" => (
                "https://token.sensenova.cn/v1/images/generations",
                Some(SENSENOVA_FIELDS),
            ),
            "sensenova-intl" => (
                "https://token.sensenova.ai/v1/images/generations",
                Some(SENSENOVA_FIELDS),
            ),
            "gemini" => return Some(Self::Gemini),
            "antigravity" => return Some(Self::Antigravity),
            "grok-web" => return Some(Self::GrokWeb),
            "gemini-web" => return Some(Self::GeminiWeb),
            "chatgpt-web" => return Some(Self::ChatGptWeb),
            "veoaifree-web" => return Some(Self::VeoFree),
            "uc-web" => return Some(Self::Uc),
            "muse-web" => return Some(Self::MuseWeb),
            "codex" => return Some(Self::Codex),
            "cloudflare-ai" => return Some(Self::Cloudflare),
            "black-forest-labs" => return Some(Self::Queued(QueueProvider::BlackForestLabs)),
            "fal-ai" => return Some(Self::Queued(QueueProvider::Fal)),
            "nanobanana" => return Some(Self::Queued(QueueProvider::NanoBanana)),
            "runwayml" => return Some(Self::Queued(QueueProvider::Runway)),
            "stability-ai" => return Some(Self::Hosted(hosted::Adapter::Stability)),
            "huggingface" => return Some(Self::Hosted(hosted::Adapter::HuggingFace)),
            _ => return image_local::Adapter::resolve(provider).map(Self::Local),
        };
        Some(Self::OpenAi { endpoint, fields })
    }

    fn url(self, model: &str, fixture: bool) -> Result<Url, url::ParseError> {
        match self {
            Self::OpenAi {
                endpoint: OPENAI, ..
            } if fixture => Url::parse(OPENAI_FIXTURE),
            Self::OpenAi { endpoint: XAI, .. } if fixture => Url::parse(XAI_FIXTURE),
            Self::OpenAi { endpoint, .. } => Url::parse(endpoint),
            Self::Gemini => {
                let base = if fixture { GEMINI_FIXTURE } else { GEMINI };
                let model = model.strip_prefix("models/").unwrap_or(model);
                Url::parse(&format!("{base}/{model}:generateContent"))
            }
            _ => unreachable!("only synchronous image adapters use this URL"),
        }
    }
}

fn proxy_required(settings: &Value, account: &Value) -> bool {
    truthy(&settings["outboundProxyEnabled"])
        || proxy_transport::account_requires_tunnel(account)
        || !fixture()
            && ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
                .iter()
                .any(|key| std::env::var(key).is_ok_and(|url| !url.is_empty()))
}

fn fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

fn protected_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(60))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn proxy_error(message: &str) -> ImageError {
    ImageError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: message.to_owned(),
    }
}

fn protected_image_error(failure: ImageError) -> Response {
    let mut response = error(failure.status, &failure.message);
    response.extensions_mut().insert(combo_chat::StopFallback);
    response
}

struct ImageError {
    status: StatusCode,
    message: String,
}

enum QueuedImageError {
    BeforeSubmit(ImageError),
    PossiblyAccepted(ImageError),
    Protected(ImageError),
}

impl From<ImageError> for QueuedImageError {
    fn from(failure: ImageError) -> Self {
        Self::BeforeSubmit(failure)
    }
}

impl ImageError {
    fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
}

fn image_key(account: &Value) -> Option<&str> {
    ["apiKey", "accessToken"]
        .into_iter()
        .find_map(|key| account[key].as_str().filter(|value| !value.is_empty()))
}

async fn generate_synchronous(
    state: &ImageState,
    settings: &Value,
    adapter: Adapter,
    provider: &str,
    model: &str,
    account: &Value,
    body: &Value,
) -> Result<(Value, bool), QueuedImageError> {
    let xai_oauth = provider == "xai" && account["authType"] == "oauth";
    let fresh;
    let settings = if xai_oauth {
        fresh = state.storage.read_settings().await.ok_or_else(|| {
            QueuedImageError::Protected(proxy_error("Image connection proxy is unavailable"))
        })?;
        &fresh
    } else {
        settings
    };
    let refreshed = if xai_oauth
        && (image_key(account).is_none()
            || provider_oauth::refresh_due_for_provider(provider, account))
    {
        provider_oauth::refresh_xai_routed(&state.storage, settings, account)
            .await
            .ok()
    } else {
        None
    };
    let account = refreshed.as_ref().unwrap_or(account);
    let latest;
    let settings = if refreshed.is_some() {
        latest = state.storage.read_settings().await.ok_or_else(|| {
            QueuedImageError::Protected(proxy_error("Image connection proxy is unavailable"))
        })?;
        &latest
    } else {
        settings
    };
    if xai_oauth && image_key(account).is_none() {
        return Err(ImageError {
            status: StatusCode::UNAUTHORIZED,
            message: "No xAI OAuth credential configured".into(),
        }
        .into());
    }
    let endpoint = adapter
        .url(model, false)
        .map_err(|_| ImageError::bad_gateway("[502]: fetch failed"))?;
    let unavailable =
        || QueuedImageError::Protected(proxy_error("Image connection proxy is unavailable"));
    let route = crate::provider_http::select(&state.storage, settings, account, endpoint.as_str())
        .await
        .map_err(|_| unavailable())?;
    let direct_endpoint = || {
        adapter
            .url(model, fixture())
            .map_err(|_| ImageError::bad_gateway("[502]: fetch failed"))
    };
    let (request, protected) = match &route {
        ProxySelection::Routed(Route::Direct) if !proxy_required(settings, account) => {
            (state.client.post(direct_endpoint()?), false)
        }
        ProxySelection::ExplicitBypass => (state.protected.post(direct_endpoint()?), true),
        ProxySelection::Routed(Route::Proxy { url, .. }) => {
            let proxy = proxy_transport::tunnel_proxy_url(url).ok_or_else(unavailable)?;
            let client = proxy_transport::vendor_metadata_client(proxy, protected_client())
                .ok_or_else(unavailable)?;
            (client.post(endpoint.clone()), true)
        }
        ProxySelection::Routed(Route::Relay {
            url,
            authorization: Some(_),
            ..
        }) => (state.protected.post(url), true),
        ProxySelection::Routed(Route::Relay { .. }) => {
            return Err(QueuedImageError::Protected(proxy_error(
                "Image generation requires a validated Relay pool",
            )));
        }
        _ => return Err(unavailable()),
    };
    let key = image_key(account);
    if protected && key.is_none_or(|key| !key.bytes().all(|byte| byte.is_ascii_graphic())) {
        return Err(QueuedImageError::Protected(proxy_error(
            "Image proxy requests require a valid saved provider API key",
        )));
    }
    let relay = match &route {
        ProxySelection::Routed(
            relay @ Route::Relay {
                authorization: Some(token),
                ..
            },
        ) => Some((relay, token)),
        _ => None,
    };
    let refresh = account["refreshToken"]
        .as_str()
        .filter(|token| !token.is_empty());
    let mut retry = None;
    let request = match adapter {
        Adapter::OpenAi { fields, .. } => {
            let mut request = request.json(&openai_body(model, body, fields));
            if xai_oauth && relay.is_none() && refresh.is_some() {
                retry = request.try_clone();
            }
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            if provider == "openrouter" {
                request = request
                    .header("HTTP-Referer", "https://endpoint-proxy.local")
                    .header("X-Title", "Endpoint Proxy");
            }
            request
        }
        Adapter::Gemini => {
            let key = key.unwrap_or("undefined");
            let model = model.strip_prefix("models/").unwrap_or(model);
            if protected
                && (model.is_empty()
                    || !model
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)))
            {
                return Err(QueuedImageError::Protected(ImageError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Invalid Gemini image model".into(),
                }));
            }
            let request = if relay.is_some() {
                let mut header = HeaderValue::from_str(key).map_err(|_| unavailable())?;
                header.set_sensitive(true);
                request.header("x-goog-api-key", header)
            } else {
                request.query(&[("key", key)])
            };
            request.json(&gemini_body(body))
        }
        _ => unreachable!("only synchronous image adapters use this transport"),
    };
    let (request, relay_token) = match relay {
        Some((route, token)) => (route.relay(request), token.to_str().ok()),
        None => (request, None),
    };
    let mut result = finish_generation(
        request,
        adapter,
        provider,
        body,
        protected || xai_oauth,
        &[key, relay_token, refresh],
    )
    .await;
    if result.as_ref().is_err_and(|failure| {
        matches!(
            failure.status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        )
    }) && let Some(retry) = retry
        && let Ok(updated) = provider_oauth::refresh_xai_inference(
            &state.storage,
            account,
            endpoint.as_str(),
            &route,
        )
        .await
        && let Some(next_key) = image_key(&updated)
        && Some(next_key) != key
        && next_key.bytes().all(|byte| byte.is_ascii_graphic())
    {
        result = finish_generation(
            retry.bearer_auth(next_key),
            adapter,
            provider,
            body,
            true,
            &[
                key,
                Some(next_key),
                refresh,
                updated["refreshToken"]
                    .as_str()
                    .filter(|token| !token.is_empty()),
            ],
        )
        .await;
    }
    result.map(|value| (value, protected)).map_err(|failure| {
        if protected {
            QueuedImageError::Protected(failure)
        } else {
            failure.into()
        }
    })
}

async fn finish_generation(
    request: reqwest::RequestBuilder,
    adapter: Adapter,
    provider: &str,
    body: &Value,
    protected: bool,
    secrets: &[Option<&str>],
) -> Result<Value, ImageError> {
    let response = request
        .send()
        .await
        .map_err(|_| ImageError::bad_gateway("[502]: fetch failed"))?;
    let status = response.status();
    if protected && status.is_redirection() {
        return Err(ImageError::bad_gateway(
            "[502]: Image upstream redirect refused",
        ));
    }
    if !status.is_success() {
        let raw = response.text().await.unwrap_or_default();
        let mut message = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|value| {
                [
                    &value["error"]["message"],
                    &value["message"],
                    &value["error"],
                ]
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
                    raw
                }
            });
        if protected || matches!(adapter, Adapter::Cloudflare) {
            for secret in secrets.iter().flatten() {
                message = message.replace(secret, "[redacted]");
            }
        }
        return Err(ImageError {
            status,
            message: format!("[{}]: {message}", status.as_u16()),
        });
    }
    if matches!(adapter, Adapter::Cloudflare) {
        return cloudflare_image::response(response).await;
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|_| ImageError::bad_gateway(format!("Invalid response from {provider}")))?;
    Ok(if matches!(adapter, Adapter::Gemini) {
        normalize_gemini(&value, &body["prompt"])
    } else {
        value
    })
}

fn openai_body(model: &str, body: &Value, fields: Option<&[&str]>) -> Value {
    let mut payload = json!({
        "model": model,
        "prompt": body["prompt"],
        "n": body.get("n").cloned().unwrap_or_else(|| json!(1)),
        "size": body.get("size").cloned().unwrap_or_else(|| json!("1024x1024")),
    });
    if let Some(format) = body.get("output_format") {
        payload["output_format"] = format.clone();
    }
    for key in ["quality", "style", "response_format"] {
        if truthy(&body[key]) {
            payload[key] = body[key].clone();
        }
    }
    if let Some(fields) = fields {
        for key in ["watermark", "prompt_extend"] {
            if fields.contains(&key)
                && let Some(value) = body.get(key)
            {
                payload[key] = value.clone();
            }
        }
        Value::Object(
            fields
                .iter()
                .filter_map(|&field| {
                    payload
                        .get_mut(field)
                        .map(|value| (field.to_owned(), value.take()))
                })
                .collect::<Map<String, Value>>(),
        )
    } else {
        payload
    }
}

fn gemini_body(body: &Value) -> Value {
    let image = if truthy(&body["image"]) {
        &body["image"]
    } else if let Some(first) = body["images"].as_array().and_then(|images| images.first()) {
        first
    } else {
        &Value::Null
    };
    let mut parts = Vec::with_capacity(2);
    if let Some(image) = image.as_str().and_then(inline_image) {
        parts.push(image);
    }
    parts.push(json!({"text":body["prompt"]}));
    json!({"contents":[{"parts":parts}],"generationConfig":{"responseModalities":["TEXT","IMAGE"]}})
}

fn inline_image(input: &str) -> Option<Value> {
    if let Some(rest) = input.strip_prefix("data:")
        && let Some((mime, payload)) = rest.split_once(";base64,")
        && let Some(kind) = mime.strip_prefix("image/")
        && !kind.is_empty()
        && !kind.contains(';')
        && !payload.is_empty()
        && !payload.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
    {
        return Some(json!({"inlineData":{"mimeType":mime,"data":payload}}));
    }
    if input
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'+' || *byte == b'/')
        && input.encode_utf16().count() > 100
        && !input.starts_with("http")
    {
        return Some(json!({"inlineData":{"mimeType":"image/png","data":input}}));
    }
    None
}

fn normalize_gemini(body: &Value, prompt: &Value) -> Value {
    let mut data: Vec<_> = body["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|part| &part["inlineData"]["data"])
        .filter(|value| truthy(value))
        .map(|value| json!({"b64_json":value}))
        .collect();
    if data.is_empty() {
        data.push(json!({"b64_json":"","revised_prompt":prompt}));
    }
    json!({"created":chrono::Utc::now().timestamp(),"data":data})
}

async fn image_response(value: Value, body: &Value, binary: bool) -> Response {
    if binary {
        let first = &value["data"][0];
        let format = body["output_format"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or("png");
        if let Some(encoded) = first["b64_json"].as_str().filter(|text| !text.is_empty()) {
            return image_bytes(decode_image(encoded), format);
        }
        if let Some(url) = first["url"].as_str().filter(|url| !url.is_empty())
            && let Some(bytes) = crate::chat_prefetch::fetch_image_bytes(url).await
        {
            return image_bytes(bytes, format);
        }
    }
    json_response(StatusCode::OK, value, None)
}

fn image_bytes(bytes: Vec<u8>, format: &str) -> Response {
    let format = format.to_lowercase();
    let (mime, extension) = match format.as_str() {
        "jpeg" => ("image/jpeg", "jpg"),
        "jpg" => ("image/jpeg", "jpg"),
        "webp" => ("image/webp", "webp"),
        _ => ("image/png", format.as_str()),
    };
    let filename = format!("inline; filename=\"image.{extension}\"");
    let Ok(disposition) = HeaderValue::from_str(&filename) else {
        return error(StatusCode::BAD_GATEWAY, "Invalid image Content-Disposition");
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::VARY, VARY)
        .body(Body::from(Bytes::from(bytes)))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn decode_image(encoded: &str) -> Vec<u8> {
    let mut base64: Vec<_> = encoded
        .bytes()
        .take_while(|byte| *byte != b'=')
        .filter_map(|byte| match byte {
            b'-' => Some(b'+'),
            b'_' => Some(b'/'),
            b'+' | b'/' => Some(byte),
            other if other.is_ascii_alphanumeric() => Some(other),
            _ => None,
        })
        .collect();
    if base64.len() % 4 == 1 {
        base64.pop();
    }
    IMAGE_BASE64.decode(base64).unwrap_or_default()
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
