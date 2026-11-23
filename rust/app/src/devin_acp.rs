use std::fs;
use std::io;
use std::process::Stdio;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures_util::stream;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::chat_format::ChatFormat;
use crate::chat_output::{self, ResponseWire};
use crate::provider_index::js_trim;
use crate::storage::Storage;

#[path = "devin_client_tools.rs"]
mod client_tools;
use client_tools::Tools;
pub(crate) use client_tools::stdio as client_tools_stdio;

const PROVIDER: &str = "devin-cli";
const MAX_LINE: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(180);

pub(crate) fn requested_model(model: &str) -> Option<&str> {
    let (provider, model) = model.split_once('/')?;
    matches!(provider, PROVIDER | "devin" | "dv").then_some(model)
}

pub(crate) struct Caller<'a> {
    pub(crate) direct_loopback: bool,
    pub(crate) key: Option<&'a str>,
    pub(crate) format: ChatFormat,
    pub(crate) accept: &'a str,
}

pub(crate) async fn respond(
    storage: &Storage,
    settings: &Value,
    caller: Caller<'_>,
    body: Value,
    model: &str,
) -> Response {
    if !caller.direct_loopback || elevated() {
        return chat_output::error(
            StatusCode::FORBIDDEN,
            "Devin CLI requires a direct local request to a non-elevated gateway",
        );
    }
    let Some(key) = caller.key.filter(|key| !key.is_empty()).map(str::to_owned) else {
        return chat_output::error(StatusCode::UNAUTHORIZED, "Missing API key");
    };
    if !storage.api_key_is_active(key.clone()).await {
        return chat_output::error(StatusCode::UNAUTHORIZED, "Invalid API key");
    }
    let target = model.to_owned();
    if !storage
        .run(false, move |db| {
            crate::key_policy::permits_resolved(db, &key, PROVIDER, &target)
        })
        .await
        .unwrap_or(false)
    {
        return chat_output::error(
            StatusCode::FORBIDDEN,
            &format!("This API key is not allowed to use model \"{PROVIDER}/{model}\""),
        );
    }
    if crate::provider_http::requires_protection(settings, &Value::Null) {
        return chat_output::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Devin CLI cannot use the configured outbound proxy",
        );
    }
    if model.is_empty() {
        return chat_output::error(StatusCode::BAD_REQUEST, "Missing model");
    }
    let Caller { format, accept, .. } = caller;
    let Some(translated) = crate::chat_protocol_input::translate(format, body.clone(), accept)
    else {
        return chat_output::error(
            StatusCode::BAD_REQUEST,
            "Devin CLI supports text-only requests",
        );
    };
    let tools = match Tools::from_chat(&translated) {
        Ok(tools) => tools,
        Err(error) => return chat_output::error(StatusCode::BAD_REQUEST, error),
    };
    let Some(prompt) = prompt(&translated, !tools.is_empty()) else {
        return chat_output::error(
            StatusCode::BAD_REQUEST,
            "Devin CLI supports text-only requests",
        );
    };
    if prompt.len() > MAX_LINE {
        return chat_output::error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Devin CLI prompt is too large",
        );
    }
    let streamed = crate::chat_input::stream_requested(&translated, accept);
    let session = tokio::task::spawn_blocking(move || Session::launch(tools)).await;
    let session = match session {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => return chat_output::error(StatusCode::BAD_GATEWAY, error),
        Err(_) => return chat_output::error(StatusCode::BAD_GATEWAY, "Could not start Devin CLI"),
    };
    let (sender, receiver) = mpsc::channel::<Bytes>(8);
    let selected = model.to_owned();
    tokio::spawn(run(session, sender, selected, prompt));
    let streamed_body =
        reqwest::Body::wrap_stream(stream::unfold(receiver, |mut receiver| async move {
            let chunk = receiver.recv().await?;
            Some((Ok::<_, io::Error>(chunk), receiver))
        }));
    let upstream = axum::http::Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(streamed_body);
    let Ok(upstream) = upstream else {
        return chat_output::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not open the Devin CLI response",
        );
    };
    chat_output::respond(
        upstream.into(),
        chat_output::Output {
            request_body: translated,
            source_body: Some(body),
            provider: PROVIDER,
            model,
            wire: ResponseWire::Chat,
            format,
        },
        streamed,
        false,
        None,
        None,
    )
    .await
}

fn elevated() -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return rustix::process::geteuid().as_raw() == 0;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    false
}

