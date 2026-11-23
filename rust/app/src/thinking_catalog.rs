use serde_json::Value;

pub fn levels(provider: &str, model: &str, caps: &Value, data: &Value) -> Option<Vec<String>> {
    if caps["reasoning"] != true {
        return None;
    }
    let thinking = &data["thinking"];
    let base = strip_suffix(model);
    let custom = if provider == "codex" {
        data["models"]["cx"]
            .as_array()?
            .iter()
            .find(|entry| entry["id"] == base)
            .and_then(|entry| entry["thinkingLevels"].as_array())
    } else {
        None
    };
    let hit = thinking["patterns"]
        .as_array()?
        .iter()
        .find(|row| {
            row["provider"].as_str().is_none_or(|id| id == provider)
                && row["pattern"]
                    .as_str()
                    .is_some_and(|pattern| matches_pattern(pattern, model))
        })
        .and_then(|row| row["levels"].as_array());
    let compatible = provider.starts_with("openai-compatible-") && accepts_max(base);
    let levels = custom
        .or(hit)
        .or_else(|| {
            compatible
                .then(|| thinking["compatibleMax"].as_array())
                .flatten()
        })
        .or_else(|| {
            caps["thinkingFormat"]
                .as_str()
                .and_then(|format| thinking["formats"][format].as_array())
        })
        .or_else(|| thinking["defaults"].as_array())?;
    let levels = levels
        .iter()
        .filter_map(Value::as_str)
        .filter(|l| *l != "none" || caps["thinkingCanDisable"] != false)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    (!levels.is_empty()).then_some(levels)
}

fn strip_suffix(model: &str) -> &str {
    let trimmed = model.trim_end();
    if let Some(stripped) = trimmed.strip_suffix(')')
        && let Some((prefix, token)) = stripped.rsplit_once('(')
        && !token.is_empty()
        && !token.contains(')')
    {
        return prefix;
    }
    model
}

fn accepts_max(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let mut segments = lower.split('/').peekable();
    while let Some(segment) = segments.next() {
        if let Some(suffix) = segment
            .strip_prefix("gpt-5.6")
            .or_else(|| segment.strip_prefix("gpt-6"))
            && (suffix.starts_with(['.', '-']) || (suffix.is_empty() && segments.peek().is_none()))
        {
            return true;
        }
    }
    false
}

pub fn matches_pattern(pattern: &str, id: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let id = id.to_ascii_lowercase();
    if !pattern.contains('*') {
        return pattern == id;
    }
    let mut items = pattern.split('*').peekable();
    let mut text = &id[..];
    if let Some(prefix) = items.next() {
        let Some(rest) = text.strip_prefix(prefix) else {
            return false;
        };
        text = rest;
    }
    while let Some(part) = items.next() {
        if items.peek().is_none() {
            return text.ends_with(part);
        }
        let Some((_, rest)) = text.split_once(part) else {
            return false;
        };
        text = rest;
    }
    true
}
