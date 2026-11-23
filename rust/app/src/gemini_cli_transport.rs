use std::sync::OnceLock;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::google_code_assist::saved_project;

static CONFIG: OnceLock<Value> = OnceLock::new();

fn conf() -> &'static Value {
    CONFIG.get_or_init(|| {
        serde_json::from_str(include_str!("gemini-cli-config.json"))
            .expect("embedded Gemini CLI transport")
    })
}

pub fn signature() -> &'static str {
    conf()["signature"].as_str().unwrap_or_default()
}

pub fn envelope(body: &mut Value, account: &Value, session: String) -> Option<()> {
    let project = saved_project(account)?;
    let session = numeric_session(session);
    let model = body["model"].clone();
    let mut request = json!({"sessionId":session});
    for key in [
        "contents",
        "systemInstruction",
        "generationConfig",
        "tools",
        "safetySettings",
    ] {
        if let Some(value) = body.get_mut(key) {
            request[key] = std::mem::take(value);
        }
    }
    if request["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty())
    {
        request["toolConfig"] = json!({"functionCallingConfig":{"mode":"VALIDATED"}});
    }
    *body = json!({
        "project":project,"model":model,"userAgent":"gemini-cli",
        "requestId":format!("agent-{}",uuid::Uuid::new_v4()),"request":request,
    });
    Some(())
}

pub(super) fn numeric_session(session: String) -> String {
    let digits = session.strip_prefix('-').unwrap_or(&session);
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return session;
    }
    let hash = Sha256::digest(session.as_bytes());
    let head = u64::from_be_bytes(std::array::from_fn(|index| hash[index]));
    format!("-{}", head & 0x7fff_ffff_ffff_ffff)
}

pub fn url(stream: bool) -> String {
    let base = conf()["baseUrl"].as_str().unwrap_or_default();
    format!(
        "{base}:{}",
        if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        }
    )
}

pub fn local_url(stream: bool) -> Option<String> {
    (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")).then(|| {
        format!(
            "http://127.0.0.1:52989/gemini-cli/v1internal:{}",
            if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            }
        )
    })
}

pub fn headers(
    request: reqwest::RequestBuilder,
    token: &str,
    model: &str,
    stream: bool,
) -> reqwest::RequestBuilder {
    let version = conf()["cliVersion"].as_str().unwrap_or_default();
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        arch => arch,
    };
    let text = format!("GeminiCLI/{version}/{model} ({os}; {arch}; terminal)");
    request
        .bearer_auth(token)
        .header("user-agent", text)
        .header(
            "x-goog-api-client",
            conf()["apiClient"].as_str().unwrap_or_default(),
        )
        .header(
            "accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
}
