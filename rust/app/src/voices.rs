use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, OnceCell};
use url::Url;

use crate::chat_accounts;
use crate::cli_video::redact_bearer;
use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::provider_index;
use crate::proxy_transport::{self, Route, Selection};
use crate::storage::Storage;

#[path = "voice_catalog.rs"]
mod voice_catalog;
use voice_catalog::nonempty;

const TTL: Duration = Duration::from_secs(24 * 60 * 60);
const PUBLIC: &[&str] = &[
    "elevenlabs",
    "deepgram",
    "inworld",
    "edge-tts",
    "local-device",
];
const EDGE: &str = "https://speech.platform.bing.com/consumer/speech/synthesize/readaloud/voices/list?trustedclienttoken=6A5AA1D4EAFF4E9FB37E23D68491D6F4";
const ELEVENLABS: &str = "https://api.elevenlabs.io/v1/voices";
pub(crate) const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";

#[derive(Clone)]
struct VoiceState {
    storage: Storage,
    client: Client,
    bypass_client: Client,
    cache: Arc<Cache>,
    fixture: bool,
}

#[derive(Default)]
struct Cache {
    edge: Mutex<HashMap<RouteKey, CachedVoices>>,
    elevenlabs: Mutex<HashMap<([u8; 32], RouteKey), CachedVoices>>,
    local: OnceCell<Vec<Value>>,
}

struct CachedVoices {
    fetched: Instant,
    voices: Vec<Value>,
}

pub fn router(storage: Storage) -> Router {
    let state = VoiceState {
        storage,
        client: Client::builder()
            .connect_timeout(Duration::from_secs(60))
            .redirect(crate::upstream::redirects())
            .no_proxy()
            .build()
            .expect("voice catalog client"),
        bypass_client: protected_client()
            .build()
            .expect("protected voice catalog client"),
        cache: Arc::new(Cache::default()),
        fixture: std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"),
    };
    let mut public = Router::new();
    for path in [
        "/v1/audio/voices",
        "/api/v1/audio/voices",
        "/v1/v1/audio/voices",
    ] {
        public = public.route(path, get(public_voices).options(options));
    }
    let mut admin = Router::new();
    for suffix in [
        "voices",
        "elevenlabs/voices",
        "deepgram/voices",
        "inworld/voices",
        "minimax/voices",
    ] {
        admin = admin.route(
            &format!("/api/media-providers/tts/{suffix}"),
            get(admin_voices),
        );
    }
    public
        .merge(admin.route_layer(middleware::from_fn_with_state(
            state.clone(),
            authorize_admin,
        )))
        .with_state(state)
}

async fn options() -> Response {
    into_http(Ok(json!({
        "status":200,"headers":{"access-control-allow-origin":"*","access-control-allow-methods":"GET, OPTIONS"},"body":"",
    })))
}

async fn authorize_admin(State(state): State<VoiceState>, req: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, req.headers()).await {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({"error":"Unauthorized"})),
    )
        .into_response()
}

fn params(request: &Request) -> HashMap<String, String> {
    let Some(query) = request.uri().query() else {
        return HashMap::new();
    };
    let mut params = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        params
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    params
}

fn query<'a>(params: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    params
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

async fn public_voices(State(state): State<VoiceState>, request: Request) -> Response {
    let params = params(&request);
    let Some(provider) = query(&params, "provider").filter(|provider| PUBLIC.contains(provider))
    else {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":format!("provider must be one of: {}", PUBLIC.join(", ")),"type":"invalid_request_error"}}),
            true,
        );
    };
    let lang = query(&params, "lang");
    let local = matches!(provider, "edge-tts" | "local-device");
    let Some(settings) = state.storage.read_settings().await else {
        return public_error(Failure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read settings",
        ));
    };
    if !local && settings["requireLogin"] != false {
        return public_error(Failure::new(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }
    let result = if local {
        generic(&state, &settings, provider, lang, None).await
    } else {
        specialized(&state, &settings, provider, lang).await
    };
    let voices = match result {
        Ok(voices) => voices,
        Err(error) => return public_error(error),
    };
    let grouped;
    let raw = if lang.is_some() {
        voices["voices"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
    } else {
        grouped = voices["byLang"]
            .as_object()
            .into_iter()
            .flat_map(|groups| groups.values())
            .filter_map(|group| group["voices"].as_array())
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        grouped.as_slice()
    };
    let alias = provider_index::find(provider)
        .and_then(|entry| nonempty(&entry["alias"]))
        .unwrap_or(provider);
    let data = raw.iter().map(|voice| {
        let id = voice["id"].as_str().unwrap_or("undefined");
        json!({"id":voice["id"],"name":voice["name"],"lang":nonempty(&voice["lang"]).unwrap_or_default(),
               "gender":nonempty(&voice["gender"]).unwrap_or_default(),"model":format!("{alias}/{id}")})
    }).collect::<Vec<_>>();
    reply(StatusCode::OK, json!({"object":"list","data":data}), true)
}

async fn admin_voices(State(state): State<VoiceState>, request: Request) -> Response {
    let params = params(&request);
    let lang = query(&params, "lang");
    let scope = request
        .uri()
        .path()
        .strip_prefix("/api/media-providers/tts/")
        .unwrap_or_default();
    let provider = if let Some(provider) = scope.strip_suffix("/voices") {
        provider
    } else {
        query(&params, "provider").unwrap_or("edge-tts")
    };
    let settings = if matches!(provider, "gemini" | "local-device") {
        Value::Null
    } else {
        let Some(settings) = state.storage.read_settings().await else {
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"Failed to read settings"}),
                false,
            );
        };
        settings
    };
    let result = match scope {
        "voices" => generic(&state, &settings, provider, lang, query(&params, "apiKey")).await,
        "minimax/voices" => {
            let name = if query(&params, "provider") == Some("minimax-cn") {
                "minimax-cn"
            } else {
                "minimax"
            };
            minimax(
                &state,
                &settings,
                name,
                lang,
                query(&params, "voice_type").unwrap_or("all"),
            )
            .await
        }
        _ => specialized(&state, &settings, provider, lang).await,
    };
    match result {
        Ok(value) => reply(StatusCode::OK, value, false),
        Err(failure) => reply(failure.status, json!({"error":failure.message}), false),
    }
}

