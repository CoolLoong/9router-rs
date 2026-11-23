use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use reqwest::{Client, RequestBuilder, Response, StatusCode, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::chat_format::ChatFormat;
use crate::provider_connections::truthy;
use crate::provider_http::{self, ProtectedEgress};
use crate::provider_index::js_trim;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const ORIGIN: &str = "https://cloud.zed.dev";
const COMPLETIONS: &str = "/completions";
const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);
const MODEL_TTL: Duration = Duration::from_secs(60 * 60);
const METADATA_LIMIT: usize = 1024 * 1024;
const ACCOUNT_LIMIT: usize = 128;
const FIRST_REPLY_TIMEOUT: Duration = Duration::from_secs(20);
#[path = "zed_stream.rs"]
mod wire;
static CLIENT: LazyLock<Result<Client, reqwest::Error>> =
    LazyLock::new(|| client_builder().build());

fn client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .connect_timeout(FIRST_REPLY_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

type AccountCache = VecDeque<([u8; 32], Arc<AccountState>)>;

static ACCOUNTS: LazyLock<Mutex<AccountCache>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

#[derive(Default)]
struct AccountState {
    authorization: AsyncMutex<Option<(Instant, String)>>,
    models: AsyncMutex<Option<(Instant, Catalog)>>,
}

#[derive(Clone)]
struct Catalog {
    models: Vec<Value>,
    routes: HashMap<String, HostedModel>,
}

#[derive(Clone)]
struct HostedModel {
    provider: String,
    disabled: bool,
    tools: bool,
    images: bool,
}

struct Credentials<'a> {
    user: String,
    token: &'a str,
    system: Option<&'a str>,
    organization: Option<String>,
}

impl<'a> Credentials<'a> {
    fn from_account(account: &'a Value) -> Option<Self> {
        if account["authType"] != "oauth" {
            return None;
        }
        Self::catalog_account(account)
    }

    fn catalog_account(account: &'a Value) -> Option<Self> {
        let metadata = &account["providerSpecificData"];
        if account["provider"] != "zed" {
            return None;
        }
        let user = identifier(&metadata["userId"]).or_else(|| identifier(&account["userId"]))?;
        let token = text(&account["accessToken"])?;
        let system = text(&metadata["systemId"]).or_else(|| text(&account["systemId"]));
        if !wire_token(&user) || !wire_token(token) || system.is_some_and(|id| !wire_token(id)) {
            return None;
        }
        let organization = identifier(&metadata["organizationId"])
            .or_else(|| identifier(&metadata["defaultOrganizationId"]));
        Some(Self {
            user,
            token,
            system,
            organization,
        })
    }

    fn headers(&self, request: RequestBuilder) -> Result<RequestBuilder, String> {
        let mut value = header::HeaderValue::from_str(&format!("{} {}", self.user, self.token))
            .map_err(|_| "Missing Zed account credentials")?;
        value.set_sensitive(true);
        let mut request = request
            .header(header::AUTHORIZATION, value)
            .header(header::ACCEPT, "application/json");
        if let Some(system) = self.system {
            request = request.header("x-zed-system-id", system);
        }
        Ok(request)
    }
}

pub fn supports(account: &Value) -> bool {
    Credentials::from_account(account).is_some()
}

pub(super) fn completion_url() -> String {
    format!("{ORIGIN}{COMPLETIONS}")
}

pub(super) fn completion_headers(request: RequestBuilder) -> RequestBuilder {
    request
        .header(
            header::ACCEPT,
            "application/x-ndjson, text/event-stream, */*",
        )
        .header(header::USER_AGENT, "9router/zed")
        .header("x-zed-client-supports-status-messages", "true")
        .header(
            "x-zed-client-supports-stream-ended-request-completion-status",
            "true",
        )
}

pub(super) async fn egress(
    storage: &Storage,
    settings: &Value,
    account: &Value,
) -> Result<Option<ProtectedEgress>, &'static str> {
    provider_http::protected_egress(storage, settings, account, ORIGIN)
        .await
        .map_err(|_| "Zed proxy is unavailable")
}

pub(super) async fn protects_chat(storage: &Storage, settings: &Value) -> bool {
    let Ok(accounts) = crate::chat_accounts::active_connections(storage, "zed").await else {
        return true;
    };
    accounts
        .iter()
        .any(proxy_transport::account_requires_tunnel)
        || !matches!(egress(storage, settings, &Value::Null).await, Ok(None))
}

struct Connection<'a> {
    credentials: Credentials<'a>,
    cache: Arc<AccountState>,
    tunnel: Option<Client>,
    relay: Option<Route>,
}

impl<'a> Connection<'a> {
    fn new(
        storage: &Storage,
        account: &'a Value,
        route: Option<ProtectedEgress>,
    ) -> Result<Self, String> {
        let credentials = Credentials::from_account(account)
            .filter(|_| route.is_some() || !proxy_transport::account_requires_tunnel(account))
            .ok_or("Missing Zed account credentials")?;
        Self::from_credentials(storage, account, credentials, route)
    }

