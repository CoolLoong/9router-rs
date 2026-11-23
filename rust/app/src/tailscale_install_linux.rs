use std::convert::Infallible;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{
    IntoResponse, Response,
    sse::{Event, Sse},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{Tailscale, approval_url, claim_operation, error, online, system_status};

const TIMEOUT: Duration = Duration::from_secs(900);
const DIAGNOSTIC_LIMIT: usize = 64 * 1024;
const PACKAGE_HOST: &str = "https://pkgs.tailscale.com/stable";
const MISSING_PACKAGE: &str = "Tailscale is unavailable in the configured repositories";
const APT_KEYRING: &str = "/usr/share/keyrings/9router-tailscale-archive-keyring.gpg";
const APT_SOURCE: &str = "/etc/apt/sources.list.d/9router-tailscale.list";
type Events = mpsc::Sender<Result<Event, Infallible>>;

struct Distribution {
    tool: &'static str,
    args: &'static [&'static str],
    repository: Option<Repository>,
}

enum Repository {
    Apt { family: &'static str, suite: String },
    Rpm(String),
    Alpine,
}

pub(super) async fn install(State(state): State<Tailscale>, request: Request) -> Response {
    let fixture = (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"))
        .then(|| state.storage.root().join("tailscale-install-fixture"));
    let Some(distribution) = distribution(fixture.as_deref()) else {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "Automatic Tailscale installation is unavailable on this distribution",
        );
    };
    let body = to_bytes(request.into_body(), 32 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or_default();
    let password = match body["sudoPassword"]
        .as_str()
        .filter(|password| !password.trim().is_empty())
    {
        Some(password) => Some(password.to_owned()),
        None => state
            .storage
            .read_settings()
            .await
            .and_then(|settings| cached_password(&settings)),
    };
    let Some(password) = password else {
        return error(StatusCode::BAD_REQUEST, "Sudo password is required");
    };
    if password.len() > 4096 || password.contains(['\r', '\n', '\0']) {
        return error(StatusCode::BAD_REQUEST, "Invalid sudo password");
    }
    let Ok(operation) = state.operation.clone().try_lock_owned() else {
        return error(
            StatusCode::CONFLICT,
            "Another gateway Funnel operation is in progress",
        );
    };
    let lease = match claim_operation(&state.storage) {
        Ok(lease) => lease,
        Err(error) => return error.response(),
    };
    let (sender, receiver) = mpsc::channel(12);
    let hostname =
        crate::cloudflare_tunnel::short_id(&crate::cloudflare_tunnel::shared_state(&state.storage));
    tokio::spawn(async move {
        let _operation = operation;
        let _lease = lease;
        match run(
            distribution,
            fixture.as_deref(),
            hostname.as_deref(),
            &password,
            &sender,
        )
        .await
        {
            Ok(url) => emit(&sender, "done", json!({"success":true,"authUrl":url})).await,
            Err(reason) => emit(&sender, "error", json!({"error":reason})).await,
        }
    });
    Sse::new(futures_util::stream::unfold(
        receiver,
        |mut events| async move { events.recv().await.map(|event| (event, events)) },
    ))
    .into_response()
}

pub(super) fn cached_password(settings: &Value) -> Option<String> {
    let stored = settings["mitmSudoEncrypted"].as_str()?;
    let mut fields = stored.split(':');
    let (iv, tag, data) = (fields.next()?, fields.next()?, fields.next()?);
    if iv.len() != 24 || tag.len() != 32 || data.len() > 8192 || fields.next().is_some() {
        return None;
    }
    let machine_id = ["/var/lib/dbus/machine-id", "/etc/machine-id"]
        .into_iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|contents| {
            contents
                .lines()
                .next()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
        })
        .find(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))?;
    let hashed_id = Sha256::digest(machine_id);
    let key = Sha256::digest(format!("{hashed_id:x}9router-mitm-pwd"));
    let cipher = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).ok()?);
    let nonce = Nonce::try_assume_unique_for_key(&decode_hex(iv)?).ok()?;
    let mut encrypted = decode_hex(data)?;
    encrypted.extend(decode_hex(tag)?);
    let clear = cipher
        .open_in_place(nonce, Aad::empty(), &mut encrypted)
        .ok()?;
    let password = std::str::from_utf8(clear).ok()?;
    if password.trim().is_empty() || password.contains(['\r', '\n', '\0']) {
        return None;
    }
    Some(password.to_owned())
}

