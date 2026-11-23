use std::path::Path;

use reqwest::Url;
use serde_json::Value;

mod cloudflared {
    include!("../app/src/cloudflared_install.rs");

    pub(super) async fn run(bin: &Path, url: Url, format: &str, mode: &str) -> Value {
        let asset = match format {
            "linux" => Asset {
                name: "cloudflared-linux-amd64",
                format: Format::Elf,
            },
            "windows" => Asset {
                name: "cloudflared-windows-amd64.exe",
                format: Format::Pe,
            },
            "darwin" => Asset {
                name: "cloudflared-darwin-amd64.tgz",
                format: Format::MachO,
            },
            _ => panic!("unknown platform fixture"),
        };
        let release = asset.url();
        assert!(asset.allows(&release));
        assert!(
            asset.allows(
                &Url::parse(&format!(
                    "https://github.com/cloudflare/cloudflared/releases/download/2026.11.0/{}",
                    asset.name,
                ))
                .unwrap()
            )
        );
        for host in [
            "release-assets.githubusercontent.com",
            "objects.githubusercontent.com",
        ] {
            assert!(asset.allows(&Url::parse(&format!(
                "https://{host}/github-production-release-asset-2e65be/1/fixture?sig=synthetic",
            )).unwrap()));
        }
        for url in [
            "http://github.com/cloudflare/cloudflared/releases/download/2026.11.0/cloudflared-linux-amd64",
            "https://github.com/cloudflare/another/releases/download/2026.11.0/cloudflared-linux-amd64",
            "https://github.com:8443/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64",
            "https://github.com@untrusted.invalid/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64",
            "https://release-assets.githubusercontent.com.untrusted.invalid/github-production-release-asset-2e65be/1/fake",
            "https://release-assets.githubusercontent.com/unrelated/path",
        ] {
            assert!(!asset.allows(&Url::parse(url).unwrap()), "{url}");
        }
        assert!(
            Asset::native()
                .unwrap()
                .allows(&Asset::native().unwrap().url())
        );

        assert_eq!(url.host_str(), Some("127.0.0.1"));
        let installer = Installer::new();
        assert_eq!(
            installer.status(),
            json!({"downloading":false,"progress":0})
        );
        assert!(installer.installed(&bin.join("unavailable")).is_none());
        let (queued, mut pre_cancelled) = watch::channel(0);
        queued.send(1).unwrap();
        assert_eq!(
            installer.ensure(bin, &mut pre_cancelled).await,
            Err(CANCELLED)
        );
        let (signal, mut cancel) = watch::channel(0);
        let mut progress = installer.progress.subscribe();
        let origin = url.origin();
        let attempt = installer.ensure_from(
            bin,
            asset,
            url,
            |url| url.origin() == origin && url.path().starts_with("/release/"),
            &mut cancel,
        );
        let trigger = async {
            if mode == "cancel" {
                loop {
                    progress.changed().await.unwrap();
                    if matches!(*progress.borrow_and_update(), Progress::Downloading(value) if value > 0)
                    {
                        assert!(installer.status()["downloading"] == true);
                        signal.send(1).unwrap();
                        break;
                    }
                }
            }
        };
        let (result, ()) = tokio::join!(attempt, trigger);
        drop(signal);
        match result {
            Ok(path) => {
                json!({"file":path.file_name().unwrap().to_str().unwrap(), "status":installer.status()})
            }
            Err(error) => json!({"error":error, "status":installer.status()}),
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    let result: Value = cloudflared::run(
        Path::new(&args[1]),
        Url::parse(&args[2]).unwrap(),
        &args[3],
        &args[4],
    )
    .await;
    println!("{result}");
}
