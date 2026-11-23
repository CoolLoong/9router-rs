use serde_json::{Value, json};

use crate::chat_input::{js_space, truthy};

pub enum Thinking {
    None,
    Auto,
    Level(String),
    Budget(f64),
}

pub fn model_suffix(model: &str) -> (&str, Option<Thinking>) {
    let Some((clean, token)) = model
        .trim_end_matches(js_space)
        .strip_suffix(')')
        .and_then(|base| base.rsplit_once('('))
        .filter(|(_, token)| !token.is_empty() && !token.contains(')'))
    else {
        return (model, None);
    };
    let token = token.trim_matches(js_space).to_ascii_lowercase();
    let intent = if !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit()) {
        token.parse().ok().map(Thinking::Budget)
    } else if matches!(token.as_str(), "none" | "off" | "auto" | "ultra")
        || level_budget(&token).is_some()
    {
        Some(parse_effort(&token))
    } else {
        None
    };
    (clean.trim_matches(js_space), intent)
}

fn parse_effort(level: &str) -> Thinking {
    match level.to_ascii_lowercase().as_str() {
        "none" | "off" => Thinking::None,
        "auto" => Thinking::Auto,
        level => Thinking::Level(level.to_owned()),
    }
}

pub fn thinking_intent(body: &Value) -> Option<Thinking> {
    for effort in [
        &body["output_config"]["effort"],
        body.get("reasoning_effort")
            .filter(|value| !value.is_null())
            .unwrap_or(&body["reasoning"]["effort"]),
    ] {
        if let Some(level) = effort.as_str().filter(|level| !level.is_empty()) {
            return Some(parse_effort(level));
        }
    }
    match body["thinking"]["type"].as_str() {
        Some("disabled") => return Some(Thinking::None),
        Some("enabled" | "adaptive") => {
            return Some(positive_budget(&body["thinking"]["budget_tokens"]));
        }
        _ => (),
    }
    for config in [
        &body["thinkingConfig"],
        &body["generationConfig"]["thinkingConfig"],
    ] {
        if truthy(config) {
            if let Some(level) = config["thinkingLevel"].as_str() {
                return Some(Thinking::Level(level.to_ascii_lowercase()));
            }
            if let Some(budget) = config.get("thinkingBudget").and_then(numeric) {
                return Some(if budget == 0.0 {
                    Thinking::None
                } else if budget < 0.0 {
                    Thinking::Auto
                } else {
                    Thinking::Budget(budget)
                });
            }
            break;
        }
    }
    match body["enable_thinking"].as_bool()? {
        true => Some(positive_budget(&body["thinking_budget"])),
        false => Some(Thinking::None),
    }
}

pub fn apply_provider_mode(body: &mut Value, config: &Value) {
    let Some(mode) = config["mode"]
        .as_str()
        .filter(|mode| !mode.is_empty() && *mode != "auto")
    else {
        return;
    };
    let Some(body) = body.as_object_mut() else {
        return;
    };
    let explicit_thinking = body.get("thinking").is_some_and(truthy);
    match mode {
        "on" if !explicit_thinking => {
            body.insert(
                "thinking".into(),
                json!({"type":"enabled","budget_tokens":10000}),
            );
        }
        "off" if !explicit_thinking => {
            body.insert("thinking".into(), json!({"type":"disabled"}));
        }
        _ if !body.get("reasoning_effort").is_some_and(truthy) => {
            body.insert("reasoning_effort".into(), json!(mode));
        }
        _ => (),
    }
}

fn positive_budget(value: &Value) -> Thinking {
    numeric(value)
        .filter(|budget| *budget > 0.0)
        .map(Thinking::Budget)
        .unwrap_or(Thinking::Auto)
}

pub fn numeric(value: &Value) -> Option<f64> {
    match value {
        Value::Null | Value::Bool(false) => Some(0.0),
        Value::Bool(true) => Some(1.0),
        Value::String(text) if text.trim_matches(js_space).is_empty() => Some(0.0),
        Value::String(text) => text
            .trim_matches(js_space)
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite()),
        _ => value.as_f64(),
    }
}

pub fn level_budget(level: &str) -> Option<f64> {
    Some(match level {
        "none" => 0.0,
        "minimal" => 512.0,
        "low" => 1024.0,
        "medium" => 8192.0,
        "high" => 24576.0,
        "xhigh" => 32768.0,
        "max" => 128000.0,
        _ => return None,
    })
}

pub fn budget_level(budget: f64) -> &'static str {
    if budget <= 0.0 {
        "medium"
    } else if budget <= 768.0 {
        "minimal"
    } else if budget <= 4096.0 {
        "low"
    } else if budget <= 16384.0 {
        "medium"
    } else if budget <= 28672.0 {
        "high"
    } else if budget <= 80384.0 {
        "xhigh"
    } else {
        "max"
    }
}
