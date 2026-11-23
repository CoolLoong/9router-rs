use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fd::OwnedFd;
use rustix::fs::{FlockOperation, Mode, OFlags, flock};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use serde_json::{Value, json};

use crate::cli_lifecycle::{inode, process_scope, process_start_time};

const OWNER: &str = "proxy.owner.json";
const PID: &str = "proxy.pid";
const OWNER_ENV: &str = "NINEROUTER_HEADROOM_OWNER";
const STARTUP: Duration = Duration::from_secs(8);
const LOCK_WAIT: Duration = Duration::from_secs(13);
const STOP_WAIT: Duration = Duration::from_secs(2);
const RESTART_WAIT: Duration = Duration::from_secs(3);
const KILL_WAIT: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(40);

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

    fn stop() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to stop headroom proxy",
            "STOP_FAILED",
        )
    }
}

struct OwnedProcess {
    pid: u32,
    pidfd: OwnedFd,
    record: Value,
}

enum Ownership {
    Empty,
    Unknown,
    Owned(OwnedProcess),
}

pub(super) fn managed_pid(data: &Path) -> Option<u32> {
    let state = ownership(&data.join("headroom")).ok()?;
    match state {
        Ownership::Owned(owner) => Some(owner.pid),
        _ => None,
    }
}

pub(super) fn start(
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
    let dir = data.join("headroom");
    let _lock = acquire(&dir)?;
    match ownership(&dir)? {
        Ownership::Unknown => return Err(Failure::unknown()),
        Ownership::Owned(owner) if !restart => {
            return Ok(json!({"success":true,"pid":owner.pid,"alreadyRunning":true}));
        }
        Ownership::Owned(owner) => {
            terminate(&owner, RESTART_WAIT)?;
            remove_record(&dir, &owner.record).map_err(|_| Failure::state())?;
        }
        Ownership::Empty => clear(&dir).map_err(|_| Failure::state())?,
    }
    let pid = launch(&dir, binary, port, code_aware, kompress)?;
    Ok(json!({"success":true,"pid":pid,"alreadyRunning":false}))
}

pub(super) fn stop(data: &Path) -> Result<Value, Failure> {
    let dir = data.join("headroom");
    let _lock = acquire(&dir)?;
    match ownership(&dir)? {
        Ownership::Unknown => Err(Failure::unknown()),
        Ownership::Empty => {
            clear(&dir).map_err(|_| Failure::state())?;
            Ok(json!({"stopped":false,"reason":"not_running"}))
        }
        Ownership::Owned(owner) => {
            terminate(&owner, STOP_WAIT)?;
            remove_record(&dir, &owner.record).map_err(|_| Failure::state())?;
            Ok(json!({"stopped":true,"pid":owner.pid}))
        }
    }
}

fn acquire(directory: &Path) -> Result<File, Failure> {
    fs::create_dir_all(directory).map_err(|_| Failure::state())?;
    let lock = rustix::fs::open(
        directory.join("proxy.lock"),
        OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
    )
    .map(File::from)
    .map_err(|_| Failure::state())?;
    if !lock.metadata().is_ok_and(|meta| meta.is_file()) {
        return Err(Failure::state());
    }
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(lock),
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) if Instant::now() < deadline => std::thread::sleep(POLL),
            Err(Errno::AGAIN) => {
                return Err(Failure::new(
                    StatusCode::CONFLICT,
                    "Another Headroom operation is in progress",
                    "PROCESS_BUSY",
                ));
            }
            Err(_) => return Err(Failure::state()),
        }
    }
}

fn read_state(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 16_384 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid Headroom process state",
        ));
    }
    let mut output = Vec::new();
    file.take(16_385).read_to_end(&mut output)?;
    if output.len() > 16_384 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid Headroom process state",
        ));
    }
    Ok(Some(output))
}

fn pid_marker(dir: &Path) -> Result<Option<u32>, Failure> {
    let raw = read_state(&dir.join(PID)).map_err(|_| Failure::state())?;
    Ok(raw
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| {
            let text = text.trim_start();
            let digits = text
                .find(|ch: char| !ch.is_ascii_digit())
                .unwrap_or(text.len());
            text[..digits].parse::<u32>().ok().filter(|pid| *pid > 0)
        }))
}

fn as_pid(raw: u32) -> Option<Pid> {
    let pid = i32::try_from(raw).ok().filter(|pid| *pid > 0)?;
    Pid::from_raw(pid)
}

