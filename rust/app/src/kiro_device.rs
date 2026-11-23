use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, TimeDelta, Utc};
use reqwest::{Client, ClientBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::dashboard_session;
use crate::kiro_models::valid_region;
use crate::provider_connections::object;
use crate::provider_index::js_trim;
use crate::provider_oauth::{save_imported, token_claims};
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const INPUT_LIMIT: usize = 64 * 1024;
const RESPONSE_LIMIT: usize = 128 * 1024;
const MAX_PENDING: usize = 512;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);
const START_URL: &str = "https://view.awsapps.com/start";
const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const ISSUER: &str = "https://identitycenter.amazonaws.com/ssoins-722374e8c3c8e6c6";
pub(super) const PROFILES: &str =
    "https://codewhisperer.us-east-1.amazonaws.com/ListAvailableProfiles";
const FIXTURE: &str = "http://127.0.0.1:52989/kiro-device";
const REGISTER: &str = "client/register";
const AUTHORIZE: &str = "device_authorization";
const TOKEN: &str = "token";

type Key = [u8; 32];
type Pool = Arc<Mutex<HashMap<Key, Entry>>>;

#[derive(Clone)]
struct Device {
    storage: Storage,
    client: Client,
    fixture: bool,
    pending: Pool,
}

enum Entry {
    Registering,
    Pending(Pending),
}

struct Pending {
    request: Arc<Attempt>,
    expires: Instant,
    next_poll: Instant,
    interval: Duration,
    polling: bool,
}

struct Attempt {
    device_code: String,
    client_id: String,
    client_secret: String,
    options: Options,
}

struct Options {
    region: String,
    start_url: String,
    method: &'static str,
}

#[derive(Clone, Copy)]
struct Error(StatusCode, &'static str);

impl Error {
    const INTERNAL: Self = Self(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not prepare Kiro device authorization",
    );
    const UNAUTHORIZED: Self = Self(StatusCode::UNAUTHORIZED, "Unauthorized");
    const AT_CAPACITY: Self = Self(
        StatusCode::SERVICE_UNAVAILABLE,
        "Too many pending Kiro sign-ins",
    );
    const REGISTER: Self = Self(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Client registration failed",
    );
    const DEVICE: Self = Self(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Device authorization failed",
    );

    fn response(self) -> Response {
        reply(self.0, json!({"error":self.1}))
    }
}

pub fn router(storage: Storage) -> Router {
    let fixture = std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1");
    Router::new()
        .route("/api/oauth/kiro/device-code", get(device_code))
        .route("/api/oauth/kiro/poll", post(poll))
        .with_state(Device {
            storage,
            client: client_builder()
                .build()
                .expect("AWS device OAuth HTTP client"),
            fixture,
            pending: Arc::default(),
        })
}

async fn device_code(State(state): State<Device>, request: Request) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return Error::UNAUTHORIZED.response();
    }
    match state.begin(request.uri().query().unwrap_or_default()).await {
        Ok(body) => reply(StatusCode::OK, body),
        Err(error) => error.response(),
    }
}

async fn poll(State(state): State<Device>, request: Request) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return Error::UNAUTHORIZED.response();
    }
    let Some(body) = to_bytes(request.into_body(), INPUT_LIMIT)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
    else {
        return Error(StatusCode::BAD_REQUEST, "Invalid or empty request body").response();
    };
    let Some(handle) = nonempty(&body["deviceCode"]) else {
        return Error(StatusCode::BAD_REQUEST, "Missing device code").response();
    };
    if handle.len() != 43 {
        return reply(StatusCode::OK, expired());
    }
    let key = hash(handle);
    let (attempt, lease) = match state.claim(key) {
        Ok(ready) => ready,
        Err(result) => return reply(StatusCode::OK, result),
    };
    let result = tokio::spawn(async move { state.exchange(attempt, lease).await })
        .await
        .unwrap_or_else(|_| {
            failure(
                "exchange_failed",
                Some("Could not complete the Kiro sign-in"),
                true,
            )
        });
    reply(StatusCode::OK, result)
}

