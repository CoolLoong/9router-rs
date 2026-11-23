use std::sync::LazyLock;

use chrono::{Datelike, NaiveDate, Utc};
use reqwest::RequestBuilder;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::chat_input::{js_json, truthy};
use crate::provider_defaults;

const MONTHLY_LIMIT: &str = "you've reached your additional usage limit for your plan";
const BLOCKED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "connection",
    "transfer-encoding",
    "authorization",
    "cookie",
];

struct Metadata {
    url: &'static str,
    responses_url: &'static str,
    messages_url: &'static str,
    vscode: &'static str,
    editor: &'static str,
    user_agent: &'static str,
    github_api: &'static str,
    anthropic_api: &'static str,
}

static METADATA: LazyLock<Metadata> = LazyLock::new(|| {
    let required =
        |group, key| provider_defaults::text(group, key).expect("built-in GitHub request metadata");
    Metadata {
        url: required("github.transport", "baseUrl"),
        responses_url: required("github.transport", "responsesUrl"),
        messages_url: required("github.transport", "messagesUrl"),
        vscode: required("github.copilot", "vscodeVersion"),
        editor: required("github.copilot", "chatVersion"),
        user_agent: required("github.copilot", "userAgent"),
        github_api: required("github.copilot", "apiVersion"),
        anthropic_api: required("shared", "anthropicApiVersion"),
    }
});

pub(super) fn catalog_metadata() -> (&'static str, &'static str, &'static str, &'static str) {
    let metadata = &*METADATA;
    (
        metadata.vscode,
        metadata.editor,
        metadata.user_agent,
        metadata.github_api,
    )
}

pub fn url() -> &'static str {
    METADATA.url
}

pub(super) fn responses_url() -> &'static str {
    METADATA.responses_url
}

pub(super) fn messages_url() -> &'static str {
    METADATA.messages_url
}

pub fn fixture_url() -> Option<String> {
    (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"))
        .then(|| "http://127.0.0.1:52989/github/chat/completions".into())
}

pub fn needs_responses(model: &str, status: u16, error: &str) -> bool {
    let model = model.to_ascii_lowercase();
    status == 400
        && !model.contains("claude")
        && !model.contains("gemini")
        && (error.contains("not accessible via the /chat/completions endpoint")
            || error.contains("The requested model is not supported"))
}

pub fn monthly_reset(status: u16, message: &str, now_ms: i64) -> Option<i64> {
    if status != 402 || !message.to_ascii_lowercase().contains(MONTHLY_LIMIT) {
        return None;
    }
    let now = chrono::DateTime::<Utc>::from_timestamp_millis(now_ms)?;
    let (year, month) = match now.month() {
        12 => (now.year() + 1, 1),
        month => (now.year(), month + 1),
    };
    Some(
        NaiveDate::from_ymd_opt(year, month, 1)?
            .and_hms_opt(0, 0, 0)?
            .and_utc()
            .timestamp_millis(),
    )
}

pub fn token(account: &Value) -> Option<&str> {
    [
        &account["providerSpecificData"]["copilotToken"],
        &account["accessToken"],
    ]
    .into_iter()
    .find_map(|value| value.as_str().filter(|value| !value.is_empty()))
}

pub fn prepare(body: &mut Value, model: &str) -> Option<()> {
    let fields = body.as_object_mut()?;
    if let Some(messages) = fields
        .get_mut("messages")
        .filter(|messages| truthy(messages))
    {
        for message in messages.as_array_mut()? {
            let message = message.as_object_mut()?;
            let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            let mut clean = Vec::with_capacity(parts.len());
            for part in std::mem::take(parts) {
                if part.is_null() {
                    return None;
                }
                let part = if matches!(part["type"].as_str(), Some("text" | "image_url")) {
                    part
                } else {
                    let value = [&part["text"], &part["content"]]
                        .into_iter()
                        .find(|value| truthy(value));
                    let text = match value {
                        Some(Value::String(value)) => value.clone(),
                        Some(value) => js_json(value, false)?,
                        None => js_json(&part, false)?,
                    };
                    json!({"type":"text", "text":text})
                };
                if part["text"] != "" {
                    clean.push(part);
                }
            }
            message.insert(
                "content".into(),
                if clean.is_empty() {
                    Value::Null
                } else {
                    Value::Array(clean)
                },
            );
        }
    }
    let lower = model.to_ascii_lowercase();
    if ["gpt-5", "o1-", "o3-", "o4-"]
        .iter()
        .any(|marker| lower.contains(marker))
        && let Some(limit) = fields.shift_remove("max_tokens")
    {
        fields.insert("max_completion_tokens".into(), limit);
    }
    if fields
        .get("reasoning_effort")
        .is_some_and(|value| value == "none")
    {
        fields.shift_remove("reasoning_effort");
    }
    if lower.contains("gpt-5.4") {
        fields.shift_remove("temperature");
    }
    Some(())
}

pub fn headers(
    request: RequestBuilder,
    account: &Value,
    settings: &Value,
    stream: bool,
) -> RequestBuilder {
    let metadata = &*METADATA;
    let fields: Vec<(&str, String)> = vec![
        ("Content-Type", "application/json".into()),
        ("copilot-integration-id", "vscode-chat".into()),
        ("editor-version", format!("vscode/{}", metadata.vscode)),
        (
            "editor-plugin-version",
            format!("copilot-chat/{}", metadata.editor),
        ),
        ("user-agent", metadata.user_agent.into()),
        ("openai-intent", "conversation-panel".into()),
        ("x-github-api-version", metadata.github_api.into()),
        ("x-request-id", Uuid::new_v4().to_string()),
        (
            "x-vscode-user-agent-library-version",
            "electron-fetch".into(),
        ),
        ("X-Initiator", "user".into()),
        ("anthropic-version", metadata.anthropic_api.into()),
        (
            "Accept",
            if stream {
                "text/event-stream".into()
            } else {
                "application/json".into()
            },
        ),
    ];
    request
        .bearer_auth(token(account).unwrap_or("undefined"))
        .headers(configured_headers(fields, settings, "github"))
}

pub(super) fn configured_headers<'a>(
    mut fields: Vec<(&'a str, String)>,
    settings: &'a Value,
    provider: &str,
) -> HeaderMap {
    if let Some(overrides) = settings["providerOverrides"][provider]["headers"].as_object() {
        for (name, value) in overrides {
            let Some(value) = value
                .as_str()
                .filter(|value| !value.is_empty() && value.len() <= 8192)
            else {
                continue;
            };
            if BLOCKED_HEADERS
                .iter()
                .any(|blocked| name.eq_ignore_ascii_case(blocked))
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                || HeaderValue::from_str(value).is_err()
            {
                continue;
            }
            if let Some((_, prior)) = fields.iter_mut().find(|(key, _)| *key == name) {
                *prior = value.to_owned();
            } else {
                fields.push((name, value.to_owned()));
            }
        }
    }

    let mut headers = HeaderMap::new();
    for (name, value) in fields {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let value = match headers
            .get(&name)
            .and_then(|prior: &HeaderValue| prior.to_str().ok())
        {
            Some(prior) => format!("{prior}, {value}"),
            None => value,
        };
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    headers
}