fn alive(raw: u32) -> bool {
    let Some(pid) = as_pid(raw) else {
        return false;
    };
    match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(fd) => exited(&fd, Duration::ZERO).map_or(true, |exited| !exited),
        Err(Errno::SRCH) => false,
        Err(_) => true,
    }
}

fn ownership(dir: &Path) -> Result<Ownership, Failure> {
    let legacy = pid_marker(dir)?;
    let raw = read_state(&dir.join(OWNER)).map_err(|_| Failure::state())?;
    let Some(raw) = raw else {
        return Ok(if legacy.is_some_and(alive) {
            Ownership::Unknown
        } else {
            Ownership::Empty
        });
    };
    let Ok(record) = serde_json::from_slice::<Value>(&raw) else {
        return Ok(Ownership::Unknown);
    };
    let (boot, namespace) = process_scope().map_err(|_| Failure::state())?;
    if record["version"] != 1 || record["pidNamespace"] != json!(namespace) {
        return Ok(Ownership::Unknown);
    }
    if record["bootId"] != boot {
        return Ok(if legacy.is_some_and(alive) {
            Ownership::Unknown
        } else {
            Ownership::Empty
        });
    }
    let Some(raw_pid) = record["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != std::process::id())
    else {
        return Ok(Ownership::Unknown);
    };
    if legacy.is_some_and(|legacy| legacy != raw_pid && alive(legacy)) {
        return Ok(Ownership::Unknown);
    }
    let Some(pid) = as_pid(raw_pid) else {
        return Ok(Ownership::Unknown);
    };
    let fd = match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(Errno::SRCH) => return Ok(Ownership::Empty),
        Err(_) => return Ok(Ownership::Unknown),
    };
    match exited(&fd, Duration::ZERO) {
        Ok(true) => return Ok(Ownership::Empty),
        Err(_) => return Ok(Ownership::Unknown),
        Ok(false) => {}
    }
    let proc_dir = Path::new("/proc").join(raw_pid.to_string());
    let uid = fs::metadata("/proc/self")
        .map_err(|_| Failure::state())?
        .uid();
    let Some(nonce) = record["nonce"]
        .as_str()
        .filter(|nonce| uuid::Uuid::parse_str(nonce).is_ok())
    else {
        return Ok(Ownership::Unknown);
    };
    if !fs::metadata(&proc_dir).is_ok_and(|metadata| metadata.uid() == uid)
        || !inode(&proc_dir.join("ns/pid")).is_ok_and(|observed| observed == namespace)
        || !process_start_time(&proc_dir).is_ok_and(|start| record["startTime"] == start)
        || !owns_nonce(&proc_dir, nonce)
    {
        return Ok(if exited(&fd, Duration::ZERO).ok() == Some(true) {
            Ownership::Empty
        } else {
            Ownership::Unknown
        });
    }
    Ok(Ownership::Owned(OwnedProcess {
        pid: raw_pid,
        pidfd: fd,
        record,
    }))
}

fn owns_nonce(process: &Path, nonce: &str) -> bool {
    let marker = format!("{OWNER_ENV}={nonce}");
    fs::read(process.join("environ")).is_ok_and(|bytes| {
        bytes
            .split(|byte| *byte == 0)
            .any(|field| field == marker.as_bytes())
    })
}

fn exited(fd: &OwnedFd, timeout: Duration) -> io::Result<bool> {
    let end = Instant::now() + timeout;
    let mut descriptors = [PollFd::new(fd, PollFlags::IN)];
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        let delay = Timespec {
            tv_sec: remaining.as_secs() as i64,
            tv_nsec: remaining.subsec_nanos() as _,
        };
        match poll(&mut descriptors, Some(&delay)) {
            Ok(_) => {
                return Ok(descriptors[0]
                    .revents()
                    .intersects(PollFlags::IN | PollFlags::HUP));
            }
            Err(Errno::INTR) if Instant::now() < end => {}
            Err(Errno::INTR) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
}

fn terminate(owner: &OwnedProcess, wait: Duration) -> Result<(), Failure> {
    let signal = |signal| match pidfd_send_signal(&owner.pidfd, signal) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(_) => Err(Failure::stop()),
    };
    signal(Signal::TERM)?;
    if !exited(&owner.pidfd, wait).map_err(|_| Failure::stop())? {
        signal(Signal::KILL)?;
        if !exited(&owner.pidfd, KILL_WAIT).map_err(|_| Failure::stop())? {
            return Err(Failure::stop());
        }
    }
    Ok(())
}

