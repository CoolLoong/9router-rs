use std::collections::{HashMap, hash_map::Entry};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, Utc};
use reqwest::{Client, ClientBuilder, RequestBuilder};
use rusqlite::TransactionBehavior;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;
use uuid::Uuid;

use super::{
    RefreshError,
    flow::{Provider, token_claims},
    store::expiry,
};
use crate::{
    dashboard_session::authorization_owner,
    provider_connections::{self, object},
    proxy_transport::{self, Route},
    storage::Storage,
};

#[path = "minimax_code_lease.rs"]
pub(crate) mod lease;
#[path = "minimax_code_usage.rs"]
pub(crate) mod usage;

pub(crate) const ATTEMPT: &str = "minimaxCodeRefreshAttempt";
pub(super) const CONTEXT: &[&str] = &[
    "proxyPoolId",
    "connectionProxyEnabled",
    "connectionProxyUrl",
    "connectionNoProxy",
    "strictProxy",
    "vercelRelayUrl",
];

const CLIENT_ID: &str = "mcode-public";
const SCOPE: &str = "agent.default";
const AUDIENCE: &str = "agent-backend";
const DEVICE_PATH: &str = "/oauth2/device/code";
const TOKEN_PATH: &str = "/oauth2/token";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const MAX_PENDING: usize = 256;
const MAX_RESPONSE: usize = 128 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Region {
    China,
    Global,
}

impl Region {
    pub(super) fn from_provider(provider: Provider) -> Option<Self> {
        match provider {
            Provider::MiniMaxCode => Some(Self::China),
            Provider::MiniMaxCodeGlobal => Some(Self::Global),
            _ => None,
        }
    }

    fn provider(self) -> &'static str {
        match self {
            Self::China => "minimax-code",
            Self::Global => "minimax-code-global",
        }
    }

    fn domain(self) -> &'static str {
        match self {
            Self::China => "minimax.cn",
            Self::Global => "minimax.io",
        }
    }

    fn verification(self, uri: &str) -> bool {
        Url::parse(uri).ok().is_some_and(|url| {
            let host = url.host_str().unwrap_or_default();
            url.scheme() == "https"
                && (host == self.domain()
                    || host == format!("account.{}", self.domain())
                    || host == format!("agent.{}", self.domain())
                    || host == format!("www.{}", self.domain()))
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        })
    }
}

type Key = [u8; 32];
type Pending = Arc<Mutex<HashMap<Uuid, Attempt>>>;

#[derive(Clone)]
struct Device {
    storage: Storage,
    client: Client,
    pending: Pending,
}

struct Attempt {
    owner: Key,
    proof: Key,
    region: Region,
    registration: Arc<Registration>,
    expires: Instant,
    next_poll: Instant,
    interval: Duration,
    polling: bool,
}

struct Registration {
    code: String,
    by_user: bool,
    verifier: String,
}

struct Lease {
    pending: Pending,
    nonce: Uuid,
    keep: bool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut pending = entries(&self.pending);
        if self.keep
            && let Some(attempt) = pending.get_mut(&self.nonce)
            && attempt.expires > Instant::now()
        {
            attempt.polling = false;
        } else {
            pending.remove(&self.nonce);
        }
    }
}

pub(super) fn router(storage: Storage) -> Router {
    let mut router = Router::new();
    for region in [Region::China, Region::Global] {
        let path = format!("/api/oauth/{}", region.provider());
        router = router
            .route(
                &format!("{path}/device-code"),
                get(move |state, headers| begin(state, headers, region)),
            )
            .route(
                &format!("{path}/poll"),
                post(move |state, request| poll(state, request, region)),
            )
            .route(&format!("{path}/lease-status"), get(lease_status))
            .route(
                &format!("{path}/lease-connect"),
                post(move |state, request| lease_connect(state, request, region)),
            );
    }
    router.with_state(Device {
        storage,
        client: client_builder()
            .build()
            .expect("MiniMax Code OAuth HTTP client"),
        pending: Arc::default(),
    })
}

