use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Duration;

use reqwest::{Client, Method, RequestBuilder, Response, StatusCode, Url, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::chat_accounts;
use crate::chat_input::{js_json, truthy};
use crate::chat_output::js_text;
use crate::provider_http::{Delivery, requires_protection};
use crate::proxy_transport;
use crate::qoder_models;
use crate::storage::Storage;

#[path = "qoder_chat_attachments.rs"]
mod attachments;
#[path = "qoder_chat_payload.rs"]
mod payload;
#[path = "qoder_chat_queue.rs"]
mod queue;
#[path = "qoder_chat_sse.rs"]
mod sse;

const CHAT_PATH: &str = "/algo/api/v2/service/pro/sse/agent_chat_generation";
const QUEUE_PATH: &str = "/algo/api/v2/service/ask/queue/status";
const TIMEOUT: Duration = Duration::from_secs(120);
const METADATA_TIMEOUT: Duration = Duration::from_secs(20);
const DATA_LIMIT: usize = 2 * 1024 * 1024;
static HTTP: LazyLock<Option<Client>> = LazyLock::new(|| client_builder().build().ok());

pub(super) struct Failure {
    pub status: StatusCode,
    pub message: String,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn unavailable() -> Self {
        Self::upstream("Qoder connection is unavailable")
    }
}

pub(super) fn supports(account: &Value) -> bool {
    account["provider"]
        .as_str()
        .is_some_and(qoder_models::supports)
        && matches!(
            account["authType"].as_str(),
            Some("oauth" | "apikey" | "api_key")
        )
}

pub(super) fn preview(account: &Value, body: Value) -> Option<Value> {
    if !supports(account) {
        return None;
    }
    let raw = saved_token(account);
    let job = raw.starts_with("pt-")
        || account["accessToken"]
            .as_str()
            .is_some_and(|token| token.starts_with("jt-"));
    if raw.is_empty()
        || !raw.starts_with("pt-")
            && (!truthy(&account["providerSpecificData"]["userId"])
                || !truthy(&account["accessToken"]))
    {
        return None;
    }
    let endpoint = inference_url(inference_origin(account["provider"].as_str()?, job)).ok()?;
    Some(json!({"url":endpoint.as_str(),"headers":{
        "Content-Type":"application/json","Accept":"text/event-stream","Cache-Control":"no-cache","Authorization":"[redacted]",
    },"body":body}))
}

fn saved_token(account: &Value) -> &str {
    account["apiKey"]
        .as_str()
        .filter(|token| !token.is_empty())
        .or_else(|| account["accessToken"].as_str())
        .unwrap_or_default()
}

fn inference_origin(provider: &str, job_token: bool) -> &'static str {
    match (provider, job_token) {
        ("qoder-cn", _) => "https://gateway.qoder.com.cn",
        (_, true) => "https://api2.qoder.sh",
        _ => "https://api3.qoder.sh",
    }
}

fn inference_url(origin: &str) -> Result<Url, Failure> {
    let mut inference =
        Url::parse(&format!("{origin}{CHAT_PATH}")).map_err(|_| Failure::unavailable())?;
    inference
        .query_pairs_mut()
        .append_pair("FetchKeys", "llm_model_result")
        .append_pair("AgentId", "agent_common")
        .append_pair("Encode", "1");
    Ok(inference)
}

pub(super) async fn protects_chat(storage: &Storage, settings: &Value, provider: &str) -> bool {
    requires_protection(settings, &Value::Null)
        || chat_accounts::active_connections(storage, provider)
            .await
            .map_or(true, |accounts| {
                accounts
                    .iter()
                    .any(|account| requires_protection(settings, account))
            })
}

