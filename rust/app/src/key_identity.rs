use std::io::Write;
use std::path::Path;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const DEFAULT_MACHINE_SALT: &str = "endpoint-proxy-salt";
const DEFAULT_KEY_SECRET: &str = "endpoint-proxy-api-key-secret";

pub fn generate(root: &Path) -> std::io::Result<(String, String)> {
    let raw = raw_machine_id(root)?;
    let hash = Sha256::digest(format!("{raw}{DEFAULT_MACHINE_SALT}"));
    let machine = format!("{hash:x}")[..16].to_owned();

    let mut key_id = String::with_capacity(6);
    while key_id.len() < 6 {
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
        for byte in bytes.iter().filter(|&&byte| byte < 252) {
            key_id.push(char::from(CHARSET[usize::from(*byte) % CHARSET.len()]));
            if key_id.len() == 6 {
                break;
            }
        }
    }

    let mut mac = Hmac::<Sha256>::new_from_slice(DEFAULT_KEY_SECRET.as_bytes())
        .map_err(std::io::Error::other)?;
    mac.update(machine.as_bytes());
    mac.update(key_id.as_bytes());
    let crc = format!("{:x}", mac.finalize().into_bytes());
    let key = format!("sk-{machine}-{key_id}-{}", &crc[..8]);
    Ok((machine, key))
}

pub(crate) fn raw_machine_id(root: &Path) -> std::io::Result<String> {
    let file = root.join("machine-id");
    if let Ok(raw) = std::fs::read_to_string(&file)
        && !raw.trim().is_empty()
    {
        return Ok(raw.trim().to_owned());
    }

    let generated = if cfg!(target_os = "linux") {
        [
            "/var/lib/dbus/machine-id",
            "/etc/machine-id",
            "/proc/sys/kernel/hostname",
        ]
        .into_iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .find_map(|raw| {
            raw.lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| {
                    let normalized = line.split_whitespace().collect::<String>().to_lowercase();
                    format!("{:x}", Sha256::digest(normalized))
                })
        })
    } else {
        None
    }
    .unwrap_or_else(|| Uuid::new_v4().to_string());

    let mut staged = tempfile::NamedTempFile::new_in(root)?;
    staged.write_all(generated.as_bytes())?;
    match staged.persist_noclobber(&file) {
        Ok(_) => Ok(generated),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read_to_string(&file)?;
            if !existing.trim().is_empty() {
                return Ok(existing.trim().to_owned());
            }
            error.file.persist(file).map_err(|error| error.error)?;
            Ok(generated)
        }
        Err(error) => Err(error.error),
    }
}

pub(crate) fn cli_token(root: &Path) -> Option<String> {
    let machine = std::fs::read_to_string(root.join("machine-id")).ok()?;
    let secret = std::fs::read_to_string(root.join("auth").join("cli-secret")).ok()?;
    let (machine, secret) = (machine.trim(), secret.trim());
    if machine.is_empty() || secret.is_empty() {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(machine.as_bytes());
    hash.update(b"9r-cli-auth");
    hash.update(secret.as_bytes());
    let expected = format!("{:x}", hash.finalize());
    Some(expected[..16].to_owned())
}
