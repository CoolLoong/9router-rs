use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::{Method, RequestBuilder, header};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::{Position, Url};

use super::{
    Credentials, Egress, fixture, http, select_destination, sign, tunnel_client, valid_region,
};
use crate::provider_index::js_trim;
use crate::storage::Storage;

#[path = "bedrock_profile_cache.rs"]
mod cache;
#[path = "bedrock_login_profile.rs"]
mod login;
#[path = "bedrock_metadata_profile.rs"]
mod metadata;
#[path = "bedrock_process_profile.rs"]
mod process;
#[path = "bedrock_sso_profile.rs"]
mod sso;

const FILE_LIMIT: u64 = 1024 * 1024;
const RESPONSE_LIMIT: usize = 128 * 1024;
const MAX_PROFILES: usize = 128;
const MAX_CHAIN: usize = 32;
const STS_ERROR: &str = "AWS STS AssumeRole failed";
const WEB_IDENTITY_ERROR: &str = "AWS STS AssumeRoleWithWebIdentity failed";
const WEB_IDENTITY_FILE_ERROR: &str = "Could not read the AWS Web Identity credential";
const TOKEN_FILE_LIMIT: u64 = 64 * 1024;
const NO_PROFILE: &str = "Could not resolve the named AWS profile";
const TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_TTL: Duration = Duration::from_secs(60);

type Fields = BTreeMap<String, String>;
type Profiles = BTreeMap<String, Fields>;
type Slot = Arc<tokio::sync::Mutex<Option<Cached>>>;

static CACHE: Mutex<VecDeque<(ProfileKey, Slot)>> = Mutex::new(VecDeque::new());

#[derive(PartialEq, Eq)]
struct ProfileKey {
    root: PathBuf,
    name: String,
    routes: Routes,
    web_identity: Option<WebIdentity>,
    credential_process: Option<[u8; 32]>,
    metadata: Option<metadata::Source>,
    login_cache: Option<PathBuf>,
}

#[derive(PartialEq, Eq, Clone)]
struct Routes {
    sts: Option<Endpoint>,
    sso: Option<sso::Endpoints>,
    web_identity: Option<Endpoint>,
    metadata: Option<Endpoint>,
}

struct Policy<'a> {
    storage: &'a Storage,
    settings: &'a Value,
    account: &'a Value,
}

#[derive(PartialEq, Eq, Clone)]
struct Endpoint {
    target: Url,
    direct_fixture: String,
    delivery: Egress,
}

impl Policy<'_> {
    async fn bind(&self, target: Url, direct_fixture: String) -> Result<Endpoint, &'static str> {
        let delivery =
            select_destination(self.storage, self.settings, self.account, &target).await?;
        Ok(Endpoint {
            target,
            direct_fixture,
            delivery,
        })
    }
}

impl Endpoint {
    async fn request(
        &self,
        policy: &Policy<'_>,
        method: Method,
    ) -> Result<RequestBuilder, &'static str> {
        let settings = policy
            .storage
            .read_settings()
            .await
            .ok_or("AWS profile connection proxy is unavailable")?;
        if select_destination(policy.storage, &settings, policy.account, &self.target).await?
            != self.delivery
        {
            return Err("AWS profile connection proxy changed before dispatch");
        }
        let host = &self.target[Position::BeforeHost..Position::AfterPort];
        let (client, url) = match &self.delivery {
            Egress::Tunnel(proxy) => tunnel_client(proxy.as_str(), &self.target)
                .ok_or("AWS profile connection proxy is unavailable")?,
            Egress::Direct | Egress::Bypassed if fixture() => {
                let mut url =
                    Url::parse(&self.direct_fixture).map_err(|_| "Invalid AWS profile endpoint")?;
                url.set_query(self.target.query());
                (http().clone(), url.to_string())
            }
            Egress::Direct | Egress::Bypassed => (http().clone(), self.target.to_string()),
        };
        Ok(client.request(method, url).header(header::HOST, host))
    }
}

struct Cached {
    credentials: Arc<Owned>,
    until: Instant,
}

pub(super) struct Owned {
    access: String,
    secret: String,
    session: Option<String>,
    expires: Option<DateTime<Utc>>,
}

impl Owned {
    pub fn credentials(&self) -> Credentials<'_> {
        Credentials {
            access: &self.access,
            secret: &self.secret,
            session: self.session.as_deref(),
        }
    }
}

