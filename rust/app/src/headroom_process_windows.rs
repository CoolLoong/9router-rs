use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use windows_sys::Win32::Foundation::{
    CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ALREADY_EXISTS,
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_SHARING_VIOLATION, FILETIME, GetLastError,
    HANDLE, SetLastError, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SE_KERNEL_OBJECT};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_APPEND_DATA, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, SYNCHRONIZE,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, IsProcessInJob, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JobObjectBasicAccountingInformation, OpenJobObjectW, QueryInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
    PROCESS_QUERY_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::windows_acl::{self, Account};
use crate::windows_process::{check, lease, owned, wait, wide};

const OWNER: &str = "proxy.owner.json";
const PID: &str = "proxy.pid";
const RECORD_VERSION: u32 = 2;
const JOB_QUERY: u32 = 0x0004;
const JOB_TERMINATE: u32 = 0x0008;
const STARTUP: Duration = Duration::from_secs(8);
const LOCK_WAIT: Duration = Duration::from_secs(13);
const STOP_WAIT: Duration = Duration::from_secs(2);
const RESTART_WAIT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(40);

pub(super) struct Failure {
    pub status: StatusCode,
    pub message: String,
    pub code: &'static str,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>, code: &'static str) -> Self {
        Self {
            status,
            message: message.into(),
            code,
        }
    }

    fn state() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read or write Headroom process state",
            "STATE_FAILED",
        )
    }

    fn unknown() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "Headroom process ownership could not be verified",
            "UNVERIFIED_PROCESS",
        )
    }

    fn stop() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to stop headroom proxy",
            "STOP_FAILED",
        )
    }

    fn spawn() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to spawn headroom proxy",
            "SPAWN_FAILED",
        )
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Identity {
    pid: u32,
    creation_time: u64,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
struct Record {
    version: u32,
    nonce: String,
    process: Option<Identity>,
}

impl Record {
    fn job_name(&self) -> io::Result<Vec<u16>> {
        let uuid = uuid::Uuid::parse_str(&self.nonce).map_err(io::Error::other)?;
        wide(OsStr::new(&format!("Local\\9Router-Headroom-{uuid}")))
    }
}

struct State {
    path: PathBuf,
    account: Account,
    _directory: File,
}

impl State {
    fn open(root: &Path, create: bool) -> io::Result<Self> {
        let path = root.join("headroom");
        let account = Account::current()?;
        if create {
            match windows_acl::create_private_directory(&path, &account) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let directory = windows_acl::validated_directory(&path, &account)?;
        Ok(Self {
            path,
            account,
            _directory: directory,
        })
    }

    fn acquire(&self) -> Result<File, Failure> {
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            let error = match lease(&self.path.join("proxy.lock")) {
                Ok(file) => {
                    self.account
                        .validate(file.as_handle(), SE_FILE_OBJECT)
                        .map_err(|_| Failure::state())?;
                    return Ok(file);
                }
                Err(error) => error,
            };
            if error.raw_os_error() != Some(ERROR_SHARING_VIOLATION as i32) {
                return Err(Failure::state());
            }
            if Instant::now() >= deadline {
                return Err(Failure::new(
                    StatusCode::CONFLICT,
                    "Another Headroom operation is in progress",
                    "PROCESS_BUSY",
                ));
            }
            std::thread::sleep(POLL);
        }
    }

    fn read(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let file = match windows_acl::validated_file(&self.path.join(name), &self.account) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(16_385).read_to_end(&mut bytes)?;
        if bytes.len() > 16_384 {
            return Err(io::Error::other("Invalid Headroom process state"));
        }
        Ok(Some(bytes))
    }

    fn read_record(&self) -> io::Result<Option<Record>> {
        self.read(OWNER)?
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .transpose()
    }

    fn pid_marker(&self) -> io::Result<Option<u32>> {
        let raw = self.read(PID)?;
        Ok(raw
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|text| {
                let text = text.trim_start();
                let digits = text
                    .find(|ch: char| !ch.is_ascii_digit())
                    .unwrap_or(text.len());
                text[..digits].parse::<u32>().ok().filter(|pid| *pid > 0)
            }))
    }

    fn publish(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.path)?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        let until = Instant::now() + Duration::from_millis(250);
        loop {
            match file.persist(self.path.join(name)) {
                Ok(_) => return Ok(()),
                Err(error)
                    if error.error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
                        && Instant::now() < until =>
                {
                    file = error.file;
                    std::thread::sleep(POLL);
                }
                Err(error) => return Err(error.error),
            }
        }
    }

    fn save_record(&self, record: &Record) -> io::Result<()> {
        self.publish(
            OWNER,
            &serde_json::to_vec(record).map_err(io::Error::other)?,
        )
    }

    fn clear(&self) -> io::Result<()> {
        self.remove(PID)?;
        self.remove(OWNER)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.path.join(name)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

struct VerifiedJob {
    query: OwnedHandle,
    record: Record,
    process: Option<OwnedHandle>,
}

enum Ownership {
    Empty,
    Unknown,
    Interrupted(VerifiedJob),
    Live(VerifiedJob, u32),
}

pub(super) fn managed_pid(root: &Path) -> Option<u32> {
    let state = State::open(root, false).ok()?;
    match ownership(&state).ok()? {
        Ownership::Live(_, pid) => Some(pid),
        _ => None,
    }
}

pub(super) fn start(
    root: &Path,
    binary: Option<PathBuf>,
    port: u16,
    code_aware: bool,
    kompress: bool,
    restart: bool,
) -> Result<Value, Failure> {
    let binary = binary.ok_or_else(|| {
        Failure::new(
            StatusCode::BAD_REQUEST,
            "Headroom CLI not installed",
            "NOT_INSTALLED",
        )
    })?;
    let state = State::open(root, true).map_err(|_| Failure::state())?;
    let _lock = state.acquire()?;
    match ownership(&state)? {
        Ownership::Unknown => return Err(Failure::unknown()),
        Ownership::Live(_, pid) if !restart => {
            return Ok(json!({"success":true,"pid":pid,"alreadyRunning":true}));
        }
        Ownership::Live(job, _) | Ownership::Interrupted(job) => {
            job.stop(&state, RESTART_WAIT)?;
        }
        Ownership::Empty => state.clear().map_err(|_| Failure::state())?,
    }
    let pid = launch(root, &state, binary, port, code_aware, kompress)?;
    Ok(json!({"success":true,"pid":pid,"alreadyRunning":false}))
}

pub(super) fn stop(root: &Path) -> Result<Value, Failure> {
    let state = State::open(root, true).map_err(|_| Failure::state())?;
    let _lock = state.acquire()?;
    match ownership(&state)? {
        Ownership::Unknown => Err(Failure::unknown()),
        Ownership::Empty => {
            state.clear().map_err(|_| Failure::state())?;
            Ok(json!({"stopped":false,"reason":"not_running"}))
        }
        Ownership::Interrupted(job) => {
            job.stop(&state, STOP_WAIT)?;
            Ok(json!({"stopped":false,"reason":"not_running"}))
        }
        Ownership::Live(job, pid) => {
            job.stop(&state, STOP_WAIT)?;
            Ok(json!({"stopped":true,"pid":pid}))
        }
    }
}

fn ownership(state: &State) -> Result<Ownership, Failure> {
    let legacy = state.pid_marker().map_err(|_| Failure::state())?;
    let unbound = || {
        if legacy.is_some_and(alive) {
            Ownership::Unknown
        } else {
            Ownership::Empty
        }
    };
    let record = match state.read_record() {
        Ok(Some(record)) if record.version == RECORD_VERSION => record,
        Ok(Some(_)) => return Ok(Ownership::Unknown),
        Ok(None) => return Ok(unbound()),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => return Ok(Ownership::Unknown),
        Err(_) => return Err(Failure::state()),
    };
    if legacy
        .is_some_and(|pid| record.process.is_none_or(|identity| pid != identity.pid) && alive(pid))
    {
        return Ok(Ownership::Unknown);
    }
    let Ok(name) = record.job_name() else {
        return Ok(Ownership::Unknown);
    };
    let query = match owned(unsafe { OpenJobObjectW(JOB_QUERY | READ_CONTROL, 0, name.as_ptr()) }) {
        Ok(query) => query,
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
            return Ok(unbound());
        }
        Err(_) => return Ok(Ownership::Unknown),
    };
    if state
        .account
        .validate(query.as_handle(), SE_KERNEL_OBJECT)
        .is_err()
    {
        return Ok(Ownership::Unknown);
    }
    match active(query.as_handle()) {
        Ok(0) => return Ok(unbound()),
        Err(_) => return Ok(Ownership::Unknown),
        _ => {}
    }
    let mut job = VerifiedJob {
        query,
        record,
        process: None,
    };
    let Some(identity) = job.record.process else {
        return Ok(Ownership::Interrupted(job));
    };
    let Some(process) = open_live(identity.pid).ok().flatten() else {
        return Ok(Ownership::Unknown);
    };
    let correct = creation_time(process.as_handle()).ok() == Some(identity.creation_time)
        && member(process.as_handle(), job.query.as_handle()).unwrap_or(false);
    if !correct {
        return Ok(Ownership::Unknown);
    }
    job.process = Some(process);
    Ok(Ownership::Live(job, identity.pid))
}

