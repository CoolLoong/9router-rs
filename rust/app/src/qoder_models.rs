use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use aes::Aes128;
use base64::{Engine, engine::general_purpose::STANDARD};
use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use chrono::{DateTime, Utc};
use md5::{Digest, Md5};
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Client, Method};
use rsa::{Pkcs1v15Encrypt, RsaPublicKey, pkcs8::DecodePublicKey};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_http::{Delivery, http_client};
use crate::storage::Storage;

const TIMEOUT: Duration = Duration::from_secs(15);
const CATALOG_TTL: Duration = Duration::from_secs(3600);
const BODY_LIMIT: usize = 2 * 1024 * 1024;
const MODEL_PATH: &str = "/algo/api/v2/model/list";
const PUBLIC_KEY: &str = "MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17XcW+ML9FoCI6AOvOzwIDAQAB";
static KEY: LazyLock<Option<RsaPublicKey>> =
    LazyLock::new(|| RsaPublicKey::from_public_key_der(&STANDARD.decode(PUBLIC_KEY).ok()?).ok());
static CLIENT: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());

struct Job {
    user: Value,
    token: String,
    expiry: i64,
}

struct Catalog {
    rows: Arc<[Value]>,
    expiry: Instant,
}

type Slot<T> = Arc<AsyncMutex<Option<T>>>;
type Cache<T> = LazyLock<Mutex<VecDeque<([u8; 32], Slot<T>)>>>;
static JOBS: Cache<Job> = LazyLock::new(Mutex::default);
static CATALOGS: Cache<Catalog> = LazyLock::new(Mutex::default);

fn slot<T>(cache: &Cache<T>, key: [u8; 32]) -> Slot<T> {
    let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(position) = cache.iter().position(|(candidate, _)| *candidate == key)
        && let Some(entry) = cache.remove(position)
    {
        let slot = entry.1.clone();
        cache.push_front(entry);
        return slot;
    }
    let slot = Arc::new(AsyncMutex::new(None));
    if cache.len() >= 128 {
        let Some(oldest) = cache
            .iter()
            .rposition(|(_, slot)| Arc::strong_count(slot) == 1)
        else {
            return slot;
        };
        cache.remove(oldest);
    }
    cache.push_front((key, slot.clone()));
    slot
}

pub(crate) fn supports(provider: &str) -> bool {
    matches!(provider, "qoder" | "qoder-cn")
}

pub(crate) async fn models(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    management: bool,
) -> Option<Vec<Value>> {
    let service = Service::new(storage, settings, account)?;
    let provider = service.provider;
    let (access, user) = service.credential().await?;
    let rows = service.catalog(&access, &user, management).await?;
    if management {
        return Some(rows.iter().map(|row| {
            let id = row["key"].as_str().unwrap_or_default();
            json!({
                "id":format!("{provider}/{id}"),"name":display(row, id),
                "contextLength":number(&row["max_input_tokens"], 131072),
                "maxOutputTokens":number(&row["max_output_tokens"], 0),
                "isVL":truthy(&row["is_vl"]),"isReasoning":truthy(&row["is_reasoning"]),
                "description":if truthy(&row["description"]) { row["description"].clone() } else { json!("") },
            })
        }).collect());
    }
    let present: HashSet<_> = rows.iter().filter_map(|row| row["key"].as_str()).collect();
    let mut seen = HashSet::new();
    let mut result: Vec<_> = [("smodel", "Sonus"), ("cmodel", "Cantus")]
        .into_iter()
        .filter(|(id, _)| !present.contains(id))
        .map(|(id, name)| json!({"id":id,"name":name}))
        .collect();
    result.extend(rows.iter().filter_map(|row| {
        let id = row["key"].as_str()?;
        seen.insert(id)
            .then(|| json!({"id":id,"name":display(row, id)}))
    }));
    Some(result)
}

pub(crate) async fn credential(
    storage: &Storage,
    settings: &Value,
    account: &Value,
) -> Option<(String, Value)> {
    Service::new(storage, settings, account)?.credential().await
}

