use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use reqwest::{Client, ClientBuilder, RequestBuilder, StatusCode, Url, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::provider_connections::truthy;
use crate::provider_http;
use crate::provider_index::js_trim;
use crate::provider_oauth::{RefreshMode, refresh_account};
use crate::proxy_transport::{self, Route, Selection};
use crate::storage::Storage;

pub(super) const DEFAULT_REGION: &str = "us-east-1";
const TIMEOUT: Duration = Duration::from_secs(30);
const TTL: Duration = Duration::from_secs(300);
const MAX_ACCOUNTS: usize = 256;
const PROFILE_ENDPOINT: &str = "https://codewhisperer.us-east-1.amazonaws.com";
const PROFILE_DEFAULTS: &str = include_str!("../../../open-sse/config/kiroConstants.js");

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OAuthMethod {
    Builder,
    IdentityCenter,
    Social,
    ExternalIdp,
    Imported,
    Unspecified,
}

impl OAuthMethod {
    pub(super) fn saved(details: &Value) -> Option<Self> {
        if !(details.is_object() || details.is_null()) {
            return None;
        }
        match details.get("authMethod") {
            None | Some(Value::Null) => Some(Self::Unspecified),
            Some(Value::String(method)) => match js_trim(method) {
                "builder-id" => Some(Self::Builder),
                "idc" => Some(Self::IdentityCenter),
                "google" | "github" => Some(Self::Social),
                "external_idp" => Some(Self::ExternalIdp),
                "import" | "imported" => Some(Self::Imported),
                "" => Some(Self::Unspecified),
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn default_profile(self) -> Option<&'static str> {
        let field = match self {
            Self::IdentityCenter | Self::ExternalIdp => return None,
            Self::Social => "social:",
            _ => "\"builder-id\":",
        };
        default_profiles()?
            .lines()
            .find_map(|line| line.trim().strip_prefix(field))?
            .trim()
            .strip_prefix('"')?
            .split_once('"')
            .map(|(arn, _)| arn)
    }
}

fn default_profiles() -> Option<&'static str> {
    PROFILE_DEFAULTS
        .split_once("export const KIRO_DEFAULT_PROFILE_ARNS")?
        .1
        .split_once("};")
        .map(|(declarations, _)| declarations)
}

static RESOLVER: LazyLock<Option<Resolver>> = LazyLock::new(|| {
    let fixture = (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"))
        .then(|| "http://127.0.0.1:52989/kiro".into());
    Resolver::new(fixture).ok()
});

static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let package: Value =
        serde_json::from_str(include_str!("../../../package.json")).unwrap_or_default();
    format!("9Router/{}", package["version"].as_str().unwrap_or("0.0.0"))
});

pub(super) fn user_agent() -> &'static str {
    USER_AGENT.as_str()
}

type CacheKey = [u8; 32];
type Slot = Arc<AsyncMutex<Option<Cached>>>;
type Cache = VecDeque<(CacheKey, Slot)>;

struct Cached {
    until: Instant,
    models: Vec<Value>,
}

pub(super) struct Resolver {
    fixture: Option<String>,
    client: Client,
    entries: Mutex<Cache>,
}

enum Target<'a> {
    ApiKey {
        key: &'a str,
        region: &'a str,
    },
    Profile {
        token: &'a str,
        arn: Option<&'a str>,
        region: &'a str,
        codewhisperer: bool,
    },
}

pub(crate) enum Discovery {
    Models(Vec<Value>),
    Unavailable,
}

enum FetchError {
    Unauthorized,
    Unavailable,
}

enum CatalogRoute {
    Direct,
    Tunnel(Url),
    Relay(Route, [u8; 32]),
}

impl CatalogRoute {
    fn protected(&self) -> bool {
        !matches!(self, Self::Direct)
    }

    fn same_egress(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Direct, Self::Direct) => true,
            (Self::Tunnel(left), Self::Tunnel(right)) => left == right,
            (Self::Relay(_, left), Self::Relay(_, right)) => left == right,
            _ => false,
        }
    }
}

async fn catalog_route(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    endpoint: &str,
) -> Option<CatalogRoute> {
    let route = provider_http::select(storage, settings, account, endpoint)
        .await
        .ok()?;
    if let Selection::Routed(route @ Route::Relay { .. }) = route {
        let identity = route.relay_identity()?;
        return Some(CatalogRoute::Relay(route, identity));
    }
    match proxy_transport::vendor_metadata_proxy(storage, settings, account, endpoint)
        .await
        .ok()?
    {
        Some(proxy) if proxy.scheme() != "socks5" => Some(CatalogRoute::Tunnel(proxy)),
        Some(_) => None,
        None => Some(CatalogRoute::Direct),
    }
}

