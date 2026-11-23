use std::sync::OnceLock;

use axum::http::{HeaderMap, HeaderValue};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::chat_format::ChatFormat;
use crate::chat_input::{PreparedChat, js_space, truthy};
use crate::responses_request::{tool_arguments, tool_output, truncate_utf16};
use crate::{codex_executor, gemini_signatures};

static CATALOG: OnceLock<Value> = OnceLock::new();
static GO_SESSIONS: OnceLock<codex_executor::Sessions> = OnceLock::new();
pub(crate) const FREE_CLIENT_REQUIRED: &str =
    "OpenCode free chat models require the OpenCode client";

#[derive(Clone, Copy)]
pub struct Endpoint {
    pub wire: ChatFormat,
    transport: &'static Value,
    zen: bool,
    normalize_responses: bool,
}

pub fn is_provider(provider: &str) -> bool {
    matches!(provider, "opencode-go" | "opencode-zen")
}

pub(crate) fn requires_official_client(provider: &str, model: &str) -> bool {
    let (model, _) = crate::thinking_intent::model_suffix(model);
    provider == "opencode"
        || provider == "opencode-zen"
            && (model.eq_ignore_ascii_case("union-alpha")
                || model.to_ascii_lowercase().ends_with("-free"))
}

pub fn supports(account: &Value) -> bool {
    account["authType"] == "apikey"
        && token(account).is_some_and(|token| token != "public")
        && !truthy(&account["refreshToken"])
}

pub fn select(provider: &str, model: &str, client: ChatFormat) -> Option<Endpoint> {
    let zen = provider == "opencode-zen";
    if !is_provider(provider) || requires_official_client(provider, model) {
        return None;
    }
    let provider = CATALOG
        .get_or_init(|| {
            serde_json::from_str(include_str!("opencode-transport.json"))
                .expect("pinned OpenCode metadata")
        })
        .get(provider)?;
    let entry = provider["models"].get(model)?;
    if matches!(
        entry["kind"].as_str(),
        Some("systemone" | "tts" | "stt" | "image" | "video")
    ) {
        return None;
    }
    let supported = entry["supported"].as_array()?;
    let source = format_name(client);
    let format = if supported.iter().any(|format| format == source) {
        source
    } else if let Some(target) = entry["target"].as_str() {
        target
    } else if supported.iter().any(|format| format == "openai") {
        "openai"
    } else {
        return None;
    };
    let wire = match format {
        "openai" => ChatFormat::Chat,
        "claude" => ChatFormat::Messages,
        "openai-responses" => ChatFormat::Responses,
        _ => return None,
    };
    let transport = provider["transports"].get(format)?;
    let normalize_responses = if zen {
        model.starts_with("muse-spark")
    } else {
        entry["target"] == "openai-responses"
    };
    Some(Endpoint {
        wire,
        transport,
        zen,
        normalize_responses,
    })
}

fn format_name(format: ChatFormat) -> &'static str {
    match format {
        ChatFormat::Chat => "openai",
        ChatFormat::Messages => "claude",
        ChatFormat::Responses => "openai-responses",
    }
}

fn token(account: &Value) -> Option<&str> {
    ["apiKey", "accessToken"]
        .into_iter()
        .find_map(|field| account[field].as_str().filter(|value| !value.is_empty()))
}

impl Endpoint {
    pub fn url(self) -> &'static str {
        self.transport["url"]
            .as_str()
            .expect("pinned OpenCode endpoint")
    }

    pub fn fixture_url(self) -> Option<String> {
        (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"))
            .then(|| self.url().strip_prefix("https://opencode.ai"))
            .flatten()
            .map(|path| format!("http://127.0.0.1:52989/opencode{path}"))
    }

    pub fn thinking_format(self) -> Option<&'static str> {
        self.transport["thinkingFormat"].as_str()
    }

    pub fn force_stream(self) -> bool {
        self.zen
    }

    pub fn headers(
        self,
        mut request: reqwest::RequestBuilder,
        account: &Value,
        incoming: &HeaderMap,
        body: &Value,
    ) -> reqwest::RequestBuilder {
        let Some(key) = token(account) else {
            return request;
        };
        request = if self.wire == ChatFormat::Messages {
            if let Ok(mut value) = HeaderValue::from_str(key) {
                value.set_sensitive(true);
                request = request.header("x-api-key", value);
            }
            request.header("anthropic-version", "2023-06-01")
        } else {
            request.bearer_auth(key)
        };
        if let Some(session) = header(incoming, "x-opencode-session") {
            request = request.header("x-opencode-session", session);
        } else if !self.zen {
            let session = gemini_signatures::session_id(incoming, body).unwrap_or_else(|| {
                GO_SESSIONS
                    .get_or_init(codex_executor::Sessions::default)
                    .pick(body, account, incoming, None)
            });
            let hash = Sha256::digest(format!("opencode-go\0generic\0{session}").as_bytes());
            let mut translated = String::from("ses_");
            for byte in &hash[..16] {
                use std::fmt::Write;
                let _ = write!(translated, "{byte:02x}");
            }
            request = request.header("x-opencode-session", translated);
        }
        if self.zen {
            for name in ["x-opencode-client", "x-opencode-project"] {
                if let Some(value) = header(incoming, name) {
                    request = request.header(name, value);
                }
            }
        }
        request
    }

    pub fn prepare(self, prepared: &mut PreparedChat) -> Option<()> {
        let fields = prepared.body.as_object_mut()?;
        if self.zen {
            fields.insert("stream".into(), json!(true));
        }
        if self.normalize_responses && self.wire == ChatFormat::Responses {
            normalize_responses(fields);
        }
        Some(())
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let text = headers.get(name)?.to_str().ok()?.trim_matches(js_space);
    (!text.is_empty() && text.encode_utf16().count() <= 256).then_some(text)
}