fn decode_hex(input: &str) -> Option<Vec<u8>> {
    if !input.len().is_multiple_of(2) || !input.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    (0..input.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&input[offset..offset + 2], 16).ok())
        .collect()
}

fn distribution(fixture: Option<&Path>) -> Option<Distribution> {
    let text = if let Some(root) = fixture {
        std::fs::read_to_string(root.join("os-release"))
    } else {
        std::fs::read_to_string("/etc/os-release")
            .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
    }
    .ok()?;
    let field = |name: &str| {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .find_map(|(key, value)| (key == name).then(|| value.trim().trim_matches(['"', '\''])))
    };
    let id = field("ID")?;
    let similar = field("ID_LIKE").unwrap_or_default();
    let family = |name| id == name || similar.split_ascii_whitespace().any(|item| item == name);
    let version = field("VERSION_ID").filter(|value| repository_component(value));
    let major = version
        .and_then(|value| value.split('.').next())
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()));
    let codename = field("VERSION_CODENAME").filter(|value| repository_component(value));
    let ubuntu = field("UBUNTU_CODENAME").filter(|value| repository_component(value));
    let debian = field("DEBIAN_CODENAME").filter(|value| repository_component(value));
    let major_number = major.and_then(|value| value.parse::<u32>().ok());
    let apt = |family, suite: Option<&str>| Distribution {
        tool: "apt-get",
        args: &["install", "-y", "tailscale"],
        repository: suite.map(|suite| Repository::Apt {
            family,
            suite: suite.into(),
        }),
    };
    let rpm = |tool, path: Option<String>| Distribution {
        tool,
        args: &["install", "-y", "tailscale"],
        repository: path.map(Repository::Rpm),
    };
    let dnf = if major == Some("7") { "yum" } else { "dnf" };
    let system = match id {
        "nixos" | "steamos" | "bazzite" | "kde-linux" => return None,
        "osmc" | "pureos" | "kaisen" => apt("debian", Some("bullseye")),
        "ubuntu" | "pop" | "neon" | "tuxedo" | "elementary" | "zorin" => {
            apt("ubuntu", ubuntu.or(codename))
        }
        "debian" if field("NAME") == Some("Parrot Security") => apt("debian", Some("bookworm")),
        "linuxmint" if ubuntu.is_none() && debian.is_some() => apt("debian", debian),
        "linuxmint" => apt("ubuntu", ubuntu.or(codename)),
        "industrial-os" | "parrot" | "mendel" | "deepin" | "Deepin" | "kali" => {
            let threshold = match id {
                "deepin" | "Deepin" => 20,
                "kali" => 2021,
                _ => 5,
            };
            let suite = if major_number.is_some_and(|version| version < threshold) {
                "buster"
            } else {
                "bullseye"
            };
            apt("debian", Some(suite))
        }
        "galliumos" => apt("ubuntu", Some("bionic")),
        "sparky" => apt("debian", debian.or(codename)),
        "pika" if major_number.is_some_and(|version| version < 4) => apt("ubuntu", ubuntu),
        "pika" => apt("debian", debian),
        "raspbian" => apt("raspbian", codename),
        "amzn" => rpm("yum", version.map(|v| format!("amazon-linux/{v}"))),
        "photon" => rpm("tdnf", major.map(|v| format!("photon/{v}"))),
        "ol" => rpm(dnf, major.map(|v| format!("oracle/{v}"))),
        "centos" => rpm(dnf, major.map(|v| format!("centos/{v}"))),
        "xenenterprise" => rpm("yum", major.map(|v| format!("centos/{v}"))),
        "rhel" | "miraclelinux" => rpm(dnf, major.map(|v| format!("rhel/{v}"))),
        "alpine" | "postmarketos" => Distribution {
            tool: "apk",
            args: &["add", "tailscale"],
            repository: Some(Repository::Alpine),
        },
        "void" => Distribution {
            tool: "xbps-install",
            args: &["-y", "tailscale"],
            repository: None,
        },
        "gentoo" => Distribution {
            tool: "emerge",
            args: &["--ask=n", "net-vpn/tailscale"],
            repository: None,
        },
        "opensuse-leap"
        | "sles"
        | "opensuse-tumbleweed"
        | "opensuse-slowroll"
        | "sle-micro-rancher"
        | "opensuse" => Distribution {
            tool: "zypper",
            args: &["--non-interactive", "install", "tailscale"],
            repository: match id {
                "opensuse-tumbleweed" | "opensuse-slowroll" => {
                    Some(Repository::Rpm("opensuse/tumbleweed".into()))
                }
                "sle-micro-rancher" => Some(Repository::Rpm("opensuse/leap/15.4".into())),
                _ => version.map(|v| Repository::Rpm(format!("opensuse/leap/{v}"))),
            },
        },
        "archarm" | "arch" | "cachyos" | "manjaro" | "manjaro-arm" | "endeavouros" | "blendos"
        | "garuda" | "archcraft" | "omarchy" | "biglinux" => Distribution {
            tool: "pacman",
            args: &["-S", "--needed", "--noconfirm", "tailscale"],
            repository: None,
        },
        _ if family("arch") => Distribution {
            tool: "pacman",
            args: &["-S", "--needed", "--noconfirm", "tailscale"],
            repository: None,
        },
        _ if ubuntu.is_some() || family("ubuntu") => apt("ubuntu", ubuntu.or(codename)),
        _ if family("debian") => apt("debian", codename),
        _ if matches!(
            id,
            "fedora"
                | "rocky"
                | "almalinux"
                | "nobara"
                | "openmandriva"
                | "sangoma"
                | "risios"
                | "cloudlinux"
                | "alinux"
                | "fedora-asahi-remix"
                | "ultramarine"
        ) || family("fedora")
            || family("rhel") =>
        {
            rpm(dnf, Some("fedora".into()))
        }
        _ if family("suse") => Distribution {
            tool: "zypper",
            args: &["--non-interactive", "install", "tailscale"],
            repository: version.map(|v| Repository::Rpm(format!("opensuse/leap/{v}"))),
        },
        _ => return None,
    };
    Some(system)
}