fn prompt(body: &Value, tools: bool) -> Option<String> {
    let messages = body["messages"]
        .as_array()
        .or_else(|| body["input"].as_array());
    let mut turns = Vec::new();
    for message in messages.into_iter().flatten() {
        if !tools
            && (message["role"] == "tool"
                || message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty()))
        {
            return None;
        }
        let mut content = match &message["content"] {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            Value::Array(parts) => {
                let mut text = String::new();
                for part in parts {
                    if !matches!(
                        part["type"].as_str(),
                        Some("text" | "input_text" | "output_text")
                    ) {
                        return None;
                    }
                    text.push_str(part["text"].as_str().unwrap_or_default());
                }
                text
            }
            _ => return None,
        };
        if message["role"] == "assistant" {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                let function = &call["function"];
                if let (Some(name), Some(id)) = (function["name"].as_str(), call["id"].as_str()) {
                    let arguments = function["arguments"]
                        .as_str()
                        .map_or_else(|| function["arguments"].to_string(), str::to_owned);
                    if !content.is_empty() {
                        content.push_str("\n\n");
                    }
                    content.push_str(&format!("[Tool call {name} id={id}]\n{arguments}"));
                }
            }
        }
        if message["role"] == "tool" {
            let id = message["tool_call_id"].as_str().unwrap_or_default();
            content = format!("[Tool result id={id}]\n{content}");
        }
        if js_trim(&content).is_empty() {
            continue;
        }
        let role = match message["role"].as_str() {
            Some("system") => "System",
            Some("assistant") => "Assistant",
            Some("tool") => "Tool",
            _ => "User",
        };
        turns.push(format!("[{role}]\n{content}"));
    }
    Some(if turns.is_empty() {
        "(empty)".into()
    } else {
        turns.join("\n\n")
    })
}

struct Session {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    workspace: TempDir,
    tools: Tools,
}

impl Session {
    fn launch(tools: Tools) -> Result<Self, &'static str> {
        let binary = crate::cli_tools::devin_binary().ok_or("Devin CLI is not installed")?;
        let mut builder = tempfile::Builder::new();
        builder.prefix("9router-devin-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let workspace = builder
            .tempdir()
            .map_err(|_| "Could not create a Devin workspace")?;
        let config = workspace.path().join("config");
        let devin = config.join("devin");
        fs::create_dir_all(&devin).map_err(|_| "Could not create a Devin workspace")?;
        let configuration = tools
            .config(workspace.path())
            .map_err(|_| "Could not configure Devin client tools")?;
        fs::write(devin.join("config.json"), configuration.to_string())
            .map_err(|_| "Could not configure Devin CLI")?;
        if !tools.is_empty() {
            fs::write(
                devin.join("mcp_config.json"),
                json!({"mcpServers":configuration["mcpServers"]}).to_string(),
            )
            .map_err(|_| "Could not configure Devin client tools")?;
        }
        let mut command = Command::new(binary);
        command
            .args(["acp", "--agent-type", "summarizer"])
            .env_clear()
            .env("XDG_CONFIG_HOME", &config)
            .env("APPDATA", &config)
            .current_dir(workspace.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for variable in [
            "HOME",
            "PATH",
            "XDG_DATA_HOME",
            "USERPROFILE",
            "LOCALAPPDATA",
            "SystemRoot",
            "LANG",
            "LC_ALL",
        ] {
            if let Some(value) = std::env::var_os(variable) {
                command.env(variable, value);
            }
        }
        let mut child = command.spawn().map_err(|_| "Could not start Devin CLI")?;
        let input = child.stdin.take().ok_or("Could not open Devin CLI stdin")?;
        let output = child
            .stdout
            .take()
            .ok_or("Could not open Devin CLI stdout")?;
        Ok(Self {
            child,
            input: Some(input),
            output: BufReader::new(output),
            workspace,
            tools,
        })
    }

    async fn send(&mut self, frame: Value) -> Result<(), &'static str> {
        let input = self.input.as_mut().ok_or("Devin ACP input closed")?;
        input
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .map_err(|_| "Could not write to Devin ACP")
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Result<(), &'static str> {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await
    }

