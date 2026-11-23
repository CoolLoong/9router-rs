use std::fs;
use std::future::pending;
use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::Duration;

use ksni::menu::StandardItem;
use ksni::{Handle, Icon, MenuItem, ToolTip, Tray, TrayMethods};
use tempfile::NamedTempFile;
use tokio::sync::oneshot;

pub struct TraySession {
    handle: Option<Handle<RouterTray>>,
    quit: oneshot::Receiver<()>,
}

impl TraySession {
    pub fn is_registered(&self) -> bool {
        self.handle.is_some()
    }

    pub async fn wait(&mut self) {
        if (&mut self.quit).await.is_ok() {
            println!("\n👋 Shutting down from tray...");
        } else {
            pending::<()>().await;
        }
    }

    pub async fn stop(self) {
        if let Some(handle) = self.handle {
            let _ = tokio::time::timeout(Duration::from_secs(3), handle.shutdown()).await;
        }
    }
}

pub async fn start(port: u16, host: &str) -> TraySession {
    let host = match host {
        "0.0.0.0" => "localhost".to_owned(),
        "::" | "[::]" => "[::1]".to_owned(),
        host if host.contains(':') && !host.starts_with('[') => format!("[{host}]"),
        host => host.to_owned(),
    };
    let dashboard = format!("http://{host}:{port}/dashboard");
    let (quit, quit_rx) = oneshot::channel();
    let tray = RouterTray {
        port,
        dashboard: dashboard.clone(),
        autostart: autostart_path(),
        quit: Some(quit),
    };
    let handle = if graphical_session() {
        match tokio::time::timeout(Duration::from_secs(3), tray.spawn()).await {
            Ok(Ok(handle)) => {
                println!(
                    "\n💡 Router is now running in system tray. Close this terminal if you want."
                );
                println!("   Right-click tray icon to open dashboard or quit.\n");
                Some(handle)
            }
            Ok(Err(error)) => {
                unavailable(&dashboard, &error.to_string());
                None
            }
            Err(_) => {
                unavailable(
                    &dashboard,
                    "the system tray did not register within 3 seconds",
                );
                None
            }
        }
    } else {
        unavailable(
            &dashboard,
            "no graphical session (DISPLAY/WAYLAND_DISPLAY unset)",
        );
        None
    };
    TraySession {
        handle,
        quit: quit_rx,
    }
}

pub fn enable_autostart() -> io::Result<()> {
    if graphical_session()
        && let Some(path) = autostart_path()
        && !path.exists()
    {
        write_autostart(&path)?;
    }
    Ok(())
}

fn graphical_session() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

fn autostart_path() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(".config/autostart/9router.desktop"))
}

fn unavailable(dashboard: &str, error: &str) {
    println!("\n⚠️  No tray icon — the server keeps running.");
    println!("   {error}");
    println!("   Dashboard: {dashboard}\n");
}

struct RouterTray {
    port: u16,
    dashboard: String,
    autostart: Option<PathBuf>,
    quit: Option<oneshot::Sender<()>>,
}

impl Tray for RouterTray {
    fn id(&self) -> String {
        "9router".into()
    }

    fn icon_name(&self) -> String {
        if icon().is_some() {
            String::new()
        } else {
            "network-server".into()
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        icon().into_iter().cloned().collect()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: format!("9Router - Port {}", self.port),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let autostart_enabled = self.autostart.as_ref().is_some_and(|path| path.exists());
        vec![
            StandardItem {
                label: format!("9Router (Port {})", self.port),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open Dashboard".into(),
                activate: Box::new(|tray: &mut Self| {
                    let url = tray.dashboard.clone();
                    std::thread::spawn(move || {
                        let _ = Command::new("xdg-open")
                            .arg(url)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
                    });
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if autostart_enabled {
                    "✓ Auto-start Enabled"
                } else {
                    "Enable Auto-start"
                }
                .into(),
                activate: Box::new(|tray: &mut Self| {
                    if let Some(path) = &tray.autostart
                        && let Err(error) = toggle_autostart(path)
                    {
                        eprintln!("Could not update 9Router auto-start: {error}");
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut Self| {
                    if let Some(quit) = tray.quit.take() {
                        let _ = quit.send(());
                    }
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn icon() -> Option<&'static Icon> {
    static ICON: LazyLock<Option<Icon>> = LazyLock::new(|| {
        let input = include_bytes!("../../../cli/src/cli/tray/icon.png");
        let mut decoder = png::Decoder::new(Cursor::new(input));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().ok()?;
        let mut data = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut data).ok()?;
        if info.color_type != png::ColorType::Rgba {
            return None;
        }
        data.truncate(info.buffer_size());
        for pixel in data.as_chunks_mut::<4>().0 {
            pixel.rotate_right(1);
        }
        Some(Icon {
            width: i32::try_from(info.width).ok()?,
            height: i32::try_from(info.height).ok()?,
            data,
        })
    });
    ICON.as_ref()
}

fn toggle_autostart(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    write_autostart(path)
}

fn write_autostart(path: &Path) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let exe = exe.to_str().ok_or(io::Error::new(
        io::ErrorKind::InvalidData,
        "9Router executable path must be UTF-8",
    ))?;
    if exe.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "9Router executable path contains a control character",
        ));
    }
    let mut command = String::new();
    for ch in exe.chars() {
        match ch {
            '\\' => command.push_str(r"\\\\"),
            '"' | '$' | '`' => {
                command.push_str(r"\\");
                command.push(ch);
            }
            '%' => command.push_str("%%"),
            _ => command.push(ch),
        }
    }
    let dir = path.parent().ok_or(io::Error::new(
        io::ErrorKind::InvalidInput,
        "9Router auto-start path has no parent",
    ))?;
    fs::create_dir_all(dir)?;
    let mut file = NamedTempFile::new_in(dir)?;
    write!(
        file,
        "[Desktop Entry]\nType=Application\nName=9Router\nComment=9Router API Proxy\nExec=\"{command}\" --tray --skip-update\nHidden=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true\n"
    )?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}