impl Device {
    async fn begin(&self, query: &str) -> Result<Value, Error> {
        let options = Options::parse(query)?;
        let handle = random()?;
        let verifier = random()?;
        let mut registration = self.reserve(hash(&handle))?;
        let register = self.aws(&options.region, REGISTER, json!({
            "clientName":"kiro-oauth-client",
            "clientType":"public",
            "scopes":["codewhisperer:completions","codewhisperer:analysis","codewhisperer:conversations"],
            "grantTypes":[GRANT,"refresh_token"],
            "issuerUrl":ISSUER,
        })).await.map_err(|_| Error::REGISTER)?;
        let Some(client_id) = nonempty(&register.body["clientId"]).filter(|_| register.ok) else {
            return Err(Error::REGISTER);
        };
        let Some(client_secret) = nonempty(&register.body["clientSecret"]) else {
            return Err(Error::REGISTER);
        };
        let authorize = self
            .aws(
                &options.region,
                AUTHORIZE,
                json!({
                    "clientId":client_id,
                    "clientSecret":client_secret,
                    "startUrl": options.start_url,
                }),
            )
            .await
            .map_err(|_| Error::DEVICE)?;
        let data = authorize.body;
        let Some(device_code) = nonempty(&data["deviceCode"]).filter(|_| authorize.ok) else {
            return Err(Error::DEVICE);
        };
        let Some(user_code) = nonempty(&data["userCode"]) else {
            return Err(Error::DEVICE);
        };
        let Some(verification_uri) = data["verificationUri"].as_str().filter(|url| https(url))
        else {
            return Err(Error::DEVICE);
        };
        let Some(seconds) = data["expiresIn"]
            .as_u64()
            .filter(|expires| (1..=86_400).contains(expires))
        else {
            return Err(Error::DEVICE);
        };
        let interval = data["interval"]
            .as_u64()
            .filter(|interval| (1..=3600).contains(interval))
            .unwrap_or(5);
        let mut reply = json!({
            "device_code":handle, "user_code":user_code, "verification_uri":verification_uri,
            "expires_in":seconds, "interval":interval, "_region":options.region,
            "_authMethod":options.method, "_startUrl":options.start_url, "codeVerifier":verifier,
        });
        if let Some(uri) = data["verificationUriComplete"]
            .as_str()
            .filter(|uri| https(uri))
        {
            reply["verification_uri_complete"] = json!(uri);
        }
        let now = Instant::now();
        let mut expires = now + Duration::from_secs(seconds);
        if let Some(secret_expiry) = register.body["clientSecretExpiresAt"].as_i64() {
            let remaining = secret_expiry.saturating_sub(Utc::now().timestamp());
            let remaining = u64::try_from(remaining)
                .ok()
                .filter(|seconds| *seconds > 0)
                .ok_or(Error::REGISTER)?;
            if remaining < seconds {
                expires = now + Duration::from_secs(remaining);
                reply["expires_in"] = json!(remaining);
            }
        }
        registration.finish(Pending {
            request: Arc::new(Attempt {
                device_code: device_code.to_owned(),
                client_id: client_id.to_owned(),
                client_secret: client_secret.to_owned(),
                options,
            }),
            expires,
            next_poll: now,
            interval: Duration::from_secs(interval),
            polling: false,
        })?;
        Ok(reply)
    }

    fn reserve(&self, key: Key) -> Result<Registration, Error> {
        let mut pool = entries(&self.pending);
        let now = Instant::now();
        pool.retain(|_, entry| match entry {
            Entry::Pending(data) => data.polling || data.expires > now,
            Entry::Registering => true,
        });
        if pool.len() >= MAX_PENDING {
            return Err(Error::AT_CAPACITY);
        }
        let std::collections::hash_map::Entry::Vacant(slot) = pool.entry(key) else {
            return Err(Error::INTERNAL);
        };
        slot.insert(Entry::Registering);
        Ok(Registration {
            pool: self.pending.clone(),
            key,
        })
    }

    fn claim(&self, key: Key) -> Result<(Arc<Attempt>, Lease), Value> {
        let mut pool = entries(&self.pending);
        let now = Instant::now();
        match pool.get_mut(&key) {
            Some(Entry::Pending(data)) if data.expires > now => {
                if data.polling {
                    return Err(failure("authorization_pending", None, false));
                }
                if data.next_poll > now {
                    return Err(failure("slow_down", None, false));
                }
                data.polling = true;
                data.next_poll = now + data.interval;
                Ok((
                    data.request.clone(),
                    Lease {
                        pool: self.pending.clone(),
                        key,
                        waiting: None,
                    },
                ))
            }
            Some(Entry::Pending(data)) if !data.polling => {
                pool.remove(&key);
                Err(expired())
            }
            _ => Err(expired()),
        }
    }

