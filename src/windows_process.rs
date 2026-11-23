use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
#[cfg(not(feature = "native-package"))]
use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, SYNCHRONIZE,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateEventW, CreateProcessW, GetCurrentProcess,
    GetExitCodeProcess, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, SetEvent,
    TerminateProcess, WaitForMultipleObjects, WaitForSingleObject,
};

pub(crate) fn lease(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("Invalid update lock file"));
    }
    Ok(file)
}

pub(crate) fn private_exit_handle() -> io::Result<OwnedHandle> {
    let current = unsafe { GetCurrentProcess() };
    let mut result = null_mut();
    check(unsafe { DuplicateHandle(current, current, current, &mut result, SYNCHRONIZE, 0, 0) })?;
    owned(result)
}

pub(crate) fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub(crate) fn event() -> io::Result<OwnedHandle> {
    owned(unsafe { CreateEventW(null(), 1, 0, null()) })
}

pub(crate) fn signal(handle: BorrowedHandle<'_>) -> io::Result<()> {
    check(unsafe { SetEvent(handle.as_raw_handle()) })
}

pub(crate) fn wait(handle: BorrowedHandle<'_>, timeout: Duration) -> io::Result<()> {
    waited(unsafe { WaitForSingleObject(handle.as_raw_handle(), millis(timeout)) }).map(drop)
}

fn millis(timeout: Duration) -> u32 {
    timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32
}

fn waited(result: u32) -> io::Result<u32> {
    match result {
        WAIT_OBJECT_0 | 1 => Ok(result),
        WAIT_TIMEOUT => Err(io::ErrorKind::TimedOut.into()),
        _ => Err(io::Error::last_os_error()),
    }
}

pub(crate) struct Child {
    process: OwnedHandle,
    thread: Option<OwnedHandle>,
    id: u32,
}

impl Child {
    pub(crate) fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn duplicate(&self, handle: BorrowedHandle<'_>) -> io::Result<u64> {
        if self.thread.is_none() {
            return Err(io::Error::other(
                "Child must be suspended before sending handles",
            ));
        }
        let mut flags = 0;
        check(unsafe { GetHandleInformation(handle.as_raw_handle(), &mut flags) })?;
        if flags & HANDLE_FLAG_INHERIT != 0 {
            return Err(io::Error::other("Update handle is inheritable"));
        }
        let current = unsafe { GetCurrentProcess() };
        let mut remote = null_mut();
        check(unsafe {
            DuplicateHandle(
                current,
                handle.as_raw_handle(),
                self.process.as_raw_handle(),
                &mut remote,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        })?;
        Ok(remote as usize as u64)
    }

    pub(crate) fn resume(&mut self) -> io::Result<()> {
        let thread = self
            .thread
            .as_ref()
            .ok_or_else(|| io::Error::other("Child is not suspended"))?;
        if unsafe { ResumeThread(thread.as_raw_handle()) } != 1 {
            return Err(io::Error::last_os_error());
        }
        self.thread.take();
        Ok(())
    }
    pub(crate) fn wait(&self, timeout: Duration) -> io::Result<u32> {
        wait(self.process.as_handle(), timeout)?;
        let mut code = 0;
        check(unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) })?;
        Ok(code)
    }

    pub(crate) fn ready(&self, event: BorrowedHandle<'_>, timeout: Duration) -> io::Result<()> {
        let handles = [self.process.as_raw_handle(), event.as_raw_handle()];
        let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, millis(timeout)) };
        if waited(result)? == WAIT_OBJECT_0 {
            return Err(io::Error::other(
                "Updated process exited before startup completed",
            ));
        }
        Ok(())
    }

    pub(crate) fn terminate(&self, timeout: Duration) -> io::Result<()> {
        match self.wait(Duration::ZERO) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                check(unsafe { TerminateProcess(self.process.as_raw_handle(), 1) })?;
                self.wait(timeout).map(drop)
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.terminate(Duration::from_secs(3));
        }
    }
}

#[cfg(not(feature = "native-package"))]
pub(crate) fn spawn(image: &Path, arguments: &[impl AsRef<OsStr>]) -> io::Result<Child> {
    start(image, arguments, false, false)
}

pub(crate) fn suspend(image: &Path, arguments: &[impl AsRef<OsStr>]) -> io::Result<Child> {
    start(image, arguments, true, false)
}

#[cfg(not(feature = "native-package"))]
pub(crate) fn spawn_unrelated(image: &Path, arguments: &[impl AsRef<OsStr>]) -> io::Result<Child> {
    start(image, arguments, false, true)
}

fn start(
    image: &Path,
    arguments: &[impl AsRef<OsStr>],
    suspend: bool,
    inherit_all: bool,
) -> io::Result<Child> {
    let application = wide(image.as_os_str())?;
    let mut command = Vec::new();
    quote(&application[..application.len() - 1], &mut command);
    for arg in arguments {
        let arg = wide(arg.as_ref())?;
        command.push(b' ' as u16);
        quote(&arg[..arg.len() - 1], &mut command);
    }
    command.push(0);
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let flags = CREATE_NO_WINDOW | if suspend { CREATE_SUSPENDED } else { 0 };
    let mut child = PROCESS_INFORMATION::default();
    check(unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            i32::from(inherit_all),
            flags,
            null(),
            null(),
            &startup,
            &mut child,
        )
    })?;
    let process = owned(child.hProcess)?;
    let thread = owned(child.hThread)?;
    Ok(Child {
        process,
        thread: suspend.then_some(thread),
        id: child.dwProcessId,
    })
}

#[cfg(not(feature = "native-package"))]
pub(crate) fn replace(path: &Path, replacement: &Path, backup: Option<&Path>) -> io::Result<()> {
    let path = wide(path.as_os_str())?;
    let replacement = wide(replacement.as_os_str())?;
    let backup = backup.map(|backup| wide(backup.as_os_str())).transpose()?;
    check(unsafe {
        ReplaceFileW(
            path.as_ptr(),
            replacement.as_ptr(),
            backup.as_ref().map_or(null(), Vec::as_ptr),
            0,
            null(),
            null(),
        )
    })
}

pub(crate) fn check(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut encoded = value.encode_wide().collect::<Vec<_>>();
    if encoded.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    encoded.push(0);
    Ok(encoded)
}

fn quote(arg: &[u16], command: &mut Vec<u16>) {
    const QUOTE: u16 = b'"' as u16;
    const SLASH: u16 = b'\\' as u16;
    command.push(QUOTE);
    let mut slashes = 0;
    for value in arg {
        if *value == SLASH {
            slashes += 1;
        } else {
            command.extend(std::iter::repeat_n(
                SLASH,
                if *value == QUOTE {
                    2 * slashes + 1
                } else {
                    slashes
                },
            ));
            command.push(*value);
            slashes = 0;
        }
    }
    command.extend(std::iter::repeat_n(SLASH, 2 * slashes));
    command.push(QUOTE);
}
