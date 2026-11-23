use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::multipart::{Form, Part};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use url::Url;

use crate::chat_accounts::{self, Selection};
use crate::chat_output::{error, js_text, json_response};
use crate::gemini_live_stt;
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::proxy_transport::{self, Route, Selection as ProxySelection};
use crate::storage::Storage;
use crate::{catalog_guard, key_policy, model_resolution, provider_index};

#[path = "codex_stt.rs"]
mod codex;
#[path = "stt_extra.rs"]
mod extra;

const BODY_LIMIT: usize = 128 * 1024 * 1024;
const GEMINI: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const GEMINI_FIXTURE: &str = "http://127.0.0.1:52989/gemini/v1beta/models";
const DEEPGRAM: &str = "https://api.deepgram.com/v1/listen";
const HUGGING_FACE: &str = "https://router.huggingface.co/hf-inference/models";
const ELEVENLABS: &str = "https://api.elevenlabs.io/v1/speech-to-text";
const PROMPT: &str =
    "Generate a transcript of the speech. Return only the transcribed text, no commentary.";
const VARY: &str =
    "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch";
const VARY_ENCODING: &str = "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding";
const FORWARDED: &[&str] = &["language", "prompt", "response_format", "temperature"];
const CODEX_FIELDS: &[&str] = &[
    "model",
    "file",
    "response_format",
    "language",
    "prompt",
    "temperature",
];

#[derive(Clone)]
struct TranscriptionState {
    storage: Storage,
    client: reqwest::Client,
    protected: reqwest::Client,
}

pub fn router(storage: Storage) -> Router {
    let state = TranscriptionState {
        storage,
        client: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(60))
            .redirect(crate::upstream::redirects())
            .build()
            .expect("STT HTTP client"),
        protected: protected_client()
            .build()
            .expect("Protected STT HTTP client"),
    };
    let mut router = Router::new();
    for path in [
        "/v1/audio/transcriptions",
        "/api/v1/audio/transcriptions",
        "/v1/v1/audio/transcriptions",
    ] {
        router = router.route(path, post(transcriptions).options(options));
    }
    router.layer(DefaultBodyLimit::disable()).with_state(state)
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

struct Audio {
    filename: String,
    mime: Option<String>,
    bytes: Bytes,
}

enum Field {
    Text(String),
    Audio(Audio),
}

impl Field {
    fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            Self::Audio(_) => None,
        }
    }

    fn truthy(&self) -> bool {
        self.as_text().is_none_or(|text| !text.is_empty())
    }
}

type Fields = HashMap<String, Field>;

fn first_text<'a>(fields: &'a Fields, key: &str) -> Option<&'a str> {
    fields
        .get(key)
        .and_then(Field::as_text)
        .filter(|text| !text.is_empty())
}

fn relevant(name: &str) -> bool {
    matches!(
        name,
        "model"
            | "file"
            | "system_instruction"
            | "setup_timeout_ms"
            | "turn_timeout_ms"
            | "timestamps_granularity"
            | "tag_audio_events"
            | "num_speakers"
            | "diarize"
    ) || FORWARDED.contains(&name)
}

async fn parse_fields(request: Request, bytes: &Bytes) -> Result<(Fields, bool), ()> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    let mut result = HashMap::new();
    let mut invalid_codex = false;
    if media_type.eq_ignore_ascii_case("application/x-www-form-urlencoded") {
        for (name, value) in url::form_urlencoded::parse(bytes) {
            invalid_codex |=
                !CODEX_FIELDS.contains(&name.as_ref()) || result.contains_key(name.as_ref());
            if relevant(&name) {
                result
                    .entry(name.into_owned())
                    .or_insert_with(|| Field::Text(value.into_owned()));
            }
        }
        return Ok((result, invalid_codex));
    }
    if !media_type.eq_ignore_ascii_case("multipart/form-data") {
        return Err(());
    }

    let mut multipart = Multipart::from_request(request, &())
        .await
        .map_err(|_| ())?;
    while let Some(field) = multipart.next_field().await.map_err(|_| ())? {
        let Some(name) = field.name() else {
            invalid_codex = true;
            continue;
        };
        invalid_codex |= !CODEX_FIELDS.contains(&name) || result.contains_key(name);
        if !relevant(name) || result.contains_key(name) {
            continue;
        }
        let name = name.to_owned();
        let entry = if let Some(filename) = field.file_name() {
            let filename = filename.to_owned();
            let mime = field.content_type().map(str::to_owned);
            Field::Audio(Audio {
                filename,
                mime,
                bytes: field.bytes().await.map_err(|_| ())?,
            })
        } else {
            Field::Text(field.text().await.map_err(|_| ())?)
        };
        result.insert(name, entry);
    }
    Ok((result, invalid_codex))
}

