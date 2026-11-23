use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};

use crate::anthropic_request::{self, RegistryMessages};
use crate::chat_format::ChatFormat;
use crate::chat_input::{PreparedChat, js_space};
use crate::thinking_intent::model_suffix;

#[path = "codebuddy_chat.rs"]
pub(crate) mod codebuddy;

static TRANSPORTS: OnceLock<Value> = OnceLock::new();

#[derive(Clone, Copy)]
pub struct Endpoint {
    transport: &'static Value,
    model: &'static Value,
    pub wire: ChatFormat,
}

pub fn available(provider: &str) -> bool {
    registry().get(provider).is_some()
}

pub(crate) fn primary(provider: &str) -> Option<Endpoint> {
    default_endpoint(provider).filter(|endpoint| endpoint.wire != ChatFormat::Messages)
}

pub(crate) fn default_endpoint(provider: &str) -> Option<Endpoint> {
    let provider = registry().get(provider)?;
    endpoint(&provider["primary"], &Value::Null)
}

pub(crate) fn openai_thinking_format(provider: &str) -> Option<&'static str> {
    let provider = registry().get(provider)?;
    provider["overrides"]["openai"]["thinkingFormat"]
        .as_str()
        .or_else(|| provider["primary"]["thinkingFormat"].as_str())
}

pub fn select(provider: &str, model: &str, source: ChatFormat) -> Option<Endpoint> {
    let provider = registry().get(provider)?;
    let model = &provider["models"][model_suffix(model).0];
    if model["strip"]
        .as_array()
        .is_some_and(|strip| !strip.is_empty())
        || matches!(model["kind"].as_str(), Some("tts" | "stt"))
        || model["upstream"]
            .as_str()
            .is_some_and(|name| name.contains('('))
    {
        return None;
    }
    let source = match source {
        ChatFormat::Chat => "openai",
        ChatFormat::Messages => "claude",
        ChatFormat::Responses => "openai-responses",
    };
    let to = model["to"].as_str();
    let overrides = &provider["overrides"];
    let use_source = model["supported"]
        .as_array()
        .is_none_or(|formats| formats.iter().any(|format| format == source));
    let transport = if use_source && overrides.get(source).is_some() {
        overrides.get(source)?
    } else if let Some(to) = to {
        overrides
            .get(to)
            .or_else(|| (provider["primary"]["format"] == to).then_some(&provider["primary"]))?
    } else {
        &provider["primary"]
    };
    endpoint(transport, model)
}

pub(super) fn muse_subscription(account: &Value) -> bool {
    account["accessToken"].as_str().is_some_and(|token| {
        token
            .strip_prefix("LLM|")
            .is_some_and(|key| !key.is_empty())
    })
}

fn endpoint(transport: &'static Value, model: &'static Value) -> Option<Endpoint> {
    let wire = match transport["format"].as_str()? {
        "openai" => ChatFormat::Chat,
        "openai-responses" | "responses" => ChatFormat::Responses,
        "claude" => ChatFormat::Messages,
        _ => return None,
    };
    Some(Endpoint {
        transport,
        model,
        wire,
    })
}

fn registry() -> &'static Value {
    TRANSPORTS.get_or_init(|| {
        serde_json::from_str(include_str!("chat-transports.json"))
            .expect("embedded provider transports")
    })
}

impl Endpoint {
    pub(crate) fn has_reasoning_scope(self) -> bool {
        self.transport["reasoningScope"].is_string()
    }

    pub fn url(self, account: &Value) -> Option<String> {
        let url = account["providerSpecificData"]["region"]
            .as_str()
            .and_then(|region| self.transport["regionalUrls"][region].as_str())
            .or_else(|| self.transport["url"].as_str())?;
        if !url.contains("{accountId}") {
            return Some(url.to_owned());
        }
        let account_id = account["providerSpecificData"]["accountId"]
            .as_str()?
            .trim_matches(js_space);
        if account_id.is_empty() {
            return None;
        }
        Some(url.replacen("{accountId}", account_id, 1))
    }

    pub fn force_stream(self) -> bool {
        self.transport["forceStream"] == true
    }

    pub fn preserve_cache_control(self) -> bool {
        self.transport["preserveCacheControl"] == true
    }