fn normalize_responses(fields: &mut Map<String, Value>) {
    let input = fields.entry("input").or_insert(Value::Null);
    if input.as_array().is_none_or(Vec::is_empty) {
        let text = input
            .as_str()
            .filter(|text| !text.trim_matches(js_space).is_empty())
            .unwrap_or("...");
        *input =
            json!([{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}]);
    } else if let Some(items) = input.as_array_mut() {
        items.retain_mut(|item| {
            if item["type"] == "reasoning" {
                return false;
            }
            let Some(fields) = item.as_object_mut() else {
                return true;
            };
            fields.shift_remove("encrypted_content");
            fields.shift_remove("reasoning_encrypted_content");
            match fields.get("type").and_then(Value::as_str) {
                Some("function_call") => {
                    let Some(name) = fields
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|name| name.trim_matches(js_space))
                        .filter(|name| !name.is_empty())
                    else {
                        return false;
                    };
                    fields.insert("name".into(), json!(truncate_utf16(name, 128)));
                    normalize_call_id(fields);
                    let raw = fields.entry("arguments").or_insert(Value::Null);
                    *raw = json!(tool_arguments(raw));
                }
                Some("function_call_output") => {
                    normalize_call_id(fields);
                    let raw = fields.entry("output").or_insert(Value::Null);
                    *raw = json!(tool_output(raw));
                }
                _ => {}
            }
            true
        });
    }
    let completions = fields.shift_remove("max_completion_tokens");
    let tokens = fields.shift_remove("max_tokens");
    if let Some(cap) = completions.or(tokens) {
        fields.entry("max_output_tokens").or_insert(cap);
    }
    if let Some(effort) = fields.shift_remove("reasoning_effort") {
        fields
            .entry("reasoning")
            .or_insert_with(|| json!({"effort":effort,"summary":"auto"}));
    }
    if let Some(reasoning) = fields.get_mut("reasoning").and_then(Value::as_object_mut)
        && reasoning.get("summary").is_none_or(|value| !truthy(value))
    {
        reasoning.insert("summary".into(), json!("auto"));
    }
    fields.insert("stream".into(), json!(true));
    fields.insert("store".into(), json!(false));
    normalize_tools(fields);
}

fn normalize_call_id(fields: &mut Map<String, Value>) {
    let id = fields.entry("call_id").or_insert(Value::Null);
    *id = id.as_str().filter(|id| !id.is_empty()).map_or_else(
        || json!(format!("call_{}", uuid::Uuid::new_v4().simple())),
        |id| json!(truncate_utf16(id, 64)),
    );
}

fn normalize_tools(fields: &mut Map<String, Value>) {
    let Some(tools) = fields.get("tools").and_then(Value::as_array) else {
        return;
    };
    let mut normalized = Vec::with_capacity(tools.len());
    for tool in tools.iter() {
        let Some(name) = tool["name"]
            .as_str()
            .or_else(|| tool["function"]["name"].as_str())
            .map(|name| name.trim_matches(js_space))
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let mut parameters = [&tool["parameters"], &tool["function"]["parameters"]]
            .into_iter()
            .find(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| json!({"type":"object","properties":{}}));
        if parameters["type"] == "object" && !truthy(&parameters["properties"]) {
            parameters["properties"] = json!({});
        }
        let mut normalized_tool = json!({"type":"function","name":truncate_utf16(name, 128)});
        if let Some(description) = tool["description"]
            .as_str()
            .or_else(|| tool["function"]["description"].as_str())
            .filter(|text| !text.is_empty())
        {
            normalized_tool["description"] = json!(description);
        }
        normalized_tool["parameters"] = parameters;
        normalized.push(normalized_tool);
    }
    let choice = &fields.get("tool_choice").unwrap_or(&Value::Null);
    if choice["type"] == "function"
        && choice["name"].as_str().is_none_or(|name| {
            !normalized
                .iter()
                .any(|tool| tool["name"] == name.trim_matches(js_space))
        })
    {
        fields.shift_remove("tool_choice");
    }
    fields.insert("tools".into(), Value::Array(normalized));
}
