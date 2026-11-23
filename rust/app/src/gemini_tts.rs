use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::Response;
use serde_json::{Value, json};
use url::Url;

use crate::chat_accounts::{self, Selection};
use crate::chat_input::{js_json, truthy};
use crate::chat_output::error;
use crate::cli_video::redact_bearer;
use crate::observed_response::into_http;
use crate::proxy_transport::{self, Route, Selection as ProxySelection};
use crate::storage::Storage;
use crate::{gemini_public, gemini_request, key_policy};

const PROVIDER: &str = "gemini";
const FIXTURE: &str = "http://127.0.0.1:52989/gemini/v1beta/models";
const VARY: &str =
    "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch";
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static PROTECTED: OnceLock<reqwest::Client> = OnceLock::new();

pub struct Native {
    requested: String,
    stream: bool,
}

impl Native {
    pub fn parse(path: &str, body: &Value) -> Option<Self> {
        let (requested, stream) = gemini_public::model_action(path)?;
        gemini_public::is_native_tts(&requested, body).then_some(Self { requested, stream })
    }

    pub async fn forward(
        self,
        storage: &Storage,
        uri: &Uri,
        headers: &HeaderMap,
        policy_key: Option<&str>,
        body: &Value,
    ) -> Option<Response> {
        let settings = storage.read_settings().await?;
        if settings.get("requireApiKey").is_none_or(truthy) {
            let Some(key) = inference_key(uri, headers).filter(|key| !key.is_empty()) else {
                return Some(plain(StatusCode::UNAUTHORIZED, "Missing API key"));
            };
            if !storage.api_key_is_active(key).await {
                return Some(plain(StatusCode::UNAUTHORIZED, "Invalid API key"));
            }
        }
        let model = gemini_public::native_model_id(&self.requested);
        if model.is_empty()
            || !model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
        {
            return Some(plain(StatusCode::BAD_REQUEST, "Invalid model"));
        }
        if let Some(key) = policy_key.filter(|key| !key.is_empty()) {
            let key = key.to_owned();
            let model_id = model.to_owned();
            if !storage
                .run(false, move |db| {
                    key_policy::permits_resolved(db, &key, PROVIDER, &model_id)
                })
                .await
                .ok()?
            {
                return Some(error(
                    StatusCode::FORBIDDEN,
                    &format!(
                        "This API key is not allowed to use model \"{}\"",
                        self.requested
                    ),
                ));
            }
        }
        let accounts = chat_accounts::active_connections(storage, PROVIDER)
            .await
            .ok()?;
        let target = upstream_url(uri, model, self.stream, false)?;
        let fixture = std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1");
        let direct_target = if fixture {
            upstream_url(uri, model, self.stream, true)?
        } else {
            target.clone()
        };
        let body = Bytes::from(js_json(body, false)?);
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .cloned()
            .filter(|value| !value.is_empty())
            .unwrap_or(HeaderValue::from_static("application/json"));
        let timeout = std::env::var("GEMINI_NATIVE_TTS_FETCH_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(45_000);
        let timeout = Duration::from_millis(timeout);
        let client = CLIENT.get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(crate::upstream::redirects())
                .build()
                .expect("Gemini TTS HTTP client")
        });
        let mut excluded = HashSet::new();
        let mut last: Option<(StatusCode, String)> = None;
        loop {
            let account =
                match chat_accounts::select(storage, PROVIDER, model, &excluded, None).await {
                    Ok(Selection::Ready(account)) => account,
                    Ok(Selection::Empty) => return Some(exhausted(last, None, None, &accounts)),
                    Ok(Selection::CoolingDown(retry)) => {
                        return Some(exhausted(
                            last,
                            retry.last_error_code,
                            retry.last_error,
                            &accounts,
                        ));
                    }
                    Err(_) => {
                        return Some(plain(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Failed to select provider credentials",
                        ));
                    }
                };
            let Some(connection) = account["id"].as_str() else {
                return Some(plain(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Invalid provider connection",
                ));
            };
            let (request, route, protected) = match routed_request(
                storage,
                &settings,
                &account,
                client,
                &target,
                &direct_target,
            )
            .await
            {
                Ok(request) => request,
                Err(message) => return Some(plain(StatusCode::SERVICE_UNAVAILABLE, message)),
            };
            let mut request = request
                .header(header::CONTENT_TYPE, content_type.clone())
                .body(body.clone());
            if let Some(key) = nonempty(&account["apiKey"]) {
                request = request.header("x-goog-api-key", key);
            } else if let Some(token) = nonempty(&account["accessToken"]) {
                request = request.bearer_auth(token);
            } else {
                return Some(plain(StatusCode::NOT_FOUND, "No Gemini API key configured"));
            }
            if let Some(route) = &route {
                request = route.relay(request);
            }
            let (status, message, response_headers) =
                match tokio::time::timeout(timeout, request.send()).await {
                    Ok(Ok(upstream)) if protected && upstream.status().is_redirection() => {
                        return Some(plain(
                            StatusCode::BAD_GATEWAY,
                            "Gemini TTS proxy returned an unexpected redirect",
                        ));
                    }
                    Ok(Ok(upstream))
                        if upstream
                            .headers()
                            .get(header::CONTENT_ENCODING)
                            .is_some_and(|value| {
                                !value.as_bytes().eq_ignore_ascii_case(b"identity")
                            }) =>
                    {
                        (
                            StatusCode::BAD_GATEWAY,
                            "Unsupported upstream content encoding".into(),
                            None,
                        )
                    }
                    Ok(Ok(upstream)) if upstream.status().is_success() => {
                        let _ = chat_accounts::mark_success(storage, connection, model).await;
                        let status = upstream.status();
                        let response_headers = upstream.headers().clone();
                        let stream = Body::from_stream(upstream.bytes_stream());
                        return Some(transparent(status, response_headers, stream));
                    }
                    Ok(Ok(upstream)) => {
                        let status = upstream.status();
                        let headers = upstream.headers().clone();
                        let Ok(bytes) = upstream.bytes().await else {
                            return Some(plain_with_code(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "terminated",
                            ));
                        };
                        let text = String::from_utf8_lossy(&bytes);
                        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                        let mut message = redact(text, &account);
                        if let Some(Route::Relay {
                            authorization: Some(secret),
                            ..
                        }) = &route
                            && let Ok(secret) = secret.to_str()
                        {
                            message = message.replace(secret, "[redacted]");
                        }
                        (status, message, Some(headers))
                    }
                    Ok(Err(error)) => (StatusCode::BAD_GATEWAY, network_error(&error), None),
                    Err(_) => (
                        StatusCode::GATEWAY_TIMEOUT,
                        "This operation was aborted (20)".into(),
                        None,
                    ),
                };
            if !protected {
                let retry = chat_accounts::mark_unavailable(
                    storage,
                    PROVIDER,
                    connection,
                    model,
                    status.as_u16(),
                    &message,
                    None,
                )
                .await;
                if retry.is_ok_and(|retry| retry.should_fallback) {
                    excluded.insert(connection.to_owned());
                    last = Some((status, message));
                    continue;
                }
            }
            return Some(match response_headers {
                Some(headers) => transparent(
                    status,
                    headers,
                    Body::from_stream(futures_util::stream::once(async move {
                        Ok::<_, std::convert::Infallible>(Bytes::from(message))
                    })),
                ),
                None => plain(status, &message),
            });
        }
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .filter(|value| !value.is_empty())
}

