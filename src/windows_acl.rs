use std::ffi::{OsStr, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

#[cfg(not(feature = "native-package"))]
use windows_sys::Wdk::Storage::FileSystem::FileRenameInformation;
use windows_sys::Wdk::Storage::FileSystem::{FileRenameInformationEx, NtSetInformationFile};
use windows_sys::Win32::Foundation::{
    GENERIC_ALL, GENERIC_WRITE, LocalFree, RtlNtStatusToDosError,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT, SE_OBJECT_TYPE,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACCESS_DENIED_ACE, ACE_HEADER, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetTokenInformation,
    IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_EA,
    GetFileInformationByHandle, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
};
#[cfg(not(feature = "native-package"))]
use windows_sys::Win32::Storage::FileSystem::{
    FileRenameInfo, FileRenameInfoEx, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::windows_process::{check, owned, wide};

const ACCESS_ALLOWED: u8 = 0;
const ACCESS_DENIED: u8 = 1;
const MODIFY: u32 = GENERIC_ALL
    | GENERIC_WRITE
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | FILE_DELETE_CHILD
    | FILE_WRITE_ATTRIBUTES
    | FILE_WRITE_EA;

pub(crate) struct Local(pub(crate) *mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

pub(crate) struct Account {
    token_user: Vec<usize>,
}

impl Account {
    pub(crate) fn current() -> io::Result<Self> {
        let mut handle = null_mut();
        check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) })?;
        let token = owned(handle)?;
        let mut bytes = 0;
        let _ = unsafe {
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut bytes)
        };
        if (bytes as usize) < size_of::<TOKEN_USER>() {
            return Err(io::Error::last_os_error());
        }
        let mut token_user = vec![0; (bytes as usize).div_ceil(size_of::<usize>())];
        check(unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                token_user.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        })?;
        Ok(Self { token_user })
    }

    fn sid(&self) -> PSID {
        unsafe { (*self.token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    pub(crate) fn descriptor(&self, allow_everyone: bool) -> io::Result<Local> {
        self.security("OICI", "GA", allow_everyone)
    }

    pub(crate) fn channel_descriptor(&self) -> io::Result<Local> {
        self.security("", "0x00020004", false)
    }

    fn security(&self, inheritance: &str, rights: &str, allow_everyone: bool) -> io::Result<Local> {
        let mut raw = null_mut();
        check(unsafe { ConvertSidToStringSidW(self.sid(), &mut raw) })?;
        let sid = Local(raw.cast());
        let mut units = 0;
        while unsafe { *raw.add(units) } != 0 {
            units += 1;
        }
        let raw = unsafe { std::slice::from_raw_parts(raw, units) };
        let owner = String::from_utf16(raw).map_err(io::Error::other)?;
        drop(sid);
        let everyone = if allow_everyone { "(A;;GA;;;WD)" } else { "" };
        let input = wide(OsStr::new(&format!(
            "O:{owner}D:P(A;{inheritance};{rights};;;{owner})(A;{inheritance};GA;;;SY){everyone}"
        )))?;
        let mut descriptor = null_mut();
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                input.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        })?;
        Ok(Local(descriptor))
    }

    pub(crate) fn validate(
        &self,
        handle: BorrowedHandle<'_>,
        kind: SE_OBJECT_TYPE,
    ) -> io::Result<()> {
        let mut owner = null_mut();
        let mut acl = null_mut();
        let mut descriptor = null_mut();
        let result = unsafe {
            GetSecurityInfo(
                handle.as_raw_handle(),
                kind,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            )
        };
        let _descriptor = Local(descriptor);
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        if owner.is_null() || acl.is_null() || unsafe { EqualSid(owner, self.sid()) } == 0 {
            return Err(invalid("Unexpected update object owner or DACL"));
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        check(unsafe {
            GetAclInformation(
                acl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        })?;
        for index in 0..info.AceCount {
            let mut raw = null_mut();
            check(unsafe { GetAce(acl, index, &mut raw) })?;
            let header = unsafe { &*raw.cast::<ACE_HEADER>() };
            match header.AceType {
                ACCESS_DENIED if usize::from(header.AceSize) >= size_of::<ACCESS_DENIED_ACE>() => {
                    ()
                }
                ACCESS_ALLOWED
                    if usize::from(header.AceSize) >= size_of::<ACCESS_ALLOWED_ACE>() =>
                {
                    let allow = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
                    let sid = unsafe {
                        std::ptr::addr_of_mut!((*raw.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast()
                    };
                    if allow.Mask & MODIFY != 0
                        && unsafe {
                            EqualSid(sid, self.sid()) == 0
                                && IsWellKnownSid(sid, WinLocalSystemSid) == 0
                                && IsWellKnownSid(sid, WinBuiltinAdministratorsSid) == 0
                        }
                    {
                        return Err(invalid(
                            "Update object grants write access to an unrelated principal",
                        ));
                    }
                }
                _ => return Err(invalid("Unexpected update ACL entry")),
            }
        }
        Ok(())
    }
}

pub(crate) fn attributes(descriptor: &Local) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    }
}

pub(crate) fn create_private_directory(path: &Path, account: &Account) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let descriptor = account.descriptor(false)?;
    let attributes = attributes(&descriptor);
    let name = wide(path.as_os_str())?;
    check(unsafe { CreateDirectoryW(name.as_ptr(), &attributes) })
}

pub(crate) fn validated_file(path: &Path, account: &Account) -> io::Result<File> {
    use std::os::windows::io::AsHandle;
    let file = OpenOptions::new()
        .read(true)
        .access_mode(
            READ_CONTROL
                | FILE_READ_ATTRIBUTES
                | windows_sys::Win32::Storage::FileSystem::FILE_READ_DATA,
        )
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
        return Err(invalid("Expected a private regular file"));
    }
    account.validate(file.as_handle(), SE_FILE_OBJECT)?;
    Ok(file)
}

