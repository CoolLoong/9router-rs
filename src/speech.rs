use std::collections::HashSet;
use std::sync::Arc;
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
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use serde_json::{Value, json};
use url::Url;

use crate::chat_accounts::{self, Selection};
use crate::chat_output::{error, js_text, json_response};
use crate::combo_chat::{self, Combo};
use crate::local_device_tts;
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::proxy_transport::{self, Route, Selection as Egress};
use crate::speech_providers;
use crate::storage::Storage;
use crate::{catalog_guard, key_policy, model_resolution};

#[path = "speech_ollama.rs"]
mod ollama;
#[path = "speech_polly.rs"]
mod polly;
#[path = "speech_web.rs"]
mod web;

const BODY_LIMIT: usize = 128 * 1024 * 1024;
const OPENAI: &str = "https://api.openai.com/v1/audio/speech";
const OPENAI_FIXTURE: &str = "http://127.0.0.1:52989/openai/v1/audio/speech";
const MIMO: &str = "https://api.xiaomimimo.com/v1/chat/completions";
const MIMO_FIXTURE: &str = "http://127.0.0.1:52989/xiaomi-mimo/v1/chat/completions";
const GEMINI: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const GEMINI_FIXTURE: &str = "http://127.0.0.1:52989/gemini/v1beta/models";
const GEMINI_MODELS: &[&str] = &[
    "gemini-3.1-flash-tts-preview",
    "gemini-2.5-flash-preview-tts",
    "gemini-2.5-pro-preview-tts",
];
const VARY: &str =
    "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch";
const PCM_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

#[derive(Clone)]
struct SpeechState {
    storage: Storage,
    client: reqwest::Client,
    protected: reqwest::Client,
    web_tokens: Arc<web::Tokens>,
}

pub fn router(storage: Storage) -> Router {
    let state = SpeechState {
        storage,
        client: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(60))
            .redirect(crate::upstream::redirects())
            .build()
            .expect("speech HTTP client"),
        protected: protected_client()
            .build()
            .expect("protected speech HTTP client"),
        web_tokens: Arc::new(web::Tokens::default()),
    };
    let mut router = Router::new();
    for path in [
        "/v1/audio/speech",
        "/api/v1/audio/speech",
        "/v1/v1/audio/speech",
    ] {
        router = router.route(path, post(speech).options(options));
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
    adapter: Option<Provider>,
}

struct SpeechInput<'a> {
    text: &'a str,
    response_format: &'a str,
    language: &'a str,
    style: &'a str,
}

fn resolve_member(db: &rusqlite::Connection, requested: &str) -> rusqlite::Result<Option<Member>> {
    Ok(
        model_resolution::get_model_info(db, requested)?.and_then(|(provider, model)| {
            let adapter = Provider::resolve(&provider);
            (!provider.is_empty()
                && (!model.is_empty() || adapter.is_some_and(Provider::accepts_empty_model)))
            .then_some(Member {
                provider,
                model,
                adapter,
            })
        }),
    )
}

