use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{MAX_LINE, elevated};

const MODE: &str = "--9router-devin-client-tools";
const SERVER: &str = "clientTools";
const SNAPSHOT: &str = "client-tools.json";
const CALL: &str = "client-tool-call.json";
const SNAPSHOT_ENV: &str = "NINEROUTER_DEVIN_TOOL_SNAPSHOT";
const NONCE_ENV: &str = "NINEROUTER_DEVIN_TOOL_NONCE";

pub(super) struct Tools {
    descriptions: Vec<Value>,
    names: HashMap<String, String>,
    results: Vec<Value>,
}

impl Tools {
    pub(super) fn from_chat(body: &Value) -> Result<Self, &'static str> {
        let raw = match &body["tools"] {
            Value::Array(tools) => tools.as_slice(),
            Value::Null => &[],
            _ => return Err("Invalid Devin client tool list"),
        };
        let mut descriptions = Vec::new();
        let mut names = HashMap::new();
        for tool in raw {
            if tool["type"].as_str().is_some_and(|kind| kind != "function") {
                return Err("Devin CLI supports client function tools only");
            }
            let function = tool.get("function").unwrap_or(tool);
            let name = function["name"]
                .as_str()
                .filter(|name| valid_name(name))
                .ok_or("Invalid Devin client tool name")?;
            let native = native_name(name);
            if names.insert(native.clone(), name.to_owned()).is_some() {
                return Err("Duplicate Devin client tool name");
            }
            let schema = match function
                .get("parameters")
                .or_else(|| function.get("input_schema"))
            {
                Some(schema) if schema.is_object() => schema.clone(),
                Some(Value::Null) | None => json!({"type":"object","properties":{}}),
                _ => return Err("Invalid Devin client tool schema"),
            };
            descriptions.push(json!({"name":native,"description":function["description"].as_str().unwrap_or_default(),"inputSchema":schema}));
        }
        let mut calls = HashMap::new();
        let mut results = Vec::new();
        for message in body["messages"].as_array().into_iter().flatten() {
            if message["role"] == "assistant" {
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let function = &call["function"];
                    if let (Some(id), Some(name), Some(args)) = (
                        call["id"].as_str(),
                        function["name"].as_str(),
                        arguments(&function["arguments"]),
                    ) {
                        let native = native_name(name);
                        if names.get(&native).is_some_and(|declared| declared == name) {
                            calls.insert(id.to_owned(), (native, args));
                        }
                    }
                }
            }
            if message["role"] == "tool"
                && let Some(id) = message["tool_call_id"].as_str()
                && let Some((name, args)) = calls.remove(id)
            {
                let content = message["content"]
                    .as_str()
                    .map_or_else(|| message["content"].to_string(), str::to_owned);
                results.push(json!({"name":name,"arguments":args,"text":content}));
            }
        }
        Ok(Self {
            descriptions,
            names,
            results,
        })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.descriptions.is_empty()
    }

    pub(super) fn config(&self, workspace: &Path) -> io::Result<Value> {
        if self.is_empty() {
            return Ok(json!({"mcpServers":{}}));
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let path = workspace.join(SNAPSHOT);
        let bytes = serde_json::to_vec(
            &json!({"nonce":nonce,"tools":self.descriptions,"results":self.results}),
        )?;
        if bytes.len() > MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Devin client tool data exceeded the limit",
            ));
        }
        let mut file = tempfile::NamedTempFile::new_in(workspace)?;
        file.write_all(&bytes)?;
        file.persist_noclobber(&path).map_err(|error| error.error)?;
        let executable = std::env::current_exe()?;
        let executable = executable.to_str().ok_or_else(invalid)?;
        let path = path.to_str().ok_or_else(invalid)?;
        let allow: Vec<_> = self
            .descriptions
            .iter()
            .filter_map(|tool| Some(format!("mcp__{SERVER}__{}", tool["name"].as_str()?)))
            .collect();
        Ok(json!({
            "mcpServers":{(SERVER):{"command":executable,"args":[MODE],"env":{(SNAPSHOT_ENV):path,(NONCE_ENV):nonce}}},
            "permission_mode":"normal",
            "permissions":{"allow":allow,"deny":["read","write","edit","grep","glob","exec","fetch","terminal","web_search","subagent","handoff"]},
            "read_config_from":{"agents_standard":false,"cursor":false,"windsurf":false,"claude":false,"copilot":false,"codex":false,"kiro":false,"opencode":false,"zed":false},
        }))
    }

    pub(super) fn recognizes_title(&self, title: &str) -> bool {
        title
            .strip_prefix("Calling ")
            .and_then(|title| title.strip_suffix(" from clientTools"))
            .is_some_and(|name| self.names.contains_key(name))
    }

    pub(super) fn receipt(&self, path: &Path) -> io::Result<Option<(String, Value)>> {
        let call = match read_data(path) {
            Ok(call) => call,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let name = call["name"]
            .as_str()
            .and_then(|name| self.names.get(name))
            .ok_or_else(invalid)?;
        let args = arguments(&call["arguments"]).ok_or_else(invalid)?;
        Ok(Some((name.clone(), args)))
    }
}

