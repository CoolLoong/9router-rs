use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use flate2::read::MultiGzDecoder;
use reqwest::{Client, StatusCode, Url, header};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, watch};

const MIN_BINARY: u64 = 1024 * 1024;
const MAX_BINARY: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 160 * 1024 * 1024;
const REDIRECT_LIMIT: usize = 5;
const CANCELLED: &str = "tunnel cancelled";

#[derive(Clone, Copy)]
enum Format {
    Elf,
    Pe,
    MachO,
}

#[derive(Clone, Copy)]
struct Asset {
    name: &'static str,
    format: Format,
}

impl Asset {
    fn native() -> Result<Self, &'static str> {
        use Format::{Elf, MachO, Pe};
        let (name, format) = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => ("cloudflared-linux-amd64", Elf),
            ("linux", "aarch64") => ("cloudflared-linux-arm64", Elf),
            ("windows", "x86_64") => ("cloudflared-windows-amd64.exe", Pe),
            ("windows", "x86" | "aarch64") => ("cloudflared-windows-386.exe", Pe),
            ("macos", "x86_64") => ("cloudflared-darwin-amd64.tgz", MachO),
            ("macos", "aarch64") => ("cloudflared-darwin-arm64.tgz", MachO),
            _ => return Err("Unsupported platform for cloudflared download"),
        };
        Ok(Self { name, format })
    }

    fn path(self, bin_dir: &Path) -> PathBuf {
        bin_dir.join(if matches!(self.format, Format::Pe) {
            "cloudflared.exe"
        } else {
            "cloudflared"
        })
    }

    fn url(self) -> Url {
        Url::parse(&format!(
            "https://github.com/cloudflare/cloudflared/releases/latest/download/{}",
            self.name
        ))
        .expect("fixed cloudflared release URL")
    }

    fn allows(self, url: &Url) -> bool {
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.fragment().is_some()
        {
            return false;
        }
        match url.host_str() {
            Some("github.com") => {
                let release = url.path().strip_prefix("/cloudflare/cloudflared/releases/");
                if let Some(pinned) = release.and_then(|path| path.strip_prefix("download/")) {
                    return pinned
                        .split_once('/')
                        .is_some_and(|(tag, name)| !tag.is_empty() && name == self.name);
                }
                release
                    .and_then(|path| path.strip_prefix("latest/download/"))
                    .is_some_and(|name| name == self.name)
            }
            Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com") => url
                .path()
                .strip_prefix("/github-production-release-asset-2e65be/")
                .is_some_and(|path| !path.is_empty()),
            _ => false,
        }
    }
}

#[derive(Clone, Copy)]
enum Progress {
    Idle,
    Downloading(u8),
    Complete,
}

pub(super) struct Installer {
    client: Client,
    progress: watch::Sender<Progress>,
    operation: Mutex<()>,
}

