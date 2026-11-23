use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use plist::{Dictionary, Value as Plist};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const STARTUP: Duration = Duration::from_secs(8);
const STOP_WAIT: Duration = Duration::from_secs(5);
const LOCK_WAIT: Duration = Duration::from_secs(20);
const COMMAND_WAIT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(120);
const STATE_LIMIT: u64 = 16 * 1024;
const OUTPUT_LIMIT: u64 = 128 * 1024;

pub(super) struct Failure {
    pub status: StatusCode,
    pub message: String,
    pub code: &'static str,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>, code: &'static str) -> Self {
        Self {
            status,
            message: message.into(),
            code,
        }
    }

    fn state() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read or write Headroom process state",
            "STATE_FAILED",
        )
    }

    fn unknown() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "Headroom process ownership could not be verified",
            "UNVERIFIED_PROCESS",
        )
    }

    fn launch() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to start the Headroom LaunchAgent in the current macOS login session",
            "SPAWN_FAILED",
        )
    }

    fn stop() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to stop headroom proxy",
            "STOP_FAILED",
        )
    }
}

pub(super) fn managed_pid(data: &Path) -> Option<u32> {
    let launcher = Launcher::native();
    let agent = launcher.agent(data, false).ok()??;
    agent.owned_job().ok().flatten()?.1.pid
}

pub(super) fn start(
    data: &Path,
    binary: Option<PathBuf>,
    port: u16,
    code_aware: bool,
    kompress: bool,
    restart: bool,
) -> Result<Value, Failure> {
    Launcher::native().start(data, binary, port, code_aware, kompress, restart)
}

pub(super) fn stop(data: &Path) -> Result<Value, Failure> {
    Launcher::native().stop(data)
}

pub(super) struct Launcher {
    command: PathBuf,
    uid: u32,
    startup: Duration,
}

impl Launcher {
    fn native() -> Self {
        Self {
            command: PathBuf::from("/bin/launchctl"),
            uid: unsafe { libc::geteuid() },
            startup: STARTUP,
        }
    }

    fn agent(&self, data: &Path, create: bool) -> Result<Option<Agent<'_>>, Failure> {
        let data = data.canonicalize().map_err(|_| Failure::state())?;
        check_directory(&data, self.uid, false)?;
        let headroom = data.join("headroom");
        let private = headroom.join("launchd");
        for (dir, only_owner) in [(&headroom, false), (&private, true)] {
            if create {
                match DirBuilder::new().mode(0o700).create(dir) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(Failure::state()),
                }
            }
            if !check_directory(dir, self.uid, only_owner)? {
                return Ok(None);
            }
        }
        let root = absolute_text(&data).ok_or_else(Failure::state)?;
        let digest = format!("{:x}", Sha256::digest(root));
        let record = private.join("proxy.plist");
        absolute_text(&record).ok_or_else(Failure::state)?;
        Ok(Some(Agent {
            launcher: self,
            headroom,
            private,
            record,
            domain: format!("gui/{}", self.uid),
            prefix: format!("com.9router.headroom.{}.", &digest[..24]),
        }))
    }

    fn start(
        &self,
        data: &Path,
        binary: Option<PathBuf>,
        port: u16,
        code_aware: bool,
        kompress: bool,
        restart: bool,
    ) -> Result<Value, Failure> {
        let binary = binary.ok_or_else(|| {
            Failure::new(
                StatusCode::BAD_REQUEST,
                "Headroom CLI not installed",
                "NOT_INSTALLED",
            )
        })?;
        let agent = self.agent(data, true)?.ok_or_else(Failure::state)?;
        let _lock = agent.lock()?;
        agent.check_legacy_pid()?;
        let existing = agent.owned_job()?;
        if let Some(pid) = existing
            .as_ref()
            .and_then(|(_, job)| job.pid)
            .filter(|_| !restart)
        {
            return Ok(json!({"success":true,"pid":pid,"alreadyRunning":true}));
        }
        let binary = binary.canonicalize().map_err(|_| Failure::launch())?;
        let program = absolute_text(&binary).ok_or_else(Failure::launch)?;
        let spec = Spec {
            label: format!("{}{}", agent.prefix, Uuid::new_v4().simple()),
            program: program.to_owned(),
            port,
            code_aware,
            kompress,
        };
        let plist = agent.plist(&spec).ok_or_else(Failure::state)?;
        agent.prepare_log()?;
        if let Some((previous, _)) = existing {
            agent.unload(&previous)?;
        } else {
            agent.remove_record()?;
        }
        agent.create_record(&plist)?;
        let pid = agent.launch(&spec)?;
        Ok(json!({"success":true,"pid":pid,"alreadyRunning":false}))
    }

    fn stop(&self, data: &Path) -> Result<Value, Failure> {
        let not_running = || json!({"stopped":false,"reason":"not_running"});
        let agent = self.agent(data, true)?.ok_or_else(Failure::state)?;
        let _lock = agent.lock()?;
        agent.check_legacy_pid()?;
        let Some((spec, loaded)) = agent.owned_job()? else {
            agent.remove_record()?;
            return Ok(not_running());
        };
        agent.unload(&spec)?;
        Ok(loaded
            .pid
            .map_or_else(not_running, |pid| json!({"stopped":true,"pid":pid})))
    }
}

