use std::fs;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Client, Method};
use serde_json::{Value, json};

use crate::key_identity;

#[path = "cli_tui_combos.rs"]
mod combos;
#[path = "cli_tui_input.rs"]
mod input;
#[path = "cli_tui_providers.rs"]
mod providers;
#[path = "cli_tui_tools.rs"]
mod tools;

use input::{confirm, menu, pause, prompt};

pub enum Exit {
    Quit,
    Hide,
}

pub async fn run(address: SocketAddr, data_dir: &Path) -> io::Result<Exit> {
    #[cfg(windows)]
    let _console_output =
        input::ConsoleMode::capture(windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE)?;
    match Launcher::new(address, data_dir)?.run().await {
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(Exit::Quit),
        result => result,
    }
}

pub fn available() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

struct Launcher {
    api: Api,
    dashboard: String,
}

impl Launcher {
    fn new(address: SocketAddr, data_dir: &Path) -> io::Result<Self> {
        let local = match address.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => {
                SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), address.port())
            }
            IpAddr::V6(ip) if ip.is_unspecified() => {
                SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), address.port())
            }
            _ => address,
        };
        let origin = format!("http://{local}");
        Ok(Self {
            dashboard: format!("{origin}/dashboard"),
            api: Api {
                origin,
                token: local_token(data_dir)?,
                client: Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(15))
                    .build()
                    .map_err(io::Error::other)?,
            },
        })
    }

    async fn run(&self) -> io::Result<Exit> {
        let version = env!("CARGO_PKG_VERSION");
        let title = format!("Choose Interface (v{version})");
        let labels = labels(&[
            "Web UI (Open in Browser)",
            "Terminal UI (Interactive CLI)",
            "Hide to Tray (Background)",
            "Exit",
        ]);
        let server = format!("🚀 Server: {}", self.api.origin);
        loop {
            match menu(&title, &server, &labels).await? {
                Some(0) => {
                    open_dashboard(self.dashboard.clone());
                    prompt("Press Enter to go back to menu...", false).await?;
                }
                Some(1) => self.terminal().await?,
                Some(2) => return Ok(Exit::Hide),
                _ => return Ok(Exit::Quit),
            }
        }
    }

    async fn terminal(&self) -> io::Result<()> {
        let labels = labels(&[
            "← Back",
            "Providers",
            "API Keys",
            "Combos",
            "CLI Tools",
            "Settings",
        ]);
        loop {
            let (tunnel, keys) = tokio::join!(
                self.api.get("/api/tunnel/status"),
                self.api.get("/api/keys")
            );
            let tunnel = tunnel.unwrap_or_default();
            let keys = keys.unwrap_or_default();
            let mut header = self.api.endpoint_header(&tunnel);
            for (index, key) in rows(&keys, "keys").iter().enumerate() {
                if let Some(key) = key["key"].as_str() {
                    let prefix = if index == 0 {
                        "Key:      "
                    } else {
                        "          "
                    };
                    header.push_str(&format!("\n{prefix}{}", printable(key)));
                }
            }
            if rows(&keys, "keys").is_empty() {
                header.push_str("\nKey:      No API keys yet");
            }
            match menu("📡 9Router Terminal UI", &header, &labels).await? {
                Some(1) => providers::run(&self.api).await?,
                Some(2) => keys_menu(&self.api).await?,
                Some(3) => combos::run(&self.api).await?,
                Some(4) => tools::run(&self.api).await?,
                Some(5) => settings_menu(&self.api).await?,
                _ => return Ok(()),
            }
        }
    }
}

struct Api {
    origin: String,
    client: Client,
    token: String,
}

impl Api {
    async fn get(&self, path: &str) -> Result<Value, String> {
        self.request(Method::GET, path, None).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.origin, path))
            .header("x-9r-cli-token", &self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        let status = response.status();
        let body = response.json::<Value>().await.unwrap_or_default();
        if status.is_success() {
            Ok(body)
        } else {
            Err(body["error"]
                .as_str()
                .map(printable)
                .unwrap_or_else(|| format!("HTTP {status}")))
        }
    }

    fn endpoint_header(&self, tunnel: &Value) -> String {
        if tunnel["enabled"] == true
            && let Some(url) = tunnel["publicUrl"].as_str()
        {
            return format!(
                "Endpoint: {}/v1\nTunnel:   ON ({})",
                printable(url),
                printable(tunnel["shortId"].as_str().unwrap_or_default())
            );
        }
        format!("Endpoint: {}/v1\nTunnel:   OFF (local only)", self.origin)
    }
}

