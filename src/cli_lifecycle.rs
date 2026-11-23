use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fd::OwnedFd;
use rustix::fs::{FlockOperation, Mode, OFlags, flock};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use serde_json::{Value, json};
use tokio::net::{TcpListener, lookup_host};
use tokio::time::{Instant, sleep};

const STARTUP_WAIT: Duration = Duration::from_secs(8);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_millis(50);

pub struct LauncherLease {
    _instance: File,
    path: PathBuf,
    identity: Value,
    published: bool,
}

impl LauncherLease {
    pub fn publish(&mut self) -> io::Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(
            self.path.parent().expect("launcher record directory"),
        )?;
        serde_json::to_writer(&mut temp, &self.identity)?;
        temp.flush()?;
        temp.persist(&self.path).map_err(|error| error.error)?;
        self.published = true;
        Ok(())
    }
}

impl Drop for LauncherLease {
    fn drop(&mut self) {
        if self.published && read_record(&self.path).ok().flatten().as_ref() == Some(&self.identity)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub async fn reserve(
    data_dir: &Path,
    host: &str,
    port: u16,
) -> io::Result<(TcpListener, LauncherLease)> {
    reserve_with_policy(data_dir, host, port, true).await
}

#[cfg(feature = "native-package")]
pub(crate) async fn reserve_update_child(
    data_dir: &Path,
    host: &str,
    port: u16,
) -> io::Result<(TcpListener, LauncherLease)> {
    reserve_with_policy(data_dir, host, port, false).await
}

async fn reserve_with_policy(
    data_dir: &Path,
    host: &str,
    port: u16,
    can_take_over: bool,
) -> io::Result<(TcpListener, LauncherLease)> {
    fs::create_dir_all(data_dir)?;
    let startup = lock_file(&data_dir.join("launcher.start.lock"))?;
    lock_with_timeout(
        &startup,
        STARTUP_WAIT,
        "Another 9Router launcher is starting",
    )
    .await?;
    let instance = lock_file(&data_dir.join("launcher.instance.lock"))?;
    let path = data_dir.join("launcher.pid");
    let addresses: Vec<_> = lookup_host((host, port)).await?.collect();
    let held = try_lock(&instance)?;
    let record = read_record(&path)?;
    let pending_listener = TcpListener::bind(addresses.as_slice()).await;
    let listener = if held || try_lock(&instance)? {
        if record.as_ref().is_some_and(legacy_launcher_alive) {
            return Err(in_use(
                "An upstream launcher still owns this data directory",
            ));
        }
        match pending_listener {
            Err(error) if !held && error.kind() == io::ErrorKind::AddrInUse => {
                TcpListener::bind(addresses.as_slice()).await?
            }
            result => result?,
        }
    } else {
        if !can_take_over {
            return Err(in_use(
                "An independent launcher already owns the data directory",
            ));
        }
        let previous = record.as_ref().ok_or_else(unverified)?;
        let owner = verified_owner(previous, &instance, None)?;
        let listener = match pending_listener {
            Ok(listener) => Some(listener),
            Err(error)
                if error.kind() == io::ErrorKind::AddrInUse
                    && addresses.contains(&owner.address)
                    && owner.owns_listener(previous) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        owner.stop().await?;
        lock_with_timeout(
            &instance,
            SHUTDOWN_WAIT,
            "Previous 9Router still owns the data directory",
        )
        .await?;
        match listener {
            Some(listener) => listener,
            None => TcpListener::bind(addresses.as_slice()).await?,
        }
    };
    clear_record(&path)?;
    let identity = current_identity(&listener, &instance)?;
    Ok((
        listener,
        LauncherLease {
            _instance: instance,
            path,
            identity,
            published: false,
        },
    ))
}

fn lock_file(path: &Path) -> io::Result<File> {
    let options =
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let file = File::from(rustix::fs::open(path, options, Mode::RUSR | Mode::WUSR)?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Launcher lock is not a regular file",
        ));
    }
    Ok(file)
}

fn try_lock(file: &File) -> io::Result<bool> {
    match flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(Errno::AGAIN) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn lock_with_timeout(file: &File, wait: Duration, message: &str) -> io::Result<()> {
    let deadline = Instant::now() + wait;
    while !try_lock(file)? {
        if Instant::now() >= deadline {
            return Err(in_use(message));
        }
        sleep(RETRY).await;
    }
    Ok(())
}

fn clear_record(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn read_record(path: &Path) -> io::Result<Option<Value>> {
    let options = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let file = match rustix::fs::open(path, options, Mode::empty()) {
        Ok(file) => File::from(file),
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Launcher record is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(65_536).read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes).ok().filter(Value::is_object))
}

fn current_identity(listener: &TcpListener, instance: &File) -> io::Result<Value> {
    let process = Path::new("/proc/self");
    let exe = process.join("exe");
    let exe_path = fs::read_link(&exe)?;
    let address = listener.local_addr()?;
    let listener_fd = listener.as_raw_fd();
    let listener_id = inode(&process.join("fd").join(listener_fd.to_string()))?;
    let (boot, namespace) = process_scope()?;
    let identity = json!({
        "cliPid": std::process::id(),
        "serverPid": null,
        "port": address.port(),
        "exe": exe_path.file_name().and_then(|name| name.to_str()).unwrap_or("9router"),
        "native": {
            "version": 1,
            "bootId": boot,
            "pidNamespace": namespace,
            "startTime": process_start_time(process)?,
            "executable": inode(&exe)?,
            "executablePath": exe_path.as_os_str().as_encoded_bytes(),
            "lockFd": instance.as_raw_fd(),
            "listenerFd": listener_fd,
            "listener": listener_id,
            "address": address.to_string(),
        }
    });
    #[cfg(feature = "native-package")]
    let identity = {
        let mut identity = identity;
        identity["native"]["packageVersion"] = json!(crate::native_updater::compiled_version());
        identity
    };
    Ok(identity)
}

pub(crate) fn process_scope() -> io::Result<(String, [u64; 2])> {
    Ok((
        fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned(),
        inode(Path::new("/proc/self/ns/pid"))?,
    ))
}

pub(crate) fn process_start_time(process: &Path) -> io::Result<u64> {
    let stat = fs::read(process.join("stat"))?;
    let end = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(unverified)?;
    let start = stat[end + 1..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .nth(19)
        .ok_or_else(unverified)?;
    std::str::from_utf8(start)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(unverified)
}

pub(crate) fn inode(path: &Path) -> io::Result<[u64; 2]> {
    let meta = fs::metadata(path)?;
    Ok([meta.dev(), meta.ino()])
}

struct VerifiedOwner {
    process: PathBuf,
    pidfd: OwnedFd,
    address: SocketAddr,
}

impl VerifiedOwner {
    fn owns_listener(&self, record: &Value) -> bool {
        let native = &record["native"];
        let Some(fd) = native["listenerFd"].as_u64() else {
            return false;
        };
        fs::metadata(self.process.join("fd").join(fd.to_string())).is_ok_and(|meta| {
            meta.file_type().is_socket()
                && native["listener"] == json!([meta.dev(), meta.ino()])
                && listener_address_matches(&self.process, self.address, meta.ino())
        })
    }

    async fn stop(self) -> io::Result<()> {
        match pidfd_send_signal(&self.pidfd, Signal::TERM) {
            Ok(()) => (),
            Err(error) if error == Errno::SRCH => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let deadline = Instant::now() + SHUTDOWN_WAIT;
        let mut fds = [PollFd::new(&self.pidfd, PollFlags::IN)];
        loop {
            match poll(&mut fds, Some(&Timespec::default())) {
                Ok(_) if fds[0].revents().intersects(PollFlags::IN | PollFlags::HUP) => {
                    return Ok(());
                }
                Ok(_) | Err(Errno::INTR) => (),
                Err(error) => return Err(error.into()),
            }
            if Instant::now() >= deadline {
                return Err(in_use("Previous 9Router did not shut down in time"));
            }
            sleep(RETRY).await;
        }
    }
}

fn listener_address_matches(process: &Path, address: SocketAddr, socket_inode: u64) -> bool {
    let (table, ip) = match address.ip() {
        IpAddr::V4(ip) => (
            "net/tcp",
            format!("{:08X}", u32::from_ne_bytes(ip.octets())),
        ),
        IpAddr::V6(ip) => {
            let ip = ip
                .octets()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| format!("{:08X}", u32::from_ne_bytes(*word)))
                .collect::<String>();
            ("net/tcp6", ip)
        }
    };
    let endpoint = format!("{ip}:{:04X}", address.port());
    fs::read_to_string(process.join(table)).is_ok_and(|sockets| {
        sockets.lines().skip(1).any(|line| {
            let mut fields = line.split_whitespace();
            fields.nth(1) == Some(endpoint.as_str())
                && fields.nth(1) == Some("0A")
                && fields.nth(5).and_then(|inode| inode.parse::<u64>().ok()) == Some(socket_inode)
        })
    })
}

#[cfg(feature = "native-package")]
pub(crate) fn child_owns_listener(
    data_dir: &Path,
    pid: u32,
    image: &Path,
    address: SocketAddr,
    version: &str,
) -> bool {
    let Ok(Some(record)) = read_record(&data_dir.join("launcher.pid")) else {
        return false;
    };
    if record["cliPid"] != json!(pid) || record["native"]["packageVersion"] != version {
        return false;
    }
    let Ok(instance) = lock_file(&data_dir.join("launcher.instance.lock")) else {
        return false;
    };
    verified_owner(&record, &instance, Some(image))
        .is_ok_and(|owner| owner.address == address && owner.owns_listener(&record))
}

fn verified_owner(
    record: &Value,
    instance: &File,
    expected_image: Option<&Path>,
) -> io::Result<VerifiedOwner> {
    let (boot, namespace) = process_scope()?;
    let native = &record["native"];
    if native["version"] != 1
        || native["bootId"] != boot
        || native["pidNamespace"] != json!(namespace)
    {
        return Err(unverified());
    }
    let raw_pid = record["cliPid"]
        .as_i64()
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0 && u32::try_from(*pid).ok() != Some(std::process::id()))
        .ok_or_else(unverified)?;
    let pid = Pid::from_raw(raw_pid).ok_or_else(unverified)?;
    let pidfd = pidfd_open(pid, PidfdFlags::empty()).map_err(|_| unverified())?;
    let process = Path::new("/proc").join(raw_pid.to_string());
    if inode(&process.join("ns/pid")).map_err(|_| unverified())? != namespace {
        return Err(unverified());
    }
    let target_exe = process.join("exe");
    let current_exe = Path::new("/proc/self/exe");
    let own_uid = fs::metadata("/proc/self")?.uid();
    let target_uid = fs::metadata(&process).map_err(|_| unverified())?.uid();
    let target_id = inode(&target_exe).map_err(|_| unverified())?;
    let target_path = fs::read_link(&target_exe).map_err(|_| unverified())?;
    let current_path = fs::read_link(current_exe)?;
    let target_bytes = target_path.as_os_str().as_encoded_bytes();
    let recorded_bytes = target_bytes
        .strip_suffix(b" (deleted)")
        .unwrap_or(target_bytes);
    let same_image = if let Some(expected_image) = expected_image {
        target_id == inode(expected_image)? && target_path == expected_image
    } else {
        target_id == inode(current_exe)?
            || recorded_bytes == current_path.as_os_str().as_encoded_bytes()
    };
    if own_uid != target_uid
        || native["startTime"] != process_start_time(&process).map_err(|_| unverified())?
        || !same_image
        || native["executable"] != json!(target_id)
        || (native["executablePath"] != json!(target_bytes)
            && native["executablePath"] != json!(recorded_bytes))
    {
        return Err(unverified());
    }
    let lock_fd = native["lockFd"]
        .as_u64()
        .ok_or_else(unverified)?
        .to_string();
    let lease_file = fs::metadata(process.join("fd").join(&lock_fd)).map_err(|_| unverified())?;
    let current_lease = instance.metadata()?;
    let fdinfo =
        fs::read_to_string(process.join("fdinfo").join(lock_fd)).map_err(|_| unverified())?;
    let owns_lock = fdinfo.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some("lock:")
            && fields.nth(1) == Some("FLOCK")
            && fields.next() == Some("ADVISORY")
            && fields.next() == Some("WRITE")
            && fields.next().and_then(|owner| owner.parse::<i32>().ok()) == Some(raw_pid)
    });
    if !owns_lock
        || (lease_file.dev(), lease_file.ino()) != (current_lease.dev(), current_lease.ino())
    {
        return Err(unverified());
    }
    let address: SocketAddr = native["address"]
        .as_str()
        .and_then(|address| address.parse().ok())
        .ok_or_else(unverified)?;
    if record["port"] != address.port() {
        return Err(unverified());
    }
    Ok(VerifiedOwner {
        process,
        pidfd,
        address,
    })
}

fn legacy_launcher_alive(record: &Value) -> bool {
    if record.get("native").is_some() {
        return false;
    }
    let Some(name) = record["exe"].as_str() else {
        return false;
    };
    ["cliPid", "serverPid"]
        .into_iter()
        .filter_map(|field| record[field].as_u64())
        .filter(|pid| *pid > 0 && *pid != u64::from(std::process::id()))
        .any(|pid| {
            fs::read_link(Path::new("/proc").join(pid.to_string()).join("exe"))
                .is_ok_and(|exe| exe.file_name().is_some_and(|file| file == name))
        })
}

fn in_use(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::AddrInUse, message)
}

fn unverified() -> io::Error {
    in_use("A launcher is starting or the previous 9Router process identity could not be verified")
}
