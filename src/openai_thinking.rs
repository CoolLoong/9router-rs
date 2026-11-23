use provider_capabilities_rs::port::model_capabilities;
use serde_json::{Value, json};

use crate::chat_input::truthy;
use crate::thinking_intent::{Thinking, budget_level, level_budget, model_suffix, thinking_intent};
use crate::{claude_thinking, openai_models, thinking_catalog};

pub struct Controls {
    intent: Thinking,
    native_format: String,
    caps: Value,
    levels: Option<Vec<String>>,
    budget_passthrough: bool,
}

pub fn capture<'a>(
    body: &mut Value,
    provider: &str,
    model: &'a str,
    override_format: Option<&str>,
    responses: bool,
) -> (&'a str, Option<Controls>) {
    let (model, suffix) = model_suffix(model);
    let caps = model_capabilities(Some(provider), model, None);
    if caps["reasoning"] != true {
        claude_thinking::strip(body);
        return (model, None);
    }
    if body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .is_none_or(|message| !matches!(message["role"].as_str(), Some("user" | "tool")))
        && let Some(fields) = body.as_object_mut()
    {
        fields.shift_remove("thinking");
    }
    let Some(intent) = suffix.or_else(|| thinking_intent(body)) else {
        return (model, None);
    };
    claude_thinking::strip(body);
    let fallback = if responses {
        "openai-responses"
    } else {
        "openai"
    };
    let native = override_format
        .or_else(|| {
            caps["thinkingFormat"].as_str().filter(|format| {
                !matches!(
                    *format,
                    "gemini-level" | "gemini-budget" | "claude-budget" | "claude-adaptive" | "kiro"
                )
            })
        })
        .unwrap_or(fallback)
        .to_owned();
    let levels = thinking_catalog::levels(provider, model, &caps, openai_models::data());
    (
        model,
        Some(Controls {
            intent,
            native_format: native,
            caps,
            levels,
            budget_passthrough: provider.starts_with("openai-compatible-"),
        }),
    )
}

impl Controls {
    fn allows(&self, level: &str) -> bool {
        self.levels
            .as_ref()
            .is_some_and(|levels| levels.iter().any(|known| known == level))
    }

    pub fn write(self, body: &mut Value) {
        let none = matches!(self.intent, Thinking::None);
        let disabled = none && self.caps["thinkingCanDisable"] != false;
        let level = match &self.intent {
            Thinking::None => "minimal",
            Thinking::Auto => "auto",
            Thinking::Level(level) => level,
            Thinking::Budget(budget) => budget_level(*budget),
        };
        match self.native_format.as_str() {
            "openai" => {
                body["reasoning_effort"] = json!(if disabled {
                    "none"
                } else {
                    self.openai_level(level)
                });
            }
            "openai-responses" if !disabled => {
                body["reasoning"] = json!({"effort":self.openai_level(level),"summary":"auto"});
            }
            "zai" if disabled => body["enable_thinking"] = json!(false),
            "zai" => {
                body["thinking"] = json!({"type":"enabled"});
                if truthy(&self.caps["thinkingEffortSupported"]) {
                    let effort = match level {
                        "low" | "minimal" => "low",
                        "medium" | "high" => "high",
                        _ => "max",
                    };
                    body["reasoning_effort"] = json!(effort);
                }
            }
            "qwen" => {
                body["enable_thinking"] = json!(!disabled);
                if !disabled && let Some(budget) = self.budget(level).filter(|value| *value > 0.0) {
                    body["thinking_budget"] = count(budget);
                }
            }
            "deepseek" => {
                body["thinking"] = json!({"type":if disabled {"disabled"} else {"enabled"}});
                if !disabled {
                    if self.budget_passthrough
                        && let Thinking::Budget(budget) = &self.intent
                        && *budget > 0.0
                    {
                        body["thinking"]["budget_tokens"] = count(*budget);
                    }
                    let effort = match level {
                        "xhigh" | "max" if self.levels.is_none() || self.allows("max") => "max",
                        "low" | "minimal" if self.allows("low") => "low",
                        _ => "high",
                    };
                    body["reasoning_effort"] = json!(effort);
                }
            }
            "kimi" if disabled => body["thinking"] = json!({"type":"disabled"}),
            "kimi" => {
                if let Some(effort) = match level {
                    "auto" => Some("high"),
                    "minimal" => Some("low"),
                    "xhigh" => Some("max"),
                    "low" | "medium" | "high" | "max" => Some(level),
                    _ => None,
                } {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            "minimax" => {
                body["thinking"] = json!({"type":if disabled {"disabled"} else {"adaptive"}})
            }
            "hunyuan" => {
                body["thinking"] = json!({"type":if disabled {"disabled"} else {"enabled"}});
                if !disabled {
                    let budget = self
                        .budget(level)
                        .filter(|value| *value != 0.0)
                        .unwrap_or(8192.0);
                    if budget != -1.0 {
                        body["thinking"]["budget_tokens"] = count(budget);
                    }
                }
            }
            "step" if !disabled => {
                body["reasoning_effort"] = json!(if matches!(level, "xhigh" | "max") {
                    "high"
                } else {
                    level
                })
            }
            "nous" if !disabled => {
                body["reasoning"] =
                    json!({"enabled":true,"effort":if level=="auto" {"medium"} else {level}});
            }
            "tokenrouter" if !none && level != "auto" => body["reasoning_effort"] = json!(level),
            "commandcode" if !disabled => {
                let value = match level {
                    "auto" => "high",
                    "minimal" => "low",
                    value => value,
                };
                if ["off", "low", "medium", "high", "xhigh", "max"].contains(&value) {
                    if !body["params"].is_object() {
                        body["params"] = json!({})
                    }
                    body["params"]["reasoning_effort"] = json!(value);
                }
            }
            _ => (),
        }
    }

    fn openai_level<'a>(&self, level: &'a str) -> &'a str {
        if !matches!(level, "max" | "ultra") || self.allows(level) {
            level
        } else if level == "ultra" && self.allows("max") {
            "max"
        } else {
            "xhigh"
        }
    }

    fn budget(&self, level: &str) -> Option<f64> {
        let mut budget = match self.intent {
            Thinking::Auto => return Some(-1.0),
            Thinking::Budget(value) => value,
            _ => level_budget(level)?,
        };
        if let Some(minimum) = self.caps["thinkingRange"]["min"].as_f64() {
            budget = budget.max(minimum)
        }
        if let Some(maximum) = self.caps["thinkingRange"]["max"].as_f64() {
            budget = budget.min(maximum)
        }
        Some(budget)
    }
}

fn count(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}
