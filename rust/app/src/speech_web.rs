use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::http::{StatusCode, header};
use axum::response::Response;
use regex::Regex;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::{
    SpeechInput, SpeechState, SynthesisError, audio_bytes, decode_audio_base64, fixture,
    json_response, protected_client, proxy_required,
};
use crate::proxy_transport::{self, Route, Selection};
use crate::voices::UA;

const BING: &str = "https://www.bing.com";
const BING_TRANSLATOR: &str = "https://www.bing.com/translator";
const BING_SPEECH: &str = "/tfettts?isVertical=1&&IG=1&IID=translator.5023&SFX=1";
const GOOGLE: &str = "https://translate.google.com";
const GOOGLE_TRANSLATOR: &str = "https://translate.google.com/";
const GOOGLE_SPEECH: &str = "/_/TranslateWebserverUi/data/batchexecute";
const GOOGLE_RPC: &str = "jQ1olc";

static BING_TOKEN: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"params_AbusePreventionHelper\s*=\s*\[([^,]+),([^,]+),"));
static GOOGLE_REQUESTS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
pub(super) enum Vendor {
    Bing,
    Google,
}

impl Vendor {
    fn origin(self) -> &'static str {
        match self {
            Self::Bing => BING,
            Self::Google => GOOGLE,
        }
    }

    fn fixture_prefix(self) -> &'static str {
        match self {
            Self::Bing => "/speech-web/bing",
            Self::Google => "/speech-web/google",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Direct,
    Bypass,
    Tunnel([u8; 32]),
}

struct WebRoute {
    client: Client,
    lane: Lane,
    vendor: Vendor,
}