impl Installer {
    pub(super) fn new() -> Self {
        Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .connect_timeout(Duration::from_secs(15))
                .build()
                .expect("cloudflared release HTTP client"),
            progress: watch::channel(Progress::Idle).0,
            operation: Mutex::new(()),
        }
    }

    pub(super) fn status(&self) -> Value {
        let (downloading, progress) = match *self.progress.borrow() {
            Progress::Idle => (false, 0),
            Progress::Downloading(progress) => (true, progress),
            Progress::Complete => (false, 100),
        };
        json!({"downloading":downloading,"progress":progress})
    }

    pub(super) fn installed(&self, bin_dir: &Path) -> Option<PathBuf> {
        let asset = Asset::native().ok()?;
        let path = asset.path(bin_dir);
        existing(&path, asset.format).filter(executable)?;
        Some(path)
    }

    pub(super) async fn ensure(
        &self,
        bin_dir: &Path,
        cancel: &mut watch::Receiver<u64>,
    ) -> Result<PathBuf, &'static str> {
        let asset = Asset::native()?;
        self.ensure_from(bin_dir, asset, asset.url(), |url| asset.allows(url), cancel)
            .await
    }

    async fn ensure_from(
        &self,
        bin_dir: &Path,
        asset: Asset,
        url: Url,
        permitted: impl Fn(&Url) -> bool,
        cancel: &mut watch::Receiver<u64>,
    ) -> Result<PathBuf, &'static str> {
        let _guard = tokio::select! {
            biased;
            _ = cancel.changed() => return Err(CANCELLED),
            guard = self.operation.lock() => guard,
        };
        let target = asset.path(bin_dir);
        if let Some(metadata) = existing(&target, asset.format) {
            if !executable(&metadata) {
                let file = File::open(&target).map_err(|_| "Could not open cloudflared")?;
                mark_executable(&file).map_err(|_| "Could not set cloudflared permissions")?;
            }
            return Ok(target);
        }

        let progress = Transfer::start(&self.progress);
        let witness = cancel.clone();
        let prepare = async {
            tokio::fs::create_dir_all(bin_dir)
                .await
                .map_err(|_| "Could not create cloudflared binary directory")?;
            let downloaded = self.download(bin_dir, url, permitted).await?;
            let dir = bin_dir.to_owned();
            tokio::task::spawn_blocking(move || stage(downloaded, asset.format, &dir, &witness))
                .await
                .map_err(|_| "Could not prepare cloudflared binary")?
        };
        let staged = tokio::select! {
            biased;
            _ = cancel.changed() => return Err(CANCELLED),
            output = tokio::time::timeout(Duration::from_secs(120), prepare) =>
                output.map_err(|_| "Cloudflared download timed out")??,
        };
        if cancelled(cancel) {
            return Err(CANCELLED);
        }
        staged
            .persist(&target)
            .map_err(|_| "Could not install cloudflared")?;
        progress.complete();
        Ok(target)
    }

    async fn download(
        &self,
        dir: &Path,
        mut url: Url,
        permitted: impl Fn(&Url) -> bool,
    ) -> Result<NamedTempFile, &'static str> {
        let mut count = 0;
        let mut response = loop {
            if !permitted(&url) {
                return Err("Cloudflared download redirected outside the release hosts");
            }
            let response = self
                .client
                .get(url.clone())
                .header(header::ACCEPT_ENCODING, "identity")
                .send()
                .await
                .map_err(|_| "Could not download cloudflared")?;
            match response.status() {
                StatusCode::OK => break response,
                StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
                    if count < REDIRECT_LIMIT =>
                {
                    url = response
                        .headers()
                        .get(header::LOCATION)
                        .and_then(|location| location.to_str().ok())
                        .and_then(|location| url.join(location).ok())
                        .ok_or("Invalid cloudflared download redirect")?;
                    count += 1;
                }
                _ => return Err("Cloudflared release download was unavailable"),
            }
        };
        let length = response.content_length().filter(|length| *length > 0);
        if length.is_some_and(|length| length > MAX_BINARY) {
            return Err("Cloudflared download exceeded its size limit");
        }
        let file =
            NamedTempFile::new_in(dir).map_err(|_| "Could not stage cloudflared download")?;
        let handle = file
            .reopen()
            .map_err(|_| "Could not open cloudflared download")?;
        let mut writer = tokio::fs::File::from_std(handle);
        let mut size = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "Could not download cloudflared")?
        {
            size += chunk.len() as u64;
            if size > MAX_BINARY {
                return Err("Cloudflared download exceeded its size limit");
            }
            writer
                .write_all(&chunk)
                .await
                .map_err(|_| "Could not store cloudflared download")?;
            if let Some(length) = length {
                let progress = ((size * 100 + length / 2) / length).min(99) as u8;
                self.progress.send_if_modified(|state| {
                    if matches!(*state, Progress::Downloading(previous) if previous == progress) {
                        return false;
                    }
                    *state = Progress::Downloading(progress);
                    true
                });
            }
        }
        writer
            .flush()
            .await
            .map_err(|_| "Could not store cloudflared download")?;
        Ok(file)
    }
}

struct Transfer<'a> {
    progress: &'a watch::Sender<Progress>,
    success: bool,
}

impl<'a> Transfer<'a> {
    fn start(progress: &'a watch::Sender<Progress>) -> Self {
        progress.send_replace(Progress::Downloading(0));
        Self {
            progress,
            success: false,
        }
    }

    fn complete(mut self) {
        self.success = true;
    }
}

impl Drop for Transfer<'_> {
    fn drop(&mut self) {
        self.progress.send_replace(if self.success {
            Progress::Complete
        } else {
            Progress::Idle
        });
    }
}

fn existing(path: &Path, format: Format) -> Option<fs::Metadata> {
    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    valid_binary(file, &metadata, format).then_some(metadata)
}

