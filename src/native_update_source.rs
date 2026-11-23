use std::cmp::Ordering;
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Client;
use ring::signature::{ED25519, UnparsedPublicKey};
use semver::Version;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use url::Url;

const MANIFEST_LIMIT: usize = 32 * 1024;
const PAYLOAD_LIMIT: u64 = 256 * 1024 * 1024;
const CHANNEL: &str = "preview";
const TARGET: &str = env!("NINEROUTER_BUILD_TARGET");
const CHECK_CACHE: Duration = Duration::from_secs(60);
pub(crate) const CHILD: &str = "NINEROUTER_NATIVE_UPDATE_CHILD";

pub(super) static SOURCE: LazyLock<Result<Option<Source>, Error>> =
    LazyLock::new(Source::configured);
static CHECK: Mutex<Option<Cached>> = Mutex::const_new(None);

struct Cached {
    checked: Instant,
    release: Option<Release>,
}

async fn discover_version() -> (Option<String>, bool) {
    let mut check = CHECK.lock().await;
    if check
        .as_ref()
        .is_none_or(|check| check.checked.elapsed() >= CHECK_CACHE)
    {
        let release = match &*SOURCE {
            Ok(Some(source)) => source.latest().await.ok(),
            _ => None,
        };
        *check = Some(Cached {
            checked: Instant::now(),
            release,
        });
    }
    let release = check
        .as_ref()
        .and_then(|check| check.release.as_ref())
        .filter(|release| release.valid());
    (
        release.map(|release| release.version.to_string()),
        release.is_some_and(Release::available),
    )
}

pub(crate) fn discover_at_startup(skip: bool) {
    if skip || !matches!(&*SOURCE, Ok(Some(_))) {
        return;
    }
    tokio::spawn(async {
        if let (Some(latest), true) = discover_version().await {
            crate::console_logs::print_terminal(&format!(
                "Signed preview update available: v{latest}"
            ));
        }
    });
}

pub(crate) async fn version() -> Response {
    let (latest, available) = discover_version().await;
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "currentVersion": current_version().to_string(),
            "latestVersion": latest,
            "hasUpdate": available,
        })),
    )
        .into_response()
}

static VERSION: LazyLock<Version> =
    LazyLock::new(|| Version::parse(env!("CARGO_PKG_VERSION")).expect("semantic package version"));

pub(super) fn current_version() -> &'static Version {
    &VERSION
}

pub(super) struct Source {
    http: Client,
    manifest: Url,
    verify: [u8; 32],
}

pub(super) struct Release {
    pub(super) version: Version,
    artifact: Url,
    size: u64,
    checksum: [u8; 32],
    expiry: i64,
}

#[derive(Clone, Copy)]
pub(super) enum Error {
    Configuration,
    Unavailable,
    InvalidManifest,
    InvalidArtifact,
    Storage,
}

impl Error {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Configuration => "Native update source configuration is invalid",
            Self::Unavailable => "The native release source did not respond",
            Self::InvalidManifest => "The native release manifest is invalid or expired",
            Self::InvalidArtifact => "The native release artifact failed verification",
            Self::Storage => "The native update could not be written to staging",
        }
    }
}

impl Source {
    pub(super) fn configured() -> Result<Option<Self>, Error> {
        let url = std::env::var("NINEROUTER_NATIVE_UPDATE_MANIFEST").ok();
        let key = std::env::var("NINEROUTER_NATIVE_UPDATE_VERIFY_KEY").ok();
        let (url, key) = match (url, key) {
            (None, None) => return Ok(None),
            (Some(url), Some(key)) => (url, key),
            _ => return Err(Error::Configuration),
        };
        let manifest = Url::parse(&url).map_err(|_| Error::Configuration)?;
        if !trusted_endpoint(&manifest) {
            return Err(Error::Configuration);
        }
        let verify = decode(&key)
            .and_then(|key| key.try_into().ok())
            .ok_or(Error::Configuration)?;
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|_| Error::Configuration)?;
        Ok(Some(Self {
            http,
            manifest,
            verify,
        }))
    }

    pub(super) async fn latest(&self) -> Result<Release, Error> {
        let mut response = self
            .http
            .get(self.manifest.clone())
            .timeout(Duration::from_secs(8))
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        if !response.status().is_success() {
            return Err(Error::Unavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
            if chunk.len() > MANIFEST_LIMIT - bytes.len() {
                return Err(Error::InvalidManifest);
            }
            bytes.extend_from_slice(&chunk);
        }
        self.verify(&bytes).ok_or(Error::InvalidManifest)
    }

    fn verify(&self, envelope: &[u8]) -> Option<Release> {
        let envelope: Value = serde_json::from_slice(envelope).ok()?;
        let payload = decode(envelope["payload"].as_str()?)?;
        let signature = decode(envelope["signature"].as_str()?)?;
        UnparsedPublicKey::new(&ED25519, self.verify)
            .verify(&payload, &signature)
            .ok()?;
        let signed: Value = serde_json::from_slice(&payload).ok()?;
        if signed["schema"] != "9router-native-update/v1"
            || signed["channel"] != CHANNEL
            || signed["target"] != TARGET
        {
            return None;
        }
        let artifact = self.manifest.join(signed["artifact"].as_str()?).ok()?;
        if artifact.origin() != self.manifest.origin() || !trusted_endpoint(&artifact) {
            return None;
        }
        let size = signed["size"]
            .as_u64()
            .filter(|size| (1..=PAYLOAD_LIMIT).contains(size))?;
        let checksum = sha256(signed["sha256"].as_str()?)?;
        let release = Release {
            version: Version::parse(signed["version"].as_str()?).ok()?,
            artifact,
            size,
            checksum,
            expiry: signed["expiresAt"].as_i64()?,
        };
        release.valid().then_some(release)
    }

    pub(super) async fn download(&self, release: &Release, staging: &Path) -> Result<(), Error> {
        let mut response = self
            .http
            .get(release.artifact.clone())
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length != release.size)
        {
            return Err(Error::InvalidArtifact);
        }
        let mut file = tokio::fs::File::create_new(staging)
            .await
            .map_err(|_| Error::Storage)?;
        let mut size = 0;
        let mut digest = Sha256::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
            size += chunk.len() as u64;
            if size > release.size {
                return Err(Error::InvalidArtifact);
            }
            digest.update(&chunk);
            file.write_all(&chunk).await.map_err(|_| Error::Storage)?;
        }
        let checksum: [u8; 32] = digest.finalize().into();
        if size != release.size || checksum != release.checksum || !release.valid() {
            return Err(Error::InvalidArtifact);
        }
        file.sync_all().await.map_err(|_| Error::Storage)
    }
}

impl Release {
    #[cfg(windows)]
    pub(super) fn artifact_identity(&self) -> (u64, [u8; 32], i64) {
        (self.size, self.checksum, self.expiry)
    }

    pub(super) fn valid(&self) -> bool {
        self.expiry > chrono::Utc::now().timestamp()
    }

    pub(super) fn available(&self) -> bool {
        self.valid() && self.version.cmp_precedence(current_version()) == Ordering::Greater
    }
}

fn decode(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value).ok()
}

fn sha256(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

fn trusted_endpoint(url: &Url) -> bool {
    let loopback_fixture = std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        && url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1");
    (url.scheme() == "https" || loopback_fixture)
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}
