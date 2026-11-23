use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use tempfile::TempDir;

enum StorageRoot {
    Temporary(TempDir),
    Persistent(PathBuf),
}

#[derive(Clone)]
pub struct Storage {
    root: Arc<StorageRoot>,
    file: PathBuf,
    catalog: Arc<Mutex<Option<Catalog>>>,
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
        let root = Arc::new(root);
        let file = root.path().join("db/data.sqlite");
        crate::storage_migration::initialize(root.path(), &file)?;
        Ok(Self {
            root,
            file,
            catalog: Arc::new(Mutex::new(None)),
        })
    }

    pub fn model_catalog(&self) -> Option<Arc<Value>> {
        let path = self.root().join("model-catalog.json");
        let metadata = std::fs::metadata(&path).ok();
        let stamp = metadata.and_then(|metadata| Some((metadata.modified().ok()?, metadata.len())));
        let mut cached = self
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
        self.root.path()
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
        let db = self.clone();
        tokio::task::spawn_blocking(move || {
            let flags = if write {
                OpenFlags::SQLITE_OPEN_READ_WRITE
            } else {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            };
            let mut connection = Connection::open_with_flags(&db.file, flags)?;
            connection.busy_timeout(Duration::from_secs(5))?;
            operation(&mut connection)
        })
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
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