async fn speech(State(state): State<SpeechState>, request: Request) -> Response {
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
    if !truthy(&body["input"]) {
        return error(StatusCode::BAD_REQUEST, "Missing required field: input");
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
    let query_format = parts.uri.query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(name, value)| (name == "response_format").then(|| value.into_owned()))
            .filter(|value| !value.is_empty())
    });
    let format = query_format
        .as_deref()
        .or_else(|| {
            body["response_format"]
                .as_str()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("mp3");
    let Some(input) = body["input"].as_str().map(js_trim) else {
        return error(StatusCode::BAD_REQUEST, "input must be a string");
    };
    if input.is_empty() {
        return error(StatusCode::BAD_REQUEST, "Missing required field: input");
    }
    let language = if truthy(&body["language"]) {
        js_text(&body["language"])
    } else {
        String::new()
    };
    let style = if truthy(&body["style"]) {
        js_text(&body["style"])
    } else {
        String::new()
    };
    let speech = SpeechInput {
        text: input,
        response_format: format,
        language: &language,
        style: &style,
    };

    match plan {
        Ok(Plan::Denied) => error(
            StatusCode::FORBIDDEN,
            &format!(
                "This API key is not allowed to use model \"{}\"",
                js_text(&body["model"])
            ),
        ),
        Ok(Plan::InvalidCombo) => error(StatusCode::BAD_REQUEST, "Invalid speech combo model list"),
        Ok(Plan::Single(member)) => speak(&state, &settings, member, &speech).await,
        Ok(Plan::Combo(combo)) => {
            let required = combo_chat::capabilities(&body);
            let catalog = state.storage.model_catalog();
            combo
                .execute(
                    &required,
                    |member| {
                        let Some(member) = member else {
                            return (None, false, None);
                        };
                        let fits = required.is_empty() || {
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
                    |member| speak(&state, &settings, member, &speech),
                    |_| {},
                )
                .await
        }
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to resolve model"),
    }
}

async fn speak(
    state: &SpeechState,
    settings: &Value,
    member: Option<Member>,
    speech: &SpeechInput<'_>,
) -> Response {
    let Some(Member {
        provider,
        model,
        adapter,
    }) = member
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid model format");
    };
    let Some(adapter) = adapter else {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("Provider '{provider}' does not support TTS via this route."),
        );
    };
    if matches!(adapter, Provider::LocalDevice) {
        return match local_device_tts::synthesize(speech.text, &model).await {
            Ok(audio) => audio_bytes(audio, "mp3", speech.response_format)
                .unwrap_or_else(|failure| error(failure.status, &failure.message)),
            Err(failure) => error(failure.status, &failure.message),
        };
    }
    if let Provider::Web(vendor) = adapter {
        return match web::synthesize(state, settings, vendor, &model, speech).await {
            Ok(response) => response,
            Err(failure) if failure.protected => protected_error(failure),
            Err(failure) => error(failure.status, &failure.message),
        };
    }
    if matches!(adapter, Provider::Standard(provider) if provider.local()) {
        return match synthesize(state, settings, adapter, &model, &Value::Null, speech).await {
            Ok(response) => response,
            Err(failure) if failure.protected => protected_error(failure),
            Err(failure) => error(failure.status, &failure.message),
        };
    }
    let mut excluded = HashSet::new();
    let mut last: Option<SynthesisError> = None;
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
                if matches!(adapter, Provider::OllamaLocal) {
                    return match ollama::synthesize(
                        state,
                        settings,
                        &Value::Null,
                        &model,
                        speech,
                        None,
                    )
                    .await
                    {
                        Ok(response) => response,
                        Err(failure) if failure.protected => protected_error(failure),
                        Err(failure) => error(failure.status, &failure.message),
                    };
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
                    "status": status.as_u16(),
                    "headers": {"content-type":"application/json"},
                    "body":json!({"error":{"message":message}}).to_string(),
                })));
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
        if matches!(adapter, Provider::XiaomiMimo)
            && crate::provider_imports::xiaomi_cloud_key(&account).is_none()
        {
            excluded.insert(connection.to_owned());
            if last.is_none() {
                last = Some(SynthesisError::upstream("xiaomi-mimo API key required"));
            }
            continue;
        }
        let response = synthesize(state, settings, adapter, &model, &account, speech).await;
        match response {
            Ok(response) => return response,
            Err(failure) if failure.protected => return protected_error(failure),
            Err(failure) => {
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
enum Provider {
    OpenAi,
    SelfHosted,
    Gemini,
    XiaomiMimo,
    OllamaLocal,
    AwsPolly,
    Standard(speech_providers::Provider),
    LocalDevice,
    Web(web::Vendor),
}

impl Provider {
    fn resolve(provider: &str) -> Option<Self> {
        match provider {
            "openai" => Some(Self::OpenAi),
            "selfhosted-tts" => Some(Self::SelfHosted),
            "gemini" => Some(Self::Gemini),
            "xiaomi-mimo" => Some(Self::XiaomiMimo),
            "ollama-local" => Some(Self::OllamaLocal),
            "aws-polly" => Some(Self::AwsPolly),
            "local-device" => Some(Self::LocalDevice),
            "edge-tts" => Some(Self::Web(web::Vendor::Bing)),
            "google-tts" => Some(Self::Web(web::Vendor::Google)),
            _ => speech_providers::Provider::resolve(provider).map(Self::Standard),
        }
    }

    fn accepts_empty_model(self) -> bool {
        matches!(
            self,
            Self::OllamaLocal
                | Self::AwsPolly
                | Self::Web(_)
                | Self::Standard(
                    speech_providers::Provider::Cartesia
                        | speech_providers::Provider::FishAudio
                        | speech_providers::Provider::Coqui
                        | speech_providers::Provider::PlayHt
                        | speech_providers::Provider::Tortoise
                )
        )
    }

    fn failure_label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::SelfHosted => "Self-hosted",
            Self::Gemini => "Gemini",
            Self::XiaomiMimo => "MiMo",
            Self::OllamaLocal => "Ollama",
            Self::AwsPolly => "AWS Polly",
            Self::Standard(_) => "Speech provider",
            Self::LocalDevice => "Local Device",
            Self::Web(_) => "Speech provider",
        }
    }
}

