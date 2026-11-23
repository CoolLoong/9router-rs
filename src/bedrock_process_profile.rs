use std::process::Stdio;

use chrono::Utc;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use super::Owned;

const OUTPUT_LIMIT: u64 = 1024 * 1024;
const PROCESS_ERROR: &str = "AWS credential_process failed";
const INVALID_OUTPUT: &str = "AWS credential_process returned invalid credentials";

struct CredentialChild(Child);

impl Drop for CredentialChild {
    fn drop(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(group) = self
            .0
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

#[cfg(not(windows))]
fn shell(line: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", line]);
    command
}

#[cfg(windows)]
fn shell(line: &str) -> Command {
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;

    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let mut command = std::process::Command::new(PathBuf::from(root).join("System32/cmd.exe"));
    command
        .args(["/d", "/s", "/c"])
        .raw_arg(format!("\"{line}\""));
    Command::from(command)
}

pub(super) async fn credentials(line: &str) -> Result<Owned, &'static str> {
    let mut command = shell(line);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    command.process_group(0);
    let mut child = CredentialChild(command.spawn().map_err(|_| PROCESS_ERROR)?);
    let mut output = child
        .0
        .stdout
        .take()
        .ok_or(PROCESS_ERROR)?
        .take(OUTPUT_LIMIT + 1);
    let mut buffer = Vec::new();
    output
        .read_to_end(&mut buffer)
        .await
        .map_err(|_| PROCESS_ERROR)?;
    if buffer.len() as u64 > OUTPUT_LIMIT {
        return Err(PROCESS_ERROR);
    }
    if !child.0.wait().await.map_err(|_| PROCESS_ERROR)?.success() {
        return Err(PROCESS_ERROR);
    }

    let data: Value = serde_json::from_slice(&buffer).map_err(|_| INVALID_OUTPUT)?;
    if data["Version"].as_f64() != Some(1.0) {
        return Err("AWS credential_process must return Version 1");
    }
    let read = |key: &str| data[key].as_str().filter(|value| !value.is_empty());
    let expires = read("Expiration")
        .map(|value| value.parse().map_err(|_| INVALID_OUTPUT))
        .transpose()?;
    if expires.is_some_and(|expires| expires <= Utc::now()) {
        return Err("AWS credential_process returned expired credentials");
    }
    let credentials = Owned {
        access: read("AccessKeyId").ok_or(INVALID_OUTPUT)?.to_owned(),
        secret: read("SecretAccessKey").ok_or(INVALID_OUTPUT)?.to_owned(),
        session: read("SessionToken").map(str::to_owned),
        expires,
    };
    credentials
        .credentials()
        .validate()
        .map_err(|_| INVALID_OUTPUT)?;
    Ok(credentials)
}
