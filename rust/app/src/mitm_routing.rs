use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use hyper::body::Incoming;
use regex::Regex;
use reqwest::{Client, ClientBuilder};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use url::Url;

use crate::cli_tools::mitm::ListenerConfig;
use crate::storage::Storage;

use crate::mitm_antigravity as antigravity;
#[path = "mitm_kiro.rs"]
mod kiro;

const LOCAL: &str = "localhost";
const TOOLS: &[(&str, &str)] = &[
    ("daily-cloudcode-pa.googleapis.com", "antigravity"),
    ("cloudcode-pa.googleapis.com", "antigravity"),
    ("api.individual.githubcopilot.com", "copilot"),
    ("runtime.us-east-1.kiro.dev", "kiro"),
    ("q.us-east-1.amazonaws.com", "kiro"),
    ("codewhisperer.us-east-1.amazonaws.com", "kiro"),
    ("api2.cursor.sh", "cursor"),
];
const BODY_LIMIT: usize = 32 * 1024 * 1024;
const CONNECT: Duration = Duration::from_secs(10);
const DNS_TTL: Duration = Duration::from_secs(300);
static MAPPING: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("mitm-mapping.json")).expect("embedded MITM mapping rules")
});
static RULES: LazyLock<Vec<(Regex, String)>> = LazyLock::new(|| {
    MAPPING["patterns"]["antigravity"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some((
                Regex::new(&format!("(?i){}", row[0].as_str()?)).ok()?,
                row[1].as_str()?.to_owned(),
            ))
        })
        .collect()
});
static UNMAPPED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    MAPPING["blocked"]["antigravity"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|rule| Regex::new(&format!("(?i){}", rule.as_str()?)).ok())
        .collect()
});

pub(super) fn hosts() -> impl Iterator<Item = &'static str> {
    std::iter::once(LOCAL).chain(TOOLS.iter().map(|(host, _)| *host))
}

pub(super) fn host(name: &str) -> Option<&'static str> {
    hosts().find(|host| host.eq_ignore_ascii_case(name))
}

pub(super) struct State {
    storage: Storage,
    base: Url,
    authorization: HeaderValue,
    router: Client,
    dns: Client,
    fixture_ca: Option<reqwest::Certificate>,
    upstream: Mutex<HashMap<&'static str, (Instant, Client)>>,
}