fn repository_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn program(name: &str, fixture: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = fixture {
        let path = root.join(name);
        return path.is_file().then_some(path);
    }
    ["/usr/bin", "/usr/sbin", "/bin", "/sbin"]
        .iter()
        .find_map(|dir| {
            let path = Path::new(dir).join(name).canonicalize().ok()?;
            let data = path.metadata().ok()?;
            (data.is_file()
                && data.uid() == 0
                && data.mode() & 0o111 != 0
                && data.mode() & 0o022 == 0
                && path.ancestors().skip(1).all(|dir| {
                    dir.metadata().is_ok_and(|meta| {
                        meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o022 == 0
                    })
                }))
            .then_some(path)
        })
}

async fn install_package(
    distribution: &Distribution,
    sudo: &Path,
    fixture: Option<&Path>,
    password: &str,
    events: &Events,
) -> Result<(), &'static str> {
    let manager =
        program(distribution.tool, fixture).ok_or("Distribution package manager is unavailable")?;
    progress(
        events,
        "Installing Tailscale from the distribution repository...",
    )
    .await;
    match elevated(sudo, &manager, distribution.args, password).await {
        Ok(()) => return Ok(()),
        Err(MISSING_PACKAGE) if fixture.is_none() => {}
        Err(error) => return Err(error),
    }
    let repository = distribution.repository.as_ref().ok_or(MISSING_PACKAGE)?;
    progress(events, "Configuring a signed Tailscale package source...").await;
    match repository {
        Repository::Apt { family, suite } => {
            if !apt_has_tailscale_source() {
                let key = apt_signing_key(family, suite).await?;
                let source =
                    format!("deb [signed-by={APT_KEYRING}] {PACKAGE_HOST}/{family} {suite} main\n");
                install_repository_file(sudo, APT_KEYRING, &key, password).await?;
                install_repository_file(sudo, APT_SOURCE, source.as_bytes(), password).await?;
            }
            elevated(sudo, &manager, &["update"], password).await?;
        }
        Repository::Rpm(path) => {
            let path = format!("{PACKAGE_HOST}/{path}");
            let configuration = format!(
                "[9router-tailscale]\nname=Tailscale stable\nbaseurl={path}/$basearch\nenabled=1\ntype=rpm\nrepo_gpgcheck=1\ngpgcheck=1\ngpgkey={path}/repo.gpg\n",
            );
            let destination = if distribution.tool == "zypper" {
                "/etc/zypp/repos.d/9router-tailscale.repo"
            } else {
                "/etc/yum.repos.d/9router-tailscale.repo"
            };
            install_repository_file(sudo, destination, configuration.as_bytes(), password).await?;
            if distribution.tool == "zypper" {
                elevated(
                    sudo,
                    &manager,
                    &[
                        "--non-interactive",
                        "--gpg-auto-import-keys",
                        "refresh",
                        "9router-tailscale",
                    ],
                    password,
                )
                .await?;
            }
        }
        Repository::Alpine => {
            let setup = program("setup-apkrepos", None)
                .ok_or("The Alpine community repository is unavailable")?;
            elevated(sudo, &setup, &["-c", "-1"], password).await?;
        }
    }
    elevated(sudo, &manager, distribution.args, password).await
}

