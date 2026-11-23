use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::mem::{offset_of, size_of};
use std::net::{SocketAddr, SocketAddrV4, TcpStream};
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::post};
use serde_json::json;
use tokio::sync::{Mutex, Notify};
use url::Url;
use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};

use crate::native_update_source::{self as source, SOURCE};
use crate::storage::Storage;
use crate::windows_acl::{self, Account, Installation, Rename};
use crate::windows_handoff::{self as handoff, Handoff};
use crate::windows_process::{self as process, Child};

#[path = "windows_update_journal.rs"]
mod journal;
use journal::{CANDIDATE, COMMITTED, HELPER, JOURNAL, Journal, PREPARED, Plan, STAGE};

const HELPER_ARG: &str = "--9router-windows-update-helper";
const CHILD_ARG: &str = "--9router-windows-update-child";
const ACK_WAIT: Duration = Duration::from_secs(9);
const EXIT_WAIT: Duration = Duration::from_secs(45);
const READY_WAIT: Duration = Duration::from_secs(20);
const EXIT_INTERRUPT: i32 = 23;

pub(crate) struct ChildEntry {
    installation: File,
    data: File,
    pub(crate) root: PathBuf,
    commit: Arc<OwnedHandle>,
    supervisor: OwnedHandle,
}

pub(crate) fn parse_entry(args: &mut Vec<String>) -> io::Result<Option<ChildEntry>> {
    match args.first().map(String::as_str) {
        Some(HELPER_ARG) => {
            if args.len() != 4 {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            helper(Path::new(&args[2]), &args[1], &args[3])?;
            std::process::exit(0);
        }
        Some(CHILD_ARG) => {
            if args.len() < 5 || source::current_version().to_string() != args[3] {
                return Err(io::Error::other(
                    "Candidate version does not match the signed update",
                ));
            }
            let account = Account::current()?;
            let mut handles = handoff::receive(args[1].as_ref(), 4, &account)?.into_iter();
            let root = PathBuf::from(&args[2]);
            let installation = File::from(handles.next().ok_or(io::ErrorKind::InvalidInput)?);
            let data = File::from(handles.next().ok_or(io::ErrorKind::InvalidInput)?);
            let commit = Arc::new(handles.next().ok_or(io::ErrorKind::InvalidInput)?);
            let supervisor = handles.next().ok_or(io::ErrorKind::InvalidInput)?;
            if !installation.metadata()?.is_file() || !data.metadata()?.is_file() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            args.drain(..4);
            Ok(Some(ChildEntry {
                installation,
                data,
                root,
                commit,
                supervisor,
            }))
        }
        _ => Ok(None),
    }
}

pub(crate) struct Runtime {
    installation: Option<File>,
    image: Option<Installation>,
    data: Option<File>,
    commit: Option<Arc<OwnedHandle>>,
    supervisor: Option<OwnedHandle>,
}

impl Runtime {
    pub(crate) fn early(root: Option<&Path>, child: Option<ChildEntry>) -> io::Result<Self> {
        if let Some(child) = child {
            let image = Account::current()
                .and_then(|account| Installation::open(&std::env::current_exe()?, &account))
                .ok();
            return Ok(Self {
                installation: Some(child.installation),
                image,
                data: Some(child.data),
                commit: Some(child.commit),
                supervisor: Some(child.supervisor),
            });
        }
        let Some(root) = root else {
            return Ok(Self {
                installation: None,
                image: None,
                data: None,
                commit: None,
                supervisor: None,
            });
        };
        fs::create_dir_all(root)?;
        let data = process::lease(&root.join(".9router-windows.lock"))?;
        let executable = std::env::current_exe()?;
        let (installation, stage) = if executable
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("9router.exe"))
        {
            let parent = executable.parent().ok_or(io::ErrorKind::InvalidInput)?;
            let stage = parent.join(STAGE);
            match process::lease(&parent.join(".9router-update.lock")) {
                Ok(lock) => (Some(lock), Some(stage)),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
                    ) && fs::symlink_metadata(&stage)
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) =>
                {
                    (None, None)
                }
                Err(error) => return Err(error),
            }
        } else {
            (None, None)
        };
        let mut image = None;
        if let (Some(installation), Some(stage)) = (&installation, stage) {
            let account = Account::current()?;
            match fs::symlink_metadata(&stage) {
                Ok(_) => {
                    let checked = Installation::open(&executable, &account)?;
                    let _stage = checked.staging(&stage, &account)?;
                    if let Some(journal) = pending(&stage, &account)? {
                        if journal.plan.install.canonicalize()? != executable.canonicalize()?
                            || journal.plan.data.canonicalize()? != root.canonicalize()?
                        {
                            return Err(io::Error::other(
                                "Recovery installation and store must match the committed journal",
                            ));
                        }
                        launch_helper(
                            &stage,
                            "recover",
                            installation,
                            &data,
                            &journal.plan,
                            &account,
                        )?;
                        std::process::exit(0);
                    }
                    image = Some(checked);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    image = Installation::open(&executable, &account).ok();
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            installation,
            image,
            data: Some(data),
            commit: None,
            supervisor: None,
        })
    }

    pub(crate) fn activate(
        mut self,
        storage: Storage,
        shutdown: Arc<Notify>,
        address: SocketAddr,
        arguments: Vec<String>,
    ) -> Updater {
        if let (Some(commit), Some(supervisor)) = (&self.commit, self.supervisor.take()) {
            let commit = commit.clone();
            let shutdown = shutdown.clone();
            tokio::task::spawn_blocking(move || {
                let until = Instant::now() + READY_WAIT;
                while Instant::now() < until {
                    if process::wait(commit.as_handle(), Duration::from_millis(75)).is_ok() {
                        return;
                    }
                    if process::wait(supervisor.as_handle(), Duration::ZERO).is_ok() {
                        break;
                    }
                }
                if process::wait(commit.as_handle(), Duration::ZERO).is_err() {
                    shutdown.notify_one();
                }
            });
        }
        Updater(Arc::new(StateData {
            runtime: self,
            storage,
            shutdown,
            address,
            arguments,
            update: Mutex::new(false),
        }))
    }
}

