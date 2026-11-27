use std::future::poll_fn;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header::HOST};
use axum::response::Response;
use base64::{Engine, engine::general_purpose::STANDARD};
use hyper_util::{
    client::legacy::connect::proxy::{SocksV5, Tunnel},
    rt::TokioIo,
};
use percent_encoding::percent_decode_str;
use reqwest::{Client, RequestBuilder};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{Instant, timeout, timeout_at};
use tokio_rustls::TlsConnector;
use tower_service::Service;
use url::{Host, Url};

use super::{ANTHROPIC, EMBEDDING, NodeState, error, respond};
use crate::catalog_guard::is_direct_loopback;
use crate::chat_prefetch::public_address;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::proxy_transport::{self, Route, Selection};

const LIMIT: usize = 64 * 1024;
const EMBEDDING_RESPONSE_LIMIT: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
static TLS: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    let roots: rustls::RootCertStore = webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect();
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
});

struct Probe {
    client: Client,
    tunnel: Option<ProxyTunnel>,
    sockets: Vec<SocketAddr>,
}

enum ProxyAuth {
    Connect(Option<HeaderValue>),
    Socks(Option<(String, String)>),
}

struct ProxyTunnel {
    uri: Uri,
    auth: ProxyAuth,
}

pub(super) async fn run(State(state): State<NodeState>, request: Request) -> Response {
    let local = is_direct_loopback(&request);
    let value = match to_bytes(request.into_body(), LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid validation payload"),
    };
    let Ok(body) = serde_json::from_slice::<Value>(&value) else {
        return error(StatusCode::BAD_REQUEST, "Invalid validation payload");
    };
    let text = |key| {
        body[key]
            .as_str()
            .map(js_trim)
            .filter(|value| !value.is_empty())
    };
    let (Some(base), Some(key)) = (text("baseUrl"), text("apiKey")) else {
        return error(StatusCode::BAD_REQUEST, "Base URL and API key required");
    };
    let Ok(url) = Url::parse(base) else {
        return error(StatusCode::BAD_REQUEST, "Invalid URL format");
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return error(StatusCode::BAD_REQUEST, "URL not allowed");
    }
    if !local && url.scheme() != "https" {
        return error(StatusCode::BAD_REQUEST, "URL not allowed");
    }
    let model = text("modelId");
    let embedding = body["type"] == EMBEDDING;
    if embedding && model.is_none() {
        return respond(
            StatusCode::OK,
            json!({"valid":false,"error":"Model ID required for embedding validation"}),
        );
    }
    let Some(settings) = state.storage.read_settings().await else {
        return unavailable("Could not read outbound proxy settings");
    };
    let tunnel = match proxy_transport::select_with_bypass(
        &state.storage,
        &settings,
        &Value::Null,
        url.as_str(),
    )
    .await
    {
        Ok(Selection::ExplicitBypass) => None,
        Ok(Selection::Routed(Route::Direct)) if !truthy(&settings["outboundProxyEnabled"]) => None,
        Ok(Selection::Routed(Route::Proxy { url, .. })) => {
            let Some(tunnel) = ProxyTunnel::new(&url) else {
                return unavailable("Unsupported endpoint validation proxy");
            };
            Some(tunnel)
        }
        _ => return unavailable("Endpoint validation proxy unavailable"),
    };
    let sockets = if local && tunnel.is_none() {
        Vec::new()
    } else {
        match timeout(TIMEOUT, allowed_addresses(&url, local)).await {
            Ok(Ok(sockets)) => sockets,
            Ok(Err(())) => return error(StatusCode::BAD_REQUEST, "URL not allowed"),
            Err(_) => return network(true),
        }
    };
    let Ok(client) = probe_client(&url, &sockets) else {
        return network(false);
    };
    let probe = Probe {
        client,
        tunnel,
        sockets,
    };
    if reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).is_err() {
        return error(StatusCode::BAD_REQUEST, "Invalid API key format");
    }
    let anthropic = body["type"] == ANTHROPIC;
    let base = url.as_str();
    if embedding && let Some(model) = model {
        return embeddings(&probe, base, key, model).await;
    }
    let paths = if anthropic {
        crate::upstream::ANTHROPIC_NODE_PATHS
    } else {
        crate::upstream::OPENAI_NODE_PATHS
    };
    let Some(models_url) = crate::upstream::node_url(base, "/models", paths) else {
        return error(StatusCode::BAD_REQUEST, "Invalid URL format");
    };
    let authentication = |request: RequestBuilder| {
        let request = if anthropic && crate::upstream::anthropic_official(&url) {
            request
        } else {
            request.bearer_auth(key)
        };
        if anthropic {
            let mut secret =
                reqwest::header::HeaderValue::from_str(key).expect("validated API key");
            secret.set_sensitive(true);
            request
                .header("x-api-key", secret)
                .header("anthropic-version", "2023-06-01")
        } else {
            request
        }
    };
    let models = match probe
        .send(authentication(probe.client.get(models_url)))
        .await
    {
        Ok(response) => response.status(),
        Err(error) => return network(error),
    };
    if models.is_success() {
        return respond(StatusCode::OK, json!({"valid":true}));
    }
    if models.is_redirection() {
        return respond(
            StatusCode::OK,
            json!({"valid":false,"error":"Provider redirect refused"}),
        );
    }
    if unauthorized(models) {
        return respond(
            StatusCode::OK,
            json!({"valid":false,"error":"API key unauthorized"}),
        );
    }
    let Some(model) = model else {
        return respond(
            StatusCode::OK,
            json!({"valid":false,"error":models_error(models)}),
        );
    };
    let responses = !anthropic && body["apiType"] == "responses";
    let (path, payload) = if anthropic {
        (
            "/messages",
            json!({"model":model,"messages":[{"role":"user","content":"ping"}],"max_tokens":1}),
        )
    } else if responses {
        (
            "/responses",
            json!({"model":model,"input":"ping","max_output_tokens":16}),
        )
    } else {
        (
            "/chat/completions",
            json!({"model":model,"messages":[{"role":"user","content":"ping"}],"max_tokens":1}),
        )
    };
    let method = if responses { "responses" } else { "chat" };
    let Some(endpoint) = crate::upstream::node_url(base, path, paths) else {
        return error(StatusCode::BAD_REQUEST, "Invalid URL format");
    };
    let result = match probe
        .send(authentication(probe.client.post(endpoint).json(&payload)))
        .await
    {
        Ok(response) => response.status(),
        Err(error) => return network(error),
    };
    if result.is_success() {
        respond(StatusCode::OK, json!({"valid":true,"method":method}))
    } else {
        respond(
            StatusCode::OK,
            json!({"valid":false,"error":inference_error(result, responses),"method":method}),
        )
    }
}