fn local_token(root: &Path) -> io::Result<String> {
    if let Some(token) = key_identity::cli_token(root) {
        return Ok(token);
    }
    key_identity::raw_machine_id(root)?;
    let auth = root.join("auth");
    fs::create_dir_all(&auth)?;
    let secret = auth.join("cli-secret");
    if !secret.exists() {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        let contents = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut staged = tempfile::NamedTempFile::new_in(&auth)?;
        staged.write_all(contents.as_bytes())?;
        match staged.persist_noclobber(&secret) {
            Ok(_) => (),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.error),
        }
    }
    key_identity::cli_token(root).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Cannot load local CLI authorization",
        )
    })
}

async fn keys_menu(api: &Api) -> io::Result<()> {
    loop {
        let keys = match api.get("/api/keys").await {
            Ok(keys) => keys,
            Err(error) => return notice(Err(error), "").await,
        };
        let keys = rows(&keys, "keys");
        let mut items = labels(&["← Back", "Create New API Key"]);
        items.extend(keys.iter().map(|key| {
            format!(
                "{} ({})",
                get_text(key, "name"),
                crate::cli::mask_key(key["key"].as_str().unwrap_or_default())
            )
        }));
        match menu(
            "🔑 API Keys Management",
            &format!("Endpoint: {}/v1", api.origin),
            &items,
        )
        .await?
        {
            Some(1) => create_key(api).await?,
            Some(index) if index > 1 => key_actions(api, &keys[index - 2]).await?,
            _ => return Ok(()),
        }
    }
}

async fn create_key(api: &Api) -> io::Result<()> {
    let Some(name) = prompt("Enter key name: ", false)
        .await?
        .filter(|name| !name.is_empty())
    else {
        return Ok(());
    };
    let result = api
        .request(Method::POST, "/api/keys", Some(json!({ "name": name })))
        .await;
    match result {
        Ok(key) => {
            println!(
                "\n✅ API Key created successfully!\nKey: {}\nName: {}\nID: {}",
                get_text(&key, "key"),
                get_text(&key, "name"),
                get_text(&key, "id")
            );
            if let Some(key) = key["key"].as_str()
                && confirm("Copy key to clipboard?").await?
            {
                print_copy_result(key).await;
            }
            pause().await
        }
        Err(error) => notice(Err(error), "").await,
    }
}

async fn key_actions(api: &Api, key: &Value) -> io::Result<()> {
    let items = labels(&["← Back", "Copy to Clipboard", "Delete Key"]);
    let header = format!(
        "Name: {}\nKey: {}\nID: {}",
        get_text(key, "name"),
        get_text(key, "key"),
        get_text(key, "id")
    );
    loop {
        match menu("🔑 API Key", &header, &items).await? {
            Some(1) => {
                if let Some(key) = key["key"].as_str() {
                    print_copy_result(key).await;
                }
                pause().await?;
            }
            Some(2) if confirm("Are you sure you want to delete this key?").await? => {
                if let Some(id) = key["id"].as_str() {
                    notice(
                        api.request(Method::DELETE, &resource("/api/keys", id), None)
                            .await,
                        "API key deleted successfully",
                    )
                    .await?;
                }
                return Ok(());
            }
            Some(2) => (),
            _ => return Ok(()),
        }
    }
}

