use std::future::poll_fn;
use std::sync::OnceLock;
use std::time::Duration;

use axum::http::{HeaderValue, StatusCode, Uri};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use hyper_util::client::legacy::connect::{
    HttpConnector,
    proxy::{SocksV5, Tunnel},
};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::{
    CloseFrame, WebSocketConfig, frame::coding::CloseCode,
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config,
    connect_async_with_config,
};
use tower_service::Service;
use url::Url;

use crate::chat_accounts::date_ms;
use crate::chat_input::truthy;
use crate::chat_output::js_text;
use crate::cli_video::{component, redact_bearer};
use crate::provider_index::js_trim;
use crate::proxy_transport::{self, Route, Selection as ProxySelection};
use crate::storage::Storage;

const SETUP_MS: u64 = 10_000;
const TURN_MS: u64 = 60_000;
const MAX_MS: f64 = 300_000.0;
const CHUNK_BYTES: usize = 16_384;
const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_INSTRUCTION: &str = "Transcribe the spoken audio verbatim.";
static CONFIG: OnceLock<Value> = OnceLock::new();

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn config() -> &'static Value {
    CONFIG.get_or_init(|| {
        serde_json::from_str(include_str!("gemini-live-stt-config.json"))
            .expect("pinned Gemini STT metadata")
    })
}

fn base_url() -> &'static str {
    config()["baseUrl"].as_str().expect("Gemini STT URL")
}

pub fn registry_live_model(provider: &str, model: &str) -> bool {
    provider == "gemini"
        && config()["models"]
            .as_array()
            .is_some_and(|models| models.iter().any(|entry| entry == model))
}

pub struct Transport {
    endpoint: Url,
    proxy: Option<Proxy>,
    protected: bool,
}

impl Transport {
    pub fn is_protected(&self) -> bool {
        self.protected
    }

    fn websocket_url(&self, model: &str, token: &str) -> Option<Url> {
        let fixture = self.proxy.is_none() && fixture() && self.endpoint.as_str() == base_url();
        let mut url = if fixture {
            Url::parse("ws://127.0.0.1:52988").ok()?
        } else {
            let mut url = self.endpoint.clone();
            url.set_scheme("wss").ok()?;
            url
        };
        let path = self.endpoint.path().trim_end_matches('/');
        let prefix = if path.starts_with("/ws/") {
            ""
        } else {
            "/ws/api"
        };
        url.set_path(&format!(
            "{prefix}{path}/{}:bidiGenerateContent",
            component(model)
        ));
        url.set_query(Some(&format!("key={}", component(token))));
        Some(url)
    }

    async fn connect(&self, url: &Url, config: WebSocketConfig) -> Result<Socket, LiveError> {
        let result = if let Some(proxy) = &self.proxy {
            let tcp = proxy.connect(url).await?;
            client_async_tls_with_config(url.as_str(), tcp, Some(config), proxy.tls.clone()).await
        } else {
            connect_async_with_config(url.as_str(), Some(config), false).await
        };
        result
            .map(|(socket, _)| socket)
            .map_err(|_| LiveError::connect())
    }
}

pub fn proxy_required(settings: &Value, account: &Value) -> bool {
    truthy(&settings["outboundProxyEnabled"])
        || proxy_transport::account_requires_tunnel(account)
        || !fixture()
            && ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
                .iter()
                .any(|key| std::env::var(key).is_ok_and(|url| !url.is_empty()))
}