fn inference_key(uri: &Uri, headers: &HeaderMap) -> Option<String> {
    if let Some(key) =
        header_text(headers, "authorization").and_then(|auth| auth.strip_prefix("Bearer "))
    {
        return Some(key.to_owned());
    }
    if let Some(key) = header_text(headers, "x-goog-api-key") {
        return Some(key.to_owned());
    }
    url::form_urlencoded::parse(uri.query()?.as_bytes())
        .find_map(|(name, value)| (name == "key").then(|| value.into_owned()))
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn protected_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
}

async fn routed_request(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    direct: &reqwest::Client,
    target: &Url,
    direct_target: &Url,
) -> Result<(reqwest::RequestBuilder, Option<Route>, bool), &'static str> {
    let unavailable = "Gemini TTS connection proxy is unavailable";
    let selection = crate::provider_http::select(storage, settings, account, target.as_str())
        .await
        .map_err(|_| unavailable)?;
    if matches!(selection, ProxySelection::Routed(Route::Direct))
        && !truthy(&settings["outboundProxyEnabled"])
        && !proxy_transport::account_requires_tunnel(account)
    {
        return Ok((direct.post(direct_target.clone()), None, false));
    }
    let credential = nonempty(&account["apiKey"]).or_else(|| nonempty(&account["accessToken"]));
    if !matches!(
        account["authType"].as_str(),
        Some("apikey" | "api_key" | "oauth")
    ) || credential.is_none_or(|token| !token.bytes().all(|byte| byte.is_ascii_graphic()))
    {
        return Err("Protected Gemini TTS requires a valid saved provider credential");
    }
    let protected = PROTECTED.get_or_init(|| {
        protected_client()
            .build()
            .expect("protected Gemini TTS HTTP client")
    });
    let (request, relay) = match selection {
        ProxySelection::ExplicitBypass => (protected.post(direct_target.clone()), None),
        ProxySelection::Routed(Route::Proxy { url, .. }) => {
            let proxy = proxy_transport::tunnel_proxy_url(&url).ok_or(unavailable)?;
            let client = proxy_transport::vendor_metadata_client(proxy, protected_client())
                .ok_or(unavailable)?;
            (client.post(target.clone()), None)
        }
        ProxySelection::Routed(
            relay @ Route::Relay {
                authorization: Some(_),
                ..
            },
        ) => {
            let Route::Relay { ref url, .. } = relay else {
                unreachable!()
            };
            for key in ["apiKey", "accessToken"]
                .into_iter()
                .filter_map(|field| nonempty(&account[field]))
            {
                if target.query_pairs().any(|(_, value)| value.contains(key)) {
                    return Err("Gemini TTS Relay destination contains a credential");
                }
            }
            (protected.post(url), Some(relay))
        }
        _ => return Err(unavailable),
    };
    Ok((request, relay, true))
}