fn fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

fn proxy_required(settings: &Value, account: &Value, http: bool) -> bool {
    let variables = if http {
        ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    };
    truthy(&settings["outboundProxyEnabled"])
        || proxy_transport::account_requires_tunnel(account)
        || !fixture()
            && variables
                .iter()
                .any(|key| std::env::var(key).is_ok_and(|url| !url.is_empty()))
}

fn protected_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(60))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn protected_error(failure: SynthesisError) -> Response {
    let mut response = error(failure.status, &failure.message);
    response.extensions_mut().insert(combo_chat::StopFallback);
    response
}

struct SynthesisError {
    status: StatusCode,
    message: String,
    protected: bool,
}

impl SynthesisError {
    fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
            protected: false,
        }
    }

    fn proxy(message: &str) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.to_owned(),
            protected: true,
        }
    }

    fn protect(
        mut self,
        protected: bool,
        key: Option<&str>,
        relay_token: Option<&HeaderValue>,
    ) -> Self {
        if !protected {
            return self;
        }
        let relay_token = relay_token.and_then(|token| token.to_str().ok());
        for secret in key.into_iter().chain(relay_token) {
            let encoded: String = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect();
            self.message = self
                .message
                .replace(&encoded, "[redacted]")
                .replace(secret, "[redacted]");
        }
        self.protected = true;
        self
    }
}

async fn synthesize(
    state: &SpeechState,
    settings: &Value,
    provider: Provider,
    model: &str,
    account: &Value,
    speech: &SpeechInput<'_>,
) -> Result<Response, SynthesisError> {
    let SpeechInput {
        text,
        response_format,
        language,
        style,
    } = *speech;
    let key = account["apiKey"].as_str().filter(|value| !value.is_empty());
    let client = &state.client;
    match provider {
        Provider::LocalDevice | Provider::Web(_) => {
            unreachable!("no-auth speech is dispatched without credentials")
        }
        Provider::OllamaLocal => {
            ollama::synthesize(state, settings, account, model, speech, key).await
        }
        Provider::AwsPolly => polly::synthesize(state, settings, account, model, speech).await,
        Provider::Standard(adapter) => {
            let direct = adapter
                .endpoint(model, fixture())
                .map_err(SynthesisError::upstream)?;
            let routed = if adapter.supports_proxy() {
                let endpoint = adapter
                    .endpoint(model, false)
                    .map_err(SynthesisError::upstream)?;
                speech_request(
                    state,
                    settings,
                    account,
                    endpoint.as_str(),
                    direct.as_str(),
                    adapter.allows_relay(model),
                )
                .await?
            } else {
                SpeechRequest::direct(client.post(direct))
            };
            if !adapter.local() {
                routed.verify_key(key)?;
            }
            let SpeechRequest {
                request,
                protected,
                relay_token,
            } = routed;
            speech_providers::synthesize(request, adapter, text, model, key, protected)
                .await
                .map_err(SynthesisError::upstream)
                .and_then(|audio| provider_audio(audio, response_format))
                .map_err(|failure| failure.protect(protected, key, relay_token.as_ref()))
        }
        Provider::XiaomiMimo => {
            let key = crate::provider_imports::xiaomi_cloud_key(account);
            let direct = if fixture() { MIMO_FIXTURE } else { MIMO };
            let routed = speech_request(state, settings, account, MIMO, direct, true).await?;
            routed.verify_key(key)?;
            let SpeechRequest {
                request,
                protected,
                relay_token,
            } = routed;
            speech_providers::synthesize_mimo(request, text, model, key, language, style, protected)
                .await
                .map_err(SynthesisError::upstream)
                .and_then(|audio| provider_audio(audio, response_format))
                .map_err(|failure| failure.protect(protected, key, relay_token.as_ref()))
        }
        Provider::OpenAi | Provider::Gemini => {
            synthesize_synchronous(state, settings, provider, model, account, speech, key).await
        }
        Provider::SelfHosted => {
            let endpoint = crate::upstream::connection_base(account, Some("http://localhost:8880"))
                .and_then(|base| {
                    crate::upstream::node_url(
                        base,
                        "/v1/audio/speech",
                        &["/v1/audio/speech", "/v1"],
                    )
                })
                .ok_or_else(|| SynthesisError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Invalid self-hosted speech base URL".into(),
                    protected: false,
                })?;
            let endpoint = endpoint.as_str();
            let routed =
                speech_request(state, settings, account, endpoint, endpoint, false).await?;
            if key.is_some() {
                routed.verify_key(key)?;
            }
            let SpeechRequest {
                request, protected, ..
            } = routed;
            let (model, voice) = selfhosted_voice(model);
            let mut request = request.json(&json!({
                "model":model,"voice":voice,"input":text,"response_format":response_format,
            }));
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let result = match fetch_speech(provider, request, protected).await {
                Ok(response) => upstream_audio(response, response_format, response_format).await,
                Err(failure) => Err(failure),
            };
            result.map_err(|failure| failure.protect(protected, key, None))
        }
    }
}

