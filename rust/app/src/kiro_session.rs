use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::provider_index::js_trim;

pub(super) const FIELD: &str = "_nativeKiroSession";
const TTL: Duration = Duration::from_secs(7200);
const MAX_ENTRIES: usize = 5000;
const MAX_ENTRY_BYTES: usize = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const TIME_PREFIX: &str = "[Context: Current time is ";

#[derive(Default)]
struct Cache {
    entries: VecDeque<([u8; 32], Entry)>,
    bytes: usize,
}

struct Entry {
    first: Value,
    intent: [u8; 32],
    initial_request: String,
    last_used: Instant,
    bytes: usize,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(Cache::default()));

pub(super) fn scope(
    body: &Value,
    headers: &HeaderMap,
    owner: Option<&str>,
) -> Option<Option<String>> {
    let header = |name| headers.get(name)?.to_str().ok().and_then(session_id);
    let claude = body["metadata"]["user_id"]
        .as_str()
        .and_then(claude_session)
        .or_else(|| header("x-claude-code-session-id").map(str::to_owned));
    let session = claude.map(|session| ("claude", session)).or_else(|| {
        let value = [
            "x-session-id",
            "session-id",
            "session_id",
            "x-amp-thread-id",
        ]
        .into_iter()
        .find_map(header)
        .or_else(|| {
            ["prompt_cache_key", "session_id", "conversation_id"]
                .into_iter()
                .find_map(|name| body[name].as_str().and_then(session_id))
        })?;
        Some(("", value.to_owned()))
    });
    let Some((kind, session)) = session else {
        return Some(None);
    };
    let owner = owner.filter(|owner| !owner.is_empty())?;
    let tag = hash(&["kiro gateway session", owner, kind, &session]);
    Some(Some(
        tag.iter().map(|value| format!("{value:02x}")).collect(),
    ))
}

fn session_id(raw: &str) -> Option<&str> {
    let value = js_trim(raw);
    (!value.is_empty() && value.encode_utf16().take(257).count() <= 256).then_some(value)
}

fn claude_session(value: &str) -> Option<String> {
    if let Some((_, suffix)) = value.rsplit_once("_session_")
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f' | b'-'))
        && let Some(suffix) = session_id(suffix)
    {
        return Some(suffix.to_owned());
    }
    if !value.starts_with('{') {
        return None;
    }
    let value: Value = serde_json::from_str(value).ok()?;
    value["session_id"]
        .as_str()
        .and_then(session_id)
        .map(str::to_owned)
}

pub(super) fn replay(payload: &mut Value, scope: &str, account: &str, region: &str, grant: &str) {
    let key = hash(&["kiro upstream session", scope, account, region, grant]);
    let conversation = &mut payload["conversationState"];
    let request_id = conversation["conversationId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let mut uuid = [0; 16];
    uuid.copy_from_slice(&key[..16]);
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    conversation["conversationId"] = json!(Uuid::from_bytes(uuid).to_string());

    let history = conversation["history"]
        .as_array()
        .expect("prepared Kiro history");
    let original = history.first().unwrap_or(&conversation["currentMessage"]);
    let user = &original["userInputMessage"];
    let Some(intent) = user["content"]
        .as_str()
        .and_then(|text| text.split_once(TIME_PREFIX))
        .zip(user["modelId"].as_str())
        .map(|((prefix, _), model)| hash(&[prefix, model]))
    else {
        return;
    };
    let Ok(mut cache) = CACHE.lock() else { return };
    let now = Instant::now();
    if let Some((_, mut entry)) = cache
        .entries
        .iter()
        .position(|(old, _)| *old == key)
        .and_then(|index| cache.entries.remove(index))
    {
        cache.bytes -= entry.bytes;
        if entry.intent == intent && now.duration_since(entry.last_used) < TTL {
            if entry.initial_request != request_id {
                let history = conversation["history"]
                    .as_array_mut()
                    .expect("prepared Kiro history");
                if let Some(first) = history.first_mut() {
                    *first = entry.first.clone();
                } else {
                    history.push(entry.first.clone());
                    history.push(json!({"assistantResponseMessage":{"content":"..."}}));
                    let current =
                        &mut conversation["currentMessage"]["userInputMessage"]["content"];
                    if let Some(text) = current.as_str()
                        && let Some(start) = text.find(TIME_PREFIX)
                    {
                        *current = json!(&text[start..]);
                    }
                }
            }
            entry.last_used = now;
            cache.bytes += entry.bytes;
            cache.entries.push_front((key, entry));
            return;
        }
    }

    let mut first = original.clone();
    if let Some(user) = first["userInputMessage"].as_object_mut() {
        user.shift_remove("origin");
        user.shift_remove("userInputMessageContext");
    }
    let bytes = first.to_string().len();
    if bytes > MAX_ENTRY_BYTES {
        return;
    }
    while cache.entries.len() >= MAX_ENTRIES || cache.bytes + bytes > MAX_TOTAL_BYTES {
        let (_, removed) = cache
            .entries
            .pop_back()
            .expect("nonempty Kiro session cache");
        cache.bytes -= removed.bytes;
    }
    cache.bytes += bytes;
    cache.entries.push_front((
        key,
        Entry {
            first,
            intent,
            initial_request: request_id,
            last_used: now,
            bytes,
        },
    ));
}

fn hash(parts: &[&str]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finalize().into()
}