pub(super) async fn complete(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
) -> Result<Response, Failure> {
    let provider = account["provider"]
        .as_str()
        .filter(|provider| qoder_models::supports(provider))
        .ok_or_else(Failure::unavailable)?;
    let key = model.strip_prefix("qoder/").unwrap_or(model);
    let raw_token = saved_token(account);
    let (access, user) = qoder_models::credential(storage, settings, account)
        .await
        .ok_or_else(|| {
            let details = &account["providerSpecificData"];
            let message = if raw_token.starts_with("pt-") {
                "qoder PAT exchange failed"
            } else if !truthy(&details["userId"]) {
                "qoder credential is missing userId; reconnect the account"
            } else {
                "qoder credential is missing accessToken; reconnect the account"
            };
            Failure::new(StatusCode::UNAUTHORIZED, message)
        })?;
    let origin = inference_origin(provider, access.starts_with("jt-"));
    let inference = inference_url(origin)?;
    let delivery = Delivery::select(storage, settings, account, inference.as_str())
        .await
        .map_err(|_| Failure::unavailable())?;
    let signing = if account["providerSpecificData"]["machineId"]
        .as_str()
        .is_some_and(|id| !id.is_empty())
    {
        Cow::Borrowed(account)
    } else {
        let mut signing = account.clone();
        if !signing["providerSpecificData"].is_object() {
            signing["providerSpecificData"] = serde_json::json!({});
        }
        signing["providerSpecificData"]["machineId"] = Value::String(machine_id(&user));
        Cow::Owned(signing)
    };
    let service = Service {
        storage,
        account,
        signing,
        provider,
        origin,
        access,
        user,
        delivery,
    };
    let config = qoder_models::raw_config(storage, settings, account, &service.access, &service.user, key)
        .await
        .or_else(|| payload::static_config(key))
        .ok_or_else(|| Failure::new(
            StatusCode::BAD_REQUEST,
            format!("qoder: model_config for \"{key}\" not yet known (run a model list fetch or check upstream connectivity)"),
        ))?;
    let mut messages = body["messages"].as_array().cloned().unwrap_or_default();
    attachments::rewrite(&mut messages, &service).await;
    let payload = payload::build(key, body, messages, config, &service.user)
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "Invalid Qoder request"))?;
    let plain = js_json(&payload, false)
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "Invalid Qoder request"))?;
    let outgoing = payload::encode(plain.as_bytes());
    let source = payload["model_config"]["source"]
        .as_str()
        .filter(|source| !source.is_empty())
        .unwrap_or("system");
    let started = Instant::now();
    let budget = queue::budget();
    let mut status_polls = true;
    let mut ready_replay = false;

    loop {
        let request = service
            .request(&inference, Method::POST, &outgoing)
            .await?
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("X-Model-Key", key)
            .header("X-Model-Source", source)
            .body(outgoing.clone());
        let response = send(request, TIMEOUT).await?;
        if !response.status().is_success() {
            let status = response.status();
            let message = bounded(response)
                .await
                .map(|data| service.redact(&String::from_utf8_lossy(&data)))
                .filter(|message| !message.is_empty())
                .unwrap_or_else(|| format!("Qoder upstream returned {status}"));
            return Err(Failure::new(status, message));
        }

        let label = format!("{provider}/{key}");
        let (stream, rejection) = sse::Stream::open(response, label, service.secrets()).await?;
        let Some(rejection) = rejection else {
            return Ok(stream.into_response(inference.clone()));
        };
        drop(stream);
        let Some(info) = queue::Info::queued(&rejection.body) else {
            return Err(Failure::new(
                sse::failure_status(rejection.status, &rejection.body),
                service.redact(&rejection.body),
            ));
        };
        if started.elapsed() >= budget {
            return Err(queue_failure(key, &info, started.elapsed()));
        }
        let mut delay = info.delay(false);
        if status_polls {
            match service.poll(key, &payload, &info).await {
                Some(state) if state.is_queued == Some(false) && !ready_replay => {
                    ready_replay = true;
                    delay = Duration::ZERO;
                }
                Some(state) if state.is_queued == Some(true) => delay = state.delay(true),
                _ => status_polls = false,
            }
        }
        let remaining = budget.saturating_sub(started.elapsed());
        if delay >= remaining {
            tokio::time::sleep(remaining).await;
            return Err(queue_failure(key, &info, started.elapsed()));
        }
        tokio::time::sleep(delay).await;
    }
}

fn queue_failure(model: &str, info: &queue::Info, elapsed: Duration) -> Failure {
    Failure::new(
        StatusCode::SERVICE_UNAVAILABLE,
        format!(
            "qoder model \"{model}\" is still queued after {}s (queueType={}); retry shortly",
            elapsed.as_secs_f64().round(),
            info.queue_type.as_deref().unwrap_or("?"),
        ),
    )
}

struct Service<'a> {
    storage: &'a Storage,
    account: &'a Value,
    signing: Cow<'a, Value>,
    provider: &'a str,
    origin: &'static str,
    access: String,
    user: Value,
    delivery: Delivery,
}