async fn embeddings(probe: &Probe, base: &str, key: &str, model: &str) -> Response {
    let Some(endpoint) =
        crate::upstream::node_url(base, "/embeddings", crate::upstream::EMBEDDING_NODE_PATHS)
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid URL format");
    };
    let request = probe
        .client
        .post(endpoint)
        .bearer_auth(key)
        .json(&json!({"model":model,"input":"ping"}));
    let deadline = Instant::now() + TIMEOUT;
    let response = match probe.send(request).await {
        Ok(response) => response,
        Err(error) => return network(error),
    };
    let status = response.status();
    if status.is_success() {
        let dimensions = timeout_at(
            deadline,
            to_bytes(response.into_body(), EMBEDDING_RESPONSE_LIMIT),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value["data"][0]["embedding"].as_array().map(Vec::len));
        return respond(
            StatusCode::OK,
            json!({"valid":true,"method":"embeddings","dimensions":dimensions}),
        );
    }
    if unauthorized(status) {
        return respond(
            StatusCode::OK,
            json!({"valid":false,"error":"API key unauthorized"}),
        );
    }
    let mut message = if status.is_redirection() {
        "Provider redirect refused".to_owned()
    } else {
        format!("Embeddings request failed ({})", status.as_u16())
    };
    if !status.is_redirection()
        && let Ok(Ok(bytes)) = timeout_at(deadline, to_bytes(response.into_body(), LIMIT)).await
        && !bytes.is_empty()
    {
        let raw = String::from_utf8_lossy(&bytes);
        let escaped = serde_json::to_string(key).expect("serialize validation token");
        let detail = raw
            .replace(key, "[redacted]")
            .replace(&escaped[1..escaped.len() - 1], "[redacted]");
        let decoded = percent_decode_str(&detail).decode_utf8_lossy();
        let detail = if decoded.contains(key) {
            decoded.replace(key, "[redacted]")
        } else {
            detail
        };
        let detail = serde_json::from_str::<Value>(&detail)
            .ok()
            .map(|value| value.to_string())
            .filter(|value| value.contains(key) || value.contains(&escaped[1..escaped.len() - 1]))
            .map(|value| {
                value
                    .replace(key, "[redacted]")
                    .replace(&escaped[1..escaped.len() - 1], "[redacted]")
            })
            .unwrap_or(detail);
        let detail = String::from_utf16_lossy(&detail.encode_utf16().take(200).collect::<Vec<_>>());
        message.push_str(": ");
        message.push_str(&detail);
    }
    respond(
        StatusCode::OK,
        json!({"valid":false,"error":message,"method":"embeddings"}),
    )
}

