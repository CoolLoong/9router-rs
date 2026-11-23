use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

use crate::pxpipe_transform::{Compression, Transform};

pub const VERSION: &str = "0.16.0";
const EVENT_BYTES: u64 = 5 * 1024 * 1024;
const MARKER: &str = "native.json";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Absent,
    Native,
}

struct Inner {
    directory: PathBuf,
    installed: AtomicBool,
    loaded: AtomicI64,
    workers: Arc<Semaphore>,
    events: mpsc::Sender<Value>,
}

#[derive(Clone)]
pub struct Runtime(Arc<Inner>);

impl Runtime {
    pub fn new(root: &Path) -> Self {
        let directory = root.join("pxpipe");
        let installed = fs::read(directory.join(MARKER))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .is_some_and(|marker| marker["engine"] == "rust")
            || legacy_install_present(&directory);
        let (events, mut receiver) = mpsc::channel(256);
        let target = directory.clone();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let directory = target.clone();
                let _ = tokio::task::spawn_blocking(move || append_event(&directory, &event)).await;
            }
        });
        Self(Arc::new(Inner {
            directory,
            installed: AtomicBool::new(installed),
            loaded: AtomicI64::new(0),
            workers: Arc::new(Semaphore::new(2)),
            events,
        }))
    }

    pub fn directory(&self) -> &Path {
        &self.0.directory
    }

    pub fn installed(&self) -> bool {
        self.0.installed.load(Ordering::Acquire)
    }

    pub fn mode(&self) -> Mode {
        if self.installed() {
            Mode::Native
        } else {
            Mode::Absent
        }
    }

    pub fn install(&self) -> io::Result<()> {
        fs::create_dir_all(self.directory())?;
        let mut temp = tempfile::NamedTempFile::new_in(self.directory())?;
        temp.write_all(
            json!({"engine":"rust","version":VERSION})
                .to_string()
                .as_bytes(),
        )?;
        temp.as_file().sync_all()?;
        temp.persist(self.directory().join(MARKER))
            .map_err(|error| error.error)?;
        self.0.installed.store(true, Ordering::Release);
        Ok(())
    }

    pub fn start(&self) {
        let _ = self
            .0
            .loaded
            .compare_exchange(0, now(), Ordering::AcqRel, Ordering::Acquire);
    }

    pub fn stop(&self) -> bool {
        self.0.loaded.swap(0, Ordering::AcqRel) != 0
    }

    pub fn loaded_at(&self) -> Option<i64> {
        Some(self.0.loaded.load(Ordering::Acquire)).filter(|loaded| *loaded != 0)
    }

    pub async fn compress(
        &self,
        settings: &Value,
        body: &Value,
        format: &str,
        model: &str,
    ) -> Compression {
        let installed = self.installed();
        if installed {
            self.start();
        }
        let prepared = match prepare(installed, settings, body, format) {
            Ok(prepared) => prepared,
            Err(summary) => {
                return Compression {
                    body: None,
                    summary,
                };
            }
        };
        let started = Instant::now();
        let scope = std::env::var("PXPIPE_MODELS").ok();
        let overrides = std::env::var("PXPIPE_GPT_PROFILES").ok();
        if !crate::pxpipe_static::supports(body, model, scope.as_deref(), overrides.as_deref()) {
            return prepared.complete(
                Ok(Some(Transform::Skipped {
                    reason: Some("unsupported_shape".into()),
                    detail: None,
                })),
                started.elapsed().as_millis() as u64,
            );
        }
        let deadline = tokio::time::Instant::now() + prepared.timeout;
        let Ok(Ok(permit)) =
            tokio::time::timeout_at(deadline, self.0.workers.clone().acquire_owned()).await
        else {
            return prepared.complete(Ok(None), started.elapsed().as_millis() as u64);
        };
        let body = body.clone();
        let model = model.to_owned();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match crate::pxpipe_static::transform(
                &body,
                &model,
                &prepared,
                scope.as_deref(),
                overrides.as_deref(),
            ) {
                Some(result) => result.map(Some),
                None => Ok(Some(Transform::Skipped {
                    reason: Some("unsupported_shape".into()),
                    detail: None,
                })),
            }
        });
        let result = match tokio::time::timeout_at(deadline, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("PXPIPE transform failed".to_owned()),
            Err(_) => Ok(None),
        };
        prepared.complete(result, started.elapsed().as_millis() as u64)
    }

    pub fn record(&self, provider: &str, model: &str, summary: Value) {
        let mut event = json!({"ts":now(),"provider":provider,"model":model});
        if let Value::Object(fields) = summary {
            event.as_object_mut().expect("PXPIPE event").extend(fields);
        }
        let _ = self.0.events.try_send(event);
    }
}

fn legacy_install_present(directory: &Path) -> bool {
    let package = directory.join("node_modules/pxpipe-proxy");
    package.join("dist/core/library.js").is_file()
        && fs::read(package.join("package.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|data| data["name"] == "pxpipe-proxy")
}

fn prepare(
    installed: bool,
    settings: &Value,
    body: &Value,
    format: &str,
) -> Result<crate::pxpipe_transform::Prepared, Value> {
    let number = crate::thinking_intent::numeric;
    crate::pxpipe_transform::prepare(
        body,
        true,
        installed,
        Some(format),
        number(&settings["pxpipeMinChars"]).unwrap_or_default(),
        number(&settings["pxpipeTimeoutMs"]).unwrap_or_default(),
    )
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn append_event(directory: &Path, event: &Value) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    let path = directory.join("events.jsonl");
    if fs::metadata(&path).is_ok_and(|meta| meta.len() > EVENT_BYTES) {
        fs::rename(&path, directory.join("events.jsonl.1"))?;
    }
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    let mut line = event.to_string();
    line.push('\n');
    file.write_all(line.as_bytes())
}