    fn from_credentials(
        storage: &Storage,
        account: &Value,
        credentials: Credentials<'a>,
        route: Option<ProtectedEgress>,
    ) -> Result<Self, String> {
        let (proxy, relay) = match route {
            Some(ProtectedEgress::Tunnel(proxy)) => (Some(proxy), None),
            Some(ProtectedEgress::Relay(relay)) => (None, Some(relay)),
            _ => (None, None),
        };
        let mut hash = Sha256::new();
        for field in [
            &*storage.root().to_string_lossy(),
            text(&account["id"]).unwrap_or_default(),
            &credentials.user,
            credentials.token,
            credentials.system.unwrap_or_default(),
            credentials.organization.as_deref().unwrap_or_default(),
            proxy.as_ref().map(reqwest::Url::as_str).unwrap_or_default(),
        ] {
            hash.update(field.len().to_le_bytes());
            hash.update(field.as_bytes());
        }
        if let Some(identity) = relay.as_ref().and_then(Route::relay_identity) {
            hash.update([1]);
            hash.update(identity);
        }
        let cache = account_cache(hash.finalize().into());
        let tunnel = proxy
            .map(|proxy| {
                proxy_transport::vendor_metadata_client(proxy, client_builder())
                    .ok_or("Zed proxy is unavailable")
            })
            .transpose()?;
        Ok(Self {
            credentials,
            cache,
            tunnel,
            relay,
        })
    }

