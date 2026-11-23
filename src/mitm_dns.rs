use std::fs::{self, Metadata};
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::SplitWhitespace;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value};
use tempfile::NamedTempFile;

const TOOLS: &[(&str, &[&str])] = &[
    (
        "antigravity",
        &[
            "daily-cloudcode-pa.googleapis.com",
            "cloudcode-pa.googleapis.com",
        ],
    ),
    ("copilot", &["api.individual.githubcopilot.com"]),
    (
        "kiro",
        &[
            "runtime.us-east-1.kiro.dev",
            "q.us-east-1.amazonaws.com",
            "codewhisperer.us-east-1.amazonaws.com",
        ],
    ),
    ("cursor", &["api2.cursor.sh"]),
];
const LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) struct Tool {
    name: &'static str,
    hosts: &'static [&'static str],
}

impl Tool {
    pub(super) fn parse(value: &str) -> Option<Self> {
        TOOLS
            .iter()
            .find_map(|&(name, hosts)| (name == value).then_some(Self { name, hosts }))
    }
}

#[derive(Debug)]
pub(super) enum ChangeError {
    Conflict,
    ManagedEntries,
    RecoveryChanged,
    File,
    Settings,
    Flush,
    Rollback,
}

impl From<rusqlite::Error> for ChangeError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Settings
    }
}

pub(super) fn status(dir: &Path, fixture: bool) -> Map<String, Value> {
    let content = read_hosts(&hosts_path(dir, fixture)).unwrap_or_default();
    status_for(&content)
}

pub(super) fn change(
    db: &mut Connection,
    dir: &Path,
    fixture: bool,
    tool: Tool,
    enable: bool,
) -> Result<Map<String, Value>, ChangeError> {
    let path = hosts_path(dir, fixture);
    update(
        db,
        &path,
        Change::Tool(tool, enable),
        |_| Ok(true),
        || flush(fixture),
    )
}

pub(super) fn stop(db: &mut Connection, dir: &Path, fixture: bool) -> Result<(), ChangeError> {
    let path = hosts_path(dir, fixture);
    update(
        db,
        &path,
        Change::Stop {
            forget: true,
            write_hosts: true,
        },
        |_| Ok(true),
        || flush(fixture),
    )
    .map(|_| ())
}

pub(super) fn suspend(db: &mut Connection, dir: &Path, fixture: bool) -> Result<(), ChangeError> {
    let path = hosts_path(dir, fixture);
    update(
        db,
        &path,
        Change::Stop {
            forget: false,
            write_hosts: true,
        },
        |_| Ok(true),
        || flush(fixture),
    )
    .map(|_| ())
}

pub(super) fn finish_listener_only(
    db: &mut Connection,
    dir: &Path,
    fixture: bool,
) -> Result<(), ChangeError> {
    update(
        db,
        &hosts_path(dir, fixture),
        Change::Stop {
            forget: true,
            write_hosts: false,
        },
        |_| Ok(true),
        || Ok(()),
    )
    .map(|_| ())
}

pub(super) fn check_listener_only(dir: &Path, fixture: bool) -> Result<(), ChangeError> {
    inspect_listener_only(&hosts_path(dir, fixture)).map(|_| ())
}

fn read_hosts(path: &Path) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        return Err(io::ErrorKind::InvalidData.into());
    }
    fs::read_to_string(path)
}

fn inspect_listener_only(path: &Path) -> Result<String, ChangeError> {
    let content = match read_hosts(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(_) => return Err(ChangeError::File),
    };
    if content.lines().any(|line| {
        TOOLS
            .iter()
            .any(|&(name, hosts)| owned_line(line, Tool { name, hosts }, false))
    }) {
        return Err(ChangeError::ManagedEntries);
    }
    Ok(content)
}

pub(super) fn restore(
    db: &mut Connection,
    dir: &Path,
    fixture: bool,
    authorized: impl FnOnce(&Connection) -> rusqlite::Result<bool>,
) -> Result<(), ChangeError> {
    let path = hosts_path(dir, fixture);
    update(db, &path, Change::Restore, authorized, || flush(fixture)).map(|_| ())
}

enum Change {
    Tool(Tool, bool),
    Stop { forget: bool, write_hosts: bool },
    Restore,
}

