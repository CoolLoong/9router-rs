use std::collections::{HashMap, hash_map::Entry};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, TimeDelta, Utc};
use reqwest::{Client, ClientBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

use crate::dashboard_session::authorization_owner;
use crate::provider_connections::object;
use crate::provider_oauth::save_imported;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const STATE_PATH: &str = "/v2/plugin/auth/state";
const TOKEN_PATH: &str = "/v2/plugin/auth/token";
pub(crate) const INTL_REFRESH_URL: &str = "https://www.codebuddy.ai/v2/plugin/auth/token/refresh";
const INTERVAL: Duration = Duration::from_secs(5);
const TTL: Duration = Duration::from_secs(120);
const MAX_PENDING: usize = 512;
const RESPONSE_LIMIT: usize = 128 * 1024;

type Key = [u8; 32];
type Pending = Arc<Mutex<HashMap<Key, Attempt>>>;

#[derive(Clone)]
struct Device {
    edition: Edition,
    storage: Storage,
    client: Client,
    pending: Pending,
    fixture: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edition {
    China,
    International,
}

impl Edition {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::China => "codebuddy-cn",
            Self::International => "codebuddy-intl",
        }
    }

    const fn origin(self) -> &'static str {
        match self {
            Self::China => "https://copilot.tencent.com",
            Self::International => "https://www.codebuddy.ai",
        }
    }

    const fn region(self) -> &'static str {
        match self {
            Self::China => "cn",
            Self::International => "intl",
        }
    }

    const fn platform(self) -> &'static str {
        match self {
            Self::China => "CLI",
            Self::International => "ide",
        }
    }

    pub(crate) const fn refresh_url(self) -> &'static str {
        match self {
            Self::China => "https://copilot.tencent.com/v2/plugin/auth/token/refresh",
            Self::International => INTL_REFRESH_URL,
        }
    }

    pub(crate) const fn refresh_fixture(self) -> &'static str {
        match self {
            Self::China => {
                "http://127.0.0.1:52989/codebuddy-device/cn/v2/plugin/auth/token/refresh"
            }
            Self::International => {
                "http://127.0.0.1:52989/codebuddy-device/intl/v2/plugin/auth/token/refresh"
            }
        }
    }

    fn plugin_headers(self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let (agent, domain) = match self {
            Self::China => ("CLI/2.63.2 CodeBuddy/2.63.2", "copilot.tencent.com"),
            Self::International => ("IDE/2.63.2 CodeBuddy/2.63.2", "www.codebuddy.ai"),
        };
        request
            .header(header::USER_AGENT, agent)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("X-Domain", domain)
            .header("X-Product", "SaaS")
    }

    pub(crate) fn refresh_request(
        self,
        request: reqwest::RequestBuilder,
        refresh: &str,
    ) -> Option<reqwest::RequestBuilder> {
        let mut token = header::HeaderValue::from_str(refresh).ok()?;
        token.set_sensitive(true);
        Some(
            self.plugin_headers(request.json(&json!({})))
                .header(header::ACCEPT, "application/json")
                .header("X-Refresh-Token", token)
                .header("X-Auth-Refresh-Source", "plugin"),
        )
    }

    fn authorization_url(self, value: &str) -> Option<Url> {
        let url = Url::parse(value).ok()?;
        let allowed = match self {
            Self::China => url.host_str() == Some("copilot.tencent.com"),
            Self::International => {
                matches!(url.host_str(), Some("codebuddy.ai" | "www.codebuddy.ai"))
            }
        };
        (allowed
            && url.scheme() == "https"
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none())
        .then_some(url)
    }
}

struct Attempt {
    owner: Key,
    proof: Key,
    state: Arc<str>,
    expires: Instant,
    next_poll: Instant,
    polling: bool,
}

enum Endpoint<'a> {
    Register,
    Poll(&'a str),
}

