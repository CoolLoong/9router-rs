use std::collections::HashSet;

use provider_capabilities_rs::port::model_capabilities;
use rusqlite::Connection;
use serde_json::Value;

use crate::{key_policy, provider_connections::truthy};

const HARD: [&str; 4] = ["vision", "pdf", "audioInput", "videoInput"];
const DEFAULT_MODEL: &str = "oc/mimo-v2.6-flash-free";

pub struct Routing<'a> {
    pub required: &'a HashSet<&'static str>,
    pub key: Option<&'a str>,
    pub catalog: Option<&'a Value>,
}

impl Routing<'_> {
    pub fn candidates<'a>(
        &self,
        db: &Connection,
        settings: &'a Value,
        models: &[&str],
    ) -> rusqlite::Result<Vec<(&'a str, f64)>> {
        let needs: Vec<_> = HARD
            .into_iter()
            .filter(|kind| self.required.contains(kind))
            .collect();
        if needs.is_empty()
            || models.is_empty()
            || models
                .iter()
                .any(|model| satisfies(&capabilities(model, self.catalog), &needs))
        {
            return Ok(Vec::new());
        }
        let mut pool = Vec::new();
        let mut seen = HashSet::new();
        for capability in HARD {
            let Some((members, _)) = pool_for(&settings["capacityAdapter"][capability]) else {
                continue;
            };
            for model in members {
                if models.contains(&model) || !seen.insert(model) {
                    continue;
                }
                let caps = capabilities(model, self.catalog);
                if !satisfies(&caps, &needs) {
                    continue;
                }
                if let Some(key) = self.key
                    && !key_policy::permits_chat(db, key, model)?
                {
                    continue;
                }
                pool.push((
                    model,
                    caps["contextWindow"]
                        .as_f64()
                        .filter(|n| *n > 0.0)
                        .unwrap_or(200000.0),
                ));
            }
        }
        Ok(pool)
    }

    pub fn solo_limit(&self, settings: &Value) -> Option<Option<usize>> {
        let mut selected = None;
        for key in HARD.into_iter().filter(|key| self.required.contains(key)) {
            if let Some((_, round_robin)) = pool_for(&settings["capacityAdapter"][key]) {
                if selected.is_some_and(|first| first != round_robin) {
                    return None;
                }
                selected = Some(round_robin);
            }
        }
        Some((selected == Some(true)).then_some(1))
    }
}

fn pool_for(value: &Value) -> Option<(Vec<&str>, bool)> {
    let (options, round_robin) = if let Some(entries) = value.as_array() {
        (
            entries
                .iter()
                .map(|item| {
                    item.get("model")
                        .filter(|model| truthy(model))
                        .unwrap_or(item)
                })
                .collect::<Vec<_>>(),
            false,
        )
    } else if value.is_object() && value["enabled"] != false {
        (
            value["models"].as_array().into_iter().flatten().collect(),
            truthy(&value["roundRobin"]),
        )
    } else {
        return None;
    };
    let mut models: Vec<_> = options
        .into_iter()
        .filter_map(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(|model| {
            if model == "oc/mimo-v2.5-free" {
                DEFAULT_MODEL
            } else {
                model
            }
        })
        .collect();
    if models.is_empty() {
        models.push(DEFAULT_MODEL);
    }
    Some((models, round_robin))
}

fn capabilities(name: &str, catalog: Option<&Value>) -> Value {
    let (provider, model) = name
        .split_once('/')
        .filter(|(provider, _)| !provider.is_empty())
        .unwrap_or(("", name));
    model_capabilities(Some(provider), model, catalog)
}

fn satisfies(caps: &Value, needs: &[&str]) -> bool {
    needs.iter().all(|key| caps[*key] == true)
}

pub fn strip_history(mut body: Value, context_window: f64) -> Value {
    let field = ["messages", "input", "contents"]
        .into_iter()
        .find(|field| body[*field].is_array());
    let Some(field) = field else { return body };
    let Some(original) = body[field].as_array_mut() else {
        return body;
    };
    let (system, rest): (Vec<_>, Vec<_>) = original
        .iter()
        .partition(|item| matches!(item["role"].as_str(), Some("system" | "developer")));
    let Some(last_assistant) = rest
        .iter()
        .rposition(|item| matches!(item["role"].as_str(), Some("assistant" | "model")))
    else {
        return body;
    };
    let (older, tail) = rest.split_at(last_assistant + 1);
    let mut keep = older.len().min(6);
    let budget = context_window * 3.2;
    let mut chars: usize = system
        .iter()
        .chain(&older[..keep])
        .chain(tail)
        .map(|item| {
            block_length(
                item.get("content")
                    .filter(|content| !content.is_null())
                    .unwrap_or(&item["parts"]),
            )
        })
        .sum();
    while chars as f64 > budget && keep > 0 {
        keep -= 1;
        let removed = older[keep];
        chars -= block_length(
            removed
                .get("content")
                .filter(|content| !content.is_null())
                .unwrap_or(&removed["parts"]),
        );
    }
    if keep < older.len() {
        *original = system
            .into_iter()
            .chain(older[..keep].iter().copied())
            .chain(tail.iter().copied())
            .cloned()
            .collect();
    }
    body
}

fn block_length(value: &Value) -> usize {
    if let Some(text) = value.as_str() {
        return text.encode_utf16().count();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|block| {
            block["text"]
                .as_str()
                .map_or(50, |text| text.encode_utf16().count())
        })
        .sum()
}