fn open_live(pid: u32) -> io::Result<Option<OwnedHandle>> {
    if pid == 0 || pid == std::process::id() {
        return Err(io::Error::other("Invalid Headroom process ID"));
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | SYNCHRONIZE, 0, pid) };
    let handle = match owned(handle) {
        Ok(handle) => handle,
        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    match wait(handle.as_handle(), Duration::ZERO) {
        Ok(()) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::TimedOut => Ok(Some(handle)),
        Err(error) => Err(error),
    }
}

fn alive(pid: u32) -> bool {
    !matches!(open_live(pid), Ok(None))
}

fn creation_time(process: BorrowedHandle<'_>) -> io::Result<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    check(unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    })?;
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn member(process: BorrowedHandle<'_>, job: BorrowedHandle<'_>) -> io::Result<bool> {
    let mut linked = 0;
    check(unsafe { IsProcessInJob(process.as_raw_handle(), job.as_raw_handle(), &mut linked) })?;
    Ok(linked != 0)
}

fn active(job: BorrowedHandle<'_>) -> io::Result<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    check(unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            null_mut(),
        )
    })?;
    Ok(info.ActiveProcesses)
}

impl VerifiedJob {
    fn stop(self, state: &State, timeout: Duration) -> Result<(), Failure> {
        let name = self.record.job_name().map_err(|_| Failure::unknown())?;
        let handle = owned(unsafe { OpenJobObjectW(JOB_TERMINATE | JOB_QUERY, 0, name.as_ptr()) })
            .map_err(|_| Failure::stop())?;
        if unsafe { CompareObjectHandles(self.query.as_raw_handle(), handle.as_raw_handle()) } == 0
        {
            return Err(Failure::unknown());
        }
        terminate_job(handle.as_handle(), timeout).map_err(|_| Failure::stop())?;
        if let Some(process) = self.process {
            wait(process.as_handle(), timeout).map_err(|_| Failure::stop())?;
        }
        state.clear().map_err(|_| Failure::state())
    }
}

