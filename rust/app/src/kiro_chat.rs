use std::borrow::Cow;
use std::collections::HashSet;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header};
use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::stream;
use reqwest::{Body, RequestBuilder, Response, ResponseBuilderExt};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::bedrock_transport::eventstream::{Frame, decode_frame, frame_length};
use crate::kiro_models::{self, OAuthMethod};
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

#[path = "kiro_session.rs"]
mod session;
#[path = "kiro_thinking.rs"]
mod thinking;
#[path = "kiro_tools.rs"]
mod tools;

const ENDPOINT: &str = "/generateAssistantResponse";
const EVENT_STREAM: &str = "application/vnd.amazon.eventstream";
const MAX_REPLY: usize = 8 * 1024 * 1024;
const DONE: &str = "data: [DONE]\n\n";
const REFRESH_LEAD_MS: f64 = 300_000.0;
const CODEWHISPERER_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";

pub(super) fn strip_model_media(body: &mut Value, model: &str) {
    if !matches!(
        thinking::base_model(model),
        Some("deepseek-3.2" | "qwen3-coder-next")
    ) {
        return;
    }
    for field in ["messages", "input"] {
        for message in body
            .get_mut(field)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            parts.retain(|part| {
                !matches!(
                    part["type"].as_str(),
                    Some("image_url" | "image" | "input_image" | "audio_url" | "input_audio")
                )
            });
            if parts.is_empty() {
                message["content"] = json!("");
            }
        }
    }
}

pub fn supports(account: &Value) -> bool {
    Connection::api_key(account).is_some()
        || Connection::candidate(account, false)
            .is_some_and(|_| oauth_remaining_ms(account).is_none_or(|time| time >= REFRESH_LEAD_MS))
}

pub fn needs_refresh(account: &Value, tunneled: bool) -> bool {
    Connection::candidate(account, tunneled).is_some_and(|connection| connection.is_oauth())
        && oauth_remaining_ms(account).is_some_and(|time| time < REFRESH_LEAD_MS)
}

pub struct Connection<'a> {
    bearer: &'a str,
    region: &'a str,
    kind: Credential<'a>,
}

enum Credential<'a> {
    ApiKey,
    OAuth {
        profile: Option<&'a str>,
        method: OAuthMethod,
    },
}

impl<'a> Connection<'a> {
    pub(crate) fn api_key(account: &'a Value) -> Option<Self> {
        let (bearer, region) = kiro_models::api_key_target(account)?;
        (chat_region(region) && nonempty(&account["apiKey"]).is_none_or(|key| key == bearer))
            .then_some(Self {
                bearer,
                region,
                kind: Credential::ApiKey,
            })
    }

    pub fn from_account(account: &'a Value) -> Option<Self> {
        let connection = Self::candidate(account, false)?;
        (!connection.is_oauth() || oauth_remaining_ms(account).is_none_or(|time| time > 0.0))
            .then_some(connection)
    }

    pub(crate) fn for_proxy(account: &'a Value) -> Option<Self> {
        if !oauth_remaining_ms(account).is_some_and(|time| time > 0.0) {
            return None;
        }
        Self::candidate(account, true)
    }

    pub(crate) fn candidate(account: &'a Value, tunnel: bool) -> Option<Self> {
        let details = &account["providerSpecificData"];
        if !tunnel && truthy(&details["strictProxy"]) {
            return None;
        }
        if !tunnel && let Some(connection) = Self::api_key(account) {
            return Some(connection);
        }
        if account["provider"] != "kiro"
            || account["authType"] != "oauth"
            || truthy(&account["apiKey"])
            || !tunnel && requests_proxy(account)
        {
            return None;
        }
        let method = OAuthMethod::saved(details)?;
        let profile = match &details["profileArn"] {
            Value::String(_) => nonempty(&details["profileArn"]),
            Value::Null => None,
            _ => return None,
        };
        if tunnel && (profile.is_none() || method == OAuthMethod::Unspecified) {
            return None;
        }
        let issuer_region = match &details["region"] {
            Value::Null => kiro_models::DEFAULT_REGION,
            Value::String(_) => nonempty(&details["region"]).unwrap_or(kiro_models::DEFAULT_REGION),
            _ => return None,
        };
        let region = kiro_models::oauth_runtime_region(issuer_region, method, details, profile)?;
        let bearer = nonempty(&account["accessToken"])?;
        bearer
            .bytes()
            .all(|byte| byte.is_ascii_graphic())
            .then_some(Self {
                bearer,
                region,
                kind: Credential::OAuth { profile, method },
            })
    }