    async fn next(&mut self) -> Result<Option<Value>, &'static str> {
        const REQUEST_PERMISSION: &str = "session/request_permission";
        loop {
            let mut bytes = Vec::new();
            let count = (&mut self.output)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut bytes)
                .await
                .map_err(|_| "Could not read Devin ACP output")?;
            if count == 0 {
                return Ok(None);
            }
            if count > MAX_LINE {
                return Err("Devin ACP frame exceeded the limit");
            }
            let Ok(frame) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let Some(method) = frame["method"].as_str() else {
                return Ok(Some(frame));
            };
            let Some(id) = frame.get("id").filter(|id| !id.is_null()) else {
                return Ok(Some(frame));
            };
            let reply = if method == REQUEST_PERMISSION {
                let reject = frame["params"]["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|option| option["kind"] == "reject_once")
                    .and_then(|option| option["optionId"].as_str());
                let outcome = match reject {
                    Some(option) => json!({"outcome":"selected","optionId":option}),
                    None => json!({"outcome":"cancelled"}),
                };
                json!({"jsonrpc":"2.0","id":id,"result":{"outcome":outcome}})
            } else {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not available"}})
            };
            self.send(reply).await?;
        }
    }

    async fn result(&mut self, id: u64) -> Result<Value, &'static str> {
        while let Some(frame) = self.next().await? {
            if frame["id"].as_u64() == Some(id) {
                return frame
                    .get("result")
                    .cloned()
                    .ok_or("Devin ACP rejected the session request");
            }
        }
        Err("Devin ACP closed during session setup")
    }

    async fn complete(
        &mut self,
        output: &mut Output,
        sender: &mpsc::Sender<Bytes>,
        prompt: &str,
    ) -> Result<&'static str, &'static str> {
        self.request(1, "initialize", json!({"protocolVersion":"0.3","clientInfo":{"name":"9router","version":"1.0"},"capabilities":{}})).await?;
        self.result(1).await?;
        let cwd = self
            .workspace
            .path()
            .to_str()
            .ok_or("Could not resolve the Devin workspace")?;
        self.request(
            2,
            "session/new",
            json!({"cwd":cwd,"mcpServers":[],"model":output.model}),
        )
        .await?;
        let session = self.result(2).await?;
        let id = session["sessionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("Devin ACP session is missing its identifier")?;
        self.request(
            3,
            "session/prompt",
            json!({"sessionId":id,"prompt":[{"type":"text","text":prompt}]}),
        )
        .await?;
        loop {
            let Some(frame) = self.next_prompt_frame(output, sender).await? else {
                return if output.tool_sent {
                    Ok("tool_calls")
                } else if output.role_sent {
                    Ok("stop")
                } else {
                    Err("Devin ACP closed without a response")
                };
            };
            if frame["id"].as_u64() == Some(3) && frame["method"].is_null() {
                let result = frame.get("result").ok_or("Devin ACP rejected the prompt")?;
                if result["stopReason"] == "cancelled" {
                    return Err("Devin ACP cancelled the prompt");
                }
                if !output.role_sent {
                    output.text(sender, &final_text(result)).await?;
                }
                return Ok("stop");
            }
            let params = &frame["params"];
            if params["sessionId"]
                .as_str()
                .is_some_and(|session| session != id)
            {
                continue;
            }
            match frame["method"].as_str() {
                Some("_cognition.ai/agent_stopped" | "$/agent_stopped")
                    if params["cause"] == "error" =>
                {
                    return Err("Devin ACP reported an agent error");
                }
                Some("_cognition.ai/agent_stopped" | "$/agent_stopped") => return Ok("stop"),
                Some("session/update" | "$/update") => (),
                _ => continue,
            }
            let update = &params["update"];
            let kind = update["sessionUpdate"]
                .as_str()
                .or_else(|| params["type"].as_str());
            match kind {
                Some("agent_message_chunk" | "message_delta" | "text_delta" | "content_delta") => {
                    let content = update.get("content").unwrap_or(&params["content"]);
                    let text = content
                        .as_str()
                        .or_else(|| content["text"].as_str())
                        .or_else(|| params["delta"].as_str())
                        .or_else(|| params["text"].as_str())
                        .unwrap_or_default();
                    output.text(sender, text).await?;
                }
                Some("message_stop" | "stop" | "done") => return Ok("stop"),
                Some("tool_call" | "tool_call_update") => {
                    if self.tools.is_empty()
                        || update["title"]
                            .as_str()
                            .is_some_and(|title| !self.tools.recognizes_title(title))
                    {
                        return Err("Devin ACP attempted to use an unavailable tool");
                    }
                }
                Some("error") => return Err("Devin ACP reported a stream error"),
                _ => (),
            }
        }
    }

    async fn next_prompt_frame(
        &mut self,
        output: &mut Output,
        sender: &mpsc::Sender<Bytes>,
    ) -> Result<Option<Value>, &'static str> {
        if self.tools.is_empty() {
            return self.next().await;
        }
        let path = client_tools::receipt_path(self.workspace.path());
        let receipt_ready = async {
            while tokio::fs::symlink_metadata(&path)
                .await
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let frame = tokio::select! {
            frame = self.next() => frame?,
            _ = receipt_ready => None,
        };
        if let Some((name, arguments)) = self
            .tools
            .receipt(&path)
            .map_err(|_| "Invalid Devin client tool request")?
        {
            output.tool(sender, name, arguments).await?;
            return Ok(None);
        }
        Ok(frame)
    }
}

