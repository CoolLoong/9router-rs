use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{OnceLock, Weak};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use url::Url;

use super::{
    cert_trusted, certificate, dns, is_admin, is_fixture, normalize_router_url, running_pid,
};
use crate::storage::{Storage, is_active};

pub(crate) struct ListenerConfig {
    pub(crate) storage: Storage,
    pub(crate) data_dir: PathBuf,
    pub(crate) certificate: PathBuf,
    pub(crate) router_base: Url,
    pub(crate) api_key: String,
}

pub(crate) type ListenerTask = Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'static>>;
type Configure = fn(TcpListener, ListenerConfig, oneshot::Receiver<()>) -> io::Result<ListenerTask>;

static LISTENER: OnceLock<Configure> = OnceLock::new();
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const RESTART_DELAYS: &[u64] = &[5, 10, 20, 40];

pub(crate) fn register_listener(configure: Configure) -> bool {
    LISTENER.set(configure).is_ok()
}

pub(super) fn available() -> bool {
    LISTENER.get().is_some()
}

#[derive(Default)]
pub(crate) struct Runtime {
    active: Option<Active>,
    closing: bool,
}

struct Active {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<io::Result<()>>,
    _lease: File,
}

impl Drop for Active {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if !self.task.is_finished() {
            self.task.abort();
        }
    }
}

struct Prepared {
    shutdown: oneshot::Sender<()>,
    task: ListenerTask,
    trusted: bool,
}

impl Prepared {
    fn activate(self, lease: File) -> Active {
        Active {
            shutdown: Some(self.shutdown),
            task: tokio::spawn(self.task),
            _lease: lease,
        }
    }
}

#[derive(PartialEq, Eq)]
struct Binding {
    key_id: String,
    fingerprint: String,
    router_base: Url,
}

enum Resume {
    Inactive,
    Invalid,
    Ready { binding: Binding, key: String },
}

#[derive(Debug)]
pub(super) enum Failure {
    Unavailable,
    ShuttingDown,
    AlreadyRunning,
    ForeignController,
    PortBusy,
    PortPermission,
    Bind,
    Certificate,
    Configure,
    State,
    Dns(dns::ChangeError),
}

impl Runtime {
    pub(super) fn pid(&self) -> Option<u32> {
        self.active
            .as_ref()
            .filter(|active| !active.task.is_finished())
            .map(|_| std::process::id())
    }

    pub(super) async fn start(
        &mut self,
        storage: &Storage,
        router_base: Url,
        url_update: Option<String>,
        api_key: String,
    ) -> Result<u32, Failure> {
        if self.closing {
            return Err(Failure::ShuttingDown);
        }
        if self.pid().is_some() {
            return Err(Failure::AlreadyRunning);
        }
        self.active = None;
        let data_dir = storage.root().join("mitm");
        let lease = claim(&data_dir).await?;
        let administrator = tokio::task::spawn_blocking({
            let directory = data_dir.clone();
            move || {
                let administrator = is_admin();
                if !administrator {
                    dns::check_listener_only(&directory, is_fixture()).map_err(Failure::Dns)?;
                }
                Ok(administrator)
            }
        })
        .await
        .map_err(|_| Failure::State)??;
        let prepared = prepare(
            storage,
            data_dir,
            router_base.clone(),
            api_key.clone(),
            false,
        )
        .await?;
        write_start_state(
            storage,
            url_update,
            prepared.trusted,
            router_base,
            api_key,
            administrator,
        )
        .await?;
        self.active = Some(prepared.activate(lease));
        Ok(std::process::id())
    }

