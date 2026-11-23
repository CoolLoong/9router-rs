use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_imports::metadata_client;
use crate::provider_index::js_trim;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const ORIGIN: &str = "https://llm.kimchi.dev";
const PATH: &str = "/v1/models/metadata?include_in_cli=true";
const FIXTURE: &str = "http://127.0.0.1:52989/kimchi/v1/models/metadata?include_in_cli=true";
const TIMEOUT: Duration = Duration::from_secs(20);
const TTL: Duration = Duration::from_secs(300);
const BODY_LIMIT: usize = 4 * 1024 * 1024;
const MAX_ACCOUNTS: usize = 128;

static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let package: Value =
        serde_json::from_str(include_str!("../../../package.json")).unwrap_or_default();
    format!("9Router/{}", package["version"].as_str().unwrap_or("0.0.0"))
});
static OFFICIAL: LazyLock<String> = LazyLock::new(|| format!("{ORIGIN}{PATH}"));

type Key = [u8; 32];
type Slot = Arc<AsyncMutex<Option<(Instant, Vec<Value>)>>>;
static CACHE: LazyLock<Mutex<VecDeque<(Key, Slot)>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

pub(crate) async fn models(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    force: bool,
) -> Option<Vec<Value>> {
    let details = &account["providerSpecificData"];
    let token = catalog_token(account)?;
    if account["provider"] != "kimchi"
        || token.len() > 16 * 1024
        || !token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return None;
    }
    let endpoint = endpoint(&details["kimchiEndpoint"])?;
    let route = proxy_transport::select(storage, settings, account, endpoint.as_str())
        .await
        .ok()?;
    if matches!(route, Route::MissingStrict)
        || matches!(route, Route::Direct)
            && (truthy(&details["vercelRelayUrl"]) || details["strictProxy"] == true)
    {
        return None;
    }
    let slot = slot(account_key(storage, account, token, &endpoint))?;
    let mut cache = slot.lock().await;
    if !force
        && let Some((_, models)) = cache
            .as_ref()
            .filter(|(expiry, _)| *expiry > Instant::now())
    {
        return Some(models.clone());
    }
    let payload = fetch(&route, &endpoint, token).await?;
    let secrets: Vec<_> = [
        "accessToken",
        "apiKey",
        "refreshToken",
        "idToken",
        "sessionToken",
    ]
    .into_iter()
    .flat_map(|field| [&account[field], &details[field]])
    .filter_map(Value::as_str)
    .filter(|value| !value.is_empty())
    .collect();
    let models: Vec<_> = payload["models"]
        .as_array()?
        .iter()
        .filter_map(normalize)
        .filter_map(|mut model| {
            let id = model["id"].as_str()?;
            if secrets.iter().any(|secret| id.contains(secret)) {
                return None;
            }
            let id = id.to_owned();
            scrub(&mut model, &secrets);
            model["id"] = json!(id);
            Some(model)
        })
        .collect();
    if models.is_empty() {
        return None;
    }
    *cache = Some((Instant::now() + TTL, models.clone()));
    Some(models)
}

fn catalog_token(account: &Value) -> Option<&str> {
    [
        &account["accessToken"],
        &account["apiKey"],
        &account["providerSpecificData"]["apiKey"],
    ]
    .into_iter()
    .find(|value| truthy(value))?
    .as_str()
}

pub(crate) fn cached_anthropic(
    storage: &Storage,
    account: &Value,
    bearer: &str,
    model: &str,
) -> bool {
    let Some(url) = endpoint(&account["providerSpecificData"]["kimchiEndpoint"]) else {
        return false;
    };
    if catalog_token(account) != Some(bearer) {
        return false;
    }
    let key = account_key(storage, account, bearer, &url);
    let active = CACHE.lock().ok().and_then(|cache| {
        cache
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, slot)| slot.clone())
    });
    let Some(active) = active else {
        return false;
    };
    let Ok(cache) = active.try_lock() else {
        return false;
    };
    cache
        .as_ref()
        .filter(|(expiry, _)| *expiry > Instant::now())
        .is_some_and(|(_, rows)| {
            let tail = model.rsplit('/').next().unwrap_or(model);
            rows.iter()
                .find(|row| {
                    row["id"]
                        .as_str()
                        .is_some_and(|id| id.eq_ignore_ascii_case(model))
                })
                .or_else(|| {
                    rows.iter().find(|row| {
                        row["id"]
                            .as_str()
                            .is_some_and(|id| id.eq_ignore_ascii_case(tail))
                    })
                })
                .is_some_and(|row| row["upstreamProvider"] == "anthropic")
        })
}

fn endpoint(stored: &Value) -> Option<Url> {
    let base = match stored {
        Value::Null => ORIGIN,
        Value::String(base) => {
            let base = js_trim(base);
            if base.is_empty() { ORIGIN } else { base }
        }
        _ => return None,
    };
    let base = Url::parse(base).ok()?;
    if !matches!(base.scheme(), "http" | "https")
        || !base.has_host()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return None;
    }
    Url::parse(&format!("{}{PATH}", base.as_str().trim_end_matches('/'))).ok()
}

