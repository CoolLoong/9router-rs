use std::sync::LazyLock;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, Utc};
use reqwest::{Client, Method, Request, RequestBuilder, Response, Url, header};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;

use super::{RefreshError, flow::nonempty, store::expiry};
use crate::provider_defaults;
use crate::provider_http::{self, Delivery, http_client};
use crate::storage::Storage;

pub(crate) const ATTEMPT: &str = "grokCliRefreshAttempt";
pub(super) const CONTEXT: &[&str] = &[
    "connectionProxyEnabled",
    "connectionProxyUrl",
    "connectionNoProxy",
    "proxyPoolId",
    "strictProxy",
    "vercelRelayUrl",
];

const RESPONSE_LIMIT: usize = 128 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
static CLIENT: LazyLock<Result<Client, reqwest::Error>> =
    LazyLock::new(|| http_client(TIMEOUT).build());
static PINNED: LazyLock<Option<RepositoryConfig>> = LazyLock::new(RepositoryConfig::pinned);
static DISCOVERED: OnceCell<Url> = OnceCell::const_new();

struct RepositoryConfig {
    issuer: Url,
    discovery: Url,
    token: Url,
    client_id: String,
}

impl RepositoryConfig {
    fn pinned() -> Option<Self> {
        let constant = |key| provider_defaults::text("xai.oauth", key);
        let issuer = authorized_url(constant("ISSUER")?)?;
        let discovery = issuer.join(constant("DISCOVERY_PATH")?).ok()?;
        let token = issuer.join(constant("TOKEN_ENDPOINT_PATH")?).ok()?;
        let client_id = provider_defaults::text("xai.transport", "clientId")?.to_owned();
        Some(Self {
            issuer,
            discovery,
            token,
            client_id,
        })
    }
}

pub(super) fn marker(token: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"xai/grok-cli-refresh:");
    hash.update(token.as_bytes());
    URL_SAFE_NO_PAD.encode(hash.finalize())
}

pub(super) async fn background_route(
    storage: &Storage,
    account: &Value,
) -> Result<(bool, Option<[u8; 32]>), RefreshError> {
    use provider_http::ProtectedEgress;

    let config = PINNED.as_ref().ok_or(RefreshError::ProviderUnavailable)?;
    let settings = storage.read_settings().await.ok_or(RefreshError::Storage)?;
    let egress =
        provider_http::protected_egress(storage, &settings, account, config.discovery.as_str())
            .await
            .map_err(|_| RefreshError::Unsupported)?;
    match egress {
        None => Ok((false, None)),
        Some(ProtectedEgress::Tunnel(_) | ProtectedEgress::Bypass) => Ok((true, None)),
        Some(ProtectedEgress::Relay(route)) if route.relay_secret().is_some() => {
            Ok((true, route.relay_identity()))
        }
        Some(ProtectedEgress::Relay(_)) => Err(RefreshError::Unsupported),
    }
}

pub(super) async fn prepare(
    storage: &Storage,
    account: &Value,
    token: &str,
    protected: bool,
    relay: Option<[u8; 32]>,
) -> Result<Prepared, RefreshError> {
    let config = PINNED.as_ref().ok_or(RefreshError::ProviderUnavailable)?;
    if token.len() > RESPONSE_LIMIT {
        return Err(RefreshError::Unsupported);
    }
    let client = CLIENT
        .as_ref()
        .map_err(|_| RefreshError::ProviderUnavailable)?;
    let request = if let Some(identity) = relay {
        let discovery = provider_http::bind_relay(
            storage,
            account,
            config.discovery.as_str(),
            client,
            TIMEOUT,
            identity,
        )
        .await
        .ok_or(RefreshError::Unsupported)?;
        let token_url = discover(config, discovery.request(Method::GET))
            .await?
            .unwrap_or_else(|| config.token.clone());
        provider_http::bind_relay(
            storage,
            account,
            token_url.as_str(),
            client,
            TIMEOUT,
            identity,
        )
        .await
        .ok_or(RefreshError::Unsupported)?
        .request(Method::POST)
    } else {
        let settings = storage.read_settings().await.ok_or(RefreshError::Storage)?;
        let discovery =
            checked_delivery(storage, &settings, account, &config.discovery, protected).await?;
        let discovery_url =
            endpoint(&config.discovery, &discovery).ok_or(RefreshError::ProviderUnavailable)?;
        let token_url = if protected {
            let discovery_client = discovery
                .client(client, TIMEOUT)
                .ok_or(RefreshError::ProviderUnavailable)?;
            discover(config, discovery_client.get(discovery_url))
                .await?
                .unwrap_or_else(|| config.token.clone())
        } else {
            DISCOVERED
                .get_or_try_init(|| async {
                    discover(config, client.get(discovery_url))
                        .await
                        .ok()
                        .flatten()
                        .ok_or(())
                })
                .await
                .unwrap_or(&config.token)
                .clone()
        };
        let delivery = checked_delivery(storage, &settings, account, &token_url, protected).await?;
        let endpoint = endpoint(&token_url, &delivery).ok_or(RefreshError::ProviderUnavailable)?;
        let client = delivery
            .client(client, TIMEOUT)
            .ok_or(RefreshError::ProviderUnavailable)?;
        client.post(endpoint)
    };
    let (client, request) = request
        .header(header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", &config.client_id),
            ("refresh_token", token),
        ])
        .build_split();
    let request = request.map_err(|_| RefreshError::ProviderUnavailable)?;
    Ok(Prepared { client, request })
}