    fn endpoint(&self, service: &str) -> String {
        let region = self.region;
        if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
            format!("http://127.0.0.1:52989/kiro/{service}/{region}{ENDPOINT}")
        } else {
            format!("https://{service}.{region}.amazonaws.com{ENDPOINT}")
        }
    }

    pub fn url(&self) -> String {
        self.endpoint("q")
    }

    pub(crate) fn upstream_url(&self) -> String {
        q_endpoint(self.region)
    }

    pub fn alternate(&self, status: StatusCode) -> Option<(String, String)> {
        (self.is_oauth()
            && self.region == "us-east-1"
            && matches!(
                status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
            ))
        .then(|| {
            (
                format!(
                    "https://codewhisperer.{}.amazonaws.com{ENDPOINT}",
                    self.region
                ),
                self.endpoint("codewhisperer"),
            )
        })
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self.kind, Credential::OAuth { .. })
    }

    pub fn body<'b>(&self, body: &'b Value, account_id: Option<&str>) -> Cow<'b, Value> {
        if matches!(self.kind, Credential::ApiKey) && body.get(session::FIELD).is_none() {
            return Cow::Borrowed(body);
        }
        let mut body = body.clone();
        let scope = body
            .as_object_mut()
            .and_then(|fields| fields.shift_remove(session::FIELD));
        let grant = match self.kind {
            Credential::OAuth { profile, method } => {
                if let Some(profile) = profile.or_else(|| method.default_profile()) {
                    body["profileArn"] = json!(profile);
                }
                profile.unwrap_or(self.bearer)
            }
            Credential::ApiKey => self.bearer,
        };
        if let Some((scope, account_id)) = scope.as_ref().and_then(Value::as_str).zip(account_id) {
            session::replay(&mut body, scope, account_id, self.region, grant);
        }
        Cow::Owned(body)
    }

    pub fn headers(&self, request: RequestBuilder, codewhisperer: bool) -> RequestBuilder {
        let mut request = request
            .bearer_auth(self.bearer)
            .header(header::ACCEPT, EVENT_STREAM)
            .header(header::USER_AGENT, kiro_models::user_agent());
        match self.kind {
            Credential::ApiKey => request.header("TokenType", "API_KEY"),
            Credential::OAuth { profile, method } => {
                let mut sso =
                    header::HeaderValue::from_str(self.bearer).expect("validated Kiro bearer");
                sso.set_sensitive(true);
                request = request.header("x-amz-sso-bearer", sso);
                if let Some(profile) = profile {
                    request = request.header("x-amzn-codewhisperer-profile-arn", profile);
                }
                if method == OAuthMethod::ExternalIdp {
                    request = request.header("TokenType", "EXTERNAL_IDP");
                }
                if codewhisperer {
                    request = request.header("X-Amz-Target", CODEWHISPERER_TARGET);
                }
                request
            }
        }
    }
}

pub(crate) fn requests_proxy(account: &Value) -> bool {
    let details = &account["providerSpecificData"];
    truthy(&details["strictProxy"])
        || truthy(&details["connectionProxyEnabled"])
        || truthy(&details["vercelRelayUrl"])
        || truthy(&details["proxyPoolId"]) && nonempty(&details["proxyPoolId"]) != Some("__none__")
}

#[derive(PartialEq, Eq)]
pub(crate) struct OAuthRoute {
    pub(crate) endpoint: String,
    route: crate::proxy_transport::Route,
    protected: bool,
}

impl OAuthRoute {
    pub(crate) async fn select(
        storage: &crate::storage::Storage,
        settings: &Value,
        account: &Value,
    ) -> Result<Self, ()> {
        use crate::provider_http;
        use crate::proxy_transport::{Route, Selection};
        let endpoint = Connection::candidate(account, true)
            .or_else(|| Connection::candidate(account, false))
            .map_or_else(
                || proxy_target(account),
                |connection| connection.upstream_url(),
            );
        let selected = provider_http::select(storage, settings, account, &endpoint)
            .await
            .map_err(|_| ())?;
        let (route, protected) = match selected {
            Selection::ExplicitBypass => (Route::Direct, true),
            Selection::Routed(Route::Direct)
                if !provider_http::requires_protection(settings, account) =>
            {
                (Route::Direct, false)
            }
            Selection::Routed(Route::Proxy { url, .. }) => {
                let proxy = crate::proxy_transport::tunnel_proxy_url(&url).ok_or(())?;
                (
                    Route::Proxy {
                        url: proxy.to_string(),
                        strict: true,
                    },
                    true,
                )
            }
            Selection::Routed(route @ Route::Relay { .. }) => (route, true),
            _ => return Err(()),
        };
        Ok(Self {
            endpoint,
            route,
            protected,
        })
    }

    pub(crate) fn protected(&self) -> bool {
        self.protected
    }

    pub(super) fn tunneled(&self) -> bool {
        matches!(self.route, crate::proxy_transport::Route::Proxy { .. })
    }