impl State {
    pub(super) fn new(config: ListenerConfig) -> io::Result<Self> {
        let base = config.router_base;
        if !matches!(base.scheme(), "http" | "https")
            || base.host().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid MITM router URL",
            ));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .map_err(io::Error::other)?;
        authorization.set_sensitive(true);
        let fixture_ca = if cfg!(debug_assertions)
            && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        {
            Some(
                reqwest::Certificate::from_pem(&std::fs::read(config.certificate)?)
                    .map_err(io::Error::other)?,
            )
        } else {
            None
        };
        let router = http(fixture_ca.as_ref())
            .build()
            .map_err(io::Error::other)?;
        let dns = http(fixture_ca.as_ref())
            .timeout(CONNECT)
            .resolve("dns.google", SocketAddr::from(([8, 8, 8, 8], 443)))
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            storage: config.storage,
            base,
            authorization,
            router,
            dns,
            fixture_ca,
            upstream: Mutex::default(),
        })
    }

    pub(super) async fn handle(&self, host: &'static str, request: Request<Incoming>) -> Response {
        let (parts, raw) = request.into_parts();
        if !authority_matches(host, &parts.uri, &parts.headers) {
            return failure(
                StatusCode::MISDIRECTED_REQUEST,
                "MITM authority does not match TLS host",
            );
        }
        if host == LOCAL {
            return if parts.method == Method::GET && parts.uri.path() == "/_mitm_health" {
                Json(json!({"ok":true,"pid":std::process::id()})).into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            };
        }
        let Some(tool) = TOOLS
            .iter()
            .find_map(|(name, tool)| (*name == host).then_some(*tool))
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let path = parts.uri.path_and_query().map_or("/", |path| path.as_str());
        let Ok(raw) = to_bytes(Body::new(raw), BODY_LIMIT).await else {
            return failure(
                StatusCode::PAYLOAD_TOO_LARGE,
                "MITM request body could not be read",
            );
        };
        if parts
            .headers
            .get("x-request-source")
            .is_some_and(|source| source == "local")
            || !chat_request(tool, path, &parts.headers)
        {
            return self
                .passthrough(host, parts.method, path, &parts.headers, raw)
                .await;
        }
        if tool == "cursor" {
            return (StatusCode::NOT_IMPLEMENTED, Json(json!({
                "error":{"message":"Cursor MITM support is coming soon.","type":"not_implemented"},
            }))).into_response();
        }
        let body = serde_json::from_slice::<Value>(&raw).ok();
        let Some(model) = model_name(path, body.as_ref(), tool) else {
            return self
                .passthrough(host, parts.method, path, &parts.headers, raw)
                .await;
        };
        let Some(mapped) = self.mapped(tool, &model).await else {
            return self
                .passthrough(host, parts.method, path, &parts.headers, raw)
                .await;
        };
        let Some(mut body) = body.filter(Value::is_object) else {
            return failure(StatusCode::BAD_REQUEST, "Invalid MITM chat request");
        };
        match tool {
            "copilot" => {
                body["model"] = json!(mapped);
                let endpoint = [
                    ("/chat/completions", "/v1/chat/completions"),
                    ("/v1/messages", "/v1/messages"),
                    ("/responses", "/v1/responses"),
                ]
                .into_iter()
                .find_map(|(prefix, endpoint)| path.contains(prefix).then_some(endpoint))
                .unwrap_or("/v1/chat/completions");
                self.forward(endpoint, &parts.headers, body).await
            }
            "antigravity" => {
                let Some(body) = antigravity::request(&body, &mapped) else {
                    return failure(StatusCode::BAD_REQUEST, "Invalid Antigravity MITM request");
                };
                self.forward_translated(
                    &parts.headers,
                    body,
                    "text/event-stream",
                    antigravity::Stream::new(),
                    antigravity::Stream::chunk,
                    antigravity::Stream::finish,
                )
                .await
            }
            "kiro" => {
                let Some(body) = kiro::request(&body, &mapped) else {
                    return failure(
                        StatusCode::BAD_REQUEST,
                        "Kiro MITM request did not contain messages",
                    );
                };
                self.forward_translated(
                    &parts.headers,
                    body,
                    "application/vnd.amazon.eventstream",
                    kiro::Stream::new(mapped),
                    kiro::Stream::chunk,
                    kiro::Stream::finish,
                )
                .await
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    async fn mapped(&self, tool: &'static str, model: &str) -> Option<String> {
        if tool == "antigravity" && UNMAPPED.iter().any(|pattern| pattern.is_match(model)) {
            return None;
        }
        let lookup = MAPPING["synonyms"][tool][model].as_str().unwrap_or(model);
        let aliases = self
            .storage
            .run(false, move |db| {
                db.query_row(
                    "SELECT value FROM kv WHERE scope='mitmAlias' AND key=?1",
                    [tool],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .await
            .ok()??;
        let aliases: Value = serde_json::from_str(&aliases).ok()?;
        let mapped = |key: &str| {
            aliases[key]
                .as_str()
                .filter(|alias| !alias.trim().is_empty())
        };
        mapped(lookup)
            .or_else(|| {
                aliases.as_object()?.iter().find_map(|(key, _)| {
                    (!key.is_empty() && (lookup.starts_with(key) || key.starts_with(lookup)))
                        .then(|| mapped(key))
                        .flatten()
                })
            })
            .or_else(|| {
                if tool == "antigravity" {
                    RULES.iter().find_map(|(pattern, key)| {
                        pattern.is_match(lookup).then(|| mapped(key)).flatten()
                    })
                } else {
                    None
                }
            })
            .map(str::to_owned)
    }

    async fn forward(&self, path: &str, incoming: &HeaderMap, body: Value) -> Response {
        let endpoint = format!("{}{path}", self.base.as_str().trim_end_matches('/'));
        let Ok(endpoint) = Url::parse(&endpoint) else {
            return failure(StatusCode::BAD_GATEWAY, "Invalid MITM router URL");
        };
        self.send_router(endpoint, incoming, body).await
    }

    async fn send_router(&self, endpoint: Url, incoming: &HeaderMap, body: Value) -> Response {
        match self.request_router(endpoint.as_str(), incoming, body).await {
            Ok(response) if !response.status().is_redirection() => stream(response, true),
            _ => failure(StatusCode::BAD_GATEWAY, "MITM router request failed"),
        }
    }

    async fn forward_translated<T: Send + 'static>(
        &self,
        incoming: &HeaderMap,
        body: Value,
        media_type: &'static str,
        encoder: T,
        chunk: fn(&mut T, Value, &mut Vec<u8>),
        finish: fn(&mut T, &mut Vec<u8>),
    ) -> Response {
        let endpoint = format!(
            "{}/v1/chat/completions",
            self.base.as_str().trim_end_matches('/')
        );
        match self.request_router(&endpoint, incoming, body).await {
            Ok(response) if response.status().is_success() => {
                translate_response(response, media_type, encoder, chunk, finish)
            }
            Ok(response) if !response.status().is_redirection() => stream(response, true),
            _ => failure(StatusCode::BAD_GATEWAY, "MITM router request failed"),
        }
    }

    async fn request_router(
        &self,
        endpoint: &str,
        incoming: &HeaderMap,
        body: Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut request = self
            .router
            .post(endpoint)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .json(&body);
        for name in [
            "accept",
            "accept-language",
            "anthropic-beta",
            "anthropic-version",
            "user-agent",
            "x-app",
        ] {
            if let Some(value) = incoming.get(name) {
                request = request.header(name, value.clone());
            }
        }
        request.send().await
    }

    async fn passthrough(
        &self,
        host: &'static str,
        method: Method,
        path: &str,
        incoming: &HeaderMap,
        body: Bytes,
    ) -> Response {
        let Some(upstream) = self.client(host).await else {
            return failure(StatusCode::BAD_GATEWAY, "MITM upstream resolution failed");
        };
        let mut headers = incoming.clone();
        strip(&mut headers);
        headers.remove(header::HOST);
        headers.remove(header::CONTENT_LENGTH);
        headers.remove("x-request-source");
        let endpoint = format!("https://{host}{path}");
        let mut request = upstream.request(method, endpoint).headers(headers);
        if !body.is_empty() {
            request = request.body(body);
        }
        match request.send().await {
            Ok(response) => stream(response, false),
            Err(_) => failure(StatusCode::BAD_GATEWAY, "MITM upstream request failed"),
        }
    }

    async fn client(&self, host: &'static str) -> Option<Client> {
        if let Some((expires, client)) = self.upstream.lock().await.get(host)
            && *expires > Instant::now()
        {
            return Some(client.clone());
        }
        let mut response = self
            .dns
            .get("https://dns.google/resolve")
            .query(&[("name", host), ("type", "A")])
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        if response
            .content_length()
            .is_some_and(|length| length > 64 * 1024)
        {
            return None;
        }
        let mut document = Vec::new();
        while let Some(bytes) = response.chunk().await.ok()? {
            if bytes.len() > 64 * 1024 - document.len() {
                return None;
            }
            document.extend_from_slice(&bytes);
        }
        let record: Value = serde_json::from_slice(&document).ok()?;
        let addresses: Vec<SocketAddr> = record["Answer"]
            .as_array()?
            .iter()
            .filter(|record| record["type"] == 1)
            .filter_map(|record| record["data"].as_str()?.parse::<Ipv4Addr>().ok())
            .filter(|address| {
                let [first, second, ..] = address.octets();
                first != 0
                    && first < 224
                    && !address.is_loopback()
                    && !address.is_private()
                    && !address.is_link_local()
                    && !(first == 100 && (64..=127).contains(&second))
            })
            .map(|address| SocketAddr::from((address, 443)))
            .collect();
        if addresses.is_empty() {
            return None;
        }
        let client = http(self.fixture_ca.as_ref())
            .resolve_to_addrs(host, &addresses)
            .build()
            .ok()?;
        self.upstream
            .lock()
            .await
            .insert(host, (Instant::now() + DNS_TTL, client.clone()));
        Some(client)
    }
}

fn http(fixture_ca: Option<&reqwest::Certificate>) -> ClientBuilder {
    let builder = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(CONNECT)
        .read_timeout(Duration::from_secs(120))
        .no_brotli()
        .no_deflate()
        .no_gzip();
    if let Some(certificate) = fixture_ca {
        builder.add_root_certificate(certificate.clone())
    } else {
        builder
    }
}

fn authority_matches(expected: &str, uri: &Uri, headers: &HeaderMap) -> bool {
    let mut authority_headers = headers.get_all(header::HOST).iter();
    let header = authority_headers.next();
    if authority_headers.next().is_some() {
        return false;
    }
    let host = header
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<axum::http::uri::Authority>().ok());
    if header.is_some() && host.is_none() {
        return false;
    }
    let authority =
        |authority: &axum::http::uri::Authority| authority.host().eq_ignore_ascii_case(expected);
    let target = uri.authority();
    (target.is_some() || host.is_some())
        && target.is_none_or(authority)
        && host.as_ref().is_none_or(authority)
}

fn chat_request(tool: &str, path: &str, headers: &HeaderMap) -> bool {
    MAPPING["paths"][tool]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|pattern| path.contains(pattern))
        || tool == "kiro"
            && headers
                .get("x-amz-target")
                .and_then(|target| target.to_str().ok())
                .is_some_and(|target| target.contains("GenerateAssistantResponse"))
}

fn model_name(path: &str, body: Option<&Value>, tool: &str) -> Option<String> {
    let path_model = path
        .split_once("/models/")
        .and_then(|(_, suffix)| suffix.split(['/', ':', '?']).next());
    let field = |value: &Value| {
        value
            .as_str()
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
    };
    let model = if tool == "kiro" {
        field(&body?["conversationState"]["currentMessage"]["userInputMessage"]["modelId"])?
    } else {
        path_model
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
            .or_else(|| field(&body?["model"]))?
    };
    let model = model.strip_prefix("models/").unwrap_or(&model);
    if matches!(
        model,
        "gemini-3.6-flash-tiered" | "gemini-3.7-flash-tiered" | "gemini-3.8-flash-tiered"
    ) && let Some(body) = body
    {
        let config = body.get("request").unwrap_or(body);
        let level = config["generationConfig"]["thinkingConfig"]["thinkingLevel"]
            .as_str()
            .map(str::to_ascii_lowercase)
            .filter(|level| matches!(level.as_str(), "high" | "medium" | "low"));
        return Some(format!(
            "{}-{}",
            model.trim_end_matches("-tiered"),
            level.as_deref().unwrap_or("medium")
        ));
    }
    Some(model.to_owned())
}

fn strip(headers: &mut HeaderMap) {
    let connection = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter_map(|name| name.parse::<header::HeaderName>().ok())
        .collect::<Vec<_>>();
    for name in connection {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

fn stream(response: reqwest::Response, router: bool) -> Response {
    let status = response.status();
    let mut headers = response.headers().clone();
    strip(&mut headers);
    if router {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        if headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"))
        {
            headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
        }
    }
    let mut result = Response::new(Body::from_stream(response.bytes_stream()));
    *result.status_mut() = status;
    *result.headers_mut() = headers;
    result
}

fn translate_response<T: Send + 'static>(
    response: reqwest::Response,
    media_type: &'static str,
    encoder: T,
    chunk: fn(&mut T, Value, &mut Vec<u8>),
    finish: fn(&mut T, &mut Vec<u8>),
) -> Response {
    let input = Box::pin(response.bytes_stream());
    let input = (input, encoder, Vec::new(), false);
    let body = futures_util::stream::unfold(
        input,
        move |(mut input, mut encoder, mut pending, mut ended)| async move {
            if ended {
                return None;
            }
            loop {
                let mut output = Vec::new();
                match input.next().await {
                    Some(Ok(bytes)) => {
                        pending.extend_from_slice(&bytes);
                        let complete = pending
                            .iter()
                            .rposition(|&byte| byte == b'\n')
                            .map_or(0, |index| index + 1);
                        for line in pending[..complete].split(|&byte| byte == b'\n') {
                            if let Some(value) = sse_json(line) {
                                chunk(&mut encoder, value, &mut output);
                            }
                        }
                        pending.drain(..complete);
                        if pending.len() > 4 * 1024 * 1024 {
                            return Some((
                                Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "MITM SSE line is too large",
                                )),
                                (input, encoder, pending, true),
                            ));
                        }
                    }
                    Some(Err(error)) => {
                        return Some((
                            Err(io::Error::other(error)),
                            (input, encoder, pending, true),
                        ));
                    }
                    None => {
                        if let Some(value) = sse_json(&pending) {
                            chunk(&mut encoder, value, &mut output);
                        }
                        finish(&mut encoder, &mut output);
                        ended = true;
                    }
                }
                if !output.is_empty() {
                    return Some((Ok(Bytes::from(output)), (input, encoder, pending, ended)));
                }
                if ended {
                    return None;
                }
            }
        },
    );
    let mut result = Response::new(Body::from_stream(body));
    result
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(media_type));
    result
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if media_type == "text/event-stream" {
        result
            .headers_mut()
            .insert("x-accel-buffering", HeaderValue::from_static("no"));
    }
    result
}

fn sse_json(line: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(line)
        .ok()?
        .trim()
        .strip_prefix("data:")?
        .trim();
    serde_json::from_str(text).ok()
}

fn failure(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        Json(json!({"error":{"message":message,"type":"mitm_error"}})),
    )
        .into_response()
}