pub fn router(storage: Storage) -> Router {
    let client = client_builder()
        .build()
        .expect("CodeBuddy OAuth HTTP client");
    let fixture = std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1");
    [Edition::China, Edition::International]
        .into_iter()
        .fold(Router::new(), |router, edition| {
            let root = format!("/api/oauth/{}", edition.id());
            router.merge(
                Router::new()
                    .route(&format!("{root}/device-code"), get(begin))
                    .route(&format!("{root}/poll"), post(poll))
                    .with_state(Device {
                        edition,
                        storage: storage.clone(),
                        client: client.clone(),
                        pending: Arc::default(),
                        fixture,
                    }),
            )
        })
}

async fn begin(State(state): State<Device>, headers: HeaderMap) -> Response {
    let Some(owner) = authorization_owner(&state.storage, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    match state.begin(owner).await {
        Some(payload) => reply(StatusCode::OK, payload),
        None => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "CodeBuddy state request failed",
        ),
    }
}

async fn poll(State(state): State<Device>, request: Request) -> Response {
    let Some(owner) = authorization_owner(&state.storage, request.headers()).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let Some(body) = to_bytes(request.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .filter(Value::is_object)
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid or empty request body");
    };
    let Some(handle) = text(&body["deviceCode"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing device code");
    };
    let Some(proof) = text(&body["codeVerifier"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing code verifier");
    };
    let (issuer_state, lease) = match state.claim(owner, hash(handle), hash(proof)) {
        Ok(claimed) => claimed,
        Err(code) => return reply(StatusCode::OK, poll_error(code)),
    };
    let result = tokio::spawn(async move { state.exchange(&issuer_state, lease).await })
        .await
        .unwrap_or_else(|_| poll_error("request_failed"));
    reply(StatusCode::OK, result)
}

impl Device {
    async fn begin(&self, owner: Key) -> Option<Value> {
        let data = self.send(Endpoint::Register).await.ok()?;
        let issuer_state = text(&data["data"]["state"]).filter(|_| data["code"] == 0)?;
        let verification = self
            .edition
            .authorization_url(text(&data["data"]["authUrl"])?)?;
        let handle = random()?;
        let proof = random()?;
        let now = Instant::now();
        let mut pending = entries(&self.pending);
        pending.retain(|_, attempt| attempt.polling || attempt.expires > now);
        if pending.len() >= MAX_PENDING {
            return None;
        }
        let Entry::Vacant(slot) = pending.entry(hash(&handle)) else {
            return None;
        };
        slot.insert(Attempt {
            owner,
            proof: hash(&proof),
            state: Arc::from(issuer_state),
            expires: now + TTL,
            next_poll: now + INTERVAL,
            polling: false,
        });
        Some(json!({
            "device_code":handle,"verification_uri":verification.as_str(),"user_code":"",
            "interval":INTERVAL.as_secs(),"expires_in":TTL.as_secs(),"codeVerifier":proof,"_isCodeBuddy":true,
        }))
    }

    fn claim(&self, owner: Key, key: Key, proof: Key) -> Result<(Arc<str>, Lease), &'static str> {
        let mut pending = entries(&self.pending);
        let now = Instant::now();
        let Some(attempt) = pending.get_mut(&key).filter(|attempt| {
            attempt.owner == owner
                && attempt.expires > now
                && bool::from(attempt.proof.ct_eq(&proof))
        }) else {
            return Err("expired_token");
        };
        if attempt.polling {
            return Err("authorization_pending");
        }
        if attempt.next_poll > now {
            return Err("slow_down");
        }
        attempt.polling = true;
        attempt.next_poll = now + INTERVAL;
        Ok((
            attempt.state.clone(),
            Lease {
                pending: self.pending.clone(),
                key,
                waiting: false,
            },
        ))
    }

    async fn exchange(&self, issuer_state: &str, mut lease: Lease) -> Value {
        let Ok(result) = self.send(Endpoint::Poll(issuer_state)).await else {
            return poll_error("request_failed");
        };
        if result["code"] == 11217 {
            lease.waiting = true;
            return poll_error("authorization_pending");
        }
        let data = &result["data"];
        if result["code"] == 0
            && let Some(token) = text(&data["accessToken"])
        {
            return self.save(token, data).await;
        }
        match text(&result["msg"]) {
            Some(code @ ("access_denied" | "expired_token" | "invalid_grant")) => poll_error(code),
            _ => poll_error("unknown_error"),
        }
    }

    async fn save(&self, token: &str, data: &Value) -> Value {
        let expires = &data["expiresIn"];
        let seconds = expires
            .as_i64()
            .or_else(|| expires.as_str()?.parse().ok())
            .filter(|seconds| *seconds > 0)
            .unwrap_or(86_400);
        let Some(time) =
            TimeDelta::try_seconds(seconds).and_then(|delta| Utc::now().checked_add_signed(delta))
        else {
            return poll_error("request_failed");
        };
        let fields = object(json!({
            "accessToken":token,"refreshToken":text(&data["refreshToken"]).unwrap_or_default(),
            "expiresIn":seconds,"expiresAt":time.to_rfc3339_opts(SecondsFormat::Millis, true),
            "providerSpecificData":{},
        }));
        let provider = self.edition.id();
        match save_imported(&self.storage, provider, fields).await {
            Ok(saved) => {
                json!({"success":true,"connection":{"id":saved["id"],"provider":provider}})
            }
            Err(_) => poll_error("request_failed"),
        }
    }

    async fn send(&self, step: Endpoint<'_>) -> Result<Value, ()> {
        let path = match step {
            Endpoint::Register => STATE_PATH,
            Endpoint::Poll(_) => TOKEN_PATH,
        };
        let target = format!("{}{path}", self.edition.origin());
        let (client, address) = if self.fixture {
            (
                self.client.clone(),
                format!(
                    "http://127.0.0.1:52989/codebuddy-device/{}{path}",
                    self.edition.region()
                ),
            )
        } else {
            let settings = self.storage.read_settings().await.ok_or(())?;
            let route =
                proxy_transport::select(&self.storage, &settings, &Value::Null, &target).await?;
            let client = match route {
                Route::Direct => self.client.clone(),
                Route::Proxy { url, .. } => client_builder()
                    .proxy(reqwest::Proxy::all(url).map_err(|_| ())?)
                    .build()
                    .map_err(|_| ())?,
                Route::Relay { .. } | Route::MissingStrict => return Err(()),
            };
            (client, target)
        };
        let mut request = match step {
            Endpoint::Register => client
                .post(address)
                .query(&[("platform", self.edition.platform())])
                .json(&json!({})),
            Endpoint::Poll(state) => client.get(address).query(&[("state", state)]),
        }
        .header(header::ACCEPT, "application/json");
        if self.edition == Edition::International {
            request = self
                .edition
                .plugin_headers(request)
                .header("X-No-Authorization", "true")
                .header("X-No-User-Id", "true");
            if matches!(step, Endpoint::Poll(_)) {
                request = request
                    .header("X-No-Enterprise-Id", "true")
                    .header("X-No-Department-Info", "true");
            }
        } else {
            request = request.header(header::USER_AGENT, "9Router");
        }
        let mut response = request.send().await.map_err(|_| ())?;
        if !response.status().is_success() {
            return Err(());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if chunk.len() > RESPONSE_LIMIT - bytes.len() {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| ())
    }
}

struct Lease {
    pending: Pending,
    key: Key,
    waiting: bool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut pending = entries(&self.pending);
        if self.waiting
            && let Some(attempt) = pending
                .get_mut(&self.key)
                .filter(|attempt| attempt.expires > Instant::now())
        {
            attempt.polling = false;
        } else {
            pending.remove(&self.key);
        }
    }
}

fn client_builder() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn entries(pending: &Pending) -> MutexGuard<'_, HashMap<Key, Attempt>> {
    pending.lock().unwrap_or_else(|error| error.into_inner())
}

fn random() -> Option<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

fn hash(input: &str) -> Key {
    Sha256::digest(input.as_bytes()).into()
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn poll_error(code: &str) -> Value {
    json!({"success":false,"error":code,"pending":matches!(code,"authorization_pending"|"slow_down")})
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}))
}

fn reply(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}