    pub(crate) fn supports_refresh(&self, account: &Value) -> bool {
        if self.route.relay_identity().is_some() {
            crate::provider_oauth::kiro_relay_refresh_supported(account)
        } else {
            crate::provider_oauth::kiro_refresh_supported(account, self.protected)
        }
    }

    pub(super) fn supports_protected(&self, account: &Value) -> bool {
        self.connection(account).is_some()
            || needs_refresh(account, self.protected) && self.supports_refresh(account)
    }

    pub(crate) fn refresh_mode(
        &self,
        direct: crate::provider_oauth::RefreshMode,
    ) -> crate::provider_oauth::RefreshMode {
        use crate::provider_oauth::RefreshMode;
        if let Some(identity) = self.route.relay_identity() {
            RefreshMode::RelayRouted(identity)
        } else if self.protected {
            RefreshMode::KiroProtected
        } else {
            direct
        }
    }

    pub(crate) fn relay_secret(&self) -> Option<&str> {
        self.route.relay_secret()
    }

    pub(crate) fn connection<'a>(&self, account: &'a Value) -> Option<Connection<'a>> {
        if self.protected {
            Connection::for_proxy(account)
        } else {
            Connection::from_account(account)
        }
    }

    pub(crate) fn request_route(&self) -> crate::proxy_transport::Route {
        self.route.clone()
    }

    pub(crate) async fn verify(&self, storage: &crate::storage::Storage, account: &Value) -> bool {
        let Some(settings) = storage.read_settings().await else {
            return false;
        };
        matches!(Self::select(storage, &settings, account).await, Ok(current) if &current == self)
    }
}

fn proxy_target(account: &Value) -> String {
    let region = nonempty(&account["providerSpecificData"]["region"])
        .filter(|region| chat_region(region))
        .unwrap_or(kiro_models::DEFAULT_REGION);
    q_endpoint(region)
}

fn q_endpoint(region: &str) -> String {
    format!("https://q.{region}.amazonaws.com{ENDPOINT}")
}

pub(crate) fn proxy_client(address: &str, endpoint: &str) -> Option<(reqwest::Client, String)> {
    let builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .proxy(reqwest::Proxy::all(address).ok()?);
    #[cfg(debug_assertions)]
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        && reqwest::Url::parse(address).is_ok_and(|url| url.host_str() == Some("127.0.0.1"))
        && let Some(cert) = std::env::var_os("NINEROUTER_KIRO_LOCAL_PROXY_CA")
    {
        let pem = std::fs::read(cert).ok()?;
        let client = builder
            .add_root_certificate(reqwest::Certificate::from_pem(&pem).ok()?)
            .build()
            .ok()?;
        return Some((client, format!("https://localhost:52987{ENDPOINT}")));
    }
    Some((builder.build().ok()?, endpoint.to_owned()))
}

fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(js_trim)
        .filter(|value| !value.is_empty())
}

fn chat_region(region: &str) -> bool {
    matches!(region, "us-east-1" | "eu-central-1")
}

pub(super) fn profile_for_region(profile: &str, region: &str) -> bool {
    let Some((saved_region, resource)) = profile
        .strip_prefix("arn:aws:codewhisperer:")
        .and_then(|value| value.split_once(':'))
    else {
        return false;
    };
    let Some((account, id)) = resource.split_once(":profile/") else {
        return false;
    };
    saved_region == region
        && account.len() == 12
        && account.bytes().all(|byte| byte.is_ascii_digit())
        && !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub(super) fn oauth_remaining_ms(account: &Value) -> Option<f64> {
    let expiry = account
        .get("expiresAt")
        .filter(|value| !value.is_null())
        .unwrap_or(&account["tokenExpiresAt"]);
    let expires_ms = if let Some(epoch) = expiry.as_f64() {
        Some(if epoch < 1e12 { epoch * 1000.0 } else { epoch })
    } else {
        nonempty(expiry)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.timestamp_millis() as f64)
    };
    expires_ms.map(|time| time - Utc::now().timestamp_millis() as f64)
}