    fn client(&self) -> Result<&Client, String> {
        match &self.tunnel {
            Some(client) => Ok(client),
            None => CLIENT
                .as_ref()
                .map_err(|_| "Could not initialize the Zed client".to_owned()),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        if self.tunnel.is_none()
            && self.relay.is_none()
            && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        {
            format!("http://127.0.0.1:52989/zed-fixture{path}")
        } else {
            format!("{ORIGIN}{path}")
        }
    }

    fn routed_request(&self, request: RequestBuilder) -> Result<RequestBuilder, String> {
        let Some(relay) = &self.relay else {
            return Ok(request);
        };
        let (client, request) = request.build_split();
        let request = request.map_err(|_| "Invalid Zed request")?;
        let relay = relay
            .relay_target(request.url())
            .ok_or("Zed relay origin changed")?;
        provider_http::relay_request(client, request, &relay)
            .ok_or_else(|| "Zed relay is unavailable".into())
    }

    async fn bearer(&self, rejected: Option<&str>) -> Result<String, String> {
        let mut state = self.cache.authorization.lock().await;
        let now = Instant::now();
        if let Some((expires, token)) = state.as_ref()
            && *expires > now
            && rejected != Some(token.as_str())
        {
            return Ok(token.clone());
        }
        let previous = state.as_ref().map(|(_, token)| token.as_str()).or(rejected);
        let client = self.client()?;
        let organization = if let Some(saved) = self.credentials.organization.as_ref() {
            saved.clone()
        } else {
            let url = self.endpoint("/client/users/me");
            let request = self
                .credentials
                .headers(client.get(url).timeout(FIRST_REPLY_TIMEOUT))?;
            let metadata = self.routed_request(request)?.send().await;
            let user = self
                .read_metadata(metadata, "Could not load the Zed account", previous)
                .await?;
            organization(&user).ok_or("No Zed organization selected")?
        };
        let url = self.endpoint("/client/llm_tokens");
        let request = self
            .credentials
            .headers(client.post(url).timeout(FIRST_REPLY_TIMEOUT))?
            .json(&json!({"organization_id":organization}));
        let token = self.routed_request(request)?.send().await;
        let value = self
            .read_metadata(token, "Could not obtain a Zed LLM token", previous)
            .await?;
        let token = text(&value["token"])
            .or_else(|| text(&value["token"][0]))
            .or_else(|| text(&value["token"]["value"]))
            .filter(|token| wire_token(token))
            .ok_or("Zed did not return an LLM token")?
            .to_owned();
        *state = Some((Instant::now() + TOKEN_TTL, token.clone()));
        Ok(token)
    }

    async fn read_metadata(
        &self,
        response: reqwest::Result<Response>,
        description: &str,
        previous: Option<&str>,
    ) -> Result<Value, String> {
        let response = response.map_err(|_| description.to_owned())?;
        let status = response.status();
        let raw = read_body(response, METADATA_LIMIT)
            .await
            .map_err(|_| format!("{description}: invalid response"))?;
        let value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        if !status.is_success() {
            let reason = text(&value["message"])
                .or_else(|| text(&value["error"]["message"]))
                .or_else(|| text(&value["error"]))
                .unwrap_or(description);
            return Err(safe_text(
                reason,
                &[
                    self.credentials.token,
                    self.credentials.system.unwrap_or_default(),
                    previous.unwrap_or_default(),
                    self.relay
                        .as_ref()
                        .and_then(Route::relay_secret)
                        .unwrap_or_default(),
                ],
            ));
        }
        if value.is_null() {
            return Err(format!("{description}: invalid response"));
        }
        Ok(value)
    }

    async fn call(
        &self,
        path: &str,
        request: impl Fn(&Client, &str) -> RequestBuilder,
        replay_post: bool,
    ) -> Result<(Response, String, Option<String>), String> {
        let endpoint = self.endpoint(path);
        let client = self.client()?;
        let token = self.bearer(None).await?;
        let initial =
            send(self.routed_request(request(client, &endpoint).bearer_auth(&token))?).await?;
        let expired = initial.headers().contains_key("x-zed-expired-token")
            || initial.headers().contains_key("x-zed-outdated-token");
        if initial.status() == StatusCode::UNAUTHORIZED
            || expired && (!replay_post || initial.status() == StatusCode::FORBIDDEN)
        {
            drop(initial);
            let renewed = self.bearer(Some(&token)).await?;
            let retried =
                send(self.routed_request(request(client, &endpoint).bearer_auth(&renewed))?)
                    .await?;
            return Ok((retried, renewed, Some(token)));
        }
        Ok((initial, token, None))
    }

    async fn catalog(&self, force: bool) -> Result<Catalog, String> {
        let mut cache = self.cache.models.lock().await;
        if !force
            && let Some((expires, result)) = &*cache
            && *expires > Instant::now()
        {
            return Ok(result.clone());
        }
        let (response, token, rejected) = self
            .call(
                "/models",
                |client, url| {
                    client
                        .get(url)
                        .timeout(FIRST_REPLY_TIMEOUT)
                        .header(header::ACCEPT, "application/json")
                        .header("x-zed-client-supports-x-ai", "true")
                },
                false,
            )
            .await?;
        let status = response.status();
        let private = [
            self.credentials.token,
            &token,
            rejected.as_deref().unwrap_or_default(),
            self.relay
                .as_ref()
                .and_then(Route::relay_secret)
                .unwrap_or_default(),
        ];
        let raw = read_body(response, METADATA_LIMIT)
            .await
            .map_err(|_| "Invalid Zed models response")?;
        if !status.is_success() {
            let wire = String::from_utf8_lossy(&raw);
            return Err(format!(
                "Zed models failed: {} {}",
                status.as_u16(),
                safe_text(&wire, &private)
            ));
        }
        let data: Value =
            serde_json::from_slice(&raw).map_err(|_| "Invalid Zed models response")?;
        let catalog = map_models(&data, &private);
        *cache = Some((Instant::now() + MODEL_TTL, catalog.clone()));
        Ok(catalog)
    }

    async fn model(&self, model: &str) -> Result<Option<HostedModel>, String> {
        let catalog = self.catalog(false).await?;
        if let Some(route) = catalog.routes.get(model) {
            return Ok(Some(route.clone()));
        }
        Ok(self.catalog(true).await?.routes.remove(model))
    }
}

pub async fn models(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    force: bool,
) -> Result<Vec<Value>, String> {
    let credentials =
        Credentials::catalog_account(account).ok_or("Missing Zed account credentials")?;
    let route = egress(storage, settings, account).await?;
    let connection = Connection::from_credentials(storage, account, credentials, route)?;
    Ok(connection.catalog(force).await?.models)
}

#[derive(Clone, Copy)]
enum Provider {
    Claude,
    OpenAi,
    Google,
    Xai,
}

impl Provider {
    fn declared(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "anthropic" => Some(Self::Claude),
            "openai" | "open_ai" => Some(Self::OpenAi),
            "google" | "gemini" => Some(Self::Google),
            "xai" | "x_ai" | "x-ai" => Some(Self::Xai),
            _ => None,
        }
    }

    fn resolve(raw: Option<&str>, model: &str) -> Self {
        raw.and_then(Self::declared).unwrap_or_else(|| {
            let model = model.to_ascii_lowercase();
            if model.contains("claude") {
                Self::Claude
            } else if model.contains("gemini") {
                Self::Google
            } else if model.contains("grok") || model.contains("xai") {
                Self::Xai
            } else {
                Self::OpenAi
            }
        })
    }

    fn wire(self) -> &'static str {
        match self {
            Self::Claude => "anthropic",
            Self::OpenAi => "open_ai",
            Self::Google => "google",
            Self::Xai => "x_ai",
        }
    }

    fn translate(self, body: &Value, model: &str) -> Option<Value> {
        let mut body = body.clone();
        body["stream"] = json!(true);
        match self {
            Self::Claude => crate::anthropic_request::prepare(ChatFormat::Chat, body, model, "")
                .map(|prepared| prepared.body),
            Self::Google => {
                let mut prepared = crate::gemini_request::prepare(
                    ChatFormat::Chat,
                    body,
                    model,
                    "",
                    |_| None,
                    crate::gemini_request::DEFAULT_SIGNATURE,
                )?
                .body;
                prepared.as_object_mut()?.shift_remove("safetySettings");
                Some(prepared)
            }
            Self::OpenAi => crate::responses_request::prepare(ChatFormat::Chat, body, model, "")
                .map(|prepared| prepared.body),
            Self::Xai => {
                body["model"] = json!(model);
                Some(body)
            }
        }
    }
}