pub(super) fn api_key_target(connection: &Value) -> Option<(&str, &str)> {
    if truthy(&connection["refreshToken"]) {
        return None;
    }
    match credential_target(connection)? {
        Target::ApiKey { key, region } => Some((key, region)),
        Target::Profile { .. } => None,
    }
}

pub(crate) async fn discover(storage: &Storage, settings: &Value, account: &Value) -> Discovery {
    let native = credential_target(account).filter(|target| {
        !matches!(target, Target::ApiKey { .. }) || !truthy(&account["refreshToken"])
    });
    let compatible = native.is_none();
    let Some(target) = native.or_else(|| saved_catalog_target(account)) else {
        return Discovery::Unavailable;
    };
    let endpoint = target.endpoint();
    let Some(route) = catalog_route(storage, settings, account, &endpoint).await else {
        return Discovery::Unavailable;
    };
    if compatible && route.protected() {
        return Discovery::Unavailable;
    }
    let Some(resolver) = RESOLVER.as_ref() else {
        return Discovery::Unavailable;
    };
    let mut refreshed = None;
    let models = match target {
        Target::ApiKey { key, .. } => {
            if route.protected() && nonempty(&account["apiKey"]).is_some_and(|saved| saved != key) {
                return Discovery::Unavailable;
            }
            resolver.discover_target(target, &route).await
        }
        Target::Profile { arn, .. } => {
            let expired =
                crate::kiro_chat::oauth_remaining_ms(account).is_some_and(|time| time <= 0.0);
            let models = if expired {
                Err(FetchError::Unauthorized)
            } else if route.protected() && !profile_token_for_proxy(account, arn) {
                Err(FetchError::Unavailable)
            } else {
                resolver.discover_target(target, &route).await
            };
            if matches!(models, Err(FetchError::Unauthorized))
                && nonempty(&account["refreshToken"]).is_some()
            {
                let mode = match &route {
                    CatalogRoute::Relay(_, identity) => RefreshMode::RelayRouted(*identity),
                    CatalogRoute::Tunnel(_) => RefreshMode::KiroProtected,
                    CatalogRoute::Direct => RefreshMode::Rejected,
                };
                let Ok(current) = refresh_account(storage, account, mode).await else {
                    return Discovery::Unavailable;
                };
                let Some(settings) = storage.read_settings().await else {
                    return Discovery::Unavailable;
                };
                let Some(retry @ Target::Profile { arn: retry_arn, .. }) =
                    credential_target(&current).or_else(|| {
                        (!route.protected())
                            .then(|| saved_catalog_target(&current))
                            .flatten()
                    })
                else {
                    return Discovery::Unavailable;
                };
                let retry_endpoint = retry.endpoint();
                if arn.is_some() && retry_endpoint != endpoint
                    || crate::kiro_chat::oauth_remaining_ms(&current)
                        .is_some_and(|time| time <= 0.0)
                    || route.protected() && !profile_token_for_proxy(&current, retry_arn)
                {
                    return Discovery::Unavailable;
                }
                let Some(after) = catalog_route(storage, &settings, &current, &retry_endpoint)
                    .await
                    .filter(|after| route.same_egress(after))
                else {
                    return Discovery::Unavailable;
                };
                let result = resolver.discover_target(retry, &after).await;
                refreshed = Some(current);
                result
            } else {
                models
            }
        }
    };
    let Ok(mut models) = models else {
        return Discovery::Unavailable;
    };
    let secrets = std::iter::once(account)
        .chain(refreshed.as_ref())
        .flat_map(|account| {
            ["accessToken", "apiKey", "refreshToken", "idToken"]
                .map(|field| &account[field])
                .into_iter()
                .chain(std::iter::once(
                    &account["providerSpecificData"]["clientSecret"],
                ))
        })
        .filter_map(nonempty)
        .chain(match &route {
            CatalogRoute::Relay(route, _) => route.relay_secret(),
            _ => None,
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    models.retain(|model| {
        model["id"]
            .as_str()
            .is_some_and(|id| !secrets.iter().any(|secret| id.contains(secret)))
    });
    for model in &mut models {
        crate::provider_tools::scrub_payload(model, &secrets);
    }
    if models.is_empty() {
        Discovery::Unavailable
    } else {
        Discovery::Models(models)
    }
}

fn saved_catalog_target(connection: &Value) -> Option<Target<'_>> {
    if connection["provider"] != "kiro" {
        return None;
    }
    let token = nonempty(&connection["accessToken"]).filter(|token| valid_credential(token))?;
    let arn = nonempty(&connection["providerSpecificData"]["profileArn"]);
    let region = match arn {
        Some(arn) => profile_arn_region(arn)?,
        None => DEFAULT_REGION,
    };
    Some(Target::Profile {
        token,
        arn,
        region,
        codewhisperer: false,
    })
}

fn profile_token_for_proxy(account: &Value, arn: Option<&str>) -> bool {
    if arn.is_some() {
        crate::kiro_chat::Connection::for_proxy(account).is_some()
    } else {
        OAuthMethod::saved(&account["providerSpecificData"])
            .is_some_and(|method| method != OAuthMethod::Unspecified)
            && !truthy(&account["apiKey"])
            && crate::kiro_chat::oauth_remaining_ms(account).is_some_and(|time| time > 0.0)
    }
}

impl Resolver {
    pub(super) fn new(fixture: Option<String>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            fixture,
            client: client()?,
            entries: Mutex::default(),
        })
    }

    async fn discover_target(
        &self,
        target: Target<'_>,
        route: &CatalogRoute,
    ) -> Result<Vec<Value>, FetchError> {
        let slot = self
            .slot(target.cache_key(route))
            .ok_or(FetchError::Unavailable)?;
        let mut cache = slot.lock().await;
        let now = Instant::now();
        if let Some(hit) = cache.as_ref().filter(|entry| entry.until > now) {
            return Ok(hit.models.clone());
        }
        let data = match route {
            CatalogRoute::Tunnel(proxy) => {
                let client =
                    proxy_transport::vendor_metadata_client(proxy.clone(), client_builder())
                        .ok_or(FetchError::Unavailable)?;
                let endpoint = target.endpoint();
                fetch(request(&client, target, endpoint)).await?
            }
            CatalogRoute::Relay(route, _) => {
                let endpoint = target.endpoint();
                let (client, request) = request(&self.client, target, endpoint)
                    .timeout(TIMEOUT)
                    .build_split();
                let request = request.map_err(|_| FetchError::Unavailable)?;
                let relay = route
                    .relay_target(request.url())
                    .ok_or(FetchError::Unavailable)?;
                fetch(
                    provider_http::relay_request(client, request, &relay)
                        .ok_or(FetchError::Unavailable)?,
                )
                .await?
            }
            CatalogRoute::Direct => {
                fetch_catalog(&self.client, target, self.fixture.as_deref()).await?
            }
        };
        let models = expand(data["models"].as_array().ok_or(FetchError::Unavailable)?);
        *cache = Some(Cached {
            until: now + TTL,
            models: models.clone(),
        });
        Ok(models)
    }

    fn slot(&self, key: CacheKey) -> Option<Slot> {
        let mut entries = self.entries.lock().ok()?;
        let index = entries.iter().position(|(other, _)| *other == key);
        let slot = index
            .and_then(|index| entries.remove(index))
            .map(|(_, slot)| slot)
            .unwrap_or_else(|| Arc::new(AsyncMutex::new(None)));
        entries.push_front((key, slot.clone()));
        entries.truncate(MAX_ACCOUNTS);
        Some(slot)
    }
}