pub fn prepare(
    body: &Value,
    model: &str,
    incoming: &HeaderMap,
    owner: Option<&str>,
) -> Option<Value> {
    let scope = session::scope(body, incoming, owner)?;
    if body.as_object()?.keys().any(|key| {
        !matches!(
            key.as_str(),
            "model"
                | "messages"
                | "stream"
                | "stream_options"
                | "temperature"
                | "top_p"
                | "max_tokens"
                | "max_completion_tokens"
                | "n"
                | "tools"
                | "tool_choice"
                | "thinking"
                | "output_config"
                | "reasoning_effort"
                | "reasoning"
                | "thinkingConfig"
                | "generationConfig"
                | "enable_thinking"
                | "thinking_budget"
                | "prompt_cache_key"
                | "session_id"
                | "conversation_id"
                | "metadata"
        )
    }) || body.get("n").is_some_and(|n| !n.is_null() && n != 1)
        || body
            .get("metadata")
            .filter(|data| !data.is_null())
            .is_some_and(|data| {
                data.as_object()
                    .is_none_or(|object| object.keys().any(|key| key != "user_id"))
            })
    {
        return None;
    }
    let raw_model = model;
    let model = upstream_model(raw_model)?;
    let (system_prefix, fields) = thinking::settings(raw_model, model, body, incoming);
    let specs = tools::specs(body)?;
    let mut groups: Vec<InputTurn> = Vec::new();
    for message in body["messages"].as_array()? {
        let role = message["role"].as_str()?;
        if message.as_object()?.keys().any(|key| {
            !matches!(key.as_str(), "role" | "content")
                && !(role == "assistant" && key == "tool_calls")
                && !(role == "tool"
                    && matches!(key.as_str(), "tool_call_id" | "is_error" | "status"))
        }) {
            return None;
        }
        let assistant = match role {
            "assistant" => true,
            "system" | "user" | "tool" => false,
            _ => return None,
        };
        let attachments: Vec<_> = if role == "tool" {
            vec![tools::result(message)?]
        } else if assistant && let Some(calls) = message.get("tool_calls") {
            calls
                .as_array()?
                .iter()
                .map(|call| tools::call(call, &specs))
                .collect::<Option<_>>()?
        } else {
            Vec::new()
        };
        let (mut text, images) = if role == "tool"
            || assistant && !attachments.is_empty() && message["content"].is_null()
        {
            (String::new(), Vec::new())
        } else {
            content(&message["content"], !assistant)?
        };
        if text.contains("<thinking_mode>") {
            return None;
        }
        if assistant {
            text = js_trim(&text).to_owned();
        } else if role == "system" && !text.is_empty() {
            text = format!("<instructions>\n{text}\n</instructions>");
        }
        if !groups
            .last()
            .is_some_and(|group| group.assistant == assistant)
        {
            groups.push(InputTurn {
                assistant,
                ..Default::default()
            });
        } else if assistant && (!attachments.is_empty() || !groups.last()?.attachments.is_empty()) {
            return None;
        }
        let group = groups.last_mut()?;
        group.attachments.extend(attachments);
        group.images.extend(images);
        if !text.is_empty() {
            group.parts.push(text);
        }
    }
    if groups.first()?.assistant
        || groups.last()?.assistant
        || !body["messages"]
            .as_array()?
            .iter()
            .any(|message| message["role"] == "user")
    {
        return None;
    }
    let now = Utc::now();
    let timestamp = format!(
        "[Context: Current time is {}]",
        now.to_rfc3339_opts(SecondsFormat::Millis, true)
    );
    let first_prefix = if system_prefix.is_empty() {
        timestamp.clone()
    } else {
        format!("{system_prefix}\n\n{timestamp}")
    };
    let mut history = Vec::new();
    let mut prior_calls = Vec::new();
    let mut ids = HashSet::new();
    let mut current = Value::Null;
    let last_index = groups.len() - 1;
    for (index, group) in groups.into_iter().enumerate() {
        let joined = group.parts.join("\n\n");
        let text = js_trim(&joined);
        let text = if !text.is_empty() {
            text
        } else if group.assistant {
            "..."
        } else if !group.attachments.is_empty() {
            "Tool results provided."
        } else {
            "continue"
        };
        if group.assistant {
            prior_calls = group.attachments;
            let mut assistant = json!({"content":text});
            if !prior_calls.is_empty() {
                assistant["toolUses"] = json!(prior_calls);
            }
            history.push(json!({"assistantResponseMessage":assistant}));
        } else {
            let results = tools::pair(&prior_calls, group.attachments, &mut ids)?;
            prior_calls.clear();
            let is_current = index == last_index;
            let text = if index == 0 {
                format!("{first_prefix}\n\n{text}")
            } else if is_current {
                format!("{timestamp}\n\n{text}")
            } else {
                text.to_owned()
            };
            let mut user = json!({"content":text,"modelId":model});
            if !group.images.is_empty() {
                user["images"] = json!(group.images);
            }
            if !results.is_empty() {
                user["userInputMessageContext"]["toolResults"] = json!(results);
            }
            if is_current {
                user["origin"] = json!("AI_EDITOR");
                if !specs.is_empty() {
                    user["userInputMessageContext"]["tools"] = json!(specs);
                }
                current = json!({"userInputMessage":user});
            } else {
                history.push(json!({"userInputMessage":user}));
            }
        }
    }
    let mut inference = json!({"maxTokens":32000});
    for (key, wire) in [("temperature", "temperature"), ("top_p", "topP")] {
        if let Some(value) = body.get(key) {
            value.as_f64()?;
            inference[wire] = value.clone();
        }
    }
    let mut payload = json!({
        "conversationState":{
            "chatTriggerType":"MANUAL",
            "conversationId":format!("{}{}",Uuid::new_v4(),now.timestamp_millis()),
            "currentMessage":current,
            "history":history,
        },
        "inferenceConfig":inference,
    });
    if let Some(fields) = fields {
        payload["additionalModelRequestFields"] = fields;
    }
    if let Some(scope) = scope {
        payload[session::FIELD] = json!(scope);
    }
    Some(payload)
}