enum Plan {
    Invalid,
    Denied,
    CodexModel,
    Unsupported(String),
    Ready(String, String, Adapter),
}

async fn transcriptions(State(state): State<TranscriptionState>, request: Request) -> Response {
    let policy_key = catalog_guard::api_key(&request);
    let (parts, incoming) = request.into_parts();
    let Ok(bytes) = to_bytes(incoming, BODY_LIMIT).await else {
        return error(StatusCode::BAD_REQUEST, "Invalid multipart form data");
    };
    let request = Request::from_parts(parts.clone(), Body::from(bytes.clone()));
    let Ok((form, invalid_codex)) = parse_fields(request, &bytes).await else {
        return error(StatusCode::BAD_REQUEST, "Invalid multipart form data");
    };

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
    if !form.get("model").is_some_and(Field::truthy) {
        return error(StatusCode::BAD_REQUEST, "Missing model");
    }
    if !form.get("file").is_some_and(Field::truthy) {
        return error(StatusCode::BAD_REQUEST, "Missing required field: file");
    }
    let Some(requested) = first_text(&form, "model") else {
        return error(StatusCode::BAD_REQUEST, "Invalid model format");
    };
    let lookup = requested.to_owned();
    let plan = state
        .storage
        .run(false, move |db| {
            let Some((provider, model)) = model_resolution::get_model_info(db, &lookup)? else {
                return Ok(Plan::Invalid);
            };
            if provider.is_empty() {
                return Ok(Plan::Invalid);
            }
            if provider == "codex" && model != "transcribe" {
                return Ok(Plan::CodexModel);
            }
            if let Some(key) = policy_key.as_ref().filter(|key| !key.is_empty())
                && !key_policy::permits_resolved(db, key, &provider, &model)?
            {
                return Ok(Plan::Denied);
            }
            let Some(mut adapter) = Adapter::resolve(&provider) else {
                return Ok(Plan::Unsupported(provider));
            };
            if adapter.format == Format::Codex {
                return Ok(Plan::Ready(provider, model, adapter));
            }
            let alias = provider_index::find(&provider)
                .and_then(|entry| entry["alias"].as_str())
                .unwrap_or(&provider);
            let custom: Option<String> = db
                .query_row(
                    "SELECT value FROM kv WHERE scope = 'customModels' AND key IN (?1, ?2)
                     ORDER BY CASE WHEN key = ?1 THEN 0 ELSE 1 END LIMIT 1",
                    [
                        format!("{provider}|{model}|stt"),
                        format!("{alias}|{model}|stt"),
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            let custom = custom
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .filter(|custom| {
                    custom["type"] == "stt"
                        && (custom["providerAlias"] == provider || custom["providerAlias"] == alias)
                        && custom["id"] == model
                });
            let marker = custom
                .as_ref()
                .and_then(|custom| custom["transport"].as_str())
                .map(js_trim)
                .filter(|marker| !marker.is_empty());
            if let Some(marker) = marker {
                adapter.format = Format::from_marker(marker);
            } else if gemini_live_stt::registry_live_model(&provider, &model) {
                adapter.format = Format::GeminiLive;
            }
            Ok(Plan::Ready(provider, model, adapter))
        })
        .await;
    let (provider, model, adapter) = match plan {
        Ok(Plan::Invalid) => return error(StatusCode::BAD_REQUEST, "Invalid model format"),
        Ok(Plan::CodexModel) => {
            return error(
                StatusCode::BAD_REQUEST,
                "Use model cx/transcribe for Codex STT",
            );
        }
        Ok(Plan::Denied) => {
            return error(
                StatusCode::FORBIDDEN,
                &format!("This API key is not allowed to use model \"{requested}\""),
            );
        }
        Ok(Plan::Unsupported(provider)) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("Provider '{provider}' does not support STT"),
            );
        }
        Ok(Plan::Ready(provider, model, adapter)) => (provider, model, adapter),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to resolve model"),
    };
    let codex_request = adapter.format == Format::Codex;
    let assembly_request = adapter.format == Format::AssemblyAi;
    let live_request = adapter.format == Format::GeminiLive;
    let file = match form.get("file") {
        Some(Field::Audio(file)) => Some(file),
        _ => None,
    };
    if !codex_request && file.is_none() {
        return error(StatusCode::BAD_REQUEST, "file must be an audio upload");
    }
    let mut excluded = HashSet::new();
    let mut last: Option<TranscriptionError> = None;
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
        if unsupported_connection(&account, adapter) {
            if proxy_required(&settings, adapter, &account) {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "This transcription connection cannot use the configured proxy",
                );
            }
            excluded.insert(connection.to_owned());
            last = Some(TranscriptionError {
                status: StatusCode::BAD_REQUEST,
                message: "Invalid saved transcription connection".into(),
            });
            continue;
        }
        let routed = if !codex_request && !assembly_request && !live_request {
            match synchronous_request(&state, &settings, adapter, &model, &account, &form).await {
                Ok(request) => Some(request),
                Err(failure) => return error(failure.status, &failure.message),
            }
        } else {
            None
        };
        let codex_route = if codex_request {
            match codex::egress(&state.storage, &settings, &account).await {
                Ok(egress) => Some(egress),
                Err(failure) => return error(failure.status, &failure.message),
            }
        } else {
            None
        };
        let live = if live_request {
            let endpoint = saved_base(&account).unwrap_or(adapter.endpoint);
            match gemini_live_stt::transport(&state.storage, &settings, &account, endpoint).await {
                Ok(transport) => Some(transport),
                Err(failure) => return error(failure.status, &failure.message),
            }
        } else {
            None
        };
        let protected_assembly = assembly_request && proxy_required(&settings, adapter, &account);
        let protected = protected_assembly
            || codex_route.as_ref().is_some_and(codex::Egress::protected)
            || routed.as_ref().is_some_and(|request| request.protected)
            || live
                .as_ref()
                .is_some_and(gemini_live_stt::Transport::is_protected);
        let response = if let Some(egress) = codex_route {
            fetch_codex(&state.storage, &account, &form, invalid_codex, egress).await
        } else if let Some(transport) = live {
            transcribe_live(
                &account,
                file.expect("validated transcription audio"),
                &model,
                &form,
                transport,
            )
            .await
        } else if assembly_request {
            let audio = file.expect("validated transcription audio");
            match provider_token(&account) {
                Some(token) if protected_assembly => {
                    extra::protected_assembly(
                        &state.storage,
                        &settings,
                        &account,
                        &state.protected,
                        adapter,
                        audio,
                        &model,
                        token,
                    )
                    .await
                }
                Some(token) => {
                    let base = saved_base(&account).unwrap_or_else(|| adapter.direct_endpoint());
                    match extra::assembly(
                        &state.client,
                        &state.protected,
                        base,
                        adapter,
                        audio,
                        &model,
                        token,
                    )
                    .await
                    {
                        Ok(response) => Ok(response),
                        Err(extra::AssemblyFailure::BeforeSubmit(failure)) => Err(failure),
                        Err(extra::AssemblyFailure::PossiblySubmitted(failure)) => {
                            return error(failure.status, &failure.message);
                        }
                    }
                }
                None => Err(TranscriptionError {
                    status: StatusCode::UNAUTHORIZED,
                    message: "No credentials for STT provider: assemblyai".into(),
                }),
            }
        } else {
            transcribe(
                adapter,
                &provider,
                &account,
                TranscriptionInput {
                    model: &model,
                    file: file.expect("validated transcription audio"),
                    form: &form,
                },
                routed.expect("synchronous transcription request"),
            )
            .await
        };
        match response {
            Ok(response) => {
                if codex_request {
                    let _ = chat_accounts::mark_success(&state.storage, connection, &model).await;
                }
                return response;
            }
            Err(failure) => {
                if protected || codex_request && matches!(failure.status.as_u16(), 400 | 413 | 504)
                {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    OpenAi,
    Gemini,
    GeminiLive,
    Deepgram,
    HuggingFace,
    ElevenLabs,
    AssemblyAi,
    Codex,
    Nvidia,
}

impl Format {
    fn from_marker(marker: &str) -> Self {
        match marker {
            "gemini-stt" => Self::Gemini,
            "gemini-live" => Self::GeminiLive,
            "deepgram" => Self::Deepgram,
            "huggingface-asr" => Self::HuggingFace,
            "elevenlabs-stt" => Self::ElevenLabs,
            "assemblyai" => Self::AssemblyAi,
            "nvidia-asr" => Self::Nvidia,
            _ => Self::OpenAi,
        }
    }
}

#[derive(Clone, Copy)]
enum Auth {
    Bearer,
    Token,
    Key,
    XiKey,
}

impl Auth {
    fn header(self, token: &str) -> Result<(header::HeaderName, HeaderValue), TranscriptionError> {
        let (field, value) = match self {
            Self::Bearer => (header::AUTHORIZATION, format!("Bearer {token}")),
            Self::Token => (header::AUTHORIZATION, format!("Token {token}")),
            Self::Key => (header::AUTHORIZATION, format!("Key {token}")),
            Self::XiKey => (
                header::HeaderName::from_static("xi-api-key"),
                token.to_owned(),
            ),
        };
        let mut value = HeaderValue::from_str(&value)
            .map_err(|_| TranscriptionError::upstream("Invalid STT credential"))?;
        value.set_sensitive(true);
        Ok((field, value))
    }
}

#[derive(Clone, Copy)]
struct Adapter {
    format: Format,
    endpoint: &'static str,
    fixture_endpoint: Option<&'static str>,
    auth: Auth,
}

impl Adapter {
    const CODEX: Self = Self {
        format: Format::Codex,
        endpoint: codex::ENDPOINT,
        fixture_endpoint: None,
        auth: Auth::Bearer,
    };

    fn resolve(provider: &str) -> Option<Self> {
        let (format, endpoint, fixture_endpoint, auth) = match provider {
            "openai" => (
                Format::OpenAi,
                "https://api.openai.com/v1/audio/transcriptions",
                None,
                Auth::Bearer,
            ),
            "groq" => (
                Format::OpenAi,
                "https://api.groq.com/openai/v1/audio/transcriptions",
                None,
                Auth::Bearer,
            ),
            "selfhosted-stt" => (
                Format::OpenAi,
                "http://localhost:8080/v1/audio/transcriptions",
                None,
                Auth::Bearer,
            ),
            "gemini" => (Format::Gemini, GEMINI, Some(GEMINI_FIXTURE), Auth::Key),
            "deepgram" => (Format::Deepgram, DEEPGRAM, None, Auth::Token),
            "huggingface" => (Format::HuggingFace, HUGGING_FACE, None, Auth::Bearer),
            "elevenlabs" => (
                Format::ElevenLabs,
                ELEVENLABS,
                Some("http://127.0.0.1:52989/elevenlabs/v1/speech-to-text"),
                Auth::XiKey,
            ),
            "assemblyai" => (
                Format::AssemblyAi,
                extra::ASSEMBLY_TRANSCRIPT,
                Some(extra::ASSEMBLY_TRANSCRIPT_FIXTURE),
                Auth::Bearer,
            ),
            "codex" => return Some(Self::CODEX),
            _ => return None,
        };
        Some(Self {
            format,
            endpoint,
            fixture_endpoint,
            auth,
        })
    }

    fn direct_endpoint(self) -> &'static str {
        if fixture() {
            self.fixture_endpoint.unwrap_or(self.endpoint)
        } else {
            self.endpoint
        }
    }

    fn elevenlabs_renderings(self) -> bool {
        self.endpoint == ELEVENLABS
    }
}

fn unsupported_connection(account: &Value, adapter: Adapter) -> bool {
    if adapter.format == Format::Codex {
        return account["authType"] != "oauth";
    }
    let base = &account["providerSpecificData"]["baseUrl"];
    truthy(base) && !base.is_string()
}

struct TranscriptionError {
    status: StatusCode,
    message: String,
}

async fn fetch_codex(
    storage: &Storage,
    observed: &Value,
    form: &Fields,
    invalid: bool,
    egress: codex::Egress,
) -> Result<Response, TranscriptionError> {
    use crate::provider_oauth::{RefreshError, RefreshMode, refresh_account};

    let changed = || TranscriptionError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "Provider connection changed during selection".into(),
    };
    let account = current_codex(storage, observed, &egress)
        .await
        .ok_or_else(changed)?;
    let updated = match refresh_account(
        storage,
        &account,
        RefreshMode::ChatRouted { rejected: false },
    )
    .await
    {
        Ok(account) => account,
        Err(RefreshError::AccountUnavailable) => return Err(changed()),
        Err(_) => account,
    };
    let latest = current_codex(storage, &updated, &egress)
        .await
        .ok_or_else(changed)?;
    let result = codex::transcribe(&latest, form, invalid, egress.clone()).await;
    if result
        .as_ref()
        .is_err_and(|error| error.status == StatusCode::UNAUTHORIZED)
        && latest["refreshToken"]
            .as_str()
            .is_some_and(|token| !token.is_empty())
        && let Some(current) = current_codex(storage, &latest, &egress).await
    {
        let updated = if current["accessToken"] != latest["accessToken"] {
            Some(current)
        } else {
            refresh_account(
                storage,
                &current,
                RefreshMode::ChatRouted { rejected: true },
            )
            .await
            .ok()
        };
        if let Some(updated) = updated
            && let Some(current) = current_codex(storage, &updated, &egress).await
            && current["accessToken"] != latest["accessToken"]
        {
            return codex::transcribe(&current, form, invalid, egress).await;
        }
    }
    result
}

