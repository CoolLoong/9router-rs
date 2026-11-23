use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use hmac::{Hmac, Mac};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::chat_format::ChatFormat;
use crate::chat_input::{PreparedChat, js_json, truthy};
use crate::cli_video::redact_bearer;
use crate::provider_index::js_trim;
use crate::proxy_transport::{self, Route, Selection};
use crate::storage::Storage;

#[path = "bedrock_eventstream.rs"]
pub(crate) mod eventstream;
#[path = "bedrock_probe.rs"]
mod probe;
#[path = "bedrock_profile.rs"]
mod profile;

pub(crate) use probe::check as credential_probe;

const AWS_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');
const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const SECURITY_HEADER: &str = "x-amz-security-token";
const DATE_HEADER: &str = "x-amz-date";
const DEFAULT_REGION: &str = "us-east-1";
const AWS_HOST: &str = "bedrock-runtime";
static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
static SIGNATURE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)(signature=)[0-9a-f]{64}").expect("signature pattern")
});

pub fn is_provider(provider: &str) -> bool {
    wire(provider).is_some()
}

pub fn wire(provider: &str) -> Option<ChatFormat> {
    match provider {
        "bedrock" => Some(ChatFormat::Messages),
        "bedrock-xai" => Some(ChatFormat::Chat),
        _ => None,
    }
}

pub fn supports(provider: &str, model: &str, account: &Value) -> bool {
    let Some(wire) = wire(provider) else {
        return false;
    };
    let details = &account["providerSpecificData"];
    model_matches(wire, model)
        && account["authType"] == "apikey"
        && !truthy(&account["refreshToken"])
        && region(account).is_ok()
        && (truthy(&details["profile"]) || Credentials::from_account(account).is_ok())
}

fn direct_environment() -> bool {
    fixture()
        || ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
            .iter()
            .all(|key| std::env::var(key).ok().is_none_or(|url| url.is_empty()))
}

pub(crate) fn requires_tunnel(settings: &Value, account: &Value) -> bool {
    truthy(&settings["outboundProxyEnabled"])
        || !direct_environment()
        || proxy_transport::account_requires_tunnel(account)
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Egress {
    Direct,
    Bypassed,
    Tunnel(Url),
}

impl Egress {
    pub(crate) fn protected(&self) -> bool {
        !matches!(self, Self::Direct)
    }
}

pub(crate) async fn select_egress(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    stream: bool,
) -> Result<Egress, &'static str> {
    let destination = invocation_url(&region(account)?, model, stream)?;
    select_destination(storage, settings, account, &destination).await
}

async fn select_destination(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    destination: &Url,
) -> Result<Egress, &'static str> {
    if truthy(&account["providerSpecificData"]["vercelRelayUrl"]) {
        return Err("Bedrock requires a direct connection or an HTTP or SOCKS tunnel");
    }
    match proxy_transport::select_with_bypass(storage, settings, account, destination.as_str())
        .await
    {
        Ok(Selection::ExplicitBypass) => Ok(Egress::Bypassed),
        Ok(Selection::Routed(Route::Direct)) if !requires_tunnel(settings, account) => {
            Ok(Egress::Direct)
        }
        Ok(Selection::Routed(Route::Proxy { url, .. })) => proxy_transport::tunnel_proxy_url(&url)
            .map(Egress::Tunnel)
            .ok_or("Unsupported Bedrock proxy URL"),
        Ok(Selection::Routed(Route::Relay { .. })) => {
            Err("Bedrock requires a direct connection or an HTTP or SOCKS tunnel")
        }
        _ => Err("Bedrock connection proxy is unavailable"),
    }
}

pub struct Exchange {
    url: Url,
    egress: Egress,
    headers: HeaderMap,
    body: String,
    format: ChatFormat,
    stream: bool,
    redactions: Redactions,
}

