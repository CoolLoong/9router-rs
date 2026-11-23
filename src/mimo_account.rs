use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, Method, Response, StatusCode, header};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use url::Url;

use crate::provider_http::{Delivery, http_client};
use crate::storage::Storage;

const SSO_HOST: &str = "account.xiaomi.com";
const CHAT_PATH: &str = "/api/route/chat/completions";
const TTL: Duration = Duration::from_secs(30 * 60);
const TIMEOUT: Duration = Duration::from_secs(20);
const CACHE_LIMIT: usize = 128;
const SSO_LIMIT: usize = 128 * 1024;
static CLIENT: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());
type Slot = Arc<tokio::sync::Mutex<Option<Session>>>;
static CACHE: Mutex<VecDeque<([u8; 32], Slot)>> = Mutex::new(VecDeque::new());

#[derive(Clone)]
struct Session {
    cookie: header::HeaderValue,
    until: Instant,
}

pub(crate) enum Request<'a> {
    Chat { body: &'a Value, stream: bool },
    Usage,
    Test,
}

pub(crate) enum Error {
    Missing,
    Route,
    Rejected,
    Unavailable,
}

impl Error {
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Missing => "Xiaomi MiMo account session is missing",
            Self::Route => "Xiaomi MiMo session cannot use the configured route",
            Self::Rejected => "Xiaomi MiMo account session was rejected",
            Self::Unavailable => "Xiaomi MiMo account service is unavailable",
        }
    }
}

pub(crate) fn account_model(model: &str) -> bool {
    let model = model.rsplit('/').next().unwrap_or_default();
    matches!(
        model,
        "mimo-v2.6-pro" | "mimo-v2.6-flash" | "mimo-v2.6-pro-ultraspeed"
    )
}

pub(crate) fn has_session(account: &Value) -> bool {
    cookie_value(&account["providerSpecificData"]["mimoPassToken"]).is_some()
}

pub(crate) fn chat_preview(account: &Value, body: Value, stream: bool) -> Value {
    let (region, _) = cluster(
        account["providerSpecificData"]["region"]
            .as_str()
            .unwrap_or_default(),
    );
    json!({"url":format!("https://mimo-server-{region}.xiaomimimo.com{CHAT_PATH}"),"headers":{
        "Content-Type":"application/json", "Accept":chat_accept(stream), "User-Agent":"9Router", "Cookie":"[redacted]",
    },"body":body})
}

fn chat_accept(stream: bool) -> &'static str {
    if stream {
        "text/event-stream"
    } else {
        "application/json"
    }
}

pub(crate) async fn test(storage: &Storage, account: &Value) -> (Option<String>, bool) {
    let Some(settings) = storage.read_settings().await else {
        return (Some(Error::Route.message().to_owned()), true);
    };
    let result = send(storage, &settings, account, Request::Test).await;
    match result {
        Ok(response) if response.status().is_success() => (None, false),
        Ok(response)
            if matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) =>
        {
            (Some(Error::Rejected.message().to_owned()), false)
        }
        Ok(_) => (Some(Error::Unavailable.message().to_owned()), true),
        Err(error) => {
            let soft = matches!(error, Error::Route | Error::Unavailable);
            (Some(error.message().to_owned()), soft)
        }
    }
}

