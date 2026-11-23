use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, header};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::cli_video::redact_bearer;
use crate::provider_connections::truthy;
use crate::proxy_transport::{self, Route, Selection};
use crate::storage::Storage;

pub(super) const ORIGIN: &str = "https://aiplatform.googleapis.com";
pub(super) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const FIXTURE_ORIGIN: &str = "http://127.0.0.1:52989/vertex";
const FIXTURE_TOKEN_URL: &str = "http://127.0.0.1:52989/vertex-oauth/token";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const TOKEN_REFRESH_BUFFER: Duration = Duration::from_secs(300);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const CACHE_LIMIT: usize = 128;

type CredentialHash = [u8; 32];

static PROJECTS: LazyLock<Mutex<HashMap<CredentialHash, String>>> = LazyLock::new(Mutex::default);
static TOKENS: LazyLock<Mutex<HashMap<(CredentialHash, bool), CachedToken>>> =
    LazyLock::new(Mutex::default);

#[derive(Clone)]
struct CachedToken {
    header: HeaderValue,
    until: Option<Instant>,
    expires_at: Option<i64>,
}

pub struct Exchange {
    url: Url,
    bearer: Option<HeaderValue>,
    stream: bool,
    service_expiry: Option<i64>,
}

pub fn supports(provider: &str, account: &Value, model: &str) -> bool {
    draft(provider, account, model).is_ok()
}

pub(crate) fn preview_target(
    provider: &str,
    model: &str,
    stream: bool,
    account: &Value,
) -> Result<(Url, bool), &'static str> {
    let fields = credential_draft(provider, account, model)?;
    let raw = matches!(fields.credential, Credential::ApiKey(_));
    let mut url = destination(&fields, model, stream, !raw)?;
    if ["apiKey", "accessToken", "idToken", "refreshToken"]
        .into_iter()
        .filter_map(|field| nonempty(&account[field]))
        .any(|secret| url.path().contains(secret))
    {
        return Err("Vertex preview destination contains a credential");
    }
    if raw {
        url.query_pairs_mut().append_pair("key", "[redacted]");
    }
    Ok((url, nonempty(&account["accessToken"]).is_some()))
}

pub(crate) fn requires_tunnel(settings: &Value, account: &Value) -> bool {
    truthy(&settings["outboundProxyEnabled"])
        || proxy_transport::account_requires_tunnel(account)
        || !is_fixture()
            && ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
                .iter()
                .any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
}

pub(crate) struct Protected {
    exchange: Exchange,
    delivery: Delivery,
}

enum Delivery {
    Tunnel(Url),
    Bypass,
    Relay { route: Route, key: HeaderValue },
}