pub(super) async fn resolve(
    raw: &Value,
    storage: &Storage,
    settings: &Value,
    account: &Value,
) -> Result<Arc<Owned>, &'static str> {
    let name = raw
        .as_str()
        .filter(|name| valid_name(name, true))
        .ok_or("Invalid AWS profile name")?;
    tokio::time::timeout(TIMEOUT, async {
        let profile = name.to_owned();
        let plan = tokio::task::spawn_blocking(move || Plan::load(&profile))
            .await
            .map_err(|_| NO_PROFILE)??;
        let policy = Policy {
            storage,
            settings,
            account,
        };
        let plan = match plan {
            Plan {
                base: Base::Static(credentials),
                sts_region: None,
                ..
            } => {
                return Ok(Arc::new(credentials));
            }
            plan => plan,
        };
        let routes = plan.routes(&policy).await?;
        let web_identity = if let Base::WebIdentity(identity) = &plan.base {
            Some(identity.clone())
        } else {
            None
        };
        let credential_process = if let Base::Process(command) = &plan.base {
            Some(Sha256::digest(command.as_bytes()).into())
        } else {
            None
        };
        let metadata = if let Base::Metadata(source) = &plan.base {
            Some(source.clone())
        } else {
            None
        };
        let login_cache = if let Base::Login(source) = &plan.base {
            Some(source.cache_file().to_owned())
        } else {
            None
        };
        let slot = profile_slot(ProfileKey {
            root: storage.root().to_owned(),
            name: name.to_owned(),
            routes: routes.clone(),
            web_identity,
            credential_process,
            metadata,
            login_cache,
        })?;
        let mut cell = slot.lock().await;
        if let Some(cached) = cell.as_ref().filter(|cached| cached.until > Instant::now()) {
            return Ok(cached.credentials.clone());
        }
        let resolved = Arc::new(plan.resolve(&policy, &routes).await?);
        let ttl = resolved.expires.map_or(DEFAULT_TTL, |expires| {
            (expires - Utc::now() - chrono::TimeDelta::minutes(5))
                .to_std()
                .unwrap_or_default()
        });
        *cell = Some(Cached {
            credentials: resolved.clone(),
            until: Instant::now() + ttl,
        });
        Ok(resolved)
    })
    .await
    .map_err(|_| "Resolving the named AWS profile timed out")?
}

fn profile_slot(key: ProfileKey) -> Result<Slot, &'static str> {
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(index) = cache.iter().position(|(profile, _)| profile == &key)
        && let Some(entry) = cache.remove(index)
    {
        let slot = entry.1.clone();
        cache.push_back(entry);
        return Ok(slot);
    }
    if cache.len() == MAX_PROFILES {
        let index = cache
            .iter()
            .position(|(_, slot)| Arc::strong_count(slot) == 1)
            .ok_or("Too many active AWS profile resolutions")?;
        cache.remove(index);
    }
    let slot = Arc::new(tokio::sync::Mutex::new(None));
    cache.push_back((key, slot.clone()));
    Ok(slot)
}

struct Plan {
    base: Base,
    roles: Vec<Role>,
    sts_region: Option<String>,
}

enum Base {
    Static(Owned),
    Sso(sso::Session),
    WebIdentity(WebIdentity),
    Process(String),
    Metadata(metadata::Source),
    Login(login::Session),
}

impl Base {
    fn from_source(source: &str, profiles: &Profiles, name: &str) -> Result<Self, &'static str> {
        if source == "Environment" {
            return environment_credentials().map(Self::Static);
        }
        metadata::Source::select(source, profiles, name).map(Self::Metadata)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct WebIdentity {
    file: PathBuf,
    role: String,
    session: Option<String>,
    region: String,
}

struct Role {
    arn: String,
    session: Option<String>,
    external_id: Option<String>,
    duration: String,
}

impl Plan {
    fn load(name: &str) -> Result<Self, &'static str> {
        let home = std::env::home_dir().ok_or(NO_PROFILE)?;
        let config = path(&home, "AWS_CONFIG_FILE", "config");
        let credentials = path(&home, "AWS_SHARED_CREDENTIALS_FILE", "credentials");
        let mut profiles = read(&config, true);
        let sessions = profiles
            .iter()
            .filter_map(|(name, values)| {
                name.strip_prefix("sso-session.")
                    .map(|name| (name.to_owned(), values.clone()))
            })
            .collect();
        for (profile, fields) in read(&credentials, false) {
            profiles.entry(profile).or_default().extend(fields);
        }
        Self::chain(&profiles, &sessions, &home, name)
    }