#[derive(Default)]
struct Features {
    tools: bool,
    images: bool,
    strict_tools: bool,
    explicit_choice: bool,
}

impl Features {
    fn extended(&self) -> bool {
        self.tools || self.images
    }

    fn supported(&self, route: Option<&HostedModel>, format: ChatFormat) -> bool {
        if !self.extended() {
            match format {
                ChatFormat::Chat => return true,
                ChatFormat::Responses => return route.is_none_or(|model| !model.disabled),
                ChatFormat::Messages => {}
            }
        }
        route.is_some_and(|model| {
            let compatible = match (format, Provider::declared(&model.provider)) {
                (_, Some(Provider::Claude)) => !self.strict_tools,
                (ChatFormat::Chat | ChatFormat::Responses, Some(Provider::OpenAi)) => {
                    !self.explicit_choice
                }
                _ => false,
            };
            compatible
                && !model.disabled
                && (!self.tools || model.tools)
                && (!self.images || model.images)
        })
    }
}

fn fields(value: &Value, permitted: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|fields| fields.keys().all(|name| permitted.contains(&name.as_str())))
}

fn function_tool(tool: &Value) -> bool {
    let function = &tool["function"];
    fields(tool, &["type", "function"])
        && tool["type"] == "function"
        && fields(function, &["name", "description", "parameters", "strict"])
        && text(&function["name"]).is_some()
        && function.get("description").is_none_or(Value::is_string)
        && function.get("parameters").is_none_or(Value::is_object)
        && function.get("strict").is_none_or(Value::is_boolean)
}

fn tool_choice(choice: &Value) -> bool {
    matches!(choice.as_str(), Some("auto" | "none" | "required"))
        || fields(choice, &["type", "function"])
            && choice["type"] == "function"
            && fields(&choice["function"], &["name"])
            && text(&choice["function"]["name"]).is_some()
}

fn tool_call(call: &Value) -> bool {
    let function = &call["function"];
    fields(call, &["id", "type", "function"])
        && call["type"] == "function"
        && text(&call["id"]).is_some()
        && fields(function, &["name", "arguments"])
        && text(&function["name"]).is_some()
        && arguments_object(&function["arguments"])
}

fn arguments_object(value: &Value) -> bool {
    value
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .is_some_and(|value| value.is_object())
}

fn inline_image(part: &Value) -> bool {
    part["image_url"]["url"]
        .as_str()
        .and_then(|url| url.strip_prefix("data:"))
        .and_then(|url| url.split_once(";base64,"))
        .is_some_and(|(mime, data)| supported_image(mime, data))
}

fn supported_image(mime: &str, data: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) && !data.is_empty()
}

fn messages_text(part: &Value) -> bool {
    fields(part, &["type", "text", "cache_control"])
        && part["type"] == "text"
        && part["text"].is_string()
}

fn messages_text_content(content: &Value) -> bool {
    content.is_string()
        || content
            .as_array()
            .is_some_and(|parts| parts.iter().all(messages_text))
}

fn messages_tool(tool: &Value) -> bool {
    fields(
        tool,
        &[
            "type",
            "name",
            "description",
            "input_schema",
            "cache_control",
        ],
    ) && tool.get("type").is_none_or(|kind| kind == "custom")
        && text(&tool["name"]).is_some()
        && tool.get("description").is_none_or(Value::is_string)
        && tool["input_schema"].is_object()
}

fn messages_tool_choice(choice: &Value) -> bool {
    match choice["type"].as_str() {
        Some("auto" | "any" | "none") => fields(choice, &["type"]),
        Some("tool") => fields(choice, &["type", "name"]) && text(&choice["name"]).is_some(),
        _ => false,
    }
}

fn messages_block(part: &Value, role: &str) -> bool {
    match (role, part["type"].as_str()) {
        (_, Some("text")) => messages_text(part),
        ("user", Some("image")) => {
            let image = &part["source"];
            fields(part, &["type", "source", "cache_control"])
                && fields(image, &["type", "media_type", "data"])
                && image["type"] == "base64"
                && image["media_type"]
                    .as_str()
                    .zip(image["data"].as_str())
                    .is_some_and(|(mime, data)| supported_image(mime, data))
        }
        ("user", Some("tool_result")) => {
            fields(
                part,
                &[
                    "type",
                    "content",
                    "tool_use_id",
                    "is_error",
                    "cache_control",
                ],
            ) && text(&part["tool_use_id"]).is_some()
                && part.get("is_error").is_none_or(Value::is_boolean)
                && part.get("content").is_none_or(messages_text_content)
        }
        ("assistant", Some("tool_use")) => {
            fields(part, &["type", "id", "name", "input", "cache_control"])
                && text(&part["id"]).is_some()
                && text(&part["name"]).is_some()
                && part["input"].is_object()
        }
        _ => false,
    }
}