impl Protected {
    pub(crate) async fn select(
        storage: &Storage,
        settings: &Value,
        provider: &str,
        model: &str,
        stream: bool,
        account: &Value,
    ) -> Result<Self, &'static str> {
        if truthy(&account["providerSpecificData"]["vercelRelayUrl"]) {
            return Err("Vertex requires a configured tunnel or eligible managed Relay");
        }
        let fields = draft(provider, account, model)?;
        let bearer = match &fields.credential {
            Credential::ApiKey(_) => None,
            Credential::Bearer(token) => {
                if bearer_expired(account) {
                    return Err("Vertex requires a current stored access token for this proxy");
                }
                Some(bearer_header(token)?)
            }
            _ => {
                return Err(
                    "Vertex proxy requires a stored API key or non-refreshing access token",
                );
            }
        };
        if fields.partner && fields.project.is_none() {
            return Err("Vertex Partner proxy requires an explicitly configured project");
        }
        let mut destination = destination(&fields, model, stream, bearer.is_some())?;
        let selected =
            proxy_transport::select_with_bypass(storage, settings, account, destination.as_str())
                .await
                .map_err(|_| "Vertex connection proxy is unavailable")?;
        let delivery = match selected {
            Selection::ExplicitBypass => Delivery::Bypass,
            Selection::Routed(Route::Proxy { url, .. }) => Delivery::Tunnel(
                proxy_transport::tunnel_proxy_url(&url).ok_or("Unsupported Vertex proxy URL")?,
            ),
            Selection::Routed(
                route @ Route::Relay {
                    authorization: Some(_),
                    ..
                },
            ) => {
                let Credential::ApiKey(key) = &fields.credential else {
                    return Err(
                        "Vertex managed Relay requires a projectless Gemini API key connection",
                    );
                };
                if fields.partner || fields.project.is_some() {
                    return Err(
                        "Vertex managed Relay requires a projectless Gemini API key connection",
                    );
                }
                let Route::Relay {
                    path,
                    authorization,
                    ..
                } = &route
                else {
                    unreachable!()
                };
                let path = percent_encoding::percent_decode_str(path).decode_utf8_lossy();
                if path.contains(key)
                    || authorization
                        .as_ref()
                        .and_then(|secret| secret.to_str().ok())
                        .is_some_and(|secret| path.contains(secret))
                {
                    return Err("Vertex Relay destination contains a credential");
                }
                let mut key = HeaderValue::from_str(key).map_err(|_| "Invalid Vertex API key")?;
                key.set_sensitive(true);
                Delivery::Relay { route, key }
            }
            _ => return Err("Vertex connection proxy is unavailable"),
        };
        if let Credential::ApiKey(key) = fields.credential
            && !matches!(delivery, Delivery::Relay { .. })
        {
            destination.query_pairs_mut().append_pair("key", key);
        }
        let exchange = Exchange {
            url: destination,
            bearer,
            stream,
            service_expiry: None,
        };
        Ok(Self { exchange, delivery })
    }

    pub(crate) async fn send(
        &self,
        body: &Value,
        mut headers: HeaderMap,
        timeout: Duration,
    ) -> Result<reqwest::Response, &'static str> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .connect_timeout(timeout)
            .redirect(reqwest::redirect::Policy::none());
        let (client, url) = match &self.delivery {
            Delivery::Tunnel(proxy) => (
                proxy_transport::vendor_metadata_client(proxy.clone(), client),
                self.exchange.url().to_owned(),
            ),
            Delivery::Bypass => (
                client.build().ok(),
                self.exchange
                    .fixture_url()
                    .unwrap_or_else(|| self.exchange.url().to_owned()),
            ),
            Delivery::Relay {
                route: Route::Relay { url, .. },
                ..
            } => (client.build().ok(), url.clone()),
            Delivery::Relay { .. } => return Err("Vertex managed Relay is unavailable"),
        };
        let client = client.ok_or("Vertex connection proxy is unavailable")?;
        for name in [
            "proxy-authorization",
            "x-relay-auth",
            "x-goog-api-key",
            "x-api-key",
            "accept",
        ] {
            headers.remove(name);
        }
        let mut request = self
            .exchange
            .headers(client.post(url).json(body).headers(headers));
        if let Delivery::Relay { route, key } = &self.delivery {
            request = route.relay(request.header("x-goog-api-key", key.clone()));
        }
        let response = tokio::time::timeout(timeout, request.send())
            .await
            .map_err(|_| "Vertex proxy connection timed out")?
            .map_err(|_| "Vertex proxy request failed")?;
        if response.status().is_redirection() {
            return Err("Vertex proxy redirect refused");
        }
        Ok(response)
    }

    pub(crate) fn redact_error(&self, text: &str) -> String {
        let mut redacted = self.exchange.redact_error(text);
        if let Delivery::Relay {
            route:
                Route::Relay {
                    authorization: Some(secret),
                    ..
                },
            key,
        } = &self.delivery
        {
            if let Ok(key) = key.to_str() {
                redacted = redact_key(&redacted, key);
            }
            if let Ok(secret) = secret.to_str() {
                redacted = redacted.replace(secret, "[redacted]");
            }
        }
        redacted
    }

    pub(crate) fn secrets(&self, secrets: &mut Vec<Vec<u8>>) {
        self.exchange.secrets(secrets);
        if let Delivery::Relay {
            route:
                Route::Relay {
                    authorization: Some(secret),
                    ..
                },
            key,
        } = &self.delivery
        {
            if let Ok(key) = key.to_str() {
                add_key_secrets(key, secrets);
            }
            secrets.push(secret.as_bytes().to_vec());
        }
    }
}