    fn chain(
        profiles: &Profiles,
        sessions: &Profiles,
        home: &Path,
        name: &str,
    ) -> Result<Self, &'static str> {
        let mut visited = HashSet::new();
        let mut roles: Vec<Role> = Vec::new();
        let mut sts_region = None;
        let profile = name;
        let mut name = name;
        let base = loop {
            let fields = profiles.get(name).ok_or(NO_PROFILE)?;
            let static_values = fields
                .get("aws_access_key_id")
                .zip(fields.get("aws_secret_access_key"));
            if !roles.is_empty()
                && let Some((access, secret)) = static_values
            {
                break Base::Static(static_credentials(fields, access, secret)?);
            }
            let source = fields.get("credential_source").map(String::as_str);
            let next = fields.get("source_profile").filter(|_| source.is_none());
            let provider = source.filter(|_| !fields.contains_key("source_profile"));
            if let Some(arn) = fields.get("role_arn")
                && (next.is_some() || provider.is_some())
            {
                if !visited.insert(name) || roles.len() == MAX_CHAIN {
                    return Err("The AWS source_profile role chain is cyclic or too long");
                }
                if next.is_some_and(|next| !valid_name(next, true)) || !valid_role(arn) {
                    return Err("Invalid AWS source_profile or role_arn");
                }
                if fields.contains_key("mfa_serial") {
                    return Err("AWS profiles that require MFA need an authenticated session");
                }
                if sts_region.is_none() {
                    sts_region = Some(role_region(fields.get("region"), profiles)?);
                }
                roles.push(Role {
                    arn: arn.clone(),
                    session: fields.get("role_session_name").cloned(),
                    external_id: fields.get("external_id").cloned(),
                    duration: duration(fields.get("duration_seconds"))?,
                });
                if let Some(next) = next {
                    name = next;
                    continue;
                }
                break Base::from_source(provider.ok_or(NO_PROFILE)?, profiles, profile)?;
            }
            if let Some(provider) = provider
                && !roles.is_empty()
            {
                break Base::from_source(provider, profiles, profile)?;
            }
            if fields.contains_key("role_arn") && source.is_some() {
                return Err("AWS credential_source is not supported");
            }
            if let Some((access, secret)) = static_values {
                break Base::Static(static_credentials(fields, access, secret)?);
            }
            if let Some(file) = fields.get("web_identity_token_file") {
                let role = fields
                    .get("role_arn")
                    .filter(|role| valid_role(role))
                    .ok_or("Invalid AWS Web Identity role_arn")?;
                let file = PathBuf::from(file);
                let file = file
                    .strip_prefix("~")
                    .map_or_else(|_| file.clone(), |relative| home.join(relative));
                if file.as_os_str().is_empty() {
                    return Err(WEB_IDENTITY_FILE_ERROR);
                }
                let region = role_region(None, profiles)?;
                let partition = if region.starts_with("cn-") {
                    "arn:aws-cn:"
                } else if region.starts_with("us-gov-") {
                    "arn:aws-us-gov:"
                } else {
                    "arn:aws:"
                };
                if !role.starts_with(partition) {
                    return Err("AWS Web Identity role does not match the STS region");
                }
                break Base::WebIdentity(WebIdentity {
                    file,
                    role: role.clone(),
                    session: fields.get("role_session_name").cloned(),
                    region,
                });
            }
            if let Some(command) = fields.get("credential_process") {
                break Base::Process(command.clone());
            }
            if fields.keys().any(|key| key.starts_with("sso_")) {
                break Base::Sso(sso::Session::select(fields, sessions, home)?);
            }
            if let Some(session) = fields.get("login_session").filter(|name| !name.is_empty()) {
                break Base::Login(login::Session::new(
                    session,
                    fields.get("region").cloned(),
                    home,
                ));
            }
            return Err(NO_PROFILE);
        };
        Ok(Self {
            base,
            roles,
            sts_region,
        })
    }

    async fn routes(&self, policy: &Policy<'_>) -> Result<Routes, &'static str> {
        let sts = async {
            let Some(region) = self.sts_region.as_deref() else {
                return Ok(None);
            };
            sts_endpoint(policy, region).await.map(Some)
        };
        let sso = async {
            match &self.base {
                Base::Sso(session) => session.routes(policy).await.map(Some),
                _ => Ok(None),
            }
        };
        let web_identity = async {
            match &self.base {
                Base::WebIdentity(identity) => {
                    sts_endpoint(policy, &identity.region).await.map(Some)
                }
                _ => Ok(None),
            }
        };
        let metadata = async {
            match &self.base {
                Base::Metadata(source) => source.route(policy).await,
                _ => Ok(None),
            }
        };
        let (sts, sso, web_identity, metadata) =
            tokio::try_join!(sts, sso, web_identity, metadata)?;
        Ok(Routes {
            sts,
            sso,
            web_identity,
            metadata,
        })
    }

    async fn resolve(self, policy: &Policy<'_>, routes: &Routes) -> Result<Owned, &'static str> {
        let mut credentials = match self.base {
            Base::Static(credentials) => credentials,
            Base::Process(command) => process::credentials(&command).await?,
            Base::Metadata(source) => source.credentials(policy, routes.metadata.as_ref()).await?,
            Base::Login(session) => session.credentials(policy).await?,
            Base::Sso(session) => {
                session
                    .credentials(policy, routes.sso.as_ref().ok_or(NO_PROFILE)?)
                    .await?
            }
            Base::WebIdentity(identity) => {
                identity
                    .assume(
                        policy,
                        routes.web_identity.as_ref().ok_or(WEB_IDENTITY_ERROR)?,
                    )
                    .await?
            }
        };
        if let Some(region) = &self.sts_region
            && let Some(endpoint) = routes.sts.as_ref()
        {
            for role in self.roles.into_iter().rev() {
                credentials =
                    assume(policy, endpoint, region, &role, credentials.credentials()).await?;
            }
        }
        Ok(credentials)
    }
}

async fn sts_endpoint(policy: &Policy<'_>, region: &str) -> Result<Endpoint, &'static str> {
    let domain = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    let url = Url::parse(&format!("https://sts.{region}.{domain}/")).map_err(|_| STS_ERROR)?;
    let fixture = format!("http://127.0.0.1:52989/bedrock-profile/sts/{region}");
    policy.bind(url, fixture).await
}

impl WebIdentity {
    async fn assume(self, policy: &Policy<'_>, endpoint: &Endpoint) -> Result<Owned, &'static str> {
        let token = tokio::task::spawn_blocking(move || web_identity_token(&self.file))
            .await
            .map_err(|_| WEB_IDENTITY_FILE_ERROR)??;
        let session = self
            .session
            .unwrap_or_else(|| format!("aws-sdk-js-session-{}", Utc::now().timestamp_millis()));
        let response = endpoint
            .request(policy, Method::POST)
            .await?
            .form(&[
                ("Action", "AssumeRoleWithWebIdentity"),
                ("Version", "2011-06-15"),
                ("RoleArn", self.role.as_str()),
                ("RoleSessionName", &session),
                ("WebIdentityToken", token.as_str()),
            ])
            .send()
            .await
            .map_err(|_| WEB_IDENTITY_ERROR)?;
        sts_credentials(response, WEB_IDENTITY_ERROR).await
    }
}

fn web_identity_token(path: &Path) -> Result<String, &'static str> {
    let contents = read_credential_file(path, WEB_IDENTITY_FILE_ERROR)?;
    let token = contents.trim_ascii();
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(WEB_IDENTITY_FILE_ERROR);
    }
    Ok(token.to_owned())
}

fn read_credential_file(path: &Path, error: &'static str) -> Result<String, &'static str> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let file: std::fs::File = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| error)?
    .into();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let file = {
        if !std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            return Err(error);
        }
        std::fs::File::open(path).map_err(|_| error)?
    };
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= TOKEN_FILE_LIMIT)
    {
        return Err(error);
    }
    let mut bytes = Vec::new();
    file.take(TOKEN_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error)?;
    if bytes.len() as u64 > TOKEN_FILE_LIMIT {
        return Err(error);
    }
    String::from_utf8(bytes).map_err(|_| error)
}