async fn checked_delivery(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    url: &Url,
    protected: bool,
) -> Result<Delivery, RefreshError> {
    let delivery = Delivery::select(storage, settings, account, url.as_str())
        .await
        .map_err(|_| RefreshError::Unsupported)?;
    if delivery.protected() != protected {
        return Err(RefreshError::Unsupported);
    }
    Ok(delivery)
}

async fn discover(
    config: &RepositoryConfig,
    request: RequestBuilder,
) -> Result<Option<Url>, RefreshError> {
    let response = request
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| RefreshError::ProviderUnavailable)?;
    if !response.status().is_success() {
        return Err(RefreshError::ProviderUnavailable);
    }
    Ok(bounded(response)
        .await
        .and_then(|value| token_endpoint(config, &value)))
}

fn token_endpoint(config: &RepositoryConfig, value: &Value) -> Option<Url> {
    if let Some(issuer) = value.get("issuer") {
        let issuer = authorized_url(issuer.as_str()?)?;
        if issuer != config.issuer {
            return None;
        }
    }
    authorized_url(value["authorization_endpoint"].as_str()?)?;
    authorized_url(value["token_endpoint"].as_str()?)
}

fn authorized_url(input: &str) -> Option<Url> {
    let url = Url::parse(input.trim()).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && (host == "x.ai" || host.ends_with(".x.ai"))
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

fn endpoint(url: &Url, delivery: &Delivery) -> Option<Url> {
    if !delivery.use_fixture() {
        return Some(url.clone());
    }
    let mut fixture = Url::parse(&format!(
        "http://127.0.0.1:52989/xai-oauth/{}{}",
        url.host_str()?,
        url.path()
    ))
    .ok()?;
    fixture.set_query(url.query());
    Some(fixture)
}

pub(super) struct Prepared {
    client: Client,
    request: Request,
}

pub(super) async fn execute(prepared: Prepared) -> Result<Map<String, Value>, RefreshError> {
    let response = prepared
        .client
        .execute(prepared.request)
        .await
        .map_err(|_| RefreshError::ReauthenticationRequired)?;
    let status = response.status();
    let response = bounded(response)
        .await
        .ok_or(RefreshError::ReauthenticationRequired)?;
    if !status.is_success() {
        return Err(
            if matches!(
                response["error"].as_str(),
                Some("invalid_grant" | "invalid_request")
            ) {
                RefreshError::ReauthenticationRequired
            } else {
                RefreshError::ProviderUnavailable
            },
        );
    }
    let access = nonempty(&response["access_token"])
        .filter(|token| {
            token.len() <= 16 * 1024 && token.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or(RefreshError::ReauthenticationRequired)?;
    let now = Utc::now();
    let lifetime = if let Some(value) = response.get("expires_in").filter(|value| !value.is_null())
    {
        let seconds = value
            .as_i64()
            .or_else(|| value.as_str()?.parse().ok())
            .filter(|seconds| *seconds > 0)
            .ok_or(RefreshError::ReauthenticationRequired)?;
        json!(seconds)
    } else {
        Value::Null
    };
    let expires =
        expiry(now, Some(&lifetime)).map_err(|_| RefreshError::ReauthenticationRequired)?;
    let mut patch = Map::from_iter([
        ("accessToken".into(), json!(access)),
        ("expiresAt".into(), expires),
        ("expiresIn".into(), lifetime),
        ("tokenExpiresAt".into(), Value::Null),
        (
            "lastRefreshAt".into(),
            json!(now.to_rfc3339_opts(SecondsFormat::Millis, true)),
        ),
        (ATTEMPT.into(), Value::Null),
    ]);
    let rotated = &response["refresh_token"];
    if !rotated.is_null() && rotated != "" {
        let rotated = nonempty(rotated)
            .filter(|token| token.len() <= RESPONSE_LIMIT)
            .ok_or(RefreshError::ReauthenticationRequired)?;
        patch.insert("refreshToken".into(), json!(rotated));
    }
    if let Some(id) = nonempty(&response["id_token"]) {
        patch.insert("idToken".into(), json!(id));
    }
    Ok(patch)
}

async fn bounded(mut response: Response) -> Option<Value> {
    if response
        .content_length()
        .is_some_and(|size| size > RESPONSE_LIMIT as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > RESPONSE_LIMIT - body.len() {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}