async fn synthesize_synchronous(
    state: &SpeechState,
    settings: &Value,
    provider: Provider,
    model: &str,
    account: &Value,
    speech: &SpeechInput<'_>,
    key: Option<&str>,
) -> Result<Response, SynthesisError> {
    let gemini = matches!(provider, Provider::Gemini);
    let (model, voice, endpoint, direct_endpoint) = if gemini {
        let (model, voice) = gemini_voice(model);
        let endpoint = format!("{GEMINI}/{model}:generateContent");
        let direct = if fixture() {
            format!("{GEMINI_FIXTURE}/{model}:generateContent")
        } else {
            endpoint.clone()
        };
        (model, voice, endpoint, direct)
    } else {
        let (model, voice) = openai_voice(model);
        let direct = if fixture() { OPENAI_FIXTURE } else { OPENAI };
        (model, voice, OPENAI.to_owned(), direct.to_owned())
    };
    let routed =
        speech_request(state, settings, account, &endpoint, &direct_endpoint, true).await?;
    routed.verify_key(key)?;
    let SpeechRequest {
        mut request,
        protected,
        relay_token,
    } = routed;
    let key = key.ok_or_else(|| {
        SynthesisError::upstream(format!(
            "No {} API key configured",
            provider.failure_label()
        ))
    })?;
    if gemini {
        request = if relay_token.is_some() {
            let mut header = HeaderValue::from_str(key)
                .map_err(|_| SynthesisError::proxy("Invalid Gemini API key"))?;
            header.set_sensitive(true);
            request.header("x-goog-api-key", header)
        } else {
            request.query(&[("key", key)])
        };
        request = request.json(&json!({
            "contents":[{"parts":[{"text":gemini_prompt(speech.text, speech.language)}]}],
            "generationConfig":{
                "responseModalities":["AUDIO"],
                "speechConfig":{"voiceConfig":{"prebuiltVoiceConfig":{"voiceName":voice}}},
            },
        }));
    } else {
        request = request.bearer_auth(key).json(&json!({
            "model":model,"voice":voice,"input":speech.text,
        }));
    }
    let result = match fetch_speech(provider, request, protected).await {
        Ok(response) if gemini => {
            gemini_audio(response, speech.response_format, model, voice).await
        }
        Ok(response) => upstream_audio(response, "mp3", speech.response_format).await,
        Err(failure) => Err(failure),
    };
    result.map_err(|failure| failure.protect(protected, Some(key), relay_token.as_ref()))
}

struct SpeechRequest {
    request: reqwest::RequestBuilder,
    protected: bool,
    relay_token: Option<HeaderValue>,
}

impl SpeechRequest {
    fn direct(request: reqwest::RequestBuilder) -> Self {
        Self {
            request,
            protected: false,
            relay_token: None,
        }
    }