fn static_credentials(fields: &Fields, access: &str, secret: &str) -> Result<Owned, &'static str> {
    let owned = Owned {
        access: access.to_owned(),
        secret: secret.to_owned(),
        session: fields.get("aws_session_token").cloned(),
        expires: None,
    };
    owned.credentials().validate()?;
    Ok(owned)
}

fn environment_credentials() -> Result<Owned, &'static str> {
    let read = |name| std::env::var(name).ok().filter(|value| !value.is_empty());
    let (access, secret) = read("AWS_ACCESS_KEY_ID")
        .zip(read("AWS_SECRET_ACCESS_KEY"))
        .ok_or("Could not resolve AWS environment credentials")?;
    let owned = Owned {
        access,
        secret,
        session: read("AWS_SESSION_TOKEN"),
        expires: read("AWS_CREDENTIAL_EXPIRATION").and_then(|value| value.parse().ok()),
    };
    owned.credentials().validate()?;
    Ok(owned)
}

fn role_region(configured: Option<&String>, profiles: &Profiles) -> Result<String, &'static str> {
    let env = std::env::var("AWS_REGION")
        .ok()
        .filter(|region| !region.is_empty());
    let default = std::env::var("AWS_PROFILE")
        .ok()
        .filter(|profile| !profile.is_empty())
        .unwrap_or_else(|| "default".to_owned());
    let region = configured
        .map(String::as_str)
        .or(env.as_deref())
        .or_else(|| {
            profiles
                .get(&default)
                .and_then(|profile| profile.get("region"))
                .map(String::as_str)
        })
        .unwrap_or("us-east-1");
    valid_region(region)
        .then(|| region.to_owned())
        .ok_or("Invalid AWS STS region")
}

