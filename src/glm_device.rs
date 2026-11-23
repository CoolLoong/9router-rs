use crate::provider_defaults;
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
use chrono::Utc;
use reqwest::{Client, ClientBuilder, Method};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::dashboard_session::authorization_owner;
use crate::provider_connections::{object, truthy};
use crate::provider_index::js_trim;
use crate::provider_oauth::save_imported;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const MAX_PENDING: usize = 512;
const LIMIT: usize = 256 * 1024;
const INIT_ERROR: &str = "Could not start Z.ai device authorization";
const EXCHANGE_ERROR: &str = "Z.ai business login did not return an access token";
const LOCATION_ERROR: &str = "Unable to resolve Z.ai organization and project for the coding plan";
const LIST_ERROR: &str = "Could not read Z.ai coding plan API keys";
const COPY_ERROR: &str = "Could not copy the Z.ai coding plan API key";
const CREATE_ERROR: &str =
    "Could not confirm Z.ai API key creation. Restart authorization to check the key list.";

type Key = [u8; 32];
type Pending = Arc<Mutex<HashMap<Key, Attempt>>>;

#[derive(Clone)]
struct Device {
    storage: Storage,
    client: Client,
    config: Arc<Config>,
    pending: Pending,
    fixture: bool,
}

struct Config {
    init: Url,
    poll: Url,
    login: Url,
    api: Url,
    provider: &'static str,
    key_name: &'static str,
}

impl Config {
    fn pinned() -> Option<Self> {
        let field = |key| provider_defaults::text("glm.oauth", key);
        Some(Self {
            init: Url::parse(field("cliInitUrl")?).ok()?,
            poll: Url::parse(field("cliPollUrl")?).ok()?,
            login: Url::parse(field("businessLoginUrl")?).ok()?,
            api: Url::parse(field("apiBaseUrl")?).ok()?,
            provider: field("providerId")?,
            key_name: field("planApiKeyName")?,
        })
    }
}

struct Attempt {
    owner: Key,
    flow: Arc<Flow>,
    expires: Instant,
    next_poll: Instant,
    interval: Duration,
    busy: bool,
}

struct Flow {
    id: String,
    token: String,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/glm/device-code", get(begin))
        .route("/api/oauth/glm/poll", post(poll))
        .with_state(Device {
            storage,
            client: client_builder().build().expect("Z.ai consent HTTP client"),
            config: Arc::new(Config::pinned().expect("pinned GLM OAuth configuration")),
            pending: Arc::default(),
            fixture: std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"),
        })
}

