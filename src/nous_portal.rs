use std::collections::HashSet;
use std::time::Duration;

use reqwest::header::{self, HeaderValue};
use reqwest::{Client, Method, Request, StatusCode, Url};
use serde_json::{Map, Value, json};

use crate::provider_http::{self, http_client};
use crate::provider_index::js_trim;
use crate::storage::Storage;

const PORTAL: &str = "https://portal.nousresearch.com/api/oauth";
const FIXTURE: &str = "http://127.0.0.1:52989/nous-device";
const CLIENT_ID: &str = "hermes-cli";
const SCOPE: &str = "inference:invoke inference:mint_agent_key";
const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const RESPONSE_LIMIT: usize = 128 * 1024;
pub(crate) const INFERENCE: &str = "https://inference-api.nousresearch.com/v1";

pub(crate) fn compatible_tags(
    provider: &str,
    destination: &str,
    account: &Value,
    body: &Value,
) -> Option<Vec<Value>> {
    if !provider.starts_with("openai-compatible-")
        || !crate::chat_input::truthy(&account["apiKey"])
        || !body.is_object()
        || !(body["tags"].is_null() || body["tags"].is_array())
        || !Url::parse(destination).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("inference-api.nousresearch.com")
                && url.path() == "/v1/chat/completions"
        })
    {
        return None;
    }
    let mut tags = body["tags"].as_array().cloned().unwrap_or_default();
    for prefix in ["product=", "client=", "user="] {
        tags.retain(|tag| {
            tag.as_str()
                .and_then(|tag| tag.strip_prefix(prefix))
                .is_none_or(|value| !js_trim(value).is_empty())
        });
        if !tags
            .iter()
            .any(|tag| tag.as_str().is_some_and(|tag| tag.starts_with(prefix)))
        {
            tags.push(json!(format!("{prefix}9router")));
        }
    }
    let mut primitives = HashSet::new();
    let mut numbers = HashSet::new();
    tags.retain(|tag| match tag {
        Value::Array(_) | Value::Object(_) => true,
        Value::Number(number) => number
            .as_f64()
            .is_none_or(|value| numbers.insert(if value == 0.0 { 0 } else { value.to_bits() })),
        _ => primitives.insert(tag.to_string()),
    });
    Some(tags)
}

#[derive(Clone)]
pub(crate) struct Portal {
    client: Client,
}

pub(crate) enum Endpoint<'a> {
    DeviceCode,
    Token(&'a str),
    AgentKey(&'a str),
    Refresh(&'a str),
}

pub(crate) struct Prepared {
    client: Client,
    request: Request,
}

impl Portal {
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: http_client(Duration::from_secs(15)).build()?,
        })
    }

    pub(crate) async fn send(
        &self,
        storage: &Storage,
        account: &Value,
        step: Endpoint<'_>,
    ) -> Result<(StatusCode, Value), ()> {
        self.prepare(storage, account, step, None)
            .await?
            .send()
            .await
    }

    pub(crate) async fn mint(
        &self,
        storage: &Storage,
        account: &Value,
        access: &str,
        expected: Option<bool>,
    ) -> Value {
        let Ok(request) = self
            .prepare(storage, account, Endpoint::AgentKey(access), expected)
            .await
        else {
            return Value::Null;
        };
        match request.send().await {
            Ok((status, minted)) if status.is_success() => minted,
            _ => Value::Null,
        }
    }

    pub(crate) async fn prepare(
        &self,
        storage: &Storage,
        account: &Value,
        step: Endpoint<'_>,
        expected: Option<bool>,
    ) -> Result<Prepared, ()> {
        let path = match step {
            Endpoint::DeviceCode => "device/code",
            Endpoint::Token(_) | Endpoint::Refresh(_) => "token",
            Endpoint::AgentKey(_) => "agent-key",
        };
        let destination = provider_http::bind(
            storage,
            account,
            &format!("{PORTAL}/{path}"),
            &format!("{FIXTURE}/{path}"),
            &self.client,
            Duration::from_secs(15),
            expected,
        )
        .await
        .ok_or(())?;
        let request = destination
            .request(Method::POST)
            .header(header::ACCEPT, "application/json");
        let request = match step {
            Endpoint::DeviceCode => request.form(&[("client_id", CLIENT_ID), ("scope", SCOPE)]),
            Endpoint::Token(code) => request.form(&[
                ("grant_type", GRANT),
                ("client_id", CLIENT_ID),
                ("device_code", code),
            ]),
            Endpoint::AgentKey(token) => request
                .bearer_auth(token)
                .json(&json!({"min_ttl_seconds":7200})),
            Endpoint::Refresh(token) => {
                let mut secret = HeaderValue::from_str(token).map_err(|_| ())?;
                secret.set_sensitive(true);
                request
                    .header("x-nous-refresh-token", secret)
                    .timeout(Duration::from_secs(30))
                    .form(&[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", token),
                        ("client_id", CLIENT_ID),
                    ])
            }
        };
        let (client, request) = request.build_split();
        let request = request.map_err(|_| ())?;
        Ok(Prepared { client, request })
    }
}

impl Prepared {
    pub(crate) async fn send(self) -> Result<(StatusCode, Value), ()> {
        let mut response = self.client.execute(self.request).await.map_err(|_| ())?;
        let status = response.status();
        if status.is_redirection() {
            return Err(());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if bytes.len() + chunk.len() > RESPONSE_LIMIT {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        let body =
            serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"error":"invalid_response"}));
        Ok((status, body))
    }
}

pub(crate) fn credentials<'a>(access: &'a str, minted: &'a Value) -> (&'a str, Map<String, Value>) {
    let key = minted["api_key"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(access);
    let details = Map::from_iter([
        ("inferenceBaseUrl".into(), json!(INFERENCE)),
        ("agentKeyId".into(), minted["key_id"].clone()),
        ("agentKeyExpiresAt".into(), minted["expires_at"].clone()),
    ]);
    (key, details)
}