fn terminate_job(handle: BorrowedHandle<'_>, timeout: Duration) -> io::Result<()> {
    if active(handle)? == 0 {
        return Ok(());
    }
    check(unsafe { TerminateJobObject(handle.as_raw_handle(), 1) })?;
    let deadline = Instant::now() + timeout;
    while active(handle)? > 0 {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

fn launch(
    root: &Path,
    state: &State,
    binary: PathBuf,
    port: u16,
    code_aware: bool,
    kompress: bool,
) -> Result<u32, Failure> {
    let binary = fs::canonicalize(binary).map_err(|_| Failure::spawn())?;
    let mut command = OsString::from("\"");
    command.push(&binary);
    command.push(format!("\" proxy --port {port}"));
    if code_aware {
        command.push(" --code-aware");
    }
    if !kompress {
        command.push(" --disable-kompress");
    }
    let mut record = Record {
        version: RECORD_VERSION,
        nonce: uuid::Uuid::new_v4().to_string(),
        process: None,
    };
    let name = record.job_name().map_err(|_| Failure::state())?;
    let descriptor = state
        .account
        .descriptor(false)
        .map_err(|_| Failure::state())?;
    let attributes = windows_acl::attributes(&descriptor);
    let raw = unsafe {
        SetLastError(0);
        CreateJobObjectW(&attributes, name.as_ptr())
    };
    let duplicate = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let job = owned(raw).map_err(|_| Failure::spawn())?;
    if duplicate {
        return Err(Failure::unknown());
    }
    state
        .account
        .validate(job.as_handle(), SE_KERNEL_OBJECT)
        .map_err(|_| Failure::state())?;
    state.save_record(&record).map_err(|_| Failure::state())?;
    let result = launch_in_job(state, &binary, command, &job);
    let (process, identity) = match result {
        Ok(result) => result,
        Err(failure) => {
            if terminate_job(job.as_handle(), STOP_WAIT).is_ok() {
                let _ = state.clear();
            }
            return Err(failure);
        }
    };
    record.process = Some(identity);
    if state.save_record(&record).is_err()
        || state
            .publish(PID, identity.pid.to_string().as_bytes())
            .is_err()
    {
        if terminate_job(job.as_handle(), STOP_WAIT).is_ok() {
            let _ = state.clear();
        }
        return Err(Failure::state());
    }
    let own_root = root.to_owned();
    std::thread::spawn(move || {
        if unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0
            && let Ok(state) = State::open(&own_root, false)
            && let Ok(_lock) = state.acquire()
            && state.read_record().ok().flatten().as_ref() == Some(&record)
            && terminate_job(job.as_handle(), STOP_WAIT).is_ok()
        {
            let _ = state.clear();
        }
    });
    Ok(identity.pid)
}

fn launch_in_job(
    state: &State,
    binary: &Path,
    command: OsString,
    job: &OwnedHandle,
) -> Result<(OwnedHandle, Identity), Failure> {
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .access_mode(FILE_APPEND_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(state.path.join("proxy.log"))
        .map_err(|_| Failure::state())?;
    if !log.metadata().is_ok_and(|meta| {
        meta.is_file() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }) {
        return Err(Failure::state());
    }
    state
        .account
        .validate(log.as_handle(), SE_FILE_OBJECT)
        .map_err(|_| Failure::state())?;
    let input = File::open("NUL").map_err(|_| Failure::state())?;
    let stdin = inheritable(input.as_handle()).map_err(|_| Failure::state())?;
    let stdout = inheritable(log.as_handle()).map_err(|_| Failure::state())?;
    let job_list = [job.as_raw_handle()];
    let stdio = [stdin.as_raw_handle(), stdout.as_raw_handle()];
    let mut attributes = ProcessAttributes::new().map_err(|_| Failure::spawn())?;
    attributes
        .set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &job_list)
        .map_err(|_| Failure::spawn())?;
    attributes
        .set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &stdio)
        .map_err(|_| Failure::spawn())?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdio[0];
    startup.StartupInfo.hStdOutput = stdio[1];
    startup.StartupInfo.hStdError = stdio[1];
    startup.lpAttributeList = attributes.list();
    let mut information = PROCESS_INFORMATION::default();
    let image = wide(binary.as_os_str()).map_err(|_| Failure::spawn())?;
    let mut command = wide(&command).map_err(|_| Failure::spawn())?;
    check(unsafe {
        CreateProcessW(
            image.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_NO_WINDOW | CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT,
            null(),
            null(),
            &startup.StartupInfo,
            &mut information,
        )
    })
    .map_err(|_| Failure::spawn())?;
    drop(attributes);
    drop(stdin);
    drop(stdout);
    let process = owned(information.hProcess).map_err(|_| Failure::spawn())?;
    let thread = owned(information.hThread).map_err(|_| Failure::spawn())?;
    if !member(process.as_handle(), job.as_handle()).unwrap_or(false) {
        let _ = unsafe { TerminateProcess(process.as_raw_handle(), 1) };
        let _ = wait(process.as_handle(), STOP_WAIT);
        return Err(Failure::unknown());
    }
    let identity = Identity {
        pid: information.dwProcessId,
        creation_time: creation_time(process.as_handle()).map_err(|_| Failure::state())?,
    };
    if unsafe { ResumeThread(thread.as_raw_handle()) } != 1 {
        return Err(Failure::spawn());
    }
    match wait(process.as_handle(), STARTUP) {
        Ok(()) => {
            let mut code = 0;
            check(unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) })
                .map_err(|_| Failure::state())?;
            Err(Failure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("headroom proxy exited early (code={code}) — see proxy.log"),
                "EARLY_EXIT",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            if !member(process.as_handle(), job.as_handle()).unwrap_or(false) {
                return Err(Failure::unknown());
            }
            Ok((process, identity))
        }
        Err(_) => Err(Failure::state()),
    }
}