impl WebRoute {
    async fn select(
        state: &SpeechState,
        settings: &Value,
        vendor: Vendor,
    ) -> Result<Self, SynthesisError> {
        let unavailable = || SynthesisError::proxy("Speech connection proxy is unavailable");
        let provider = match vendor {
            Vendor::Bing => "edge-tts",
            Vendor::Google => "google-tts",
        };
        let account = &proxy_transport::public_proxy_account(&state.storage, settings, provider)
            .await
            .map_err(|_| unavailable())?;
        let (client, lane) = match proxy_transport::select_with_bypass(
            &state.storage,
            settings,
            account,
            vendor.origin(),
        )
        .await
        .map_err(|_| unavailable())?
        {
            Selection::Routed(Route::Direct) if !proxy_required(settings, account, false) => {
                (state.client.clone(), Lane::Direct)
            }
            Selection::ExplicitBypass => (state.protected.clone(), Lane::Bypass),
            Selection::Routed(Route::Proxy { url, .. }) => {
                let proxy = proxy_transport::tunnel_proxy_url(&url).ok_or_else(unavailable)?;
                let lane = Lane::Tunnel(Sha256::digest(proxy.as_str().as_bytes()).into());
                let client = proxy_transport::vendor_metadata_client(proxy, protected_client())
                    .ok_or_else(unavailable)?;
                (client, lane)
            }
            _ => return Err(unavailable()),
        };
        Ok(Self {
            client,
            lane,
            vendor,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let endpoint = if fixture() && !matches!(self.lane, Lane::Tunnel(_)) {
            format!(
                "http://127.0.0.1:52989{}{path}",
                self.vendor.fixture_prefix()
            )
        } else {
            format!("{}{path}", self.vendor.origin())
        };
        self.client.request(method, endpoint)
    }

    async fn send(&self, request: RequestBuilder) -> Result<reqwest::Response, SynthesisError> {
        let response = request
            .send()
            .await
            .map_err(|_| SynthesisError::upstream("fetch failed"))?;
        if self.lane != Lane::Direct && response.status().is_redirection() {
            return Err(SynthesisError::upstream("Speech upstream redirect refused"));
        }
        Ok(response)
    }
}

#[derive(Default)]
pub(super) struct Tokens {
    bing: Mutex<Option<CachedToken<BingToken>>>,
    google: Mutex<Option<CachedToken<GoogleToken>>>,
}

struct CachedToken<T> {
    lane: Lane,
    fetched: Instant,
    value: Arc<T>,
}

async fn get_token<T>(
    cache: &Mutex<Option<CachedToken<T>>>,
    lane: Lane,
    ttl: Duration,
    rejected: Option<&Arc<T>>,
    fetch: impl Future<Output = Result<T, SynthesisError>>,
) -> Result<Arc<T>, SynthesisError> {
    let mut cache = cache.lock().await;
    if let Some(token) = cache.as_ref().filter(|token| token.lane == lane) {
        if token.fetched.elapsed() < ttl
            && rejected.is_none_or(|rejected| !Arc::ptr_eq(rejected, &token.value))
        {
            return Ok(token.value.clone());
        }
        *cache = None;
    }
    let fetched = Instant::now();
    let value = Arc::new(fetch.await?);
    *cache = Some(CachedToken {
        lane,
        fetched,
        value: value.clone(),
    });
    Ok(value)
}

struct BingToken {
    key: String,
    token: String,
    cookie: Option<header::HeaderValue>,
}

struct GoogleToken {
    session: String,
    build: String,
}

pub(super) async fn synthesize(
    state: &SpeechState,
    settings: &Value,
    vendor: Vendor,
    model: &str,
    speech: &SpeechInput<'_>,
) -> Result<Response, SynthesisError> {
    let route = WebRoute::select(state, settings, vendor).await?;
    let result = match vendor {
        Vendor::Bing => bing(&route, &state.web_tokens, model, speech).await,
        Vendor::Google => google(&route, &state.web_tokens, model, speech).await,
    };
    result.map_err(|mut failure| {
        failure.protected = route.lane != Lane::Direct;
        failure
    })
}

async fn bing_token(route: &WebRoute) -> Result<BingToken, SynthesisError> {
    let response = route
        .send(
            route
                .request(Method::GET, "/translator")
                .header(header::USER_AGENT, UA)
                .header(header::ACCEPT_LANGUAGE, "vi,en-US;q=0.9,en;q=0.8"),
        )
        .await?;
    if !response.status().is_success() {
        return Err(SynthesisError::upstream(format!(
            "Bing translator fetch failed: {}",
            response.status().as_u16()
        )));
    }
    let cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|cookie| cookie.to_str().ok()?.split(';').next())
        .collect::<Vec<_>>()
        .join("; ");
    let cookie = (!cookies.is_empty())
        .then(|| header::HeaderValue::from_str(&cookies).ok())
        .flatten()
        .map(|mut value| {
            value.set_sensitive(true);
            value
        });
    let html = response_text(response).await?;
    let captures = BING_TOKEN
        .as_ref()
        .ok()
        .and_then(|pattern| pattern.captures(&html))
        .ok_or_else(|| SynthesisError::upstream("Failed to parse Bing token"))?;
    let (_, [key, token]) = captures.extract();
    Ok(BingToken {
        key: key.to_owned(),
        token: token.replace('"', ""),
        cookie,
    })
}

async fn bing(
    route: &WebRoute,
    cache: &Tokens,
    model: &str,
    speech: &SpeechInput<'_>,
) -> Result<Response, SynthesisError> {
    let voice = if model.is_empty() {
        "vi-VN-HoaiMyNeural"
    } else {
        model
    };
    let lang = voice.split('-').take(2).collect::<Vec<_>>().join("-");
    let gender = if voice.to_lowercase().contains("male") {
        "Male"
    } else {
        "Female"
    };
    let ssml = format!(
        "<speak version='1.0' xml:lang='{lang}'><voice xml:lang='{lang}' xml:gender='{gender}' name='{voice}'><prosody rate='0.00%'>{}</prosody></voice></speak>",
        speech.text
    );
    let ttl = Duration::from_secs(5 * 60);
    let mut token = get_token(&cache.bing, route.lane, ttl, None, bing_token(route)).await?;
    let mut response = bing_request(route, &ssml, &token).await?;
    if matches!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS | StatusCode::FORBIDDEN
    ) {
        token = get_token(
            &cache.bing,
            route.lane,
            ttl,
            Some(&token),
            bing_token(route),
        )
        .await?;
        response = bing_request(route, &ssml, &token).await?;
    }
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let message = if text.is_empty() {
            format!("Bing TTS failed: {status}")
        } else {
            format!("Bing TTS failed: {status} - {text}")
        };
        let mut failure = SynthesisError::upstream(message);
        let cookies = token
            .cookie
            .as_ref()
            .and_then(|cookie| cookie.to_str().ok());
        let secrets = [token.key.as_str(), token.token.as_str()]
            .into_iter()
            .chain(cookies.into_iter().flat_map(|cookie| {
                cookie
                    .split("; ")
                    .filter_map(|part| part.split_once('=').map(|(_, value)| value))
            }));
        for secret in secrets.filter(|secret| !secret.is_empty()) {
            failure = failure.protect(true, Some(secret), None);
        }
        return Err(failure);
    }
    let audio = response
        .bytes()
        .await
        .map_err(|_| SynthesisError::upstream("terminated"))?;
    if audio.len() < 1024 {
        return Err(SynthesisError::upstream("Bing TTS returned empty audio"));
    }
    audio_bytes(audio, "mp3", speech.response_format)
}