fn lease_dir(value: Option<&str>) -> Result<std::path::PathBuf, lease::Error> {
    value.filter(|value| !value.is_empty()).map_or_else(
        || lease::data_dir(&Value::Null),
        |value| lease::normalize(std::path::Path::new(value.trim())),
    )
}

async fn lease_status(
    State(state): State<Device>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if authorization_owner(&state.storage, &headers)
        .await
        .is_none()
    {
        return reply(StatusCode::UNAUTHORIZED, json!({"error":"Unauthorized"}));
    }
    let requested = query.get("data_dir").map(String::as_str);
    let path = match lease_dir(requested) {
        Ok(path) => path,
        Err(error) => {
            return reply(
                StatusCode::OK,
                json!({
                    "found":false, "dataDir":requested.unwrap_or_default().trim(),
                    "error":error.code, "message":error.message,
                }),
            );
        }
    };
    let path_string = path.to_string_lossy();
    let result = match lease::status(&path).await {
        Ok(status) => {
            let mut result = json!({"found":true, "dataDir":path_string});
            for field in ["status", "generation", "expiresAtMs"] {
                if let Some(value) = status.get(field) {
                    result[field] = value.clone();
                }
            }
            result
        }
        Err(error) => json!({
            "found":false, "dataDir":path_string, "error":error.code, "message":error.message,
        }),
    };
    reply(StatusCode::OK, result)
}

async fn lease_connect(State(state): State<Device>, request: Request, region: Region) -> Response {
    if authorization_owner(&state.storage, request.headers())
        .await
        .is_none()
    {
        return reply(StatusCode::UNAUTHORIZED, json!({"error":"Unauthorized"}));
    }
    let Some(body) = to_bytes(request.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"Invalid or empty request body"}),
        );
    };
    let path = match lease_dir(body["dataDir"].as_str()) {
        Ok(path) => path,
        Err(error) => {
            return reply(
                StatusCode::CONFLICT,
                json!({"error":error.message, "code":error.code}),
            );
        }
    };
    if let Err(error) = lease::lease(&path).await {
        return reply(
            StatusCode::CONFLICT,
            json!({"error":error.message, "code":error.code}),
        );
    }
    let provider = region.provider();
    let name = match region {
        Region::China => "MiniMax Desktop (Lease)",
        Region::Global => "MiniMax Desktop (Lease, Global)",
    };
    let result = state.storage.run(true, move |db| {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut connections = provider_connections::read_all(&tx, Some(provider))?;
        if let Some(existing) = connections.iter().find(|account| {
            account["authType"] == "mcode-lease" && lease::data_dir(account).is_ok_and(|dir| dir == path)
        }) {
            return Ok(json!({"success":true, "reused":true, "connection":{"id":existing["id"], "provider":provider}}));
        }
        let now = provider_connections::now();
        let id = Uuid::new_v4().to_string();
        let priority = connections.drain(..).filter_map(|account| account["priority"].as_i64())
            .max().unwrap_or(0).saturating_add(1);
        provider_connections::write(&tx, &json!({
            "id":id, "provider":provider, "name":name, "authType":"mcode-lease",
            "isActive":true, "priority":priority, "testStatus":"active",
            "createdAt":now, "updatedAt":now,
            "providerSpecificData":{"mcodeDataDir":path.to_string_lossy()},
        }))?;
        provider_connections::reorder(&tx, provider)?;
        tx.commit()?;
        Ok(json!({"success":true,"connection":{"id":id,"provider":provider}}))
    }).await;
    match result {
        Ok(result) => reply(StatusCode::OK, result),
        Err(_) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"Could not save MiniMax Code connection"}),
        ),
    }
}

