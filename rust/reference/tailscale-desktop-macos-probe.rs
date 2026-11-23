use std::path::Path;
use std::time::Duration;

struct Output {
    success: bool,
    stdout: Vec<u8>,
}

async fn invoke(
    binary: &Path,
    socket: Option<&Path>,
    args: &[&str],
    _: Duration,
) -> Option<Output> {
    assert!(socket.is_none());
    let path = Path::new(args.last()?);
    assert_eq!(path.file_name()?.to_str()?, "Tailscale.pkg");
    assert_eq!(std::fs::read(path).ok()?, b"xar!fixture package only");
    let result = std::env::var("TAILSCALE_SIGNATURE_FIXTURE").ok()?;
    match binary.to_str()? {
        "/usr/sbin/pkgutil" => {
            assert_eq!(args[0], "--check-signature");
            println!("pkgutil");
            let publisher = if result == "wrong-team" { "W5364U7YZA" } else { "W5364U7YZB" };
            Some(Output {
                success: result != "unsigned",
                stdout: format!("Certificate Chain:\n  1. Developer ID Installer: Tailscale Inc. ({publisher})\n").into_bytes(),
            })
        }
        "/usr/sbin/spctl" => {
            assert_eq!(&args[..args.len()-1], ["--assess", "--type", "install"]);
            println!("spctl");
            Some(Output { success: result != "gatekeeper-denied", stdout: Vec::new() })
        }
        _ => panic!("unexpected platform command"),
    }
}

mod desktop {
    mod package {
        include!("../app/src/tailscale_install_package.rs");
    }
    mod macos {
        include!("../app/src/tailscale_install_macos.rs");
    }

    pub async fn exercise(mode: &str, brew: &std::path::Path) -> Result<(), &'static str> {
        if mode == "brew" {
            return macos::install_brew(brew).await;
        }
        let (sent, _received) = tokio::sync::mpsc::channel(12);
        let downloaded = package::fetch(package::Asset::Mac, &sent).await?;
        match macos::verify(&downloaded).await {
            Ok(()) => println!("trusted"),
            Err(_) => println!("rejected"),
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), &'static str> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [mode, brew] = args.as_slice() else { return Err("missing fixture arguments") };
    desktop::exercise(mode, Path::new(brew)).await
}
