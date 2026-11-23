use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::time::Instant;
use url::Url;
use uuid::Uuid;

use crate::catalog_guard::{is_direct_loopback, valid_cli_token};
use crate::dashboard_session::{authorization_owner, has_session};
use crate::mimo_account::valid_cookie;
use crate::provider_http::{Delivery, http_client};
use crate::provider_imports::save_xiaomi_web_session;
use crate::settings::reply;
use crate::storage::Storage;

const ACCOUNT: &str = "https://account.xiaomi.com";
const LOCAL: &str = "127.0.0.2";
const TAKEOVER: &str = "/__mimo_login/mimo";
const LIFETIME: Duration = Duration::from_secs(15 * 60);
const TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_LIMIT: usize = 2 * 1024 * 1024;
const RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
const ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'.')
    .remove(b'-')
    .remove(b'_')
    .remove(b'~');

#[derive(Clone)]
struct Manager {
    storage: Storage,
    sessions: Arc<Mutex<HashMap<Uuid, Arc<Session>>>>,
}

struct Session {
    owner: [u8; 32],
    until: Instant,
    region: &'static str,
    relay: Mutex<Relay>,
}

struct Relay {
    storage: Storage,
    origin: String,
    service: String,
    page: String,
    activation: Option<Uuid>,
    cookie_name: String,
    cookie: Uuid,
    jar: Vec<VendorCookie>,
    account_route: Option<Delivery>,
    service_route: Option<Delivery>,
    result: Option<Value>,
    stop: Option<oneshot::Sender<()>>,
}

struct VendorCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    host_only: bool,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/xiaomi-mimo/login/start", post(start))
        .route("/api/oauth/xiaomi-mimo/login/status", get(status))
        .route("/api/oauth/xiaomi-mimo/login/cancel", post(cancel))
        .with_state(Manager {
            storage,
            sessions: Arc::default(),
        })
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}), true)
}

async fn owner(storage: &Storage, headers: &HeaderMap, loopback: bool) -> Option<[u8; 32]> {
    if !loopback {
        return None;
    }
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let authority: axum::http::uri::Authority = host.parse().ok()?;
    if !matches!(authority.host(), "localhost" | "127.0.0.1" | "[::1]") {
        return None;
    }
    if !has_session(storage, headers) && !valid_cli_token(storage, headers) {
        return None;
    }
    authorization_owner(storage, headers).await
}

async fn body(request: Request) -> Option<Value> {
    let bytes = to_bytes(request.into_body(), 8192).await.ok()?;
    if bytes.is_empty() {
        Some(Value::Null)
    } else {
        serde_json::from_slice(&bytes).ok()
    }
}

async fn start(State(manager): State<Manager>, request: Request) -> Response {
    let Some(owner) = owner(
        &manager.storage,
        request.headers(),
        is_direct_loopback(&request),
    )
    .await
    else {
        return error(
            StatusCode::FORBIDDEN,
            "MiMo Desktop sign-in requires a local authenticated dashboard",
        );
    };
    let Some(input) = body(request).await else {
        return error(StatusCode::BAD_REQUEST, "Invalid MiMo sign-in request");
    };
    let region = match input["region"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "sgp" => "sgp",
        "ams" => "ams",
        "ru" => "ru",
        "in" => "in",
        _ => "cn",
    };
    let Ok(listener) = TcpListener::bind((LOCAL, 0)).await else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Could not start the isolated MiMo sign-in origin",
        );
    };
    let Ok(address) = listener.local_addr() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Could not read the MiMo sign-in address",
        );
    };
    let state = Uuid::new_v4();
    let launch = Uuid::new_v4();
    let origin = format!("http://{address}");
    let until = Instant::now() + LIFETIME;
    let (stop, stopped) = oneshot::channel();
    let session = Arc::new(Session {
        owner,
        until,
        region,
        relay: Mutex::new(Relay {
            storage: manager.storage.clone(),
            origin: origin.clone(),
            service: format!("https://mimo-server-{region}.xiaomimimo.com"),
            page: String::new(),
            activation: Some(launch),
            cookie_name: format!("mi_mo_relay_{}", address.port()),
            cookie: Uuid::new_v4(),
            jar: Vec::new(),
            account_route: None,
            service_route: None,
            result: None,
            stop: Some(stop),
        }),
    });
    {
        let mut sessions = manager.sessions.lock().await;
        sessions.retain(|_, session| session.until > Instant::now());
        if sessions.len() >= 16 {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many MiMo sign-in sessions",
            );
        }
        sessions.insert(state, session.clone());
    }
    let app = Router::new()
        .route("/__9r_mimo/launch/{token}", get(activate))
        .fallback(forward)
        .with_state(session);
    let sessions = manager.sessions.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = axum::serve(listener, app).into_future() => {},
            _ = tokio::time::sleep_until(until) => {},
            _ = stopped => {},
        }
        let mut sessions = sessions.lock().await;
        if let Some(session) = sessions.get(&state)
            && session.until <= Instant::now()
        {
            sessions.remove(&state);
        }
    });
    reply(
        StatusCode::OK,
        json!({"success":true,"state":state.to_string(),"region":region,"pageUrl":format!("{origin}/__9r_mimo/launch/{launch}")}),
        true,
    )
}

