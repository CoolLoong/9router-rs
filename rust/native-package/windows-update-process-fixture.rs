use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::GetHandleInformation;
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

mod windows_acl;
mod windows_gateway;
mod windows_handoff;
mod windows_process;
use windows_acl::{Account, Installation, Rename, attributes};
use windows_process as process;

const WAIT: Duration = Duration::from_secs(6);

fn main() -> io::Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    match args
        .first()
        .and_then(|arg| arg.to_str())
        .unwrap_or_default()
    {
        "parent" => parent(&args[1..]),
        "helper" => helper(&args[1..]),
        "candidate" => candidate(&args[1..]),
        "observer" => observer(&args[1..]),
        "acl" => acl_fixture(Path::new(&args[1])),
        "gateway-init" => windows_gateway::initialize(
            Path::new(&args[1]),
            Path::new(&args[2]),
            Path::new(&args[3]),
        ),
        "gateway-update" => windows_gateway::upgrade(
            Path::new(&args[1]),
            args[2]
                .to_string_lossy()
                .parse()
                .map_err(io::Error::other)?,
            args[3].to_str().ok_or(io::ErrorKind::InvalidInput)?,
            args[4].to_str().ok_or(io::ErrorKind::InvalidInput)?,
            args.get(5).and_then(|arg| arg.to_str()),
        ),
        "gateway-recover" => windows_gateway::recover(
            Path::new(&args[1]),
            args[2]
                .to_string_lossy()
                .parse()
                .map_err(io::Error::other)?,
            args[3].to_str().ok_or(io::ErrorKind::InvalidInput)?,
        ),
        "contender" => {
            let lock = process::lease(Path::new(&args[1]));
            if let Err(error) = lock {
                if error.raw_os_error() == Some(32) {
                    std::process::exit(17);
                }
                return Err(error);
            }
            Ok(())
        }
        _ => Err(io::ErrorKind::InvalidInput.into()),
    }
}

fn argument(handle: &OwnedHandle) -> OsString {
    (handle.as_raw_handle() as usize).to_string().into()
}

fn mark(path: &Path, state: &str) -> io::Result<()> {
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(log, "{state}")?;
    log.sync_all()
}

fn wait_marker(path: &Path, expected: &str) -> io::Result<()> {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if fs::read_to_string(path).is_ok_and(|state| state.lines().any(|row| row == expected)) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(io::ErrorKind::TimedOut.into())
}

fn parent(args: &[OsString]) -> io::Result<()> {
    let image = std::env::current_exe()?;
    let directory = image.parent().ok_or(io::ErrorKind::NotFound)?;
    let _padding = (0..128)
        .map(|_| process::event())
        .collect::<io::Result<Vec<_>>>()?;
    let lock_path = directory.join("lease");
    let lock = process::lease(&lock_path)?;
    let contender = process::spawn(&image, &[OsString::from("contender"), lock_path.into()])?;
    if contender.wait(WAIT)? != 17 {
        return Err(io::Error::other(
            "installation lease does not exclude another process",
        ));
    }
    let parent = process::private_exit_handle()?;
    let account = Account::current()?;
    let handoff = windows_handoff::Handoff::create(&account)?;
    let helper = directory.join("helper.exe");
    let mut helper_args = vec![OsString::from("helper"), image.clone().into()];
    helper_args.extend_from_slice(args);
    let mut child = handoff.start(&helper, &helper_args)?;
    handoff.assign(&child, &[parent.as_handle(), lock.as_handle()])?;
    let observer = process::spawn_unrelated(
        &image,
        &[
            OsString::from("observer"),
            handoff.name().to_owned(),
            argument(&parent),
            (lock.as_raw_handle() as usize).to_string().into(),
        ],
    )?;
    if observer.wait(WAIT)? != 0 {
        return Err(io::Error::other("Unexpected access from unrelated process"));
    }
    child.resume()?;
    let report = Path::new(&args[1]);
    wait_marker(report, "parent-lease-held")?;
    mark(report, "parent-exiting")
}