async fn allowed_addresses(url: &Url, local: bool) -> Result<Vec<SocketAddr>, ()> {
    let port = url.port_or_known_default().ok_or(())?;
    let sockets: Vec<_> = match url.host().ok_or(())? {
        Host::Ipv4(ip) => vec![SocketAddr::new(ip.into(), port)],
        Host::Ipv6(ip) => vec![SocketAddr::new(ip.into(), port)],
        Host::Domain(host) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| ())?
            .collect(),
    };
    let allowed = |address: IpAddr| {
        let address = address.to_canonical();
        public_address(address).is_some()
            || local
                && match address {
                    IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
                    IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
                }
    };
    if sockets.is_empty() || !sockets.iter().all(|socket| allowed(socket.ip())) {
        return Err(());
    }
    Ok(sockets)
}

fn probe_client(url: &Url, sockets: &[SocketAddr]) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder()
        .no_proxy()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never());
    if !sockets.is_empty()
        && let Some(host) = url.domain()
    {
        builder = builder.resolve_to_addrs(host, sockets);
    }
    builder.build()
}

impl Probe {
    async fn send(&self, request: RequestBuilder) -> Result<Response, bool> {
        if let Some(tunnel) = &self.tunnel {
            return timeout(TIMEOUT, tunnel.send(request, &self.sockets))
                .await
                .map_err(|_| true)?
                .map_err(|_| false);
        }
        let response = request.send().await.map_err(|error| error.is_timeout())?;
        let status = response.status();
        let mut response = Response::new(Body::from_stream(response.bytes_stream()));
        *response.status_mut() = status;
        Ok(response)
    }
}

impl ProxyTunnel {
    fn new(address: &str) -> Option<Self> {
        let url = proxy_transport::tunnel_proxy_url(address)?;
        let credentials = proxy_transport::tunnel_credentials(&url).ok()?;
        let (scheme, port, auth) = match url.scheme() {
            "socks5" | "socks5h" => ("http", 1080, ProxyAuth::Socks(credentials)),
            scheme => {
                let header = if let Some((user, password)) = credentials {
                    let encoded = STANDARD.encode(format!("{user}:{password}"));
                    let mut header = HeaderValue::from_str(&format!("Basic {encoded}")).ok()?;
                    header.set_sensitive(true);
                    Some(header)
                } else {
                    None
                };
                let port = if scheme == "https" { 443 } else { 80 };
                (scheme, port, ProxyAuth::Connect(header))
            }
        };
        let uri = format!("{scheme}://{}:{}", url.host()?, url.port().unwrap_or(port))
            .parse()
            .ok()?;
        Some(Self { uri, auth })
    }