fn bearer_expired(account: &Value) -> bool {
    ["expiresAt", "tokenExpiresAt"]
        .into_iter()
        .find_map(|field| {
            let value = &account[field];
            value
                .as_i64()
                .map(|time| {
                    if time < 1_000_000_000_000 {
                        time.saturating_mul(1000)
                    } else {
                        time
                    }
                })
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
                        .ok()
                        .map(|time| time.timestamp_millis())
                })
        })
        .is_some_and(|expiry| expiry <= chrono::Utc::now().timestamp_millis())
}

pub fn prepare_body(body: &mut Value) {
    if let Some(contents) = body.get_mut("contents").and_then(Value::as_array_mut) {
        for turn in contents {
            if let Some(parts) = turn.get_mut("parts").and_then(Value::as_array_mut) {
                for part in parts {
                    if let Some(object) = part.as_object_mut() {
                        if let Some(signature) = object.get_mut("thoughtSignature") {
                            *signature = json!(include_str!("vertex-thinking-signature.txt"));
                        }
                        for field in ["functionCall", "functionResponse"] {
                            if let Some(child) =
                                object.get_mut(field).and_then(Value::as_object_mut)
                            {
                                child.shift_remove("id");
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Exchange {
    pub async fn resolve(
        client: &reqwest::Client,
        provider: &str,
        model: &str,
        stream: bool,
        account: &Value,
    ) -> Result<Self, &'static str> {
        let mut fields = draft(provider, account, model)?;
        let mut service_expiry = None;
        let bearer = match &fields.credential {
            Credential::ApiKey(_) => None,
            Credential::Bearer(token) => Some(bearer_header(token)?),
            Credential::ServiceAccount(account) => {
                let token = account.mint(client, is_fixture()).await?;
                service_expiry = token.expires_at;
                Some(token.header)
            }
            Credential::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
            } => Some(adc_token(client, client_id, client_secret, refresh_token).await?),
        };
        if fields.partner && fields.project.is_none() {
            let key = nonempty(&account["apiKey"]).ok_or("Vertex: could not resolve project_id from API key. Please add it manually in provider settings.")?;
            fields.project = Some(resolve_project(client, key).await?);
        }
        let mut url = destination(&fields, model, stream, bearer.is_some())?;
        if let Credential::ApiKey(key) = fields.credential {
            url.query_pairs_mut().append_pair("key", key);
        }
        Ok(Self {
            url,
            bearer,
            stream,
            service_expiry,
        })
    }

    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    pub fn fixture_url(&self) -> Option<String> {
        is_fixture().then(|| fixture_url(&self.url))
    }

    pub fn headers(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.bearer {
            request = request.header(header::AUTHORIZATION, token.clone());
        }
        if self.stream {
            request = request.header(header::ACCEPT, "text/event-stream");
        }
        request
    }

    pub fn redact_error(&self, text: &str) -> String {
        let mut result = redact_bearer(text);
        if let Some((_, key)) = self.url.query_pairs().find(|(name, _)| name == "key") {
            result = redact_key(&result, &key);
        }
        if let Some(token) = self.bearer_value() {
            result = result.replace(token, "[redacted]");
        }
        result
    }

    fn bearer_value(&self) -> Option<&str> {
        self.bearer
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|value| !value.is_empty())
    }

    pub(crate) fn service_credential(&self) -> Option<(&str, i64)> {
        self.bearer_value().zip(self.service_expiry)
    }

    pub(crate) fn secrets(&self, secrets: &mut Vec<Vec<u8>>) {
        if let Some((_, key)) = self.url.query_pairs().find(|(name, _)| name == "key") {
            add_key_secrets(&key, secrets);
        }
        if let Some(token) = self.bearer_value() {
            secrets.push(token.as_bytes().to_vec());
        }
    }
}

fn redact_key(text: &str, key: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
    text.replace(key, "[redacted]")
        .replace(&encoded, "[redacted]")
}

fn add_key_secrets(key: &str, secrets: &mut Vec<Vec<u8>>) {
    secrets.push(key.as_bytes().to_vec());
    secrets.push(
        url::form_urlencoded::byte_serialize(key.as_bytes())
            .collect::<String>()
            .into_bytes(),
    );
}

struct Draft<'a> {
    partner: bool,
    project: Option<String>,
    location: Option<&'a str>,
    credential: Credential<'a>,
}

fn destination(
    fields: &Draft<'_>,
    model: &str,
    stream: bool,
    bearer: bool,
) -> Result<Url, &'static str> {
    let path = if fields.partner {
        let project = fields.project.as_deref().ok_or("Vertex partner models require a project_id. Add it in providerSpecificData or use Service Account JSON.")?;
        format!("/v1/projects/{project}/locations/global/endpoints/openapi/chat/completions")
    } else {
        let action = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let tail = format!("publishers/google/models/{model}:{action}");
        if let Some(project) = &fields.project {
            let location = fields
                .location
                .unwrap_or_else(|| default_location(model, bearer));
            format!("/v1/projects/{project}/locations/{location}/{tail}")
        } else if !bearer {
            format!("/v1/{tail}")
        } else {
            return Err(
                "Vertex OAuth/ADC requires a project_id. Add quota_project_id to your ADC JSON or set providerSpecificData.projectId.",
            );
        }
    };
    let mut url = Url::parse(&format!("{ORIGIN}{path}")).map_err(|_| "Invalid Vertex endpoint")?;
    if stream && !fields.partner {
        url.query_pairs_mut().append_pair("alt", "sse");
    }
    Ok(url)
}

enum Credential<'a> {
    ApiKey(&'a str),
    Bearer(&'a str),
    ServiceAccount(ServiceAccount),
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
}

pub(super) struct ServiceAccount {
    pub project: String,
    fingerprint: CredentialHash,
    email: String,
    private_key: String,
}

impl ServiceAccount {
    pub(super) fn parse(raw: &str) -> Option<Self> {
        let data: Value = serde_json::from_str(raw).ok()?;
        if data["type"] != "service_account" {
            return None;
        }
        Self::from_json(&data, raw).ok()
    }

    fn from_json(data: &Value, raw: &str) -> Result<Self, &'static str> {
        Ok(Self {
            email: owned_required(data, "client_email")?,
            private_key: owned_required(data, "private_key")?,
            project: owned_required(data, "project_id")?,
            fingerprint: Sha256::digest(raw.as_bytes()).into(),
        })
    }

    pub(super) async fn token(
        &self,
        client: &reqwest::Client,
        fixture: bool,
    ) -> Result<HeaderValue, &'static str> {
        Ok(self.mint(client, fixture).await?.header)
    }

    async fn mint(
        &self,
        client: &reqwest::Client,
        fixture: bool,
    ) -> Result<CachedToken, &'static str> {
        service_token(
            client,
            self.fingerprint,
            &self.email,
            &self.private_key,
            fixture,
        )
        .await
    }
}

fn configured<'a>(account: &'a Value, key: &str) -> Result<Option<&'a str>, &'static str> {
    let value = &account["providerSpecificData"][key];
    if !truthy(value) {
        return Ok(None);
    }
    let segment = nonempty(value)
        .filter(|segment| valid_segment(segment))
        .ok_or("Invalid Vertex endpoint configuration")?;
    Ok(Some(segment))
}

