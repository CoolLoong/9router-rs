use std::sync::LazyLock;

use chrono::DateTime;
use regex::Regex;
use reqwest::{RequestBuilder, StatusCode};
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::provider_connections::truthy;

const NEUTRAL_PROMPT: &str =
    "You are a helpful AI assistant that helps with software engineering tasks.";

static AGENT_IDENTITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)you are claude code|claude.?code.+official.+cli|anthropic.+official.+cli|anxthxropic.+official.+cli|you are (?:cursor|windsurf|cline|aider|continue|copilot|cody)|you are an? (?:ai )?(?:coding |code )?agent|cc_entrypoint\s*=\s*(?:cli|vscode|jetbrains|gui)|claude.?code.+issues|give feedback.+claude.?code|you are .{0,30}(?:powerful )?ai agent|orchestration capabilities|OhMyOpenCode|<agent-identity>|<Role>|<Behavior_Instructions>")
        .expect("CodeBuddy system prompt pattern")
});

static RATE_LIMIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("(?i)超出频率限制|frequency limit|限额").expect("CodeBuddy rate limit pattern")
});

static RESET_AT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)([0-9]{4}-[0-9]{2}-[0-9]{2})\s+([0-9]{2}:[0-9]{2}:[0-9]{2})(?:\s*UTC\+?([0-9:]+))?",
    )
    .expect("CodeBuddy reset time pattern")
});

pub(super) fn request_ids(request: RequestBuilder) -> RequestBuilder {
    let id = uuid::Uuid::new_v4().to_string();
    request
        .header("X-Request-ID", &id)
        .header("X-Conversation-ID", uuid::Uuid::new_v4().to_string())
        .header("X-Conversation-Request-ID", &id)
        .header("X-Conversation-Message-ID", &id)
}

pub(crate) fn is_provider(provider: &str) -> bool {
    matches!(provider, "codebuddy-cn" | "codebuddy-intl")
}

pub(crate) fn supports_account(account: &Value) -> bool {
    let present = |value: &Value| value.as_str().is_some_and(|token| !token.is_empty());
    match account["authType"].as_str() {
        Some("oauth") => present(&account["apiKey"]) || present(&account["accessToken"]),
        Some("apikey" | "api_key") => present(&account["apiKey"]),
        _ => false,
    }
}

pub(super) fn prepare(body: &mut Value, international: bool) {
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    fields.insert("stream".into(), Value::Bool(true));
    if let Some(effort) = fields.get("reasoning_effort") {
        if matches!(effort.as_str(), Some("none" | "off")) {
            fields.shift_remove("reasoning_effort");
        } else if truthy(effort) {
            fields.insert("reasoning_summary".into(), json!("auto"));
        }
    }
    if !international {
        if let Some(messages) = fields.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages
                .iter_mut()
                .filter(|message| message["role"] == "system")
            {
                let content = &mut message["content"];
                let text = match content {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .map(|part| part["text"].as_str().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => continue,
                };
                if text.encode_utf16().count() > 2000 || AGENT_IDENTITY.is_match(&text) {
                    *content = if content.is_string() {
                        json!(NEUTRAL_PROMPT)
                    } else {
                        json!([{"type":"text","text":NEUTRAL_PROMPT}])
                    };
                }
            }
        }
        return;
    }
    let source = match fields.shift_remove("messages") {
        Some(Value::Array(messages)) => messages,
        _ => Vec::new(),
    };
    let mut messages = Vec::with_capacity(source.len() + 1);
    messages.push(Value::Null);
    let mut system = Vec::new();
    for mut message in source.into_iter().filter(Value::is_object) {
        match message["role"].as_str() {
            Some("system" | "developer") => {
                let content = &message["content"];
                let text = if let Some(text) = content.as_str() {
                    text.to_owned()
                } else if let Some(parts) = content.as_array() {
                    parts
                        .iter()
                        .map(|part| &part["text"])
                        .map(|text| {
                            if truthy(text) {
                                js_text(text)
                            } else {
                                String::new()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    String::new()
                };
                if !text.is_empty() {
                    system.push(if AGENT_IDENTITY.is_match(&text) {
                        NEUTRAL_PROMPT.into()
                    } else {
                        text
                    });
                }
                continue;
            }
            Some("user") if let Some(text) = message["content"].as_str() => {
                message["content"] = json!([{"type":"text","text":text}]);
            }
            _ => (),
        }
        messages.push(message);
    }
    let system = if system.is_empty() {
        "You are CodeBuddy Code.".into()
    } else {
        system.join("\n\n")
    };
    messages[0] = json!({"role":"system","content":system});
    fields.insert("messages".into(), Value::Array(messages));
}

pub(crate) fn rate_limit(status: StatusCode, failure: &str) -> Option<(String, Option<i64>)> {
    let prefix = format!("[{}]: ", status.as_u16());
    let body: Value = serde_json::from_str(failure.strip_prefix(&prefix)?).ok()?;
    let message = [&body["msg"], &body["message"], &body["error"]["message"]]
        .into_iter()
        .find_map(|value| value.as_str().filter(|value| !value.is_empty()))
        .unwrap_or("");
    if body["code"].as_f64() != Some(6004.0) && !RATE_LIMIT.is_match(message) {
        return None;
    }
    let resets_at = RESET_AT.captures(message).and_then(|date| {
        let timezone = match date.get(3).map(|m| m.as_str()) {
            Some(offset) if offset.contains(':') => format!("+{offset}"),
            Some(offset) => format!("+{offset:0>2}:00"),
            None => "+08:00".into(),
        };
        let iso = format!("{}T{}{timezone}", &date[1], &date[2]);
        DateTime::parse_from_rfc3339(&iso)
            .ok()
            .map(|time| time.timestamp_millis())
    });
    let message = if message.is_empty() {
        "CodeBuddy frequency limit (6004)"
    } else {
        message
    };
    Some((format!("[429]: {message}"), resets_at))
}
