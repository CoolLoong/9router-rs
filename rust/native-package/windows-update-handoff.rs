use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, OwnedHandle};
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::SE_KERNEL_OBJECT;
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

use crate::windows_acl::{Account, attributes};
use crate::windows_process::{self as process, Child, owned, wide};

const MAX_HANDLES: usize = 4;
const MAGIC: [u8; 8] = *b"9RUPDIPC";
const LENGTH: usize = 8 + size_of::<u32>() * 2 + size_of::<u64>() * MAX_HANDLES;
const PREFIX: &str = "Local\\9router-update-";

pub(crate) struct Handoff {
    name: OsString,
    mapping: OwnedHandle,
}

struct View(MEMORY_MAPPED_VIEW_ADDRESS);

impl View {
    fn open(mapping: &OwnedHandle, mode: u32) -> io::Result<Self> {
        let value = unsafe { MapViewOfFile(mapping.as_raw_handle(), mode, 0, 0, LENGTH) };
        if value.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(value))
    }
}

impl Drop for View {
    fn drop(&mut self) {
        unsafe { UnmapViewOfFile(self.0) };
    }
}

impl Handoff {
    pub(crate) fn create(account: &Account) -> io::Result<Self> {
        let mut suffix = [0u8; 16];
        if unsafe {
            BCryptGenRandom(
                null_mut(),
                suffix.as_mut_ptr(),
                suffix.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        } != 0
        {
            return Err(io::Error::other(
                "Could not create a private update channel name",
            ));
        }
        let suffix = suffix
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let name = OsString::from(format!("{PREFIX}{suffix}"));
        let wide = wide(&name)?;
        let descriptor = account.channel_descriptor()?;
        let attributes = attributes(&descriptor);
        let raw = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                &attributes,
                PAGE_READWRITE,
                0,
                LENGTH as u32,
                wide.as_ptr(),
            )
        };
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let mapping = owned(raw)?;
        if existed {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        account.validate(mapping.as_handle(), SE_KERNEL_OBJECT)?;
        Ok(Self { name, mapping })
    }

    pub(crate) fn start(&self, image: &Path, args: &[OsString]) -> io::Result<Child> {
        let mut arguments = args.to_vec();
        arguments.push(self.name.clone());
        process::suspend(image, &arguments)
    }

    pub(crate) fn name(&self) -> &OsStr {
        &self.name
    }

    pub(crate) fn assign(&self, child: &Child, handles: &[BorrowedHandle<'_>]) -> io::Result<()> {
        if handles.is_empty() || handles.len() > MAX_HANDLES {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let view = View::open(&self.mapping, FILE_MAP_WRITE)?;
        let mut data = [0u8; LENGTH];
        data[..8].copy_from_slice(&MAGIC);
        data[8..12].copy_from_slice(&child.id().to_le_bytes());
        data[12..16].copy_from_slice(&(handles.len() as u32).to_le_bytes());
        for (slot, handle) in data[16..].chunks_exact_mut(8).zip(handles.iter()) {
            slot.copy_from_slice(&child.duplicate(*handle)?.to_le_bytes());
        }
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), view.0.Value.cast(), LENGTH) };
        Ok(())
    }
}

#[cfg(not(feature = "native-package"))]
pub(crate) fn published_read_only(name: &OsStr) -> io::Result<bool> {
    let name = wide(name)?;
    let result = owned(unsafe { OpenFileMappingW(FILE_MAP_WRITE, 0, name.as_ptr()) });
    match result {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(true),
        Err(error) => Err(error),
    }
}

pub(crate) fn receive(
    name: &OsStr,
    count: usize,
    account: &Account,
) -> io::Result<Vec<OwnedHandle>> {
    if !(1..=MAX_HANDLES).contains(&count)
        || !name.to_str().is_some_and(|name| {
            name.strip_prefix(PREFIX).is_some_and(|suffix| {
                suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        })
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let name = wide(name)?;
    let mapping =
        owned(unsafe { OpenFileMappingW(FILE_MAP_READ | READ_CONTROL, 0, name.as_ptr()) })?;
    account.validate(mapping.as_handle(), SE_KERNEL_OBJECT)?;
    let view = View::open(&mapping, FILE_MAP_READ)?;
    let mut data = [0u8; LENGTH];
    unsafe { std::ptr::copy_nonoverlapping(view.0.Value.cast(), data.as_mut_ptr(), LENGTH) };
    if data[..8] != MAGIC
        || data[8..12] != unsafe { GetCurrentProcessId() }.to_le_bytes()
        || data[12..16] != (count as u32).to_le_bytes()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Update channel is bound to another process",
        ));
    }
    let mut result = Vec::with_capacity(count);
    for slot in data[16..].chunks_exact(8).take(count) {
        let value = u64::from_le_bytes(slot.try_into().map_err(io::Error::other)?);
        let handle = usize::try_from(value).map_err(io::Error::other)? as _;
        result.push(owned(handle)?);
    }
    Ok(result)
}