#[derive(Clone)]
pub(crate) struct Updater(Arc<StateData>);

struct StateData {
    runtime: Runtime,
    storage: Storage,
    shutdown: Arc<Notify>,
    address: SocketAddr,
    arguments: Vec<String>,
    update: Mutex<bool>,
}

impl Updater {
    pub(crate) fn router(&self) -> Router {
        Router::new()
            .route("/api/version/update", post(update))
            .with_state(self.clone())
    }

    pub(crate) async fn gate(
        State(updater): State<Self>,
        request: Request,
        next: Next,
    ) -> Response {
        if updater
            .0
            .runtime
            .commit
            .as_ref()
            .is_none_or(|event| process::wait(event.as_handle(), Duration::ZERO).is_ok())
            || matches!(request.uri().path(), "/api/health" | "/api/version")
        {
            return next.run(request).await;
        }
        reply(
            StatusCode::SERVICE_UNAVAILABLE,
            false,
            "The signed preview is still being checked",
        )
    }
}

async fn update(State(updater): State<Updater>, request: Request) -> Response {
    let state = &updater.0;
    if !crate::dashboard_session::has_session(&state.storage, request.headers()) {
        return reply(StatusCode::UNAUTHORIZED, false, "Unauthorized");
    }
    if !same_origin(&request) {
        return reply(
            StatusCode::FORBIDDEN,
            false,
            "The native update requires a same-origin request",
        );
    }
    let Ok(mut update) = state.update.try_lock() else {
        return reply(
            StatusCode::CONFLICT,
            false,
            "A native update is already in progress",
        );
    };
    if *update {
        return reply(
            StatusCode::CONFLICT,
            false,
            "A native update is already in progress",
        );
    }
    let (Some(installation), Some(image), Some(data), SocketAddr::V4(address)) = (
        &state.runtime.installation,
        &state.runtime.image,
        &state.runtime.data,
        state.address,
    ) else {
        return reply(
            StatusCode::CONFLICT,
            false,
            "Native update requires a persistent 9router.exe installation and an IPv4 listener",
        );
    };
    let source = match &*SOURCE {
        Ok(Some(source)) => source,
        Ok(None) => {
            return reply(
                StatusCode::SERVICE_UNAVAILABLE,
                false,
                "A signed native release source is not configured",
            );
        }
        Err(error) => return reply(StatusCode::SERVICE_UNAVAILABLE, false, error.message()),
    };
    let release = match source.latest().await {
        Ok(release) if release.available() => release,
        Ok(_) => {
            return reply(
                StatusCode::CONFLICT,
                false,
                "No newer native release is available",
            );
        }
        Err(error) => return reply(StatusCode::BAD_GATEWAY, false, error.message()),
    };
    let staged = stage(
        source,
        &release,
        &state.storage,
        address,
        &state.arguments,
        installation,
        image,
        data,
    )
    .await;
    if staged.is_err() {
        return reply(
            StatusCode::CONFLICT,
            false,
            "The signed update could not be safely staged",
        );
    }
    *update = true;
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        shutdown.notify_one();
    });
    reply(
        StatusCode::OK,
        true,
        "Native update verified and staged. This app will restart shortly.",
    )
}