impl Exchange {
    pub async fn new(
        provider: &str,
        model: &str,
        account: &Value,
        prepared: &PreparedChat,
        egress: Egress,
        storage: &Storage,
        settings: &Value,
    ) -> Result<Self, &'static str> {
        let PreparedChat { body, stream } = prepared;
        let stream = *stream;
        let format = wire(provider).ok_or("Unsupported Bedrock provider")?;
        if !model_matches(format, model) {
            return Err("Unsupported Bedrock model family");
        }
        let resolved = resolve(storage, settings, account, account).await?;
        let region = &resolved.region;
        let credentials = resolved.credentials();
        let redactions = Redactions::for_credentials(credentials);
        let url = invocation_url(region, model, stream)?;
        let current = storage
            .read_settings()
            .await
            .ok_or("Bedrock connection proxy is unavailable")?;
        if select_destination(storage, &current, account, &url).await? != egress {
            return Err("Bedrock connection proxy changed before dispatch");
        }
        let body = request_body(body.clone(), format).ok_or("Invalid Bedrock request body")?;
        let body = js_json(&body, false).unwrap_or_else(|| body.to_string());
        let headers = sign(
            &url,
            "POST",
            region,
            "bedrock",
            BTreeMap::from([
                ("accept", accept(stream)),
                ("content-type", "application/json"),
            ]),
            body.as_bytes(),
            credentials,
        )?;
        Ok(Self {
            url,
            egress,
            headers,
            body,
            format,
            stream,
            redactions,
        })
    }

    pub async fn send(self) -> Result<(reqwest::Response, Redactions), &'static str> {
        let timeout = std::env::var("FETCH_CONNECT_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(60_000);
        let (client, url) = match self.egress {
            Egress::Tunnel(proxy) => {
                tunnel_client(proxy.as_str(), &self.url).ok_or("[502]: Invalid Bedrock proxy")?
            }
            Egress::Direct | Egress::Bypassed if fixture() => (
                http().clone(),
                format!("http://127.0.0.1:52989/bedrock{}", self.url.path()),
            ),
            Egress::Direct | Egress::Bypassed => (http().clone(), self.url.to_string()),
        };
        let request = client.post(url).headers(self.headers).body(self.body);
        let response = tokio::time::timeout(Duration::from_millis(timeout), request.send())
            .await
            .map_err(|_| "[502]: Bedrock fetch connect timeout")?
            .map_err(|_| "[502]: Bedrock fetch failed")?;
        if response.status().is_redirection() {
            return Err("[502]: Bedrock returned an unexpected redirect");
        }
        let response = if self.stream && response.status().is_success() {
            eventstream::adapt(response, self.format, self.redactions.0.clone())
        } else {
            response
        };
        Ok((response, self.redactions))
    }
}

pub(crate) fn preview(
    provider: &str,
    model: &str,
    account: &Value,
    prepared: PreparedChat,
) -> Option<Value> {
    if !supports(provider, model, account) {
        return None;
    }
    let url = invocation_url(&region(account).ok()?, model, prepared.stream).ok()?;
    let mut body = request_body(prepared.body, wire(provider)?)?;
    if !prepared.stream {
        body["stream"] = json!(false);
    }
    Some(json!({"url":url.as_str(),"headers":{
        "Content-Type":"application/json", "Accept":accept(prepared.stream), "Authorization":"[redacted]",
    },"body":body}))
}

fn request_body(mut body: Value, format: ChatFormat) -> Option<Value> {
    let fields = body.as_object_mut()?;
    fields.shift_remove("model");
    fields.shift_remove("stream");
    if format == ChatFormat::Messages {
        fields.insert("anthropic_version".into(), json!("bedrock-2023-05-31"));
    }
    Some(body)
}

fn accept(stream: bool) -> &'static str {
    if stream {
        "application/vnd.amazon.eventstream"
    } else {
        "application/json"
    }
}

fn invocation_url(region: &str, model: &str, stream: bool) -> Result<Url, &'static str> {
    let action = if stream {
        "invoke-with-response-stream"
    } else {
        "invoke"
    };
    let encoded = utf8_percent_encode(model, AWS_ESCAPE);
    Url::parse(&format!(
        "https://{AWS_HOST}.{region}.amazonaws.com/model/{encoded}/{action}"
    ))
    .map_err(|_| "Invalid Bedrock endpoint")
}