fn inheritable(handle: BorrowedHandle<'_>) -> io::Result<OwnedHandle> {
    let here = unsafe { GetCurrentProcess() };
    let mut duplicate = null_mut();
    check(unsafe {
        DuplicateHandle(
            here,
            handle.as_raw_handle(),
            here,
            &mut duplicate,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    })?;
    owned(duplicate)
}

struct ProcessAttributes {
    words: Vec<usize>,
}

impl ProcessAttributes {
    fn new() -> io::Result<Self> {
        let mut bytes = 0;
        let _ = unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut bytes) };
        if bytes == 0 || bytes > 16_384 {
            return Err(io::Error::other(
                "Failed to allocate Headroom process attributes",
            ));
        }
        let mut words = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        check(unsafe {
            InitializeProcThreadAttributeList(words.as_mut_ptr().cast(), 2, 0, &mut bytes)
        })?;
        Ok(Self { words })
    }

    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.words.as_mut_ptr().cast()
    }

    fn set(&mut self, attribute: u32, values: &[HANDLE]) -> io::Result<()> {
        check(unsafe {
            UpdateProcThreadAttribute(
                self.list(),
                0,
                attribute as usize,
                values.as_ptr().cast(),
                std::mem::size_of_val(values),
                null_mut(),
                null_mut(),
            )
        })
    }
}

impl Drop for ProcessAttributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}