async fn begin(State(state): State<Device>, headers: HeaderMap) -> Response {
    let Some(owner) = authorization_owner(&state.storage, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    match state.begin(owner).await {
        Some(body) => reply(StatusCode::OK, body),
        None => error(StatusCode::INTERNAL_SERVER_ERROR, INIT_ERROR),
    }
}

async fn poll(State(state): State<Device>, request: Request) -> Response {
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
    let Some(handle) = nonempty(&body["deviceCode"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing device code");
    };
    let (flow, lease) = match state.claim(owner, hash(handle)) {
        Ok(result) => result,
        Err(code) => return reply(StatusCode::OK, poll_result(code, None)),
    };
    let result = tokio::spawn(async move { state.poll(&flow, lease).await })
        .await
        .unwrap_or_else(|_| fatal("Could not complete Z.ai device authorization"));
    reply(StatusCode::OK, result)
}

impl Device {
    async fn begin(&self, owner: Key) -> Option<Value> {
        let mut poll_token = [0u8; 32];
        getrandom::fill(&mut poll_token).ok()?;
        let token: String = poll_token
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let (status, payload) = self
            .send(
                Method::POST,
                &self.config.init,
                Some(&token),
                Some(json!({"provider":self.config.provider})),
                false,
            )
            .await
            .ok()?;
        if !status.is_success() || !success(&payload["code"]) {
            return None;
        }
        let data = &payload["data"];
        let id = nonempty(&data["flow_id"]).filter(|id| segment_allowed(id))?;
        let verification = verification_uri(&data["authorize_url"])?;
        let seconds = remaining(data.get("expires_at"))?;
        let interval = positive(&data["poll_interval_sec"]).unwrap_or(3);
        let now = Instant::now();
        let expires = now.checked_add(Duration::from_secs(seconds))?;
        let interval = Duration::from_secs(interval);
        let next_poll = now.checked_add(interval)?;
        let handle = random()?;
        let verifier = random()?;
        let mut pending = entries(&self.pending);
        pending.retain(|_, attempt| attempt.busy || attempt.expires > now);
        if pending.len() >= MAX_PENDING {
            return None;
        }
        let Entry::Vacant(slot) = pending.entry(hash(&handle)) else {
            return None;
        };
        slot.insert(Attempt {
            owner,
            flow: Arc::new(Flow {
                id: id.into(),
                token,
            }),
            expires,
            next_poll,
            interval,
            busy: false,
        });
        Some(json!({
            "device_code":handle, "verification_uri":verification,
            "expires_in":seconds, "interval":interval.as_secs(), "codeVerifier":verifier,
        }))
    }

    fn claim(&self, owner: Key, key: Key) -> Result<(Arc<Flow>, Lease), &'static str> {
        let now = Instant::now();
        let mut pending = entries(&self.pending);
        let Some(attempt) = pending
            .get_mut(&key)
            .filter(|attempt| attempt.owner == owner && attempt.expires > now)
        else {
            return Err("expired_token");
        };
        if attempt.busy {
            return Err("authorization_pending");
        }
        if attempt.next_poll > now {
            return Err("slow_down");
        }
        attempt.busy = true;
        attempt.next_poll = now.checked_add(attempt.interval).unwrap_or(attempt.expires);
        Ok((
            attempt.flow.clone(),
            Lease {
                pending: self.pending.clone(),
                key,
                keep: false,
            },
        ))
    }

    async fn poll(&self, flow: &Flow, mut lease: Lease) -> Value {
        let Some(url) = append(&self.config.poll, &[&flow.id]) else {
            return fatal("Could not resolve Z.ai's device authorization URL");
        };
        let Ok((status, payload)) = self
            .send(Method::GET, &url, Some(&flow.token), None, false)
            .await
        else {
            return fatal("Could not poll Z.ai device authorization");
        };
        if !status.is_success() {
            return poll_result(
                "access_denied",
                Some(&format!("ZCode poll failed (HTTP {})", status.as_u16())),
            );
        }
        if !success(&payload["code"]) {
            return poll_result("access_denied", Some("ZCode poll failed"));
        }
        let data = &payload["data"];
        match data["status"].as_str() {
            Some("ready") => (),
            Some("failed") => {
                return poll_result(
                    "access_denied",
                    Some("ZCode authorization failed or was cancelled"),
                );
            }
            status => {
                lease.keep = true;
                let description =
                    (status != Some("pending")).then_some("ZCode authorization is still pending");
                return poll_result("authorization_pending", description);
            }
        }
        if nonempty(&data["providerId"]).is_some_and(|provider| provider != self.config.provider) {
            return poll_result(
                "access_denied",
                Some("Unexpected ZCode authorization provider"),
            );
        }
        let provider = &data[self.config.provider];
        let Some(access) = [
            &provider["access_token"],
            &provider["accessToken"],
            &data["accessToken"],
            &data["access_token"],
        ]
        .into_iter()
        .find_map(nonempty) else {
            return poll_result(
                "access_denied",
                Some("ZCode poll response missing access token"),
            );
        };
        match self.coding_plan_key(access).await {
            Ok(key) => self.save(&key, &data["user"]).await,
            Err(message) => fatal(message),
        }
    }

    async fn coding_plan_key(&self, access: &str) -> Result<String, &'static str> {
        let login = self
            .business(
                Method::POST,
                &self.config.login,
                None,
                Some(json!({"token":access})),
                EXCHANGE_ERROR,
            )
            .await?;
        let bearer = trimmed(&login["access_token"])
            .or_else(|| trimmed(&login["accessToken"]))
            .ok_or(EXCHANGE_ERROR)?;
        let customer_url = append(
            &self.config.api,
            &["api", "biz", "customer", "getCustomerInfo"],
        )
        .ok_or(LOCATION_ERROR)?;
        let customer = self
            .business(
                Method::GET,
                &customer_url,
                Some(bearer),
                None,
                LOCATION_ERROR,
            )
            .await?;
        let (org, project) = location(&customer).ok_or(LOCATION_ERROR)?;
        let keys_url = append(
            &self.config.api,
            &[
                "api",
                "biz",
                "v1",
                "organization",
                &org,
                "projects",
                &project,
                "api_keys",
            ],
        )
        .ok_or(LIST_ERROR)?;
        let keys = self
            .business(Method::GET, &keys_url, Some(bearer), None, LIST_ERROR)
            .await?;
        let keys = keys.as_array().ok_or(LIST_ERROR)?;
        let created;
        let key = if let Some(existing) = keys
            .iter()
            .find(|entry| entry["name"] == self.config.key_name)
        {
            existing
        } else {
            created = self
                .business(
                    Method::POST,
                    &keys_url,
                    Some(bearer),
                    Some(json!({"name":self.config.key_name})),
                    CREATE_ERROR,
                )
                .await?;
            &created
        };
        let id = trimmed(&key["apiKey"])
            .filter(|id| segment_allowed(id))
            .ok_or(COPY_ERROR)?;
        let copy_url = append(&keys_url, &["copy", id]).ok_or(COPY_ERROR)?;
        let secret = self
            .business(Method::GET, &copy_url, Some(bearer), None, COPY_ERROR)
            .await?;
        let secret = trimmed(&secret["secretKey"]).ok_or(COPY_ERROR)?;
        let full = format!("{id}.{secret}");
        if full.len() > 4096 || header::HeaderValue::from_str(&full).is_err() {
            return Err(COPY_ERROR);
        }
        Ok(full)
    }

    async fn business(
        &self,
        method: Method,
        url: &Url,
        bearer: Option<&str>,
        body: Option<Value>,
        error: &'static str,
    ) -> Result<Value, &'static str> {
        let (status, payload) = self
            .send(method, url, bearer, body, true)
            .await
            .map_err(|_| error)?;
        if !status.is_success() || !success(&payload["code"]) || payload["success"] == false {
            return Err(error);
        }
        Ok(payload
            .get("data")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or(payload))
    }

    async fn save(&self, key: &str, user: &Value) -> Value {
        let name = nonempty(&user["name"]);
        let email = nonempty(&user["email"]);
        let mut details = object(json!({"authMethod":"cli_poll"}));
        if let Some(name) = name {
            details.insert("username".into(), json!(name));
        }
        if (user["user_id"].is_string() || user["user_id"].is_number()) && truthy(&user["user_id"])
        {
            details.insert("userId".into(), user["user_id"].clone());
        }
        let mut fields = object(json!({
            "accessToken":key, "expiresAt":null,"refreshToken":null,"email":email,"providerSpecificData":details,
        }));
        if let Some(name) = name.or(email) {
            fields.insert("displayName".into(), json!(name));
        }
        match save_imported(&self.storage, "glm", fields).await {
            Ok(saved) => json!({"success":true,"connection":{"id":saved["id"],"provider":"glm"}}),
            Err(_) => fatal("Could not save the provider connection"),
        }
    }

    async fn send(
        &self,
        method: Method,
        url: &Url,
        bearer: Option<&str>,
        body: Option<Value>,
        business: bool,
    ) -> Result<(StatusCode, Value), ()> {
        let (client, target) = if self.fixture {
            let host = url.host_str().ok_or(())?;
            (
                self.client.clone(),
                format!("http://127.0.0.1:52989/glm-consent/{host}{}", url.path()),
            )
        } else {
            let settings = self.storage.read_settings().await.ok_or(())?;
            let route =
                proxy_transport::select(&self.storage, &settings, &Value::Null, url.as_str())
                    .await?;
            let client = match route {
                Route::Direct => self.client.clone(),
                Route::Proxy { url, .. } => client_builder()
                    .proxy(reqwest::Proxy::all(url).map_err(|_| ())?)
                    .build()
                    .map_err(|_| ())?,
                Route::Relay { .. } | Route::MissingStrict => return Err(()),
            };
            (client, url.to_string())
        };
        let mut request = client.request(method, target);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        } else if business {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let mut response = request.send().await.map_err(|_| ())?;
        let status = response.status();
        if !status.is_success() {
            return Ok((status, Value::Null));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if chunk.len() > LIMIT - bytes.len() {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map(|data| (status, data))
            .map_err(|_| ())
    }
}

fn client_builder() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(20))
}