pub async fn transport(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    endpoint: &str,
) -> Result<Transport, LiveError> {
    if truthy(&account["providerSpecificData"]["vercelRelayUrl"]) {
        return Err(LiveError::proxy(
            "Gemini Live requires HTTP CONNECT or SOCKS5h",
        ));
    }
    let invalid_url = || LiveError {
        status: StatusCode::BAD_REQUEST,
        message: "Invalid Gemini Live base URL".into(),
    };
    let mut endpoint = Url::parse(endpoint)
        .ok()
        .filter(|url| {
            matches!(url.scheme(), "http" | "https" | "ws" | "wss")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .ok_or_else(invalid_url)?;
    endpoint.set_scheme("https").map_err(|_| invalid_url())?;
    let route = proxy_transport::select_with_bypass(storage, settings, account, endpoint.as_str())
        .await
        .map_err(|_| LiveError::proxy("Gemini Live connection proxy is unavailable"))?;
    let proxy = match route {
        ProxySelection::Routed(Route::Direct) if !proxy_required(settings, account) => {
            return Ok(Transport {
                endpoint,
                proxy: None,
                protected: false,
            });
        }
        ProxySelection::ExplicitBypass => None,
        ProxySelection::Routed(Route::Proxy { url, .. }) => {
            Some(Proxy::from_address(&url).ok_or_else(|| {
                LiveError::proxy("Gemini Live requires a valid HTTP CONNECT or SOCKS5h proxy")
            })?)
        }
        ProxySelection::Routed(Route::Relay { .. }) => {
            return Err(LiveError::proxy(
                "Gemini Live requires HTTP CONNECT or SOCKS5h",
            ));
        }
        _ => {
            return Err(LiveError::proxy(
                "Gemini Live connection proxy is unavailable",
            ));
        }
    };
    Ok(Transport {
        endpoint,
        proxy,
        protected: true,
    })
}

struct Proxy {
    address: Uri,
    protocol: ProxyProtocol,
    tls: Option<Connector>,
}

enum ProxyProtocol {
    Http(Option<HeaderValue>),
    Socks(Option<(String, String)>),
}

impl Proxy {
    fn from_address(address: &str) -> Option<Self> {
        let url = proxy_transport::tunnel_proxy_url(address)?;
        let credentials = proxy_transport::tunnel_credentials(&url).ok()?;
        let (protocol, default_port) = match url.scheme() {
            "http" => {
                let auth = if let Some((user, password)) = credentials {
                    let encoded = STANDARD.encode(format!("{user}:{password}"));
                    Some(HeaderValue::from_str(&format!("Basic {encoded}")).ok()?)
                } else {
                    None
                };
                (ProxyProtocol::Http(auth), 80)
            }
            "socks5h" => {
                if credentials
                    .as_ref()
                    .is_some_and(|(user, password)| user.len() > 255 || password.len() > 255)
                {
                    return None;
                }
                (ProxyProtocol::Socks(credentials), 1080)
            }
            _ => return None,
        };
        let address = format!(
            "http://{}:{}",
            url.host()?,
            url.port().unwrap_or(default_port)
        )
        .parse()
        .ok()?;
        #[cfg(debug_assertions)]
        let tls = if fixture() && url.host_str() == Some("127.0.0.1") {
            fixture_tls().ok()?
        } else {
            None
        };
        #[cfg(not(debug_assertions))]
        let tls = None;
        Some(Self {
            address,
            protocol,
            tls,
        })
    }

    async fn connect(&self, destination: &Url) -> Result<TcpStream, LiveError> {
        let host = destination.host().ok_or_else(LiveError::connect)?;
        let port = destination
            .port_or_known_default()
            .ok_or_else(LiveError::connect)?;
        let authority = format!("https://{host}:{port}")
            .parse::<Uri>()
            .map_err(|_| LiveError::connect())?;
        let connector = HttpConnector::new();
        let stream = match &self.protocol {
            ProxyProtocol::Http(auth) => {
                let mut proxy = Tunnel::new(self.address.clone(), connector);
                if let Some(auth) = auth {
                    proxy = proxy.with_auth(auth.clone());
                }
                poll_fn(|cx| proxy.poll_ready(cx))
                    .await
                    .map_err(|_| LiveError::connect())?;
                proxy
                    .call(authority)
                    .await
                    .map_err(|_| LiveError::connect())?
            }
            ProxyProtocol::Socks(auth) => {
                let mut proxy = SocksV5::new(self.address.clone(), connector);
                if let Some((user, password)) = auth {
                    proxy = proxy.with_auth(user.clone(), password.clone());
                }
                poll_fn(|cx| proxy.poll_ready(cx))
                    .await
                    .map_err(|_| LiveError::connect())?;
                proxy
                    .call(authority)
                    .await
                    .map_err(|_| LiveError::connect())?
            }
        };
        Ok(stream.into_inner())
    }
}

#[cfg(debug_assertions)]
fn fixture_tls() -> Result<Option<Connector>, ()> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let Some(path) = std::env::var_os("NINEROUTER_MODEL_LOCAL_PROXY_CA") else {
        return Ok(None);
    };
    let pem = std::fs::read(path).map_err(|_| ())?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(&pem).map_err(|_| ())?)
        .map_err(|_| ())?;
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Some(Connector::Rustls(std::sync::Arc::new(config))))
}

fn fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

pub struct Options {
    instruction: String,
    setup_timeout: Duration,
    turn_timeout: Duration,
    verbose: bool,
}

