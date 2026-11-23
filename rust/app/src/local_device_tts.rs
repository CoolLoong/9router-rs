use std::io::{self, Read};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::provider_index::js_trim;

const MAX_AUDIO_BYTES: u64 = 128 * 1024 * 1024;
const MAX_VOICE_BYTES: u64 = 1024 * 1024;
const VOICE_TIMEOUT: Duration = Duration::from_secs(15);
const SYNTHESIS_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Failure {
    pub status: StatusCode,
    pub message: String,
}

impl Failure {
    fn gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
}

pub async fn synthesize(input: &str, model: &str) -> Result<Bytes, Failure> {
    let input = js_trim(input);
    if input.is_empty() {
        return Err(Failure {
            status: StatusCode::BAD_REQUEST,
            message: "Missing required field: input".to_owned(),
        });
    }
    let scratch = tempfile::Builder::new()
        .prefix("tts-")
        .tempdir()
        .map_err(|_| Failure::gateway("Could not create local speech temporary directory"))?;
    timeout(SYNTHESIS_TIMEOUT, render(input, model, scratch))
        .await
        .map_err(|_| Failure::gateway("Local device speech synthesis timed out"))?
}

async fn render(input: &str, model: &str, scratch: TempDir) -> Result<Bytes, Failure> {
    let aiff = scratch.path().join("out.aiff");
    let mp3 = scratch.path().join("out.mp3");
    let mut say = command("say");
    if !model.is_empty() {
        say.args(["-v", model]);
    }
    say.arg("-o").arg(&aiff);
    if input.starts_with('-') {
        say.arg("--");
    }
    say.arg(input);
    run(say, "say").await?;

    let mut ffmpeg = command("ffmpeg");
    ffmpeg
        .args(["-y", "-i"])
        .arg(&aiff)
        .args(["-codec:a", "libmp3lame", "-qscale:a", "4"])
        .arg(&mp3);
    run(ffmpeg, "ffmpeg").await?;
    tokio::task::spawn_blocking(move || {
        let output = audio(&mp3);
        drop(scratch);
        output
    })
    .await
    .map_err(|_| Failure::gateway("Could not read local speech output"))?
}

fn command(executable: &str) -> Command {
    let mut command = Command::new(executable);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    command
}

async fn run(mut command: Command, executable: &str) -> Result<(), Failure> {
    let status = command.status().await.map_err(|error| {
        let message = match error.kind() {
            io::ErrorKind::NotFound => format!("spawn {executable} ENOENT"),
            io::ErrorKind::PermissionDenied => format!("spawn {executable} EACCES"),
            _ => format!("Could not start {executable}"),
        };
        Failure::gateway(message)
    })?;
    if !status.success() {
        let message = status.code().map_or_else(
            || format!("{executable} was interrupted"),
            |code| format!("{executable} exited with status {code}"),
        );
        return Err(Failure::gateway(message));
    }
    Ok(())
}

fn audio(path: &Path) -> Result<Bytes, Failure> {
    let file = std::fs::File::open(path)
        .map_err(|_| Failure::gateway("Could not read local speech output"))?;
    let mut bytes = Vec::new();
    file.take(MAX_AUDIO_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::gateway("Could not read local speech output"))?;
    if bytes.len() as u64 > MAX_AUDIO_BYTES {
        return Err(Failure::gateway("Local device audio output is too large"));
    }
    Ok(Bytes::from(bytes))
}

pub async fn voices() -> Option<Vec<Value>> {
    let (executable, arguments) = voice_command();
    let output = timeout(VOICE_TIMEOUT, capture(executable, arguments))
        .await
        .ok()??;
    parse_voices(&String::from_utf8_lossy(&output))
}

async fn capture(executable: &str, arguments: &[&str]) -> Option<Vec<u8>> {
    let mut child = command(executable)
        .args(arguments)
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut output = Vec::new();
    child
        .stdout
        .take()?
        .take(MAX_VOICE_BYTES + 1)
        .read_to_end(&mut output)
        .await
        .ok()?;
    if output.len() as u64 > MAX_VOICE_BYTES || !child.wait().await.ok()?.success() {
        return None;
    }
    Some(output)
}

#[cfg(not(windows))]
fn voice_command() -> (&'static str, &'static [&'static str]) {
    ("say", &["-v", "?"])
}

#[cfg(not(windows))]
fn parse_voices(stdout: &str) -> Option<Vec<Value>> {
    Some(stdout.lines().filter_map(parse_say_voice).collect())
}

#[cfg(not(windows))]
fn parse_say_voice(line: &str) -> Option<Value> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let mut start = None;
    for (index, ch) in line.char_indices() {
        if ch.is_whitespace() {
            start.get_or_insert(index);
        } else if let Some(separator) = start
            .take()
            .filter(|start| line[*start..index].chars().count() >= 2)
        {
            let Some(locale) = line[index..].get(..5) else {
                continue;
            };
            let [a, b, b'_', c, d] = locale.as_bytes() else {
                continue;
            };
            if a.is_ascii_lowercase()
                && b.is_ascii_lowercase()
                && c.is_ascii_uppercase()
                && d.is_ascii_uppercase()
            {
                let name = js_trim(&line[..separator]);
                let (lang, country) = locale.split_once('_')?;
                return Some(
                    json!({"id":name,"name":name,"locale":locale,"lang":lang,"country":country,"gender":""}),
                );
            }
        }
    }
    None
}

#[cfg(windows)]
fn voice_command() -> (&'static str, &'static [&'static str]) {
    (
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            concat!(
                "Add-Type -AssemblyName System.Speech; ",
                "$s = New-Object System.Speech.Synthesis.SpeechSynthesizer; ",
                "$s.GetInstalledVoices() | ForEach-Object { $v = $_.VoiceInfo; ",
                "[PSCustomObject]@{ Name=$v.Name; Culture=$v.Culture.Name; Gender=$v.Gender } } ",
                "| ConvertTo-Json -Compress",
            ),
        ],
    )
}

#[cfg(windows)]
fn parse_voices(stdout: &str) -> Option<Vec<Value>> {
    let text = js_trim(stdout);
    if text.is_empty() {
        return Some(Vec::new());
    }
    let value: Value = serde_json::from_str(text).ok()?;
    let voices = value
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(&value));
    Some(voices.iter().filter_map(|voice| {
        let name = voice["Name"].as_str()?;
        let culture = voice["Culture"].as_str().filter(|value| !value.is_empty()).unwrap_or("en-US");
        let mut codes = culture.split('-');
        let lang = codes.next().unwrap_or_default();
        let country = codes.next().unwrap_or_default();
        let gender = match &voice["Gender"] {
            value if value == 1 || value == "1" || value == "Male" => "Male",
            value if value == 2 || value == "2" || value == "Female" => "Female",
            _ => "",
        };
        Some(json!({"id":name,"name":name,"locale":culture.replacen('-', "_", 1), "lang":lang,"country":country,"gender":gender}))
    }).collect())
}