pub(crate) async fn test_key(storage: &Storage, settings: &Value, account: &Value) -> bool {
    let Some(service) = Service::new(storage, settings, account) else {
        return false;
    };
    let Some(key) = nonempty(&account["apiKey"]) else {
        return false;
    };
    let key = if key.starts_with("pt-") {
        std::borrow::Cow::Borrowed(key)
    } else {
        std::borrow::Cow::Owned(format!("pt-{key}"))
    };
    let endpoint = format!("{}/api/v1/jobToken/exchange", service.api_origin());
    let Some(delivery) = service.delivery(&endpoint).await else {
        return false;
    };
    let Some(request) = service.request(&endpoint, Method::POST, delivery) else {
        return false;
    };
    request
        .header(header::ACCEPT, "application/json")
        .header("Cosy-Version", "1.0.1")
        .header("Cosy-ClientType", "5")
        .json(&json!({"personal_token":key}))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

pub(crate) async fn raw_config(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    access: &str,
    user: &Value,
    model: &str,
) -> Option<Value> {
    let service = Service::new(storage, settings, account)?;
    let rows = service.catalog(access, user, false).await?;
    if let Some(config) = rows.iter().rev().find(|row| row["key"] == model) {
        return Some(config.clone());
    }
    service
        .catalog(access, user, true)
        .await?
        .iter()
        .rev()
        .find(|row| row["key"] == model)
        .cloned()
}

struct Service<'a> {
    storage: &'a Storage,
    settings: &'a Value,
    account: &'a Value,
    provider: &'a str,
}

impl<'a> Service<'a> {
    fn new(storage: &'a Storage, settings: &'a Value, account: &'a Value) -> Option<Self> {
        let provider = account["provider"]
            .as_str()
            .filter(|provider| supports(provider))?;
        Some(Self {
            storage,
            settings,
            account,
            provider,
        })
    }

