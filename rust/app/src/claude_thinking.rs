use serde_json::{Value, json};

use crate::chat_input::truthy;
use crate::thinking_intent::{Thinking, budget_level, level_budget, numeric};
use crate::{openai_models, thinking_catalog};

const CONTROLS: &[&str] = &[
    "thinking",
    "reasoning_effort",
    "reasoning",
    "thinkingConfig",
    "enable_thinking",
    "thinking_budget",
    "output_config",
];

pub fn strip(body: &mut Value) {
    if let Some(fields) = body.as_object_mut() {
        for key in CONTROLS {
            fields.shift_remove(*key);
        }
        if let Some(config) = fields
            .get_mut("generationConfig")
            .and_then(Value::as_object_mut)
        {
            config.shift_remove("thinkingConfig");
        }
        if let Some(params) = fields.get_mut("params").and_then(Value::as_object_mut) {
            params.shift_remove("thinking");
            params.shift_remove("reasoning_effort");
        }
    }
}

pub fn capture_display(source: &Value) -> Option<String> {
    source["thinking"]["display"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            let reasoning = &source["reasoning"];
            if reasoning.is_object() {
                reasoning["summary"]
                    .as_str()
                    .filter(|value| !value.is_empty() && *value != "none")
                    .map(|_| "summarized".to_owned())
            } else {
                source["reasoning_effort"]
                    .is_string()
                    .then(|| "summarized".to_owned())
            }
        })
}

pub fn apply(
    body: &mut Value,
    provider: &str,
    model: &str,
    caps: &Value,
    intent: Option<Thinking>,
    display: Option<String>,
) {
    if caps["reasoning"] != true {
        strip(body);
        return;
    }
    let Some(intent) = intent else { return };
    let display = display.as_deref();
    let none = matches!(intent, Thinking::None);
    let can_disable = caps["thinkingCanDisable"] != false;
    strip(body);
    if none && can_disable {
        let style = caps["thinkingOffType"].as_str().unwrap_or("disabled");
        body["thinking"] = json!({"type":style});
        return;
    }
    if caps["thinkingFormat"] == "claude-adaptive" {
        let effort = match &intent {
            Thinking::None => "minimal",
            Thinking::Auto => "high",
            Thinking::Level(value) => value,
            Thinking::Budget(value) => budget_level(*value),
        };
        let supported = thinking_catalog::levels(provider, model, caps, openai_models::data());
        let effort = match effort {
            "minimal" => "low",
            "xhigh"
                if !supported
                    .as_ref()
                    .is_some_and(|levels| levels.iter().any(|level| level == "xhigh")) =>
            {
                "high"
            }
            other => other,
        };
        if can_disable
            || (display.is_some() && !none && model.to_ascii_lowercase().contains("claude"))
        {
            body["thinking"] = json!({"type":"adaptive"});
            if let Some(display) = display {
                body["thinking"]["display"] = json!(display);
            }
        }
        body["output_config"] = json!({"effort":effort});
        return;
    }

    let budget = match &intent {
        Thinking::None => Some(512.0),
        Thinking::Auto => Some(-1.0),
        Thinking::Level(level) => level_budget(level),
        Thinking::Budget(value) => Some(*value),
    };
    let budget = budget.map(|mut budget| {
        if budget != -1.0 {
            if let Some(min) = caps["thinkingRange"]["min"].as_f64() {
                budget = budget.max(min)
            }
            if let Some(max) = caps["thinkingRange"]["max"].as_f64() {
                budget = budget.min(max)
            }
        }
        budget
    });
    body["thinking"] = json!({"type":"enabled"});
    if let Some(display) = display {
        body["thinking"]["display"] = json!(display);
    }
    if budget != Some(-1.0) {
        let mut budget = budget.filter(|value| *value != 0.0).unwrap_or(8192.0);
        if truthy(&body["max_tokens"])
            && let Some(tokens) = numeric(&body["max_tokens"])
        {
            let ceiling = caps["maxOutput"]
                .as_f64()
                .filter(|max| *max > 0.0)
                .unwrap_or(64000.0);
            if budget >= tokens.min(ceiling) {
                let limit = (budget + 1024.0).min(ceiling);
                body["max_tokens"] = count(limit);
                if budget >= limit {
                    budget = (limit - 1024.0).max(1024.0)
                }
            }
        }
        body["thinking"]["budget_tokens"] = count(budget);
    }
}

fn count(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}
