use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode, Version, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::cursor_identity::Account;
use crate::provider_http::Delivery;
use crate::provider_index::js_trim;

pub(crate) const ENDPOINT: &str =
    "https://agent.api5.cursor.sh/agent.v1.AgentService/GetUsableModels";
const FIXTURE: &str = "http://127.0.0.1:52982/agent.v1.AgentService/GetUsableModels";
const TTL: Duration = Duration::from_secs(300);
const DEADLINE: Duration = Duration::from_secs(10);
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
const MAX_ACCOUNTS: usize = 128;

type CacheKey = [u8; 32];
type Entry = Arc<AsyncMutex<Option<Cached>>>;
type Cache = VecDeque<(CacheKey, Entry)>;

struct Cached {
    expires: Instant,
    models: Vec<Value>,
}

struct Catalog {
    client: Client,
    cache: Mutex<Cache>,
}

static CATALOG: LazyLock<Option<Catalog>> = LazyLock::new(|| {
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http2_prior_knowledge()
        .timeout(DEADLINE)
        .build()
        .ok()?;
    Some(Catalog {
        client,
        cache: Mutex::default(),
    })
});

impl Account<'_> {
    fn key(&self, delivery: &Delivery) -> CacheKey {
        let mut hash = Sha256::new();
        hash.update((self.machine.len() as u32).to_be_bytes());
        hash.update(self.machine.as_bytes());
        hash.update(self.token.as_bytes());
        hash.update([u8::from(self.ghost)]);
        match delivery {
            Delivery::Direct => hash.update([0]),
            Delivery::Bypass => hash.update([1]),
            Delivery::Tunnel(url) => {
                hash.update([2]);
                hash.update(url.as_str());
            }
        }
        hash.finalize().into()
    }
}

pub(super) async fn models_routed(
    connection: &Value,
    force_refresh: bool,
    delivery: &Delivery,
) -> Option<Vec<Value>> {
    let account = Account::from_connection(connection)?;
    let catalog = CATALOG.as_ref()?;
    let slot = catalog.entry(account.key(delivery))?;
    let mut cached = slot.lock().await;
    let now = Instant::now();
    if !force_refresh && let Some(hit) = cached.as_ref().filter(|entry| entry.expires > now) {
        return Some(hit.models.clone());
    }
    let response = catalog.fetch(&account, delivery).await?;
    let mut models = parse(&response)?;
    models.retain(|row| !has_identity(&row["id"], &account));
    for row in &mut models {
        if has_identity(&row["name"], &account) {
            row["name"] = row["id"].clone();
        }
    }
    if models.is_empty() {
        return None;
    }
    *cached = Some(Cached {
        expires: now + TTL,
        models: models.clone(),
    });
    Some(models)
}

fn has_identity(field: &Value, account: &Account<'_>) -> bool {
    field
        .as_str()
        .is_some_and(|text| text.contains(account.token) || text.contains(account.machine))
}

impl Catalog {
    fn entry(&self, key: CacheKey) -> Option<Entry> {
        let mut cache = self.cache.lock().ok()?;
        let slot = cache
            .iter()
            .position(|(candidate, _)| *candidate == key)
            .and_then(|index| cache.remove(index))
            .map_or_else(|| Arc::new(AsyncMutex::new(None)), |(_, slot)| slot);
        cache.push_front((key, slot.clone()));
        cache.truncate(MAX_ACCOUNTS);
        Some(slot)
    }