    async fn exchange(self, attempt: Arc<Attempt>, mut lease: Lease) -> Value {
        let region = &attempt.options.region;
        let result = self
            .aws(
                region,
                TOKEN,
                json!({
                    "clientId":attempt.client_id,
                    "clientSecret":attempt.client_secret,
                    "deviceCode":attempt.device_code,
                    "grantType":GRANT,
                }),
            )
            .await;
        let Ok(result) = result else {
            return failure(
                "exchange_failed",
                Some("Could not poll for a Kiro device token"),
                true,
            );
        };
        if let Some(token) = nonempty(&result.body["accessToken"]).filter(|_| result.ok) {
            return self.save(&attempt, token, &result.body).await;
        }
        let code = response_code(&result.body);
        if matches!(code, "authorization_pending" | "slow_down") {
            lease.waiting = Some(code);
        }
        let description = match code {
            "authorization_pending" => None,
            "slow_down" => None,
            "access_denied" => Some("The Kiro device authorization was denied"),
            "expired_token" => Some("The device code has expired"),
            _ => Some("Kiro device authorization failed"),
        };
        failure(code, description, false)
    }

    async fn save(&self, attempt: &Attempt, token: &str, data: &Value) -> Value {
        let claims = token_claims(token);
        let email = ["email", "preferred_username", "sub"]
            .iter()
            .find_map(|key| nonempty(&claims[*key]));
        let mut profile = nonempty(&data["profileArn"]).map(str::to_owned);
        if profile.is_none() {
            profile = self.profile(token).await;
        }
        let mut connection = object(json!({
            "accessToken":token, "email":email,
            "providerSpecificData":{
                "profileArn":profile,"clientId":attempt.client_id,"clientSecret":attempt.client_secret,
                "region":attempt.options.region,"authMethod":attempt.options.method,"startUrl":attempt.options.start_url,
            },
        }));
        if let Some(refresh) = nonempty(&data["refreshToken"]) {
            connection.insert("refreshToken".into(), json!(refresh));
        }
        if let Some(expiry) = data["expiresIn"].as_i64().filter(|seconds| *seconds > 0) {
            let Some(at) = TimeDelta::try_seconds(expiry)
                .and_then(|delta| Utc::now().checked_add_signed(delta))
            else {
                return failure(
                    "exchange_failed",
                    Some("Invalid Kiro token expiration"),
                    true,
                );
            };
            connection.insert("expiresIn".into(), json!(expiry));
            connection.insert(
                "expiresAt".into(),
                json!(at.to_rfc3339_opts(SecondsFormat::Millis, true)),
            );
        } else {
            connection.insert("expiresAt".into(), Value::Null);
        }
        match save_imported(&self.storage, "kiro", connection).await {
            Ok(saved) => json!({"success":true,"connection":{"id":saved["id"],"provider":"kiro"}}),
            Err(_) => failure(
                "exchange_failed",
                Some("Could not save the provider connection"),
                true,
            ),
        }
    }

    async fn profile(&self, access: &str) -> Option<String> {
        let data = self
            .send(PROFILES, "profile", Some(access), json!({"maxResults":10}))
            .await
            .ok()?;
        if !data.ok {
            return None;
        }
        data.body["profiles"]
            .as_array()?
            .iter()
            .find_map(|profile| {
                nonempty(&profile["arn"])
                    .map(js_trim)
                    .filter(|arn| !arn.is_empty())
                    .map(str::to_owned)
            })
    }

    async fn aws(&self, region: &str, path: &str, payload: Value) -> Result<Upstream, ()> {
        debug_assert!(valid_region(region));
        let endpoint = format!("https://oidc.{region}.amazonaws.com/{path}");
        self.send(&endpoint, &format!("{region}/{path}"), None, payload)
            .await
    }

    async fn send(
        &self,
        url: &str,
        fixture_path: &str,
        bearer: Option<&str>,
        payload: Value,
    ) -> Result<Upstream, ()> {
        let (client, endpoint, route) = if self.fixture {
            (
                self.client.clone(),
                format!("{FIXTURE}/{fixture_path}"),
                Route::Direct,
            )
        } else {
            let settings = self.storage.read_settings().await.ok_or(())?;
            let route =
                proxy_transport::select(&self.storage, &settings, &Value::Null, url).await?;
            let mut endpoint = url.to_owned();
            let client = match &route {
                Route::Direct => self.client.clone(),
                Route::Proxy { url, .. } => client_builder()
                    .proxy(reqwest::Proxy::all(url).map_err(|_| ())?)
                    .build()
                    .map_err(|_| ())?,
                Route::Relay { url, .. } => {
                    endpoint = url.clone();
                    self.client.clone()
                }
                Route::MissingStrict => return Err(()),
            };
            (client, endpoint, route)
        };
        let mut request = client
            .post(endpoint)
            .header(header::ACCEPT.as_str(), "application/json")
            .json(&payload);
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        let mut response = route.relay(request).send().await.map_err(|_| ())?;
        if response.status().is_redirection() {
            return Err(());
        }
        let ok = response.status().is_success();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if bytes.len() + chunk.len() > RESPONSE_LIMIT {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        let body =
            serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"error":"invalid_response"}));
        Ok(Upstream { ok, body })
    }
}

