use std::time::Duration;

use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, RequestBuilder};
use serde_json::{Value, json};
use url::{Host, Url};

use super::{StateData, error, now, persist, public_pool, reply};

pub(super) const PRIVATE_FIELDS: [&str; 4] = [
    "_managedCloudflare",
    "_managedDeno",
    "_managedNetlify",
    "_managedVercel",
];
pub(super) const NETLIFY_FUNCTION_PATH: &str = "/.netlify/functions/relay";
pub(super) const VERCEL_FUNCTION_PATH: &str = "/api/relay";
const MAX_RESPONSE: usize = 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Platform {
    Cloudflare,
    Deno,
    Netlify,
    Vercel,
}

impl Platform {
    fn fields(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Cloudflare => ("cloudflare", ".workers.dev", PRIVATE_FIELDS[0]),
            Self::Deno => ("deno", ".deno.net", PRIVATE_FIELDS[1]),
            Self::Netlify => ("netlify", ".netlify.app", PRIVATE_FIELDS[2]),
            Self::Vercel => ("vercel", ".vercel.app", PRIVATE_FIELDS[3]),
        }
    }

    pub(super) fn url(self, value: &str) -> Option<Url> {
        let (_, suffix, _) = self.fields();
        let url = Url::parse(value).ok()?;
        let labels = url.host_str()?.strip_suffix(suffix)?;
        let path_matches = match self {
            Self::Cloudflare | Self::Deno => {
                labels
                    .split_once('.')
                    .is_some_and(|(app, organization)| dns_label(app) && dns_label(organization))
                    && url.path() == "/"
            }
            Self::Netlify => dns_label(labels) && url.path() == NETLIFY_FUNCTION_PATH,
            Self::Vercel => dns_label(labels) && url.path() == VERCEL_FUNCTION_PATH,
        };
        (public_target(&url) && path_matches && url.query().is_none()).then_some(url)
    }

    pub(super) fn request_url(self, address: &str) -> Option<String> {
        let url = self.url(address)?;
        let (kind, _, _) = self.fields();
        if fixture() {
            return Some(format!(
                "http://127.0.0.1:52989/{kind}-relay/{}",
                url.host_str()?
            ));
        }
        Some(address.to_owned())
    }
}

pub(super) fn fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

pub(super) fn dns_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub(crate) fn public_target(url: &Url) -> bool {
    let Some(Host::Domain(host)) = url.host() else {
        return false;
    };
    let Some((_, suffix)) = host.rsplit_once('.') else {
        return false;
    };
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && suffix.bytes().any(|byte| byte.is_ascii_lowercase())
        && !matches!(
            suffix,
            "localhost" | "local" | "internal" | "invalid" | "test" | "example" | "onion" | "arpa"
        )
        && host.split('.').all(dns_label)
}

pub(crate) fn managed(pool: &Value) -> bool {
    PRIVATE_FIELDS
        .iter()
        .any(|field| pool.get(*field).is_some())
}

pub(crate) fn validated_relay(pool: &Value, address: &str) -> Option<(String, HeaderValue)> {
    let platform = match pool["type"].as_str()? {
        "cloudflare" => Platform::Cloudflare,
        "deno" => Platform::Deno,
        "netlify" => Platform::Netlify,
        "vercel" => Platform::Vercel,
        _ => return None,
    };
    let (_, _, field) = platform.fields();
    if PRIVATE_FIELDS
        .iter()
        .any(|other| *other != field && pool.get(*other).is_some())
    {
        return None;
    }
    let bindings = &pool[field];
    if bindings["url"] != address {
        return None;
    }
    let token = bindings["token"].as_str().filter(|value| {
        value.len() == 43
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    })?;
    let mut header = HeaderValue::from_str(token).ok()?;
    header.set_sensitive(true);
    Some((platform.request_url(address)?, header))
}

pub(super) fn token() -> Option<String> {
    let mut secret = [0; 32];
    getrandom::fill(&mut secret).ok()?;
    Some(URL_SAFE_NO_PAD.encode(secret))
}

pub(super) fn api_client() -> Option<Client> {
    Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .ok()
}

pub(super) async fn request_json(request: RequestBuilder) -> Result<Value, StatusCode> {
    let mut response = request.send().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let status = response.status();
    if !status.is_success() {
        return Err(status);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?
    {
        if chunk.len() > MAX_RESPONSE - bytes.len() {
            return Err(StatusCode::BAD_GATEWAY);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_GATEWAY)
}

pub(super) fn api_error(status: StatusCode, message: &str) -> Response {
    let status = match status {
        StatusCode::BAD_REQUEST
        | StatusCode::UNAUTHORIZED
        | StatusCode::FORBIDDEN
        | StatusCode::CONFLICT
        | StatusCode::TOO_MANY_REQUESTS => status,
        _ => StatusCode::BAD_GATEWAY,
    };
    error(status, message)
}

pub(super) async fn save(
    state: StateData,
    platform: Platform,
    name: String,
    url: String,
    secret: String,
) -> Response {
    let (kind, _, private) = platform.fields();
    let time = now();
    let mut pool = json!({
        "id":uuid::Uuid::new_v4().to_string(),
        "name":name,"proxyUrl":url,"noProxy":"","type":kind,
        "isActive":true,"strictProxy":false,"testStatus":"unknown",
        "lastTestedAt":null,"lastError":null,"createdAt":time,"updatedAt":time,
    });
    pool[private] = json!({"url":url,"token":secret});
    match state
        .storage
        .run(true, move |db| {
            persist(db, &pool)?;
            Ok(public_pool(pool))
        })
        .await
    {
        Ok(pool) => reply(
            StatusCode::CREATED,
            json!({"proxyPool":pool,"deployUrl":url}),
            false,
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Relay deployed but the proxy pool could not be saved",
        ),
    }
}