pub(super) fn client() -> Result<Client, reqwest::Error> {
    client_builder().build()
}

pub(super) fn client_builder() -> ClientBuilder {
    Client::builder()
        .timeout(TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .user_agent(user_agent())
}

pub(super) async fn validate_api_key(
    client: &Client,
    key: &str,
    region: &str,
    fixture: Option<&str>,
) -> bool {
    valid_credential(key)
        && valid_region(region)
        && fetch_catalog(client, Target::ApiKey { key, region }, fixture)
            .await
            .is_ok_and(|data| {
                data["models"]
                    .as_array()
                    .is_some_and(|models| !models.is_empty())
            })
}

async fn fetch_catalog(
    client: &Client,
    target: Target<'_>,
    fixture: Option<&str>,
) -> Result<Value, FetchError> {
    let endpoint = match (fixture, &target) {
        (
            Some(local),
            Target::ApiKey { region, .. }
            | Target::Profile {
                region,
                codewhisperer: false,
                ..
            },
        ) => {
            format!("{local}/q/{region}/ListAvailableModels")
        }
        (Some(local), Target::Profile { .. }) => format!("{local}/codewhisperer/us-east-1"),
        (None, _) => target.endpoint(),
    };
    fetch(request(client, target, endpoint)).await
}

async fn fetch(request: RequestBuilder) -> Result<Value, FetchError> {
    let response = request.send().await.map_err(|_| FetchError::Unavailable)?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(FetchError::Unauthorized);
    }
    if !response.status().is_success() {
        return Err(FetchError::Unavailable);
    }
    response.json().await.map_err(|_| FetchError::Unavailable)
}

