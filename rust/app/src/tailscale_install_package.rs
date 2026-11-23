use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::response::sse::Event;
use reqwest::{Client, Url, header};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

const HOST: &str = "pkgs.tailscale.com";
const PREFIX: &str = "/stable/";
const MAXIMUM_BYTES: u64 = 160 * 1024 * 1024;
const REDIRECTS: usize = 4;
pub(super) type Events = mpsc::Sender<Result<Event, Infallible>>;

#[derive(Clone, Copy)]
pub(super) enum Asset {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Mac,
    #[cfg_attr(not(windows), allow(dead_code))]
    Windows(&'static str),
}

impl Asset {
    fn name(self, version: &str) -> String {
        match self {
            Self::Mac => format!("Tailscale-{version}-macos.pkg"),
            Self::Windows(architecture) => format!("tailscale-setup-{version}-{architecture}.msi"),
        }
    }

    fn parts(self) -> (&'static str, &'static str) {
        match self {
            Self::Mac => ("Tailscale-", "-macos.pkg"),
            Self::Windows("amd64") => ("tailscale-setup-", "-amd64.msi"),
            Self::Windows("arm64") => ("tailscale-setup-", "-arm64.msi"),
            Self::Windows(_) => ("tailscale-setup-", "-x86.msi"),
        }
    }

    fn local_name(self) -> &'static str {
        match self {
            Self::Mac => "Tailscale.pkg",
            Self::Windows(_) => "Tailscale.msi",
        }
    }

    fn magic(self) -> &'static [u8] {
        match self {
            Self::Mac => b"xar!",
            Self::Windows(_) => b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1",
        }
    }

    fn allows(self, url: &Url) -> bool {
        if url.scheme() != "https"
            || url.host_str() != Some(HOST)
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
        {
            return false;
        }
        let (prefix, suffix) = self.parts();
        let Some(version) = url
            .path()
            .strip_prefix(PREFIX)
            .and_then(|path| path.strip_prefix(prefix))
            .and_then(|asset| asset.strip_suffix(suffix))
        else {
            return false;
        };
        version == "latest" || {
            let parts: Vec<_> = version.split('.').collect();
            parts.len() == 3
                && parts.iter().all(|part| {
                    !part.is_empty() && part.bytes().all(|digit| digit.is_ascii_digit())
                })
        }
    }
}

pub(super) struct Package {
    _temporary: TempDir,
    path: PathBuf,
}

impl Package {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(windows)]
    pub(super) fn directory(&self) -> &Path {
        self._temporary.path()
    }
}

pub(super) async fn fetch(asset: Asset, events: &Events) -> Result<Package, &'static str> {
    let filename = asset.name("latest");
    let fixture = std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1");
    let origin = if fixture {
        format!("http://127.0.0.1:52989/tailscale-desktop/{filename}")
    } else {
        format!("https://{HOST}{PREFIX}{filename}")
    };
    let mut url = Url::parse(&origin).map_err(|_| "Invalid Tailscale package URL")?;
    let client = Client::builder()
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|_| "Could not initialize the Tailscale download")?;
    progress(events, "Downloading the official Tailscale package...").await;
    let mut response = {
        let mut remaining = REDIRECTS;
        loop {
            let response = client
                .get(url.clone())
                .send()
                .await
                .map_err(|_| "Tailscale package download failed")?;
            if response.status().is_redirection() {
                let destination = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|header| header.to_str().ok())
                    .and_then(|header| url.join(header).ok())
                    .filter(|target| !fixture && asset.allows(target))
                    .ok_or("Tailscale package download returned an unexpected redirect")?;
                remaining = remaining
                    .checked_sub(1)
                    .ok_or("Too many Tailscale package redirects")?;
                url = destination;
                continue;
            }
            if response.status() != reqwest::StatusCode::OK {
                return Err("Tailscale package download failed");
            }
            break response;
        }
    };
    if response
        .content_length()
        .is_some_and(|size| size > MAXIMUM_BYTES)
    {
        return Err("Tailscale package exceeded the allowed download size");
    }
    let stage = tempfile::Builder::new()
        .prefix("9router-tailscale-")
        .tempdir()
        .map_err(|_| "Could not create the private Tailscale staging directory")?;
    let path = stage.path().join(asset.local_name());
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .await
        .map_err(|_| "Could not write the Tailscale package")?;
    let mut total = 0;
    let mut initial = Vec::new();
    let magic = asset.magic();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Tailscale package download was interrupted")?
    {
        total += chunk.len() as u64;
        if total > MAXIMUM_BYTES {
            return Err("Tailscale package exceeded the allowed download size");
        }
        if initial.len() < magic.len() {
            initial.extend_from_slice(&chunk[..chunk.len().min(magic.len() - initial.len())]);
            if !magic.starts_with(&initial) {
                return Err("Tailscale package format is invalid");
            }
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| "Could not write the Tailscale package")?;
    }
    if initial != magic {
        return Err("Tailscale package format is invalid");
    }
    file.sync_all()
        .await
        .map_err(|_| "Could not flush the Tailscale package")?;
    Ok(Package {
        _temporary: stage,
        path,
    })
}

pub(super) async fn progress(events: &Events, message: &'static str) {
    emit(events, "progress", json!({"message":message})).await;
}

pub(super) async fn emit(events: &Events, name: &'static str, data: Value) {
    let _ = events
        .send(Ok(Event::default().event(name).data(data.to_string())))
        .await;
}