fn apt_has_tailscale_source() -> bool {
    let contains_source =
        |path: &Path| {
            std::fs::read_to_string(path).is_ok_and(|contents| {
                let configured = |line: &str| {
                    let line = line.trim();
                    !line.starts_with('#') && line.contains("https://pkgs.tailscale.com/")
                };
                if path
                    .extension()
                    .is_some_and(|extension| extension == "sources")
                {
                    contents.replace("\r\n", "\n").split("\n\n").any(|record| {
                        !record.lines().filter_map(|line| line.split_once(':')).any(
                            |(name, value)| {
                                name.eq_ignore_ascii_case("enabled")
                                    && value.trim().eq_ignore_ascii_case("no")
                            },
                        ) && record.lines().any(configured)
                    })
                } else {
                    contents.lines().any(configured)
                }
            })
        };
    contains_source(Path::new("/etc/apt/sources.list"))
        || std::fs::read_dir("/etc/apt/sources.list.d")
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                matches!(
                    path.extension().and_then(|name| name.to_str()),
                    Some("list" | "sources")
                )
            })
            .any(|path| contains_source(&path))
}

async fn apt_signing_key(family: &str, suite: &str) -> Result<Vec<u8>, &'static str> {
    let failure = "Could not download the Tailscale signing key";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| failure)?;
    let prefix = format!("{PACKAGE_HOST}/{family}/{suite}");
    let mut key = client
        .get(format!("{prefix}.noarmor.gpg"))
        .send()
        .await
        .map_err(|_| failure)?;
    let armored = key.status() == StatusCode::NOT_FOUND;
    if armored {
        key = client
            .get(format!("{prefix}.asc"))
            .send()
            .await
            .map_err(|_| failure)?;
    }
    if !key.status().is_success() {
        return Err(failure);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = key.chunk().await.map_err(|_| failure)? {
        if chunk.len() > DIAGNOSTIC_LIMIT - bytes.len() {
            return Err(failure);
        }
        bytes.extend_from_slice(&chunk);
    }
    if armored {
        let text = std::str::from_utf8(&bytes).map_err(|_| failure)?;
        let text = text.replace("\r\n", "\n");
        let (_, payload) = text
            .strip_prefix("-----BEGIN PGP PUBLIC KEY BLOCK-----")
            .and_then(|value| value.split_once("\n\n"))
            .ok_or(failure)?;
        let (payload, _) = payload
            .split_once("-----END PGP PUBLIC KEY BLOCK-----")
            .ok_or(failure)?;
        let encoded = payload
            .lines()
            .filter(|line| !line.starts_with('='))
            .collect::<String>();
        bytes = STANDARD.decode(encoded).map_err(|_| failure)?;
    }
    if !bytes.first().is_some_and(|byte| {
        byte & 0x80 != 0
            && if byte & 0x40 != 0 {
                byte & 0x3f == 6
            } else {
                (byte >> 2) & 0x0f == 6
            }
    }) {
        return Err(failure);
    }
    Ok(bytes)
}

