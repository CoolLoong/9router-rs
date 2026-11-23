use std::io;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::Duration;

use tokio::sync::Notify;

#[path = "../app/src/cli_macos_launch.rs"]
mod launcher;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Quit,
    Hide,
    Tray,
    EarlyShutdown,
    FinishBeforeRunLoop,
    NoTray,
}

struct Fixture {
    scenario: Scenario,
    trace: Vec<&'static str>,
}

static MAIN: OnceLock<ThreadId> = OnceLock::new();
static STATE: Mutex<Fixture> = Mutex::new(Fixture { scenario: Scenario::Quit, trace: Vec::new() });
static CHANGED: Condvar = Condvar::new();

fn observe(event: &'static str) {
    STATE.lock().unwrap().trace.push(event);
    CHANGED.notify_all();
}

fn scenario() -> Scenario {
    STATE.lock().unwrap().scenario
}

fn wait_for(event: &str) {
    let (state, _) = CHANGED
        .wait_timeout_while(STATE.lock().unwrap(), Duration::from_secs(2), |state| {
            !state.trace.contains(&event)
        })
        .unwrap();
    assert!(state.trace.contains(&event), "{event}: {:?}", state.trace);
}

mod cli_tui {
    use std::{io, net::SocketAddr, path::Path};

    pub enum Exit { Quit, Hide }

    pub async fn run(_: SocketAddr, _: &Path) -> io::Result<Exit> {
        super::observe("terminal");
        match super::scenario() {
            super::Scenario::Quit => Ok(Exit::Quit),
            super::Scenario::EarlyShutdown => std::future::pending().await,
            _ => Ok(Exit::Hide),
        }
    }
}

mod cli_tray_macos {
    use std::{io, net::SocketAddr, sync::Arc};
    use tokio::sync::Notify;

    pub struct TraySession(Arc<Notify>);

    pub fn start(_: SocketAddr, shutdown: Arc<Notify>) -> Option<TraySession> {
        assert_eq!(std::thread::current().id(), *super::MAIN.get().unwrap());
        super::observe("register");
        if super::scenario() == super::Scenario::NoTray {
            shutdown.notify_one();
            None
        } else {
            Some(TraySession(shutdown))
        }
    }

    impl TraySession {
        pub fn enable_autostart(&self) -> io::Result<()> {
            super::observe("autostart");
            if super::scenario() == super::Scenario::FinishBeforeRunLoop {
                self.0.notify_one();
                super::wait_for("stop-appkit");
            }
            Ok(())
        }

        pub fn run(&self) {
            assert_eq!(std::thread::current().id(), *super::MAIN.get().unwrap());
            super::observe("run-appkit");
            self.0.notify_one();
            super::wait_for("stop-appkit");
        }
    }

    pub struct StopOnDrop;

    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            super::observe("stop-appkit");
        }
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    MAIN.set(std::thread::current().id()).unwrap();
    for case in [
        Scenario::Quit, Scenario::Hide, Scenario::Tray, Scenario::EarlyShutdown,
        Scenario::FinishBeforeRunLoop, Scenario::NoTray,
    ] {
        *STATE.lock().unwrap() = Fixture { scenario: case, trace: Vec::new() };
        let terminal = (case != Scenario::Tray).then_some(Path::new("/ephemeral"));
        let shutdown = Arc::new(Notify::new());
        let server_shutdown = shutdown.clone();
        let serve = async move {
            if case != Scenario::EarlyShutdown {
                server_shutdown.notified().await;
            }
            observe("served-and-stopped");
            Ok(())
        };
        launcher::run(serve, "127.0.0.1:20128".parse().unwrap(), terminal, shutdown).await?;
        let trace = &STATE.lock().unwrap().trace;
        match case {
            Scenario::Quit => assert_eq!(trace, &["terminal","served-and-stopped"]),
            Scenario::Hide => assert_eq!(trace, &[
                "terminal","register","autostart","run-appkit","served-and-stopped","stop-appkit",
            ]),
            Scenario::Tray => assert_eq!(trace, &[
                "register","run-appkit","served-and-stopped","stop-appkit",
            ]),
            Scenario::EarlyShutdown => assert!(!trace.contains(&"register") && trace.contains(&"served-and-stopped")),
            Scenario::FinishBeforeRunLoop => assert_eq!(trace, &[
                "terminal","register","autostart","served-and-stopped","stop-appkit","run-appkit",
            ]),
            Scenario::NoTray => assert_eq!(trace, &["terminal","register","served-and-stopped"]),
        }
    }
    println!("macOS launch handoff passed six synthetic window-main-thread, autostart and shutdown scenarios.");
    Ok(())
}
