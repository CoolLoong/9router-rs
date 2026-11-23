use std::path::PathBuf;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{
    IntoResponse, Response,
    sse::{KeepAlive, Sse},
};
use serde_json::json;
use tokio::sync::mpsc;

use super::{PROBE, Tailscale, approval_url, error, invoke, system_status};

#[cfg(target_os = "macos")]
#[path = "tailscale_install_macos.rs"]
mod macos;
#[path = "tailscale_install_package.rs"]
mod package;
#[cfg(windows)]
#[path = "tailscale_install_windows.rs"]
mod windows;

use package::{Asset, Events, emit, fetch, progress};

#[cfg(target_os = "macos")]
pub(super) fn brew_available() -> bool {
    macos::brew().is_some()
}

pub(super) fn installed() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return macos::installed();
    #[cfg(windows)]
    return windows::installed();
}

pub(super) async fn install(State(state): State<Tailscale>) -> Response {
    let Ok(operation) = state.operation.try_lock_owned() else {
        return error(
            StatusCode::CONFLICT,
            "Another gateway Funnel operation is in progress",
        );
    };
    let (sender, receiver) = mpsc::channel(12);
    tokio::spawn(async move {
        let _operation = operation;
        match run(&sender).await {
            Ok(reboot) => {
                let auth_url = if let Some(cli) = installed() {
                    system_status(&cli)
                        .await
                        .and_then(|status| status["AuthURL"].as_str().and_then(approval_url))
                } else {
                    None
                };
                let mut result = json!({"success":true,"authUrl":auth_url});
                if reboot {
                    result["rebootRequired"] = json!(true);
                }
                emit(&sender, "done", result).await;
            }
            Err(reason) => emit(&sender, "error", json!({"error":reason})).await,
        }
    });
    Sse::new(futures_util::stream::unfold(
        receiver,
        |mut events| async move { events.recv().await.map(|event| (event, events)) },
    ))
    .keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("working"),
    )
    .into_response()
}

async fn run(events: &Events) -> Result<bool, &'static str> {
    let reboot = if installed().is_none() {
        #[cfg(target_os = "macos")]
        {
            if let Some(brew) = macos::brew() {
                progress(events, "Installing Tailscale with Homebrew...").await;
                active(events)?;
                macos::install_brew(&brew).await?;
            } else {
                let package = download(Asset::Mac, events).await?;
                progress(events, "Verifying the Tailscale package signature...").await;
                macos::verify(&package).await?;
                progress(events, "Complete setup in the macOS Installer window...").await;
                active(events)?;
                macos::open_installer(package).await?;
            }
            false
        }
        #[cfg(windows)]
        {
            let architecture = match std::env::consts::ARCH {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                "x86" => "x86",
                _ => {
                    return Err(
                        "Tailscale does not have a Windows installer for this architecture",
                    );
                }
            };
            let package = download(Asset::Windows(architecture), events).await?;
            progress(events, "Verifying the Tailscale installer signature. Approve Windows Installer when prompted...").await;
            active(events)?;
            windows::install(package, events.clone()).await?
        }
    } else {
        false
    };
    if !reboot {
        progress(events, "Verifying the Tailscale installation...").await;
        let cli = installed().ok_or("Tailscale was not installed or setup was canceled")?;
        if !invoke(&cli, None, &["version"], PROBE)
            .await
            .is_some_and(|reply| reply.success)
        {
            return Err("The installed Tailscale executable did not respond");
        }
    }
    Ok(reboot)
}

fn active(events: &Events) -> Result<(), &'static str> {
    if events.is_closed() {
        Err("Tailscale setup was canceled")
    } else {
        Ok(())
    }
}

async fn download(asset: Asset, events: &Events) -> Result<package::Package, &'static str> {
    tokio::select! {
        _ = events.closed() => Err("Tailscale setup was canceled"),
        result = fetch(asset, events) => result,
    }
}