async fn install_repository_file(
    sudo: &Path,
    destination: &str,
    contents: &[u8],
    password: &str,
) -> Result<(), &'static str> {
    let failure = "Could not stage Tailscale repository metadata";
    let executable = program("install", None).ok_or("System install command is unavailable")?;
    let directory = tempfile::tempdir().map_err(|_| failure)?;
    let file = directory.path().join("repository");
    let mut output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&file)
        .map_err(|_| failure)?;
    output.write_all(contents).map_err(|_| failure)?;
    drop(output);
    let source = file.to_str().ok_or(failure)?;
    elevated(
        sudo,
        &executable,
        &["-D", "-m", "0644", "--", source, destination],
        password,
    )
    .await
}

async fn start_daemon(
    distribution: &Distribution,
    sudo: &Path,
    fixture: Option<&Path>,
    installed: bool,
    password: &str,
) -> Result<(), &'static str> {
    let run = |name| program(name, fixture).ok_or("System service manager is unavailable");
    match distribution.tool {
        "apk" => {
            if installed {
                elevated(sudo, &run("rc-update")?, &["add", "tailscale"], password).await?;
            }
            elevated(sudo, &run("rc-service")?, &["tailscale", "start"], password).await
        }
        "xbps-install" => {
            const SERVICE: &str = "/var/service/tailscaled";
            if fixture.is_none() && !Path::new(SERVICE).exists() {
                let source = "/etc/sv/tailscaled";
                if !Path::new(source).join("run").is_file() {
                    return Err("The installed Tailscale runit service is missing");
                }
                elevated(sudo, &run("ln")?, &["-sT", "--", source, SERVICE], password).await?;
            }
            elevated(sudo, &run("sv")?, &["up", SERVICE], password).await
        }
        "emerge" if fixture.is_none() && !Path::new("/run/systemd/system").is_dir() => {
            if installed {
                elevated(sudo, &run("rc-update")?, &["add", "tailscaled"], password).await?;
            }
            elevated(
                sudo,
                &run("rc-service")?,
                &["tailscaled", "start"],
                password,
            )
            .await
        }
        _ => {
            let command = run("systemctl")?;
            if installed && fixture.is_none() {
                elevated(sudo, &command, &["enable", "--now", "tailscaled"], password).await
            } else {
                elevated(sudo, &command, &["start", "tailscaled"], password).await
            }
        }
    }
}