    fn api_origin(&self) -> &'static str {
        if self.provider == "qoder-cn" {
            "https://openapi.qoder.com.cn"
        } else {
            "https://openapi.qoder.sh"
        }
    }

    async fn catalog(&self, access: &str, user: &Value, force: bool) -> Option<Arc<[Value]>> {
        let origin = match (self.provider, access.starts_with("jt-")) {
            ("qoder-cn", _) => "https://gateway.qoder.com.cn",
            (_, true) => "https://api2.qoder.sh",
            _ => "https://api3.qoder.sh",
        };
        let target = format!("{origin}{MODEL_PATH}");
        let delivery = self.delivery(&target).await?;
        let hash = self
            .scope(&delivery, &target)
            .chain_update(access.as_bytes())
            .chain_update([0])
            .chain_update(js_text(user))
            .finalize()
            .into();
        let entry = slot(&CATALOGS, hash);
        let mut entry = entry.lock().await;
        let rows = if !force
            && let Some(cached) = entry
                .as_ref()
                .filter(|cached| Instant::now() < cached.expiry)
        {
            cached.rows.clone()
        } else {
            let auth = cosy(access, user, self.account, "/api/v2/model/list", b"")?;
            let response = self
                .request(&target, Method::GET, delivery)?
                .header(header::ACCEPT, "application/json")
                .header(header::ACCEPT_ENCODING, "identity")
                .headers(auth)
                .send()
                .await
                .ok()?;
            let response = json_body(response).await?;
            let rows: Arc<[Value]> = response["chat"]
                .as_array()?
                .iter()
                .filter(|row| {
                    row.is_object()
                        && row["key"]
                            .as_str()
                            .is_some_and(|id| !id.is_empty() && !id.contains(access))
                })
                .cloned()
                .collect::<Vec<_>>()
                .into();
            *entry = Some(Catalog {
                rows: rows.clone(),
                expiry: Instant::now() + CATALOG_TTL,
            });
            rows
        };
        Some(rows)
    }

    async fn delivery(&self, url: &str) -> Option<Delivery> {
        Delivery::select(self.storage, self.settings, self.account, url)
            .await
            .ok()
    }

    fn scope(&self, delivery: &Delivery, url: &str) -> Sha256 {
        let mut hash = Sha256::new()
            .chain_update(url)
            .chain_update([0])
            .chain_update(self.storage.root().as_os_str().as_encoded_bytes())
            .chain_update([0]);
        match delivery {
            Delivery::Direct => hash.update([0]),
            Delivery::Bypass => hash.update([1]),
            Delivery::Tunnel(proxy) => {
                hash.update([2]);
                hash.update(proxy.as_str());
            }
        }
        hash.chain_update([0])
    }

    fn request(
        &self,
        url: &str,
        method: Method,
        delivery: Delivery,
    ) -> Option<reqwest::RequestBuilder> {
        let destination = if delivery.use_fixture() {
            let original = reqwest::Url::parse(url).ok()?;
            format!(
                "http://127.0.0.1:52989/qoder-models/{}/{}{}",
                self.provider,
                original.host_str()?,
                original.path()
            )
        } else {
            url.to_owned()
        };
        Some(
            delivery
                .client(CLIENT.as_ref()?, TIMEOUT)?
                .request(method, destination),
        )
    }

    async fn credential(&self) -> Option<(String, Value)> {
        let saved_user = &self.account["providerSpecificData"]["userId"];
        let access = nonempty(&self.account["accessToken"]);
        let raw = nonempty(&self.account["apiKey"]).or(access);
        let Some(pat) = raw.filter(|key| key.starts_with("pt-")) else {
            return truthy(saved_user)
                .then(|| (access.unwrap_or_default().to_owned(), saved_user.clone()))
                .filter(|(token, _)| !token.is_empty());
        };
        let base = self.api_origin();
        let exchange = format!("{base}/api/v1/jobToken/exchange");
        let identity = format!("{base}/api/v1/userinfo");
        let delivery = self.delivery(&exchange).await?;
        let identity_route = self.delivery(&identity).await?;
        let id = self
            .scope(&delivery, &exchange)
            .chain_update(self.scope(&identity_route, &identity).finalize())
            .chain_update(pat.as_bytes())
            .finalize()
            .into();
        let entry = slot(&JOBS, id);
        let mut entry = entry.lock().await;
        let now = Utc::now().timestamp_millis();
        if entry
            .as_ref()
            .is_none_or(|job| job.expiry.saturating_sub(now) <= 300_000)
        {
            let response = self
                .request(&exchange, Method::POST, delivery)?
                .header(header::ACCEPT, "application/json")
                .header(header::USER_AGENT, "qodercli/1.0.0")
                .header("Cosy-Version", "1.0.0")
                .header("Cosy-ClientType", "5")
                .json(&json!({"personal_token":pat}))
                .send()
                .await
                .ok()?;
            let body = json_body(response).await?;
            let token = nonempty(&body["token"])?.to_owned();
            let expiry = if truthy(&body["expires_at"]) {
                body["expires_at"]
                    .as_str()
                    .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                    .map_or(now.saturating_add(86_400_000), |time| {
                        time.timestamp_millis()
                    })
            } else {
                now.saturating_add(
                    body["expires_in"]
                        .as_f64()
                        .filter(|ttl| *ttl > 0.0)
                        .map_or(86_400_000, |ttl| ttl as i64),
                )
            };
            let identity = self
                .request(&identity, Method::GET, identity_route)?
                .bearer_auth(&token)
                .header(header::ACCEPT, "application/json")
                .header(header::USER_AGENT, "qodercli/1.0.0")
                .send()
                .await
                .ok();
            let user = if let Some(response) = identity {
                json_body(response)
                    .await
                    .and_then(|body| {
                        ["id", "userId", "user_id"]
                            .into_iter()
                            .map(|key| &body[key])
                            .find(|field| truthy(field))
                            .cloned()
                    })
                    .unwrap_or_default()
            } else {
                Value::Null
            };
            *entry = Some(Job {
                user,
                token,
                expiry,
            });
        }
        let job = entry.as_ref()?;
        let user = if truthy(&job.user) {
            &job.user
        } else {
            saved_user
        };
        truthy(user).then(|| (job.token.clone(), user.clone()))
    }
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