#[derive(Default)]
struct InputTurn {
    assistant: bool,
    parts: Vec<String>,
    attachments: Vec<Value>,
    images: Vec<Value>,
}

fn content(value: &Value, allow_images: bool) -> Option<(String, Vec<Value>)> {
    if let Some(text) = value.as_str() {
        return Some((text.to_owned(), Vec::new()));
    }
    let mut fragments = Vec::new();
    let mut images = Vec::new();
    for part in value.as_array()? {
        match part["type"].as_str()? {
            "text" => fragments.push(part["text"].as_str()?.to_owned()),
            "image_url" if allow_images => {
                let url = part["image_url"]["url"].as_str()?;
                if let Some((mime, data)) = url
                    .strip_prefix("data:")
                    .and_then(|url| url.split_once(";base64,"))
                {
                    images.push(image(mime, data)?);
                } else if url.starts_with("https://") || url.starts_with("http://") {
                    fragments.push(format!("[Image: {url}]"));
                } else {
                    return None;
                }
            }
            "image" if allow_images && part["source"]["type"] == "base64" => {
                let source = &part["source"];
                let mime = source["media_type"]
                    .as_str()
                    .filter(|mime| !mime.is_empty())
                    .unwrap_or("image/png");
                images.push(image(mime, source["data"].as_str()?)?);
            }
            _ => return None,
        }
    }
    Some((fragments.join("\n"), images))
}

fn image(mime: &str, data: &str) -> Option<Value> {
    let format = mime
        .strip_prefix("image/")
        .filter(|name| matches!(*name, "png" | "jpeg" | "jpg" | "gif" | "webp"))?;
    if data.is_empty()
        || !data.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'+' | b'/' | b'=')
                || byte.is_ascii_whitespace()
        })
    {
        return None;
    }
    Some(json!({"format":format,"source":{"bytes":data}}))
}

fn upstream_model(model: &str) -> Option<&str> {
    let model = thinking::base_model(model)?;
    let id = model.to_ascii_lowercase();
    let known = if id.contains("sonnet") {
        if id.contains("5.5") || id.contains("5-5") {
            "claude-sonnet-5.5"
        } else if id.contains("4.6") || id.contains("4-6") {
            "claude-sonnet-4.6"
        } else if id.contains("sonnet-5") || id.contains("sonnet.5") {
            "claude-sonnet-5"
        } else if id.contains("4.5") || id.contains("4-5") {
            "claude-sonnet-4.5"
        } else if id.contains("sonnet-4") || id.contains("sonnet.4") {
            "claude-sonnet-4"
        } else {
            "claude-sonnet-4.5"
        }
    } else if id.contains("fable") {
        "claude-fable-5"
    } else if id.contains("opus") {
        if id.contains("5.5") || id.contains("5-5") {
            "claude-opus-5.5"
        } else if ["opus-5", "opus.5", "opus 5"]
            .iter()
            .any(|word| id.contains(word))
        {
            "claude-opus-5"
        } else if id.contains("4.8") || id.contains("4-8") {
            "claude-opus-4.8"
        } else if id.contains("4.7") || id.contains("4-7") {
            "claude-opus-4.7"
        } else if id.contains("4.5") || id.contains("4-5") {
            "claude-opus-4.5"
        } else {
            "claude-opus-4.6"
        }
    } else if id.contains("haiku") {
        if id.contains("5.5") || id.contains("5-5") {
            "claude-haiku-5.5"
        } else {
            "claude-haiku-4.5"
        }
    } else if id == "auto" {
        "auto"
    } else if id.contains("deepseek") {
        "deepseek-3.2"
    } else if id.contains("glm") {
        "glm-5"
    } else if id.contains("minimax") {
        if id.contains("2.5") || id.contains("2-5") {
            "minimax-m2.5"
        } else {
            "minimax-m2.1"
        }
    } else if id.contains("qwen") {
        "qwen3-coder-next"
    } else if id.contains("kimi") {
        "kimi-k3"
    } else if id.contains("gpt") {
        if id.contains("terra") {
            "gpt-5.6-terra"
        } else if id.contains("luna") {
            "gpt-5.6-luna"
        } else if ["sol", "5.6", "5-6"].iter().any(|word| id.contains(word)) {
            "gpt-5.6-sol"
        } else {
            model
        }
    } else {
        model
    };
    Some(known)
}

