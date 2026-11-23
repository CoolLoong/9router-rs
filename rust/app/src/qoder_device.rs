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
use chrono::{DateTime, NaiveDate, SecondsFormat, TimeDelta, Utc};
use reqwest::{Client, ClientBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;
use uuid::Uuid;

use crate::dashboard_session::authorization_owner;
use crate::provider_connections::object;
use crate::provider_index::js_trim;
use crate::provider_oauth::save_imported;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const TTL: Duration = Duration::from_secs(300);
const INTERVAL: Duration = Duration::from_secs(2);
const MAX_PENDING: usize = 512;
const RESPONSE_LIMIT: usize = 128 * 1024;
const DAY_SECONDS: i64 = 24 * 60 * 60;
const TOKEN_PATH: &str = "/api/v1/deviceToken/poll";
const PROFILE_PATH: &str = "/api/v1/userinfo";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Region {
    International,
    China,
}

impl Region {
    fn provider(self) -> &'static str {
        match self {
            Self::International => "qoder",
            Self::China => "qoder-cn",
        }
    }

    fn login(self) -> &'static str {
        match self {
            Self::International => "https://qoder.com/device/selectAccounts",
            Self::China => "https://qoder.com.cn/device/selectAccounts",
        }
    }

    fn api(self) -> &'static str {
        match self {
            Self::International => "https://openapi.qoder.sh",
            Self::China => "https://openapi.qoder.com.cn",
        }
    }
}

type Key = [u8; 32];
type Pending = Arc<Mutex<HashMap<Uuid, Attempt>>>;

#[derive(Clone)]
struct Device {
    storage: Storage,
    client: Client,
    pending: Pending,
    fixture: bool,
}

struct Attempt {
    owner: Key,
    proof: Key,
    region: Region,
    registration: Arc<Registration>,
    expires: Instant,
    next_poll: Instant,
    polling: bool,
}

struct Registration {
    nonce: String,
    verifier: String,
    machine: String,
}

pub fn router(storage: Storage) -> Router {
    let mut router = Router::new();
    for region in [Region::International, Region::China] {
        let path = format!("/api/oauth/{}", region.provider());
        router = router
            .route(
                &format!("{path}/device-code"),
                get(move |state, headers| begin(state, headers, region)),
            )
            .route(
                &format!("{path}/poll"),
                post(move |state, request| poll(state, request, region)),
            );
    }
    router.with_state(Device {
        storage,
        client: client_builder().build().expect("Qoder OAuth HTTP client"),
        pending: Arc::default(),
        fixture: std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"),
    })
}

async fn begin(State(state): State<Device>, headers: HeaderMap, region: Region) -> Response {
    let Some(owner) = authorization_owner(&state.storage, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    match state.begin(region, owner) {
        Some(body) => reply(StatusCode::OK, body),
        None => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not prepare Qoder device authorization",
        ),
    }
}