fn account_key(storage: &Storage, account: &Value, token: &str, endpoint: &Url) -> Key {
    let mut hasher = Sha256::new();
    for part in [
        storage.root().as_os_str().as_encoded_bytes(),
        account["id"].as_str().unwrap_or_default().as_bytes(),
        token.as_bytes(),
        endpoint.as_str().as_bytes(),
    ] {
        hasher.update(part.len().to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn slot(key: Key) -> Option<Slot> {
    let mut cache = CACHE.lock().ok()?;
    let slot = cache
        .iter()
        .position(|(candidate, _)| *candidate == key)
        .and_then(|index| cache.remove(index))
        .map_or_else(|| Arc::new(AsyncMutex::new(None)), |(_, slot)| slot);
    cache.push_front((key, slot.clone()));
    cache.truncate(MAX_ACCOUNTS);
    Some(slot)
}

async fn fetch(route: &Route, endpoint: &Url, token: &str) -> Option<Value> {
    let client = metadata_client(route, TIMEOUT)?;
    let target = match route {
        Route::Relay { url, .. } => url.as_str(),
        Route::Direct
            if endpoint.as_str() == OFFICIAL.as_str()
                && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") =>
        {
            FIXTURE
        }
        _ => endpoint.as_str(),
    };
    let mut response = route
        .relay(
            client
                .get(target)
                .bearer_auth(token)
                .header(reqwest::header::ACCEPT, "application/json")
                .header(reqwest::header::USER_AGENT, USER_AGENT.as_str()),
        )
        .send()
        .await
        .ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|size| size > BODY_LIMIT as u64)
    {
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

fn normalize(item: &Value) -> Option<Value> {
    let id = pick(item, &["slug", "id", "model", "name"])?
        .as_str()
        .map(js_trim)?;
    if id.is_empty() {
        return None;
    }
    let mut output = item.as_object()?.clone();
    let name = pick(item, &["display_name", "displayName", "name"])
        .map(js_text)
        .map_or_else(|| id.to_owned(), |value| js_trim(&value).to_owned());
    let modalities: Vec<_> = item["input_modalities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|kind| *kind == "text" || *kind == "image")
        .cloned()
        .collect();
    let vision = modalities.iter().any(|kind| kind == "image");
    let upstream = item["provider"].as_str().unwrap_or_default();
    let reasoning = item["reasoning"] == true;
    let kind = if vision { "imageToText" } else { "llm" };
    let mut capabilities = Map::from_iter([
        ("vision".into(), json!(vision)),
        ("reasoning".into(), json!(reasoning)),
    ]);
    if !upstream.is_empty() {
        capabilities.insert("upstreamProvider".into(), json!(upstream));
    }
    for (nested, camel, outer, capability) in [
        (
            "context_window",
            "contextLength",
            "context_length",
            "contextWindow",
        ),
        (
            "max_output_tokens",
            "maxOutputTokens",
            "max_output_tokens",
            "maxOutput",
        ),
    ] {
        let configured = item["limits"]
            .get(nested)
            .filter(|value| truthy(value))
            .or_else(|| pick(item, &[camel, outer]));
        if let Some(limit) = configured.and_then(number) {
            output.insert(camel.into(), limit.clone());
            capabilities.insert(capability.into(), limit);
        }
    }
    output.extend([
        ("id".into(), json!(id)),
        ("name".into(), json!(name)),
        ("provider".into(), json!(upstream)),
        ("upstreamProvider".into(), json!(upstream)),
        ("reasoning".into(), json!(reasoning)),
        ("inputModalities".into(), json!(modalities)),
        ("kind".into(), json!(kind)),
        ("type".into(), json!(kind)),
        ("capabilities".into(), Value::Object(capabilities)),
    ]);
    if upstream == "anthropic" {
        output.insert(
            "compat".into(),
            json!({
                "supportsReasoningEffort": false, "cacheControlFormat": "anthropic",
            }),
        );
    }
    Some(Value::Object(output))
}

fn pick<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a Value> {
    fields
        .iter()
        .find_map(|key| value.get(*key).filter(|value| truthy(value)))
}

fn number(raw: &Value) -> Option<Value> {
    let value = crate::thinking_intent::numeric(raw).or_else(|| {
        let rendered = js_text(raw);
        let text = js_trim(&rendered);
        for (prefixes, radix) in [(["0x", "0X"], 16), (["0o", "0O"], 8), (["0b", "0B"], 2)] {
            if let Some(value) = prefixes
                .into_iter()
                .find_map(|prefix| text.strip_prefix(prefix))
            {
                return u64::from_str_radix(value, radix)
                    .ok()
                    .map(|value| value as f64);
            }
        }
        text.parse::<f64>().ok().filter(|value| value.is_finite())
    })?;
    (value != 0.0).then(|| {
        if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
            json!(value as i64)
        } else {
            json!(value)
        }
    })
}

fn scrub(payload: &mut Value, secrets: &[&str]) {
    match payload {
        Value::Object(fields) => fields.retain(|key, value| {
            let label = key.to_ascii_lowercase().replace(['_', '-'], "");
            let hidden = matches!(
                label.as_str(),
                "authorization"
                    | "apikey"
                    | "accesstoken"
                    | "refreshtoken"
                    | "idtoken"
                    | "sessiontoken"
                    | "authtoken"
                    | "token"
                    | "clientsecret"
                    | "password"
                    | "privatekey"
                    | "secret"
                    | "cookie"
                    | "credentials"
            ) || secrets.iter().any(|secret| key.contains(secret));
            if !hidden {
                scrub(value, secrets);
            }
            !hidden
        }),
        Value::Array(entries) => entries.iter_mut().for_each(|entry| scrub(entry, secrets)),
        Value::String(text) => {
            for secret in secrets {
                if text.contains(secret) {
                    *text = text.replace(secret, "[redacted]");
                }
            }
        }
        _ => (),
    }
}