async fn lookup(
    manager: &Manager,
    owner: [u8; 32],
    input: Option<&str>,
) -> Option<(Uuid, Arc<Session>)> {
    let id = Uuid::parse_str(input?).ok()?;
    let sessions = manager.sessions.lock().await;
    let session = sessions
        .get(&id)
        .filter(|session| session.owner == owner && session.until > Instant::now())?;
    Some((id, session.clone()))
}

async fn status(
    State(manager): State<Manager>,
    Query(query): Query<HashMap<String, String>>,
    request: Request,
) -> Response {
    let Some(owner) = owner(
        &manager.storage,
        request.headers(),
        is_direct_loopback(&request),
    )
    .await
    else {
        return error(
            StatusCode::FORBIDDEN,
            "MiMo Desktop sign-in requires a local authenticated dashboard",
        );
    };
    let Some((_, session)) = lookup(&manager, owner, query.get("state").map(String::as_str)).await
    else {
        return reply(StatusCode::NOT_FOUND, json!({"status":"expired"}), true);
    };
    let mut relay = session.relay.lock().await;
    if let Some(value) = &relay.result {
        return reply(StatusCode::OK, value.clone(), true);
    }
    let Some((token, user, client_user)) = relay.identity() else {
        return reply(
            StatusCode::OK,
            json!({"status":"pending","region":session.region}),
            true,
        );
    };
    match save_xiaomi_web_session(&manager.storage, session.region, token, user, client_user).await
    {
        Ok(value) => {
            relay.jar.clear();
            relay.stop.take();
            let result =
                json!({"status":"done","region":session.region,"connection":value["connection"]});
            relay.result = Some(result.clone());
            reply(StatusCode::OK, result, true)
        }
        Err(()) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save the MiMo Desktop connection",
        ),
    }
}

async fn cancel(State(manager): State<Manager>, request: Request) -> Response {
    let Some(owner) = owner(
        &manager.storage,
        request.headers(),
        is_direct_loopback(&request),
    )
    .await
    else {
        return error(
            StatusCode::FORBIDDEN,
            "MiMo Desktop sign-in requires a local authenticated dashboard",
        );
    };
    let input = body(request).await.unwrap_or(Value::Null);
    let Some((id, session)) = lookup(&manager, owner, input["state"].as_str()).await else {
        return reply(StatusCode::NOT_FOUND, json!({"status":"expired"}), true);
    };
    {
        let mut relay = session.relay.lock().await;
        relay.jar.clear();
        relay.activation.take();
        relay.stop.take();
    }
    manager.sessions.lock().await.remove(&id);
    reply(StatusCode::OK, json!({"success":true}), true)
}