fn reply(status: StatusCode, body: Value, cors: bool) -> Response {
    let headers = if cors {
        json!({"content-type":"application/json","access-control-allow-origin":"*"})
    } else {
        json!({"content-type":"application/json"})
    };
    into_http(Ok(
        json!({"status":status.as_u16(),"headers":headers,"body":body.to_string()}),
    ))
}

fn public_error(error: Failure) -> Response {
    reply(
        error.status,
        json!({"error":{"message":error.message,"type":"server_error"}}),
        true,
    )
}

#[derive(Debug)]
struct Failure {
    status: StatusCode,
    message: String,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }
}

async fn generic(
    state: &VoiceState,
    settings: &Value,
    provider: &str,
    lang: Option<&str>,
    key: Option<&str>,
) -> Result<Value, Failure> {
    let voices = match provider {
        "gemini" => voice_catalog::gemini().to_vec(),
        "local-device" => state
            .cache
            .local
            .get_or_try_init(|| async { crate::local_device_tts::voices().await.ok_or(()) })
            .await
            .cloned()
            .unwrap_or_default(),
        "edge-tts" => edge(state, settings).await?,
        "elevenlabs" => elevenlabs(state, settings, key, &Value::Null).await?,
        _ => {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                format!("Provider '{provider}' does not support voice listing"),
            ));
        }
    };
    Ok(voice_catalog::generic(provider, &voices, lang))
}

async fn active_account(
    state: &VoiceState,
    provider: &str,
    display: &str,
) -> Result<Value, Failure> {
    let accounts = chat_accounts::active_connections(&state.storage, provider)
        .await
        .map_err(|_| Failure::upstream("Failed to fetch voices"))?;
    accounts
        .into_iter()
        .find(|account| nonempty(&account["apiKey"]).is_some())
        .ok_or_else(|| {
            Failure::new(
                StatusCode::BAD_REQUEST,
                format!("No {display} connection found"),
            )
        })
}

async fn specialized(
    state: &VoiceState,
    settings: &Value,
    provider: &str,
    lang: Option<&str>,
) -> Result<Value, Failure> {
    let (provider_name, original, fixture_path, authorization, voice_field) = match provider {
        "elevenlabs" => {
            let account = active_account(state, provider, "ElevenLabs").await?;
            let voices =
                elevenlabs(state, settings, nonempty(&account["apiKey"]), &account).await?;
            return Ok(voice_catalog::specialized(provider, &voices, lang));
        }
        "deepgram" => (
            "Deepgram",
            "https://api.deepgram.com/v1/models",
            "/deepgram/v1/models",
            "Token",
            "tts",
        ),
        "inworld" => (
            "Inworld",
            "https://api.inworld.ai/tts/v1/voices",
            "/inworld/tts/v1/voices",
            "Basic",
            "voices",
        ),
        _ => {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "Unsupported voice provider",
            ));
        }
    };
    let account = active_account(state, provider, provider_name).await?;
    let key = nonempty(&account["apiKey"]).expect("verified voice catalog API key");
    let route = voice_route(state, settings, original, &account).await?;
    let request = route
        .request(state, Method::GET, original, fixture_path)?
        .header(header::AUTHORIZATION, format!("{authorization} {key}"));
    let upstream = route.send(request).await?;
    if !upstream.status().is_success() {
        let status = upstream.status();
        let text = upstream.text().await.unwrap_or_default();
        let reason = if text.is_empty() {
            "Failed".to_owned()
        } else {
            route.redact(&text, key)
        };
        return Err(Failure::upstream(format!(
            "{provider_name} API {}: {reason}",
            status.as_u16()
        )));
    }
    let data: Value = upstream
        .json()
        .await
        .map_err(|_| Failure::upstream("Invalid voice catalog response"))?;
    let voices = data[voice_field]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    Ok(voice_catalog::specialized(provider, voices, lang))
}