async fn stage(
    source: &source::Source,
    release: &source::Release,
    storage: &Storage,
    address: SocketAddrV4,
    arguments: &[String],
    installation: &File,
    checked: &Installation,
    data: &File,
) -> io::Result<()> {
    let install = std::env::current_exe()?.canonicalize()?;
    let data_dir = storage.root().canonicalize()?;
    let account = Account::current()?;
    let _data = windows_acl::validated_directory(&data_dir, &account)?;
    checked.recheck(&install)?;
    let stage = checked.root.join(STAGE);
    if stage.exists() {
        let directory = checked.staging(&stage, &account)?;
        if pending(&stage, &account)?.is_some() {
            return Err(io::Error::other("A pending recovery must complete first"));
        }
        drop(directory);
        fs::remove_dir_all(&stage)?;
    }
    windows_acl::create_private_directory(&stage, &account)?;
    let directory = checked.staging(&stage, &account)?;
    let (_, next, expires) = release.artifact_identity();
    let result = async {
        source
            .download(release, &stage.join(CANDIDATE))
            .await
            .map_err(|error| io::Error::other(error.message()))?;
        checked.recheck(&install)?;
        checked.recheck_staging(&stage, &directory, &account)?;
        let old = journal::checksum(&install, &account)?;
        fs::copy(&install, stage.join(HELPER))?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(stage.join(HELPER))?
            .sync_all()?;
        let plan = Plan {
            install,
            data: data_dir,
            address,
            arguments: arguments.to_vec(),
            old_version: source::current_version().to_string(),
            next_version: release.version.to_string(),
            old,
            next,
            expires,
        };
        journal::publish(&stage, PREPARED, &plan)?;
        launch_helper(&stage, "update", installation, data, &plan, &account)
    }
    .await;
    if result.is_err() {
        drop(directory);
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

fn pending(stage: &Path, account: &Account) -> io::Result<Option<Journal>> {
    let Some(journal) = (match journal::read(stage, JOURNAL, account) {
        Ok(value) => Some(value),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    }) else {
        return Ok(None);
    };
    if journal::committed(stage, account)? {
        return Ok(None);
    }
    Ok(Some(journal))
}

fn launch_helper(
    stage: &Path,
    mode: &str,
    installation: &File,
    data: &File,
    plan: &Plan,
    account: &Account,
) -> io::Result<()> {
    let helper = stage.join(HELPER);
    if journal::checksum(&helper, account)? != plan.old {
        return Err(io::Error::other(
            "Recovery helper differs from the original image",
        ));
    }
    let parent = process::private_exit_handle()?;
    let ack = process::event()?;
    let channel = Handoff::create(account)?;
    let mut child = channel.start(
        &helper,
        &[
            OsString::from(HELPER_ARG),
            mode.into(),
            stage.as_os_str().to_owned(),
        ],
    )?;
    channel.assign(
        &child,
        &[
            installation.as_handle(),
            data.as_handle(),
            parent.as_handle(),
            ack.as_handle(),
        ],
    )?;
    child.resume()?;
    if let Err(error) = child.ready(ack.as_handle(), ACK_WAIT) {
        child.terminate(ACK_WAIT)?;
        return Err(error);
    }
    Ok(())
}

fn helper(stage: &Path, mode: &str, channel: &str) -> io::Result<()> {
    let account = Account::current()?;
    let executable = std::env::current_exe()?.canonicalize()?;
    if executable != stage.join(HELPER).canonicalize()? {
        return Err(io::Error::other("Unexpected recovery helper path"));
    }
    let mut handles = handoff::receive(channel.as_ref(), 4, &account)?.into_iter();
    let installation = handles.next().ok_or(io::ErrorKind::InvalidInput)?;
    let data = handles.next().ok_or(io::ErrorKind::InvalidInput)?;
    let parent = handles.next().ok_or(io::ErrorKind::InvalidInput)?;
    let ack = handles.next().ok_or(io::ErrorKind::InvalidInput)?;
    let recovery = match mode {
        "recover" => Some(pending(stage, &account)?.ok_or(io::ErrorKind::InvalidData)?),
        "update" => None,
        _ => return Err(io::ErrorKind::InvalidInput.into()),
    };
    let plan = match &recovery {
        Some(journal) => journal.plan.clone(),
        None => journal::read(stage, PREPARED, &account)?,
    };
    let check = Installation::open(&plan.install, &account)?;
    let directory = check.staging(stage, &account)?;
    let _data_root = windows_acl::validated_directory(&plan.data, &account)?;
    if plan
        .install
        .parent()
        .and_then(|root| root.join(STAGE).canonicalize().ok())
        .as_ref()
        != Some(&stage.canonicalize()?)
    {
        return Err(io::Error::other(
            "Recovery directory is outside the installation",
        ));
    }
    if journal::checksum(&executable, &account)? != plan.old {
        return Err(io::Error::other("Unexpected recovery helper bytes"));
    }
    process::signal(ack.as_handle())?;
    process::wait(parent.as_handle(), EXIT_WAIT)?;
    check.recheck_staging(stage, &directory, &account)?;
    let mut update_failed = false;
    let journal = if let Some(journal) = recovery {
        journal
    } else {
        if !journal::checksum(&stage.join(CANDIDATE), &account).is_ok_and(|hash| hash == plan.next)
            || chrono::Utc::now().timestamp() >= plan.expires
        {
            resume_previous(&plan, &installation, &data, &account)?;
            return Err(io::Error::other("The signed candidate is no longer usable"));
        }
        let journal = match journal::snapshot(plan.clone(), stage, &account) {
            Ok(journal) => journal,
            Err(error) => {
                resume_previous(&plan, &installation, &data, &account)?;
                return Err(error);
            }
        };
        let launch = check.recheck(&plan.install).and_then(|_| {
            check.anchored_replace(
                &stage.join(CANDIDATE),
                "9router.exe".as_ref(),
                Rename::NativeExtended,
                &account,
            )?;
            fixture_interrupt("after-swap");
            launch_candidate(
                &plan,
                &plan.next_version,
                &installation,
                &data,
                &account,
                Some(stage),
            )
        });
        if launch.is_ok() {
            return Ok(());
        }
        if journal::committed(stage, &account)? {
            return launch;
        }
        update_failed = true;
        journal
    };
    journal::restore(&journal, stage, &account)?;
    launch_candidate(
        &plan,
        &plan.old_version,
        &installation,
        &data,
        &account,
        Some(stage),
    )?;
    if update_failed {
        return Err(io::Error::other(
            "Update failed and the previous Windows gateway was restored",
        ));
    }
    Ok(())
}

fn resume_previous(
    plan: &Plan,
    installation: &OwnedHandle,
    data: &OwnedHandle,
    account: &Account,
) -> io::Result<()> {
    launch_candidate(plan, &plan.old_version, installation, data, account, None)
}

fn launch_candidate(
    plan: &Plan,
    version: &str,
    installation: &OwnedHandle,
    data: &OwnedHandle,
    account: &Account,
    stage: Option<&Path>,
) -> io::Result<()> {
    let commit = process::event()?;
    let parent = process::private_exit_handle()?;
    let channel = Handoff::create(account)?;
    let mut args = vec![
        OsString::from(CHILD_ARG),
        channel.name().to_owned(),
        plan.data.as_os_str().to_owned(),
        version.into(),
    ];
    args.extend(plan.arguments.iter().map(OsString::from));
    args.extend([
        "--host".into(),
        plan.address.ip().to_string().into(),
        "--port".into(),
        plan.address.port().to_string().into(),
        "--no-browser".into(),
        "--skip-update".into(),
    ]);
    let mut child = process::suspend(&plan.install, &args)?;
    channel.assign(
        &child,
        &[
            installation.as_handle(),
            data.as_handle(),
            commit.as_handle(),
            parent.as_handle(),
        ],
    )?;
    child.resume()?;
    let ready = readiness(&child, plan.address, version).and_then(|_| {
        if let Some(stage) = stage {
            journal::publish(stage, COMMITTED, &true)?;
        }
        process::signal(commit.as_handle())
    });
    if ready.is_err() {
        child.terminate(ACK_WAIT)?;
    }
    ready
}

fn readiness(child: &Child, address: SocketAddrV4, version: &str) -> io::Result<()> {
    let deadline = Instant::now() + READY_WAIT;
    let probe = if address.ip().is_unspecified() {
        SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, address.port())
    } else {
        address
    };
    while Instant::now() < deadline {
        if child.wait(Duration::ZERO).is_ok() {
            return Err(io::Error::other(
                "Gateway exited before the readiness check",
            ));
        }
        if owned_listener(child, address)?
            && http(probe, "/api/health").is_ok_and(|body| body["ok"] == true)
        {
            let reported = http(probe, "/api/version")?;
            if reported["currentVersion"] != version {
                return Err(io::Error::other(
                    "Gateway running version differs from the signed manifest",
                ));
            }
            if owned_listener(child, address)?
                && child
                    .wait(Duration::ZERO)
                    .is_err_and(|err| err.kind() == io::ErrorKind::TimedOut)
            {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    Err(io::ErrorKind::TimedOut.into())
}

fn owned_listener(child: &Child, address: SocketAddrV4) -> io::Result<bool> {
    let mut size = 0u32;
    let status = unsafe {
        GetExtendedTcpTable(null_mut(), &mut size, 0, 2, TCP_TABLE_OWNER_PID_LISTENER, 0)
    };
    if status != ERROR_INSUFFICIENT_BUFFER || size == 0 || size > 4 * 1024 * 1024 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    let status = unsafe {
        GetExtendedTcpTable(
            data.as_mut_ptr().cast(),
            &mut size,
            0,
            2,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if status == ERROR_INSUFFICIENT_BUFFER {
        return Ok(false);
    }
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let offset = offset_of!(MIB_TCPTABLE_OWNER_PID, table);
    let count = unsafe { *data.as_ptr().cast::<u32>() } as usize;
    if count > (size as usize).saturating_sub(offset) / size_of::<MIB_TCPROW_OWNER_PID>() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let table = unsafe {
        std::slice::from_raw_parts(
            data.as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<MIB_TCPROW_OWNER_PID>(),
            count,
        )
    };
    Ok(table.iter().any(|row| {
        row.dwOwningPid == child.id()
            && row.dwLocalAddr.to_ne_bytes() == address.ip().octets()
            && u16::from_be(row.dwLocalPort as u16) == address.port()
    }))
}

fn http(address: SocketAddrV4, path: &str) -> io::Result<serde_json::Value> {
    let mut stream = TcpStream::connect_timeout(&address.into(), Duration::from_secs(1))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )?;
    let mut data = String::new();
    stream.take(32 * 1024).read_to_string(&mut data)?;
    if !data.starts_with("HTTP/1.1 200") {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let (_, body) = data
        .split_once("\r\n\r\n")
        .ok_or(io::ErrorKind::InvalidData)?;
    serde_json::from_str(body).map_err(io::Error::other)
}

fn fixture_interrupt(point: &str) {
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        && std::env::var("NINEROUTER_WINDOWS_UPDATE_FIXTURE_INTERRUPT").as_deref() == Ok(point)
    {
        std::process::exit(EXIT_INTERRUPT);
    }
}

fn same_origin(request: &Request) -> bool {
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let Some(origin) = origin
        .to_str()
        .ok()
        .and_then(|origin| Url::parse(origin).ok())
    else {
        return false;
    };
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    origin.username().is_empty()
        && origin.password().is_none()
        && origin.path() == "/"
        && origin.query().is_none()
        && origin.fragment().is_none()
        && matches!(origin.scheme(), "http" | "https")
        && host
            .and_then(|host| Url::parse(&format!("{}://{host}", origin.scheme())).ok())
            .is_some_and(|expected| expected.origin() == origin.origin())
}

fn reply(status: StatusCode, success: bool, message: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"success":success,"message":message})),
    )
        .into_response()
}