async fn activate(
    State(session): State<Arc<Session>>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    let mut relay = session.relay.lock().await;
    if session.until <= Instant::now()
        || !relay.host(&headers)
        || !Uuid::parse_str(&token)
            .ok()
            .is_some_and(|token| Some(token) == relay.activation)
    {
        return relay_response(
            StatusCode::FORBIDDEN,
            "MiMo sign-in link is invalid or expired",
        );
    }
    relay.activation.take();
    if relay.open_account().await.is_err() {
        relay.result = Some(
            json!({"status":"error","region":session.region,"error":"Could not open the Xiaomi account login"}),
        );
        return relay_response(
            StatusCode::BAD_GATEWAY,
            "Could not open the Xiaomi account login",
        );
    }
    let (Ok(location), Ok(cookie)) = (
        HeaderValue::from_str(&relay.page),
        HeaderValue::from_str(&format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=900",
            relay.cookie_name, relay.cookie
        )),
    ) else {
        return relay_response(StatusCode::BAD_GATEWAY, "Invalid MiMo sign-in redirect");
    };
    let mut response = relay_response(StatusCode::SEE_OTHER, "");
    response.headers_mut().insert(header::LOCATION, location);
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    response
}

async fn forward(State(session): State<Arc<Session>>, request: Request) -> Response {
    let mut relay = session.relay.lock().await;
    if session.until <= Instant::now()
        || relay.result.is_some()
        || !relay.host(request.headers())
        || !relay.browser_session(request.headers())
    {
        return relay_response(
            StatusCode::FORBIDDEN,
            "MiMo sign-in session is missing or expired",
        );
    }
    let method = request.method().clone();
    if !matches!(method, Method::GET | Method::HEAD | Method::POST)
        || method == Method::POST
            && request
                .headers()
                .get(header::ORIGIN)
                .and_then(|value| value.to_str().ok())
                != Some(relay.origin.as_str())
    {
        return relay_response(
            StatusCode::FORBIDDEN,
            "Invalid MiMo login request origin or method",
        );
    }
    if request.uri().path() == TAKEOVER || request.uri().path().starts_with(&format!("{TAKEOVER}/"))
    {
        return if relay.identity().is_some() {
            relay_response(StatusCode::OK, "Xiaomi account linked. Return to 9Router.")
        } else {
            relay_response(StatusCode::CONFLICT, "Xiaomi sign-in is still pending")
        };
    }
    match relay.proxy(request).await {
        Ok(response) => response,
        Err(()) => relay_response(StatusCode::BAD_GATEWAY, "Xiaomi sign-in request failed"),
    }
}

impl Relay {
    fn host(&self, headers: &HeaderMap) -> bool {
        self.origin.strip_prefix("http://").is_some_and(|expected| {
            headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                == Some(expected)
        })
    }

