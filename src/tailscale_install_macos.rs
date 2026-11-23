use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use super::super::invoke;
use super::package::Package;

const FORMULA_LIMIT: Duration = Duration::from_secs(900);
const SIGNATURE_LIMIT: Duration = Duration::from_secs(30);
const PUBLISHER_TEAM: &str = " (W5364U7YZB)";

pub(in super::super) fn brew() -> Option<PathBuf> {
    ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
        .into_iter()
        .find_map(|path| {
            let path = Path::new(path);
            let resolved = path.canonicalize().ok()?;
            let metadata = resolved.metadata().ok()?;
            (resolved.starts_with("/opt/homebrew") || resolved.starts_with("/usr/local"))
                .then_some(())
                .filter(|_| {
                    metadata.is_file()
                        && metadata.permissions().mode() & 0o111 != 0
                        && metadata.permissions().mode() & 0o002 == 0
                })
                .map(|_| path.to_owned())
        })
}

pub(in super::super) fn installed() -> Option<PathBuf> {
    [
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
        "/opt/homebrew/opt/tailscale/bin/tailscale",
        "/usr/local/opt/tailscale/bin/tailscale",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| {
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

pub(super) async fn install_brew(brew: &Path) -> Result<(), &'static str> {
    let mut child = Command::new(brew)
        .args(["install", "tailscale"])
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "Could not start Homebrew")?;
    match tokio::time::timeout(FORMULA_LIMIT, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(_) => Err("Homebrew could not install Tailscale"),
        Err(_) => Err("Homebrew timed out while installing Tailscale"),
    }
}

pub(super) async fn verify(package: &Package) -> Result<(), &'static str> {
    let failure = "The Tailscale package signature could not be verified";
    let path = package.path().to_str().ok_or(failure)?;
    let signature = invoke(
        Path::new("/usr/sbin/pkgutil"),
        None,
        &["--check-signature", path],
        SIGNATURE_LIMIT,
    )
    .await
    .ok_or(failure)?;
    if !signature.success || !valid_publisher(&String::from_utf8_lossy(&signature.stdout)) {
        return Err(failure);
    }
    let gatekeeper = invoke(
        Path::new("/usr/sbin/spctl"),
        None,
        &["--assess", "--type", "install", path],
        SIGNATURE_LIMIT,
    )
    .await
    .ok_or(failure)?;
    if gatekeeper.success {
        Ok(())
    } else {
        Err(failure)
    }
}

fn valid_publisher(output: &str) -> bool {
    let leaf = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("1. "));
    let name = leaf
        .and_then(|line| line.strip_prefix("Developer ID Installer: "))
        .and_then(|subject| subject.strip_suffix(PUBLISHER_TEAM));
    matches!(name, Some("Tailscale Inc." | "Tailscale, Inc."))
}

pub(super) async fn open_installer(package: Package) -> Result<(), &'static str> {
    let (sender, result) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("9router-tailscale-pkg".into())
        .spawn(move || {
            let installer = std::process::Command::new("/usr/bin/open")
                .args([
                    "-n",
                    "-W",
                    "-a",
                    "/System/Library/CoreServices/Installer.app",
                    "--",
                ])
                .arg(package.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let status = if installer.is_ok_and(|status| status.success()) {
                Ok(())
            } else {
                Err("Could not open or complete macOS Installer")
            };
            let _ = sender.send(status);
        })
        .map_err(|_| "Could not start the macOS Installer task")?;
    result
        .await
        .map_err(|_| "macOS Installer task ended unexpectedly")?
}