pub fn redact_error(account: &Value, text: &str) -> String {
    let mut private: Vec<&str> = [
        &account["apiKey"],
        &account["accessToken"],
        &account["refreshToken"],
        &account["providerSpecificData"]["profileArn"],
        &account["providerSpecificData"]["clientSecret"],
    ]
    .into_iter()
    .filter_map(nonempty)
    .collect();
    private.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
    private.into_iter().fold(text.to_owned(), |text, value| {
        text.replace(value, "[redacted]")
    })
}

pub fn adapt(response: Response, model: &str, body: &Value, stall: Duration) -> Response {
    if !response.status().is_success() {
        return response;
    }
    let origin = response.url().clone();
    let model = model.to_owned();
    let state = Turn::new(model, tools::names(body));
    let stream = futures_util::StreamExt::chain(
        stream::iter([Ok::<_, std::convert::Infallible>(Bytes::from_static(
            b": kiro-validation\n\n",
        ))]),
        stream::once(async move { Ok(Bytes::from(read_turn(response, state, stall).await)) }),
    );
    axum::http::Response::builder()
        .url(origin)
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::wrap_stream(stream))
        .expect("Kiro SSE response")
        .into()
}

async fn read_turn(mut response: Response, mut turn: Turn, stall: Duration) -> String {
    if !response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content| content.to_ascii_lowercase().contains(EVENT_STREAM))
    {
        return failure(
            "kiro_upstream_eventstream_error",
            "Kiro returned an unexpected response type",
        );
    }
    let mut body = Vec::new();
    loop {
        match tokio::time::timeout(stall, response.chunk()).await {
            Ok(Ok(Some(chunk))) if chunk.len() <= MAX_REPLY.saturating_sub(body.len()) => {
                body.extend_from_slice(&chunk);
            }
            Ok(Ok(Some(_))) => {
                return failure(
                    "kiro_integrity_buffer_exceeded",
                    "Kiro response exceeded the integrity buffer",
                );
            }
            Ok(Ok(None)) => break,
            _ => return failure("kiro_missing_terminal", "Kiro EventStream read failed"),
        }
    }

    let mut pending = body.as_slice();
    while !pending.is_empty() {
        let Ok(size) = frame_length(pending) else {
            return failure(
                "kiro_missing_terminal",
                "Kiro EventStream prelude was invalid or truncated",
            );
        };
        let Some((raw, rest)) = pending.split_at_checked(size) else {
            return failure(
                "kiro_missing_terminal",
                "Kiro EventStream ended with a truncated frame",
            );
        };
        pending = rest;
        let Ok(frame) = decode_frame(raw) else {
            return failure(
                "kiro_missing_terminal",
                "Kiro EventStream frame was corrupt",
            );
        };
        if let Err((code, message)) = turn.push(frame) {
            return failure(code, message);
        }
    }
    turn.finish()
}

struct Turn {
    model: String,
    id: String,
    created: i64,
    deltas: Vec<Value>,
    usage: Map<String, Value>,
    text: String,
    reasoning: bool,
    thinking: Thinking,
    stop: Option<Stop>,
    context: Option<f64>,
    metering: bool,
    output_units: usize,
    allowed_tools: HashSet<String>,
    tool_calls: tools::StreamCalls,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stop {
    Complete,
    Tool,
    Length,
    Malformed,
    ContextExceeded,
    Unknown,
    Incomplete,
    Refusal,
}

impl Turn {
    fn new(model: String, allowed_tools: HashSet<String>) -> Self {
        let now = Utc::now();
        Self {
            model,
            id: format!("chatcmpl-{}", now.timestamp_millis()),
            created: now.timestamp(),
            deltas: Vec::new(),
            usage: Map::new(),
            text: String::new(),
            reasoning: false,
            thinking: Thinking::default(),
            stop: None,
            context: None,
            metering: false,
            output_units: 0,
            allowed_tools,
            tool_calls: tools::StreamCalls::default(),
        }
    }