pub(crate) async fn send(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    request: Request<'_>,
) -> Result<Response, Error> {
    let data = &account["providerSpecificData"];
    let pass = cookie_value(&data["mimoPassToken"]).ok_or(Error::Missing)?;
    let mut saved = Vec::new();
    if let Some(user) = cookie_id(&data["mimoUserId"]) {
        saved.push(("userId".to_owned(), user));
    }
    saved.push(("passToken".to_owned(), pass.to_owned()));
    if let Some(user) = cookie_id(&data["mimoCUserId"]) {
        saved.push(("cUserId".to_owned(), user));
    }
    let (region, sid) = cluster(data["region"].as_str().unwrap_or_default());
    let host = format!("mimo-server-{region}.xiaomimimo.com");
    let path = match request {
        Request::Chat { .. } => CHAT_PATH,
        Request::Usage => "/api/user/usage",
        Request::Test => "/api/user/xiaomi/me",
    };
    let endpoint = Url::parse(&format!("https://{host}{path}")).map_err(|_| Error::Route)?;
    let mut sso =
        Url::parse(&format!("https://{SSO_HOST}/pass/serviceLogin")).map_err(|_| Error::Route)?;
    sso.query_pairs_mut()
        .append_pair("_locale", "zh_CN")
        .append_pair("_snsNone", "true")
        .append_pair("sid", sid)
        .append_pair("_json", "true");
    let (sso_route, service_route) = tokio::try_join!(
        select(storage, settings, account, &sso),
        select(storage, settings, account, &endpoint),
    )?;
    let link = Link {
        storage,
        account,
        region,
        host,
        sso_route,
        service_route,
    };
    let mut hash = Sha256::new();
    hash.update(storage.root().as_os_str().as_encoded_bytes());
    for item in [
        region,
        delivery_key(&link.sso_route),
        delivery_key(&link.service_route),
    ] {
        hash.update([0]);
        hash.update(item);
    }
    for (name, value) in &saved {
        hash.update([0]);
        hash.update(name);
        hash.update([0]);
        hash.update(value);
    }
    let slot = slot(hash.finalize().into())?;
    let credential = {
        let mut entry = slot.lock().await;
        session(&mut entry, &link, &sso, sid, &saved).await?
    };
    let response = link
        .operation(&endpoint, &request, credential.cookie)
        .await?;
    if response.status() != StatusCode::UNAUTHORIZED || !matches!(request, Request::Chat { .. }) {
        return Ok(response);
    }
    let renewed = {
        let mut entry = slot.lock().await;
        if entry
            .as_ref()
            .is_some_and(|current| current.until == credential.until)
        {
            *entry = None;
        }
        session(&mut entry, &link, &sso, sid, &saved).await
    };
    match renewed {
        Ok(credential) => link.operation(&endpoint, &request, credential.cookie).await,
        Err(Error::Route) => Err(Error::Route),
        Err(_) => Ok(response),
    }
}

fn cluster(region: &str) -> (&'static str, &'static str) {
    match region.to_ascii_lowercase().as_str() {
        "cn" => ("cn", "mimopc"),
        "ams" => ("ams", "mimoams"),
        "ru" => ("ru", "mimoru"),
        "in" => ("in", "mimoin"),
        _ => ("sgp", "mimosgp"),
    }
}

fn cookie_value(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| valid_cookie(value))
}

fn cookie_id(value: &Value) -> Option<String> {
    cookie_value(value)
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

pub(crate) fn valid_cookie(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8192
        && value != "EXPIRED"
        && value.bytes().all(
            |byte| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e),
        )
}

fn cookie_header(values: &[(String, String)]) -> Result<header::HeaderValue, Error> {
    let serialized = values
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut value = header::HeaderValue::from_str(&serialized).map_err(|_| Error::Rejected)?;
    value.set_sensitive(true);
    Ok(value)
}

fn delivery_key(delivery: &Delivery) -> &str {
    match delivery {
        Delivery::Direct => "direct",
        Delivery::Bypass => "bypass",
        Delivery::Tunnel(url) => url.as_str(),
    }
}

async fn select(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    url: &Url,
) -> Result<Delivery, Error> {
    Delivery::select(storage, settings, account, url.as_str())
        .await
        .map_err(|_| Error::Route)
}

struct Link<'a> {
    storage: &'a Storage,
    account: &'a Value,
    region: &'static str,
    host: String,
    sso_route: Delivery,
    service_route: Delivery,
}

