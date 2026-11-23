use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::mem::{offset_of, size_of};
use std::net::{SocketAddrV4, TcpStream};
use std::os::windows::io::{AsHandle, BorrowedHandle};
use std::path::Path;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};

use crate::windows_acl::{Account, Installation, Rename};
use crate::windows_process::{self as process, Child};
use crate::{create_dir, mark, wait_marker};

const WAIT: Duration = Duration::from_secs(12);

pub(crate) fn initialize(root: &Path, old: &Path, candidate: &Path) -> io::Result<()> {
    let account = Account::current()?;
    for path in [
        root.to_owned(),
        root.join("install"),
        root.join("install/stage"),
        root.join("store"),
    ] {
        create_dir(&path, &account, false)?;
    }
    fs::copy(old, root.join("install/9router.exe"))?;
    fs::copy(candidate, root.join("install/stage/candidate.exe"))?;
    Ok(())
}

pub(crate) fn upgrade(
    root: &Path,
    address: SocketAddrV4,
    previous: &str,
    expected: &str,
    fault: Option<&str>,
) -> io::Result<()> {
    let account = Account::current()?;
    let install = root.join("install/9router.exe");
    let state = root.join("install/stage");
    let report = root.join("gateway.txt");
    let installation = Installation::open(&install, &account)?;
    let stage = installation.staging(&state, &account)?;
    let lease = process::lease(&root.join("install/lease"))?;
    let data_lease = process::lease(&root.join("store/lease"))?;
    let start = || launch(&install, address, lease.as_handle(), data_lease.as_handle());
    let original = start()?;
    verified(&original, address, previous)?;
    if !http(address, "/api/version")?.contains(&format!("\"latestVersion\":\"{expected}\"")) {
        return Err(io::Error::other(
            "The gateway could not verify the signed release",
        ));
    }
    mark(&report, "old-signed-feed-verified")?;
    if original.wait(Duration::from_secs(30))? != 0 {
        return Err(io::Error::other("Old gateway exited unsuccessfully"));
    }

    let database = root.join("store/db/data.sqlite");
    let snapshot = state.join("snapshot.sqlite");
    let snapshot_wal = state.join("snapshot.sqlite-wal");
    let wal = database.with_file_name("data.sqlite-wal");
    fs::copy(&database, &snapshot)?;
    if wal.exists() {
        fs::copy(&wal, &snapshot_wal)?;
    }
    installation.recheck(&install)?;
    installation.recheck_staging(&state, &stage, &account)?;
    let backup = state.join("previous.exe");
    fs::copy(&install, &backup)?;
    anchored(
        &installation,
        &state.join("candidate.exe"),
        &install,
        &account,
    )?;
    if fault == Some("interrupt-after-swap") {
        mark(&report, "recovery-snapshot-ready")?;
        std::process::exit(23);
    }
    let candidate = start()?;
    if verified(&candidate, address, expected).is_ok() {
        mark(&report, "actual-replacement-verified")?;
        if candidate.wait(Duration::from_secs(30))? != 0 {
            return Err(io::Error::other("Candidate exit failed"));
        }
        return Ok(());
    }
    if fault == Some("exercise-dirty-store") {
        mark(&report, "bad-candidate-awaiting-dirty-store")?;
        wait_marker(&root.join("restore.txt"), "continue")?;
    }
    candidate.terminate(WAIT)?;
    restore(root, &installation, &account)?;
    let restored = start()?;
    verified(&restored, address, previous)?;
    mark(&report, "actual-rollback-verified")?;
    if restored.wait(Duration::from_secs(30))? != 0 {
        return Err(io::Error::other("Restored gateway exit failed"));
    }
    Ok(())
}

pub(crate) fn recover(root: &Path, address: SocketAddrV4, version: &str) -> io::Result<()> {
    let account = Account::current()?;
    let install = root.join("install/9router.exe");
    let installation = Installation::open(&install, &account)?;
    let _stage = installation.staging(&root.join("install/stage"), &account)?;
    let lease = process::lease(&root.join("install/lease"))?;
    let data_lease = process::lease(&root.join("store/lease"))?;
    restore(root, &installation, &account)?;
    let restored = launch(&install, address, lease.as_handle(), data_lease.as_handle())?;
    verified(&restored, address, version)?;
    mark(&root.join("gateway.txt"), "actual-manual-recovery-verified")?;
    if restored.wait(Duration::from_secs(30))? != 0 {
        return Err(io::Error::other("Recovered gateway exit failed"));
    }
    Ok(())
}