async fn begin(State(state): State<Device>, headers: HeaderMap, region: Region) -> Response {
    let Some(owner) = authorization_owner(&state.storage, &headers).await else {
        return reply(StatusCode::UNAUTHORIZED, json!({"error":"Unauthorized"}));
    };
    match state.begin(region, owner).await {
        Ok(result) => reply(StatusCode::OK, result),
        Err(()) => reply(
            StatusCode::BAD_GATEWAY,
            json!({"error":"Could not start MiniMax Code device authorization"}),
        ),
    }
}

async fn poll(State(state): State<Device>, request: Request, region: Region) -> Response {
    let Some(owner) = authorization_owner(&state.storage, request.headers()).await else {
        return reply(StatusCode::UNAUTHORIZED, json!({"error":"Unauthorized"}));
    };
    let Some(body) = to_bytes(request.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
    else {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"Invalid or empty request body"}),
        );
    };
    let Some(nonce) = field(&body["deviceCode"]) else {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"Missing device code"}),
        );
    };
    let Some(proof) = field(&body["codeVerifier"]) else {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"Missing code verifier"}),
        );
    };
    let Ok(nonce) = Uuid::parse_str(nonce) else {
        return reply(StatusCode::OK, failure("expired_token"));
    };
    let (registration, lease) = match state.claim(region, owner, nonce, digest(proof)) {
        Ok(result) => result,
        Err(code) => return reply(StatusCode::OK, failure(code)),
    };
    let result = tokio::spawn(async move { state.poll(region, &registration, lease).await })
        .await
        .unwrap_or_else(|_| failure("poll_failed"));
    reply(StatusCode::OK, result)
}

impl Device {
    async fn begin(&self, region: Region, owner: Key) -> Result<Value, ()> {
        let verifier = random()?;
        let challenge = URL_SAFE_NO_PAD.encode(digest(&verifier));
        let request = prepare(
            &self.storage,
            &self.client,
            &Value::Null,
            region,
            DEVICE_PATH,
            &[
                ("client_id", CLIENT_ID),
                ("scope", SCOPE),
                ("audience", AUDIENCE),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ],
        )
        .await?;
        let (status, body) = receive(request).await?;
        if !status.is_success() || field(&body["error"]).is_some() {
            return Err(());
        }
        let user = field(&body["user_code"]).ok_or(())?;
        let device = field(&body["device_code"]);
        let by_user = device.is_none();
        let verification = field(&body["verification_uri"])
            .or_else(|| field(&body["verification_url"]))
            .or_else(|| field(&body["verification_uri_complete"]))
            .filter(|uri| region.verification(uri))
            .ok_or(())?;
        let complete =
            field(&body["verification_uri_complete"]).filter(|uri| region.verification(uri));
        let elapsed = positive(&body["expired_in"]);
        let expires = positive(&body["expires_in"])
            .or_else(|| {
                elapsed.map(|value| {
                    if value < 1e12 {
                        value
                    } else {
                        ((value - Utc::now().timestamp_millis() as f64) / 1000.0).ceil()
                    }
                })
            })
            .filter(|value| *value > 0.0)
            .ok_or(())?
            .min(3600.0);
        if by_user && elapsed.is_none() {
            return Err(());
        }
        let interval = positive(&body["interval"])
            .map(|value| {
                if by_user && value >= 60.0 {
                    value / 1000.0
                } else {
                    value
                }
            })
            .unwrap_or(5.0)
            .clamp(1.0, 3600.0);
        let proof = random()?;
        let nonce = Uuid::new_v4();
        let registration = Registration {
            code: device.unwrap_or(user).to_owned(),
            by_user,
            verifier,
        };
        let now = Instant::now();
        let mut pending = entries(&self.pending);
        pending.retain(|_, attempt| attempt.polling || attempt.expires > now);
        if pending.len() >= MAX_PENDING {
            return Err(());
        }
        let Entry::Vacant(slot) = pending.entry(nonce) else {
            return Err(());
        };
        slot.insert(Attempt {
            owner,
            proof: digest(&proof),
            region,
            registration: Arc::new(registration),
            expires: now + Duration::from_secs_f64(expires),
            next_poll: now + Duration::from_secs_f64(interval),
            interval: Duration::from_secs_f64(interval),
            polling: false,
        });
        Ok(json!({
            "device_code":nonce.to_string(), "user_code":user, "verification_uri":verification,
            "verification_uri_complete":complete, "expires_in":expires, "interval":interval,
            "codeVerifier":proof, "_minimaxPollByUser":by_user,
        }))
    }