struct Upstream {
    ok: bool,
    body: Value,
}

impl Options {
    fn parse(query: &str) -> Result<Self, Error> {
        let value = |name| {
            url::form_urlencoded::parse(query.as_bytes())
                .find_map(|(key, value)| (key == name).then(|| js_trim(&value).to_owned()))
                .filter(|value| !value.is_empty())
        };
        let region = value("region").unwrap_or_else(|| "us-east-1".to_owned());
        if !valid_region(&region) {
            return Err(Error(StatusCode::INTERNAL_SERVER_ERROR, "Invalid region"));
        }
        let start_url = value("start_url").unwrap_or_else(|| START_URL.to_owned());
        if !https(&start_url) {
            return Err(Error(StatusCode::BAD_REQUEST, "Invalid start URL"));
        }
        let method = if url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(key, value)| (key == "auth_method").then_some(value))
            .as_deref()
            == Some("idc")
        {
            "idc"
        } else {
            "builder-id"
        };
        Ok(Self {
            region,
            start_url,
            method,
        })
    }
}

struct Registration {
    pool: Pool,
    key: Key,
}

impl Registration {
    fn finish(&mut self, pending: Pending) -> Result<(), Error> {
        let mut pool = entries(&self.pool);
        let Some(entry @ Entry::Registering) = pool.get_mut(&self.key) else {
            return Err(Error::INTERNAL);
        };
        *entry = Entry::Pending(pending);
        Ok(())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut pool = entries(&self.pool);
        if matches!(pool.get(&self.key), Some(Entry::Registering)) {
            pool.remove(&self.key);
        }
    }
}

struct Lease {
    pool: Pool,
    key: Key,
    waiting: Option<&'static str>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut pool = entries(&self.pool);
        if let Some(Entry::Pending(data)) = pool.get_mut(&self.key)
            && let Some(code) = self.waiting
            && data.expires > Instant::now()
        {
            data.polling = false;
            if code == "slow_down" {
                data.interval += Duration::from_secs(5);
                data.next_poll = Instant::now() + data.interval;
            }
        } else {
            pool.remove(&self.key);
        }
    }
}

fn entries(pool: &Pool) -> MutexGuard<'_, HashMap<Key, Entry>> {
    pool.lock().unwrap_or_else(|error| error.into_inner())
}

fn client_builder() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .timeout(CLIENT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn https(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn hash(value: &str) -> Key {
    Sha256::digest(value.as_bytes()).into()
}

fn random() -> Result<String, Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::INTERNAL)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn expired() -> Value {
    failure("expired_token", None, false)
}

fn response_code(value: &Value) -> &'static str {
    match value["error"].as_str() {
        Some("authorization_pending") | None if value.is_object() => "authorization_pending",
        Some("slow_down") => "slow_down",
        Some("access_denied") => "access_denied",
        Some("expired_token") => "expired_token",
        Some("invalid_grant") => "invalid_grant",
        Some("invalid_request") => "invalid_request",
        Some("invalid_client") => "invalid_client",
        Some("unauthorized_client") => "unauthorized_client",
        Some("invalid_scope") => "invalid_scope",
        Some("unsupported_grant_type") => "unsupported_grant_type",
        Some("server_error") => "server_error",
        Some("temporarily_unavailable") => "temporarily_unavailable",
        Some("invalid_response") => "invalid_response",
        _ => "device_authorization_failed",
    }
}

fn failure(code: &str, description: Option<&str>, fatal: bool) -> Value {
    let waiting = matches!(code, "authorization_pending" | "slow_down");
    let mut response = json!({"success":false,"error":code});
    if let Some(description) = description {
        response["errorDescription"] = json!(description);
    }
    response["pending"] = json!(waiting);
    if fatal {
        response["fatal"] = json!(true);
    }
    response
}

fn reply(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}
