use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::SocketAddrV4;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::windows_acl::{self, Account, Installation, Rename};

pub(super) const STAGE: &str = ".9router-update";
pub(super) const PREPARED: &str = "prepared.json";
pub(super) const JOURNAL: &str = "journal.json";
pub(super) const COMMITTED: &str = "committed";
pub(super) const CANDIDATE: &str = "candidate.exe";
pub(super) const PREVIOUS: &str = "previous.exe";
pub(super) const HELPER: &str = "helper.exe";
const DATABASE: &str = "data.sqlite";
const WAL: &str = "data.sqlite-wal";
const SHM: &str = "data.sqlite-shm";

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Plan {
    pub(super) install: PathBuf,
    pub(super) data: PathBuf,
    pub(super) address: SocketAddrV4,
    pub(super) arguments: Vec<String>,
    pub(super) old_version: String,
    pub(super) next_version: String,
    pub(super) old: [u8; 32],
    pub(super) next: [u8; 32],
    pub(super) expires: i64,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Journal {
    pub(super) plan: Plan,
    database: [u8; 32],
    wal: Option<[u8; 32]>,
}

pub(super) fn read<T: serde::de::DeserializeOwned>(
    stage: &Path,
    leaf: &str,
    account: &Account,
) -> io::Result<T> {
    let raw = windows_acl::validated_file(&stage.join(leaf), account)?;
    let mut data = Vec::new();
    raw.take(64 * 1024 + 1).read_to_end(&mut data)?;
    if data.len() > 64 * 1024 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    serde_json::from_slice(&data).map_err(io::Error::other)
}

pub(super) fn publish<T: Serialize>(stage: &Path, name: &str, data: &T) -> io::Result<()> {
    let mut output = tempfile::NamedTempFile::new_in(stage)?;
    serde_json::to_writer(&mut output, data).map_err(io::Error::other)?;
    output.flush()?;
    output.as_file().sync_all()?;
    output
        .persist_noclobber(stage.join(name))
        .map_err(|error| error.error)?;
    Ok(())
}

pub(super) fn checksum(path: &Path, account: &Account) -> io::Result<[u8; 32]> {
    let mut file = windows_acl::validated_file(path, account)?;
    let mut hash = Sha256::new();
    io::copy(&mut file, &mut hash)?;
    Ok(hash.finalize().into())
}

fn durable_copy(source: &Path, output: &Path, account: &Account) -> io::Result<[u8; 32]> {
    let mut source = windows_acl::validated_file(source, account)?;
    let mut target = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)?;
    io::copy(&mut source, &mut target)?;
    target.sync_all()?;
    drop(target);
    checksum(output, account)
}

pub(super) fn snapshot(plan: Plan, stage: &Path, account: &Account) -> io::Result<Journal> {
    let database = plan.data.join("db");
    let _directory = windows_acl::validated_directory(&database, account)?;
    let old = durable_copy(&plan.install, &stage.join(PREVIOUS), account)?;
    if old != plan.old {
        return Err(io::Error::other(
            "Installed executable changed before snapshot",
        ));
    }
    let saved_database = durable_copy(&database.join(DATABASE), &stage.join(DATABASE), account)?;
    let wal = match durable_copy(&database.join(WAL), &stage.join(WAL), account) {
        Ok(value) => Some(value),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !database.join(WAL).exists() => {
            None
        }
        Err(error) => return Err(error),
    };
    let journal = Journal {
        plan,
        database: saved_database,
        wal,
    };
    publish(stage, JOURNAL, &journal)?;
    Ok(journal)
}

fn require_hash(path: &Path, expected: &[u8; 32], account: &Account) -> io::Result<()> {
    if checksum(path, account)? != *expected {
        return Err(io::Error::other(
            "Recovery snapshot does not match the committed journal",
        ));
    }
    Ok(())
}

pub(super) fn restore(journal: &Journal, stage: &Path, account: &Account) -> io::Result<()> {
    let plan = &journal.plan;
    require_hash(&stage.join(PREVIOUS), &plan.old, account)?;
    require_hash(&stage.join(DATABASE), &journal.database, account)?;
    if let Some(wal) = &journal.wal {
        require_hash(&stage.join(WAL), wal, account)?;
    }
    let image = Installation::open(&plan.install, account)?;
    let _stage = image.staging(stage, account)?;
    let installed = checksum(&plan.install, account)?;
    if installed != plan.old && installed != plan.next {
        return Err(io::Error::other(
            "Installation differs from both versions in the journal",
        ));
    }
    if installed != plan.old {
        restore_file(&stage.join(PREVIOUS), &plan.install, &image, account)?;
    }
    super::fixture_interrupt("rollback-after-pe");
    let directory = plan.data.join("db");
    let database = directory.join(DATABASE);
    let _data_root = windows_acl::validated_directory(&plan.data, account)?;
    if fs::symlink_metadata(&directory).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        windows_acl::create_private_directory(&directory, account)?;
    }
    let _database_root = windows_acl::validated_directory(&directory, account)?;
    if fs::symlink_metadata(&database).is_err_and(|error| error.kind() == io::ErrorKind::NotFound) {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&database)?;
    }
    let identity = Installation::open(&database, account)?;
    remove_sidecar(&directory.join(WAL), account)?;
    remove_sidecar(&directory.join(SHM), account)?;
    restore_file(&stage.join(DATABASE), &database, &identity, account)?;
    super::fixture_interrupt("rollback-after-database");
    if journal.wal.is_some() {
        let mut snapshot = windows_acl::validated_file(&stage.join(WAL), account)?;
        let mut output = tempfile::NamedTempFile::new_in(&directory)?;
        io::copy(&mut snapshot, &mut output)?;
        output.as_file().sync_all()?;
        output
            .persist_noclobber(directory.join(WAL))
            .map_err(|error| error.error)?;
    }
    Ok(())
}

fn restore_file(
    from: &Path,
    to: &Path,
    identity: &Installation,
    account: &Account,
) -> io::Result<()> {
    let directory = to.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let mut output = tempfile::NamedTempFile::new_in(directory)?;
    let mut input = windows_acl::validated_file(from, account)?;
    io::copy(&mut input, &mut output)?;
    output.as_file().sync_all()?;
    let path = output.into_temp_path();
    identity.recheck(to)?;
    identity.anchored_replace(
        &path,
        to.file_name().ok_or(io::ErrorKind::InvalidInput)?,
        Rename::NativeExtended,
        account,
    )
}

fn remove_sidecar(path: &Path, account: &Account) -> io::Result<()> {
    match windows_acl::validated_file(path, account) {
        Ok(file) => {
            drop(file);
            fs::remove_file(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(super) fn committed(stage: &Path, account: &Account) -> io::Result<bool> {
    match read::<bool>(stage, COMMITTED, account) {
        Ok(true) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
        Ok(false) => Err(io::ErrorKind::InvalidData.into()),
    }
}
