use std::sync::OnceLock;

use serde_json::Value;

static INDEX: OnceLock<Vec<Value>> = OnceLock::new();

pub fn entries() -> &'static [Value] {
    INDEX.get_or_init(|| {
        serde_json::from_str(include_str!("../../provider-index.json"))
            .expect("embedded provider index")
    })
}

pub fn find(id: &str) -> Option<&'static Value> {
    entries().iter().find(|entry| entry["id"] == id)
}

pub fn normalize(id: &str) -> String {
    let name = js_trim(id);
    if find(name).is_some() {
        return name.to_owned();
    }
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(slug.trim_end_matches('-').len());
    if find(&slug).is_some() {
        return slug;
    }
    let lowercase = name.to_lowercase();
    entries()
        .iter()
        .find(|entry| {
            entry["name"]
                .as_str()
                .is_some_and(|name| name.to_lowercase() == lowercase)
        })
        .and_then(|entry| entry["id"].as_str())
        .unwrap_or(name)
        .to_owned()
}

pub fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Compatible {
    OpenAi,
    Anthropic,
    Embedding,
}

pub fn compatible(id: &str) -> Option<Compatible> {
    if id.starts_with("openai-compatible-") {
        Some(Compatible::OpenAi)
    } else if id.starts_with("anthropic-compatible-") {
        Some(Compatible::Anthropic)
    } else if id.starts_with("custom-embedding-") {
        Some(Compatible::Embedding)
    } else {
        None
    }
}