impl Options {
    pub fn from_fields<'a>(fields: impl Fn(&str) -> Option<&'a str>) -> Self {
        let field = |key| fields(key).map(js_trim).filter(|value| !value.is_empty());
        let instruction = if let Some(override_) = field("system_instruction") {
            override_.to_owned()
        } else {
            let prompt = field("prompt").unwrap_or(DEFAULT_INSTRUCTION);
            field("language").map_or_else(
                || prompt.to_owned(),
                |lang| format!("{prompt} Language: {lang}."),
            )
        };
        Self {
            instruction,
            setup_timeout: duration(field("setup_timeout_ms"), SETUP_MS),
            turn_timeout: duration(field("turn_timeout_ms"), TURN_MS),
            verbose: field("response_format")
                .is_some_and(|format| format.eq_ignore_ascii_case("verbose_json")),
        }
    }
}

fn duration(raw: Option<&str>, fallback: u64) -> Duration {
    let milliseconds = raw
        .and_then(|text| {
            if let Ok(number) = text.parse::<f64>() {
                return Some(number);
            }
            let prefix = text.get(..2)?.to_ascii_lowercase();
            let radix = match prefix.as_str() {
                "0x" => 16,
                "0o" => 8,
                "0b" => 2,
                _ => return None,
            };
            u64::from_str_radix(&text[2..], radix)
                .ok()
                .map(|value| value as f64)
        })
        .filter(|number| number.is_finite() && *number > 0.0)
        .map_or(fallback, |number| number.clamp(1.0, MAX_MS) as u64);
    Duration::from_millis(milliseconds)
}

pub struct LiveError {
    pub status: StatusCode,
    pub message: String,
}

impl LiveError {
    fn proxy(message: &str) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }

    fn connect() -> Self {
        Self::upstream("Gemini Live websocket connection failed")
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }

    fn timed_out(waiting_setup: bool) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: if waiting_setup {
                "Gemini Live timed out waiting for setupComplete"
            } else {
                "Gemini Live transcription timed out"
            }
            .into(),
        }
    }
}

pub async fn transcribe(
    audio: &[u8],
    mime_type: String,
    model: &str,
    token: &str,
    options: Options,
    transport: Transport,
) -> Result<Value, LiveError> {
    if audio.is_empty() {
        return Err(LiveError {
            status: StatusCode::BAD_REQUEST,
            message: "Empty audio file".into(),
        });
    }
    let url = transport
        .websocket_url(model, token)
        .ok_or_else(LiveError::connect)?;
    let request = TranscriptRequest {
        audio,
        mime_type: &mime_type,
        model,
        token,
        options: &options,
    };
    let mut transcript = Transcript::default();
    let mut allow_reconnect = true;
    loop {
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        let mut socket = timeout(options.setup_timeout, transport.connect(&url, config))
            .await
            .map_err(|_| LiveError::timed_out(true))??;
        let result = run_socket(&mut socket, &request, &mut transcript, allow_reconnect).await;
        let _ = timeout(
            Duration::from_millis(100),
            socket.close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            })),
        )
        .await;
        match result? {
            Progress::Done => return Ok(transcript.response(options.verbose)),
            Progress::Reconnect => allow_reconnect = false,
        }
    }
}

struct TranscriptRequest<'a> {
    audio: &'a [u8],
    mime_type: &'a str,
    model: &'a str,
    token: &'a str,
    options: &'a Options,
}

#[derive(Default)]
struct Transcript {
    text: String,
    chunks: Vec<String>,
    sent_bytes: usize,
}

impl Transcript {
    fn append(&mut self, delta: &Value, verbose: bool) {
        if let Some(text) = delta.as_str().filter(|text| !js_trim(text).is_empty()) {
            self.text.push_str(text);
            if verbose {
                self.chunks.push(text.to_owned());
            }
        }
    }

    fn response(self, verbose: bool) -> Value {
        if verbose {
            let segments: Vec<_> = self
                .chunks
                .into_iter()
                .enumerate()
                .map(|(id, text)| json!({"id":id,"text":text}))
                .collect();
            json!({"text":self.text,"segments":segments})
        } else {
            json!({"text":self.text})
        }
    }
}

enum Progress {
    Done,
    Reconnect,
}