    fn claim(
        &self,
        region: Region,
        owner: Key,
        nonce: Uuid,
        proof: Key,
    ) -> Result<(Arc<Registration>, Lease), &'static str> {
        let mut pending = entries(&self.pending);
        let now = Instant::now();
        let Some(attempt) = pending.get_mut(&nonce).filter(|attempt| {
            attempt.region == region
                && bool::from(attempt.owner.ct_eq(&owner))
                && bool::from(attempt.proof.ct_eq(&proof))
                && attempt.expires > now
        }) else {
            return Err("expired_token");
        };
        if attempt.polling || attempt.next_poll > now {
            return Err("authorization_pending");
        }
        attempt.polling = true;
        Ok((
            attempt.registration.clone(),
            Lease {
                pending: self.pending.clone(),
                nonce,
                keep: false,
            },
        ))
    }

    async fn poll(&self, region: Region, registration: &Registration, mut lease: Lease) -> Value {
        let code = if registration.by_user {
            "user_code"
        } else {
            "device_code"
        };
        let request = prepare(
            &self.storage,
            &self.client,
            &Value::Null,
            region,
            TOKEN_PATH,
            &[
                ("grant_type", DEVICE_GRANT),
                (code, registration.code.as_str()),
                ("client_id", CLIENT_ID),
                ("code_verifier", registration.verifier.as_str()),
            ],
        )
        .await;
        let (status, data) = match request {
            Ok(request) => match receive(request).await {
                Ok(result) => result,
                Err(()) => return failure("poll_failed"),
            },
            Err(()) => return failure("poll_failed"),
        };
        let code = field(&data["error"]).or_else(|| field(&data["status"]));
        let problem = match code {
            Some("pending" | "authorization_pending") => Some("authorization_pending"),
            Some("slow_down") => Some("slow_down"),
            Some("denied" | "access_denied") => Some("access_denied"),
            Some("expired" | "expired_token") => Some("expired_token"),
            None if status == StatusCode::TOO_MANY_REQUESTS => Some("slow_down"),
            None | Some("success") if status.is_success() && field(&data["error"]).is_none() => {
                None
            }
            _ => Some("poll_failed"),
        };
        if let Some(problem) = problem {
            if matches!(problem, "authorization_pending" | "slow_down")
                && let Some(attempt) = entries(&lease.pending).get_mut(&lease.nonce)
            {
                if problem == "slow_down" {
                    attempt.interval += Duration::from_secs(5);
                }
                attempt.next_poll = Instant::now() + attempt.interval;
                lease.keep = true;
            }
            return failure(problem);
        }
        if entries(&lease.pending)
            .get(&lease.nonce)
            .is_none_or(|attempt| attempt.expires <= Instant::now())
        {
            return failure("expired_token");
        }
        let Some(fields) = grant_fields(&data, None) else {
            return failure("invalid_token_response");
        };
        let provider = region.provider();
        match super::save_imported(&self.storage, provider, fields).await {
            Ok(saved) => {
                json!({"success":true, "connection":{"id":saved["id"], "provider":provider}})
            }
            Err(()) => failure("exchange_failed"),
        }
    }
}

pub(super) async fn prepare_refresh(
    storage: &Storage,
    account: &Value,
    region: Region,
    token: &str,
) -> Result<RequestBuilder, RefreshError> {
    let client = client_builder()
        .build()
        .map_err(|_| RefreshError::ProviderUnavailable)?;
    prepare(
        storage,
        &client,
        account,
        region,
        TOKEN_PATH,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", token),
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("audience", AUDIENCE),
        ],
    )
    .await
    .map_err(|_| RefreshError::ProviderUnavailable)
}