    async fn fetch(&self, account: &Account<'_>, delivery: &Delivery) -> Option<Vec<u8>> {
        let endpoint = if delivery.use_fixture() {
            FIXTURE
        } else {
            ENDPOINT
        };
        let client = delivery.client(&self.client, DEADLINE)?;
        let mut response = client
            .post(endpoint)
            .version(Version::HTTP_2)
            .bearer_auth(account.token)
            .header(header::ACCEPT, "application/proto")
            .header(header::CONTENT_TYPE, "application/proto")
            .header("x-cursor-checksum", account.checksum()?)
            .header("x-ghost-mode", if account.ghost { "true" } else { "false" })
            .body(Vec::new())
            .send()
            .await
            .ok()?;
        if response.status() != StatusCode::OK
            || response.version() != Version::HTTP_2
            || response
                .content_length()
                .is_some_and(|size| size > MAX_RESPONSE as u64)
        {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if chunk.len() > MAX_RESPONSE - bytes.len() {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        Some(bytes)
    }
}

pub(crate) fn parse(message: &[u8]) -> Option<Vec<Value>> {
    let mut decoder = Decoder(message);
    let mut model_ids = HashSet::new();
    let mut models = Vec::new();
    let mut flags = HashMap::new();
    while !decoder.0.is_empty() {
        match decoder.field()? {
            (1, Wire::Bytes(bytes)) => {
                let [id, display, name, short] = first_fields(bytes, [1, 3, 4, 5])?.map(text);
                let id = js_trim(&id);
                if id.is_empty() || !model_ids.insert(id.to_owned()) {
                    continue;
                }
                let name = [&name, &short, &display]
                    .into_iter()
                    .find(|name| !name.is_empty())
                    .map_or(id, |name| js_trim(name));
                models.push(json!({"id":id,"name":name}));
            }
            (2, Wire::Bytes(bytes)) => {
                let [id, default, chat, agent, hidden] = first_fields(bytes, [1, 2, 4, 5, 35])?;
                let name = text(id);
                let name = js_trim(&name);
                if !name.is_empty() {
                    flags.entry(name.to_owned()).or_insert([
                        flag(default),
                        flag(chat),
                        flag(agent),
                        flag(hidden),
                    ]);
                }
            }
            _ => (),
        }
    }
    for row in &mut models {
        if let Some([default, chat, agent, hidden]) =
            row["id"].as_str().and_then(|id| flags.get(id))
        {
            row["defaultOn"] = json!(default);
            row["isChatOnly"] = json!(chat);
            row["supportsAgent"] = json!(agent);
            row["isHidden"] = json!(hidden);
        }
    }
    Some(models)
}

pub(crate) fn first_fields<const N: usize>(
    bytes: &[u8],
    ids: [u32; N],
) -> Option<[Option<Wire<'_>>; N]> {
    let mut decoder = Decoder(bytes);
    let mut seen = [None; N];
    while !decoder.0.is_empty() {
        let (id, value) = decoder.field()?;
        if let Some(index) = ids.iter().position(|candidate| *candidate == id) {
            seen[index].get_or_insert(value);
        }
    }
    Some(seen)
}

#[derive(Clone, Copy)]
pub(crate) enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn flag(value: Option<Wire<'_>>) -> bool {
    matches!(value, Some(Wire::Varint(1)))
}

fn text(value: Option<Wire<'_>>) -> std::borrow::Cow<'_, str> {
    match value {
        Some(Wire::Bytes(bytes)) => String::from_utf8_lossy(bytes),
        _ => "".into(),
    }
}

pub(crate) struct Decoder<'a>(pub(crate) &'a [u8]);

impl<'a> Decoder<'a> {
    pub(crate) fn field(&mut self) -> Option<(u32, Wire<'a>)> {
        let key = u32::try_from(self.varint()?).ok()?;
        let id = key >> 3;
        if id == 0 {
            return None;
        }
        let value = match key & 7 {
            0 => Wire::Varint(self.varint()?),
            1 => Wire::Bytes(self.bytes(8)?),
            2 => {
                let size = usize::try_from(self.varint()?).ok()?;
                Wire::Bytes(self.bytes(size)?)
            }
            5 => Wire::Bytes(self.bytes(4)?),
            _ => return None,
        };
        Some((id, value))
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0;
        for shift in (0..=63).step_by(7) {
            let (&byte, rest) = self.0.split_first()?;
            self.0 = rest;
            if shift == 63 && byte > 1 {
                return None;
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn bytes(&mut self, length: usize) -> Option<&'a [u8]> {
        let (value, tail) = self.0.split_at_checked(length)?;
        self.0 = tail;
        Some(value)
    }
}