fn update(
    db: &mut Connection,
    path: &Path,
    change: Change,
    authorized: impl FnOnce(&Connection) -> rusqlite::Result<bool>,
    flush: impl Fn() -> io::Result<()>,
) -> Result<Map<String, Value>, ChangeError> {
    let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !authorized(&transaction)? {
        return Err(ChangeError::RecoveryChanged);
    }
    let state: Option<String> = transaction
        .query_row("SELECT data FROM settings WHERE id=1", [], |row| row.get(0))
        .optional()?;
    let mut state: Map<String, Value> = state
        .map_or_else(|| Ok(Map::new()), |text| serde_json::from_str(&text))
        .map_err(|_| ChangeError::Settings)?;
    let legacy_enabled = match change {
        Change::Tool(tool, enable) => {
            let mut desired = state
                .remove("dnsToolEnabled")
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            let mut managed = state
                .remove("_nativeMitmDns")
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            let legacy_enabled = desired.get(tool.name) == Some(&Value::Bool(true))
                && managed.get(tool.name) != Some(&Value::Bool(true));
            desired.insert(tool.name.to_owned(), Value::Bool(enable));
            managed.insert(tool.name.to_owned(), Value::Bool(true));
            state.insert("dnsToolEnabled".to_owned(), Value::Object(desired));
            state.insert("_nativeMitmDns".to_owned(), Value::Object(managed));
            legacy_enabled
        }
        Change::Stop { forget: true, .. } => {
            state.insert("mitmEnabled".to_owned(), Value::Bool(false));
            state.remove("_nativeMitmEnabled");
            transaction.execute(
                "DELETE FROM kv WHERE scope='mitmRuntime' AND key='binding'",
                [],
            )?;
            false
        }
        Change::Stop { forget: false, .. } | Change::Restore => false,
    };
    let desired = &state.get("dnsToolEnabled");
    let managed = &state.get("_nativeMitmDns");
    let restore_enabled = |name: &str| {
        matches!(change, Change::Restore)
            && desired.is_some_and(|values| values[name] == true)
            && managed.is_some_and(|values| values[name] == true)
    };
    transaction.execute(
        "INSERT INTO settings(id,data) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
        [serde_json::to_string(&state).map_err(|_| ChangeError::Settings)?],
    )?;
    if matches!(
        change,
        Change::Stop {
            write_hosts: false,
            ..
        }
    ) {
        let content = inspect_listener_only(path)?;
        transaction.commit()?;
        return Ok(status_for(&content));
    }

    let before = match Snapshot::read(path) {
        Ok(snapshot) => snapshot,
        Err(error)
            if !matches!(change, Change::Tool(_, true))
                && !TOOLS.iter().any(|&(name, _)| restore_enabled(name))
                && error.kind() == io::ErrorKind::NotFound =>
        {
            transaction.commit()?;
            return Ok(status_for(""));
        }
        Err(_) => return Err(ChangeError::File),
    };
    let after = match change {
        Change::Tool(tool, enable) => edited(&before.content, tool, enable, legacy_enabled)?,
        Change::Stop { .. } | Change::Restore => {
            TOOLS
                .iter()
                .try_fold(before.content.clone(), |content, &(name, hosts)| {
                    edited(&content, Tool { name, hosts }, restore_enabled(name), false)
                })?
        }
    };
    let changed = after != before.content;
    if changed {
        before.replace(path, &before.content, &after)?;
        if flush().is_err() {
            before
                .replace(path, &after, &before.content)
                .map_err(|_| ChangeError::Rollback)?;
            let _ = flush();
            return Err(ChangeError::Flush);
        }
    }
    if transaction.commit().is_err() {
        if changed {
            before
                .replace(path, &after, &before.content)
                .map_err(|_| ChangeError::Rollback)?;
            let _ = flush();
        }
        return Err(ChangeError::Settings);
    }
    Ok(status_for(&after))
}

fn edited(
    content: &str,
    tool: Tool,
    enable: bool,
    legacy_enabled: bool,
) -> Result<String, ChangeError> {
    if !enable {
        return Ok(content
            .split_inclusive('\n')
            .filter(|line| !owned_line(line, tool, legacy_enabled))
            .collect());
    }
    let marker = format!("9Router MITM {}", tool.name);

    let mut missing = Vec::new();
    for host in tool.hosts {
        let mut found = false;
        for (address, mut aliases) in content.lines().filter_map(entry) {
            if aliases.any(|alias| alias.eq_ignore_ascii_case(host)) {
                if !address.is_loopback() {
                    return Err(ChangeError::Conflict);
                }
                found = true;
            }
        }
        if !found {
            missing.push(*host);
        }
    }
    let mut next = if legacy_enabled {
        let mut marked = String::with_capacity(content.len());
        for line in content.split_inclusive('\n') {
            let bare = line.trim_end_matches(['\r', '\n']);
            if legacy_mapping(bare, tool) {
                marked.push_str(bare);
                marked.push_str("\t# ");
                marked.push_str(&marker);
                marked.push_str(&line[bare.len()..]);
            } else {
                marked.push_str(line);
            }
        }
        marked
    } else {
        content.to_owned()
    };
    let eol = if cfg!(windows) || content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    if !missing.is_empty() && !next.is_empty() && !next.ends_with('\n') {
        next.push_str(eol);
    }
    for host in missing {
        next.push_str(&format!("127.0.0.1 {host}\t# {marker}{eol}"));
    }
    Ok(next)
}