async fn current_codex(
    storage: &Storage,
    account: &Value,
    expected: &codex::Egress,
) -> Option<Value> {
    let settings = storage.read_settings().await?;
    let id = account["id"].as_str()?.to_owned();
    let current = storage
        .run(false, move |db| crate::provider_connections::by_id(db, &id))
        .await
        .ok()??;
    (current["isActive"] == true
        && current["provider"] == "codex"
        && current["authType"] == "oauth"
        && matches!(
            codex::egress(storage, &settings, &current).await,
            Ok(actual) if &actual == expected
        ))
    .then_some(current)
}

impl TranscriptionError {
    fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
}

struct SynchronousRequest {
    upstream: reqwest::RequestBuilder,
    relay: Option<Route>,
    protected: bool,
}

fn protected_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn proxy_required(settings: &Value, adapter: Adapter, account: &Value) -> bool {
    if adapter.format == Format::GeminiLive {
        return gemini_live_stt::proxy_required(settings, account);
    }
    if truthy(&settings["outboundProxyEnabled"])
        || proxy_transport::account_requires_tunnel(account)
    {
        return true;
    }
    if fixture() {
        return false;
    }
    let base = if adapter.format == Format::Codex {
        codex::ENDPOINT
    } else {
        saved_base(account).unwrap_or_else(|| adapter.direct_endpoint())
    };
    let variables = if Url::parse(base).is_ok_and(|url| url.scheme() == "http") {
        ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    };
    variables
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|address| !address.is_empty()))
}