fn duration(value: Option<&String>) -> Result<String, &'static str> {
    let Some(raw) = value else {
        return Ok("3600".into());
    };
    let raw = raw.strip_prefix('+').unwrap_or(raw);
    let digits = raw.bytes().take_while(u8::is_ascii_digit).count();
    let amount = raw[..digits]
        .parse::<u32>()
        .ok()
        .filter(|seconds| (900..=43_200).contains(seconds))
        .ok_or("Invalid AWS STS duration_seconds")?;
    Ok(amount.to_string())
}

fn valid_role(arn: &str) -> bool {
    let mut fields = arn.splitn(6, ':');
    fields.next() == Some("arn")
        && fields
            .next()
            .is_some_and(|part| matches!(part, "aws" | "aws-cn" | "aws-us-gov"))
        && fields.next() == Some("iam")
        && fields.next() == Some("")
        && fields.next().is_some_and(|account| {
            account.len() == 12 && account.bytes().all(|value| value.is_ascii_digit())
        })
        && fields.next().is_some_and(|role| {
            role.starts_with("role/")
                && role.len() > 5
                && role
                    .bytes()
                    .all(|value| value.is_ascii_alphanumeric() || b"/+=,.@_-".contains(&value))
        })
}

async fn assume(
    policy: &Policy<'_>,
    endpoint: &Endpoint,
    region: &str,
    role: &Role,
    source: Credentials<'_>,
) -> Result<Owned, &'static str> {
    let session = role
        .session
        .clone()
        .unwrap_or_else(|| format!("aws-sdk-js-{}", Utc::now().timestamp_millis()));
    let body = {
        let mut values = url::form_urlencoded::Serializer::new(String::new());
        values
            .append_pair("Action", "AssumeRole")
            .append_pair("Version", "2011-06-15")
            .append_pair("RoleArn", &role.arn)
            .append_pair("RoleSessionName", &session)
            .append_pair("DurationSeconds", &role.duration);
        if let Some(external_id) = &role.external_id {
            values.append_pair("ExternalId", external_id);
        }
        values.finish()
    };
    let headers = sign(
        &endpoint.target,
        "POST",
        region,
        "sts",
        BTreeMap::from([("content-type", "application/x-www-form-urlencoded")]),
        body.as_bytes(),
        source,
    )
    .map_err(|_| STS_ERROR)?;
    let response = endpoint
        .request(policy, Method::POST)
        .await?
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|_| STS_ERROR)?;
    sts_credentials(response, STS_ERROR).await
}