fn owned_line(line: &str, tool: Tool, legacy_enabled: bool) -> bool {
    let line = line.trim_end_matches(['\r', '\n']);
    let Some((address, mut aliases)) = entry(line) else {
        return false;
    };
    let Some(name) = aliases.next() else {
        return false;
    };
    if !address.is_loopback()
        || aliases.next().is_some()
        || !tool
            .hosts
            .iter()
            .any(|host| name.eq_ignore_ascii_case(host))
    {
        return false;
    }
    let marked = line
        .split_once('#')
        .and_then(|(_, comment)| comment.trim().strip_prefix("9Router MITM "))
        == Some(tool.name);
    marked || legacy_enabled && legacy_mapping(line, tool)
}

fn legacy_mapping(line: &str, tool: Tool) -> bool {
    line.strip_prefix("127.0.0.1 ")
        .is_some_and(|host| tool.hosts.contains(&host))
}

fn status_for(content: &str) -> Map<String, Value> {
    let entries: Vec<_> = content
        .lines()
        .filter_map(entry)
        .flat_map(|(address, aliases)| aliases.map(move |alias| (alias, address)))
        .collect();
    TOOLS
        .iter()
        .map(|&(tool, hosts)| {
            let enabled = hosts.iter().all(|host| {
                let mut mappings = entries
                    .iter()
                    .filter(|(alias, _)| alias.eq_ignore_ascii_case(host))
                    .map(|(_, address)| address);
                mappings.next().is_some_and(IpAddr::is_loopback)
                    && mappings.all(IpAddr::is_loopback)
            });
            (tool.to_owned(), Value::Bool(enabled))
        })
        .collect()
}

fn entry(line: &str) -> Option<(IpAddr, SplitWhitespace<'_>)> {
    let mut fields = line
        .split_once('#')
        .map_or(line, |(entry, _)| entry)
        .split_whitespace();
    let address = fields.next()?.trim_start_matches('\u{feff}').parse().ok()?;
    Some((address, fields))
}

fn hosts_path(dir: &Path, fixture: bool) -> PathBuf {
    if fixture {
        dir.join("hosts")
    } else if cfg!(windows) {
        windows_root().join("System32/drivers/etc/hosts")
    } else {
        PathBuf::from("/etc/hosts")
    }
}

fn windows_root() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
}

struct Snapshot {
    content: String,
    metadata: Metadata,
}

impl Snapshot {
    fn read(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.len() > LIMIT {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Self {
            content: fs::read_to_string(path)?,
            metadata,
        })
    }

    fn replace(&self, path: &Path, expected: &str, replacement: &str) -> Result<(), ChangeError> {
        let parent = path.parent().ok_or(ChangeError::File)?;
        let mut file = NamedTempFile::new_in(parent).map_err(|_| ChangeError::File)?;
        self.copy_metadata(&file).map_err(|_| ChangeError::File)?;
        file.write_all(replacement.as_bytes())
            .map_err(|_| ChangeError::File)?;
        file.as_file().sync_all().map_err(|_| ChangeError::File)?;
        let actual = Self::read(path).map_err(|_| ChangeError::File)?;
        if actual.content != expected {
            return Err(ChangeError::Conflict);
        }
        file.persist(path).map_err(|_| ChangeError::File)?;
        Ok(())
    }

    fn copy_metadata(&self, file: &NamedTempFile) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix::fs::{Gid, Uid, fchown};
            use std::os::unix::fs::MetadataExt;
            let staged = file.as_file().metadata()?;
            if staged.uid() != self.metadata.uid() || staged.gid() != self.metadata.gid() {
                fchown(
                    file.as_file(),
                    Some(Uid::from_raw(self.metadata.uid())),
                    Some(Gid::from_raw(self.metadata.gid())),
                )?;
            }
        }
        file.as_file().set_permissions(self.metadata.permissions())
    }
}

fn flush(fixture: bool) -> io::Result<()> {
    if fixture {
        return Ok(());
    }
    let command = if cfg!(windows) {
        (windows_root().join("System32/ipconfig.exe"), "/flushdns")
    } else if cfg!(target_os = "macos") {
        (PathBuf::from("/usr/bin/dscacheutil"), "-flushcache")
    } else {
        (PathBuf::from("/usr/bin/resolvectl"), "flush-caches")
    };
    if !command.0.is_file() {
        return Ok(());
    }
    let status = Command::new(command.0)
        .arg(command.1)
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("DNS cache flush failed"))
    }
}