pub(crate) fn validated_directory(path: &Path, account: &Account) -> io::Result<File> {
    use std::os::windows::io::AsHandle;
    let directory = open(path, true)?;
    account.validate(directory.as_handle(), SE_FILE_OBJECT)?;
    Ok(directory)
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Identity(u32, u64);

fn open(path: &Path, directory: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .access_mode(
            READ_CONTROL | FILE_READ_ATTRIBUTES | if directory { FILE_TRAVERSE } else { 0 },
        )
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let meta = file.metadata()?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || directory && !meta.is_dir()
        || !directory && !meta.is_file()
    {
        return Err(invalid("Unexpected installation path type"));
    }
    Ok(file)
}

fn identity(file: &File) -> io::Result<Identity> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    check(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) })?;
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    if index == 0 {
        return Err(invalid(
            "Installation filesystem did not report an object identity",
        ));
    }
    Ok(Identity(information.dwVolumeSerialNumber, index))
}

pub(crate) struct Installation {
    pub(crate) root: PathBuf,
    image: Identity,
    directory: File,
}

#[derive(Clone, Copy)]
pub(crate) enum Rename {
    #[cfg(not(feature = "native-package"))]
    Win32,
    #[cfg(not(feature = "native-package"))]
    Win32Extended,
    #[cfg(not(feature = "native-package"))]
    Native,
    NativeExtended,
    #[cfg(not(feature = "native-package"))]
    AbsoluteControl,
}

#[cfg(not(feature = "native-package"))]
impl Rename {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Win32 => "FileRenameInfo",
            Self::Win32Extended => "FileRenameInfoEx",
            Self::Native => "NtFileRenameInformation",
            Self::NativeExtended => "NtFileRenameInformationEx",
            Self::AbsoluteControl => "FileRenameInfoAbsolute",
        }
    }
}

impl Installation {
    pub(crate) fn open(path: &Path, account: &Account) -> io::Result<Self> {
        use std::os::windows::io::AsHandle;
        let root = path
            .parent()
            .ok_or_else(|| invalid("Missing installation directory"))?;
        let directory = open(root, true)?;
        account.validate(directory.as_handle(), SE_FILE_OBJECT)?;
        let image = open(path, false)?;
        account.validate(image.as_handle(), SE_FILE_OBJECT)?;
        let id = identity(&image)?;
        if id.0 != identity(&directory)?.0 {
            return Err(invalid("Installation image is on another volume"));
        }
        Ok(Self {
            root: root.canonicalize()?,
            image: id,
            directory,
        })
    }

    pub(crate) fn recheck(&self, path: &Path) -> io::Result<()> {
        let directory = path
            .parent()
            .ok_or_else(|| invalid("Missing installation directory"))?;
        if identity(&open(directory, true)?)? != identity(&self.directory)?
            || identity(&open(path, false)?)? != self.image
        {
            return Err(invalid("Installation path identity changed"));
        }
        Ok(())
    }

