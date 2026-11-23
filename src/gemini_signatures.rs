use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::gemini_upstream::ThoughtSignature;
use crate::storage::Storage;

static THREAD_SEED: LazyLock<(uuid::Uuid, i64)> =
    LazyLock::new(|| (uuid::Uuid::new_v4(), chrono::Utc::now().timestamp_millis()));

const MEMORY_TTL: Duration = Duration::from_secs(3600);
const MAX_IN_MEMORY: usize = 2000;
const PERSISTED_TTL_MS: i64 = 7 * 24 * 3600 * 1000;

#[derive(Clone, Default)]
pub struct GeminiSignatures {
    entries: Arc<Mutex<HashMap<String, Entry>>>,
}

struct Entry {
    signature: String,
    family: String,
    expires: Instant,
}

#[derive(Clone)]
pub struct SignatureContext {
    cache: GeminiSignatures,
    storage: Storage,
    session: Option<String>,
}

impl GeminiSignatures {
    pub fn context(&self, storage: Storage, headers: &HeaderMap, body: &Value) -> SignatureContext {
        SignatureContext {
            cache: self.clone(),
            storage,
            session: session_id(headers, body),
        }
    }
}

impl SignatureContext {
    pub fn lookup(&self, id: &str, model: &str) -> Option<String> {
        if id.is_empty() {
            return None;
        }
        let family = model_family(model);
        let entries = self.cache.entries.lock().ok()?;
        let now = Instant::now();
        let session_key = self
            .session
            .as_ref()
            .map(|session| format!("{session}:{id}"));
        session_key
            .as_deref()
            .into_iter()
            .chain([id])
            .find_map(|key| {
                let entry = entries.get(key)?;
                (entry.expires > now
                    && (entry.family.is_empty() || family.is_empty() || entry.family == family))
                    .then(|| entry.signature.clone())
            })
    }

    pub fn store(&self, events: Vec<ThoughtSignature>) {
        if events.is_empty() {
            return;
        }
        let mut rows = Vec::new();
        let now = Instant::now();
        let created = chrono::Utc::now().timestamp_millis();
        if let Ok(mut entries) = self.cache.entries.lock() {
            entries.retain(|_, entry| entry.expires > now);
            for event in events {
                let id = event.tool_call_id;
                let family = model_family(&event.model);
                let session_key = self
                    .session
                    .as_ref()
                    .map(|session| format!("{session}:{id}"));
                for key in session_key.into_iter().chain([id]) {
                    entries.insert(
                        key.clone(),
                        Entry {
                            signature: event.signature.clone(),
                            family: family.clone(),
                            expires: now + MEMORY_TTL,
                        },
                    );
                    rows.push((
                        key,
                        json!({
                            "signature": event.signature,
                            "family": family,
                            "createdAt": created,
                            "expiresAt": created + PERSISTED_TTL_MS,
                        })
                        .to_string(),
                    ));
                }
            }
            if entries.len() > MAX_IN_MEMORY {
                let mut sorted = entries
                    .iter()
                    .map(|(key, entry)| (entry.expires, key.clone()))
                    .collect::<Vec<_>>();
                sorted.sort_unstable();
                for (_, key) in sorted.into_iter().take(entries.len() - MAX_IN_MEMORY) {
                    entries.remove(&key);
                }
            }
        }
        let storage = self.storage.clone();
        tokio::spawn(async move {
            let _ = storage.run(true, move |db| {
                let tx = db.transaction()?;
                for (key, value) in rows {
                    tx.execute(
                        "INSERT INTO kv (scope, key, value) VALUES ('gemini_thought_signatures', ?1, ?2)
                         ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value
                         WHERE COALESCE(json_extract(kv.value, '$.createdAt'), 0) <= ?3",
                        rusqlite::params![key, value, created],
                    )?;
                }
                tx.commit()
            }).await;
        });
    }
}

fn model_family(model: &str) -> String {
    let model = model.to_ascii_lowercase();
    if model.contains("claude") {
        "claude".into()
    } else if model.contains("gemini") {
        "gemini".into()
    } else {
        model
    }
}

pub(crate) fn session_id(headers: &HeaderMap, body: &Value) -> Option<String> {
    let header = |key| {
        headers
            .get(key)
            .and_then(|value| value.to_str().ok())
            .and_then(normalize)
    };
    let metadata = body["metadata"]["user_id"].as_str();
    let claude = metadata
        .and_then(|value| {
            if let Some((_, tail)) = value.rsplit_once("_session_")
                && !tail.is_empty()
                && tail
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f' | b'-'))
            {
                return Some(tail.to_owned());
            }
            if !value.starts_with('{') {
                return None;
            }
            let parsed: Value = serde_json::from_str(value).ok()?;
            parsed["session_id"]
                .as_str()
                .and_then(normalize)
                .map(str::to_owned)
        })
        .or_else(|| header("x-claude-code-session-id").map(str::to_owned));
    if let Some(claude) = claude {
        return Some(format!("claude:{claude}"));
    }
    [
        "x-session-id",
        "session-id",
        "session_id",
        "x-amp-thread-id",
        "x-client-request-id",
    ]
    .into_iter()
    .find_map(header)
    .or_else(|| {
        ["prompt_cache_key", "session_id", "conversation_id"]
            .iter()
            .find_map(|key| body[*key].as_str().and_then(normalize))
    })
    .or_else(|| metadata.and_then(normalize))
    .map(str::to_owned)
}

fn normalize(value: &str) -> Option<&str> {
    let value = value.trim_matches(crate::chat_input::js_space);
    (!value.is_empty() && value.encode_utf16().count() <= 256).then_some(value)
}

pub(crate) fn assistant_prefix(source: &Value) -> Vec<u16> {
    const LIMIT: usize = 50;
    let messages = source["messages"]
        .as_array()
        .or_else(|| source["input"].as_array());
    let mut prefix = Vec::with_capacity(LIMIT);
    for message in messages
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "assistant")
    {
        let content = &message["content"];
        let text = content.as_str().into_iter().chain(
            content.as_array().into_iter().flatten().filter_map(|part| {
                part["text"]
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .or_else(|| part["output"].as_str())
            }),
        );
        for piece in text {
            prefix.extend(piece.encode_utf16().take(LIMIT - prefix.len()));
            if prefix.len() == LIMIT {
                return prefix;
            }
        }
    }
    prefix.clear();
    prefix
}

pub(crate) fn implicit_session(
    provider: &str,
    connection: &str,
    caller: Option<&str>,
    history: &[u16],
) -> String {
    let mut hash = Sha256::new();
    hash.update(THREAD_SEED.0.as_bytes());
    for scope in [provider, connection, caller.unwrap_or_default()] {
        hash.update((scope.len() as u64).to_le_bytes());
        hash.update(scope.as_bytes());
    }
    for unit in history {
        hash.update(unit.to_le_bytes());
    }
    let bytes = hash.finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&bytes[..16]);
    let id = uuid::Builder::from_random_bytes(id).into_uuid();
    format!("{id}{}", THREAD_SEED.1)
}