fn draft<'a>(provider: &str, account: &'a Value, model: &str) -> Result<Draft<'a>, &'static str> {
    let fields = credential_draft(provider, account, model)?;
    match &fields.credential {
        Credential::ApiKey(_) => (),
        Credential::Bearer(_)
            if fields.partner
                && fields.project.is_none()
                && nonempty(&account["apiKey"]).is_none() =>
        {
            return Err(
                "Vertex: could not resolve project_id from API key. Please add it manually in provider settings.",
            );
        }
        _ if fields.project.is_none() && !fields.partner => {
            return Err(
                "Vertex OAuth/ADC requires a project_id. Add quota_project_id to your ADC JSON or set providerSpecificData.projectId.",
            );
        }
        Credential::AuthorizedUser { .. } if fields.project.is_none() => {
            return Err(
                "Vertex partner models require a project_id. Add it in providerSpecificData or use Service Account JSON.",
            );
        }
        _ => (),
    }
    Ok(fields)
}

fn credential_draft<'a>(
    provider: &str,
    account: &'a Value,
    model: &str,
) -> Result<Draft<'a>, &'static str> {
    let partner = match provider {
        "vertex" => false,
        "vertex-partner" => true,
        _ => return Err("Unsupported Vertex provider"),
    };
    if model.is_empty() || (!partner && !valid_segment(model)) {
        return Err("Unsupported Vertex model");
    }
    if truthy(&account["refreshToken"]) {
        return Err("Unsupported Vertex refresh configuration");
    }
    let key = nonempty(&account["apiKey"]);
    let token = nonempty(&account["accessToken"]);
    let doc = key
        .filter(|key| key.trim_start().starts_with('{'))
        .map(|key| serde_json::from_str::<Value>(key).map_err(|_| "Invalid Vertex credential JSON"))
        .transpose()?;
    let (project, credential) = match doc.as_ref() {
        Some(data) if data["type"] == "service_account" => {
            let service = ServiceAccount::from_json(data, key.unwrap_or_default())?;
            (
                Some(service.project.clone()),
                Credential::ServiceAccount(service),
            )
        }
        Some(data) if data["type"] == "authorized_user" => {
            let client_id = owned_required(data, "client_id")?;
            let client_secret = owned_required(data, "client_secret")?;
            let refresh_token = owned_required(data, "refresh_token")?;
            let project = nonempty(&data["quota_project_id"]).map(str::to_owned);
            (
                project,
                Credential::AuthorizedUser {
                    client_id,
                    client_secret,
                    refresh_token,
                },
            )
        }
        Some(_) => return Err("Unsupported Vertex credential JSON"),
        None if token.is_some() => (None, Credential::Bearer(token.unwrap_or_default())),
        None => (
            None,
            Credential::ApiKey(key.ok_or("Missing Vertex credentials")?),
        ),
    };
    let project = match project {
        Some(project) if valid_segment(&project) => Some(project),
        Some(_) => return Err("Invalid Vertex project"),
        None => configured(account, "projectId")?.map(str::to_owned),
    };
    let location = if partner {
        None
    } else {
        configured(account, "location")?
    };
    Ok(Draft {
        partner,
        project,
        location,
        credential,
    })
}