async fn settings_menu(api: &Api) -> io::Result<()> {
    loop {
        let (tunnel, settings) =
            tokio::join!(api.get("/api/tunnel/status"), api.get("/api/settings"));
        let tunnel = tunnel.unwrap_or_default();
        let settings = match settings {
            Ok(settings) => settings,
            Err(error) => return notice(Err(error), "").await,
        };
        let rtk = settings["rtkEnabled"] != false;
        let headroom = settings["headroomEnabled"] == true;
        let auth = settings["authMode"].as_str().unwrap_or("password");
        let header = format!(
            "{}\nRTK:      {} (Token Saver)\nHeadroom: {} ({})\nAuth:     {} (login mode)",
            api.endpoint_header(&tunnel),
            on_off(rtk),
            on_off(headroom),
            printable(
                settings["headroomUrl"]
                    .as_str()
                    .unwrap_or("http://localhost:8787")
            ),
            auth.to_uppercase()
        );
        let items = labels(&["← Back", "Tunnel ON", "Tunnel OFF"]);
        let items = items
            .into_iter()
            .chain([
                format!("Token Saver (RTK): {} → toggle", on_off(rtk)),
                format!("Token Saver (Headroom): {} → toggle", on_off(headroom)),
                "🔑 Reset Password to Default".into(),
                if auth == "password" {
                    "🔓 Reset Auth Mode (already password)".into()
                } else {
                    format!("🔓 Reset Auth Mode to Password (current: {auth})")
                },
            ])
            .collect::<Vec<_>>();
        match menu("⚙️ Settings", &header, &items).await? {
            Some(1) => {
                notice(
                    api.request(Method::POST, "/api/tunnel/enable", None).await,
                    "Tunnel enabled",
                )
                .await?
            }
            Some(2) => {
                notice(
                    api.request(Method::POST, "/api/tunnel/disable", None).await,
                    "Tunnel disabled",
                )
                .await?
            }
            Some(3) => {
                notice(
                    api.request(
                        Method::PATCH,
                        "/api/settings",
                        Some(json!({"rtkEnabled": !rtk})),
                    )
                    .await,
                    "Token Saver updated",
                )
                .await?
            }
            Some(4) => {
                notice(
                    api.request(
                        Method::PATCH,
                        "/api/settings",
                        Some(json!({"headroomEnabled": !headroom})),
                    )
                    .await,
                    "Headroom updated",
                )
                .await?
            }
            Some(5) => {
                if confirm("Reset dashboard password to default \"123456\"?").await? {
                    notice(
                        api.request(Method::POST, "/api/auth/reset-password", None)
                            .await,
                        "Password reset. Default: 123456",
                    )
                    .await?;
                }
            }
            Some(6) => {
                if confirm("Reset auth mode to PASSWORD (disable OIDC)?").await? {
                    notice(
                        api.request(
                            Method::PATCH,
                            "/api/settings",
                            Some(json!({"authMode":"password"})),
                        )
                        .await,
                        "Auth mode reset to password. OIDC disabled.",
                    )
                    .await?;
                }
            }
            _ => return Ok(()),
        }
    }
}

fn labels(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|label| (*label).to_owned()).collect()
}

fn rows<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value[key]
        .as_array()
        .or_else(|| value.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn get_text(value: &Value, key: &str) -> String {
    printable(value[key].as_str().unwrap_or_default())
}

fn printable(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
}

fn resource(path: &str, id: &str) -> String {
    const RESERVED: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    format!("{path}/{}", utf8_percent_encode(id, RESERVED))
}

fn on_off(on: bool) -> &'static str {
    if on { "ON" } else { "OFF" }
}

async fn notice(result: Result<Value, String>, success: &str) -> io::Result<()> {
    match result {
        Ok(_) => println!("\n✅ {success}"),
        Err(error) => println!("\n❌ {}", printable(&error)),
    }
    pause().await
}

async fn print_copy_result(value: &str) {
    let value = value.to_owned();
    let copied = tokio::task::spawn_blocking(move || copy(&value))
        .await
        .unwrap_or(false);
    println!(
        "{}",
        if copied {
            "Key copied to clipboard!"
        } else {
            "Failed to copy to clipboard"
        }
    );
}

#[cfg(target_os = "linux")]
fn copy(value: &str) -> bool {
    [
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
        ("xsel", &["--clipboard", "--input"][..]),
    ]
    .into_iter()
    .any(|(command, args)| copy_using(Command::new(command).args(args), value.as_bytes()))
}

#[cfg(target_os = "macos")]
fn copy(value: &str) -> bool {
    copy_using(&mut Command::new("/usr/bin/pbcopy"), value.as_bytes())
}

#[cfg(windows)]
fn copy(value: &str) -> bool {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut folder = [0_u16; 32768];
    let chars = unsafe { GetSystemDirectoryW(folder.as_mut_ptr(), folder.len() as u32) } as usize;
    if chars == 0 || chars >= folder.len() {
        return false;
    }
    let path = PathBuf::from(OsString::from_wide(&folder[..chars])).join("clip.exe");
    let unicode: Vec<_> = std::iter::once(0xfeff)
        .chain(value.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    copy_using(&mut Command::new(path), &unicode)
}

fn copy_using(command: &mut Command, value: &[u8]) -> bool {
    let Ok(mut child) = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut input| input.write_all(value).is_ok());
    child.wait().is_ok_and(|status| status.success()) && wrote
}

#[cfg(unix)]
fn open_dashboard(url: String) {
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        let program = "/usr/bin/open";
        #[cfg(target_os = "linux")]
        let program = "xdg-open";
        let _ = Command::new(program)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

#[cfg(windows)]
fn open_dashboard(url: String) {
    std::thread::spawn(move || {
        use std::ptr::{null, null_mut};
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let operation: Vec<_> = "open".encode_utf16().chain([0]).collect();
        let url: Vec<_> = url.encode_utf16().chain([0]).collect();
        unsafe {
            ShellExecuteW(
                null_mut(),
                operation.as_ptr(),
                url.as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            );
        }
    });
}