struct Agent<'a> {
    launcher: &'a Launcher,
    headroom: PathBuf,
    private: PathBuf,
    record: PathBuf,
    domain: String,
    prefix: String,
}

struct Spec {
    label: String,
    program: String,
    port: u16,
    code_aware: bool,
    kompress: bool,
}

struct Job {
    pid: Option<u32>,
    outcome: Option<String>,
}

impl Agent<'_> {
    fn lock(&self) -> Result<File, Failure> {
        let file = open_file(
            &self.private.join("proxy.lock"),
            true,
            self.launcher.uid,
            true,
        )
        .map_err(|_| Failure::state())?;
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            use std::os::fd::AsRawFd;
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(file);
            }
            match io::Error::last_os_error().kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock if Instant::now() < deadline => std::thread::sleep(POLL),
                io::ErrorKind::WouldBlock => {
                    return Err(Failure::new(
                        StatusCode::CONFLICT,
                        "Another Headroom operation is in progress",
                        "PROCESS_BUSY",
                    ));
                }
                _ => return Err(Failure::state()),
            }
        }
    }

    fn check_legacy_pid(&self) -> Result<(), Failure> {
        let Some(bytes) = self.read(&self.headroom.join("proxy.pid"), false)? else {
            return Ok(());
        };
        let Some(pid) = std::str::from_utf8(&bytes).ok().and_then(|text| {
            let text = text.trim_start();
            let end = text
                .find(|ch: char| !ch.is_ascii_digit())
                .unwrap_or(text.len());
            text[..end].parse::<i32>().ok().filter(|pid| *pid > 0)
        }) else {
            return Ok(());
        };
        let pid = pid.to_string();
        let (status, stdout, stderr) = self
            .run(Path::new("/bin/ps"), &["-p", &pid, "-o", "pid="])
            .map_err(|_| Failure::unknown())?;
        if status.code() == Some(1) && stdout.trim().is_empty() && stderr.trim().is_empty() {
            Ok(())
        } else {
            Err(Failure::unknown())
        }
    }

    fn owned_job(&self) -> Result<Option<(Spec, Job)>, Failure> {
        let Some(bytes) = self.read(&self.record, true)? else {
            return Ok(None);
        };
        let plist = Plist::from_reader_xml(bytes.as_slice()).map_err(|_| Failure::unknown())?;
        let spec = self.spec(&plist).ok_or_else(Failure::unknown)?;
        Ok(self.lookup(&spec)?.map(|job| (spec, job)))
    }

    fn read(&self, path: &Path, private: bool) -> Result<Option<Vec<u8>>, Failure> {
        let file = match open_file(path, false, self.launcher.uid, private) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Failure::state()),
        };
        bounded_read(file, STATE_LIMIT)
            .map(Some)
            .map_err(|_| Failure::state())
    }

    fn spec(&self, value: &Plist) -> Option<Spec> {
        let dict = value.as_dictionary()?;
        let label = dict.get("Label")?.as_string()?;
        let nonce = label.strip_prefix(&self.prefix)?;
        if nonce.len() != 32 || Uuid::parse_str(nonce).ok()?.simple().to_string() != nonce {
            return None;
        }
        let program = absolute_text(Path::new(dict.get("Program")?.as_string()?))?;
        let args = dict.get("ProgramArguments")?.as_array()?;
        let port = args
            .get(3)?
            .as_string()?
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)?;
        let spec = Spec {
            label: label.to_owned(),
            program: program.to_owned(),
            port,
            code_aware: args
                .iter()
                .any(|arg| arg.as_string() == Some("--code-aware")),
            kompress: !args
                .iter()
                .any(|arg| arg.as_string() == Some("--disable-kompress")),
        };
        (self.plist(&spec)? == *value).then_some(spec)
    }

    fn plist(&self, spec: &Spec) -> Option<Plist> {
        let log_path = self.headroom.join("proxy.log");
        let log = absolute_text(&log_path)?;
        let bin = Path::new(&spec.program).parent()?.to_str()?;
        let mut args = vec![
            spec.program.clone(),
            "proxy".into(),
            "--port".into(),
            spec.port.to_string(),
        ];
        if spec.code_aware {
            args.push("--code-aware".into());
        }
        if !spec.kompress {
            args.push("--disable-kompress".into());
        }
        let path = std::env::join_paths([
            Path::new(bin),
            Path::new("/opt/homebrew/bin"),
            Path::new("/usr/local/bin"),
            Path::new("/usr/bin"),
            Path::new("/bin"),
            Path::new("/usr/sbin"),
            Path::new("/sbin"),
        ])
        .ok()?;
        let environment =
            Dictionary::from_iter([("PATH".to_owned(), Plist::String(path.into_string().ok()?))]);
        Some(Plist::Dictionary(Dictionary::from_iter([
            ("Label".to_owned(), Plist::String(spec.label.clone())),
            ("Program".to_owned(), Plist::String(spec.program.clone())),
            (
                "ProgramArguments".to_owned(),
                Plist::Array(args.into_iter().map(Plist::String).collect()),
            ),
            ("RunAtLoad".to_owned(), Plist::Boolean(true)),
            ("KeepAlive".to_owned(), Plist::Boolean(false)),
            ("ExitTimeOut".to_owned(), Plist::Integer(2u64.into())),
            ("StandardOutPath".to_owned(), Plist::String(log.to_owned())),
            (
                "StandardErrorPath".to_owned(),
                Plist::String(log.to_owned()),
            ),
            (
                "EnvironmentVariables".to_owned(),
                Plist::Dictionary(environment),
            ),
        ])))
    }

    fn prepare_log(&self) -> Result<(), Failure> {
        open_file(
            &self.headroom.join("proxy.log"),
            true,
            self.launcher.uid,
            false,
        )
        .map(|_| ())
        .map_err(|_| Failure::state())
    }

    fn create_record(&self, plist: &Plist) -> Result<(), Failure> {
        let mut file =
            tempfile::NamedTempFile::new_in(&self.private).map_err(|_| Failure::state())?;
        plist::to_writer_xml(&mut file, plist).map_err(|_| Failure::state())?;
        file.as_file().sync_all().map_err(|_| Failure::state())?;
        file.persist_noclobber(&self.record)
            .map_err(|_| Failure::state())?;
        self.sync_directory()
    }

    fn remove_record(&self) -> Result<(), Failure> {
        match fs::remove_file(&self.record) {
            Ok(()) => self.sync_directory(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(Failure::state()),
        }
    }

    fn sync_directory(&self) -> Result<(), Failure> {
        File::open(&self.private)
            .and_then(|file| file.sync_all())
            .map_err(|_| Failure::state())
    }

    fn service(&self, spec: &Spec) -> String {
        format!("{}/{}", self.domain, spec.label)
    }

    fn lookup(&self, spec: &Spec) -> Result<Option<Job>, Failure> {
        let service = self.service(spec);
        let (status, stdout, stderr) = self
            .run(&self.launcher.command, &["print", &service])
            .map_err(|_| Failure::unknown())?;
        if !status.success() {
            let missing = format!("Could not find service \"{}\" in domain ", spec.label);
            return if stdout.trim().is_empty()
                && stderr.lines().any(|line| line.starts_with(&missing))
            {
                Ok(None)
            } else {
                Err(Failure::unknown())
            };
        }
        let mut lines = stdout.lines();
        if lines.next() != Some(&format!("{service} = {{")) {
            return Err(Failure::unknown());
        }
        let mut fields = std::collections::HashMap::new();
        loop {
            let line = lines.next().ok_or_else(Failure::unknown)?;
            if line == "}" {
                break;
            }
            if let Some((key, value)) = line
                .strip_prefix('\t')
                .and_then(|line| line.split_once(" = "))
                && matches!(
                    key,
                    "path" | "program" | "pid" | "last exit code" | "last terminating signal"
                )
                && fields.insert(key, value).is_some()
            {
                return Err(Failure::unknown());
            }
        }
        if fields.get("path").copied() != self.record.to_str()
            || fields.get("program").copied() != Some(spec.program.as_str())
        {
            return Err(Failure::unknown());
        }
        let pid = match fields.get("pid") {
            Some(pid) => Some(
                pid.parse::<u32>()
                    .ok()
                    .filter(|pid| *pid > 0)
                    .ok_or_else(Failure::unknown)?,
            ),
            None => None,
        };
        let code = fields
            .get("last exit code")
            .and_then(|value| value.split_whitespace().next())
            .and_then(|code| code.parse::<i32>().ok());
        let outcome = code.map(|code| code.to_string()).or_else(|| {
            fields
                .get("last terminating signal")
                .map(|_| "null".to_owned())
        });
        Ok(Some(Job { pid, outcome }))
    }

    fn launch(&self, spec: &Spec) -> Result<u32, Failure> {
        let path = absolute_text(&self.record).ok_or_else(Failure::state)?;
        let (status, _, _) = self
            .run(&self.launcher.command, &["bootstrap", &self.domain, path])
            .map_err(|_| Failure::launch())?;
        if !status.success() {
            return Err(Failure::launch());
        }
        let deadline = Instant::now() + self.launcher.startup;
        loop {
            let Some(job) = self.lookup(spec)? else {
                return Err(Failure::launch());
            };
            if job.pid.is_none()
                && let Some(outcome) = job.outcome
            {
                self.unload(spec)?;
                return Err(Failure::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("headroom proxy exited early (code={outcome}) — see proxy.log"),
                    "EARLY_EXIT",
                ));
            }
            if Instant::now() >= deadline {
                if let Some(pid) = job.pid {
                    return Ok(pid);
                }
                self.unload(spec)?;
                return Err(Failure::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "headroom proxy exited during startup — see proxy.log",
                    "EARLY_EXIT",
                ));
            }
            std::thread::sleep(POLL);
        }
    }

    fn unload(&self, spec: &Spec) -> Result<(), Failure> {
        let service = self.service(spec);
        let (status, _, _) = self
            .run(&self.launcher.command, &["bootout", &service])
            .map_err(|_| Failure::stop())?;
        if !status.success() && self.lookup(spec)?.is_some() {
            return Err(Failure::stop());
        }
        let deadline = Instant::now() + STOP_WAIT;
        while self.lookup(spec)?.is_some() {
            if Instant::now() >= deadline {
                return Err(Failure::stop());
            }
            std::thread::sleep(POLL);
        }
        self.remove_record()
    }

    fn run(&self, program: &Path, arguments: &[&str]) -> io::Result<(ExitStatus, String, String)> {
        let mut stdout = tempfile::tempfile_in(&self.private)?;
        let mut stderr = tempfile::tempfile_in(&self.private)?;
        let mut child = Command::new(program)
            .args(arguments)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?)
            .spawn()?;
        let deadline = Instant::now() + COMMAND_WAIT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(result
                        .err()
                        .unwrap_or_else(|| io::Error::from(io::ErrorKind::TimedOut)));
                }
            }
        };
        stdout.seek(SeekFrom::Start(0))?;
        stderr.seek(SeekFrom::Start(0))?;
        let read =
            |file| String::from_utf8(bounded_read(file, OUTPUT_LIMIT)?).map_err(io::Error::other);
        Ok((status, read(stdout)?, read(stderr)?))
    }
}

fn check_directory(path: &Path, uid: u32, private: bool) -> Result<bool, Failure> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(Failure::state()),
    };
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & if private { 0o077 } else { 0o022 } != 0
    {
        return Err(Failure::state());
    }
    Ok(true)
}

fn absolute_text(path: &Path) -> Option<&str> {
    path.to_str()
        .filter(|value| path.is_absolute() && !value.chars().any(char::is_control))
}

fn open_file(path: &Path, create: bool, uid: u32, private: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != uid
        || meta.mode() & if private { 0o077 } else { 0o022 } != 0
    {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(file)
}

fn bounded_read(file: File, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(bytes)
}