fn owned_required(data: &Value, key: &str) -> Result<String, &'static str> {
    nonempty(&data[key])
        .map(str::to_owned)
        .ok_or("Missing Vertex credential field")
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

pub(super) fn valid_segment(segment: &str) -> bool {
    !matches!(segment, "" | "." | "..")
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:@".contains(&byte))
}

fn default_location(model: &str, bearer: bool) -> &'static str {
    if !bearer || model == "gemini-4-argon" {
        "global"
    } else {
        "us-central1"
    }
}

pub(super) fn is_fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

pub(super) fn fixture_url(url: &Url) -> String {
    let mut local = format!("{FIXTURE_ORIGIN}{}", url.path());
    if let Some(query) = url.query() {
        local.push('?');
        local.push_str(query);
    }
    local
}

async fn resolve_project(client: &reqwest::Client, key: &str) -> Result<String, &'static str> {
    const FAILURE: &str = "Vertex: could not resolve project_id from API key. Please add it manually in provider settings.";
    let fingerprint = Sha256::digest(key.as_bytes()).into();
    if let Some(project) = PROJECTS
        .lock()
        .ok()
        .and_then(|projects| projects.get(&fingerprint).cloned())
    {
        return Ok(project);
    }
    let mut url = Url::parse(&format!(
        "{ORIGIN}/v1/publishers/google/models/__probe__:generateContent"
    ))
    .map_err(|_| FAILURE)?;
    url.query_pairs_mut().append_pair("key", key);
    let endpoint = if is_fixture() {
        fixture_url(&url)
    } else {
        url.into()
    };
    let data: Value = client
        .post(endpoint)
        .timeout(HANDSHAKE_TIMEOUT)
        .json(&json!({}))
        .send()
        .await
        .map_err(|_| FAILURE)?
        .json()
        .await
        .map_err(|_| FAILURE)?;
    let message = nonempty(&data[0]["error"]["message"])
        .or_else(|| nonempty(&data["error"]["message"]))
        .ok_or(FAILURE)?;
    let project = message
        .split_once("projects/")
        .and_then(|(_, tail)| tail.split_once('/'))
        .map(|(project, _)| project)
        .filter(|project| valid_segment(project))
        .ok_or(FAILURE)?
        .to_owned();
    if let Ok(mut projects) = PROJECTS.lock() {
        if projects.len() >= CACHE_LIMIT {
            projects.clear();
        }
        projects.insert(fingerprint, project.clone());
    }
    Ok(project)
}