fn saved_base(account: &Value) -> Option<&str> {
    account["providerSpecificData"]["baseUrl"]
        .as_str()
        .filter(|base| !base.is_empty())
        .map(|base| base.trim_end_matches('/'))
}

fn provider_token(account: &Value) -> Option<&str> {
    ["apiKey", "accessToken"]
        .into_iter()
        .find_map(|key| account[key].as_str().filter(|value| !value.is_empty()))
}

async fn synchronous_request(
    state: &TranscriptionState,
    settings: &Value,
    adapter: Adapter,
    model: &str,
    account: &Value,
    form: &Fields,
) -> Result<SynchronousRequest, TranscriptionError> {
    let address = |real| {
        let base = saved_base(account).unwrap_or_else(|| {
            if real {
                adapter.endpoint
            } else {
                adapter.direct_endpoint()
            }
        });
        match adapter.format {
            Format::Gemini => Ok(format!("{base}/{model}:generateContent")),
            Format::Deepgram => deepgram_url(base, model, form).map(Into::into),
            Format::HuggingFace => hugging_face_url(base, model),
            _ => Ok(base.to_owned()),
        }
    };
    if !proxy_required(settings, adapter, account) {
        return Ok(SynchronousRequest {
            upstream: state.client.post(address(false)?),
            relay: None,
            protected: false,
        });
    }
    let unavailable = || TranscriptionError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "STT connection proxy is unavailable".into(),
    };
    let endpoint = Url::parse(&address(true)?)
        .ok()
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        })
        .ok_or_else(unavailable)?;
    if adapter.format == Format::Gemini
        && (model.is_empty()
            || !model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            || endpoint.query().is_some())
    {
        return Err(TranscriptionError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid Gemini batch STT endpoint".into(),
        });
    }
    if adapter.format == Format::HuggingFace
        && (!model.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        }) || endpoint.query().is_some())
    {
        return Err(TranscriptionError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid Hugging Face STT endpoint".into(),
        });
    }
    let selected =
        crate::provider_http::select(&state.storage, settings, account, endpoint.as_str())
            .await
            .map_err(|_| unavailable())?;
    let (upstream, relay) = match selected {
        ProxySelection::ExplicitBypass => (state.protected.post(address(false)?), None),
        ProxySelection::Routed(Route::Proxy { url, .. }) if endpoint.scheme() == "https" => {
            let proxy = proxy_transport::tunnel_proxy_url(&url).ok_or_else(unavailable)?;
            let client = proxy_transport::vendor_metadata_client(proxy, protected_client())
                .ok_or_else(unavailable)?;
            (client.post(endpoint), None)
        }
        ProxySelection::Routed(
            relay @ Route::Relay {
                authorization: Some(_),
                ..
            },
        ) if relay_destination(adapter, account, &endpoint) => {
            let Route::Relay { ref url, .. } = relay else {
                unreachable!()
            };
            (state.protected.post(url), Some(relay))
        }
        ProxySelection::Routed(Route::Relay { .. }) => {
            return Err(TranscriptionError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "STT requires a validated Relay pool and an eligible destination".into(),
            });
        }
        _ => return Err(unavailable()),
    };
    Ok(SynchronousRequest {
        upstream,
        relay,
        protected: true,
    })
}

