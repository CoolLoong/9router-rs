#[path = "../../app/src/cli_tray.rs"]
mod cli_tray;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;
use zbus::Proxy;
use zbus::zvariant::OwnedValue;

const PORT: u16 = 20128;

struct Watcher(mpsc::UnboundedSender<String>);

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, name: &str) {
        self.0.send(name.to_owned()).expect("watcher receiver");
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let scenario = std::env::args().nth(1).expect("scenario");
    let (registered, mut registration) = mpsc::unbounded_channel();
    let watcher = if matches!(scenario.as_str(), "headless" | "no-watcher") {
        None
    } else {
        Some(
            zbus::connection::Builder::session()
                .unwrap()
                .name("org.kde.StatusNotifierWatcher")
                .unwrap()
                .serve_at("/StatusNotifierWatcher", Watcher(registered))
                .unwrap()
                .build()
                .await
                .unwrap(),
        )
    };
    if scenario == "hide" {
        cli_tray::enable_autostart().unwrap();
    }
    let mut tray = cli_tray::start(PORT, "0.0.0.0").await;
    let Some(watcher) = watcher else {
        let stopped = tokio::time::timeout(Duration::from_millis(100), tray.wait())
            .await
            .is_ok();
        tray.stop().await;
        println!("{}", json!({"ready":false,"quit":stopped}));
        return;
    };
    let name = registration.recv().await.expect("registered tray");
    let item = Proxy::new(
        &watcher,
        name.as_str(),
        "/StatusNotifierItem",
        "org.kde.StatusNotifierItem",
    )
    .await
    .unwrap();
    let menu = Proxy::new(
        &watcher,
        name.as_str(),
        "/MenuBar",
        "com.canonical.dbusmenu",
    )
    .await
    .unwrap();
    type Pixmaps = Vec<(i32, i32, Vec<u8>)>;
    let tooltip: (String, Pixmaps, String, String) = item.get_property("ToolTip").await.unwrap();
    let pixels: Pixmaps = item.get_property("IconPixmap").await.unwrap();
    let choices = choices(&menu).await;
    let mut result = json!({
        "ready": true,
        "tooltip": tooltip.2,
        "icon": pixels.into_iter().map(|(w,h,bytes)| json!([w,h,bytes.len()])).collect::<Vec<_>>(),
        "menu": choices.iter().map(|(_,title,enabled)| json!([title, enabled])).collect::<Vec<_>>(),
    });

    if matches!(scenario.as_str(), "autostart" | "pre-enabled") {
        click(&menu, choices[2].0).await;
        result["autostart"] = json!(self::choices(&menu).await[2].1);
    }
    if scenario == "dashboard-quit" {
        click(&menu, choices[1].0).await;
        let opened = Path::new("opened-dashboard");
        let url = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(url) = fs::read_to_string(opened)
                    && !url.is_empty()
                {
                    break url;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dashboard command");
        result["opened"] = url.trim().into();
        click(&menu, choices[3].0).await;
        result["quit"] = json!(
            tokio::time::timeout(Duration::from_secs(3), tray.wait())
                .await
                .is_ok()
        );
    }
    tray.stop().await;
    let bus = zbus::fdo::DBusProxy::new(&watcher).await.unwrap();
    result["removed"] = json!(
        !bus.name_has_owner(name.as_str().try_into().unwrap())
            .await
            .unwrap()
    );
    println!("{result}");
}

async fn choices(menu: &Proxy<'_>) -> Vec<(i32, String, bool)> {
    let choices: Vec<(i32, HashMap<String, OwnedValue>)> = menu
        .call(
            "GetGroupProperties",
            &(Vec::<i32>::new(), vec!["label", "enabled"]),
        )
        .await
        .expect("tray menu");
    let mut choices: Vec<_> = choices
        .into_iter()
        .filter_map(|(id, data)| {
            let title = <&str>::try_from(data.get("label")?).ok()?.to_owned();
            let enabled = data
                .get("enabled")
                .and_then(|value| bool::try_from(value).ok())
                .unwrap_or(true);
            Some((id, title, enabled))
        })
        .collect();
    choices.sort_by_key(|(id, _, _)| *id);
    choices
}

async fn click(menu: &Proxy<'_>, id: i32) {
    let data = OwnedValue::from(0i32);
    let _: () = menu
        .call("Event", &(id, "clicked", data, 0u32))
        .await
        .expect("menu event");
}