async fn json_body(mut response: reqwest::Response) -> Option<Value> {
    if !response.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > BODY_LIMIT - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}

fn display(row: &Value, id: &str) -> String {
    if truthy(&row["display_name"]) {
        js_text(&row["display_name"])
    } else {
        id.into()
    }
}

fn number(value: &Value, default: i64) -> Value {
    let value = value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok());
    match value.filter(|number| number.is_finite() && *number != 0.0) {
        Some(number)
            if number.fract() == 0.0 && number >= i64::MIN as f64 && number < i64::MAX as f64 =>
        {
            json!(number as i64)
        }
        Some(number) => json!(number),
        None => json!(default),
    }
}

pub(crate) fn cosy(
    token: &str,
    user: &Value,
    account: &Value,
    path: &str,
    body: &[u8],
) -> Option<HeaderMap> {
    let user_info = json!({
        "uid":user,"security_oauth_token":token,"name":nonempty(&account["displayName"]).unwrap_or_default(),
        "aid":"","email":nonempty(&account["email"]).unwrap_or_default(),
    }).to_string();
    let uuid = Uuid::new_v4().to_string();
    let key = &uuid.as_bytes()[..16];
    let wrapped = KEY
        .as_ref()?
        .encrypt(&mut rand::rngs::OsRng, Pkcs1v15Encrypt, key)
        .ok()?;
    let wrapped = STANDARD.encode(wrapped);
    let mut buffer = user_info.into_bytes();
    let len = buffer.len();
    buffer.resize(len + 16, 0);
    let encrypted = cbc::Encryptor::<Aes128>::new_from_slices(key, key)
        .ok()?
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, len)
        .ok()?;
    let payload = STANDARD.encode(
        json!({
            "version":"v1","requestId":Uuid::new_v4().to_string(),"info":STANDARD.encode(encrypted),
            "cosyVersion":"1.0.0","ideVersion":"",
        })
        .to_string(),
    );
    let timestamp = Utc::now().timestamp().to_string();
    let signature = Md5::new()
        .chain_update(&payload)
        .chain_update(b"\n")
        .chain_update(&wrapped)
        .chain_update(b"\n")
        .chain_update(&timestamp)
        .chain_update(b"\n")
        .chain_update(body)
        .chain_update(b"\n")
        .chain_update(path.as_bytes())
        .finalize();
    let random_machine = Uuid::new_v4().to_string();
    let machine =
        nonempty(&account["providerSpecificData"]["machineId"]).unwrap_or(&random_machine);
    let mut auth = HeaderValue::from_str(&format!("Bearer COSY.{payload}.{signature:x}")).ok()?;
    auth.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, auth);
    for (name, value) in [
        ("cosy-key", wrapped.as_str()),
        ("cosy-user", &js_text(user)),
        ("cosy-date", timestamp.as_str()),
        ("cosy-version", "1.0.0"),
        ("cosy-machineid", machine),
        ("cosy-machinetoken", machine),
        ("cosy-machinetype", "5"),
        ("cosy-machineos", "x86_64_windows"),
        ("cosy-clienttype", "5"),
        ("cosy-clientip", "127.0.0.1"),
        ("cosy-bodyhash", &format!("{:x}", Md5::digest(body))),
        ("cosy-bodylength", &body.len().to_string()),
        ("cosy-sigpath", path),
        ("cosy-data-policy", "disagree"),
        ("cosy-organization-id", ""),
        ("cosy-organization-tags", ""),
        ("login-version", "v2"),
        ("x-request-id", &Uuid::new_v4().to_string()),
    ] {
        let mut value = HeaderValue::from_str(value).ok()?;
        if name == "cosy-key" {
            value.set_sensitive(true);
        }
        headers.insert(name, value);
    }
    Some(headers)
}