async fn sts_credentials(
    response: reqwest::Response,
    failure: &'static str,
) -> Result<Owned, &'static str> {
    let bytes = response_bytes(response).await.ok_or(failure)?;
    let xml = std::str::from_utf8(&bytes).map_err(|_| failure)?;
    let doc = roxmltree::Document::parse(xml).map_err(|_| failure)?;
    let fields = doc
        .descendants()
        .find(|node| node.has_tag_name("Credentials"))
        .ok_or(failure)?;
    let value = |tag| {
        fields
            .children()
            .find(|child| child.has_tag_name(tag))
            .and_then(|node| node.text())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or(failure)
    };
    let expires = DateTime::parse_from_rfc3339(value("Expiration")?)
        .map_err(|_| failure)?
        .with_timezone(&Utc);
    if expires <= Utc::now() {
        return Err(failure);
    }
    let credential = Owned {
        access: value("AccessKeyId")?.to_owned(),
        secret: value("SecretAccessKey")?.to_owned(),
        session: Some(value("SessionToken")?.to_owned()),
        expires: Some(expires),
    };
    credential.credentials().validate().map_err(|_| failure)?;
    Ok(credential)
}

async fn response_bytes(mut response: reqwest::Response) -> Option<Vec<u8>> {
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > RESPONSE_LIMIT as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > RESPONSE_LIMIT {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}

fn path(home: &Path, var: &str, default: &str) -> PathBuf {
    let Some(value) = std::env::var_os(var).filter(|value| !value.is_empty()) else {
        return home.join(".aws").join(default);
    };
    let path = PathBuf::from(value);
    path.strip_prefix("~")
        .map_or_else(|_| path.clone(), |relative| home.join(relative))
}

fn read(path: &Path, config: bool) -> Profiles {
    read_bounded(path).map_or_else(BTreeMap::new, |text| parse_ini(&text, config))
}

fn read_bounded(path: &Path) -> Option<String> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > FILE_LIMIT {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn parse_ini(text: &str, config: bool) -> Profiles {
    let mut profiles: Profiles = BTreeMap::new();
    let mut section = None;
    let mut subsection = None;
    for raw in text.split('\n') {
        let line = js_trim(strip_comment(raw));
        if let Some(header) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            if matches!(header, "__proto__" | "profile __proto__") {
                return BTreeMap::new();
            }
            section = parse_section(header, config);
            subsection = None;
            continue;
        }
        let Some(section) = &section else {
            continue;
        };
        if let Some((name, value)) = line.split_once('=') {
            let name = js_trim(name);
            let value = js_trim(value);
            if name.is_empty() {
                continue;
            }
            if value.is_empty() {
                subsection = Some(name);
                continue;
            }
            if raw
                .chars()
                .next()
                .is_none_or(|ch| !crate::cli::js_space(ch))
            {
                subsection = None;
            }
            let key = subsection.map_or_else(|| name.to_owned(), |sub| format!("{sub}.{name}"));
            profiles
                .entry(section.clone())
                .or_default()
                .insert(key, value.to_owned());
        }
    }
    profiles
}

fn strip_comment(line: &str) -> &str {
    let mut prior_space = true;
    for (index, ch) in line.char_indices() {
        if prior_space && matches!(ch, ';' | '#') {
            return &line[..index];
        }
        prior_space = crate::cli::js_space(ch);
    }
    line
}

fn parse_section(header: &str, config: bool) -> Option<String> {
    if let Some((group, name)) = header.split_once(crate::cli::js_space) {
        let name = name
            .strip_prefix('"')
            .and_then(|name| name.strip_suffix('"'))
            .or_else(|| {
                name.strip_prefix('\'')
                    .and_then(|name| name.strip_suffix('\''))
            })
            .unwrap_or(name);
        if valid_name(name, false) {
            return match group {
                "profile" if config => Some(name.to_owned()),
                "sso-session" => Some(format!("{group}.{name}")),
                "profile" | "services" if !config => Some(format!("{group}.{name}")),
                _ => None,
            };
        }
    }
    if config {
        (header == "default").then(|| header.to_owned())
    } else {
        Some(header.to_owned())
    }
}

fn valid_name(name: &str, selection: bool) -> bool {
    !name.is_empty()
        && (!selection || name.len() <= 64)
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || b"_@+.%:-".contains(&byte)
                || if selection {
                    byte == b' '
                } else {
                    byte == b'/'
                }
        })
}
