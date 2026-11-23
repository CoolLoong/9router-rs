use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind, Read};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{NonNull, null_mut};

use tempfile::Builder;
use winapi::shared::winerror::HRESULT;
use winapi::shared::wtypesbase::CLSCTX_INPROC_SERVER;
use winapi::um::combaseapi::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize};
use winapi::um::knownfolders::FOLDERID_Startup;
use winapi::um::objbase::COINIT_APARTMENTTHREADED;
use winapi::um::objidl::IPersistFile;
use winapi::um::shlobj::SHGetKnownFolderPath;
use winapi::um::shobjidl_core::{IShellLinkW, ShellLink};
use winapi::um::unknwnbase::IUnknown;
use winapi::{Class, Interface};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWMINNOACTIVE;

const MARKER: &str = "9Router login startup";
const ARGUMENTS: &str = "--tray --skip-update --no-browser";
const LEGACY_LIMIT: u64 = 65536;

pub(super) struct Startup {
    shortcut: PathBuf,
    script: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Entry {
    Missing,
    Owned,
    Foreign,
}

impl Startup {
    pub(super) fn new() -> io::Result<Self> {
        let mut directory = null_mut();
        hr(unsafe { SHGetKnownFolderPath(&FOLDERID_Startup, 0, null_mut(), &mut directory) })?;
        let directory =
            NonNull::new(directory).ok_or_else(|| io::Error::other("Missing Startup folder"))?;
        let path = unsafe {
            let mut length = 0;
            while *directory.as_ptr().add(length) != 0 {
                length += 1;
            }
            let path = OsString::from_wide(std::slice::from_raw_parts(directory.as_ptr(), length));
            CoTaskMemFree(directory.as_ptr().cast());
            PathBuf::from(path)
        };
        Ok(Self {
            shortcut: path.join("9router.lnk"),
            script: path.join("9router.vbs"),
        })
    }

    pub(super) fn enabled(&self) -> io::Result<bool> {
        let (shortcut, script) = self.entries()?;
        Ok(shortcut == Entry::Owned || script == Entry::Owned)
    }

    pub(super) fn toggle(&self) -> io::Result<()> {
        let (shortcut, script) = self.entries()?;
        if shortcut == Entry::Owned || script == Entry::Owned {
            for (state, path) in [(shortcut, &self.shortcut), (script, &self.script)] {
                if state == Entry::Owned {
                    fs::remove_file(path)?;
                }
            }
            return Ok(());
        }
        self.register(shortcut, script)
    }

    pub(super) fn enable(&self) -> io::Result<()> {
        let (shortcut, script) = self.entries()?;
        if shortcut == Entry::Owned || script == Entry::Owned {
            return Ok(());
        }
        self.register(shortcut, script)
    }

    fn register(&self, shortcut: Entry, script: Entry) -> io::Result<()> {
        if shortcut == Entry::Foreign || script == Entry::Foreign {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                "Startup already contains an unrecognized 9Router file",
            ));
        }
        self.write_link(&std::env::current_exe()?)
    }

    fn entries(&self) -> io::Result<(Entry, Entry)> {
        let shortcut = entry(&self.shortcut, |path| {
            let _com = Apartment::enter()?;
            let link = new_link()?;
            let persistence = persist_file(&link)?;
            hr(unsafe { persistence.0.as_ref().Load(wide_path(path).as_ptr(), 0) })?;
            let mut description = [0_u16; 256];
            let mut args = [0_u16; 128];
            unsafe {
                hr(link
                    .0
                    .as_ref()
                    .GetDescription(description.as_mut_ptr(), description.len() as i32))?;
                hr(link
                    .0
                    .as_ref()
                    .GetArguments(args.as_mut_ptr(), args.len() as i32))?;
            }
            Ok(read_wide(&description) == MARKER && read_wide(&args) == ARGUMENTS)
        })?;
        let script = entry(&self.script, |path| {
            let mut bytes = Vec::new();
            fs::File::open(path)?
                .take(LEGACY_LIMIT + 1)
                .read_to_end(&mut bytes)?;
            Ok(bytes.len() as u64 <= LEGACY_LIMIT && old_script(&bytes))
        })?;
        Ok((shortcut, script))
    }

    fn write_link(&self, exe: &Path) -> io::Result<()> {
        let startup = self
            .shortcut
            .parent()
            .ok_or_else(|| io::Error::other("Missing Startup folder"))?;
        let programs = startup
            .parent()
            .ok_or_else(|| io::Error::other("Missing Start Menu Programs folder"))?;
        let working = exe
            .parent()
            .ok_or_else(|| io::Error::other("Missing executable directory"))?;
        fs::create_dir_all(startup)?;
        let temporary = Builder::new()
            .prefix(".9router-")
            .suffix(".lnk")
            .tempfile_in(programs)?
            .into_temp_path();
        let _com = Apartment::enter()?;
        let link = new_link()?;
        unsafe {
            hr(link.0.as_ref().SetPath(wide_path(exe).as_ptr()))?;
            hr(link
                .0
                .as_ref()
                .SetWorkingDirectory(wide_path(working).as_ptr()))?;
            hr(link
                .0
                .as_ref()
                .SetArguments(super::wide(ARGUMENTS).as_ptr()))?;
            hr(link.0.as_ref().SetDescription(super::wide(MARKER).as_ptr()))?;
            hr(link.0.as_ref().SetShowCmd(SW_SHOWMINNOACTIVE))?;
        }
        let persistence = persist_file(&link)?;
        hr(unsafe {
            persistence
                .0
                .as_ref()
                .Save(wide_path(&temporary).as_ptr(), 0)
        })?;
        drop(persistence);
        drop(link);
        temporary
            .persist_noclobber(&self.shortcut)
            .map_err(|error| error.error)
    }
}