fn relay_destination(adapter: Adapter, account: &Value, endpoint: &Url) -> bool {
    match adapter.format {
        Format::Deepgram | Format::HuggingFace => {
            saved_base(account).is_none_or(|base| base == adapter.endpoint)
        }
        Format::ElevenLabs => endpoint.as_str() == adapter.endpoint,
        _ => endpoint.query().is_none(),
    }
}

async fn transcribe_live(
    account: &Value,
    file: &Audio,
    model: &str,
    form: &Fields,
    transport: gemini_live_stt::Transport,
) -> Result<Response, TranscriptionError> {
    let token = provider_token(account).ok_or_else(|| TranscriptionError {
        status: StatusCode::UNAUTHORIZED,
        message: format!(
            "No credentials for STT provider: {}",
            account["provider"].as_str().unwrap_or("gemini")
        ),
    })?;
    let options = gemini_live_stt::Options::from_fields(|key| first_text(form, key));
    let value = gemini_live_stt::transcribe(
        &file.bytes,
        resolve_audio_type(file),
        model,
        token,
        options,
        transport,
    )
    .await
    .map_err(|failure| TranscriptionError {
        status: failure.status,
        message: failure.message,
    })?;
    Ok(json_response(StatusCode::OK, value, None))
}

struct TranscriptionInput<'a> {
    model: &'a str,
    file: &'a Audio,
    form: &'a Fields,
}