async fn run(
    distribution: Distribution,
    fixture: Option<&Path>,
    hostname: Option<&str>,
    password: &str,
    events: &Events,
) -> Result<Option<String>, &'static str> {
    let sudo = program("sudo", fixture).ok_or("sudo is unavailable")?;
    let installed = program("tailscale", fixture).is_none();
    if installed {
        install_package(&distribution, &sudo, fixture, password, events).await?;
    }
    let cli = program("tailscale", fixture)
        .ok_or("Tailscale executable is unavailable after installation")?;
    let mut status = system_status(&cli).await;
    if status.is_none() {
        progress(events, "Starting daemon...").await;
        start_daemon(&distribution, &sudo, fixture, installed, password).await?;
        status = tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if let Some(status) = system_status(&cli).await {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .ok();
    }
    let Some(status) = status else {
        return Err("Tailscale daemon did not become ready");
    };
    if online(&status) {
        return Ok(None);
    }
    progress(events, "Logging in...").await;
    if let Some(url) = status["AuthURL"].as_str().and_then(approval_url) {
        return Ok(Some(url));
    }
    let hostname = hostname.map(|name| format!("--hostname={name}"));
    let mut args = vec!["up", "--accept-routes", "--timeout=3s"];
    args.extend(hostname.as_deref());
    let result = invoke_elevated(&sudo, &cli, &args, password).await;
    if let Ok(output) = &result
        && let Some(url) = [&output.stdout, &output.stderr]
            .into_iter()
            .find_map(|bytes| approval_url(&String::from_utf8_lossy(bytes)))
    {
        return Ok(Some(url));
    }
    if let Some(current) = system_status(&cli).await {
        if let Some(url) = current["AuthURL"].as_str().and_then(approval_url) {
            return Ok(Some(url));
        }
        if online(&current) {
            return Ok(None);
        }
    }
    result?.status()?;
    Err("Tailscale login did not become ready")
}

async fn elevated(
    sudo: &Path,
    executable: &Path,
    args: &[&str],
    password: &str,
) -> Result<(), &'static str> {
    invoke_elevated(sudo, executable, args, password)
        .await?
        .status()
}

struct Output {
    succeeded: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn invoke_elevated(
    sudo: &Path,
    executable: &Path,
    args: &[&str],
    password: &str,
) -> Result<Output, &'static str> {
    let failure = "Tailscale installation command failed";
    let mut child = Command::new(sudo)
        .args(["-S", "-p", "", "--"])
        .arg(executable)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| failure)?;
    let mut stdin = child.stdin.take().ok_or(failure)?;
    let stdout = child.stdout.take().ok_or(failure)?;
    let stderr = child.stderr.take().ok_or(failure)?;
    let completed = tokio::time::timeout(TIMEOUT, async {
        let send = async {
            let result = async {
                stdin.write_all(password.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.shutdown().await
            }
            .await;
            match result {
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                result => result,
            }
        };
        tokio::try_join!(child.wait(), diagnostics(stdout), diagnostics(stderr), send)
    })
    .await;
    let Ok(Ok((status, stdout, stderr, _))) = completed else {
        return Err(failure);
    };
    Ok(Output {
        succeeded: status.success(),
        stdout,
        stderr,
    })
}

impl Output {
    fn status(&self) -> Result<(), &'static str> {
        if self.succeeded {
            return Ok(());
        }
        let detail = String::from_utf8_lossy(&self.stderr);
        if detail.contains("incorrect password") || detail.contains("Sorry") {
            return Err("Wrong sudo password");
        }
        if [&self.stdout, &self.stderr].iter().any(|bytes| {
            String::from_utf8_lossy(bytes).lines().any(|line| {
                let line = line.to_ascii_lowercase();
                line.contains("tailscale")
                    && [
                        "unable to locate package",
                        "has no installation candidate",
                        "is not available",
                        "no match for argument:",
                        "unable to find a match:",
                        "no package",
                        "not found",
                        "no provider of",
                        "(no such package)",
                    ]
                    .iter()
                    .any(|message| line.contains(message))
            })
        }) {
            return Err(MISSING_PACKAGE);
        }
        Err("Tailscale installation command failed")
    }
}

async fn diagnostics(mut stream: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let size = stream.read(&mut buffer).await?;
        if size == 0 {
            return Ok(output);
        }
        let excess = (output.len() + size).saturating_sub(DIAGNOSTIC_LIMIT);
        output.drain(..excess);
        output.extend_from_slice(&buffer[..size]);
    }
}

async fn progress(events: &Events, message: &str) {
    emit(events, "progress", json!({"message":message})).await;
}

async fn emit(events: &Events, name: &str, data: Value) {
    let _ = events
        .send(Ok(Event::default().event(name).data(data.to_string())))
        .await;
}