    async fn recover(&mut self, storage: &Storage) -> Result<(), Failure> {
        if self.closing || self.pid().is_some() {
            return Ok(());
        }
        self.active = None;
        if storage.read_settings().await.ok_or(Failure::State)?["_nativeMitmEnabled"] != true {
            return Ok(());
        }
        if !tokio::task::spawn_blocking(is_admin)
            .await
            .map_err(|_| Failure::State)?
        {
            return Ok(());
        }
        let data_dir = storage.root().join("mitm");
        let lease = claim(&data_dir).await?;
        let resume = storage
            .run(false, |db| read_resume(db))
            .await
            .map_err(|_| Failure::State)?;
        let (binding, key) = match resume {
            Resume::Inactive => return Ok(()),
            Resume::Invalid => return tidy_dns(storage, data_dir, true, true).await,
            Resume::Ready { binding, key } => (binding, key),
        };
        let prepared = match prepare(
            storage,
            data_dir.clone(),
            binding.router_base.clone(),
            key,
            true,
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                tidy_dns(storage, data_dir, false, true).await?;
                return Err(error);
            }
        };
        let restore_dir = data_dir.clone();
        let restored = storage.run(true, move |db| {
            Ok(dns::restore(db, &restore_dir, is_fixture(), |db| {
                Ok(matches!(read_resume(db)?, Resume::Ready { binding: current, .. } if current == binding))
            }))
        }).await.map_err(|_| Failure::State).and_then(|result| result.map_err(Failure::Dns));
        if let Err(error) = restored {
            let cleanup = tidy_dns(storage, data_dir, false, true).await;
            cleanup?;
            return Err(error);
        }
        self.active = Some(prepared.activate(lease));
        Ok(())
    }

    pub(super) async fn stop(&mut self, storage: &Storage) -> Result<(), Failure> {
        let data_dir = storage.root().join("mitm");
        let administrator = tokio::task::spawn_blocking(is_admin)
            .await
            .map_err(|_| Failure::State)?;
        let mut recovered = None;
        if self.active.is_none() {
            let recover =
                storage.read_settings().await.ok_or(Failure::State)?["_nativeMitmEnabled"] == true;
            if !recover {
                return tokio::task::spawn_blocking(move || {
                    if running_pid(&data_dir).is_some() {
                        return Err(Failure::ForeignController);
                    }
                    if !administrator {
                        dns::check_listener_only(&data_dir, is_fixture()).map_err(Failure::Dns)?;
                    }
                    Ok(())
                })
                .await
                .map_err(|_| Failure::State)?;
            }
            recovered = Some(claim(&data_dir).await?);
        }
        tidy_dns(storage, data_dir, true, administrator).await?;
        self.close_listener().await;
        drop(recovered);
        Ok(())
    }

    async fn close_listener(&mut self) {
        if let Some(mut active) = self.active.take() {
            if let Some(shutdown) = active.shutdown.take() {
                let _ = shutdown.send(());
            }
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut active.task)
                .await
                .is_err()
            {
                active.task.abort();
                let _ = (&mut active.task).await;
            }
        }
    }

    pub(super) async fn shutdown(&mut self, storage: &Storage) -> Result<(), Failure> {
        self.closing = true;
        if self.active.is_none() {
            return Ok(());
        }
        let administrator = tokio::task::spawn_blocking(is_admin).await.unwrap_or(false);
        let forget = !administrator
            || storage
                .run(false, |db| read_resume(db))
                .await
                .is_ok_and(|resume| !matches!(resume, Resume::Ready { .. }));
        let result = tidy_dns(storage, storage.root().join("mitm"), forget, administrator).await;
        self.close_listener().await;
        result
    }
}

pub(super) fn watch_recovery(storage: Storage, runtime: Weak<Mutex<Runtime>>) {
    if !available() {
        return;
    }
    tokio::spawn(async move {
        let mut initial = true;
        let mut retries = 0;
        loop {
            {
                let Some(slot) = runtime.upgrade() else {
                    return;
                };
                let mut slot = slot.lock().await;
                if slot.closing {
                    return;
                }
                if slot.pid().is_some() {
                    retries = 0;
                } else if initial
                    || retries <= RESTART_DELAYS.len() && (slot.active.is_some() || retries > 0)
                {
                    match slot.recover(&storage).await {
                        Ok(()) | Err(Failure::ForeignController) => retries = 0,
                        Err(error) => {
                            eprintln!("MITM recovery failed: {error:?}");
                            retries += 1;
                        }
                    }
                }
            }
            initial = false;
            let seconds = retries
                .checked_sub(1)
                .and_then(|index| RESTART_DELAYS.get(index))
                .copied()
                .unwrap_or(RESTART_DELAYS[0]);
            tokio::time::sleep(Duration::from_secs(seconds)).await;
        }
    });
}

async fn tidy_dns(
    storage: &Storage,
    data_dir: PathBuf,
    forget: bool,
    administrator: bool,
) -> Result<(), Failure> {
    storage
        .run(true, move |db| {
            Ok(if !administrator {
                dns::finish_listener_only(db, &data_dir, is_fixture())
            } else if forget {
                dns::stop(db, &data_dir, is_fixture())
            } else {
                dns::suspend(db, &data_dir, is_fixture())
            })
        })
        .await
        .map_err(|_| Failure::State)?
        .map_err(Failure::Dns)
}

async fn prepare(
    storage: &Storage,
    data_dir: PathBuf,
    router_base: Url,
    api_key: String,
    recover_existing: bool,
) -> Result<Prepared, Failure> {
    let configure = LISTENER.get().ok_or(Failure::Unavailable)?;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, if is_fixture() { 0 } else { 443 }));
    let listener = TcpListener::bind(address)
        .await
        .map_err(|error| match error.kind() {
            io::ErrorKind::AddrInUse => Failure::PortBusy,
            io::ErrorKind::PermissionDenied => Failure::PortPermission,
            _ => Failure::Bind,
        })?;
    let storage = storage.clone();
    let (shutdown, stopped) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let root = if recover_existing {
            certificate::owned_root(&data_dir, SystemTime::now())
                .map_err(|_| Failure::Certificate)?;
            data_dir.join(certificate::CA_CERT)
        } else {
            certificate::ensure(&data_dir).map_err(|_| Failure::Certificate)?
        };
        let trusted = cert_trusted(&data_dir, &root);
        let config = ListenerConfig {
            storage,
            data_dir,
            certificate: root,
            router_base,
            api_key,
        };
        let task = configure(listener, config, stopped).map_err(|_| Failure::Configure)?;
        Ok(Prepared {
            shutdown,
            task,
            trusted,
        })
    })
    .await
    .map_err(|_| Failure::Configure)?
}