    fn push(&mut self, frame: Frame) -> Result<(), (&'static str, &'static str)> {
        let event = frame.headers.get(":event-type").map(String::as_str);
        let message = frame.headers.get(":message-type").map(String::as_str);
        if matches!(message, Some("error" | "exception")) {
            return Err((
                "kiro_upstream_eventstream_error",
                "Kiro upstream rejected the request",
            ));
        }
        let value = &frame.payload;
        match event {
            Some("assistantResponseEvent") => {
                if let Some(text) = value["content"].as_str() {
                    let visible = self.thinking.strip(text);
                    if !visible.is_empty() || !self.reasoning {
                        self.append_text(&visible);
                    }
                }
            }
            Some("reasoningContentEvent") => {
                let data = inner(value, "reasoningContentEvent");
                let text = data
                    .as_str()
                    .or_else(|| data["text"].as_str())
                    .or_else(|| data["content"].as_str());
                if let Some(text) = text.filter(|text| !text.is_empty()) {
                    self.reasoning = true;
                    self.output_units += text.encode_utf16().count();
                    self.deltas.push(json!({"reasoning_content":text}));
                }
            }
            Some("codeEvent") => {
                if let Some(text) = value["content"].as_str() {
                    self.append_text(text);
                }
            }
            Some("toolUseEvent") => {
                if self.allowed_tools.is_empty() {
                    return Err((
                        "invalid_kiro_tool_call",
                        "Kiro returned a tool call for a text-only request",
                    ));
                }
                self.tool_calls
                    .push(&self.allowed_tools, value, self.created);
            }
            Some("messageStopEvent") => self.stop(value, true),
            Some("metadataEvent" | "MetadataEvent") => {
                let data = value
                    .get("metadataEvent")
                    .or_else(|| value.get("metadata"))
                    .unwrap_or(value);
                self.stop(data, false);
            }
            Some("contextUsageEvent") => {
                self.context = value.get("contextUsagePercentage").and_then(number)
            }
            Some("meteringEvent") => {
                self.metering = true;
                let data = inner(value, "meteringEvent");
                if let Some(credit) = data.get("usage").and_then(number) {
                    self.usage
                        .insert("kiro_credits".into(), json_number(credit));
                    self.usage.insert(
                        "kiro_credit_unit".into(),
                        json!(data["unit"].as_str().unwrap_or("credit")),
                    );
                }
            }
            Some("metricsEvent") => self.metrics(inner(value, "metricsEvent")),
            _ => (),
        }
        Ok(())
    }

    fn append_text(&mut self, text: &str) {
        self.output_units += text.encode_utf16().count();
        self.text.push_str(text);
        self.deltas.push(json!({"content":text}));
    }

    fn stop(&mut self, data: &Value, required: bool) {
        let raw = data["stopReason"].as_str().or(data["stop_reason"].as_str());
        if let Some(reason) = raw
            .filter(|text| !js_trim(text).is_empty())
            .map(stop)
            .or(required.then_some(Stop::Complete))
        {
            self.stop = self.stop.max(Some(reason));
        }
    }

    fn metrics(&mut self, data: &Value) {
        let input = data.get("inputTokens").and_then(number).unwrap_or(0.0);
        let output = data.get("outputTokens").and_then(number).unwrap_or(0.0);
        if input != 0.0 || output != 0.0 {
            self.token_usage(input, output);
            for (key, camel) in [
                ("cache_read_input_tokens", "cacheReadInputTokens"),
                ("cache_creation_input_tokens", "cacheCreationInputTokens"),
            ] {
                if let Some(count) = data
                    .get(camel)
                    .and_then(number)
                    .filter(|count| *count != 0.0)
                    .or_else(|| data.get(key).and_then(number).filter(|count| *count != 0.0))
                {
                    self.usage.insert(key.into(), json_number(count));
                }
            }
        }
    }

    fn token_usage(&mut self, input: f64, output: f64) {
        for (key, value) in [
            ("prompt_tokens", input),
            ("completion_tokens", output),
            ("total_tokens", input + output),
        ] {
            self.usage.insert(key.into(), json_number(value));
        }
    }

    fn finish(mut self) -> String {
        if matches!(self.stop, Some(Stop::Refusal)) {
            return failure("kiro_terminal_refusal", "Kiro refused the request");
        }
        if let Some(reason) = self.stop {
            match reason {
                Stop::Malformed => {
                    return failure(
                        "kiro_retryable_protocol_failure",
                        "Kiro reported malformed model output",
                    );
                }
                Stop::Incomplete => {
                    return failure(
                        "kiro_terminal_incomplete",
                        "Kiro ended before completing a response",
                    );
                }
                Stop::Unknown => {
                    return failure(
                        "kiro_unknown_stop_reason",
                        "Kiro reported an unsupported stop reason",
                    );
                }
                _ => (),
            }
        }
        if !self.thinking.hidden && !self.thinking.pending.is_empty() {
            let pending = std::mem::take(&mut self.thinking.pending);
            self.append_text(&pending);
        }
        let (calls, units, malformed) = std::mem::take(&mut self.tool_calls).finish();
        let has_tools = !calls.is_empty();
        if !has_tools && (malformed || self.stop == Some(Stop::Tool)) && self.output_units == 0 {
            return failure(
                "invalid_kiro_tool_call",
                "Kiro ended before returning a complete tool call",
            );
        }
        self.output_units += units;
        self.deltas.extend(calls);
        if self.thinking.hidden
            || self.stop == Some(Stop::ContextExceeded) && self.output_units == 0
            || self.stop.is_none() && self.output_units == 0
        {
            return failure(
                "kiro_missing_terminal",
                "Kiro EventStream ended without complete model output",
            );
        }
        if !has_tools && matches!(js_trim(&self.text), "..." | "…") {
            return failure(
                "kiro_missing_terminal",
                "Kiro did not return a complete answer",
            );
        }
        if self.metering
            && let Some(context) = self.context
            && !self.usage.get("total_tokens").is_some_and(truthy)
        {
            let caps = provider_capabilities_rs::port::model_capabilities(
                Some("kiro"),
                upstream_model(&self.model).unwrap_or(&self.model),
                None,
            );
            let window = caps["contextWindow"].as_f64().unwrap_or(200_000.0);
            let completion = if self.output_units == 0 {
                0
            } else {
                (self.output_units / 4).max(1)
            };
            self.token_usage((context * window / 100.0).floor(), completion as f64);
        }
        let mut sse = String::new();
        for (index, delta) in self.deltas.iter().enumerate() {
            let mut delta = delta.clone();
            if index == 0 {
                delta["role"] = json!("assistant");
            }
            frame_text(&mut sse, self.chunk(delta, Value::Null));
            if sse.len() > MAX_REPLY {
                return failure(
                    "kiro_integrity_buffer_exceeded",
                    "Kiro response exceeded the integrity buffer",
                );
            }
        }
        let reason = if has_tools {
            "tool_calls"
        } else if matches!(self.stop, Some(Stop::ContextExceeded | Stop::Length)) {
            "length"
        } else {
            "stop"
        };
        let mut final_chunk = self.chunk(json!({}), json!(reason));
        if !self.usage.is_empty() {
            final_chunk["usage"] = Value::Object(self.usage);
        }
        frame_text(&mut sse, final_chunk);
        sse.push_str(DONE);
        sse
    }

