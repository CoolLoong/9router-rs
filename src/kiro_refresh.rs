use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{SecondsFormat, Utc};
use reqwest::{Client, ClientBuilder, Method, Request, Response, Url, header};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::refresh::{RefreshMode, kiro_due, refresh_account};
use super::{RefreshError, store::expiry};
use crate::kiro_models::OAuthMethod;
use crate::provider_connections::truthy;
use crate::provider_http::{self, Delivery};
use crate::provider_index::js_trim;
use crate::proxy_transport;
use crate::storage::Storage;

pub(crate) const ATTEMPT: &str = "kiroRefreshAttempt";
pub(super) const LEGACY_FIELDS: &[&str] = &["clientId", "clientSecret", "region", "apiKey"];
pub(super) const CONTEXT: &[&str] = &[
    "authMethod",
    "profileArn",
    "region",
    "clientId",
    "client_id",
    "clientSecret",
    "tokenEndpoint",
    "token_endpoint",
    "scope",
    "scopes",
    "connectionProxyEnabled",
    "connectionProxyUrl",
    "connectionNoProxy",
    "proxyPoolId",
    "strictProxy",
    "vercelRelayUrl",
];
pub(crate) const SOCIAL: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken";
const RESPONSE_LIMIT: usize = 128 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
static CLIENT: LazyLock<Result<Client, reqwest::Error>> =
    LazyLock::new(|| client_builder().build());

fn client_builder() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

pub(crate) fn supports(account: &Value, protected: bool) -> bool {
    Grant::for_route(account, protected).is_some()
}

pub(crate) fn supports_relay(account: &Value) -> bool {
    Grant::for_relay(account).is_some()
}

pub(crate) async fn test_expiry(
    storage: &Storage,
    account: Value,
) -> (Value, Option<String>, bool) {
    if !truthy(&account["accessToken"]) {
        return (account, Some("No access token".into()), false);
    }
    if !kiro_due(&account) {
        return (account, None, false);
    }
    if !truthy(&account["refreshToken"]) {
        return (account, Some("Token expired".into()), false);
    }
    if let Some(grant) = Grant::for_test(&account, false).or_else(|| Grant::for_relay(&account))
        && let Some(settings) = storage.read_settings().await
        && let Some(mode) = test_mode(storage, &settings, &account, grant.url.as_str()).await
    {
        if let Ok(latest) = refresh_account(storage, &account, mode).await
            && truthy(&latest["accessToken"])
        {
            let refreshed = latest[ATTEMPT].is_null()
                && truthy(&latest["lastRefreshAt"])
                && latest["lastRefreshAt"] != account["lastRefreshAt"];
            if refreshed || !kiro_due(&latest) {
                return (latest, None, refreshed);
            }
        }
    }
    (
        account,
        Some("Token expired and refresh failed".into()),
        false,
    )
}

async fn test_mode(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    endpoint: &str,
) -> Option<RefreshMode> {
    use proxy_transport::{Route, Selection};
    match provider_http::select(storage, settings, account, endpoint)
        .await
        .ok()?
    {
        Selection::Routed(route @ Route::Relay { .. }) if route.relay_secret().is_some() => {
            route.relay_identity().map(RefreshMode::RelayRouted)
        }
        _ => Delivery::select(storage, settings, account, endpoint)
            .await
            .ok()
            .map(|delivery| RefreshMode::KiroTest {
                protected: delivery.protected(),
            }),
    }
}

pub(super) async fn background_route(
    storage: &Storage,
    account: &Value,
) -> Result<(bool, Option<[u8; 32]>), RefreshError> {
    let grant = Grant::from_account(account)
        .or_else(|| Grant::for_relay(account))
        .ok_or(RefreshError::Unsupported)?;
    let settings = storage.read_settings().await.ok_or(RefreshError::Storage)?;
    match test_mode(storage, &settings, account, grant.url.as_str()).await {
        Some(RefreshMode::RelayRouted(identity)) => Ok((true, Some(identity))),
        Some(RefreshMode::KiroTest { protected }) => Ok((protected, None)),
        _ => Err(RefreshError::Unsupported),
    }
}

