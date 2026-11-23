use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;

const MAX_IDLE_READERS: usize = 8;

enum StorageRoot {
    Temporary(TempDir),
    Persistent(PathBuf),
}

#[derive(Clone)]
pub struct Storage(Arc<StorageState>);

struct StorageState {
    readers: Mutex<Vec<Connection>>,
    writer: Arc<AsyncMutex<Writer>>,
    catalog: Mutex<Option<Catalog>>,
    database: Arc<Database>,
}

struct Writer {
    connection: Option<Connection>,
    database: Arc<Database>,
}

struct Database {
    root: StorageRoot,
    file: PathBuf,
}

struct Catalog {
    stamp: (SystemTime, u64),
    value: Option<Arc<Value>>,
}

impl Storage {
    pub fn temporary() -> Result<Self, Box<dyn std::error::Error>> {
        let root = tempfile::Builder::new()
            .prefix("9router-migration-")
            .tempdir()?;
        Self::initialize(StorageRoot::Temporary(root))
    }

    pub fn open(root: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        std::fs::create_dir_all(&root)?;
        Self::initialize(StorageRoot::Persistent(root.canonicalize()?))
    }

    fn initialize(root: StorageRoot) -> Result<Self, Box<dyn std::error::Error>> {
        let file = root.path().join("db/data.sqlite");
        crate::storage_migration::initialize(root.path(), &file)?;
        let database = Arc::new(Database { root, file });
        Ok(Self(Arc::new(StorageState {
            readers: Mutex::new(Vec::with_capacity(MAX_IDLE_READERS)),
            writer: Arc::new(AsyncMutex::new(Writer {
                connection: None,
                database: database.clone(),
            })),
            catalog: Mutex::new(None),
            database,
        })))
    }

    pub fn model_catalog(&self) -> Option<Arc<Value>> {
        let path = self.root().join("model-catalog.json");
        let metadata = std::fs::metadata(&path).ok();
        let stamp = metadata.and_then(|metadata| Some((metadata.modified().ok()?, metadata.len())));
        let mut cached = self
            .0
            .catalog
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(stamp) = stamp else {
            *cached = None;
            return None;
        };
        if let Some(catalog) = cached.as_ref()
            && catalog.stamp == stamp
        {
            return catalog.value.clone();
        }
        let value = std::fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .map(Arc::new);
        *cached = Some(Catalog {
            stamp,
            value: value.clone(),
        });
        value
    }

    pub fn root(&self) -> &Path {
        self.0.database.root.path()
    }

    pub async fn api_key_is_active(&self, key: String) -> bool {
        self.run(false, move |connection| {
            connection
                .query_row(
                    "SELECT isActive FROM apiKeys WHERE key = ?1",
                    [key],
                    |row| Ok(is_active(row.get_ref(0)?)),
                )
                .optional()
                .map(|active| active.unwrap_or(false))
        })
        .await
        .unwrap_or(false)
    }

    pub async fn read_settings(&self) -> Option<Value> {
        self.run(false, |connection| {
            let raw: Option<String> = connection
                .query_row("SELECT data FROM settings WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .optional()?;
            Ok(raw
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_else(|| json!({})))
        })
        .await
        .ok()
    }

    pub async fn run<T: Send + 'static>(
        &self,
        write: bool,
        operation: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, String> {
        let task = if write {
            let writer = self.0.writer.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || {
                let mut writer = writer;
                let mut connection = match writer.connection.take() {
                    Some(connection) => connection,
                    None => writer.database.connect(OpenFlags::SQLITE_OPEN_READ_WRITE)?,
                };
                let result = operation(&mut connection);
                if result.is_ok() && connection.is_autocommit() {
                    writer.connection = Some(connection);
                }
                result
            })
        } else {
            let state = self.0.clone();
            tokio::task::spawn_blocking(move || {
                let cached = state
                    .readers
                    .lock()
                    .ok()
                    .and_then(|mut readers| readers.pop());
                let mut connection = match cached {
                    Some(connection) => connection,
                    None => state.database.connect(OpenFlags::SQLITE_OPEN_READ_ONLY)?,
                };
                let result = operation(&mut connection);
                if result.is_ok()
                    && connection.is_autocommit()
                    && let Ok(mut readers) = state.readers.lock()
                    && readers.len() < MAX_IDLE_READERS
                {
                    readers.push(connection);
                }
                result
            })
        };
        task.await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }
}

impl Database {
    fn connect(&self, flags: OpenFlags) -> rusqlite::Result<Connection> {
        let connection = Connection::open_with_flags(&self.file, flags)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(connection)
    }
}

impl StorageRoot {
    fn path(&self) -> &Path {
        match self {
            Self::Temporary(root) => root.path(),
            Self::Persistent(root) => root,
        }
    }
}

pub fn is_active(value: rusqlite::types::ValueRef<'_>) -> bool {
    matches!(value, rusqlite::types::ValueRef::Integer(1))
}