fn launch(
    dir: &Path,
    binary: PathBuf,
    port: u16,
    code_aware: bool,
    kompress: bool,
) -> Result<u32, Failure> {
    let log = rustix::fs::open(
        dir.join("proxy.log"),
        OFlags::CREATE
            | OFlags::WRONLY
            | OFlags::APPEND
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
    )
    .map(File::from)
    .map_err(|_| Failure::state())?;
    if !log.metadata().is_ok_and(|meta| meta.is_file()) {
        return Err(Failure::state());
    }
    let nonce = uuid::Uuid::new_v4().to_string();
    let mut command = Command::new(binary);
    command.arg("proxy").arg("--port").arg(port.to_string());
    if code_aware {
        command.arg("--code-aware");
    }
    if !kompress {
        command.arg("--disable-kompress");
    }
    command
        .env(OWNER_ENV, &nonce)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|_| Failure::state())?))
        .stderr(Stdio::from(log))
        .process_group(0);

    let mut child = command.spawn().map_err(|_| {
        Failure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to spawn headroom proxy",
            "SPAWN_FAILED",
        )
    })?;
    let pid = child.id();
    let result = startup(dir, &mut child, &nonce);
    let record = match result {
        Ok(record) => record,
        Err(failure) => {
            let _ = child.kill();
            let _ = child.wait();
            if let Ok(Some(record)) = read_record(dir)
                && record["nonce"] == nonce
            {
                let _ = remove_record(dir, &record);
            }
            return Err(failure);
        }
    };
    let owned_dir = dir.to_owned();
    std::thread::spawn(move || {
        let _ = child.wait();
        if let Ok(_lock) = acquire(&owned_dir) {
            let _ = remove_record(&owned_dir, &record);
        }
    });
    Ok(pid)
}

fn startup(dir: &Path, child: &mut Child, nonce: &str) -> Result<Value, Failure> {
    if let Some(status) = child.try_wait().map_err(|_| Failure::state())? {
        return Err(early(status));
    }
    let pid = child.id();
    let (boot, namespace) = process_scope().map_err(|_| Failure::state())?;
    let proc_dir = Path::new("/proc").join(pid.to_string());
    let start = match process_start_time(&proc_dir) {
        Ok(time) => time,
        Err(_) => {
            if let Some(status) = child.try_wait().map_err(|_| Failure::state())? {
                return Err(early(status));
            }
            return Err(Failure::state());
        }
    };
    let record = json!({
        "version":1,"bootId":boot,"pidNamespace":namespace,"pid":pid,"startTime":start,"nonce":nonce,
    });
    publish(dir, OWNER, record.to_string().as_bytes()).map_err(|_| Failure::state())?;
    publish(dir, PID, pid.to_string().as_bytes()).map_err(|_| Failure::state())?;
    let deadline = Instant::now() + STARTUP;
    loop {
        if let Some(status) = child.try_wait().map_err(|_| Failure::state())? {
            return Err(early(status));
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL);
    }
    match ownership(dir)? {
        Ownership::Owned(owner) if owner.pid == pid && owner.record == record => Ok(record),
        _ => Err(Failure::unknown()),
    }
}

fn early(status: std::process::ExitStatus) -> Failure {
    let code = status
        .code()
        .map_or_else(|| "null".into(), |code| code.to_string());
    Failure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("headroom proxy exited early (code={code}) — see proxy.log"),
        "EARLY_EXIT",
    )
}

fn publish(directory: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(directory.join(name))
        .map_err(|error| error.error)?;
    File::open(directory)?.sync_all()
}

fn clear(directory: &Path) -> io::Result<()> {
    for name in [PID, OWNER] {
        remove_if_present(&directory.join(name))?;
    }
    Ok(())
}

fn remove_if_present(file: &Path) -> io::Result<()> {
    match fs::remove_file(file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn read_record(directory: &Path) -> io::Result<Option<Value>> {
    Ok(read_state(&directory.join(OWNER))?
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok()))
}

fn remove_record(directory: &Path, expected: &Value) -> io::Result<()> {
    if read_record(directory)?.as_ref() == Some(expected) {
        if let Ok(Some(pid)) = pid_marker(directory)
            && expected["pid"] == pid
        {
            remove_if_present(&directory.join(PID))?;
        }
        remove_if_present(&directory.join(OWNER))?;
    }
    Ok(())
}