pub(super) fn receipt_path(workspace: &Path) -> PathBuf {
    workspace.join(CALL)
}

pub(crate) async fn stdio(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some(MODE) {
        return None;
    }
    if args.len() != 1 || elevated() {
        return Some(2);
    }
    Some(match tokio::task::spawn_blocking(serve).await {
        Ok(Ok(())) => 0,
        _ => 2,
    })
}

fn serve() -> io::Result<()> {
    let path = std::env::var_os(SNAPSHOT_ENV)
        .map(PathBuf::from)
        .ok_or_else(invalid)?;
    let nonce = std::env::var(NONCE_ENV).map_err(|_| invalid())?;
    let parent = path.parent().ok_or_else(invalid)?;
    if !path.is_absolute()
        || path.file_name().and_then(|name| name.to_str()) != Some(SNAPSHOT)
        || !parent
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("9router-devin-"))
        || nonce.len() != 32
        || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !private(parent, true)
        || !private(&path, false)
    {
        return Err(invalid());
    }
    let snapshot = read_data(&path)?;
    if snapshot["nonce"] != nonce {
        return Err(invalid());
    }
    let descriptions = snapshot["tools"].as_array().ok_or_else(invalid)?;
    let results = snapshot["results"].as_array().ok_or_else(invalid)?;
    let mut names = HashSet::new();
    for tool in descriptions {
        let name = tool["name"]
            .as_str()
            .filter(|name| {
                name.starts_with("mcp_") && name.len() <= 68 && name.bytes().all(valid_name_byte)
            })
            .ok_or_else(invalid)?;
        if !names.insert(name) || !tool["inputSchema"].is_object() {
            return Err(invalid());
        }
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    loop {
        let mut bytes = Vec::new();
        let count = (&mut input)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            return Ok(());
        }
        if count > MAX_LINE {
            return Err(invalid());
        }
        let Ok(request) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(id) = request
            .get("id")
            .filter(|id| id.is_number() || id.is_string())
        else {
            continue;
        };
        let body = match request["method"].as_str() {
            Some("initialize") => {
                json!({"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":SERVER,"version":"1.0"}}})
            }
            Some("tools/list") => json!({"result":{"tools":descriptions}}),
            Some("tools/call") => {
                let params = &request["params"];
                match (
                    params["name"].as_str().filter(|name| names.contains(name)),
                    arguments(&params["arguments"]),
                ) {
                    (Some(name), Some(args)) => {
                        let prior = results
                            .iter()
                            .rev()
                            .find(|result| result["name"] == name && result["arguments"] == args)
                            .and_then(|result| result["text"].as_str());
                        let text = if let Some(prior) = prior {
                            prior
                        } else {
                            let mut file = tempfile::NamedTempFile::new_in(parent)?;
                            serde_json::to_writer(
                                &mut file,
                                &json!({"name":name,"arguments":args}),
                            )?;
                            match file.persist_noclobber(receipt_path(parent)) {
                                Ok(_) => (),
                                Err(error)
                                    if error.error.kind() == io::ErrorKind::AlreadyExists => {}
                                Err(error) => return Err(error.error),
                            }
                            "(awaiting client tool result)"
                        };
                        json!({"result":{"content":[{"type":"text","text":text}]}})
                    }
                    _ => {
                        json!({"error":{"code":-32602,"message":"Unknown tool or invalid arguments"}})
                    }
                }
            }
            Some("ping") => json!({"result":{}}),
            _ => json!({"error":{"code":-32601,"message":"Method not available"}}),
        };
        let mut reply = body;
        reply["jsonrpc"] = json!("2.0");
        reply["id"] = id.clone();
        writeln!(output, "{reply}")?;
        output.flush()?;
    }
}

fn native_name(name: &str) -> String {
    if name.starts_with("mcp_") {
        name.to_owned()
    } else {
        format!("mcp_{name}")
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(valid_name_byte)
}

fn valid_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn arguments(value: &Value) -> Option<Value> {
    let value = match value {
        Value::String(args) => serde_json::from_str(args).ok()?,
        Value::Null => json!({}),
        _ => value.clone(),
    };
    value.is_object().then_some(value)
}

fn read_data(path: &Path) -> io::Result<Value> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let file = {
        use rustix::fs::{Mode, OFlags, open};
        fs::File::from(open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )?)
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let file = {
        if !fs::symlink_metadata(path)?.is_file() {
            return Err(invalid());
        }
        fs::File::open(path)?
    };
    if !file.metadata()?.is_file() {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(MAX_LINE as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LINE {
        return Err(invalid());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn private(path: &Path, directory: bool) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        if if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file()
        } {
            return false;
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::os::unix::fs::MetadataExt;
            metadata.uid() == rustix::process::geteuid().as_raw() && metadata.mode() & 0o077 == 0
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        true
    })
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Invalid Devin client tool session",
    )
}
