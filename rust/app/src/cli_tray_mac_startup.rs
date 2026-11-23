use std::fs;
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use plist::{Dictionary, Value};
use tempfile::NamedTempFile;

const LABEL: &str = "com.9router.autostart";
const ARGUMENTS: &[&str] = &["--tray", "--skip-update", "--no-browser"];
const LIMIT: u64 = 128 * 1024;
const TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct Startup {
    path: PathBuf,
    launchctl: PathBuf,
    service: String,
}

impl Startup {
    pub(super) fn new(home: &Path, uid: u32, launchctl: &Path) -> Self {
        Self {
            path: home
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
            launchctl: launchctl.to_owned(),
            service: format!("gui/{uid}/{LABEL}"),
        }
    }

    pub(super) fn enabled(&self) -> io::Result<bool> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.len() > LIMIT {
            return Err(unrecognized());
        }
        let mut bytes = Vec::new();
        fs::File::open(&self.path)?
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT || !owned(&bytes) {
            return Err(unrecognized());
        }
        Ok(true)
    }

    pub(super) fn toggle(&self, exe: &Path) -> io::Result<()> {
        if self.enabled()? {
            fs::remove_file(&self.path)?;
            return self.set_registration("disable");
        }
        self.register(exe)
    }

    pub(super) fn enable(&self, exe: &Path) -> io::Result<()> {
        if self.enabled()? {
            return Ok(());
        }
        self.register(exe)
    }

    fn register(&self, exe: &Path) -> io::Result<()> {
        enable_at(&self.path, exe)?;
        if let Err(error) = self.set_registration("enable") {
            let _ = fs::remove_file(&self.path);
            return Err(error);
        }
        Ok(())
    }

    fn set_registration(&self, operation: &str) -> io::Result<()> {
        let mut child = Command::new(&self.launchctl)
            .args([operation, self.service.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "launchctl {operation} failed ({status})"
                    )))
                };
            }
            if start.elapsed() >= TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    format!("launchctl {operation} did not finish"),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn enable_at(path: &Path, exe: &Path) -> io::Result<()> {
    if !exe.is_absolute() || !exe.is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "Missing 9Router executable",
        ));
    }
    let exe = exe
        .to_str()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "Executable path is not UTF-8"))?;
    if exe.chars().any(char::is_control) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "Executable path contains a control character",
        ));
    }
    let arguments = std::iter::once(exe)
        .chain(ARGUMENTS.iter().copied())
        .map(|argument| Value::String(argument.into()))
        .collect();
    let plist = Value::Dictionary(Dictionary::from_iter([
        ("Label".to_owned(), Value::String(LABEL.into())),
        ("ProgramArguments".to_owned(), Value::Array(arguments)),
        (
            "LimitLoadToSessionType".to_owned(),
            Value::String("Aqua".into()),
        ),
        ("RunAtLoad".to_owned(), Value::Boolean(true)),
        ("KeepAlive".to_owned(), Value::Boolean(false)),
    ]));
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing LaunchAgents directory"))?;
    fs::create_dir_all(directory)?;
    let mut file = NamedTempFile::new_in(directory)?;
    plist.to_writer_xml(&mut file).map_err(io::Error::other)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}

fn owned(bytes: &[u8]) -> bool {
    let Ok(Value::Dictionary(plist)) = Value::from_reader(std::io::Cursor::new(bytes)) else {
        return false;
    };
    if plist.get("Label").and_then(Value::as_string) != Some(LABEL) || plist.contains_key("Program")
    {
        return false;
    }
    let Some(args) = plist.get("ProgramArguments").and_then(Value::as_array) else {
        return false;
    };
    let Some(args) = args
        .iter()
        .map(Value::as_string)
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    match args.as_slice() {
        [exe, flags @ ..] if flags == ARGUMENTS => Path::new(exe).is_absolute(),
        [node, cli, "--tray", "--skip-update"] => {
            Path::new(node).is_absolute()
                && Path::new(cli).is_absolute()
                && Path::new(node)
                    .file_name()
                    .is_some_and(|name| name == "node")
                && Path::new(cli)
                    .file_name()
                    .is_some_and(|name| name == "cli.js")
        }
        _ => false,
    }
}

fn unrecognized() -> io::Error {
    io::Error::new(
        ErrorKind::AlreadyExists,
        "LaunchAgents already contains an unrecognized 9Router file",
    )
}