pub struct Redactions(Arc<[String]>);

impl Redactions {
    pub(crate) fn secrets(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub(crate) fn for_credentials(credentials: Credentials<'_>) -> Self {
        Self(
            [
                Some(credentials.access),
                Some(credentials.secret),
                credentials.session,
            ]
            .into_iter()
            .flatten()
            .filter(|token| token.encode_utf16().count() >= 8)
            .map(str::to_owned)
            .collect(),
        )
    }

    pub fn redact(&self, text: &str) -> String {
        redact_values(text, self.0.iter().map(String::as_str))
    }
}

struct Resolved<'a> {
    region: String,
    material: Material<'a>,
}

enum Material<'a> {
    Static(Credentials<'a>),
    Profile(Arc<profile::Owned>),
}

impl Resolved<'_> {
    fn credentials(&self) -> Credentials<'_> {
        match &self.material {
            Material::Static(credentials) => *credentials,
            Material::Profile(owned) => owned.credentials(),
        }
    }
}

async fn resolve<'a>(
    storage: &Storage,
    settings: &Value,
    account: &'a Value,
    route_account: &Value,
) -> Result<Resolved<'a>, &'static str> {
    let region = region(account)?;
    let name = &account["providerSpecificData"]["profile"];
    let material = if truthy(name) {
        Material::Profile(profile::resolve(name, storage, settings, route_account).await?)
    } else {
        Material::Static(Credentials::from_account(account)?)
    };
    Ok(Resolved { region, material })
}

fn http() -> &'static reqwest::Client {
    HTTP.get_or_init(|| client_builder().build().expect("Bedrock HTTP client"))
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn tunnel_client(address: &str, endpoint: &Url) -> Option<(reqwest::Client, String)> {
    let builder = client_builder().proxy(reqwest::Proxy::all(address).ok()?);
    #[cfg(debug_assertions)]
    if fixture()
        && Url::parse(address).is_ok_and(|url| url.host_str() == Some("127.0.0.1"))
        && let Some(cert) = std::env::var_os("NINEROUTER_BEDROCK_LOCAL_PROBE_CA")
    {
        let pem = std::fs::read(cert).ok()?;
        let client = builder
            .add_root_certificate(reqwest::Certificate::from_pem(&pem).ok()?)
            .build()
            .ok()?;
        let mut destination = endpoint.clone();
        destination.set_host(Some("localhost")).ok()?;
        destination.set_port(Some(52987)).ok()?;
        return Some((client, destination.into()));
    }
    Some((builder.build().ok()?, endpoint.to_string()))
}

fn model_matches(format: ChatFormat, model: &str) -> bool {
    if model.encode_utf16().count() > 2048
        || model.chars().any(char::is_control)
        || model.contains(['(', ')'])
    {
        return false;
    }
    match format {
        ChatFormat::Messages => {
            model.starts_with("anthropic.")
                || model.contains(".anthropic.")
                || model.contains("/anthropic.")
        }
        ChatFormat::Chat => {
            model.starts_with("xai.") || model.contains(".xai.") || model.contains("/xai.")
        }
        ChatFormat::Responses => false,
    }
}

fn fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

pub(crate) fn region(account: &Value) -> Result<String, &'static str> {
    let explicit = &account["providerSpecificData"]["region"];
    let region = if truthy(explicit) {
        explicit.as_str().ok_or("Invalid AWS region")?.to_owned()
    } else {
        ["AWS_REGION", "AWS_DEFAULT_REGION"]
            .into_iter()
            .find_map(|key| std::env::var(key).ok().filter(|region| !region.is_empty()))
            .unwrap_or_else(|| DEFAULT_REGION.to_owned())
    };
    let region = js_trim(&region);
    if !valid_region(region) {
        return Err("Invalid AWS region");
    }
    Ok(region.to_owned())
}