fn upstream_url(uri: &Uri, model: &str, stream: bool, fixture: bool) -> Option<Url> {
    let base = if fixture {
        FIXTURE
    } else {
        gemini_request::BASE_URL
    };
    let action = if stream {
        "streamGenerateContent"
    } else {
        "generateContent"
    };
    let mut url = Url::parse(&format!("{base}/{model}:{action}")).ok()?;
    if let Some(query) = uri.query() {
        let mut query = url::form_urlencoded::parse(query.as_bytes())
            .filter(|(name, _)| name != "key")
            .peekable();
        if query.peek().is_some() {
            url.query_pairs_mut().extend_pairs(query);
        }
    }
    Some(url)
}

fn exhausted(
    last: Option<(StatusCode, String)>,
    code: Option<u16>,
    stored: Option<String>,
    accounts: &[Value],
) -> Response {
    let status = last
        .as_ref()
        .map(|(status, _)| *status)
        .or_else(|| StatusCode::from_u16(code?).ok())
        .unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    let message = last
        .map(|(_, message)| message)
        .filter(|text| !text.is_empty())
        .or(stored.filter(|text| !text.is_empty()))
        .unwrap_or_else(|| "No active credentials for provider: gemini".into());
    let message = accounts
        .iter()
        .fold(message, |text, account| redact(&text, account));
    plain(status, &message)
}

fn plain(status: StatusCode, message: &str) -> Response {
    json_error(status, json!({"error":{"message":message}}))
}

fn plain_with_code(status: StatusCode, message: &str) -> Response {
    json_error(
        status,
        json!({"error":{"message":message,"code":status.as_u16()}}),
    )
}

fn json_error(status: StatusCode, body: Value) -> Response {
    into_http(Ok(
        json!({"status":status.as_u16(),"headers":{"content-type":"application/json"},"body":body.to_string()}),
    ))
}

fn network_error(error: &reqwest::Error) -> String {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = cause {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            let code = match io.kind() {
                std::io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
                std::io::ErrorKind::ConnectionReset => "ECONNRESET",
                std::io::ErrorKind::TimedOut => "ETIMEDOUT",
                _ => "TypeError",
            };
            return format!("fetch failed ({code})");
        }
        cause = error.source();
    }
    "fetch failed (TypeError)".into()
}

fn redact(text: &str, account: &Value) -> String {
    ["apiKey", "accessToken", "refreshToken"]
        .into_iter()
        .filter_map(|field| nonempty(&account[field]))
        .filter(|secret| secret.encode_utf16().count() >= 8)
        .fold(redact_bearer(text), |safe, secret| {
            safe.replace(secret, "[redacted]")
        })
}

fn transparent(status: StatusCode, mut headers: HeaderMap, body: Body) -> Response {
    let mut hop_headers: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(','))
        .filter_map(|name| header::HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    hop_headers.extend([
        header::CONTENT_LENGTH,
        header::CONTENT_ENCODING,
        header::TRANSFER_ENCODING,
        header::CONNECTION,
        header::TE,
        header::TRAILER,
        header::UPGRADE,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
    ]);
    for name in hop_headers {
        headers.remove(name);
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    let compressible = header_text(&headers, "content-type")
        .map(|mime| {
            mime.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .is_some_and(|mime| {
            mime.starts_with("text/")
                || mime.ends_with("+json")
                || mime.ends_with("+xml")
                || matches!(
                    mime.as_str(),
                    "application/json" | "application/javascript" | "application/xml"
                )
        });
    let vary = header_text(&headers, "vary");
    let mut vary = vary.map_or_else(|| VARY.to_owned(), |upstream| format!("{VARY}, {upstream}"));
    if compressible
        && !vary
            .split(',')
            .any(|name| name.trim().eq_ignore_ascii_case("Accept-Encoding"))
    {
        vary.push_str(", Accept-Encoding");
    }
    if let Ok(value) = HeaderValue::from_str(&vary) {
        headers.insert(header::VARY, value);
    }
    let mut reply = Response::new(body);
    *reply.status_mut() = status;
    *reply.headers_mut() = headers;
    reply
}