pub(crate) fn token_endpoint(value: &str) -> Result<Url, &'static str> {
    if value.is_empty() {
        return Err("token_endpoint is required");
    }
    let url = Url::parse(value).map_err(|_| "token_endpoint must be a valid URL")?;
    if url.scheme() != "https" {
        return Err("token_endpoint must use https");
    }
    if !matches!(
        url.host_str(),
        Some("login.microsoftonline.com" | "login.microsoft.com" | "login.windows.net")
    ) || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return Err("token_endpoint must be a Microsoft login endpoint");
    }
    Ok(url)
}

pub(super) fn marker(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}

pub(super) struct Grant<'a> {
    pub token: &'a str,
    url: Url,
    wire: Wire<'a>,
    profile_lookup_region: Option<&'a str>,
    test: bool,
}

enum Wire<'a> {
    Aws { client: &'a str, secret: &'a str },
    Social,
    Microsoft { client: &'a str, scope: String },
}

impl<'a> Grant<'a> {
    pub fn from_account(account: &'a Value) -> Option<Self> {
        Self::for_route(account, false)
    }

    pub fn for_route(account: &'a Value, protected: bool) -> Option<Self> {
        Self::saved(account, protected, false, false)
    }

    pub fn for_test(account: &'a Value, protected: bool) -> Option<Self> {
        Self::saved(account, protected, true, false)
    }

    pub fn for_relay(account: &'a Value) -> Option<Self> {
        Self::saved(account, true, false, true)
    }

    fn saved(account: &'a Value, protected: bool, test: bool, relay: bool) -> Option<Self> {
        let details = &account["providerSpecificData"];
        if account["provider"] != "kiro"
            || account["authType"] != "oauth"
            || truthy(&account["apiKey"])
            || !relay && truthy(&details["vercelRelayUrl"])
            || !test && !protected && proxy_transport::account_requires_tunnel(account)
        {
            return None;
        }
        let method = OAuthMethod::saved(details)?;
        if protected
            && (method == OAuthMethod::Unspecified
                || test
                    && ["region", "clientId", "clientSecret"]
                        .into_iter()
                        .any(|field| !truthy(&details[field]) && truthy(&account[field]))
                || method == OAuthMethod::Social
                    && (truthy(&details["clientId"]) || truthy(&details["clientSecret"])))
        {
            return None;
        }
        let profile = &details["profileArn"];
        let saved_region = |key| {
            let value = &details[key];
            if test && !truthy(value) {
                &account[key]
            } else {
                value
            }
        };
        let region = saved_region("region");
        let region = if region.is_null() || region == "" {
            "us-east-1"
        } else {
            field(region)?
        };
        if !profile_missing(profile) && field(profile).is_none() {
            return None;
        }
        let runtime_region =
            crate::kiro_models::oauth_runtime_region(region, method, details, field(profile))?;
        let token = field(&account["refreshToken"])?;
        let url_and_wire = if method == OAuthMethod::ExternalIdp {
            let endpoint =
                field(&details["tokenEndpoint"]).or_else(|| field(&details["token_endpoint"]))?;
            let client = field(&details["clientId"]).or_else(|| field(&details["client_id"]))?;
            let scopes = if truthy(&details["scope"]) {
                &details["scope"]
            } else {
                &details["scopes"]
            };
            let scope = match scopes {
                Value::String(_) => field(scopes)?.to_owned(),
                Value::Array(names) => names.iter().filter_map(field).collect::<Vec<_>>().join(" "),
                _ => return None,
            };
            if scope.is_empty() {
                return None;
            }
            Some((
                token_endpoint(endpoint).ok()?,
                Wire::Microsoft { client, scope },
            ))
        } else if let Some((client, secret)) =
            field(saved_region("clientId")).zip(field(saved_region("clientSecret")))
        {
            let issuer_region = if test || method == OAuthMethod::IdentityCenter {
                region
            } else {
                "us-east-1"
            };
            Some((
                Url::parse(&format!("https://oidc.{issuer_region}.amazonaws.com/token")).ok()?,
                Wire::Aws { client, secret },
            ))
        } else if test
            || matches!(method, OAuthMethod::Imported | OAuthMethod::Unspecified)
            || method == OAuthMethod::Social
                && !truthy(&details["clientId"])
                && !truthy(&details["clientSecret"])
            || method == OAuthMethod::Builder
                && ["clientId", "clientSecret"].into_iter().all(|key| {
                    [&details[key], &account[key]]
                        .into_iter()
                        .all(|value| value.is_null() || value == "")
                })
        {
            Some((Url::parse(SOCIAL).ok()?, Wire::Social))
        } else {
            None
        };
        let (url, wire) = url_and_wire?;
        let profile_lookup_region = (profile_missing(profile)
            && !matches!(wire, Wire::Microsoft { .. }))
        .then_some(runtime_region);
        Some(Self {
            token,
            url,
            wire,
            profile_lookup_region,
            test,
        })
    }