    async fn send(&self, request: RequestBuilder, sockets: &[SocketAddr]) -> Result<Response, ()> {
        let request = request.build().map_err(|_| ())?;
        let target = request.url();
        let host = target.host().ok_or(())?;
        let origin: Uri = target.as_str().parse().map_err(|_| ())?;
        let mut outbound = Request::builder()
            .method(request.method().clone())
            .uri(origin.path_and_query().ok_or(())?.as_str())
            .body(Body::from(
                request
                    .body()
                    .map(|body| body.as_bytes().ok_or(()))
                    .transpose()?
                    .unwrap_or_default()
                    .to_vec(),
            ))
            .map_err(|_| ())?;
        *outbound.headers_mut() = request.headers().clone();
        outbound.headers_mut().insert(
            HOST,
            HeaderValue::from_str(origin.authority().ok_or(())?.as_str()).map_err(|_| ())?,
        );
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(TLS.as_ref().clone())
            .https_or_http()
            .enable_http1()
            .build();
        for socket in sockets {
            let destination = format!("https://{socket}").parse::<Uri>().map_err(|_| ())?;
            let connection = match &self.auth {
                ProxyAuth::Connect(authorization) => {
                    let mut tunnel = Tunnel::new(self.uri.clone(), connector.clone());
                    if let Some(authorization) = authorization {
                        tunnel = tunnel.with_auth(authorization.clone());
                    }
                    poll_fn(|cx| tunnel.poll_ready(cx)).await.map_err(|_| ())?;
                    tunnel.call(destination).await.ok()
                }
                ProxyAuth::Socks(credentials) => {
                    let mut tunnel = SocksV5::new(self.uri.clone(), connector.clone());
                    if let Some((user, password)) = credentials {
                        tunnel = tunnel.with_auth(user.clone(), password.clone());
                    }
                    poll_fn(|cx| tunnel.poll_ready(cx)).await.map_err(|_| ())?;
                    tunnel.call(destination).await.ok()
                }
            };
            let Some(stream) = connection.map(TokioIo::new) else {
                continue;
            };
            if target.scheme() == "https" {
                let server = match host {
                    Host::Domain(domain) => {
                        rustls::pki_types::ServerName::try_from(domain.to_owned())
                            .map_err(|_| ())?
                    }
                    Host::Ipv4(ip) => rustls::pki_types::ServerName::from(IpAddr::V4(ip)),
                    Host::Ipv6(ip) => rustls::pki_types::ServerName::from(IpAddr::V6(ip)),
                };
                let stream = TlsConnector::from(TLS.clone())
                    .connect(server, stream)
                    .await
                    .map_err(|_| ())?;
                return exchange(stream, outbound).await;
            }
            return exchange(stream, outbound).await;
        }
        Err(())
    }
}

async fn exchange(
    stream: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
    request: Request,
) -> Result<Response, ()> {
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| ())?;
    tokio::spawn(connection);
    sender
        .send_request(request)
        .await
        .map(|response| response.map(Body::new))
        .map_err(|_| ())
}

fn unauthorized(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

fn models_error(status: StatusCode) -> String {
    match status.as_u16() {
        404 => "/models endpoint not found - try chat validation with model ID".into(),
        500..=599 => "Server error - try again later".into(),
        code => format!("Unexpected response ({code})"),
    }
}

fn inference_error(status: StatusCode, responses: bool) -> String {
    let endpoint = if responses { "Responses" } else { "Chat" };
    match status.as_u16() {
        300..=399 => "Provider redirect refused".into(),
        400 => "Invalid model or bad request".into(),
        401 | 403 => "API key unauthorized".into(),
        404 => format!("{endpoint} endpoint not found"),
        500..=599 => "Server error - try again later".into(),
        code => format!("{endpoint} request failed ({code})"),
    }
}

fn unavailable(message: &str) -> Response {
    respond(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"valid":false,"error":message}),
    )
}

fn network(timed_out: bool) -> Response {
    if timed_out {
        unavailable("Request timeout (>10s) - provider node not responding")
    } else {
        unavailable("Network connection failed - check URL and network connectivity")
    }
}