async fn edge(state: &VoiceState, settings: &Value) -> Result<Vec<Value>, Failure> {
    let account = proxy_transport::public_proxy_account(&state.storage, settings, "edge-tts")
        .await
        .map_err(|_| {
            Failure::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not select an Edge TTS proxy pool",
            )
        })?;
    let route = voice_route(state, settings, EDGE, &account).await?;
    let key = route.key();
    let mut cache = state.cache.edge.lock().await;
    cache.retain(|_, entry| entry.fetched.elapsed() < TTL);
    if let Some(entry) = cache.get(&key) {
        return Ok(entry.voices.clone());
    }
    let started = Instant::now();
    let request = route
        .request(
            state,
            Method::GET,
            EDGE,
            "/edge/consumer/voices/list?fixture=voices",
        )?
        .header(header::USER_AGENT, UA);
    let upstream = route.send(request).await?;
    if !upstream.status().is_success() {
        return Err(Failure::upstream(format!(
            "Edge TTS voices fetch failed: {}",
            upstream.status().as_u16()
        )));
    }
    let voices: Vec<Value> = upstream
        .json()
        .await
        .map_err(|_| Failure::upstream("Invalid voice catalog response"))?;
    cache.insert(
        key,
        CachedVoices {
            fetched: started,
            voices: voices.clone(),
        },
    );
    Ok(voices)
}

async fn elevenlabs(
    state: &VoiceState,
    settings: &Value,
    key: Option<&str>,
    account: &Value,
) -> Result<Vec<Value>, Failure> {
    let key = key.ok_or_else(|| Failure::upstream("ElevenLabs API key required"))?;
    let route = voice_route(state, settings, ELEVENLABS, account).await?;
    let hashed: [u8; 32] = Sha256::digest(key.as_bytes()).into();
    let cache_key = (hashed, route.key());
    {
        let mut cache = state.cache.elevenlabs.lock().await;
        cache.retain(|_, entry| entry.fetched.elapsed() < TTL);
        if let Some(entry) = cache.get(&cache_key) {
            return Ok(entry.voices.clone());
        }
    }
    let started = Instant::now();
    let request = route
        .request(state, Method::GET, ELEVENLABS, "/elevenlabs/v1/voices")?
        .header("xi-api-key", key)
        .header(header::CONTENT_TYPE, "application/json");
    let upstream = route.send(request).await?;
    if !upstream.status().is_success() {
        return Err(Failure::upstream(format!(
            "ElevenLabs voices fetch failed: {}",
            upstream.status().as_u16()
        )));
    }
    let data: Value = upstream
        .json()
        .await
        .map_err(|_| Failure::upstream("Invalid voice catalog response"))?;
    let voices = data["voices"].as_array().cloned().unwrap_or_default();
    state.cache.elevenlabs.lock().await.insert(
        cache_key,
        CachedVoices {
            fetched: started,
            voices: voices.clone(),
        },
    );
    Ok(voices)
}

async fn minimax(
    state: &VoiceState,
    settings: &Value,
    provider: &str,
    lang: Option<&str>,
    voice_type: &str,
) -> Result<Value, Failure> {
    let account = active_account(state, provider, provider).await?;
    let key = nonempty(&account["apiKey"]).expect("verified voice catalog API key");
    let (original, fixture) = if provider == "minimax-cn" {
        (
            "https://api.minimaxi.com/v1/get_voice",
            "/minimax-cn/v1/get_voice",
        )
    } else {
        (
            "https://api.minimax.io/v1/get_voice",
            "/minimax/v1/get_voice",
        )
    };
    let route = voice_route(state, settings, original, &account).await?;
    let request = route
        .request(state, Method::POST, original, fixture)?
        .bearer_auth(key)
        .json(&json!({"voice_type":voice_type}));
    let upstream = route.send(request).await?;
    let status = upstream.status();
    let raw = upstream.text().await.unwrap_or_default();
    let data: Value = serde_json::from_str(&raw).unwrap_or_default();
    let meta = data
        .get("base_resp")
        .filter(|value| !value.is_null())
        .unwrap_or(&data["baseResp"]);
    let code = meta
        .get("status_code")
        .filter(|value| !value.is_null())
        .unwrap_or(&meta["statusCode"]);
    let message = ["status_msg", "statusMsg"]
        .into_iter()
        .find_map(|field| nonempty(&meta[field]))
        .or_else(|| nonempty(&data["message"]));
    if !status.is_success() {
        let text = message.unwrap_or(if raw.is_empty() { "Failed" } else { &raw });
        return Err(Failure::upstream(format!(
            "MiniMax API {}: {}",
            status.as_u16(),
            route.redact(text, key)
        )));
    }
    let code = code
        .as_f64()
        .or_else(|| {
            code.as_str()
                .map(|text| text.trim().parse().unwrap_or(f64::NAN))
        })
        .or_else(|| code.as_bool().map(|flag| f64::from(u8::from(flag))))
        .unwrap_or(0.0);
    if code != 0.0 {
        return Err(Failure::upstream(
            route.redact(message.unwrap_or("MiniMax voice API error"), key),
        ));
    }
    Ok(voice_catalog::minimax(&data, lang))
}