struct Lease {
    pending: Pending,
    key: Key,
    keep: bool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut pending = entries(&self.pending);
        if self.keep
            && let Some(attempt) = pending
                .get_mut(&self.key)
                .filter(|entry| entry.expires > Instant::now())
        {
            attempt.busy = false;
        } else {
            pending.remove(&self.key);
        }
    }
}

fn entries(pending: &Pending) -> MutexGuard<'_, HashMap<Key, Attempt>> {
    pending.lock().unwrap_or_else(|error| error.into_inner())
}

fn location(customer: &Value) -> Option<(String, String)> {
    let mut candidates = customer["organizations"]
        .as_array()?
        .iter()
        .filter_map(|organization| {
            let id = identifier(&organization["organizationId"])?;
            let mut projects = organization["projects"]
                .as_array()?
                .iter()
                .filter(|project| {
                    let kind = &project["projectType"];
                    kind.as_f64() != Some(2.0)
                        && kind.as_str().is_none_or(|kind| js_trim(kind) != "2")
                });
            let first = projects.next()?;
            let project = if default_name(&first["projectName"]) {
                first
            } else {
                projects
                    .find(|project| default_name(&project["projectName"]))
                    .unwrap_or(first)
            };
            Some((id, project, default_name(&organization["organizationName"])))
        });
    let first = candidates.next()?;
    let (org, project, _) = if first.2 {
        first
    } else {
        candidates.find(|item| item.2).unwrap_or(first)
    };
    Some((org, identifier(&project["projectId"])?))
}