    fn browser_session(&self, headers: &HeaderMap) -> bool {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|part| part.trim().split_once('='))
            .any(|(name, value)| {
                name == self.cookie_name && Uuid::parse_str(value).ok() == Some(self.cookie)
            })
    }

    fn identity(&self) -> Option<(&str, &str, Option<&str>)> {
        let find = |name| {
            self.jar
                .iter()
                .rev()
                .find(|cookie| {
                    cookie.name == name
                        && domain_matches("account.xiaomi.com", &cookie.domain, cookie.host_only)
                })
                .map(|cookie| cookie.value.as_str())
        };
        Some((find("passToken")?, find("userId")?, find("cUserId")))
    }

    async fn open_account(&mut self) -> Result<(), ()> {
        let first = Url::parse(&format!("{}/api/user/xiaomi/me", self.service)).map_err(|_| ())?;
        let answer = self
            .fetch(Method::GET, &first, HeaderMap::new(), Vec::new())
            .await?;
        let second = redirect(&answer, &first)?;
        if !account_host(&second) || second.path() != "/pass/serviceLogin" {
            return Err(());
        }
        let answer = self
            .fetch(Method::GET, &second, HeaderMap::new(), Vec::new())
            .await?;
        let page = redirect(&answer, &second)?;
        if !account_host(&page) || !page.path().starts_with("/fe/") {
            return Err(());
        }
        self.page = self.rewrite(
            &page[url::Position::BeforePath..url::Position::AfterQuery],
            true,
        );
        Ok(())
    }

    async fn proxy(&mut self, request: Request) -> Result<Response, ()> {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path_and_query().ok_or(())?.as_str();
        if !path.starts_with('/') || path.starts_with("//") {
            return Err(());
        }
        let target =
            Url::parse(&format!("{ACCOUNT}{}", self.rewrite(path, false))).map_err(|_| ())?;
        if !account_host(&target) {
            return Err(());
        }
        let mut headers = HeaderMap::new();
        for name in [
            header::ACCEPT,
            header::ACCEPT_LANGUAGE,
            header::CONTENT_TYPE,
            header::HeaderName::from_static("x-requested-with"),
        ] {
            if let Some(value) = parts.headers.get(&name) {
                headers.insert(name, value.clone());
            }
        }
        if parts
            .headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            == Some(self.origin.as_str())
        {
            headers.insert(header::ORIGIN, HeaderValue::from_static(ACCOUNT));
        }
        if let Some(referer) = parts
            .headers
            .get(header::REFERER)
            .and_then(|value| value.to_str().ok())
            && referer
                .strip_prefix(&self.origin)
                .is_some_and(|suffix| suffix.starts_with('/'))
            && let Ok(value) = HeaderValue::from_str(&self.rewrite(referer, false))
        {
            headers.insert(header::REFERER, value);
        }
        let mut request_body = to_bytes(body, REQUEST_LIMIT)
            .await
            .map_err(|_| ())?
            .to_vec();
        if headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(textual)
            && let Ok(text) = std::str::from_utf8(&request_body)
        {
            request_body = self.rewrite(text, false).into_bytes();
        }
        let answer = self
            .fetch(parts.method, &target, headers, request_body)
            .await?;
        if answer.status().is_redirection() {
            let next = redirect(&answer, &target)?;
            if !secure(&next) {
                return Err(());
            }
            let mut response = relay_response(answer.status(), "");
            let location =
                HeaderValue::from_str(&self.rewrite(next.as_str(), true)).map_err(|_| ())?;
            response.headers_mut().insert(header::LOCATION, location);
            return Ok(response);
        }
        let status = answer.status();
        let content_type = answer.headers().get(header::CONTENT_TYPE).cloned();
        let mut bytes = bounded(answer).await?;
        if content_type
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .is_some_and(textual)
            && let Ok(text) = std::str::from_utf8(&bytes)
        {
            bytes = self.rewrite(text, true).into_bytes();
        }
        let mut response = relay_response(status, bytes);
        if let Some(value) = content_type {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
        Ok(response)
    }

    async fn fetch(
        &mut self,
        method: Method,
        url: &Url,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, ()> {
        let service = Url::parse(&self.service).map_err(|_| ())?;
        if !secure(url) || (!account_host(url) && url.origin() != service.origin()) {
            return Err(());
        }
        let settings = self.storage.read_settings().await.ok_or(())?;
        let route = Delivery::select(&self.storage, &settings, &Value::Null, url.as_str()).await?;
        let selected = if account_host(url) {
            &mut self.account_route
        } else {
            &mut self.service_route
        };
        if selected.as_ref().is_some_and(|previous| previous != &route) {
            return Err(());
        }
        *selected = Some(route);
        let direct = http_client(TIMEOUT).build().map_err(|_| ())?;
        let client = selected
            .as_ref()
            .and_then(|route| route.client(&direct, TIMEOUT))
            .ok_or(())?;
        let mut request = client
            .request(method, url.clone())
            .headers(headers)
            .header(header::USER_AGENT, "9Router");
        let cookie = self
            .jar
            .iter()
            .filter(|cookie| cookie.applies(url))
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; ");
        if !cookie.is_empty() {
            let mut header = HeaderValue::from_str(&cookie).map_err(|_| ())?;
            header.set_sensitive(true);
            request = request.header(header::COOKIE, header);
        }
        if !body.is_empty() {
            request = request.body(body);
        }
        let response = request.send().await.map_err(|_| ())?;
        for value in response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
        {
            if let Some((cookie, expired)) = VendorCookie::parse(url, value) {
                self.jar.retain(|old| {
                    old.name != cookie.name
                        || old.domain != cookie.domain
                        || old.path != cookie.path
                });
                if !expired && self.jar.len() < 96 {
                    self.jar.push(cookie);
                }
            }
        }
        Ok(response)
    }

    fn rewrite(&self, input: &str, to_browser: bool) -> String {
        let relay_service = format!("{}{TAKEOVER}", self.origin);
        let pairs = if to_browser {
            [
                (&self.service[..], relay_service.as_str()),
                (ACCOUNT, self.origin.as_str()),
            ]
        } else {
            [
                (relay_service.as_str(), self.service.as_str()),
                (self.origin.as_str(), ACCOUNT),
            ]
        };
        let mut output = input.to_owned();
        for (from, to) in pairs {
            let relative = from.split_once(':').map_or(from, |(_, suffix)| suffix);
            let variants = [
                (from.to_owned(), to.to_owned()),
                (relative.to_owned(), to.to_owned()),
                (from.replace('/', "\\/"), to.replace('/', "\\/")),
            ];
            for (mut from, mut to) in variants {
                for _ in 0..=3 {
                    output = output.replace(&from, &to);
                    from = utf8_percent_encode(&from, ENCODE).to_string();
                    to = utf8_percent_encode(&to, ENCODE).to_string();
                }
            }
        }
        output
    }
}

impl VendorCookie {
    fn parse(request: &Url, raw: &str) -> Option<(Self, bool)> {
        let host = request.host_str()?;
        let mut fields = raw.split(';');
        let (name, value) = fields.next()?.trim().split_once('=')?;
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return None;
        }
        let mut domain = host.to_owned();
        let mut host_only = true;
        let mut path = request
            .path()
            .rsplit_once('/')
            .map(|(prefix, _)| prefix)
            .filter(|prefix| !prefix.is_empty())
            .unwrap_or("/")
            .to_owned();
        let mut expired = !valid_cookie(value);
        for (name, value) in fields.filter_map(|field| field.trim().split_once('=')) {
            if name.eq_ignore_ascii_case("domain") {
                let claimed = value.trim_start_matches('.').to_ascii_lowercase();
                if !domain_matches(host, &claimed, false)
                    || !matches!(claimed.as_str(), "xiaomi.com" | "xiaomimimo.com")
                        && claimed != host
                {
                    return None;
                }
                domain = claimed;
                host_only = false;
            } else if name.eq_ignore_ascii_case("path") && value.starts_with('/') {
                path = value.to_owned();
            } else if name.eq_ignore_ascii_case("max-age") {
                expired |= value.parse::<i64>().is_ok_and(|seconds| seconds <= 0);
            } else if name.eq_ignore_ascii_case("expires") {
                expired |= chrono::DateTime::parse_from_rfc2822(value)
                    .is_ok_and(|time| time <= chrono::Utc::now());
            }
        }
        Some((
            Self {
                name: name.into(),
                value: value.into(),
                domain,
                path,
                host_only,
            },
            expired,
        ))
    }

    fn applies(&self, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| domain_matches(host, &self.domain, self.host_only))
            && url.path().strip_prefix(&self.path).is_some_and(|tail| {
                tail.is_empty() || self.path.ends_with('/') || tail.starts_with('/')
            })
    }
}

fn domain_matches(host: &str, domain: &str, host_only: bool) -> bool {
    host == domain
        || !host_only
            && !domain.is_empty()
            && host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
}

fn account_host(url: &Url) -> bool {
    secure(url) && url.host_str() == Some("account.xiaomi.com")
}

fn secure(url: &Url) -> bool {
    url.scheme() == "https"
        && url.has_host()
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn redirect(response: &reqwest::Response, original: &Url) -> Result<Url, ()> {
    if !response.status().is_redirection() {
        return Err(());
    }
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(())?;
    original.join(location).map_err(|_| ())
}

fn textual(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("javascript")
        || content_type.contains("urlencoded")
}

async fn bounded(mut response: reqwest::Response) -> Result<Vec<u8>, ()> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if chunk.len() > RESPONSE_LIMIT - bytes.len() {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn relay_response(status: StatusCode, body: impl IntoResponse) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
            (
                header::HeaderName::from_static("cross-origin-opener-policy"),
                "same-origin",
            ),
        ],
        body,
    )
        .into_response()
}
