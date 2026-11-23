use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use tokio::sync::Notify;

use crate::{cli_tray_macos, cli_tui};

const WAITING: u8 = 0;
const APPKIT: u8 = 1;
const FINISHED: u8 = 2;

pub async fn run(
    serve: impl Future<Output = io::Result<()>> + Send + 'static,
    address: SocketAddr,
    terminal: Option<&Path>,
    shutdown: Arc<Notify>,
) -> io::Result<()> {
    let handoff = Arc::new(AtomicU8::new(WAITING));
    let completed = ServerFinished(handoff.clone());
    let mut worker = tokio::spawn(async move {
        let _completed = completed;
        serve.await
    });
    if let Some(directory) = terminal {
        let choice = tokio::select! {
            result = &mut worker => return result.map_err(io::Error::other)?,
            result = cli_tui::run(address, directory) => result,
        };
        match choice {
            Ok(cli_tui::Exit::Hide) => (),
            Ok(cli_tui::Exit::Quit) => {
                shutdown.notify_one();
                return worker.await.map_err(io::Error::other)?;
            }
            Err(error) => {
                eprintln!("{error}");
                shutdown.notify_one();
                return worker.await.map_err(io::Error::other)?;
            }
        }
    }
    let _hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    if handoff.load(Ordering::Acquire) != FINISHED
        && let Some(tray) = cli_tray_macos::start(address, shutdown)
        && handoff
            .compare_exchange(WAITING, APPKIT, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        if terminal.is_some()
            && let Err(error) = tray.enable_autostart()
        {
            eprintln!("{error}");
        }
        tray.run();
    }
    worker.await.map_err(io::Error::other)?
}

struct ServerFinished(Arc<AtomicU8>);

impl Drop for ServerFinished {
    fn drop(&mut self) {
        if self.0.swap(FINISHED, Ordering::AcqRel) == APPKIT {
            drop(cli_tray_macos::StopOnDrop);
        }
    }
}