    pub fn thinking_format(self) -> Option<&'static str> {
        self.transport["thinkingFormat"].as_str()
    }

    pub fn timeout(self, default: Duration) -> Duration {
        self.transport["timeoutMs"]
            .as_u64()
            .filter(|ms| *ms > 0)
            .map_or(default, Duration::from_millis)
    }

    pub fn upstream_model(self, requested: &str) -> &str {
        if model_suffix(requested).0 != requested {
            return requested;
        }
        self.model["upstream"].as_str().unwrap_or(requested)
    }

    pub(crate) fn prepare_messages(
        self,
        client: ChatFormat,
        body: Value,
        provider: &str,
        requested: &str,
        accept: &str,
    ) -> Option<PreparedChat> {
        let (base, _) = model_suffix(requested);
        let rewritten;
        let model = if let Some(upstream) = self.model["upstream"].as_str() {
            if base == requested {
                upstream
            } else {
                let (_, suffix) = requested.rsplit_once('(')?;
                rewritten = format!("{upstream}({suffix}");
                &rewritten
            }
        } else {
            requested
        };
        anthropic_request::prepare_registered(
            client,
            body,
            model,
            accept,
            RegistryMessages {
                provider,
                thinking_format: self.thinking_format(),
                tool_types: self.transport["claudeSupportedToolTypes"]
                    .as_array()
                    .map(Vec::as_slice),
                drop_output_config: self.transport["dropOutputConfig"] == true,
                require_tool_type: self.transport["requireClaudeToolType"] == true,
            },
        )
    }

    pub(crate) fn fixture_url(self, provider: &str, account: &Value) -> Option<String> {
        if provider == "xiaomi-mimo"
            && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        {
            let path = self.transport["url"]
                .as_str()?
                .strip_prefix("https://api.xiaomimimo.com")?;
            return Some(format!("http://127.0.0.1:52989/xiaomi-chat/cloud{path}"));
        }
        if provider == "xiaomi-tokenplan"
            && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        {
            let original = url::Url::parse(&self.url(account)?).ok()?;
            let region = original
                .host_str()?
                .strip_prefix("token-plan-")?
                .strip_suffix(".xiaomimimo.com")?;
            return Some(format!(
                "http://127.0.0.1:52989/xiaomi-chat/{region}{}",
                original.path()
            ));
        }
        if provider == "muse" && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        {
            let path = self.transport["url"]
                .as_str()?
                .strip_prefix("https://api.meta.ai")?;
            return Some(format!("http://127.0.0.1:52989/muse-chat{path}"));
        }
        if provider == "kimi" && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        {
            let path = match self.wire {
                ChatFormat::Chat => "chat/completions",
                ChatFormat::Messages => "messages?beta=true",
                ChatFormat::Responses => "responses",
            };
            return Some(format!("http://127.0.0.1:52989/kimi-chat/{path}"));
        }
        if self.wire != ChatFormat::Messages {
            return fixture_url(provider);
        }
        let enabled = std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1");
        if !enabled
            || !matches!(
                provider,
                "deepseek"
                    | "glm"
                    | "minimax"
                    | "minimax-cn"
                    | "minimax-code"
                    | "minimax-code-global"
            )
        {
            return None;
        }
        let suffix = self.transport["url"]
            .as_str()?
            .split_once('?')
            .map_or("", |(_, suffix)| suffix);
        let suffix = if suffix.is_empty() {
            String::new()
        } else {
            format!("?{suffix}")
        };
        Some(format!(
            "http://127.0.0.1:52989/static-chat/messages/{provider}{suffix}"
        ))
    }

    pub(crate) fn builtin_headers(
        self,
        mut request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        for (name, value) in self.transport["headers"].as_object().into_iter().flatten() {
            if let Some(value) = value.as_str() {
                request = request.header(name.as_str(), value);
            }
        }
        request
    }

    pub fn auth(
        self,
        mut request: reqwest::RequestBuilder,
        api_key: &str,
        account: &Value,
    ) -> reqwest::RequestBuilder {
        request = self.builtin_headers(request);
        if self.transport["authHook"] == "codebuddyHeaders" {
            request = codebuddy::request_ids(request);
        }
        if self.transport["authHook"] == "kimiHeaders"
            && let Some(headers) = crate::provider_oauth::kimi_portal::saved_device_id(account)
                .and_then(crate::provider_oauth::kimi_portal::device_headers)
        {
            request = request.headers(headers);
        }
        if self.transport["authHook"] == "museHeaders"
            && account["apiKey"].as_str().is_none_or(str::is_empty)
            && account["accessToken"]
                .as_str()
                .is_some_and(|token| !token.is_empty())
        {
            request = request.header("x-api-version", "1.0.0");
        }
        if self.transport["authHook"] == "minimaxHeaders" {
            let timezone = chrono::Local::now().offset().local_minus_utc();
            request = request
                .header("anthropic-version", "2023-06-01")
                .header("x-api-key", "sk-xxx")
                .header("X-Mavis-Timezone-Offset", timezone.to_string())
                .header("X-Mavis-Session-Id", uuid::Uuid::new_v4().to_string());
        }
        let name = self.transport["authHeader"]
            .as_str()
            .unwrap_or("authorization");
        if api_key == "undefined" && self.transport["combinedAuth"] != true {
            request
        } else if self.transport["bearer"] == true {
            request.header(name, format!("Bearer {api_key}"))
        } else {
            request.header(name, api_key)
        }
    }

    pub fn prepare(self, provider: &str, model: &str, prepared: &mut PreparedChat) {
        if codebuddy::is_provider(provider) {
            codebuddy::prepare(&mut prepared.body, provider == "codebuddy-intl");
        }
        let body = &mut prepared.body;
        if let Some(fields) = body.as_object_mut() {
            if self.transport["dropClientMetadata"] == true {
                fields.shift_remove("client_metadata");
            }
            if provider == "openai"
                && let Some(tokens) = fields.shift_remove("max_tokens")
            {
                fields.entry("max_completion_tokens").or_insert(tokens);
            }
            if provider == "nvidia" {
                for key in [
                    "client_metadata",
                    "prompt_cache_key",
                    "service_tier",
                    "anthropic_beta",
                    "thinking_budget",
                    "user_id",
                    "custom_field",
                ] {
                    fields.shift_remove(key);
                }
            }
            if provider == "volcengine-ark" {
                let id = model.to_ascii_lowercase();
                if id.contains("glm-5") || id.contains("kimi") {
                    let cap = provider_capabilities_rs::port::model_capabilities(
                        Some(provider),
                        model,
                        None,
                    )["maxOutput"]
                        .as_i64();
                    let limit = if id.contains("kimi") {
                        Some(cap.map_or(32768, |cap| cap.min(32768)))
                    } else {
                        cap
                    };
                    if let Some(limit) = limit {
                        for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
                            if fields
                                .get(key)
                                .and_then(Value::as_f64)
                                .is_some_and(|tokens| tokens > limit as f64)
                            {
                                fields.insert(key.into(), json!(limit));
                            }
                        }
                    }
                }
            }
        }
        if let Some(messages) = body["messages"].as_array_mut() {
            for message in messages {
                if provider == "cloudflare-ai"
                    && let Some(parts) = message["content"].as_array()
                {
                    let text: String = parts
                        .iter()
                        .filter(|part| part["type"] == "text")
                        .filter_map(|part| part["text"].as_str())
                        .collect();
                    message["content"] = json!(text);
                }
                if message["role"] != "assistant" {
                    continue;
                }
                if matches!(provider, "groq" | "mistral" | "cerebras") {
                    if let Some(fields) = message.as_object_mut() {
                        for key in ["reasoning_content", "reasoning", "reasoning_details"] {
                            fields.shift_remove(key);
                        }
                    }
                } else if provider == "nvidia"
                    && let Some(fields) = message.as_object_mut()
                    && let Some(value) = fields.shift_remove("reasoning")
                    && fields.get("reasoning_content").is_none_or(|current| {
                        current.is_null()
                            || current
                                .as_str()
                                .is_some_and(|text| text.trim_matches(js_space).is_empty())
                    })
                {
                    fields.insert("reasoning_content".into(), value);
                }
                if (self.transport["reasoningScope"] == "all"
                    || self.transport["reasoningScope"] == "toolCalls"
                        && message["tool_calls"]
                            .as_array()
                            .is_some_and(|calls| !calls.is_empty()))
                    && message["reasoning_content"]
                        .as_str()
                        .is_none_or(str::is_empty)
                {
                    message["reasoning_content"] = json!(" ");
                }
            }
        }
        if provider == "deepseek" {
            let config = match model {
                "deepseek-v4-pro-max" => Some(("enabled", Some("max"))),
                "deepseek-v4-pro-none" => Some(("disabled", None)),
                _ => None,
            };
            if let Some((mode, effort)) = config {
                let settings = &mut body["extra_body"];
                if !settings.is_object() {
                    *settings = json!({});
                }
                settings["thinking"] = json!({ "type": mode });
                if let Some(effort) = effort {
                    body["reasoning_effort"] = json!(effort);
                } else if let Some(fields) = body.as_object_mut() {
                    fields.shift_remove("reasoning_effort");
                }
            }
        }
    }
}

pub fn fixture_url(provider: &str) -> Option<String> {
    let enabled = std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1");
    (enabled
        && matches!(
            provider,
            "openai"
                | "xai"
                | "groq"
                | "openrouter"
                | "deepseek"
                | "perplexity-agent"
                | "kimchi"
                | "codebuddy-cn"
                | "codebuddy-intl"
                | "minimax"
                | "minimax-cn"
                | "nous"
                | "alicode"
                | "alicode-intl"
                | "alims-intl"
                | "alitp-intl"
                | "sensenova"
                | "sensenova-intl"
        ))
    .then(|| format!("http://127.0.0.1:52989/static-chat/{provider}"))
}