fn helper(args: &[OsString]) -> io::Result<()> {
    let (channel, args) = args.split_last().ok_or(io::ErrorKind::InvalidInput)?;
    let mut handles = windows_handoff::receive(channel, 2, &Account::current()?)?.into_iter();
    let parent = handles.next().ok_or(io::ErrorKind::InvalidInput)?;
    let lease = handles.next().ok_or(io::ErrorKind::InvalidInput)?;
    let install = PathBuf::from(&args[0]);
    let replacement = Path::new(&args[1]);
    let report = Path::new(&args[2]);
    let address = args[3].clone();
    let lock = install.with_file_name("lease");
    let contender = process::spawn(
        &install,
        &[OsString::from("contender"), lock.clone().into()],
    )?;
    let waiting = process::wait(parent.as_handle(), Duration::ZERO);
    if waiting
        .as_ref()
        .err()
        .is_none_or(|error| error.kind() != io::ErrorKind::TimedOut)
        || contender.wait(WAIT)? != 17
    {
        return Err(io::Error::other("parent or inherited lease proof failed"));
    }
    mark(report, "parent-lease-held")?;
    process::wait(parent.as_handle(), WAIT)?;
    let contender = process::spawn(&install, &[OsString::from("contender"), lock.into()])?;
    if contender.wait(WAIT)? != 17 {
        return Err(io::Error::other("lease released before the handoff"));
    }
    mark(report, "parent-exit-observed")?;

    let backup = install.with_file_name("previous.exe");
    process::replace(&install, replacement, Some(&backup))?;
    if launch(&install, &lease, &address, '2').is_ok() {
        fs::remove_file(backup)?;
        return mark(report, "verified-replacement");
    }
    mark(report, "candidate-rejected")?;
    if let Some(ready) = args.get(4) {
        wait_marker(Path::new(ready), "continue")?;
    }
    process::replace(&install, &backup, None)?;
    launch(&install, &lease, &address, '1')?;
    mark(report, "verified-rollback")
}

fn launch(image: &Path, lease: &OwnedHandle, address: &OsStr, version: char) -> io::Result<()> {
    let event = process::event()?;
    let handoff = windows_handoff::Handoff::create(&Account::current()?)?;
    let mut child = handoff.start(image, &[OsString::from("candidate"), address.into()])?;
    handoff.assign(&child, &[lease.as_handle(), event.as_handle()])?;
    child.resume()?;
    let ready = child
        .ready(event.as_handle(), WAIT)
        .and_then(|_| response(address, "VERSION"))
        .and_then(|actual| {
            if actual == version.to_string()
                && child
                    .wait(Duration::ZERO)
                    .is_err_and(|error| error.kind() == io::ErrorKind::TimedOut)
            {
                Ok(())
            } else {
                Err(io::Error::other(
                    "replacement version disagrees with readiness result",
                ))
            }
        });
    if ready.is_err() {
        child.terminate(WAIT)?;
    }
    ready
}

fn candidate(args: &[OsString]) -> io::Result<()> {
    let mut handles = windows_handoff::receive(&args[1], 2, &Account::current()?)?.into_iter();
    let lock = File::from(handles.next().ok_or(io::ErrorKind::InvalidInput)?);
    let metadata = lock.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("installation lease type mismatch"));
    }
    let ready = handles.next().ok_or(io::ErrorKind::InvalidInput)?;
    let image = std::env::current_exe()?;
    let version = fs::read(image)?
        .last()
        .copied()
        .ok_or(io::ErrorKind::InvalidData)?;
    if version == b'!' {
        return Err(io::Error::other("fixture boot failure"));
    }
    let listener = TcpListener::bind(args[0].to_string_lossy().as_ref())?;
    process::signal(ready.as_handle())?;
    for connection in listener.incoming() {
        let mut connection = connection?;
        connection.set_read_timeout(Some(WAIT))?;
        let mut buffer = [0u8; 16];
        let size = connection.read(&mut buffer)?;
        if &buffer[..size] == b"STOP" {
            return Ok(());
        }
        connection.write_all(&[version])?;
    }
    Ok(())
}

fn observer(args: &[OsString]) -> io::Result<()> {
    if !windows_handoff::published_read_only(&args[0])?
        || !windows_handoff::receive(&args[0], 2, &Account::current()?)
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
    {
        return Err(io::Error::other(
            "Unrelated process saw the handoff channel contents",
        ));
    }
    for value in &args[1..] {
        let value = value
            .to_string_lossy()
            .parse::<usize>()
            .map_err(io::Error::other)?;
        let mut flags = 0;
        if unsafe { GetHandleInformation(value as _, &mut flags) } != 0 {
            return Err(io::Error::other(
                "Unrelated process inherited an update handle",
            ));
        }
    }
    Ok(())
}

fn create_dir(path: &Path, account: &Account, public: bool) -> io::Result<()> {
    let descriptor = account.descriptor(public)?;
    let attrs = attributes(&descriptor);
    let path = process::wide(path.as_os_str())?;
    process::check(unsafe { CreateDirectoryW(path.as_ptr(), &attrs) })
}