fn final_text(result: &Value) -> String {
    for candidate in [
        &result["content"],
        &result["text"],
        &result["message"]["content"],
    ] {
        if let Some(text) = candidate.as_str() {
            return text.to_owned();
        }
    }
    result["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "assistant")
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

struct Output {
    id: String,
    model: String,
    created: i64,
    role_sent: bool,
    tool_sent: bool,
    content_units: usize,
}

impl Output {
    fn chunk(&self, delta: Value, reason: Value) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,
               "choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    }

    async fn text(&mut self, sender: &mpsc::Sender<Bytes>, text: &str) -> Result<(), &'static str> {
        if text.is_empty() {
            return Ok(());
        }
        if !self.role_sent {
            emit(
                sender,
                self.chunk(json!({"role":"assistant","content":""}), Value::Null),
            )
            .await?;
            self.role_sent = true;
        }
        self.content_units += text.encode_utf16().count();
        emit(sender, self.chunk(json!({"content":text}), Value::Null)).await
    }

    async fn tool(
        &mut self,
        sender: &mpsc::Sender<Bytes>,
        name: String,
        arguments: Value,
    ) -> Result<(), &'static str> {
        if !self.role_sent {
            emit(
                sender,
                self.chunk(json!({"role":"assistant","content":null}), Value::Null),
            )
            .await?;
            self.role_sent = true;
        }
        let call_id = format!("call_{}", uuid::Uuid::new_v4().simple());
        let call = json!({"index":0,"id":call_id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}});
        emit(
            sender,
            self.chunk(json!({"tool_calls":[call]}), Value::Null),
        )
        .await?;
        self.tool_sent = true;
        Ok(())
    }
}

async fn emit(sender: &mpsc::Sender<Bytes>, value: Value) -> Result<(), &'static str> {
    sender
        .send(Bytes::from(format!("data: {value}\n\n")))
        .await
        .map_err(|_| "Devin CLI client disconnected")
}

async fn run(mut session: Session, sender: mpsc::Sender<Bytes>, model: String, prompt: String) {
    let now = chrono::Utc::now();
    let mut output = Output {
        id: format!("chatcmpl-devin-{}", now.timestamp_millis()),
        model,
        created: now.timestamp(),
        role_sent: false,
        tool_sent: false,
        content_units: 0,
    };
    let result = tokio::select! {
        _ = sender.closed() => return,
        response = tokio::time::timeout(TIMEOUT, session.complete(&mut output, &sender, &prompt)) => {
            response.unwrap_or(Err("Devin ACP timed out"))
        }
    };
    let frame = match result {
        Ok(reason) => {
            let prompt_units = prompt.encode_utf16().count();
            let mut frame = output.chunk(json!({}), json!(reason));
            frame["usage"] = json!({"prompt_tokens":prompt_units.div_ceil(4),"completion_tokens":output.content_units.div_ceil(4),
                "total_tokens":(prompt_units + output.content_units).div_ceil(4),"estimated":true});
            frame
        }
        Err(message) => json!({"error":{"message":message,"type":"devin_cli_error"}}),
    };
    drop(session.input.take());
    let finish = async {
        let _ = emit(&sender, frame).await;
        let _ = sender.send(Bytes::from_static(b"data: [DONE]\n\n")).await;
    };
    let reap = async {
        if tokio::time::timeout(Duration::from_secs(2), session.child.wait())
            .await
            .is_err()
        {
            let _ = session.child.kill().await;
        }
    };
    tokio::join!(finish, reap);
}