    pub(crate) fn staging(&self, path: &Path, account: &Account) -> io::Result<File> {
        use std::os::windows::io::AsHandle;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("Missing staging parent"))?;
        if identity(&open(parent, true)?)? != identity(&self.directory)? {
            return Err(invalid("Staging directory is outside the installation"));
        }
        let staged = open(path, true)?;
        account.validate(staged.as_handle(), SE_FILE_OBJECT)?;
        if identity(&staged)?.0 != self.image.0 {
            return Err(invalid("Staging directory is on another volume"));
        }
        Ok(staged)
    }

    pub(crate) fn recheck_staging(
        &self,
        path: &Path,
        anchor: &File,
        account: &Account,
    ) -> io::Result<()> {
        if identity(&self.staging(path, account)?)? != identity(anchor)? {
            return Err(invalid("Staging path identity changed"));
        }
        Ok(())
    }

    pub(crate) fn anchored_replace(
        &self,
        source: &Path,
        leaf: &OsStr,
        mode: Rename,
        account: &Account,
    ) -> io::Result<()> {
        use std::os::windows::io::AsHandle;
        const REPLACE_IF_EXISTS: u32 = 1;
        let mut components = Path::new(leaf).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(name)) if name == leaf)
            || components.next().is_some()
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let source = OpenOptions::new()
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(source)?;
        let metadata = source.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(invalid("Candidate path is not a regular file"));
        }
        account.validate(source.as_handle(), SE_FILE_OBJECT)?;
        if identity(&source)?.0 != identity(&self.directory)?.0 {
            return Err(invalid(
                "Candidate and installation are on different volumes",
            ));
        }
        #[cfg(feature = "native-package")]
        let absolute = false;
        #[cfg(not(feature = "native-package"))]
        let absolute = matches!(mode, Rename::AbsoluteControl);
        let mut name = if absolute {
            wide(self.root.join(leaf).as_os_str())?
        } else {
            wide(leaf)?
        };
        let extended = match mode {
            Rename::NativeExtended => true,
            #[cfg(not(feature = "native-package"))]
            Rename::Win32Extended => true,
            #[cfg(not(feature = "native-package"))]
            _ => false,
        };
        name.pop();
        let bytes = size_of::<u16>() * name.len();
        let offset = offset_of!(FILE_RENAME_INFO, FileName);
        let len = (offset + bytes).max(size_of::<FILE_RENAME_INFO>());
        let mut buffer = vec![0usize; len.div_ceil(size_of::<usize>())];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            if extended {
                (*info).Anonymous.Flags = REPLACE_IF_EXISTS;
            } else {
                (*info).Anonymous.ReplaceIfExists = true;
            }
            (*info).RootDirectory = if absolute {
                null_mut()
            } else {
                self.directory.as_raw_handle()
            };
            (*info).FileNameLength = u32::try_from(bytes).map_err(io::Error::other)?;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                std::ptr::addr_of_mut!((*info).FileName).cast(),
                name.len(),
            );
            let len = u32::try_from(len).map_err(io::Error::other)?;
            match mode {
                #[cfg(not(feature = "native-package"))]
                Rename::Win32 | Rename::Win32Extended | Rename::AbsoluteControl => {
                    check(SetFileInformationByHandle(
                        source.as_raw_handle(),
                        if matches!(mode, Rename::Win32Extended) {
                            FileRenameInfoEx
                        } else {
                            FileRenameInfo
                        },
                        info.cast(),
                        len,
                    ))
                }
                _ => {
                    let mut result = IO_STATUS_BLOCK::default();
                    #[cfg(feature = "native-package")]
                    let class = FileRenameInformationEx;
                    #[cfg(not(feature = "native-package"))]
                    let class = if matches!(mode, Rename::NativeExtended) {
                        FileRenameInformationEx
                    } else {
                        FileRenameInformation
                    };
                    let status = NtSetInformationFile(
                        source.as_raw_handle(),
                        &mut result,
                        info.cast(),
                        len,
                        class,
                    );
                    if status == 0 {
                        Ok(())
                    } else {
                        Err(io::Error::from_raw_os_error(
                            RtlNtStatusToDosError(status) as i32
                        ))
                    }
                }
            }
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