pub(super) enum Rotation {
    Complete(Map<String, Value>),
    Throttled,
    Uncertain,
}

pub(super) async fn refresh(request: RequestBuilder, account: &Value) -> Rotation {
    let Ok((status, data)) = receive(request).await else {
        return Rotation::Uncertain;
    };
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Rotation::Throttled;
    }
    if !status.is_success() || field(&data["error"]).is_some() {
        return Rotation::Uncertain;
    }
    let Some(mut patch) = grant_fields(&data, field(&account["refreshToken"])) else {
        return Rotation::Uncertain;
    };
    patch.extend(object(json!({
        (ATTEMPT):Value::Null, "lastRefreshAt":Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    })));
    Rotation::Complete(patch)
}

pub(super) fn marker(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest(token))
}

fn grant_fields(data: &Value, old_refresh: Option<&str>) -> Option<Map<String, Value>> {
    let access = field(&data["access_token"])?;
    let refresh = field(&data["refresh_token"]).or(old_refresh)?;
    let _ = field(&data["token_type"]).filter(|token| token.eq_ignore_ascii_case("bearer"))?;
    let seconds = positive(&data["expires_in"])? as i64;
    if seconds <= 0 {
        return None;
    }
    let claims = token_claims(access);
    let scope = [&data["scope"], &claims["scope"], &claims["scp"]]
        .into_iter()
        .find(|field| !field.is_null())?;
    let has_scope = scope
        .as_str()
        .is_some_and(|list| list.split_ascii_whitespace().any(|name| name == SCOPE))
        || scope.as_array().is_some_and(|list| {
            list.iter().all(Value::is_string) && list.iter().any(|name| name == SCOPE)
        });
    if !has_scope {
        return None;
    }
    let expires = json!(seconds);
    let deadline = expiry(Utc::now(), Some(&expires)).ok()?;
    Some(object(json!({
        "accessToken":access, "refreshToken":refresh, "expiresIn":expires,
        "expiresAt":deadline,
    })))
}

async fn prepare(
    storage: &Storage,
    client: &Client,
    account: &Value,
    region: Region,
    path: &str,
    form: &[(&str, &str)],
) -> Result<RequestBuilder, ()> {
    let target = format!("https://account.{}{path}", region.domain());
    let (client, target) = if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
        (
            client.clone(),
            format!(
                "http://127.0.0.1:52989/minimax-code-oauth/{}{path}",
                region.provider()
            ),
        )
    } else {
        let settings = storage.read_settings().await.ok_or(())?;
        let client = match proxy_transport::select(storage, &settings, account, &target).await? {
            Route::Direct => client.clone(),
            Route::Proxy { url, .. } => client_builder()
                .proxy(reqwest::Proxy::all(url).map_err(|_| ())?)
                .build()
                .map_err(|_| ())?,
            Route::Relay { .. } | Route::MissingStrict => return Err(()),
        };
        (client, target)
    };
    Ok(client
        .post(target)
        .header(header::ACCEPT, "application/json")
        .form(form))
}

async fn receive(request: RequestBuilder) -> Result<(StatusCode, Value), ()> {
    let mut response = request.send().await.map_err(|_| ())?;
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if chunk.len() > MAX_RESPONSE - body.len() {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, serde_json::from_slice(&body).unwrap_or_default()))
}

fn client_builder() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn random() -> Result<String, ()> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| ())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn digest(value: &str) -> Key {
    Sha256::digest(value.as_bytes()).into()
}

fn field(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn positive(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
}

fn entries(pending: &Pending) -> MutexGuard<'_, HashMap<Uuid, Attempt>> {
    pending.lock().unwrap_or_else(|error| error.into_inner())
}

fn failure(code: &'static str) -> Value {
    json!({"success":false, "error":code, "pending":matches!(code, "authorization_pending" | "slow_down")})
}

fn reply(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}