async fn transcribe(
    adapter: Adapter,
    provider: &str,
    account: &Value,
    input: TranscriptionInput<'_>,
    routed: SynchronousRequest,
) -> Result<Response, TranscriptionError> {
    let TranscriptionInput { model, file, form } = input;
    let Some(token) = provider_token(account) else {
        return Err(TranscriptionError {
            status: StatusCode::UNAUTHORIZED,
            message: format!("No credentials for STT provider: {provider}"),
        });
    };

    let SynchronousRequest {
        upstream,
        relay,
        protected,
    } = routed;
    if let Some(Route::Relay {
        path,
        authorization: Some(secret),
        ..
    }) = &relay
    {
        let path = percent_encoding::percent_decode_str(path).decode_utf8_lossy();
        if path.contains(token) || secret.to_str().is_ok_and(|secret| path.contains(secret)) {
            return Err(TranscriptionError {
                status: StatusCode::BAD_REQUEST,
                message: "STT Relay destination contains a credential".into(),
            });
        }
    }
    let request = if adapter.format == Format::Gemini {
        let request = if relay.is_some() {
            let mut key = HeaderValue::from_str(token)
                .map_err(|_| TranscriptionError::upstream("Invalid Gemini API key"))?;
            key.set_sensitive(true);
            upstream.header("x-goog-api-key", key)
        } else {
            upstream.query(&[("key", token)])
        };
        let mut prompt = first_text(form, "prompt")
            .map(js_trim)
            .filter(|prompt| !prompt.is_empty())
            .unwrap_or(PROMPT)
            .to_owned();
        if let Some(language) = first_text(form, "language")
            .map(js_trim)
            .filter(|value| !value.is_empty())
        {
            prompt.push_str(&format!(" Language: {language}."));
        }
        request.json(&json!({
            "contents":[{"parts":[{"text":prompt},{"inline_data":{"mime_type":resolve_audio_type(file),"data":STANDARD.encode(&file.bytes)}}]}],
        }))
    } else {
        let (field, value) = adapter.auth.header(token)?;
        let request = upstream.header(field, value);
        match adapter.format {
            Format::OpenAi => request.multipart(openai_multipart(file, model, form)?),
            Format::Nvidia => request.multipart(multipart_file(file, "model", model)?),
            Format::ElevenLabs => request.multipart(extra::elevenlabs_form(
                file,
                model,
                form,
                adapter.elevenlabs_renderings(),
            )?),
            Format::Deepgram | Format::HuggingFace => request
                .header(header::CONTENT_TYPE, resolve_audio_type(file))
                .body(file.bytes.clone()),
            Format::Codex | Format::AssemblyAi | Format::GeminiLive | Format::Gemini => {
                unreachable!("specialized transcription uses its own request transport")
            }
        }
    };
    let request = if let Some(route) = &relay {
        route.relay(request)
    } else {
        request
    };
    let response = request
        .send()
        .await
        .map_err(|_| TranscriptionError::upstream("fetch failed"))?;
    let status = response.status();
    if protected && status.is_redirection() {
        return Err(TranscriptionError::upstream(
            "STT upstream redirect refused",
        ));
    }
    if !status.is_success() {
        let mut failure = upstream_failure(response).await;
        if protected || matches!(adapter.format, Format::ElevenLabs | Format::Nvidia) {
            failure.message = failure.message.replace(token, "[redacted]");
            if let Some(Route::Relay {
                authorization: Some(secret),
                ..
            }) = &relay
                && let Ok(secret) = secret.to_str()
            {
                failure.message = failure.message.replace(secret, "[redacted]");
            }
        }
        return Err(failure);
    }
    if adapter.format == Format::OpenAi {
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .cloned()
            .filter(|value| !value.as_bytes().is_empty())
            .unwrap_or_else(|| HeaderValue::from_static("application/json"));
        let text = response
            .text()
            .await
            .map_err(|_| TranscriptionError::upstream("terminated"))?;
        return Ok(raw_text_response(text, content_type));
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|_| TranscriptionError::upstream("Invalid JSON response from STT provider"))?;
    if adapter.format == Format::ElevenLabs {
        return Ok(extra::elevenlabs_output(
            value,
            form,
            adapter.elevenlabs_renderings(),
        ));
    }
    let transcript = match adapter.format {
        Format::Gemini => json!(
            value["candidates"][0]["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|part| &part["text"])
                .filter(|text| truthy(text))
                .map(js_text)
                .collect::<String>()
        ),
        Format::Deepgram => {
            let text = &value["results"]["channels"][0]["alternatives"][0]["transcript"];
            if text.is_null() {
                json!("")
            } else {
                text.clone()
            }
        }
        Format::HuggingFace if truthy(&value["text"]) => value["text"].clone(),
        Format::Nvidia if truthy(&value["text"]) => value["text"].clone(),
        Format::Nvidia if truthy(&value["transcript"]) => value["transcript"].clone(),
        _ => json!(""),
    };
    Ok(json_response(
        StatusCode::OK,
        json!({"text": transcript}),
        None,
    ))
}

fn deepgram_url(base: &str, model: &str, form: &Fields) -> Result<Url, TranscriptionError> {
    let mut url = Url::parse(base).map_err(|_| TranscriptionError::upstream("Invalid URL"))?;
    set_query(&mut url, "model", model);
    set_query(&mut url, "smart_format", "true");
    set_query(&mut url, "punctuate", "true");
    if let Some(language) = first_text(form, "language")
        .map(js_trim)
        .filter(|value| !value.is_empty())
    {
        set_query(&mut url, "language", language);
    } else {
        set_query(&mut url, "detect_language", "true");
    }
    Ok(url)
}

fn hugging_face_url(base: &str, model: &str) -> Result<String, TranscriptionError> {
    if model.contains("..") || model.contains("//") {
        return Err(TranscriptionError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid model ID".into(),
        });
    }
    Ok(format!("{}/{model}", base.trim_end_matches('/')))
}

