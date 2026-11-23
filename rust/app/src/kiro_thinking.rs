use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::provider_index::js_trim;
use crate::thinking_intent::{Thinking, level_budget, model_suffix, thinking_intent};

const DEFAULT_BUDGET: f64 = 16_000.0;
const SOURCE: &str = include_str!("../../../open-sse/config/kiroConstants.js");

pub(super) fn base_model(raw: &str) -> Option<&str> {
    let (model, intent) = model_suffix(raw);
    if (raw != model && intent.is_none()) || model.contains(['(', ')']) || model.is_empty() {
        return None;
    }
    let clean = model.strip_suffix("-agentic").unwrap_or(model);
    let clean = clean.strip_suffix("-thinking").unwrap_or(clean);
    Some(clean)
}

pub(super) fn settings(
    raw_model: &str,
    model: &str,
    body: &Value,
    headers: &HeaderMap,
) -> (String, Option<Value>) {
    let (requested, override_value) = model_suffix(raw_model);
    let choose_effort = [
        &body["output_config"]["effort"],
        &body["reasoning_effort"],
        &body["reasoning"]["effort"],
    ]
    .into_iter()
    .find(|effort| !effort.is_null())
    .and_then(Value::as_str);
    let effort = match &override_value {
        Some(Thinking::Level(level)) => Some(level.as_str()),
        None => choose_effort,
        _ => None,
    }
    .map(str::to_ascii_lowercase);
    let explicit = override_value.or_else(|| thinking_intent(body));
    let budget = match explicit {
        Some(Thinking::None) => None,
        Some(Thinking::Level(level)) if level == "disabled" => None,
        Some(Thinking::Level(level)) => Some(level_budget(&level).unwrap_or(DEFAULT_BUDGET)),
        Some(Thinking::Budget(budget)) => Some(if budget == 0.0 {
            DEFAULT_BUDGET
        } else {
            budget
        }),
        Some(Thinking::Auto) => Some(DEFAULT_BUDGET),
        None => {
            let model = requested.to_ascii_lowercase();
            (model.contains("thinking")
                || model.contains("-reason")
                || headers
                    .get("anthropic-beta")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| {
                        value.to_ascii_lowercase().contains("interleaved-thinking")
                    }))
            .then_some(DEFAULT_BUDGET)
        }
    };
    let gpt = model == "gpt-5.6" || model.starts_with("gpt-5.6-");
    let additional = effort
        .as_deref()
        .and_then(|effort| fields(model, effort, gpt));
    let mut prefix = String::new();
    if let Some(budget) = budget
        && !(gpt && additional.is_some())
    {
        prefix = format!(
            "<thinking_mode>enabled</thinking_mode>\n<max_thinking_length>{}</max_thinking_length>",
            budget.clamp(1.0, 32_000.0)
        );
    }
    if requested.ends_with("-agentic")
        && let Some((_, content)) = SOURCE.split_once("export const KIRO_AGENTIC_SYSTEM_PROMPT = `")
        && let Some((prompt, _)) = content.split_once('`')
    {
        if !prefix.is_empty() {
            prefix.push_str("\n\n");
        }
        prefix.push_str(js_trim(prompt));
    }
    (prefix, additional)
}

fn fields(model: &str, effort: &str, gpt: bool) -> Option<Value> {
    if !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") {
        return None;
    }
    if gpt {
        let effort = if effort == "max" { "xhigh" } else { effort };
        return Some(json!({"reasoning":{"effort":effort}}));
    }
    let version = model.strip_prefix("claude-")?.rsplit('-').next()?;
    let (major, minor) = version
        .split_once('.')
        .map_or((version, None), |(major, minor)| {
            (major, minor.parse::<u32>().ok())
        });
    let major = major.parse::<u32>().ok()?;
    if major < 4 || major == 4 && minor.is_none_or(|minor| minor <= 5 || minor >= 1000) {
        return None;
    }
    let effort = if effort == "xhigh" && major == 4 && minor.is_some_and(|minor| minor <= 6) {
        "high"
    } else {
        effort
    };
    Some(
        json!({"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":effort}}),
    )
}