fn restore(root: &Path, installation: &Installation, account: &Account) -> io::Result<()> {
    let install = root.join("install/9router.exe");
    let state = root.join("install/stage");
    anchored(installation, &state.join("previous.exe"), &install, account)?;
    let database = root.join("store/db/data.sqlite");
    let snapshot_wal = state.join("snapshot.sqlite-wal");
    for suffix in ["data.sqlite-wal", "data.sqlite-shm"] {
        let path = database.with_file_name(suffix);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    let staged_database = database.with_file_name("recovered-data.sqlite");
    fs::copy(state.join("snapshot.sqlite"), &staged_database)?;
    let database_identity = Installation::open(&database, account)?;
    anchored(&database_identity, &staged_database, &database, account)?;
    if snapshot_wal.exists() {
        fs::copy(snapshot_wal, database.with_file_name("data.sqlite-wal"))?;
    }
    Ok(())
}

fn launch(
    install: &Path,
    address: SocketAddrV4,
    lease: BorrowedHandle<'_>,
    data_lease: BorrowedHandle<'_>,
) -> io::Result<Child> {
    let mut child = process::suspend(
        install,
        &[
            "--host".to_owned(),
            address.ip().to_string(),
            "--port".to_owned(),
            address.port().to_string(),
            "--no-browser".to_owned(),
        ],
    )?;
    child.duplicate(lease)?;
    child.duplicate(data_lease)?;
    child.resume()?;
    Ok(child)
}

fn anchored(
    checked: &Installation,
    source: &Path,
    target: &Path,
    account: &Account,
) -> io::Result<()> {
    checked.anchored_replace(
        source,
        target.file_name().ok_or(io::ErrorKind::InvalidInput)?,
        Rename::NativeExtended,
        account,
    )
}

fn verified(child: &Child, socket: SocketAddrV4, version: &str) -> io::Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        if !child
            .wait(Duration::ZERO)
            .is_err_and(|error| error.kind() == io::ErrorKind::TimedOut)
        {
            return Err(io::Error::other("Gateway died before owning its listener"));
        }
        if owns_listener(child, socket)?
            && let Ok(health) = http(socket, "/api/health")
            && health.contains("\"ok\":true")
        {
            let version_result = http(socket, "/api/version")?;
            if !version_result.contains(&format!("\"currentVersion\":\"{version}\"")) {
                return Err(io::Error::other("Gateway compiled version mismatch"));
            }
            if owns_listener(child, socket)? {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn owns_listener(child: &Child, address: SocketAddrV4) -> io::Result<bool> {
    let mut bytes = 0u32;
    let result = unsafe {
        GetExtendedTcpTable(
            null_mut(),
            &mut bytes,
            0,
            2,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if result != ERROR_INSUFFICIENT_BUFFER || bytes == 0 || bytes > 4 * 1024 * 1024 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    let result = unsafe {
        GetExtendedTcpTable(
            buffer.as_mut_ptr().cast(),
            &mut bytes,
            0,
            2,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if result == ERROR_INSUFFICIENT_BUFFER {
        return Ok(false);
    }
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let base = offset_of!(MIB_TCPTABLE_OWNER_PID, table);
    let count = unsafe { *buffer.as_ptr().cast::<u32>() } as usize;
    if count > (bytes as usize).saturating_sub(base) / size_of::<MIB_TCPROW_OWNER_PID>() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let rows = unsafe {
        std::slice::from_raw_parts(
            buffer
                .as_ptr()
                .cast::<u8>()
                .add(base)
                .cast::<MIB_TCPROW_OWNER_PID>(),
            count,
        )
    };
    Ok(rows.iter().any(|row| {
        row.dwOwningPid == child.id()
            && row.dwLocalAddr.to_ne_bytes() == address.ip().octets()
            && u16::from_be(row.dwLocalPort as u16) == address.port()
    }))
}

fn http(address: SocketAddrV4, path: &str) -> io::Result<String> {
    let mut socket = TcpStream::connect_timeout(&(address.into()), Duration::from_secs(1))?;
    socket.set_read_timeout(Some(WAIT))?;
    write!(
        socket,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )?;
    let mut data = String::new();
    socket.take(131_072).read_to_string(&mut data)?;
    if !data.starts_with("HTTP/1.1 200") {
        return Err(io::Error::other("Gateway health probe failed"));
    }
    Ok(data)
}