fn endpoint(state: &VoiceState, original: &str, fixture: &str) -> String {
    if state.fixture {
        format!("http://127.0.0.1:52989{fixture}")
    } else {
        original.to_owned()
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum RouteKey {
    Direct,
    Bypass,
    Proxy([u8; 32]),
    Relay([u8; 32]),
}

enum VoiceRoute {
    Direct,
    Bypass,
    Proxy(Url),
    Relay { route: Route, identity: [u8; 32] },
}

impl VoiceRoute {
    fn key(&self) -> RouteKey {
        match self {
            Self::Direct => RouteKey::Direct,
            Self::Bypass => RouteKey::Bypass,
            Self::Proxy(url) => RouteKey::Proxy(Sha256::digest(url.as_str().as_bytes()).into()),
            Self::Relay { identity, .. } => RouteKey::Relay(*identity),
        }
    }

    fn request(
        &self,
        state: &VoiceState,
        method: Method,
        original: &str,
        fixture: &str,
    ) -> Result<RequestBuilder, Failure> {
        let request = match self {
            Self::Direct => state
                .client
                .request(method, endpoint(state, original, fixture)),
            Self::Bypass => state
                .bypass_client
                .request(method, endpoint(state, original, fixture)),
            Self::Proxy(proxy) => {
                proxy_transport::vendor_metadata_client(proxy.clone(), protected_client())
                    .ok_or_else(proxy_unavailable)?
                    .request(method, original)
            }
            Self::Relay { route, .. } => {
                let Route::Relay { url, .. } = route else {
                    unreachable!("voice relay contains a managed Relay route");
                };
                route.relay(state.bypass_client.request(method, url))
            }
        };
        Ok(request)
    }

    async fn send(&self, request: RequestBuilder) -> Result<reqwest::Response, Failure> {
        let upstream = request
            .send()
            .await
            .map_err(|_| Failure::upstream("fetch failed"))?;
        if !matches!(self, Self::Direct) && upstream.status().is_redirection() {
            return Err(Failure::upstream("Voice catalog upstream redirect refused"));
        }
        Ok(upstream)
    }

    fn redact(&self, message: &str, key: &str) -> String {
        let safe = redact_bearer(message);
        let safe = if !matches!(self, Self::Direct) || key.encode_utf16().count() >= 8 {
            safe.replace(key, "[redacted]")
        } else {
            safe
        };
        if let Self::Relay { route, .. } = self
            && let Some(secret) = route.relay_secret()
        {
            return safe.replace(secret, "[redacted]");
        }
        safe
    }
}

async fn voice_route(
    state: &VoiceState,
    settings: &Value,
    original: &str,
    account: &Value,
) -> Result<VoiceRoute, Failure> {
    let configured = proxy_transport::account_requires_tunnel(account)
        || truthy(&settings["outboundProxyEnabled"])
        || !state.fixture
            && ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
                .iter()
                .any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()));
    match crate::provider_http::select(&state.storage, settings, account, original)
        .await
        .map_err(|_| proxy_unavailable())?
    {
        Selection::ExplicitBypass => Ok(VoiceRoute::Bypass),
        Selection::Routed(Route::Direct) if !configured => Ok(VoiceRoute::Direct),
        Selection::Routed(Route::Proxy { url, .. }) => proxy_transport::tunnel_proxy_url(&url)
            .map(VoiceRoute::Proxy)
            .ok_or_else(proxy_unavailable),
        Selection::Routed(route @ Route::Relay { .. }) if route.relay_secret().is_some() => {
            let identity = route.relay_identity().ok_or_else(proxy_unavailable)?;
            Ok(VoiceRoute::Relay { route, identity })
        }
        _ => Err(proxy_unavailable()),
    }
}

fn protected_client() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn proxy_unavailable() -> Failure {
    Failure::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "Voice catalog proxy is unavailable",
    )
}