fn valid_binary(mut file: File, metadata: &fs::Metadata, format: Format) -> bool {
    let size = metadata.len();
    if !metadata.is_file() || !(MIN_BINARY..=MAX_BINARY).contains(&size) {
        return false;
    }
    let mut magic = [0; 4];
    if file.read_exact(&mut magic).is_err() {
        return false;
    }
    match format {
        Format::Elf => magic == *b"\x7fELF",
        Format::MachO => matches!(magic, [0xcf, 0xfa, 0xed, 0xfe] | [0xce, 0xfa, 0xed, 0xfe]),
        Format::Pe if &magic[..2] == b"MZ" => {
            let mut offset = [0; 4];
            if file
                .seek(SeekFrom::Start(0x3c))
                .and_then(|_| file.read_exact(&mut offset))
                .is_err()
            {
                return false;
            }
            let offset = u32::from_le_bytes(offset) as u64;
            offset >= 64
                && offset + 4 <= size
                && file
                    .seek(SeekFrom::Start(offset))
                    .and_then(|_| file.read_exact(&mut magic))
                    .is_ok()
                && magic == *b"PE\0\0"
        }
        Format::Pe => false,
    }
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn mark_executable(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn mark_executable(_: &File) -> io::Result<()> {
    Ok(())
}

fn cancelled(cancel: &watch::Receiver<u64>) -> bool {
    cancel.has_changed().unwrap_or(true)
}

fn stage(
    downloaded: NamedTempFile,
    format: Format,
    dir: &Path,
    cancel: &watch::Receiver<u64>,
) -> Result<NamedTempFile, &'static str> {
    let candidate = if matches!(format, Format::MachO) {
        extract(&downloaded, dir, cancel)?
    } else {
        downloaded
    };
    if cancelled(cancel) {
        return Err(CANCELLED);
    }
    let file = candidate
        .reopen()
        .map_err(|_| "Could not open cloudflared binary")?;
    let metadata = file
        .metadata()
        .map_err(|_| "Could not read cloudflared binary")?;
    if !valid_binary(file, &metadata, format) {
        return Err("Cloudflared download was not a valid binary");
    }
    mark_executable(candidate.as_file())
        .and_then(|_| candidate.as_file().sync_all())
        .map_err(|_| "Could not prepare cloudflared binary")?;
    Ok(candidate)
}

fn extract(
    downloaded: &NamedTempFile,
    dir: &Path,
    cancel: &watch::Receiver<u64>,
) -> Result<NamedTempFile, &'static str> {
    let invalid = "Invalid cloudflared archive";
    let file = downloaded.reopen().map_err(|_| invalid)?;
    let stream = ArchiveSource {
        data: MultiGzDecoder::new(file),
        cancel,
    };
    let mut archive = tar::Archive::new(stream.take(MAX_ARCHIVE + 1));
    let mut binary = None;
    for entry in archive.entries().map_err(|_| invalid)? {
        let mut entry = entry.map_err(|_| invalid)?;
        let name = entry.path().map_err(|_| invalid)?;
        if name
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return Err(invalid);
        }
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        if !kind.is_file() {
            return Err(invalid);
        }
        if name
            .components()
            .filter(|part| *part != Component::CurDir)
            .eq([Component::Normal(std::ffi::OsStr::new("cloudflared"))])
        {
            if binary.is_some() || !(MIN_BINARY..=MAX_BINARY).contains(&entry.size()) {
                return Err(invalid);
            }
            let mut file =
                NamedTempFile::new_in(dir).map_err(|_| "Could not stage cloudflared binary")?;
            let size = io::copy(&mut entry, &mut file).map_err(|_| invalid)?;
            if size != entry.size() {
                return Err(invalid);
            }
            binary = Some(file);
        }
    }
    let mut remaining = archive.into_inner();
    io::copy(&mut remaining, &mut io::sink()).map_err(|_| invalid)?;
    if remaining.limit() == 0 {
        return Err("Cloudflared archive exceeded its size limit");
    }
    binary.ok_or(invalid)
}

struct ArchiveSource<'a> {
    data: MultiGzDecoder<File>,
    cancel: &'a watch::Receiver<u64>,
}

impl Read for ArchiveSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if cancelled(self.cancel) {
            return Err(io::Error::other(CANCELLED));
        }
        self.data.read(buf)
    }
}