async fn claim(dir: &Path) -> Result<File, Failure> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if running_pid(&dir).is_some() {
            return Err(Failure::ForeignController);
        }
        if !dir.parent().is_some_and(safe_directory) {
            return Err(Failure::State);
        }
        fs::create_dir_all(&dir).map_err(|_| Failure::State)?;
        if !safe_directory(&dir) {
            return Err(Failure::State);
        }
        let path = dir.join("controller.lock");
        if fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
            return Err(Failure::State);
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let lease = options.open(path).map_err(|_| Failure::State)?;
        match lease.try_lock() {
            Ok(()) => Ok(lease),
            Err(std::fs::TryLockError::WouldBlock) => Err(Failure::ForeignController),
            Err(std::fs::TryLockError::Error(_)) => Err(Failure::State),
        }
    })
    .await
    .map_err(|_| Failure::State)?
}

fn safe_directory(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.uid() == rustix::process::geteuid().as_raw() && metadata.mode() & 0o022 == 0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    true
}

async fn write_start_state(
    storage: &Storage,
    url: Option<String>,
    trusted: bool,
    router_base: Url,
    api_key: String,
    administrator: bool,
) -> Result<(), Failure> {
    storage.run(true, move |db| {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw: Option<String> = tx.query_row("SELECT data FROM settings WHERE id=1", [], |row| row.get(0)).optional()?;
        let mut settings: Map<String, Value> = raw
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?
            .unwrap_or_default();
        settings.insert("mitmEnabled".to_owned(), json!(administrator));
        if administrator {
            settings.insert("_nativeMitmEnabled".to_owned(), json!(true));
        } else {
            settings.remove("_nativeMitmEnabled");
        }
        settings.insert("mitmCertInstalled".to_owned(), json!(trusted));
        if let Some(url) = url {
            settings.insert("mitmRouterBaseUrl".to_owned(), json!(url));
        }
        let key_id = if administrator { tx.query_row(
            "SELECT id,isActive FROM apiKeys WHERE key=?1",
            [&api_key],
            |row| Ok((row.get::<_, String>(0)?, is_active(row.get_ref(1)?))),
        ).optional()?.and_then(|(id, active)| active.then_some(id)) } else { None };
        if let Some(id) = key_id {
            let binding = json!({"keyId":id,"fingerprint":fingerprint(&api_key),"routerBase":router_base.as_str()});
            tx.execute(
                "INSERT INTO kv(scope,key,value) VALUES('mitmRuntime','binding',?1) ON CONFLICT(scope,key) DO UPDATE SET value=excluded.value",
                [binding.to_string()],
            )?;
        } else {
            tx.execute("DELETE FROM kv WHERE scope='mitmRuntime' AND key='binding'", [])?;
        }
        tx.execute("INSERT INTO settings(id,data) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data", [Value::Object(settings).to_string()])?;
        tx.commit()
    }).await.map_err(|_| Failure::State)
}

fn read_resume(db: &Connection) -> rusqlite::Result<Resume> {
    let settings: Option<String> = db
        .query_row("SELECT data FROM settings WHERE id=1", [], |row| row.get(0))
        .optional()?;
    let settings: Value = settings
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if settings["_nativeMitmEnabled"] != true {
        return Ok(Resume::Inactive);
    }
    if settings["mitmEnabled"] != true {
        return Ok(Resume::Invalid);
    }
    let saved: Option<String> = db
        .query_row(
            "SELECT value FROM kv WHERE scope='mitmRuntime' AND key='binding'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let saved: Value = saved
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let Ok((router_base, _)) = normalize_router_url(&settings["mitmRouterBaseUrl"]) else {
        return Ok(Resume::Invalid);
    };
    let (Some(id), Some(hash)) = (saved["keyId"].as_str(), saved["fingerprint"].as_str()) else {
        return Ok(Resume::Invalid);
    };
    if saved["routerBase"] != router_base.as_str() {
        return Ok(Resume::Invalid);
    }
    let key = db
        .query_row(
            "SELECT key,isActive FROM apiKeys WHERE id=?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, is_active(row.get_ref(1)?))),
        )
        .optional()?;
    let Some((key, true)) = key.filter(|(key, _)| !key.is_empty() && fingerprint(key) == hash)
    else {
        return Ok(Resume::Invalid);
    };
    Ok(Resume::Ready {
        binding: Binding {
            key_id: id.to_owned(),
            fingerprint: hash.to_owned(),
            router_base,
        },
        key,
    })
}

fn fingerprint(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}