async fn bing_request(
    route: &WebRoute,
    ssml: &str,
    token: &BingToken,
) -> Result<reqwest::Response, SynthesisError> {
    let mut request = route
        .request(Method::POST, BING_SPEECH)
        .header(header::ACCEPT, "*/*")
        .header(header::ORIGIN, BING)
        .header(header::REFERER, BING_TRANSLATOR)
        .header(header::USER_AGENT, UA)
        .form(&[("ssml", ssml), ("token", &token.token), ("key", &token.key)]);
    if let Some(cookie) = &token.cookie {
        request = request.header(header::COOKIE, cookie.clone());
    }
    route.send(request).await
}

async fn google_token(route: &WebRoute) -> Result<GoogleToken, SynthesisError> {
    let response = route
        .send(
            route
                .request(Method::GET, "/")
                .header(header::USER_AGENT, UA),
        )
        .await?;
    if !response.status().is_success() {
        return Err(SynthesisError::upstream(format!(
            "Google translate fetch failed: {}",
            response.status().as_u16()
        )));
    }
    let html = response_text(response).await?;
    let field = |delimiter| {
        html.split_once(delimiter)?
            .1
            .split_once('"')
            .map(|(value, _)| value)
            .filter(|value| {
                !value.is_empty() && !value.contains(['\r', '\n', '\u{2028}', '\u{2029}'])
            })
            .map(str::to_owned)
    };
    match (field("\"FdrFJe\":\""), field("\"cfb2h\":\"")) {
        (Some(session), Some(build)) => Ok(GoogleToken { session, build }),
        _ => Err(SynthesisError::upstream("Failed to parse Google token")),
    }
}

async fn google(
    route: &WebRoute,
    cache: &Tokens,
    model: &str,
    speech: &SpeechInput<'_>,
) -> Result<Response, SynthesisError> {
    let lang = if model.is_empty() { "en" } else { model };
    let token = get_token(
        &cache.google,
        route.lane,
        Duration::from_secs(11 * 60),
        None,
        google_token(route),
    )
    .await?;
    let cleaned = speech
        .text
        .replace(
            [
                '@', '^', '*', '(', ')', '\\', '/', '-', '_', '+', '=', '>', '<', '"', '\'', '“',
                '”', '【', '】',
            ],
            " ",
        )
        .replace(", ", ". ");
    let sequence = GOOGLE_REQUESTS.fetch_add(1, Ordering::Relaxed) + 1;
    let mut bytes = [0; 4];
    let _ = getrandom::fill(&mut bytes);
    let reqid =
        (sequence * 100_000 + 1_000 + u64::from(u32::from_le_bytes(bytes) % 9_000)).to_string();
    let query = [
        ("rpcids", GOOGLE_RPC),
        ("f.sid", &token.session),
        ("bl", &token.build),
        ("hl", lang),
        ("soc-app", "1"),
        ("soc-platform", "1"),
        ("soc-device", "1"),
        ("_reqid", &reqid),
        ("rt", "c"),
    ];
    let payload = json!([cleaned, lang, null, "undefined", [0]]).to_string();
    let rpc = json!([[[GOOGLE_RPC, payload, null, "generic"]]]).to_string();
    let response = route
        .send(
            route
                .request(Method::POST, GOOGLE_SPEECH)
                .header(header::REFERER, GOOGLE_TRANSLATOR)
                .query(&query)
                .form(&[("f.req", rpc)]),
        )
        .await?;
    if !response.status().is_success() {
        return Err(SynthesisError::upstream(format!(
            "Google TTS failed: {}",
            response.status().as_u16()
        )));
    }
    let text = response_text(response).await?;
    let invalid = || SynthesisError::upstream("Invalid Google TTS response");
    let envelope: Value = serde_json::from_str(text.split('\n').nth(3).ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    let data: Value = serde_json::from_str(envelope[0][2].as_str().ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    let encoded = data[0]
        .as_str()
        .filter(|value| value.encode_utf16().count() >= 100)
        .ok_or_else(|| SynthesisError::upstream("Google TTS returned empty audio"))?;
    if speech.response_format == "json" {
        return Ok(json_response(
            StatusCode::OK,
            json!({"audio":encoded,"format":"mp3"}),
            None,
        ));
    }
    audio_bytes(
        Bytes::from(decode_audio_base64(encoded)),
        "mp3",
        speech.response_format,
    )
}

async fn response_text(response: reqwest::Response) -> Result<String, SynthesisError> {
    response
        .text()
        .await
        .map_err(|_| SynthesisError::upstream("terminated"))
}
