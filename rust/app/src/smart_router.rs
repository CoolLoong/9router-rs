use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::chat_input::{js_space, truthy};
use crate::thinking_intent::numeric;

const TTL: Duration = Duration::from_secs(7200);
const CAPACITY: usize = 5000;
type StickySessions = HashMap<(String, String), (String, Instant)>;

static SESSIONS: OnceLock<Mutex<StickySessions>> = OnceLock::new();

#[derive(Clone)]
pub struct Config {
    pub router: String,
    pub policy: String,
    pub sticky: bool,
    pub threshold: f64,
}

impl Config {
    pub fn new(config: &Value) -> Self {
        let router = config["routerModel"]
            .as_str()
            .map(trim)
            .filter(|model| !model.is_empty())
            .unwrap_or("oc/jev-1.13-free")
            .to_owned();
        let policy = config["smartPolicy"]
            .as_str()
            .filter(|policy| !policy.is_empty())
            .unwrap_or("smart")
            .to_ascii_lowercase();
        let stored = config["confidenceThreshold"]
            .as_f64()
            .filter(|value| value.is_finite() && *value > 0.0);
        let threshold = match policy.as_str() {
            "smart-switch-70" => 0.7,
            "smart-switch-90" => 0.9,
            "smart" | "custom" => stored.unwrap_or(0.8),
            policy if policy.starts_with("smart-switch") => stored.unwrap_or(0.8),
            _ => 0.8,
        };
        Self {
            router,
            sticky: config["sessionSticky"] != false && policy != "per-turn",
            policy,
            threshold,
        }
    }

    pub fn strict(&self) -> bool {
        self.policy == "strict"
    }
}

pub struct Choice {
    pub model: String,
    pub confidence: Option<f64>,
    probabilities: HashMap<String, f64>,
}

impl Choice {
    pub fn parse(value: &Value, models: &[String]) -> Option<Self> {
        let answer = &value["answers"]["route"];
        let selected = trim(answer["choice"].as_str()?);
        if !models.iter().any(|model| model == selected) {
            return None;
        }
        let probability = answer["probabilities"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.iter())
            .filter_map(|(model, number)| numeric(number).map(|number| (model.clone(), number)))
            .collect();
        Some(Self {
            model: selected.into(),
            confidence: answer["confidence"].as_f64(),
            probabilities: probability,
        })
    }

    pub fn prioritize<T>(&self, members: &mut Vec<(String, T)>) {
        let Some(first) = members.iter().position(|(model, _)| model == &self.model) else {
            return;
        };
        let selected = members.remove(first);
        members.retain(|(model, _)| model != &self.model);
        members.sort_by(|(a, _), (b, _)| {
            let probability = |model| self.probabilities.get(model).copied().unwrap_or(0.0);
            probability(b).total_cmp(&probability(a))
        });
        members.insert(0, selected);
    }
}

pub fn prioritize_sticky<T>(model: &str, members: &mut Vec<(String, T)>) {
    if let Some(first) = members.iter().position(|(candidate, _)| candidate == model) {
        let selected = members.remove(first);
        members.retain(|(candidate, _)| candidate != model);
        members.insert(0, selected);
    }
}

pub fn criteria(models: &[String]) -> Map<String, Value> {
    models
        .iter()
        .map(|model| trim(model))
        .filter(|model| !model.is_empty())
        .map(|model| (model.to_owned(), Value::Null))
        .collect()
}

pub fn prompt(body: &Value) -> String {
    let messages = body["messages"]
        .as_array()
        .or_else(|| body["input"].as_array());
    if let Some(messages) = messages.filter(|messages| !messages.is_empty()) {
        for message in messages.iter().rev() {
            if message["role"] == "user"
                || message["type"] == "message"
                || (!truthy(&message["role"]) && truthy(&message["content"]))
            {
                let text = contents(message);
                if !trim(&text).is_empty() {
                    return units(trim(&text), 2000);
                }
            }
        }
        if let Some(message) = messages.last() {
            let text = contents(message);
            if !text.is_empty() {
                return units(trim(&text), 2000);
            }
        }
    }
    for field in ["prompt", "input"] {
        if let Some(text) = body[field]
            .as_str()
            .map(trim)
            .filter(|text| !text.is_empty())
        {
            return units(text, 2000);
        }
    }
    String::new()
}

fn contents(message: &Value) -> String {
    let content = message
        .get("content")
        .filter(|value| !value.is_null())
        .unwrap_or(&message["text"]);
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.as_str()
                    .or_else(|| {
                        (part["type"] == "text")
                            .then(|| part["text"].as_str())
                            .flatten()
                    })
                    .filter(|text| !text.is_empty())
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

pub fn session(headers: &HeaderMap, body: &Value) -> Option<String> {
    crate::gemini_signatures::session_id(headers, body).or_else(|| {
        let messages = body["messages"]
            .as_array()
            .or_else(|| body["input"].as_array())?;
        let text = messages.iter().find_map(|message| {
            (message["role"] == "assistant")
                .then(|| contents(message))
                .filter(|text| text.encode_utf16().count() >= 50)
        })?;
        let first = units(&text, 50);
        Some(format!("{:x}", Sha256::digest(first.as_bytes()))[..16].to_owned())
    })
}

pub fn get(combo: &str, session: &str, models: &[String]) -> Option<String> {
    let key = (combo.to_owned(), session.to_owned());
    let mut slots = SESSIONS.get_or_init(Mutex::default).lock().ok()?;
    let (model, time) = slots.get_mut(&key)?;
    if time.elapsed() > TTL || !models.contains(model) {
        slots.remove(&key);
        return None;
    }
    *time = Instant::now();
    Some(model.clone())
}

pub fn assign(combo: &str, session: &str, model: &str) {
    if let Ok(mut slots) = SESSIONS.get_or_init(Mutex::default).lock() {
        if slots.len() >= CAPACITY {
            slots.retain(|_, (_, time)| time.elapsed() <= TTL);
            if slots.len() >= CAPACITY {
                let mut oldest: Vec<_> = slots
                    .iter()
                    .map(|(key, (_, time))| (key.clone(), *time))
                    .collect();
                oldest.sort_by_key(|(_, time)| *time);
                for (key, _) in oldest.into_iter().take(CAPACITY / 4) {
                    slots.remove(&key);
                }
            }
        }
        slots.insert(
            (combo.to_owned(), session.to_owned()),
            (model.to_owned(), Instant::now()),
        );
    }
}

pub fn clear(combo: Option<&str>, session: Option<&str>) {
    if let Some(store) = SESSIONS.get()
        && let Ok(mut slots) = store.lock()
    {
        slots.retain(|(name, id), _| {
            combo.is_some_and(|value| value != name) || session.is_some_and(|value| value != id)
        });
    }
}

fn trim(text: &str) -> &str {
    text.trim_matches(js_space)
}

fn units(text: &str, limit: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(limit).collect::<Vec<_>>())
}
