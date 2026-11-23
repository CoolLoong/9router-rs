use std::io;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use reqwest::{Client, ClientBuilder, Url};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use uuid::Uuid;

use crate::chat_output::{json_response, reply};
use crate::provider_connections::truthy;
use crate::proxy_transport::{self, Route, Selection};
use crate::storage::Storage;

use super::{Adapter, Field, Fields, TranscriptionError, fixture, proxy_required};

pub(super) const ENDPOINT: &str = "https://chatgpt.com/backend-api/transcribe";
const FIXTURE: &str = "http://127.0.0.1:52987/codex/backend-api/transcribe";
const AUDIO_LIMIT: usize = 25 * 1024 * 1024;
const RESPONSE_LIMIT: usize = 1024 * 1024;
const AUDIO_SECONDS: usize = 120;
const SAMPLE_RATE: u32 = 24000;
const PCM_LIMIT: usize = AUDIO_SECONDS * SAMPLE_RATE as usize * 2;
const TRANSCRIPTION_TIMEOUT: Duration = Duration::from_secs(60);
const AUDIO_TIMEOUT: Duration = Duration::from_secs(15);

static ACTIVE: Semaphore = Semaphore::const_new(2);
static CLIENT: OnceLock<Client> = OnceLock::new();
static USER_AGENT: OnceLock<String> = OnceLock::new();

fn transport() -> &'static Client {
    CLIENT.get_or_init(|| {
        client_builder()
            .build()
            .expect("Codex transcription HTTP client")
    })
}

fn client_builder() -> ClientBuilder {
    Client::builder()
        .connect_timeout(TRANSCRIPTION_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Egress {
    Direct,
    Bypass,
    Tunnel(Url),
}

impl Egress {
    pub fn protected(&self) -> bool {
        !matches!(self, Self::Direct)
    }
}

pub(super) async fn egress(
    storage: &Storage,
    settings: &Value,
    account: &Value,
) -> Result<Egress, TranscriptionError> {
    let unavailable = || {
        failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Codex transcription proxy is unavailable",
        )
    };
    if truthy(&account["providerSpecificData"]["vercelRelayUrl"]) {
        return Err(unavailable());
    }
    match proxy_transport::select_with_bypass(storage, settings, account, ENDPOINT)
        .await
        .map_err(|_| unavailable())?
    {
        Selection::ExplicitBypass => Ok(Egress::Bypass),
        Selection::Routed(Route::Direct) if !proxy_required(settings, Adapter::CODEX, account) => {
            Ok(Egress::Direct)
        }
        Selection::Routed(Route::Proxy { url, .. }) => proxy_transport::tunnel_proxy_url(&url)
            .map(Egress::Tunnel)
            .ok_or_else(unavailable),
        _ => Err(unavailable()),
    }
}

fn user_agent() -> &'static str {
    USER_AGENT.get_or_init(|| {
        let pinned: Value = serde_json::from_str(include_str!("codex-config.json"))
            .expect("embedded Codex transport");
        let version = pinned["version"]
            .as_str()
            .expect("pinned Codex CLI version");
        format!("codex_cli_rs/{version}")
    })
}

fn failure(status: StatusCode, message: &str) -> TranscriptionError {
    TranscriptionError {
        status,
        message: message.to_owned(),
    }
}

fn bad_input(message: &str) -> TranscriptionError {
    failure(StatusCode::BAD_REQUEST, message)
}

fn upstream() -> TranscriptionError {
    failure(StatusCode::BAD_GATEWAY, "Codex transcription failed")
}

fn account_headers(account: &Value) -> Result<(&str, &str), TranscriptionError> {
    let details = &account["providerSpecificData"];
    let token = account["accessToken"]
        .as_str()
        .filter(|token| visible_ascii(token));
    let workspace = ["workspaceId", "chatgptAccountId", "accountId"]
        .into_iter()
        .find_map(|key| details.get(key).filter(|value| truthy(value)))
        .and_then(Value::as_str)
        .filter(|workspace| workspace.len() <= 256 && visible_ascii(workspace));
    match (account["authType"].as_str(), token, workspace) {
        (Some("oauth"), Some(token), Some(workspace)) => Ok((token, workspace)),
        _ => Err(failure(
            StatusCode::UNAUTHORIZED,
            "A valid Codex OAuth account is required",
        )),
    }
}