async fn run_socket(
    socket: &mut Socket,
    request: &TranscriptRequest<'_>,
    transcript: &mut Transcript,
    allow_reconnect: bool,
) -> Result<Progress, LiveError> {
    let options = request.options;
    let mut waiting_setup = true;
    let mut deadline = Instant::now() + options.setup_timeout;
    timeout_at(
        deadline,
        send(
            socket,
            json!({"setup":{
                "model":format!("models/{}", request.model),
                "generationConfig":{"responseModalities":["TEXT"],"inputAudioTranscription":{}},
                "systemInstruction":{"parts":[{"text":options.instruction}]},
            }}),
        ),
    )
    .await
    .map_err(|_| LiveError::timed_out(true))?
    .map_err(|_| LiveError::connect())?;
    let mut rotation: Option<Instant> = None;
    loop {
        let next = rotation.map_or(deadline, |at| at.min(deadline));
        let frame = match timeout_at(next, socket.next()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(_))) => return Err(LiveError::connect()),
            Ok(None) => return on_close(&transcript.text, None),
            Err(_) if rotation.is_some_and(|at| at <= deadline) => return Ok(Progress::Reconnect),
            Err(_) => return Err(LiveError::timed_out(waiting_setup)),
        };
        let parsed = match frame {
            Message::Text(data) => serde_json::from_str::<Value>(data.as_str()),
            Message::Binary(data) => serde_json::from_slice::<Value>(&data),
            Message::Close(info) => {
                return on_close(&transcript.text, info.map(|frame| u16::from(frame.code)));
            }
            _ => continue,
        };
        let Ok(frame) = parsed else { continue };
        if truthy(&frame["error"]) {
            let problem = &frame["error"];
            let status = problem
                .get("status")
                .filter(|value| truthy(value))
                .map(js_text)
                .map_or_else(String::new, |status| format!(" ({status})"));
            let text = problem
                .get("message")
                .filter(|message| truthy(message))
                .map_or_else(|| "unknown".into(), js_text);
            let safe = redact_bearer(&format!("Gemini Live error{status}: {text}"));
            return Err(LiveError::upstream(if request.token.is_empty() {
                safe
            } else {
                safe.replace(&component(request.token), "[redacted]")
                    .replace(request.token, "[redacted]")
            }));
        }
        if truthy(&frame["goAway"]) {
            if allow_reconnect && rotation.is_none() {
                let delay = frame["goAway"]["time"]
                    .as_str()
                    .and_then(|_| date_ms(&frame["goAway"]["time"]))
                    .map(|target| {
                        target
                            .saturating_sub(chrono::Utc::now().timestamp_millis())
                            .max(0) as u64
                    })
                    .map(Duration::from_millis)
                    .unwrap_or_default()
                    .min(options.setup_timeout);
                rotation = Some(Instant::now() + delay);
            }
            continue;
        }
        let content = &frame["serverContent"];
        transcript.append(&content["inputTranscription"]["text"], options.verbose);
        if waiting_setup && (truthy(&content["setupComplete"]) || truthy(&frame["setupComplete"])) {
            waiting_setup = false;
            deadline = Instant::now() + options.turn_timeout;
            timeout_at(
                deadline,
                send_audio(socket, request, &mut transcript.sent_bytes),
            )
            .await
            .map_err(|_| LiveError::timed_out(false))??;
        } else if truthy(&content["turnComplete"]) {
            return Ok(Progress::Done);
        }
    }
}

fn on_close(text: &str, code: Option<u16>) -> Result<Progress, LiveError> {
    if !js_trim(text).is_empty() {
        return Ok(Progress::Done);
    }
    let code = code.map_or_else(String::new, |code| format!(" (code {code})"));
    Err(LiveError::upstream(format!(
        "Gemini Live socket closed before completion{code}"
    )))
}

async fn send_audio(
    socket: &mut Socket,
    request: &TranscriptRequest<'_>,
    sent_bytes: &mut usize,
) -> Result<(), LiveError> {
    for chunk in request.audio[*sent_bytes..].chunks(CHUNK_BYTES) {
        send(socket, json!({"realtimeInput":{"mediaChunks":[{"mimeType":request.mime_type,"data":STANDARD.encode(chunk)}]}}))
            .await.map_err(|_| LiveError::upstream("Gemini Live socket closed while streaming audio"))?;
        *sent_bytes += chunk.len();
    }
    let _ = send(socket, json!({"clientContent":{"turns":[{"parts":[{"text":request.options.instruction}]}],"turnComplete":true}})).await;
    Ok(())
}

async fn send(
    socket: &mut Socket,
    frame: Value,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    socket.send(Message::Text(frame.to_string().into())).await
}