impl Link<'_> {
    async fn request(
        &self,
        method: Method,
        target: &Url,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let lane = match target.host_str() {
            Some(SSO_HOST) => (&self.sso_route, "sso"),
            Some(host) if host == self.host => (&self.service_route, self.region),
            _ => return Err(Error::Route),
        };
        if target.scheme() != "https"
            || target.port().is_some()
            || !target.username().is_empty()
            || target.password().is_some()
            || target.fragment().is_some()
        {
            return Err(Error::Route);
        }
        let current = self.storage.read_settings().await.ok_or(Error::Route)?;
        if &select(self.storage, &current, self.account, target).await? != lane.0 {
            return Err(Error::Route);
        }
        let client = lane
            .0
            .client(CLIENT.as_ref().ok_or(Error::Unavailable)?, TIMEOUT)
            .ok_or(Error::Route)?;
        let destination = if lane.0.use_fixture() {
            let mut url = Url::parse(&format!(
                "http://127.0.0.1:52989/mimo-account/{}{}",
                lane.1,
                target.path()
            ))
            .map_err(|_| Error::Route)?;
            url.set_query(target.query());
            url
        } else {
            target.clone()
        };
        Ok(client
            .request(method, destination)
            .header(header::USER_AGENT, "9Router"))
    }

    async fn operation(
        &self,
        endpoint: &Url,
        request: &Request<'_>,
        cookie: header::HeaderValue,
    ) -> Result<Response, Error> {
        let method = if matches!(request, Request::Chat { .. }) {
            Method::POST
        } else {
            Method::GET
        };
        let builder = self
            .request(method, endpoint)
            .await?
            .header(header::COOKIE, cookie);
        let builder = match request {
            Request::Chat { body, stream } => builder
                .header(header::ACCEPT, chat_accept(*stream))
                .json(body),
            Request::Usage => builder
                .timeout(Duration::from_secs(10))
                .header(header::ACCEPT, "application/json"),
            Request::Test => builder.header(header::ACCEPT, "application/json"),
        };
        let response = builder.send().await.map_err(|_| Error::Unavailable)?;
        if response.status().is_redirection() {
            return Err(Error::Route);
        }
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let mut output = axum::http::Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(retry_after) = response.headers().get(header::RETRY_AFTER).filter(|value| {
            value.to_str().is_ok_and(|text| {
                text.parse::<u64>().is_ok() || chrono::DateTime::parse_from_rfc2822(text).is_ok()
            })
        }) {
            output = output.header(header::RETRY_AFTER, retry_after.clone());
        }
        let reason = if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            Error::Rejected.message()
        } else {
            Error::Unavailable.message()
        };
        output
            .body(json!({"error":{"message":reason}}).to_string())
            .map(Response::from)
            .map_err(|_| Error::Unavailable)
    }
}

fn slot(key: [u8; 32]) -> Result<Slot, Error> {
    let mut cache = CACHE.lock().map_err(|_| Error::Unavailable)?;
    if let Some(index) = cache.iter().position(|(candidate, _)| candidate == &key)
        && let Some(entry) = cache.remove(index)
    {
        let result = entry.1.clone();
        cache.push_back(entry);
        return Ok(result);
    }
    if cache.len() >= CACHE_LIMIT {
        let index = cache
            .iter()
            .position(|(_, slot)| Arc::strong_count(slot) == 1)
            .ok_or(Error::Unavailable)?;
        cache.remove(index);
    }
    let slot = Arc::new(tokio::sync::Mutex::new(None));
    cache.push_back((key, slot.clone()));
    Ok(slot)
}

async fn session(
    entry: &mut Option<Session>,
    link: &Link<'_>,
    sso: &Url,
    sid: &str,
    saved: &[(String, String)],
) -> Result<Session, Error> {
    if let Some(entry) = entry.as_ref().filter(|entry| entry.until > Instant::now()) {
        return Ok(entry.clone());
    }
    let cookie = tokio::time::timeout(TIMEOUT, acquire(link, sso, sid, saved))
        .await
        .map_err(|_| Error::Unavailable)??;
    let credential = Session {
        cookie,
        until: Instant::now() + TTL,
    };
    *entry = Some(credential.clone());
    Ok(credential)
}