fn fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

fn multipart_file(
    file: &Audio,
    key: &'static str,
    model: &str,
) -> Result<Form, TranscriptionError> {
    Ok(Form::new()
        .part("file", audio_part(file)?)
        .text(key, model.to_owned()))
}

fn openai_multipart(
    file: &Audio,
    model: &str,
    fields: &Fields,
) -> Result<Form, TranscriptionError> {
    let mut body = multipart_file(file, "model", model)?;
    for &key in FORWARDED {
        body = match fields.get(key) {
            Some(Field::Text(value)) if !value.is_empty() => body.text(key, value.clone()),
            Some(Field::Audio(file)) => body.part(key, audio_part(file)?),
            _ => body,
        };
    }
    Ok(body)
}

fn audio_part(file: &Audio) -> Result<Part, TranscriptionError> {
    Part::stream_with_length(
        reqwest::Body::from(file.bytes.clone()),
        file.bytes.len() as u64,
    )
    .file_name(if file.filename.is_empty() {
        "audio.wav".into()
    } else {
        file.filename.clone()
    })
    .mime_str(
        file.mime
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("application/octet-stream"),
    )
    .map_err(|_| TranscriptionError::upstream("Invalid audio MIME type"))
}

async fn upstream_failure(response: reqwest::Response) -> TranscriptionError {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    upstream_error(status, &text)
}