pub fn messages_to_chat(body: Value, accept: &str) -> Option<Value> {
    if !fields(
        &body,
        &[
            "model",
            "messages",
            "system",
            "stream",
            "max_tokens",
            "temperature",
            "tools",
            "tool_choice",
            "cache_control",
            "metadata",
            "prompt_cache_key",
            "session_id",
            "conversation_id",
        ],
    ) || !body["max_tokens"].as_u64().is_some_and(|limit| limit > 0)
        || !body.get("stream").is_none_or(Value::is_boolean)
        || !body.get("system").is_none_or(messages_text_content)
        || !body.get("tools").is_none_or(|tools| {
            tools
                .as_array()
                .is_some_and(|tools| tools.iter().all(messages_tool))
        })
        || !body.get("tool_choice").is_none_or(messages_tool_choice)
    {
        return None;
    }
    let messages = body["messages"].as_array()?;
    if messages.is_empty()
        || !messages.iter().all(|message| {
            let role = message["role"].as_str().unwrap_or_default();
            matches!(role, "user" | "assistant")
                && fields(message, &["role", "content"])
                && (message["content"].is_string()
                    || message["content"]
                        .as_array()
                        .is_some_and(|parts| parts.iter().all(|part| messages_block(part, role))))
        })
    {
        return None;
    }
    let failed: HashSet<String> = messages
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "tool_result" && part["is_error"] == true)
        .filter_map(|part| part["tool_use_id"].as_str().map(str::to_owned))
        .collect();
    let choice = body.get("tool_choice").cloned();
    let mut output = crate::chat_protocol_input::translate(ChatFormat::Messages, body, accept)?;
    if let Some(choice) = choice {
        output["tool_choice"] = choice;
    }
    if !failed.is_empty() {
        for message in output["messages"].as_array_mut()? {
            if message["role"] == "tool"
                && message["tool_call_id"]
                    .as_str()
                    .is_some_and(|id| failed.contains(id))
            {
                message["is_error"] = json!(true);
            }
        }
    }
    Some(output)
}

fn responses_tool(tool: &Value) -> bool {
    fields(
        tool,
        &["type", "name", "description", "parameters", "strict"],
    ) && tool["type"] == "function"
        && text(&tool["name"]).is_some()
        && tool.get("description").is_none_or(Value::is_string)
        && tool.get("parameters").is_none_or(Value::is_object)
        && tool.get("strict").is_none_or(Value::is_boolean)
}

fn responses_item(item: &Value) -> bool {
    if !item.get("id").is_none_or(Value::is_string)
        || !item
            .get("status")
            .is_none_or(|status| status == "completed")
    {
        return false;
    }
    match item["type"].as_str() {
        Some("function_call") => {
            fields(
                item,
                &["type", "id", "status", "call_id", "name", "arguments"],
            ) && text(&item["call_id"]).is_some()
                && text(&item["name"]).is_some()
                && arguments_object(&item["arguments"])
        }
        Some("function_call_output") => {
            fields(item, &["type", "id", "status", "call_id", "output"])
                && text(&item["call_id"]).is_some()
                && item["output"].is_string()
        }
        Some("message") | None => {
            let role = item["role"].as_str().unwrap_or_default();
            let content = &item["content"];
            fields(item, &["type", "id", "status", "role", "content"])
                && matches!(role, "system" | "developer" | "user" | "assistant")
                && (content.is_string()
                    || content.as_array().is_some_and(|parts| {
                        parts
                            .iter()
                            .all(|part| match (role, part["type"].as_str()) {
                                ("assistant", Some("output_text")) => {
                                    fields(part, &["type", "text", "annotations", "logprobs"])
                                        && part["text"].is_string()
                                        && ["annotations", "logprobs"].iter().all(|field| {
                                            part.get(*field).is_none_or(|value| {
                                                value.as_array().is_some_and(Vec::is_empty)
                                            })
                                        })
                                }
                                ("user" | "system" | "developer", Some("input_text")) => {
                                    fields(part, &["type", "text"]) && part["text"].is_string()
                                }
                                ("user", Some("input_image")) => {
                                    fields(part, &["type", "image_url", "detail"])
                                        && part["image_url"].is_string()
                                        && part.get("detail").is_none_or(Value::is_string)
                                }
                                _ => false,
                            })
                    }))
        }
        _ => false,
    }
}

pub fn responses_to_chat(mut body: Value, accept: &str) -> Option<Value> {
    body.as_object_mut()?.shift_remove("_compact");
    if !fields(
        &body,
        &[
            "model",
            "input",
            "instructions",
            "stream",
            "store",
            "max_output_tokens",
            "temperature",
            "top_p",
            "tools",
            "tool_choice",
            "metadata",
            "prompt_cache_key",
            "session_id",
            "conversation_id",
        ],
    ) || !body.get("instructions").is_none_or(Value::is_string)
        || !body.get("stream").is_none_or(Value::is_boolean)
        || !body.get("store").is_none_or(|store| store == false)
        || !body
            .get("tool_choice")
            .is_none_or(|choice| choice == "auto")
        || !body.get("tools").is_none_or(|tools| {
            tools
                .as_array()
                .is_some_and(|tools| tools.iter().all(responses_tool))
        })
    {
        return None;
    }
    let input = &body["input"];
    if !input.is_string()
        && !input
            .as_array()
            .is_some_and(|items| !items.is_empty() && items.iter().all(responses_item))
    {
        return None;
    }
    crate::chat_protocol_input::translate(ChatFormat::Responses, body, accept)
}