async fn acquire(
    link: &Link<'_>,
    sso: &Url,
    sid: &str,
    saved: &[(String, String)],
) -> Result<header::HeaderValue, Error> {
    let response = link
        .request(Method::GET, sso)
        .await?
        .header(header::COOKIE, cookie_header(saved)?)
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| Error::Unavailable)?;
    handshake_status(response.status())?;
    let mut jar = saved.to_vec();
    jar.retain(|(name, _)| name == "userId");
    absorb(&mut jar, response.headers(), sid)?;
    let bytes = bounded(response).await?;
    let body: Value = serde_json::from_slice(bytes.strip_prefix(b"&&&START&&&").unwrap_or(&bytes))
        .map_err(|_| Error::Rejected)?;
    if body["code"] != 0 || body["bSecondValidation"] == true {
        return Err(Error::Rejected);
    }
    let nonce = body["nonce"]
        .as_u64()
        .filter(|number| *number > 0)
        .map(|nonce| nonce.to_string())
        .or_else(|| {
            body["nonce"]
                .as_str()
                .filter(|nonce| {
                    !nonce.is_empty() && nonce.bytes().all(|byte| byte.is_ascii_digit())
                })
                .map(str::to_owned)
        })
        .ok_or(Error::Rejected)?;
    let security = body["ssecurity"]
        .as_str()
        .filter(|secret| !secret.is_empty() && secret.len() <= 8192)
        .ok_or(Error::Rejected)?;
    let mut target = sso
        .join(
            body["location"]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or(Error::Rejected)?,
        )
        .map_err(|_| Error::Rejected)?;
    if target.query_pairs().any(|(key, _)| key == "clientSign") {
        return Err(Error::Rejected);
    }
    let mut input = format!("nonce={nonce}");
    if !security.trim().is_empty() {
        input.push('&');
        input.push_str(security);
    }
    target.query_pairs_mut().append_pair(
        "clientSign",
        &STANDARD.encode(Sha1::digest(input.as_bytes())),
    );
    for _ in 0..8 {
        let response = link
            .request(Method::GET, &target)
            .await?
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        absorb(&mut jar, response.headers(), sid)?;
        if response.status().is_redirection() {
            let next = response
                .headers()
                .get(header::LOCATION)
                .and_then(|location| location.to_str().ok())
                .ok_or(Error::Rejected)?;
            target = target.join(next).map_err(|_| Error::Rejected)?;
            continue;
        }
        handshake_status(response.status())?;
        if !jar.iter().any(|(name, _)| name == "serviceToken") {
            return Err(Error::Rejected);
        }
        return cookie_header(&jar);
    }
    Err(Error::Rejected)
}

fn handshake_status(status: StatusCode) -> Result<(), Error> {
    match status {
        status if status.is_success() => Ok(()),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::Rejected),
        status if status.is_redirection() => Err(Error::Route),
        _ => Err(Error::Unavailable),
    }
}

fn absorb(
    jar: &mut Vec<(String, String)>,
    headers: &header::HeaderMap,
    sid: &str,
) -> Result<(), Error> {
    let scoped = format!("{sid}_serviceToken");
    for item in headers.get_all(header::SET_COOKIE) {
        let Ok(value) = item.to_str() else {
            continue;
        };
        let mut parts = value.split(';');
        let Some((name, value)) = parts.next().and_then(|part| part.trim().split_once('=')) else {
            continue;
        };
        let name = if name == scoped { "serviceToken" } else { name };
        if !matches!(name, "serviceToken" | "userId")
            && !name.ends_with("_ph")
            && !name.ends_with("_slh")
        {
            continue;
        }
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || name.len() > 64
        {
            continue;
        }
        let expired = parts
            .filter_map(|part| part.trim().split_once('='))
            .any(|(key, value)| {
                key.eq_ignore_ascii_case("max-age")
                    && value.trim().parse::<i64>().is_ok_and(|value| value <= 0)
            });
        jar.retain(|(key, _)| key != name);
        if !expired && valid_cookie(value) {
            if jar.len() >= 16 {
                return Err(Error::Rejected);
            }
            jar.push((name.to_owned(), value.to_owned()));
        }
    }
    Ok(())
}

async fn bounded(mut response: Response) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
        if chunk.len() > SSO_LIMIT - bytes.len() {
            return Err(Error::Rejected);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