    fn chunk(&self, delta: Value, finish_reason: Value) -> Value {
        json!({
            "id":self.id,"object":"chat.completion.chunk","created":self.created,
            "model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":finish_reason}],
        })
    }
}

#[derive(Default)]
struct Thinking {
    hidden: bool,
    pending: String,
}

impl Thinking {
    fn strip(&mut self, text: &str) -> String {
        self.pending.push_str(text);
        let mut visible = String::new();
        let mut rest = self.pending.as_str();
        while !rest.is_empty() {
            let marker = if self.hidden {
                "</thinking>"
            } else {
                "<thinking>"
            };
            if let Some(index) = rest.find(marker) {
                if !self.hidden {
                    visible.push_str(&rest[..index]);
                }
                rest = &rest[index + marker.len()..];
                if self.hidden {
                    rest = rest.strip_prefix('\n').unwrap_or(rest);
                }
                self.hidden = !self.hidden;
            } else {
                let preserve = (1..marker.len())
                    .rev()
                    .find(|length| rest.ends_with(&marker[..*length]))
                    .unwrap_or(0);
                let (before, pending) = rest.split_at(rest.len() - preserve);
                if !self.hidden {
                    visible.push_str(before);
                }
                self.pending = pending.to_owned();
                return visible;
            }
        }
        self.pending.clear();
        visible
    }
}

fn stop(reason: &str) -> Stop {
    let reason = reason
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| byte.to_ascii_lowercase() as char)
        .collect::<String>();
    match reason.as_str() {
        "endturn" | "stop" | "stopsequence" => Stop::Complete,
        "tooluse" | "toolcalls" => Stop::Tool,
        "maxtokens" | "maxoutputtokens" | "length" => Stop::Length,
        "malformedmodeloutput" | "invalidmodeloutput" => Stop::Malformed,
        "modelcontextwindowexceeded" => Stop::ContextExceeded,
        "cancelled" | "pauseturn" => Stop::Incomplete,
        "refusal" => Stop::Refusal,
        _ if ["contentfilter", "guardrail", "safety", "policy", "blocked"]
            .iter()
            .any(|hint| reason.contains(hint)) =>
        {
            Stop::Refusal
        }
        _ => Stop::Unknown,
    }
}

fn inner<'a>(data: &'a Value, key: &str) -> &'a Value {
    data.get(key).filter(|value| truthy(value)).unwrap_or(data)
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Null | Value::Bool(false) => Some(0.0),
        Value::Bool(true) => Some(1.0),
        Value::String(text) if js_trim(text).is_empty() => Some(0.0),
        Value::String(text) => js_trim(text)
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        _ => value.as_f64().filter(|value| value.is_finite()),
    }
}

fn json_number(value: f64) -> Value {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value < i64::MAX as f64 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn frame_text(sse: &mut String, data: Value) {
    sse.push_str("data: ");
    sse.push_str(&data.to_string());
    sse.push_str("\n\n");
}

fn failure(code: &str, message: &str) -> String {
    let mut wire = String::new();
    frame_text(
        &mut wire,
        json!({"error":{"type":"upstream_error","code":code,"message":message,"status":502}}),
    );
    wire.push_str(DONE);
    wire
}