pub(super) fn bearer_header(token: &str) -> Result<HeaderValue, &'static str> {
    let mut header = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "Invalid Vertex bearer token")?;
    header.set_sensitive(true);
    Ok(header)
}

async fn service_token(
    client: &reqwest::Client,
    fingerprint: CredentialHash,
    email: &str,
    private_key: &str,
    fixture: bool,
) -> Result<CachedToken, &'static str> {
    const FAILURE: &str = "Vertex: failed to mint access token from Service Account JSON";
    if let Some(token) = TOKENS.lock().ok().and_then(|tokens| {
        tokens
            .get(&(fingerprint, fixture))
            .filter(|token| {
                token.until.is_some_and(|until| {
                    until.saturating_duration_since(Instant::now()) > TOKEN_REFRESH_BUFFER
                })
            })
            .cloned()
    }) {
        return Ok(token);
    }
    let key = EncodingKey::from_rsa_pem(private_key.replace("\\n", "\n").as_bytes())
        .map_err(|_| FAILURE)?;
    let now = chrono::Utc::now().timestamp();
    let claims = json!({"iss":email,"aud":TOKEN_URL,"scope":SCOPE,"iat":now,"exp":now+3600});
    let mut protected = Header::new(Algorithm::RS256);
    protected.typ = None;
    let assertion = encode(&protected, &claims, &key).map_err(|_| FAILURE)?;
    let data = token_request(
        client,
        fixture,
        false,
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &assertion),
        ],
    )
    .await
    .ok_or(FAILURE)?;
    let header = bearer_header(nonempty(&data["access_token"]).ok_or(FAILURE)?)?;
    let lifetime = Duration::from_secs(data["expires_in"].as_u64().unwrap_or(3600));
    let now = Instant::now();
    let cached = CachedToken {
        header,
        until: now.checked_add(lifetime),
        expires_at: i64::try_from(lifetime.as_millis())
            .ok()
            .and_then(|lifetime| chrono::Utc::now().timestamp_millis().checked_add(lifetime)),
    };
    if cached.until.is_some()
        && let Ok(mut tokens) = TOKENS.lock()
    {
        tokens.retain(|_, token| token.until.is_some_and(|until| until > now));
        if tokens.len() >= CACHE_LIMIT {
            tokens.clear();
        }
        tokens.insert((fingerprint, fixture), cached.clone());
    }
    Ok(cached)
}

async fn adc_token(
    client: &reqwest::Client,
    id: &str,
    secret: &str,
    refresh: &str,
) -> Result<HeaderValue, &'static str> {
    const FAILURE: &str = "Vertex: failed to refresh access token from ADC JSON (authorized_user)";
    let data = token_request(
        client,
        is_fixture(),
        true,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", id),
            ("client_secret", secret),
        ],
    )
    .await
    .ok_or(FAILURE)?;
    bearer_header(nonempty(&data["access_token"]).ok_or(FAILURE)?)
}

async fn token_request(
    client: &reqwest::Client,
    fixture: bool,
    accept_json: bool,
    form: &[(&str, &str)],
) -> Option<Value> {
    let url = if fixture {
        FIXTURE_TOKEN_URL
    } else {
        TOKEN_URL
    };
    let mut request = client.post(url).timeout(HANDSHAKE_TIMEOUT).form(form);
    if accept_json {
        request = request.header(header::ACCEPT, "application/json");
    }
    request
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()
}