    pub async fn request(
        &self,
        storage: &Storage,
        account: &Value,
        protected: bool,
        relay: Option<[u8; 32]>,
    ) -> Result<(Client, Request), RefreshError> {
        let request = if let Some(identity) = relay {
            let client = CLIENT
                .as_ref()
                .map_err(|_| RefreshError::ProviderUnavailable)?;
            provider_http::bind_relay(
                storage,
                account,
                self.url.as_str(),
                client,
                TIMEOUT,
                identity,
            )
            .await
            .ok_or(RefreshError::Unsupported)?
            .request(Method::POST)
        } else {
            let (client, fixture) =
                provider_client(storage, account, self.url.as_str(), protected).await?;
            let url = if fixture {
                let host = self.url.host_str().ok_or(RefreshError::Unsupported)?;
                let mut url = Url::parse(&format!(
                    "http://127.0.0.1:52989/kiro-refresh/{host}{}",
                    self.url.path()
                ))
                .map_err(|_| RefreshError::Unsupported)?;
                url.set_query(self.url.query());
                url
            } else {
                self.url.clone()
            };
            client.post(url)
        };
        let request = request.header(header::ACCEPT, "application/json");
        let request = match &self.wire {
            Wire::Aws {client, secret} => request.json(&json!({
                "clientId":client,"clientSecret":secret,"refreshToken":self.token,"grantType":"refresh_token",
            })),
            Wire::Social => request.header(header::USER_AGENT, "9Router")
                .json(&json!({"refreshToken":self.token})),
            Wire::Microsoft {client, scope} => request.form(&[
                ("grant_type", "refresh_token"),("client_id", *client),("refresh_token", self.token),("scope", scope.as_str()),
            ]),
        };
        let (client, request) = request.build_split();
        Ok((client, request.map_err(|_| RefreshError::Unsupported)?))
    }

    pub async fn execute(&self, (client, request): (Client, Request)) -> Outcome {
        let Ok(response) = client.execute(request).await else {
            return Outcome::Uncertain;
        };
        let status = response.status();
        let Some(data) = bounded_json(response).await else {
            return if status.is_client_error() && status.as_u16() == 429 {
                Outcome::Throttled
            } else {
                Outcome::Uncertain
            };
        };
        if !status.is_success() {
            return if status.as_u16() == 429
                || data["error"] == "temporarily_unavailable" && status.is_client_error()
            {
                Outcome::Throttled
            } else {
                Outcome::Uncertain
            };
        }
        let (access, rotated, ttl) = match self.wire {
            Wire::Microsoft { .. } => (
                &data["access_token"],
                &data["refresh_token"],
                &data["expires_in"],
            ),
            _ => (
                &data["accessToken"],
                &data["refreshToken"],
                &data["expiresIn"],
            ),
        };
        let default_ttl = json!(3600);
        let ttl = if self.test && !matches!(self.wire, Wire::Microsoft { .. }) && !truthy(ttl) {
            &default_ttl
        } else {
            ttl
        };
        let Some(access) =
            field(access).filter(|token| token.bytes().all(|byte| byte.is_ascii_graphic()))
        else {
            return Outcome::Uncertain;
        };
        if ttl
            .as_i64()
            .or_else(|| ttl.as_str().and_then(|value| value.parse::<i64>().ok()))
            .is_none_or(|seconds| seconds <= 0)
        {
            return Outcome::Uncertain;
        }
        let now = Utc::now();
        let Ok(expires) = expiry(now, Some(ttl)) else {
            return Outcome::Uncertain;
        };
        if expires.is_null() {
            return Outcome::Uncertain;
        }
        let mut patch = Map::from_iter([
            ("accessToken".into(), json!(access)),
            ("expiresAt".into(), expires),
            ("expiresIn".into(), ttl.clone()),
            (
                "lastRefreshAt".into(),
                json!(now.to_rfc3339_opts(SecondsFormat::Millis, true)),
            ),
            (ATTEMPT.into(), Value::Null),
        ]);
        if let Some(rotated) = field(rotated) {
            patch.insert("refreshToken".into(), json!(rotated));
        }
        if let Some(region) = self.profile_lookup_region
            && let Some(arn) = field(&data["profileArn"])
                .filter(|arn| crate::kiro_models::profile_arn_region(arn) == Some(region))
        {
            patch.insert("providerSpecificData".into(), json!({"profileArn":arn}));
        }
        Outcome::Complete(patch)
    }