fn valid_region(region: &str) -> bool {
    !region.is_empty()
        && region.len() <= 31
        && !region.starts_with('-')
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

#[derive(Clone, Copy)]
pub(crate) struct Credentials<'a> {
    access: &'a str,
    secret: &'a str,
    session: Option<&'a str>,
}

impl<'a> Credentials<'a> {
    pub(crate) fn from_account(account: &'a Value) -> Result<Self, &'static str> {
        let details = &account["providerSpecificData"];
        let access =
            nonempty(&details["accessKeyId"]).ok_or("Bedrock static credentials are incomplete")?;
        let secret =
            nonempty(&account["apiKey"]).ok_or("Bedrock static credentials are incomplete")?;
        let session = nonempty(&details["sessionToken"]);
        let credentials = Self {
            access,
            secret,
            session,
        };
        credentials.validate()?;
        Ok(credentials)
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.access.is_empty() || self.secret.is_empty() {
            return Err("Bedrock credentials are incomplete");
        }
        if self.access.starts_with("ASIA") && self.session.is_none() {
            return Err("Temporary AWS credentials require a session token");
        }
        if !self
            .access
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            || self
                .session
                .is_some_and(|token| HeaderValue::from_str(token).is_err())
        {
            return Err("Invalid Bedrock credentials");
        }
        Ok(())
    }
}

pub(crate) fn sign(
    url: &Url,
    method: &str,
    region: &str,
    service: &str,
    headers: BTreeMap<&'static str, &str>,
    body: &[u8],
    credentials: Credentials<'_>,
) -> Result<HeaderMap, &'static str> {
    let host = url.host_str().ok_or("Invalid Bedrock endpoint")?;
    let date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let scope = format!("{}/{region}/{service}/aws4_request", &date[..8]);
    let mut fields = BTreeMap::from([("host", host), (DATE_HEADER, date.as_str())]);
    fields.extend(headers);
    if let Some(session) = credentials.session {
        fields.insert(SECURITY_HEADER, session);
    }
    let signed = fields.keys().copied().collect::<Vec<_>>().join(";");
    let mut canonical_headers = String::new();
    for (name, value) in &fields {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(&value.split_whitespace().collect::<Vec<_>>().join(" "));
        canonical_headers.push('\n');
    }
    let path = url
        .path()
        .split('/')
        .map(|part| utf8_percent_encode(part, AWS_ESCAPE).to_string())
        .collect::<Vec<_>>()
        .join("/");
    let body_hash = format!("{:x}", Sha256::digest(body));
    let canonical = format!("{method}\n{path}\n\n{canonical_headers}\n{signed}\n{body_hash}");
    let signing_input = format!(
        "{ALGORITHM}\n{date}\n{scope}\n{:x}",
        Sha256::digest(canonical)
    );
    let mut base = Vec::from(b"AWS4");
    base.extend_from_slice(credentials.secret.as_bytes());
    let k_date = hmac(&base, &date[..8]);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, service);
    let k_signing = hmac(&k_service, "aws4_request");
    let signature: String = hmac(&k_signing, &signing_input)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let authorization = format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        credentials.access
    );
    let mut headers = HeaderMap::new();
    for (name, value) in fields {
        let mut value =
            HeaderValue::from_str(value).map_err(|_| "Invalid Bedrock request headers")?;
        if name == SECURITY_HEADER {
            value.set_sensitive(true);
        }
        headers.insert(HeaderName::from_static(name), value);
    }
    let mut authorization =
        HeaderValue::from_str(&authorization).map_err(|_| "Invalid Bedrock authorization")?;
    authorization.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, authorization);
    Ok(headers)
}

fn hmac(key: &[u8], text: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(text.as_bytes());
    mac.finalize().into_bytes().into()
}

fn redact_values<'a>(text: &str, values: impl Iterator<Item = &'a str>) -> String {
    let safe = redact_bearer(text);
    values.fold(
        SIGNATURE.replace_all(&safe, "${1}[redacted]").into_owned(),
        |safe, token| safe.replace(token, "[redacted]"),
    )
}