    fn verify_key(&self, key: Option<&str>) -> Result<(), SynthesisError> {
        if self.protected && key.is_none_or(|key| !key.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            return Err(SynthesisError::proxy(
                "Speech proxy requests require a valid saved provider API key",
            ));
        }
        Ok(())
    }
}

async fn speech_request(
    state: &SpeechState,
    settings: &Value,
    account: &Value,
    endpoint: &str,
    direct_endpoint: &str,
    allows_relay: bool,
) -> Result<SpeechRequest, SynthesisError> {
    let unavailable = || SynthesisError::proxy("Speech connection proxy is unavailable");
    let target = Url::parse(endpoint).ok();
    let http = target.as_ref().is_some_and(|url| url.scheme() == "http");
    let selection = crate::provider_http::select(&state.storage, settings, account, endpoint)
        .await
        .map_err(|_| unavailable())?;
    if matches!(selection, Egress::Routed(Route::Direct))
        && !proxy_required(settings, account, http)
    {
        return Ok(SpeechRequest::direct(state.client.post(direct_endpoint)));
    }
    let target = target
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .ok_or_else(|| SynthesisError::proxy("Invalid protected speech endpoint"))?;
    let (request, relay_token) = match &selection {
        Egress::ExplicitBypass => (state.protected.post(direct_endpoint), None),
        Egress::Routed(Route::Proxy { url, .. }) if target.scheme() == "https" => {
            let proxy = proxy_transport::tunnel_proxy_url(url).ok_or_else(unavailable)?;
            let client = proxy_transport::vendor_metadata_client(proxy, protected_client())
                .ok_or_else(unavailable)?;
            (client.post(target), None)
        }
        Egress::Routed(Route::Proxy { .. }) => {
            return Err(SynthesisError::proxy(
                "Speech tunnels require an HTTPS endpoint",
            ));
        }
        Egress::Routed(
            relay @ Route::Relay {
                url,
                authorization: Some(token),
                ..
            },
        ) if allows_relay => (relay.relay(state.protected.post(url)), Some(token.clone())),
        Egress::Routed(Route::Relay {
            authorization: Some(_),
            ..
        }) => {
            return Err(SynthesisError::proxy(
                "This speech request does not support managed Relay",
            ));
        }
        Egress::Routed(Route::Relay { .. }) => {
            return Err(SynthesisError::proxy(
                "Speech requires a validated Relay pool",
            ));
        }
        _ => return Err(unavailable()),
    };
    Ok(SpeechRequest {
        request,
        protected: true,
        relay_token,
    })
}

async fn gemini_audio(
    response: reqwest::Response,
    response_format: &str,
    model: &str,
    voice: &str,
) -> Result<Response, SynthesisError> {
    let data = response
        .json::<Value>()
        .await
        .map_err(|_| SynthesisError::upstream("Invalid JSON from Gemini TTS"))?;
    let encoded = data["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|part| {
            part["inlineData"]["data"]
                .as_str()
                .filter(|text| !text.is_empty())
        });
    let Some(encoded) = encoded else {
        let reason = [
            &data["candidates"][0]["finishReason"],
            &data["promptFeedback"]["blockReason"],
        ]
        .into_iter()
        .find(|value| truthy(value))
        .map(js_text)
        .unwrap_or_else(|| "unknown".to_owned());
        return Err(SynthesisError::upstream(format!(
            "Gemini TTS returned no audio (finishReason: {reason}, voice: {voice}, model: {model})"
        )));
    };
    let pcm = decode_audio_base64(encoded);
    audio_bytes(pcm_to_wav(pcm, 24_000)?, "wav", response_format)
}

async fn upstream_audio(
    response: reqwest::Response,
    output_format: &str,
    response_format: &str,
) -> Result<Response, SynthesisError> {
    if response_format != "json"
        && let Some(length) = response.content_length()
        && !response.headers().contains_key(header::CONTENT_ENCODING)
    {
        return binary_response(
            Body::from_stream(response.bytes_stream()),
            output_format,
            length,
        );
    }
    let audio = response
        .bytes()
        .await
        .map_err(|_| SynthesisError::upstream("terminated"))?;
    audio_bytes(audio, output_format, response_format)
}