fn request(client: &Client, target: Target<'_>, endpoint: String) -> RequestBuilder {
    let request = match target {
        Target::ApiKey { key, .. } => client
            .get(endpoint)
            .query(&[("origin", "AI_EDITOR")])
            .bearer_auth(key)
            .header("TokenType", "API_KEY"),
        Target::Profile {
            token,
            arn,
            codewhisperer: false,
            ..
        } => {
            let request = client
                .get(endpoint)
                .query(&[("origin", "AI_EDITOR")])
                .bearer_auth(token);
            match arn {
                Some(arn) => request.query(&[("profileArn", arn)]),
                None => request,
            }
        }
        Target::Profile {
            token,
            arn,
            codewhisperer: true,
            ..
        } => client
            .post(endpoint)
            .header(header::CONTENT_TYPE, "application/x-amz-json-1.0")
            .header(
                "x-amz-target",
                "AmazonCodeWhispererService.ListAvailableModels",
            )
            .bearer_auth(token)
            .json(&json!({"origin":"AI_EDITOR","profileArn":arn})),
    };
    request.header(header::ACCEPT, "application/json")
}

impl Target<'_> {
    fn endpoint(&self) -> String {
        match self {
            Self::ApiKey { region, .. }
            | Self::Profile {
                region,
                codewhisperer: false,
                ..
            } => {
                format!("https://q.{region}.amazonaws.com/ListAvailableModels")
            }
            Self::Profile {
                codewhisperer: true,
                ..
            } => PROFILE_ENDPOINT.to_owned(),
        }
    }

    fn cache_key(&self, route: &CatalogRoute) -> CacheKey {
        let mut hash = Sha256::new();
        let (kind, scope, access): (&str, &str, &str) = match self {
            Self::ApiKey { key, region } => ("key", region, key),
            Self::Profile {
                token,
                arn,
                codewhisperer: true,
                ..
            } => ("codewhisperer", arn.unwrap_or_default(), token),
            Self::Profile {
                token,
                arn,
                codewhisperer: false,
                ..
            } => ("q-profile", arn.unwrap_or_default(), token),
        };
        hash.update(kind);
        hash.update([0]);
        hash.update(scope);
        hash.update([0]);
        hash.update(access);
        match route {
            CatalogRoute::Tunnel(proxy) => {
                hash.update([0]);
                hash.update(proxy.as_str());
            }
            CatalogRoute::Relay(_, identity) => {
                hash.update([1]);
                hash.update(identity);
            }
            CatalogRoute::Direct => (),
        }
        hash.finalize().into()
    }
}

fn credential_target(connection: &Value) -> Option<Target<'_>> {
    if connection["provider"] != "kiro" {
        return None;
    }
    let details = &connection["providerSpecificData"];
    match connection["authType"].as_str() {
        Some("api_key" | "apikey")
            if details["authMethod"] == "api_key" && !truthy(&details["profileArn"]) =>
        {
            let key =
                nonempty(&connection["accessToken"]).or_else(|| nonempty(&connection["apiKey"]))?;
            let region = region(&details["region"])?;
            valid_credential(key).then_some(Target::ApiKey { key, region })
        }
        Some("oauth") => {
            let method = OAuthMethod::saved(details)?;
            let token = nonempty(&connection["accessToken"])?;
            let arn = match &details["profileArn"] {
                Value::Null => None,
                Value::String(value) => Some(js_trim(value)).filter(|value| !value.is_empty()),
                _ => return None,
            };
            let region = match arn {
                Some(arn) => profile_arn_region(arn)?,
                None => DEFAULT_REGION,
            };
            valid_credential(token).then_some(Target::Profile {
                token,
                arn,
                region,
                codewhisperer: arn.is_some()
                    && region == DEFAULT_REGION
                    && matches!(method, OAuthMethod::Builder | OAuthMethod::IdentityCenter),
            })
        }
        _ => None,
    }
}

fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(js_trim)
        .filter(|value| !value.is_empty())
}

fn valid_credential(value: &str) -> bool {
    value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn region(value: &Value) -> Option<&str> {
    let value = match value {
        Value::Null => DEFAULT_REGION,
        Value::String(value) if js_trim(value).is_empty() => DEFAULT_REGION,
        Value::String(value) => js_trim(value),
        _ => return None,
    };
    valid_region(value).then_some(value)
}

pub(super) fn valid_region(region: &str) -> bool {
    let mut parts = region.split('-');
    let (Some(partition), Some(area), Some(sequence), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    partition.len() == 2
        && partition.bytes().all(|value| value.is_ascii_lowercase())
        && !area.is_empty()
        && area.bytes().all(|value| value.is_ascii_lowercase())
        && matches!(sequence.len(), 1 | 2)
        && sequence.bytes().all(|value| value.is_ascii_digit())
}

pub(super) fn oauth_runtime_region<'a>(
    issuer_region: &'a str,
    method: OAuthMethod,
    details: &Value,
    profile: Option<&str>,
) -> Option<&'a str> {
    let runtime = if matches!(issuer_region, "us-east-1" | "eu-central-1") {
        issuer_region
    } else if method == OAuthMethod::IdentityCenter
        && valid_region(issuer_region)
        && profile.and_then(profile_arn_region) == Some(DEFAULT_REGION)
        && ["clientId", "clientSecret"]
            .into_iter()
            .all(|key| nonempty(&details[key]).is_some_and(valid_credential))
    {
        DEFAULT_REGION
    } else {
        return None;
    };
    profile
        .is_none_or(|arn| crate::kiro_chat::profile_for_region(arn, runtime))
        .then_some(runtime)
}

pub(super) fn profile_arn_region(arn: &str) -> Option<&str> {
    let (region, _) = arn
        .strip_prefix("arn:aws:codewhisperer:")?
        .split_once(':')?;
    if !matches!(region, "us-east-1" | "eu-central-1")
        || !crate::kiro_chat::profile_for_region(arn, region)
    {
        return None;
    }
    let shared = default_profiles().is_some_and(|declarations| declarations.contains(arn));
    (!shared).then_some(region)
}

fn expand(raw: &[Value]) -> Vec<Value> {
    let mut models = Vec::new();
    for model in raw {
        let Some(id) = model["modelId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .or_else(|| model["id"].as_str().filter(|id| !id.is_empty()))
        else {
            continue;
        };
        let base = id.strip_suffix("-agentic").unwrap_or(id);
        let base = base.strip_suffix("-thinking").unwrap_or(base);
        let model_name = model["modelName"]
            .as_str()
            .filter(|name| !name.is_empty())
            .unwrap_or(id);
        let rate = model.get("rateMultiplier").and_then(numeric).unwrap_or(1.0);
        let name = display_name(js_trim(model_name), rate);
        let context = numeric(&model["tokenLimits"]["maxInputTokens"])
            .filter(|value| *value != 0.0)
            .unwrap_or(200_000.0);
        let description = if truthy(&model["description"]) {
            model["description"].clone()
        } else {
            json!("")
        };
        let variants = [
            ("", "", false, false),
            ("-thinking", " (Thinking)", true, false),
            ("-agentic", " (Agentic)", false, true),
            ("-thinking-agentic", " (Thinking + Agentic)", true, true),
        ];
        for (id_suffix, label, thinking, agentic) in variants {
            if base == "auto" && agentic {
                break;
            }
            models.push(json!({
                "id":format!("{base}{id_suffix}"),"name":format!("{name}{label}"),
                "capabilities":{"thinking":thinking,"agentic":agentic},
                "contextLength":json_number(context),"rateMultiplier":json_number(rate),
                "upstreamModelId":id,"description":description,
            }));
        }
    }
    models
}

fn display_name(name: &str, rate: f64) -> String {
    if rate <= 0.0 || (rate - 1.0).abs() < 1e-9 {
        return format!("Kiro {name}");
    }
    let credits = if rate.fract() == 0.25 {
        format!("{:.0}.3", rate.floor())
    } else {
        format!("{rate:.1}")
    };
    format!("Kiro {name} ({credits}x credit)")
}

fn numeric(value: &Value) -> Option<f64> {
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