fn entry(path: &Path, owned: impl FnOnce(&Path) -> io::Result<bool>) -> io::Result<Entry> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Entry::Missing),
        Err(error) => Err(error),
        Ok(meta) if meta.is_file() && owned(path)? => Ok(Entry::Owned),
        Ok(_) => Ok(Entry::Foreign),
    }
}

fn old_script(contents: &[u8]) -> bool {
    let Ok(script) = std::str::from_utf8(contents) else {
        return false;
    };
    let mut lines = script.trim_start_matches('\u{feff}').trim_end().lines();
    if lines.next() != Some(r#"Set WshShell = CreateObject("WScript.Shell")"#) {
        return false;
    }
    let Some(line) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false;
    }
    let Some((node, cli)) = line
        .strip_prefix(r#"WshShell.Run """"#)
        .and_then(|text| text.strip_suffix(r#""" --tray --skip-update", 0, False"#))
        .and_then(|text| text.split_once(r#""" """#))
    else {
        return false;
    };
    [node, cli]
        .into_iter()
        .all(|path| Path::new(path).is_absolute() && !path.contains('"'))
        && Path::new(node)
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("node.exe"))
        && Path::new(cli)
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("cli.js"))
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

fn read_wide(input: &[u16]) -> String {
    String::from_utf16_lossy(
        &input[..input
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(input.len())],
    )
}

fn hr(status: HRESULT) -> io::Result<()> {
    if status >= 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Windows Startup operation failed (HRESULT {status:#010x})"
        )))
    }
}

struct Apartment;

impl Apartment {
    fn enter() -> io::Result<Self> {
        hr(unsafe { CoInitializeEx(null_mut(), COINIT_APARTMENTTHREADED) })?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct Com<T>(NonNull<T>);

impl<T> Drop for Com<T> {
    fn drop(&mut self) {
        unsafe { self.0.cast::<IUnknown>().as_ref().Release() };
    }
}

fn new_link() -> io::Result<Com<IShellLinkW>> {
    let mut link = null_mut();
    hr(unsafe {
        CoCreateInstance(
            &ShellLink::uuidof(),
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &IShellLinkW::uuidof(),
            &mut link,
        )
    })?;
    NonNull::new(link.cast())
        .map(Com)
        .ok_or_else(|| io::Error::other("Missing Windows Startup shortcut"))
}

fn persist_file(link: &Com<IShellLinkW>) -> io::Result<Com<IPersistFile>> {
    let mut file = null_mut();
    hr(unsafe {
        link.0
            .as_ref()
            .QueryInterface(&IPersistFile::uuidof(), &mut file)
    })?;
    NonNull::new(file.cast())
        .map(Com)
        .ok_or_else(|| io::Error::other("Missing Windows shortcut persistence interface"))
}