fn provider_audio(
    audio: speech_providers::Audio,
    response_format: &str,
) -> Result<Response, SynthesisError> {
    if response_format == "json"
        && let Some(encoded) = audio.base64
    {
        return Ok(json_response(
            StatusCode::OK,
            json!({"audio":encoded,"format":audio.format}),
            None,
        ));
    }
    audio_bytes(audio.bytes, &audio.format, response_format)
}

async fn fetch_speech(
    provider: Provider,
    request: reqwest::RequestBuilder,
    protected: bool,
) -> Result<reqwest::Response, SynthesisError> {
    let response = request
        .send()
        .await
        .map_err(|_| SynthesisError::upstream("fetch failed"))?;
    let status = response.status();
    if protected && status.is_redirection() {
        return Err(SynthesisError::upstream("Speech upstream redirect refused"));
    }
    if status.is_success() {
        return Ok(response);
    }
    let message = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|value| {
            truthy(&value["error"]["message"]).then(|| js_text(&value["error"]["message"]))
        })
        .unwrap_or_else(|| {
            format!(
                "{} TTS failed: {}",
                provider.failure_label(),
                status.as_u16()
            )
        });
    Err(SynthesisError::upstream(message))
}

fn openai_voice(model: &str) -> (&str, &str) {
    const DEFAULT: &str = "gpt-4o-mini-tts";
    if let Some((model, voice)) = model.split_once('/') {
        if voice.contains('/') {
            (DEFAULT, "alloy")
        } else {
            (model, voice)
        }
    } else if model.is_empty() {
        (DEFAULT, "alloy")
    } else {
        (DEFAULT, model)
    }
}

fn selfhosted_voice(model: &str) -> (&str, String) {
    let mut parts = model.split('/').filter(|segment| !segment.is_empty());
    let model = parts.next().unwrap_or("kokoro");
    let voice = parts.collect::<Vec<_>>().join("/");
    (
        model,
        if voice.is_empty() {
            "af_heart".into()
        } else {
            voice
        },
    )
}

fn gemini_voice(model: &str) -> (&str, &str) {
    for &known in GEMINI_MODELS {
        if model == known {
            return (known, "Kore");
        }
        if let Some(voice) = model
            .strip_prefix(known)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            return (known, voice);
        }
    }
    (
        GEMINI_MODELS[0],
        if model.is_empty() { "Kore" } else { model },
    )
}

fn gemini_prompt(text: &str, language: &str) -> String {
    if text
        .match_indices(':')
        .any(|(i, _)| text[i + 1..].chars().next().is_some_and(js_whitespace))
    {
        text.to_owned()
    } else if language.is_empty() {
        format!("Say: {text}")
    } else {
        format!("Say in {language}: {text}")
    }
}

fn js_whitespace(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(js_whitespace)
}

fn decode_audio_base64(encoded: &str) -> Vec<u8> {
    let mut base64: Vec<_> = encoded
        .bytes()
        .take_while(|&byte| byte != b'=')
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
    PCM_BASE64.decode(base64).unwrap_or_default()
}

fn pcm_to_wav(mut pcm: Vec<u8>, sample_rate: u32) -> Result<Bytes, SynthesisError> {
    let length = u32::try_from(pcm.len())
        .ok()
        .filter(|len| *len <= u32::MAX - 36)
        .ok_or_else(|| SynthesisError::upstream("PCM audio too large"))?;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + length).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&length.to_le_bytes());
    wav.append(&mut pcm);
    Ok(Bytes::from(wav))
}

fn audio_bytes(
    audio: Bytes,
    format: &str,
    response_format: &str,
) -> Result<Response, SynthesisError> {
    if response_format == "json" {
        return Ok(json_response(
            StatusCode::OK,
            json!({
                "audio": STANDARD.encode(audio),
                "format": format,
            }),
            None,
        ));
    }
    let len = audio.len() as u64;
    binary_response(Body::from(audio), format, len)
}

fn binary_response(audio: Body, format: &str, length: u64) -> Result<Response, SynthesisError> {
    let format = HeaderValue::from_str(&format!("audio/{format}"))
        .map_err(|_| SynthesisError::upstream("Invalid audio content type"))?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, format)
        .header(header::CONTENT_LENGTH, length)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::VARY, VARY)
        .body(audio)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}