fn inspect(body: &Value, format: ChatFormat) -> Option<Features> {
    if !fields(
        body,
        &[
            "model",
            "messages",
            "stream",
            "stream_options",
            "max_tokens",
            "max_completion_tokens",
            "max_output_tokens",
            "temperature",
            "top_p",
            "thread_id",
            "prompt_id",
            "tools",
            "tool_choice",
        ],
    ) {
        return None;
    }
    let mut features = Features::default();
    if let Some(tools) = body.get("tools") {
        let tools = tools.as_array()?;
        if !tools.iter().all(function_tool) {
            return None;
        }
        features.tools = !tools.is_empty();
        features.strict_tools = tools
            .iter()
            .any(|tool| tool["function"].get("strict").is_some());
    }
    if let Some(choice) = body.get("tool_choice") {
        let valid = if format == ChatFormat::Messages {
            messages_tool_choice(choice)
        } else {
            tool_choice(choice)
        };
        if !valid {
            return None;
        }
        features.tools |= choice != "auto";
        features.explicit_choice = format != ChatFormat::Messages && choice != "auto";
    }
    let messages = body["messages"].as_array()?;
    if messages.is_empty() {
        return None;
    }
    for message in messages {
        let role = message["role"].as_str()?;
        let content = &message["content"];
        match role {
            "tool"
                if (fields(message, &["role", "content", "tool_call_id"])
                    || format == ChatFormat::Messages
                        && message["is_error"] == true
                        && fields(message, &["role", "content", "tool_call_id", "is_error"]))
                    && text(&message["tool_call_id"]).is_some() =>
            {
                features.tools = true
            }
            "assistant"
                if message.get("tool_calls").is_some()
                    && fields(message, &["role", "content", "tool_calls"])
                    && message["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty() && calls.iter().all(tool_call)) =>
            {
                features.tools = true;
                if content.is_null() {
                    continue;
                }
            }
            "system" | "developer" | "user" | "assistant"
                if fields(message, &["role", "content"]) => {}
            _ => return None,
        }
        if content.is_string() {
            continue;
        }
        for part in content.as_array()? {
            if part["type"] == "text" && part["text"].is_string() {
                continue;
            }
            if role != "user" || part["type"] != "image_url" || !inline_image(part) {
                return None;
            }
            features.images = true;
        }
    }
    Some(features)
}

pub async fn supports_body(
    storage: &Storage,
    settings: &Value,
    accounts: &[Value],
    model: &str,
    body: &Value,
    format: ChatFormat,
) -> bool {
    let Some(features) = inspect(body, format) else {
        return false;
    };
    if format == ChatFormat::Chat && !features.extended() {
        return true;
    }
    for account in accounts {
        let Ok(proxy) = egress(storage, settings, account).await else {
            return false;
        };
        let Ok(connection) = Connection::new(storage, account, proxy) else {
            return false;
        };
        let route = connection.model(model).await;
        if !features.supported(route.ok().flatten().as_ref(), format) {
            return false;
        }
    }
    true
}

pub struct Failure {
    pub status: StatusCode,
    pub message: String,
}

impl Failure {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
}

pub async fn complete(
    storage: &Storage,
    account: &Value,
    model: &str,
    body: &Value,
    thread: &str,
    format: ChatFormat,
    route: Option<ProtectedEgress>,
) -> Result<Response, Failure> {
    let Completion {
        response,
        provider,
        secrets,
    } = request_completion(
        storage,
        account,
        model,
        body,
        CompletionMode::Gateway(thread, format),
        route,
    )
    .await?;
    if response.status().is_success() {
        return Ok(wire::adapt(response, provider, model, secrets));
    }
    let status = response.status();
    let raw = read_body(response, METADATA_LIMIT)
        .await
        .unwrap_or_default();
    let value: Value = serde_json::from_slice(&raw).unwrap_or_default();
    let raw = String::from_utf8_lossy(&raw);
    let code = text(&value["code"]).or_else(|| text(&value["error"]["code"]));
    let reason = text(&value["message"])
        .or_else(|| text(&value["error"]["message"]))
        .or_else(|| (!raw.is_empty()).then_some(raw.as_ref()))
        .or_else(|| status.canonical_reason())
        .unwrap_or("Zed upstream error");
    let message = match code {
        Some("trial_blocked") => format!(
            "Zed trial access is blocked upstream. The account can list hosted models, but Zed is refusing completions until trial/billing access is enabled or unblocked. Zed says: {reason}",
        ),
        Some(code) => format!("Zed {code}: {reason}"),
        None => reason.to_owned(),
    };
    let private = secrets.each_ref().map(String::as_str);
    Err(Failure {
        status,
        message: safe_text(&message, &private).chars().take(2000).collect(),
    })
}

pub(super) async fn translator_reply(
    storage: &Storage,
    account: &Value,
    model: &str,
    body: &Value,
    route: Option<ProtectedEgress>,
) -> Result<(Response, Vec<Vec<u8>>), Failure> {
    let Completion {
        response,
        provider,
        secrets,
    } = request_completion(
        storage,
        account,
        model,
        body,
        CompletionMode::Translator,
        route,
    )
    .await?;
    let redactions = secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .map(|secret| secret.as_bytes().to_vec())
        .collect();
    let response = if response.status().is_success() {
        wire::adapt(response, provider, model, secrets)
    } else {
        response
    };
    Ok((response, redactions))
}

struct Completion {
    response: Response,
    provider: Provider,
    secrets: [String; 5],
}

#[derive(Clone, Copy)]
enum CompletionMode<'a> {
    Gateway(&'a str, ChatFormat),
    Translator,
}

pub(super) fn thread_id(
    source: &Value,
    headers: &header::HeaderMap,
    connection: &str,
    caller: Option<&str>,
) -> String {
    crate::gemini_signatures::session_id(headers, source).unwrap_or_else(|| {
        let prefix = crate::gemini_signatures::assistant_prefix(source);
        crate::gemini_signatures::implicit_session("zed", connection, caller, &prefix)
    })
}

async fn request_completion(
    storage: &Storage,
    account: &Value,
    model: &str,
    body: &Value,
    mode: CompletionMode<'_>,
    route: Option<ProtectedEgress>,
) -> Result<Completion, Failure> {
    let format = match mode {
        CompletionMode::Gateway(_, format) => format,
        CompletionMode::Translator => ChatFormat::Chat,
    };
    let invalid = || Failure {
        status: StatusCode::BAD_REQUEST,
        message: "The Zed model does not support this request".into(),
    };
    let features = inspect(body, format).ok_or_else(invalid)?;
    let protected = route.is_some();
    let connection = Connection::new(storage, account, route).map_err(Failure::transport)?;
    let route = match connection.model(model).await {
        Ok(route) => route,
        Err(message) if protected => return Err(Failure::transport(message)),
        Err(_) => None,
    };
    let provider = Provider::resolve(route.as_ref().map(|route| route.provider.as_str()), model);
    if !features.supported(route.as_ref(), format) {
        return Err(invalid());
    }
    let mut request = provider.translate(body, model).ok_or_else(|| Failure {
        status: StatusCode::BAD_REQUEST,
        message: "Could not normalize the Zed request".into(),
    })?;
    if (features.extended() || format == ChatFormat::Messages)
        && let Some(system) = request.get_mut("system").and_then(Value::as_array_mut)
        && system.first().is_some_and(|block| {
            block["type"] == "text"
                && block["text"] == "You are Claude Code, Anthropic's official CLI for Claude."
        })
    {
        system.remove(0);
        if system.is_empty()
            && let Some(fields) = request.as_object_mut()
        {
            fields.shift_remove("system");
        }
    }
    if matches!(mode, CompletionMode::Translator) && matches!(provider, Provider::Xai) {
        request["stream"] = json!(body["stream"] != false);
    }
    let mut payload = json!({
        "provider":provider.wire(),"model":model,"provider_request":request,
    });
    let thread = text(&body["thread_id"]).or(match mode {
        CompletionMode::Gateway(thread, _) => Some(thread),
        CompletionMode::Translator => None,
    });
    if let Some(thread) = thread {
        payload["thread_id"] = json!(thread);
    }
    if let Some(prompt) = text(&body["prompt_id"]) {
        payload["prompt_id"] = json!(prompt);
    }
    let (response, token, rejected) = connection
        .call(
            COMPLETIONS,
            |client, url| completion_headers(client.post(url)).json(&payload),
            true,
        )
        .await
        .map_err(Failure::transport)?;
    if (protected || matches!(mode, CompletionMode::Translator))
        && response.status().is_redirection()
    {
        return Err(Failure::transport("Zed upstream redirected the request"));
    }
    let mut secrets = [
        connection.credentials.token.to_owned(),
        token,
        rejected.unwrap_or_default(),
        connection.credentials.system.unwrap_or_default().to_owned(),
        connection
            .relay
            .as_ref()
            .and_then(Route::relay_secret)
            .unwrap_or_default()
            .to_owned(),
    ];
    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    Ok(Completion {
        response,
        provider,
        secrets,
    })
}

async fn send(request: RequestBuilder) -> Result<Response, String> {
    tokio::time::timeout(FIRST_REPLY_TIMEOUT, request.send())
        .await
        .map_err(|_| "Zed upstream response timed out".to_owned())?
        .map_err(|_| "Could not reach Zed upstream".to_owned())
}

fn account_cache(key: [u8; 32]) -> Arc<AccountState> {
    let mut cache = ACCOUNTS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(position) = cache.iter().position(|(stored, _)| *stored == key) {
        let slot = cache.remove(position).expect("existing Zed account cache");
        let state = slot.1.clone();
        cache.push_back(slot);
        return state;
    }
    let state = Arc::new(AccountState::default());
    if cache.len() == ACCOUNT_LIMIT {
        let Some(position) = cache
            .iter()
            .position(|(_, state)| Arc::strong_count(state) == 1)
        else {
            return state;
        };
        cache.remove(position);
    }
    cache.push_back((key, state.clone()));
    state
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().map(js_trim).filter(|text| !text.is_empty())
}

fn wire_token(value: &str) -> bool {
    value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn identifier(value: &Value) -> Option<String> {
    text(value)
        .or_else(|| text(&value[0]))
        .or_else(|| text(&value["id"]))
        .map(str::to_owned)
        .or_else(|| {
            value
                .as_i64()
                .filter(|id| *id != 0)
                .map(|id| id.to_string())
        })
}

fn organization(user: &Value) -> Option<String> {
    identifier(&user["default_organization_id"])
        .or_else(|| identifier(&user["defaultOrganizationId"]))
        .or_else(|| {
            let rows = user["organizations"].as_array()?;
            let row = rows
                .iter()
                .find(|row| truthy(&row["is_personal"]))
                .or_else(|| rows.first())?;
            identifier(&row["id"])
        })
}

fn map_models(value: &Value, secrets: &[&str]) -> Catalog {
    let mut models = Vec::new();
    let mut routes = HashMap::new();
    for entry in value["models"].as_array().into_iter().flatten() {
        let Some(id) = identifier(&entry["id"]) else {
            continue;
        };
        let id = safe_text(&id, secrets);
        if let Some(provider) = text(&entry["provider"]) {
            routes.insert(
                id.clone(),
                HostedModel {
                    provider: safe_text(provider, secrets),
                    disabled: truthy(&entry["is_disabled"]),
                    tools: truthy(&entry["supports_tools"]),
                    images: truthy(&entry["supports_images"]),
                },
            );
        }
        if truthy(&entry["is_disabled"]) {
            continue;
        }
        let name = text(&entry["display_name"])
            .or_else(|| text(&entry["displayName"]))
            .unwrap_or(&id);
        let mut mapped = json!({"id":id,"name":safe_text(name,secrets)});
        for (key, aliases) in [
            ("provider", ["provider", "provider"]),
            ("contextLength", ["max_token_count", "maxTokenCount"]),
            (
                "contextLengthInMaxMode",
                ["max_token_count_in_max_mode", "maxTokenCountInMaxMode"],
            ),
            ("maxOutputTokens", ["max_output_tokens", "maxOutputTokens"]),
        ] {
            if let Some(value) = entry
                .get(aliases[0])
                .filter(|value| !value.is_null())
                .or_else(|| entry.get(aliases[1]))
            {
                mapped[key] = sanitized(value, secrets);
            }
        }
        for (key, field) in [
            ("supportsTools", "supports_tools"),
            ("supportsImages", "supports_images"),
            ("supportsThinking", "supports_thinking"),
            ("supportsDisablingThinking", "supports_disabling_thinking"),
            ("supportsFastMode", "supports_fast_mode"),
            (
                "supportsServerSideCompaction",
                "supports_server_side_compaction",
            ),
            ("supportsStreamingTools", "supports_streaming_tools"),
            ("supportsParallelToolCalls", "supports_parallel_tool_calls"),
        ] {
            mapped[key] = json!(truthy(&entry[field]));
        }
        mapped["supportedEffortLevels"] = entry["supported_effort_levels"]
            .as_array()
            .or_else(|| entry["supportedEffortLevels"].as_array())
            .map_or_else(
                || json!([]),
                |values| {
                    json!(
                        values
                            .iter()
                            .map(|value| sanitized(value, secrets))
                            .collect::<Vec<_>>()
                    )
                },
            );
        models.push(mapped);
    }
    Catalog { models, routes }
}

fn sanitized(value: &Value, secrets: &[&str]) -> Value {
    match value {
        Value::String(text) => json!(safe_text(text, secrets)),
        Value::Array(values) => json!(
            values
                .iter()
                .map(|value| sanitized(value, secrets))
                .collect::<Vec<_>>()
        ),
        Value::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(name, value)| (safe_text(name, secrets), sanitized(value, secrets)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn safe_text(value: &str, secrets: &[&str]) -> String {
    let mut value = value.to_owned();
    for secret in secrets {
        if !secret.is_empty() && value.contains(secret) {
            value = value.replace(secret, "[redacted]");
        }
    }
    value
}

async fn read_body(mut response: Response, limit: usize) -> Result<Vec<u8>, ()> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if chunk.len() > limit - bytes.len() {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
