use std::io;

#[cfg(any(unix, all(windows, feature = "native-package")))]
use std::{fs::File, io::Read, path::Path};

#[cfg(any(unix, all(windows, feature = "native-package")))]
const LOCATIONS: &[&[&str]] = &[
    &[".next", "standalone", "logs", "translator"],
    &["logs", "translator"],
];

#[cfg(any(unix, all(windows, feature = "native-package")))]
pub(super) fn read_legacy(name: &str, limit: usize) -> io::Result<Vec<u8>> {
    let working_directory = std::env::current_dir()?;
    for location in LOCATIONS {
        match open_legacy(&working_directory, location, name) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > limit {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                return Ok(bytes);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::ErrorKind::NotFound.into())
}

#[cfg(not(any(unix, all(windows, feature = "native-package"))))]
pub(super) fn read_legacy(_: &str, _: usize) -> io::Result<Vec<u8>> {
    Err(io::ErrorKind::NotFound.into())
}

#[cfg(unix)]
fn open_legacy(_: &Path, location: &[&str], name: &str) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, open, openat};
    use std::os::unix::fs::MetadataExt;

    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let uid = rustix::process::geteuid().as_raw();
    let trusted =
        |metadata: &std::fs::Metadata| metadata.uid() == uid && metadata.mode() & 0o022 == 0;
    let mut directory = File::from(open(".", flags | OFlags::DIRECTORY, Mode::empty())?);
    if !trusted(&directory.metadata()?) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    for folder in location {
        directory = File::from(openat(
            &directory,
            *folder,
            flags | OFlags::DIRECTORY,
            Mode::empty(),
        )?);
        if !trusted(&directory.metadata()?) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    let file = File::from(openat(&directory, name, flags, Mode::empty())?);
    let metadata = file.metadata()?;
    if !trusted(&metadata) || !metadata.is_file() || metadata.nlink() != 1 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}

#[cfg(all(windows, feature = "native-package"))]
fn open_legacy(root: &Path, location: &[&str], name: &str) -> io::Result<File> {
    use crate::windows_acl::{Account, validated_directory, validated_file};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let account = Account::current()?;
    let mut path = root.to_owned();
    let mut handles = vec![validated_directory(&path, &account)?];
    for folder in location {
        path.push(folder);
        handles.push(validated_directory(&path, &account)?);
    }
    let file = validated_file(&path.join(name), &account)?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if information.nNumberOfLinks != 1 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}