fn upstream_error(status: StatusCode, text: &str) -> TranscriptionError {
    let message = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| {
            [
                &value["error"]["message"],
                &value["error"],
                &value["message"],
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
            if text.is_empty() {
                format!("Upstream error ({})", status.as_u16())
            } else {
                text.to_owned()
            }
        });
    TranscriptionError { status, message }
}

fn raw_text_response(text: String, content_type: HeaderValue) -> Response {
    let media_type = content_type
        .to_str()
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let compressible = media_type.starts_with("text/")
        || media_type.ends_with("+json")
        || media_type.ends_with("+xml")
        || matches!(
            media_type.as_str(),
            "application/json" | "application/xml" | "application/javascript"
        );
    let text = if let Some(rest) = text.strip_prefix('\u{feff}') {
        rest.to_owned()
    } else {
        text
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(
            header::VARY,
            if compressible { VARY_ENCODING } else { VARY },
        )
        .body(Body::from(text))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn resolve_audio_type(file: &Audio) -> String {
    let mime = file.mime.as_deref().unwrap_or_default().to_lowercase();
    if mime.starts_with("audio/") {
        return mime;
    }
    let extension = file
        .filename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "mp3" => "audio/mpeg",
        "mp4" | "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "webm" => "audio/webm",
        "aac" => "audio/aac",
        "opus" => "audio/opus",
        _ => "application/octet-stream",
    }
    .to_owned()
}

fn set_query(url: &mut Url, key: &str, value: &str) {
    let mut found = false;
    let mut pairs = Vec::new();
    for (name, old) in url.query_pairs() {
        if name == key {
            if !found {
                pairs.push((name.into_owned(), value.to_owned()));
                found = true;
            }
        } else {
            pairs.push((name.into_owned(), old.into_owned()));
        }
    }
    if !found {
        pairs.push((key.to_owned(), value.to_owned()));
    }
    url.query_pairs_mut().clear().extend_pairs(pairs);
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}