    pub async fn discover_profile(
        &self,
        storage: &Storage,
        account: &Value,
        protected: bool,
        relay: Option<[u8; 32]>,
    ) -> Option<Map<String, Value>> {
        let region = self.profile_lookup_region?;
        if !profile_missing(&account["providerSpecificData"]["profileArn"]) {
            return None;
        }
        let access = field(&account["accessToken"])?;
        let endpoint = crate::kiro_device::PROFILES;
        let request = if let Some(identity) = relay {
            provider_http::bind_relay(
                storage,
                account,
                endpoint,
                CLIENT.as_ref().ok()?,
                TIMEOUT,
                identity,
            )
            .await?
            .request(Method::POST)
        } else {
            let (client, fixture) = provider_client(storage, account, endpoint, protected)
                .await
                .ok()?;
            let url = if fixture {
                "http://127.0.0.1:52989/kiro-refresh/codewhisperer.us-east-1.amazonaws.com/ListAvailableProfiles"
            } else {
                endpoint
            };
            client.post(url)
        };
        let response = request
            .header(header::ACCEPT, "application/json")
            .bearer_auth(access)
            .json(&json!({"maxResults":10}))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let data = bounded_json(response).await?;
        let arn = data["profiles"]
            .as_array()?
            .iter()
            .filter_map(|profile| field(&profile["arn"]))
            .find(|arn| crate::kiro_models::profile_arn_region(arn) == Some(region))?;
        Some(Map::from_iter([(
            "providerSpecificData".into(),
            json!({"profileArn":arn}),
        )]))
    }
}

pub(super) enum Outcome {
    Complete(Map<String, Value>),
    Throttled,
    Uncertain,
}

fn field(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(js_trim)
        .filter(|value| !value.is_empty())
}

fn profile_missing(value: &Value) -> bool {
    value.is_null()
        || value
            .as_str()
            .is_some_and(|value| js_trim(value).is_empty())
}

async fn provider_client(
    storage: &Storage,
    account: &Value,
    endpoint: &str,
    protected: bool,
) -> Result<(Client, bool), RefreshError> {
    let settings = storage.read_settings().await.ok_or(RefreshError::Storage)?;
    let delivery = Delivery::select(storage, &settings, account, endpoint)
        .await
        .map_err(|_| RefreshError::Unsupported)?;
    if delivery.protected() != protected {
        return Err(RefreshError::Unsupported);
    }
    let fixture = delivery.use_fixture();
    let client = match delivery {
        Delivery::Tunnel(proxy) if proxy.scheme() != "socks5" => {
            proxy_transport::vendor_metadata_client(proxy, client_builder())
                .ok_or(RefreshError::ProviderUnavailable)?
        }
        Delivery::Direct | Delivery::Bypass => CLIENT
            .as_ref()
            .cloned()
            .map_err(|_| RefreshError::ProviderUnavailable)?,
        _ => return Err(RefreshError::Unsupported),
    };
    Ok((client, fixture))
}

async fn bounded_json(mut response: Response) -> Option<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > RESPONSE_LIMIT - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}
