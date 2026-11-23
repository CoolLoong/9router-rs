use std::fs;
use std::io;
use std::path::Path;

fn marker_path(storage: &Path) -> std::path::PathBuf {
    storage.join("tunnel/cloudflared.pid")
}

pub(super) fn marker_absent(storage: &Path) -> bool {
    fs::symlink_metadata(marker_path(storage))
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

pub(super) fn check_directory(storage: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(storage.join("tunnel")) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Err(unverified());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != fs::metadata(storage)?.uid() || metadata.mode() & 0o022 != 0 {
            return Err(unverified());
        }
    }
    Ok(())
}

pub(super) fn clear_expired_marker(storage: &Path) -> io::Result<()> {
    let path = marker_path(storage);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > 16 {
        return Err(unverified());
    }
    clear(&path, &metadata)
}

#[cfg(target_os = "linux")]
fn clear(path: &Path, initial: &fs::Metadata) -> io::Result<()> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

    use rustix::fs::{Mode, OFlags, open};
    use rustix::io::Errno;
    use rustix::process::{Pid, PidfdFlags, pidfd_open};

    let same_file = |metadata: &fs::Metadata| {
        metadata.is_file()
            && metadata.ino() == initial.ino()
            && metadata.dev() == initial.dev()
            && metadata.len() == initial.len()
            && metadata.ctime() == initial.ctime()
            && metadata.ctime_nsec() == initial.ctime_nsec()
    };
    let opened = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let mut file = fs::File::from(opened);
    let metadata = file.metadata()?;
    if !same_file(&metadata) || metadata.nlink() != 1 {
        return Err(unverified());
    }
    let mut raw = String::new();
    (&mut file).take(17).read_to_string(&mut raw)?;
    let pid = raw
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
        .and_then(Pid::from_raw)
        .ok_or_else(unverified)?;
    if !matches!(pidfd_open(pid, PidfdFlags::empty()), Err(Errno::SRCH))
        || !same_file(&fs::symlink_metadata(path)?)
    {
        return Err(unverified());
    }
    fs::remove_file(path)
}

#[cfg(not(target_os = "linux"))]
fn clear(_: &Path, _: &fs::Metadata) -> io::Result<()> {
    Err(unverified())
}

fn unverified() -> io::Error {
    io::Error::from(io::ErrorKind::PermissionDenied)
}