fn acl_fixture(root: &Path) -> io::Result<()> {
    let account = Account::current()?;
    create_dir(root, &account, false)?;
    let install = root.join("install");
    let public = root.join("public");
    create_dir(&install, &account, false)?;
    create_dir(&public, &account, true)?;
    let image = install.join("9router.exe");
    let public_image = public.join("9router.exe");
    fs::write(&image, b"fixture-original")?;
    fs::write(&public_image, b"fixture-public")?;
    if !Installation::open(&public_image, &account)
        .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
    {
        return Err(io::Error::other("World-writable installation was accepted"));
    }
    let identity = Installation::open(&image, &account)?;
    if identity.root != install.canonicalize()? {
        return Err(io::Error::other(
            "Installation resident directory is incorrect",
        ));
    }
    identity.recheck(&image)?;
    let stage = install.join("stage");
    let unsafe_stage = install.join("stage-observable");
    let outside = root.join("outside");
    create_dir(&stage, &account, false)?;
    create_dir(&unsafe_stage, &account, true)?;
    create_dir(&outside, &account, false)?;
    let safe_stage = identity.staging(&stage, &account)?;
    identity.recheck_staging(&stage, &safe_stage, &account)?;
    let moved_stage = install.join("moving-stage");
    if fs::rename(&stage, &moved_stage).is_ok() {
        create_dir(&stage, &account, false)?;
        if !identity
            .recheck_staging(&stage, &safe_stage, &account)
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
        {
            return Err(io::Error::other(
                "A substituted staging directory was accepted",
            ));
        }
        fs::remove_dir(&stage)?;
        fs::rename(&moved_stage, &stage)?;
        println!("opened-stage: share mode permitted rename; substituted identity rejected");
    }
    let moved_install = root.join("moving-install");
    if fs::rename(&install, &moved_install).is_ok() {
        create_dir(&install, &account, false)?;
        fs::write(&image, b"fixture-false-image")?;
        if !identity
            .recheck(&image)
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
        {
            return Err(io::Error::other(
                "A substituted installation directory was accepted",
            ));
        }
        fs::remove_file(&image)?;
        fs::remove_dir(&install)?;
        fs::rename(&moved_install, &install)?;
        println!("opened-install: share mode permitted rename; substituted identity rejected");
    }
    for path in [&unsafe_stage, &outside] {
        if !identity
            .staging(path, &account)
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
        {
            return Err(io::Error::other("Unsafe staging directory was accepted"));
        }
    }
    let replacement = install.join("changed.exe");
    fs::rename(&image, &replacement)?;
    fs::write(&image, b"fixture-changed")?;
    if !identity
        .recheck(&image)
        .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
    {
        return Err(io::Error::other(
            "Changed installation identity was accepted",
        ));
    }
    let link = install.join("linked");
    match std::os::windows::fs::symlink_dir(&stage, &link) {
        Ok(()) => {
            if !identity
                .staging(&link, &account)
                .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
            {
                return Err(io::Error::other("Reparse staging directory was accepted"));
            }
            println!("reparse-directory: rejected");
        }
        Err(error) => println!("reparse-directory: fixture could not create link ({error})"),
    }
    for mode in [
        Rename::Win32Extended,
        Rename::Win32,
        Rename::NativeExtended,
        Rename::Native,
        Rename::AbsoluteControl,
    ] {
        anchor_fixture(root, &account, mode)?;
    }
    println!(
        "private-directory: accepted; world-writable, foreign and changed identities: rejected"
    );
    Ok(())
}

fn anchor_fixture(root: &Path, account: &Account, mode: Rename) -> io::Result<()> {
    let label = mode.label();
    let original = root.join(label);
    let moved = root.join(format!("{label}-anchor"));
    let source = root.join(format!("{label}-candidate"));
    let leaf = format!("9router-anchor-{label}.dat");
    let initial = original.join(&leaf);
    create_dir(&original, account, false)?;
    fs::write(&initial, b"fixture-original")?;
    fs::write(&source, b"fixture-new")?;
    let anchor = Installation::open(&initial, account)?;
    let target =
        if !matches!(mode, Rename::AbsoluteControl) && fs::rename(&original, &moved).is_ok() {
            create_dir(&original, account, false)?;
            fs::write(&initial, b"fixture-substitute")?;
            moved.join(&leaf)
        } else {
            initial.clone()
        };
    match anchor.anchored_replace(&source, OsStr::new(&leaf), mode, account) {
        Ok(()) => {
            if fs::read(&target)? != b"fixture-new" || source.exists() {
                return Err(io::Error::other(
                    "Handle-relative replacement did not reach its original directory",
                ));
            }
            println!("{label}: replacement passed");
        }
        Err(error) => {
            if fs::read(&target)? != b"fixture-original" || !source.exists() {
                return Err(io::Error::other(
                    "A failed handle-relative replacement changed files",
                ));
            }
            println!(
                "{label}: replacement unavailable ({})",
                error.raw_os_error().unwrap_or_default()
            );
        }
    }
    if target != initial && fs::read(&initial)? != b"fixture-substitute" {
        return Err(io::Error::other(
            "Handle-relative replacement wrote to substituted directory",
        ));
    }
    Ok(())
}

fn response(address: &OsStr, command: &str) -> io::Result<String> {
    let mut socket = TcpStream::connect(address.to_string_lossy().as_ref())?;
    socket.set_read_timeout(Some(WAIT))?;
    socket.write_all(command.as_bytes())?;
    let mut result = String::new();
    socket.read_to_string(&mut result)?;
    Ok(result)
}
