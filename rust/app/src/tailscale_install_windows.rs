use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use winapi::um::combaseapi::{CoInitializeEx, CoUninitialize};
use winapi::um::objbase::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Cryptography::{
    CERT_NAME_SIMPLE_DISPLAY_TYPE, CertFreeCertificateContext, CertGetNameStringW,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
    MSI_INVALID_HASH_IS_FATAL, MsiGetFileSignatureInformationW,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::package::{Events, Package};

const PUBLISHER: &str = "Tailscale Inc.";
const REFUSED: &str = "The Tailscale installer signature could not be verified";

pub(in super::super) fn installed() -> Option<PathBuf> {
    ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .chain([
            PathBuf::from(r"C:\Program Files"),
            PathBuf::from(r"C:\Program Files (x86)"),
        ])
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(r"Tailscale\tailscale.exe"))
        .find(|path| path.is_file())
}

pub(super) async fn install(package: Package, events: Events) -> Result<bool, &'static str> {
    let (sender, result) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("9router-tailscale-msi".into())
        .spawn(move || {
            let _ = sender.send(verified_install(package, events));
        })
        .map_err(|_| "Could not start the Windows Installer task")?;
    result
        .await
        .map_err(|_| "Windows Installer task ended unexpectedly")?
}

fn verified_install(package: Package, events: Events) -> Result<bool, &'static str> {
    let _directory = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(package.directory())
        .map_err(|_| "Could not lock the Tailscale staging directory")?;
    let _bytes = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(package.path())
        .map_err(|_| "Could not lock the Tailscale installer")?;
    verify(package.path())?;
    if events.is_closed() {
        return Err("Tailscale setup was canceled");
    }
    elevate(package.path())
}

fn verify(path: &Path) -> Result<(), &'static str> {
    let path = wide(path.as_os_str());
    let mut certificate = null_mut();
    let status = unsafe {
        MsiGetFileSignatureInformationW(
            path.as_ptr(),
            MSI_INVALID_HASH_IS_FATAL,
            &mut certificate,
            null_mut(),
            null_mut(),
        )
    };
    let publisher = if status == 0 && !certificate.is_null() {
        let mut buffer = [0_u16; 256];
        let size = unsafe {
            CertGetNameStringW(
                certificate,
                CERT_NAME_SIMPLE_DISPLAY_TYPE,
                0,
                null(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        };
        (size > 1 && size <= buffer.len() as u32)
            .then(|| String::from_utf16_lossy(&buffer[..size as usize - 1]))
    } else {
        None
    };
    if !certificate.is_null() {
        unsafe { CertFreeCertificateContext(certificate) };
    }
    if publisher.as_deref() == Some(PUBLISHER) {
        Ok(())
    } else {
        Err(REFUSED)
    }
}

fn elevate(package: &Path) -> Result<bool, &'static str> {
    let mut system = [0_u16; 32768];
    let length = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) };
    if length == 0 || length >= system.len() as u32 {
        return Err("Could not locate the Windows system installer");
    }
    let folder = PathBuf::from(OsString::from_wide(&system[..length as usize]));
    let installer = folder.join("msiexec.exe");
    if !installer.is_file() {
        return Err("Could not locate the Windows system installer");
    }
    let cwd = wide(folder.as_os_str());
    let installer = wide(installer.as_os_str());
    let verb = wide(OsStr::new("runas"));
    let args = arguments(package)?;
    let _apartment = Apartment::enter()?;
    let mut launch = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb.as_ptr(),
        lpFile: installer.as_ptr(),
        lpParameters: args.as_ptr(),
        lpDirectory: cwd.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    if unsafe { ShellExecuteExW(&mut launch) } == 0 {
        return if unsafe { GetLastError() } == ERROR_CANCELLED {
            Err("Windows Installer approval was canceled")
        } else {
            Err("Could not open the Windows Installer")
        };
    }
    if launch.hProcess.is_null() {
        return Err("Windows Installer returned no process handle");
    }
    let mut code = 0;
    let finished = unsafe {
        let waited = WaitForSingleObject(launch.hProcess, INFINITE);
        let read = waited == WAIT_OBJECT_0 && GetExitCodeProcess(launch.hProcess, &mut code) != 0;
        CloseHandle(launch.hProcess);
        read
    };
    match (finished, code) {
        (true, 0) => Ok(false),
        (true, 3010) => Ok(true),
        (true, 1602) => Err("Windows Installer was canceled"),
        _ => Err("Windows Installer could not complete Tailscale setup"),
    }
}

fn arguments(package: &Path) -> Result<Vec<u16>, &'static str> {
    let path = package.as_os_str().encode_wide().collect::<Vec<_>>();
    if !package.is_absolute() || path.iter().any(|unit| matches!(*unit, 0 | 34)) {
        return Err("Invalid Tailscale installer path");
    }
    Ok("/i \""
        .encode_utf16()
        .chain(path)
        .chain("\" TS_NOLAUNCH=true /passive /norestart".encode_utf16())
        .chain([0])
        .collect())
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

struct Apartment;

impl Apartment {
    fn enter() -> Result<Self, &'static str> {
        if unsafe {
            CoInitializeEx(
                null_mut(),
                COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
            )
        } < 0
        {
            return Err("Could not initialize the Windows Installer prompt");
        }
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