impl Service<'_> {
    async fn request(
        &self,
        target: &Url,
        method: Method,
        body: &[u8],
    ) -> Result<RequestBuilder, Failure> {
        let settings = self
            .storage
            .read_settings()
            .await
            .ok_or_else(Failure::unavailable)?;
        let actual = Delivery::select(self.storage, &settings, self.account, target.as_str())
            .await
            .map_err(|_| Failure::unavailable())?;
        if actual != self.delivery {
            return Err(Failure::upstream(
                "Qoder proxy selection changed before sending",
            ));
        }
        let client = match &actual {
            Delivery::Tunnel(proxy) => {
                proxy_transport::vendor_metadata_client(proxy.clone(), client_builder())
            }
            _ => HTTP.as_ref().cloned(),
        }
        .ok_or_else(Failure::unavailable)?;
        let url = if actual.use_fixture() {
            let mut local = Url::parse(&format!(
                "http://127.0.0.1:52989/qoder-chat/{}/{}{}",
                self.provider,
                target.host_str().ok_or_else(Failure::unavailable)?,
                target.path(),
            ))
            .map_err(|_| Failure::unavailable())?;
            local.set_query(target.query());
            local
        } else {
            target.clone()
        };
        let path = target.path().strip_prefix("/algo").unwrap_or(target.path());
        let signed = qoder_models::cosy(&self.access, &self.user, &self.signing, path, body)
            .ok_or_else(|| Failure::new(StatusCode::UNAUTHORIZED, "qoder cosy signing failed"))?;
        Ok(client
            .request(method, url)
            .headers(signed)
            .header(header::ACCEPT_ENCODING, "identity"))
    }

    async fn poll(&self, key: &str, payload: &Value, queued: &queue::Info) -> Option<queue::Info> {
        let mut target = Url::parse(&format!("{}{QUEUE_PATH}", self.origin)).ok()?;
        {
            let mut query = target.query_pairs_mut();
            query.append_pair("requestSetId", payload["request_set_id"].as_str()?);
            query.append_pair("modelKey", queued.model_key.as_deref().unwrap_or(key));
            if let Some(kind) = queued.queue_type.as_deref() {
                query.append_pair("queueType", kind);
            }
        }
        let correlations = header::HeaderMap::from_iter([
            (
                header::HeaderName::from_static("x-request-id"),
                header::HeaderValue::from_str(payload["request_id"].as_str()?).ok()?,
            ),
            (
                header::HeaderName::from_static("x-session-id"),
                header::HeaderValue::from_str(payload["session_id"].as_str()?).ok()?,
            ),
        ]);
        let request = self
            .request(&target, Method::GET, b"")
            .await
            .ok()?
            .header(header::ACCEPT, "application/json")
            .headers(correlations);
        let response = send(request, METADATA_TIMEOUT)
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        queue::Info::status(&String::from_utf8_lossy(&bounded(response).await?))
    }

    fn secrets(&self) -> Vec<String> {
        let mut secrets = vec![self.access.clone()];
        secrets.extend(
            ["accessToken", "apiKey", "refreshToken"]
                .into_iter()
                .filter_map(|key| self.account[key].as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        );
        secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        secrets
    }

    fn redact(&self, message: &str) -> String {
        redact(message, &self.secrets())
    }
}

fn redact(message: &str, secrets: &[String]) -> String {
    secrets.iter().fold(message.to_owned(), |text, secret| {
        text.replace(secret, "[redacted]")
    })
}

pub(super) fn machine_id(user: &Value) -> String {
    let hash = format!(
        "{:x}",
        Sha256::new()
            .chain_update(b"qoder-machine\0")
            .chain_update(js_text(user))
            .finalize()
    );
    format!(
        "{}-{}-5{}-a{}-{}",
        &hash[..8],
        &hash[8..12],
        &hash[13..16],
        &hash[17..20],
        &hash[20..32]
    )
}

fn client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .connect_timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

async fn send(request: RequestBuilder, timeout: Duration) -> Result<Response, Failure> {
    match tokio::time::timeout(timeout, request.send()).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) if !error.is_timeout() => Err(Failure::unavailable()),
        _ => Err(Failure::new(
            StatusCode::GATEWAY_TIMEOUT,
            "Qoder upstream timed out",
        )),
    }
}

async fn bounded(mut response: Response) -> Option<Vec<u8>> {
    tokio::time::timeout(METADATA_TIMEOUT, async {
        let mut output = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if chunk.len() > DATA_LIMIT - output.len() {
                return None;
            }
            output.extend_from_slice(&chunk);
        }
        Some(output)
    })
    .await
    .ok()
    .flatten()
}