fn identifier(value: &Value) -> Option<String> {
    if let Some(text) = nonempty(value).filter(|text| segment_allowed(text)) {
        return Some(text.to_owned());
    }
    value
        .as_number()
        .filter(|number| number.as_f64() != Some(0.0))
        .map(ToString::to_string)
}

fn segment_allowed(text: &str) -> bool {
    text.len() <= 1024 && !matches!(text, "." | "..") && !text.chars().any(char::is_control)
}

fn default_name(value: &Value) -> bool {
    value.as_str().is_some_and(|name| {
        js_trim(name).eq_ignore_ascii_case("default")
            || name.contains("默认机构")
            || name.contains("默认项目")
    })
}

fn append(origin: &Url, paths: &[&str]) -> Option<Url> {
    let mut url = origin.clone();
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(paths.iter().copied());
    Some(url)
}

fn verification_uri(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| {
        Url::parse(value).is_ok_and(|url| {
            url.scheme() == "https"
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && url
                    .host_str()
                    .is_some_and(|host| host == "z.ai" || host.ends_with(".z.ai"))
        })
    })
}

fn remaining(expiry: Option<&Value>) -> Option<u64> {
    let Some(epoch) = expiry.and_then(positive) else {
        return Some(300);
    };
    let millis = if epoch > 1_000_000_000_000 {
        epoch
    } else {
        epoch.saturating_mul(1000)
    };
    let seconds = millis.saturating_sub(Utc::now().timestamp_millis().max(0) as u64) / 1000;
    (seconds > 0).then_some(seconds)
}

fn positive(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| js_trim(value.as_str()?).parse().ok())
        .filter(|value| *value > 0)
}

fn success(value: &Value) -> bool {
    value.is_null()
        || matches!(value.as_f64(), Some(0.0 | 200.0))
        || matches!(value.as_str(), Some("0" | "200"))
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn trimmed(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(js_trim)
        .filter(|value| !value.is_empty())
}

fn random() -> Option<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

fn hash(value: &str) -> Key {
    Sha256::digest(value.as_bytes()).into()
}

fn poll_result(code: &str, description: Option<&str>) -> Value {
    let mut body = json!({"success":false,"error":code,"pending":matches!(code,"authorization_pending"|"slow_down")});
    if let Some(description) = description {
        body["errorDescription"] = json!(description);
    }
    body
}

fn fatal(message: &str) -> Value {
    json!({"success":false,"error":"exchange_failed","errorDescription":message,"pending":false,"fatal":true})
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