fn visible_ascii(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

pub(super) async fn transcribe(
    account: &Value,
    form: &Fields,
    unsupported_or_duplicate: bool,
    egress: Egress,
) -> Result<Response, TranscriptionError> {
    let (token, workspace) = account_headers(account)?;
    if unsupported_or_duplicate {
        return Err(bad_input("Unsupported or duplicate form field"));
    }
    let text = match form.get("response_format") {
        None => false,
        Some(Field::Text(value)) if value.is_empty() || value == "json" => false,
        Some(Field::Text(value)) if value == "text" => true,
        _ => {
            return Err(bad_input(
                "Only json and text response formats are supported",
            ));
        }
    };
    for (key, message) in [
        (
            "language",
            "language is not supported by Codex transcription",
        ),
        ("prompt", "prompt is not supported by Codex transcription"),
        (
            "temperature",
            "temperature is not supported by Codex transcription",
        ),
    ] {
        if form
            .get(key)
            .is_some_and(|value| value.as_text() != Some(""))
        {
            return Err(bad_input(message));
        }
    }
    let Some(Field::Audio(file)) = form
        .get("file")
        .filter(|entry| matches!(entry, Field::Audio(audio) if !audio.bytes.is_empty()))
    else {
        return Err(bad_input("A non-empty audio file is required"));
    };
    if file.bytes.len() > AUDIO_LIMIT {
        return Err(failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Audio exceeds 25 MiB",
        ));
    }

    let _permit = ACTIVE.try_acquire().map_err(|_| {
        failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Transcription capacity reached",
        )
    })?;
    let unavailable = || {
        failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Codex transcription proxy is unavailable",
        )
    };
    let direct = if fixture() { FIXTURE } else { ENDPOINT };
    let (routed, endpoint) = match egress {
        Egress::Tunnel(proxy) => {
            let builder = client_builder().retry(reqwest::retry::never());
            let client =
                proxy_transport::vendor_metadata_client(proxy, builder).ok_or_else(unavailable)?;
            (Some(client), ENDPOINT)
        }
        Egress::Bypass => {
            let client = client_builder()
                .retry(reqwest::retry::never())
                .build()
                .map_err(|_| unavailable())?;
            (Some(client), direct)
        }
        Egress::Direct => (None, direct),
    };
    let client = match routed.as_ref() {
        Some(client) => client,
        None => transport(),
    };
    let result = timeout(TRANSCRIPTION_TIMEOUT, async {
        let pcm = convert(&file.bytes).await?;
        send(client, endpoint, token, workspace, pcm).await
    })
    .await
    .map_err(|_| failure(StatusCode::GATEWAY_TIMEOUT, "Codex transcription failed"))??;

    let mut response = if text {
        let mut response = reply(StatusCode::OK, Body::from(result), false, None);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        response
    } else {
        json_response(StatusCode::OK, json!({"text":result}), None)
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn convert(input: &[u8]) -> Result<Vec<u8>, TranscriptionError> {
    let mut directory = tempfile::Builder::new();
    directory.prefix("9router-codex-stt-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let scratch = directory.tempdir().map_err(|_| upstream())?;
    let path = scratch.path().join("input");
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path).await.map_err(|_| upstream())?;
    file.write_all(input).await.map_err(|_| upstream())?;
    file.flush().await.map_err(|_| upstream())?;
    drop(file);

    let mut ffmpeg = Command::new("ffmpeg");
    ffmpeg
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-threads",
            "1",
            "-protocol_whitelist",
            "file,pipe",
            "-format_whitelist",
            "wav,mp3,mov,matroska,webm,ogg,flac,aac",
            "-i",
        ])
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-t",
            "120.01",
            "-threads",
            "1",
            "-ac",
            "1",
            "-ar",
            "24000",
            "-c:a",
            "pcm_s16le",
            "-f",
            "s16le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    ffmpeg.creation_flags(0x08000000);
    let decode_error = || failure(StatusCode::GATEWAY_TIMEOUT, "Audio conversion timed out");
    let mut child = ffmpeg.spawn().map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "ffmpeg is required for transcription",
            )
        } else {
            decode_error()
        }
    })?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(upstream)?
        .take((PCM_LIMIT + 1) as u64);
    let work = async {
        let mut pcm = Vec::new();
        output.read_to_end(&mut pcm).await.map_err(|_| upstream())?;
        if pcm.len() > PCM_LIMIT {
            let _ = child.kill().await;
            return Err(bad_input("Audio duration exceeds 120 seconds"));
        }
        let status = child.wait().await.map_err(|_| upstream())?;
        if !status.success() || pcm.is_empty() {
            return Err(bad_input("Invalid or unsupported audio"));
        }
        Ok(pcm)
    };
    match timeout(AUDIO_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            Err(bad_input("Invalid or unsupported audio"))
        }
    }
}

fn multipart(pcm: Vec<u8>) -> (String, Vec<u8>) {
    let boundary = format!("----9router-codex-{}", Uuid::new_v4());
    let prefix = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
    );
    let suffix = format!("\r\n--{boundary}--\r\n");
    let mut body = Vec::with_capacity(prefix.len() + 44 + pcm.len() + suffix.len());
    body.extend_from_slice(prefix.as_bytes());
    body.extend_from_slice(b"RIFF");
    body.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    body.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    body.extend_from_slice(&pcm);
    body.extend_from_slice(suffix.as_bytes());
    (boundary, body)
}

async fn send(
    client: &Client,
    endpoint: &str,
    token: &str,
    workspace: &str,
    pcm: Vec<u8>,
) -> Result<String, TranscriptionError> {
    let (boundary, body) = multipart(pcm);
    let mut response = client
        .post(endpoint)
        .bearer_auth(token)
        .header("chatgpt-account-id", workspace)
        .header(header::USER_AGENT, user_agent())
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .map_err(|_| upstream())?;
    let status = response.status();
    if !status.is_success() {
        return Err(failure(
            if status.is_client_error() || status.is_server_error() {
                status
            } else {
                StatusCode::BAD_GATEWAY
            },
            "Codex transcription failed",
        ));
    }
    let too_large = || failure(StatusCode::BAD_GATEWAY, "Body exceeds size limit");
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_LIMIT as u64)
    {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| upstream())? {
        if bytes.len() + chunk.len() > RESPONSE_LIMIT {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    let invalid = || failure(StatusCode::BAD_GATEWAY, "Invalid transcription response");
    let value: Value =
        serde_json::from_str(&String::from_utf8_lossy(&bytes)).map_err(|_| invalid())?;
    let text = value["text"].as_str().ok_or_else(invalid)?;
    if text.encode_utf16().count() > 250000 {
        return Err(invalid());
    }
    Ok(text.to_owned())
}