async fn poll(State(state): State<Device>, request: Request, region: Region) -> Response {
    let Some(owner) = authorization_owner(&state.storage, request.headers()).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let Some(body) = to_bytes(request.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid or empty request body");
    };
    let Some(nonce) = text(&body["deviceCode"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing device code");
    };
    let Some(proof) = text(&body["codeVerifier"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing code verifier");
    };
    let Ok(nonce) = Uuid::parse_str(nonce) else {
        return reply(StatusCode::OK, failure("expired_token", None));
    };
    let (registration, lease) = match state.claim(region, owner, nonce, hash(proof)) {
        Ok(result) => result,
        Err(code) => return reply(StatusCode::OK, failure(code, None)),
    };
    let result = tokio::spawn(async move { state.poll(region, &registration, lease).await })
        .await
        .unwrap_or_else(|_| {
            failure(
                "poll_failed",
                Some("Could not complete Qoder device authorization"),
            )
        });
    reply(StatusCode::OK, result)
}

impl Device {
    fn begin(&self, region: Region, owner: Key) -> Option<Value> {
        let verifier = random()?;
        let proof = random()?;
        let nonce = Uuid::new_v4();
        let registration = Registration {
            nonce: nonce.to_string(),
            verifier,
            machine: Uuid::new_v4().to_string(),
        };
        let challenge = URL_SAFE_NO_PAD.encode(hash(&registration.verifier));
        let mut url = Url::parse(region.login()).ok()?;
        url.query_pairs_mut()
            .append_pair("challenge", &challenge)
            .append_pair("challenge_method", "S256")
            .append_pair("machine_id", &registration.machine)
            .append_pair("nonce", &registration.nonce);
        let body = json!({
            "device_code":registration.nonce, "user_code":registration.nonce[..8].to_ascii_uppercase(),
            "verification_uri":region.login(), "verification_uri_complete":url.as_str(),
            "expires_in":TTL.as_secs(), "interval":INTERVAL.as_secs(), "codeVerifier":proof,
            "_qoderNonce":registration.nonce, "_qoderMachineId":registration.machine,
        });
        let now = Instant::now();
        let mut pending = entries(&self.pending);
        pending.retain(|_, attempt| attempt.polling || attempt.expires > now);
        if pending.len() >= MAX_PENDING {
            return None;
        }
        let Entry::Vacant(slot) = pending.entry(nonce) else {
            return None;
        };
        slot.insert(Attempt {
            owner,
            proof: hash(&proof),
            region,
            registration: Arc::new(registration),
            expires: now + TTL,
            next_poll: now + INTERVAL,
            polling: false,
        });
        Some(body)
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
            attempt.owner == owner
                && attempt.region == region
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
            attempt.registration.clone(),
            Lease {
                pending: self.pending.clone(),
                nonce,
                keep: false,
            },
        ))
    }

    async fn poll(&self, region: Region, registration: &Registration, mut lease: Lease) -> Value {
        let poll = self
            .send(
                region,
                TOKEN_PATH,
                None,
                &[
                    ("nonce", &registration.nonce),
                    ("verifier", &registration.verifier),
                    ("challenge_method", "S256"),
                ],
            )
            .await;
        let Ok((status, data)) = poll else {
            return failure(
                "poll_failed",
                Some("Could not poll for a Qoder device token"),
            );
        };
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NOT_FOUND) {
            lease.keep = true;
            return failure("authorization_pending", None);
        }
        if !status.is_success() {
            let description = format!("Qoder device token poll failed: HTTP {}", status.as_u16());
            return failure("poll_failed", Some(&description));
        }
        let Some(access) = text(&data["token"]) else {
            return failure(
                "poll_failed",
                Some("Qoder device token poll returned 200 but no token"),
            );
        };
        let hint = expiry_ms(
            &data["expires_at"],
            &data["expires_in"],
            Utc::now().timestamp_millis(),
        );
        let profile = match self.send(region, PROFILE_PATH, Some(access), &[]).await {
            Ok((status, profile)) if status.is_success() => profile,
            _ => Value::Null,
        };
        self.save(region, registration, access, &data, &profile, hint)
            .await
    }

    async fn save(
        &self,
        region: Region,
        registration: &Registration,
        access: &str,
        data: &Value,
        profile: &Value,
        expiry_ms: i64,
    ) -> Value {
        let now = Utc::now();
        let seconds = expiry_ms
            .saturating_sub(now.timestamp_millis())
            .div_euclid(1000)
            .max(DAY_SECONDS);
        let Some(expiration) =
            TimeDelta::try_seconds(seconds).and_then(|delta| now.checked_add_signed(delta))
        else {
            return failure("exchange_failed", Some("Invalid Qoder token expiration"));
        };
        let user = &data["user_id"];
        let user_id = match user {
            Value::String(value) if !value.is_empty() => Some(value.clone()),
            Value::Number(value) if value.as_f64() != Some(0.0) => Some(value.to_string()),
            _ => None,
        };
        let email = trimmed(&profile["email"]).map(str::to_owned).or_else(|| {
            user_id
                .as_ref()
                .map(|user_id| format!("qoder-user-{user_id}"))
        });
        let name = text(&profile["name"])
            .or_else(|| text(&profile["username"]))
            .map(js_trim)
            .filter(|value| !value.is_empty());
        let fields = object(json!({
            "accessToken":access, "refreshToken":text(&data["refresh_token"]), "email":email, "displayName":name,
            "expiresIn":seconds, "expiresAt":expiration.to_rfc3339_opts(SecondsFormat::Millis, true),
            "providerSpecificData":{
                "authMethod":"device", "userId":if user_id.is_some() { user.clone() } else { json!("") },
                "machineId":registration.machine, "organizationId":trimmed(&profile["organization_id"]).unwrap_or_default(),
            },
        }));
        let provider = region.provider();
        match save_imported(&self.storage, provider, fields).await {
            Ok(saved) => {
                json!({"success":true,"connection":{"id":saved["id"],"provider":provider}})
            }
            Err(_) => failure(
                "exchange_failed",
                Some("Could not save the provider connection"),
            ),
        }
    }

    async fn send(
        &self,
        region: Region,
        path: &str,
        bearer: Option<&str>,
        query: &[(&str, &str)],
    ) -> Result<(StatusCode, Value), ()> {
        let mut target = format!("{}{path}", region.api());
        let route = if self.fixture {
            target = format!(
                "http://127.0.0.1:52989/qoder-device/{}{path}",
                region.provider()
            );
            Route::Direct
        } else {
            let settings = self.storage.read_settings().await.ok_or(())?;
            proxy_transport::select(&self.storage, &settings, &Value::Null, &target).await?
        };
        let client = match &route {
            Route::Direct => self.client.clone(),
            Route::Proxy { url, .. } => client_builder()
                .proxy(reqwest::Proxy::all(url).map_err(|_| ())?)
                .build()
                .map_err(|_| ())?,
            Route::Relay { .. } | Route::MissingStrict => return Err(()),
        };
        let mut request = client
            .get(target)
            .query(query)
            .header(header::ACCEPT, "application/json")
            .header(header::USER_AGENT, "9Router");
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let mut response = request.send().await.map_err(|_| ())?;
        let status = response.status();
        if !status.is_success() || status == StatusCode::ACCEPTED {
            return Ok((status, Value::Null));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if chunk.len() > RESPONSE_LIMIT - bytes.len() {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map(|data| (status, data))
            .map_err(|_| ())
    }
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
            && let Some(attempt) = pending
                .get_mut(&self.nonce)
                .filter(|attempt| attempt.expires > Instant::now())
        {
            attempt.polling = false;
        } else {
            pending.remove(&self.nonce);
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

fn expiry_ms(at: &Value, remaining: &Value, now: i64) -> i64 {
    if let Some(ms) = at.as_f64().filter(|ms| ms.is_finite() && *ms > 0.0) {
        return ms as i64;
    }
    if let Some(at) = trimmed(at) {
        if at.bytes().all(|byte| byte.is_ascii_digit()) {
            if let Ok(ms) = at.parse::<i64>()
                && ms > 0
            {
                return ms;
            }
        } else if let Some(parsed) = DateTime::parse_from_rfc3339(at)
            .or_else(|_| DateTime::parse_from_rfc2822(at))
            .ok()
            .map(|value| value.timestamp_millis())
            .or_else(|| {
                NaiveDate::parse_from_str(at, "%Y-%m-%d")
                    .ok()?
                    .and_hms_opt(0, 0, 0)
                    .map(|date| date.and_utc().timestamp_millis())
            })
        {
            return parsed;
        }
    }
    let ttl = remaining
        .as_f64()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map_or(30 * DAY_SECONDS * 1000, |seconds| (seconds * 1000.0) as i64);
    now.saturating_add(ttl)
}

fn entries(pending: &Pending) -> MutexGuard<'_, HashMap<Uuid, Attempt>> {
    pending.lock().unwrap_or_else(|error| error.into_inner())
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn trimmed(value: &Value) -> Option<&str> {
    text(value).map(js_trim).filter(|value| !value.is_empty())
}

fn random() -> Option<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

fn hash(value: &str) -> Key {
    Sha256::digest(value.as_bytes()).into()
}

fn failure(code: &str, description: Option<&str>) -> Value {
    let mut result = json!({"success":false,"error":code,"pending":matches!(code,"authorization_pending"|"slow_down")});
    if let Some(description) = description {
        result["errorDescription"] = json!(description);
    }
    result
}

fn error(status: StatusCode, message: &'static str) -> Response {
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
