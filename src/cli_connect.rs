use std::collections::HashSet;
use std::fs;
use std::io::{self, ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};

use futures_util::future::join_all;
use reqwest::header::{ACCEPT, AUTHORIZATION, COOKIE, HeaderMap, SET_COOKIE};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use url::Url;

use crate::cli::{Tool, js_space, mask_key, resolve_tools, without_trailing_commas};

#[path = "cli_connect_prompt.rs"]
mod prompt;

#[path = "cli_connect_tools.rs"]
pub(crate) mod tools;

const MODELS: [(&str, &str); 4] = [
    ("fable", "ANTHROPIC_DEFAULT_FABLE_MODEL"),
    ("opus", "ANTHROPIC_DEFAULT_OPUS_MODEL"),
    ("sonnet", "ANTHROPIC_DEFAULT_SONNET_MODEL"),
    ("haiku", "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
];

pub async fn run(args: &[String]) -> Option<i32> {
    let home = std::env::home_dir()?;
    let data_dir = data_dir(&home);
    let password = std::env::var("NINE_ROUTER_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty());
    run_in(
        args,
        &home,
        &data_dir,
        password,
        &hostname(),
        io::stdin().is_terminal(),
        std::env::var_os("XDG_CONFIG_HOME")
            .as_deref()
            .map(Path::new),
    )
    .await
}

pub(crate) async fn run_in(
    args: &[String],
    home: &Path,
    data_dir: &Path,
    password_env: Option<String>,
    host: &str,
    tty: bool,
    config_home: Option<&Path>,
) -> Option<i32> {
    let options = Options::parse(args, password_env, host)?;
    let tools = if let Some(input) = options.tools {
        resolve_tools(input.split(',')).ok()?
    } else if tty {
        match prompted(prompt::tools(home, config_home)) {
            Ok(tools) => tools,
            Err(status) => return Some(status),
        }
    } else {
        vec![Tool::Claude]
    };
    let paths = tools::Paths { home, config_home };
    if options.reset {
        let mut failed = false;
        for tool in tools {
            match tool.reset(&paths) {
                Ok(files) if files.is_empty() => println!("• {}: nothing to reset", tool.name()),
                Ok(files) => println!(
                    "✅ {}: removed 9router settings ({})",
                    tool.name(),
                    display_paths(&files),
                ),
                Err(error) => {
                    println!("❌ {}: {error}", tool.name());
                    failed = true;
                }
            }
        }
        return Some(i32::from(failed));
    }

    let server = normalize_server(options.url?).ok()?;
    if Url::parse(&server).is_ok_and(|url| {
        url.scheme() == "http"
            && !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
    }) {
        println!(
            "\x1b[33m⚠ Plain HTTP: password and API key travel unencrypted. Use only on a trusted LAN/VPN.\x1b[0m"
        );
    }
    let saved_file = data_dir.join("connect.env");
    let saved =
        if options.api_key.is_none() && options.password.as_deref().is_none_or(str::is_empty) {
            saved_password(&saved_file, &server)
        } else {
            None
        };
    let entered;
    let mut password = options.password.as_deref().or(saved.as_deref());
    if tty && options.api_key.is_none() && password.is_none() {
        entered = match prompted(prompt::password()) {
            Ok(password) => password,
            Err(status) => return Some(status),
        };
        password = Some(entered.as_str());
    }
    let mut secrets: Vec<String> = [password, options.api_key]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect();
    match workflow(
        &options,
        &tools,
        &paths,
        &server,
        Credentials {
            file: &saved_file,
            saved: saved.as_deref(),
            password,
        },
        &mut secrets,
    )
    .await
    {
        Ok(status) => Some(status),
        Err(error) => {
            eprintln!("❌ {}", clean(&error, &secrets));
            Some(1)
        }
    }
}

fn prompted<T>(result: io::Result<Option<T>>) -> Result<T, i32> {
    match result {
        Ok(Some(value)) => Ok(value),
        Ok(None) => {
            println!("Cancelled.");
            Err(130)
        }
        Err(error) => {
            eprintln!("❌ Terminal prompt failed: {error}");
            Err(1)
        }
    }
}

struct Options<'a> {
    url: Option<&'a str>,
    tools: Option<&'a str>,
    password: Option<String>,
    key_name: String,
    api_key: Option<&'a str>,
    models: [Option<&'a str>; MODELS.len()],
    model: Option<&'a str>,
    reset: bool,
    save: bool,
    print: bool,
}

impl<'a> Options<'a> {
    fn parse(args: &'a [String], password: Option<String>, host: &str) -> Option<Self> {
        let mut options = Self {
            url: None,
            tools: None,
            password,
            key_name: String::from_utf16_lossy(
                &format!("cli-{host}")
                    .encode_utf16()
                    .take(64)
                    .collect::<Vec<_>>(),
            ),
            api_key: None,
            models: [None; MODELS.len()],
            model: None,
            reset: false,
            save: false,
            print: false,
        };
        let mut args = args.iter().map(String::as_str);
        while let Some(arg) = args.next() {
            match arg {
                "--tools" => options.tools = Some(args.next()?),
                "--password" => options.password = Some(args.next()?.to_owned()),
                "--key-name" => options.key_name = args.next()?.to_owned(),
                "--api-key" => {
                    options.api_key = Some(args.next()?).filter(|value| !value.is_empty())
                }
                "--model" => options.model = Some(args.next()?).filter(|value| !value.is_empty()),
                "--save" => options.save = true,
                "--reset" => options.reset = true,
                "--print-env" => options.print = true,
                "-h" | "--help" => return None,
                arg if arg.starts_with("--") => {
                    let flag = MODELS.iter().position(|(flag, _)| *flag == &arg[2..])?;
                    options.models[flag] = args.next();
                    options.models[flag]?;
                }
                arg if !arg.starts_with('-') && options.url.is_none_or(str::is_empty) => {
                    options.url = Some(arg);
                }
                _ => return None,
            }
        }
        Some(options)
    }
}

fn data_dir(home: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("DATA_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("9router")
    } else {
        home.join(".9router")
    }
}

fn hostname() -> String {
    if let Ok(text) = fs::read_to_string("/proc/sys/kernel/hostname") {
        let name = text.trim_end();
        if !name.is_empty() {
            return name.into();
        }
    }
    if cfg!(windows)
        && let Ok(name) = std::env::var("COMPUTERNAME")
        && !name.is_empty()
    {
        return name;
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

fn normalize_server(url: &str) -> Result<String, String> {
    let trimmed = url.trim_matches(js_space);
    let lower = trimmed.to_ascii_lowercase();
    let url = if lower.starts_with("http://") || lower.starts_with("https://") {
        Url::parse(trimmed)
    } else {
        Url::parse(&format!("http://{trimmed}"))
    }
    .map_err(|_| "Invalid URL".to_owned())?;
    Ok(url.origin().ascii_serialization())
}

fn saved_password(file: &Path, server: &str) -> Option<String> {
    let data = fs::read_to_string(file).ok()?;
    let mut stored_server = None;
    let mut stored_password = None;
    for line in data.lines() {
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let output = match key {
            "NINE_ROUTER_SERVER" => &mut stored_server,
            "NINE_ROUTER_PASSWORD" => &mut stored_password,
            _ => continue,
        };
        *output = match serde_json::from_str::<Value>(raw) {
            Ok(value) => value.as_str().map(str::to_owned),
            Err(_) => Some(raw.to_owned()),
        };
    }
    if stored_server.as_deref() == Some(server) {
        stored_password.filter(|value| !value.is_empty())
    } else {
        None
    }
}

struct Credentials<'a> {
    file: &'a Path,
    saved: Option<&'a str>,
    password: Option<&'a str>,
}

async fn workflow(
    options: &Options<'_>,
    tools: &[Tool],
    paths: &tools::Paths<'_>,
    server: &str,
    credentials: Credentials<'_>,
    secrets: &mut Vec<String>,
) -> Result<i32, String> {
    let Credentials {
        file: saved_file,
        saved,
        password,
    } = credentials;
    let remote = Remote::new(server)?;
    let (key, cookie) = if let Some(key) = options.api_key {
        println!("• Using provided API key");
        (key.to_owned(), None)
    } else {
        if saved.is_some() {
            println!("• Using saved password ({})", saved_file.display());
        }
        let password =
            password.ok_or("Password required: pass --password or set NINE_ROUTER_PASSWORD")?;
        println!("• Logging in to {server}");
        let cookie = remote.login(password).await.map_err(|error| {
            if saved.is_some() {
                format!(
                    "{error} — saved password may be stale; delete {} or pass --password",
                    saved_file.display()
                )
            } else {
                error
            }
        })?;
        secrets.push(cookie.trim_start_matches("auth_token=").to_owned());
        if options.save && saved != Some(password) {
            let contents = format!(
                "NINE_ROUTER_SERVER={}\nNINE_ROUTER_PASSWORD={}\n",
                json!(server),
                json!(password)
            );
            write_private(saved_file, contents.as_bytes(), false)
                .map_err(|error| format!("Cannot save password: {error}"))?;
            println!(
                "• Saved password to {} (plain text, mode 600)",
                saved_file.display()
            );
        }
        let (key, created) = remote.key(&cookie, &options.key_name).await?;
        secrets.push(key.clone());
        println!(
            "• {} API key \"{}\" ({})",
            if created { "Created" } else { "Reusing" },
            clean(&options.key_name, secrets),
            mask_key(&key),
        );
        (key, Some(cookie))
    };

    let has_claude = tools.contains(&Tool::Claude);
    let mut settings = Value::Null;
    let mut configured = Vec::new();
    if let Some(cookie) = &cookie {
        if has_claude {
            settings = remote.tool_settings("claude-settings", cookie).await;
        }
        let mut wanted: Vec<_> = tools
            .iter()
            .filter_map(|tool| tool.route().map(|route| (*tool, route)))
            .collect();
        if !tools.contains(&Tool::OpenCode) && tools.iter().any(|tool| tool.needs_model()) {
            wanted.push((Tool::OpenCode, "opencode-settings"));
        }
        configured = join_all(wanted.into_iter().map(|(tool, route)| {
            let remote = &remote;
            async move {
                let settings = remote.tool_settings(route, cookie).await;
                (tool, tool.server_model(&settings))
            }
        }))
        .await;
    } else {
        println!(
            "• --api-key given: can't read the server's models (needs a dashboard login) — pass model flags"
        );
    }
    let available = remote.list_models(&key).await?;
    let warn_missing = |label: &str, model: &str, flag: &str| {
        let normalized = model.strip_suffix("[1m]").unwrap_or(model);
        if available
            .as_ref()
            .is_some_and(|ids| !ids.contains(normalized))
        {
            println!(
                "\x1b[33m⚠ {label}: \"{}\" not listed by server — override with {flag} <model>\x1b[0m",
                clean(model, secrets),
            );
        }
    };

    let mut claude_models = [None; MODELS.len()];
    let mut unset = Vec::new();
    if has_claude {
        for (i, (flag, env)) in MODELS.iter().enumerate() {
            claude_models[i] = options.models[i]
                .filter(|model| !model.is_empty())
                .or_else(|| {
                    settings["settings"]["env"][env]
                        .as_str()
                        .filter(|model| !model.is_empty())
                });
            if let Some(model) = claude_models[i] {
                warn_missing(&format!("claude {flag}"), model, &format!("--{flag}"));
            } else {
                unset.push(*flag);
            }
        }
    }
    let server_model = |tool| {
        configured
            .iter()
            .find(|(id, _)| *id == tool)
            .and_then(|(_, model)| model.as_deref())
    };
    let shared = server_model(Tool::OpenCode);
    let mut warned = HashSet::new();
    let mut inherited = Vec::new();
    let resolved: Vec<_> = tools
        .iter()
        .map(|tool| {
            if !tool.needs_model() {
                return (*tool, None);
            }
            let own = options.model.or_else(|| server_model(*tool));
            let model = own.or(shared);
            if own.is_none() && shared.is_some() {
                inherited.push(tool.id());
            }
            if let Some(model) = model
                && warned.insert(model)
            {
                warn_missing("model", model, "--model");
            }
            (*tool, model)
        })
        .collect();
    if !inherited.is_empty() {
        println!(
            "• {}: no own model on the server — using its OpenCode model",
            inherited.join(", ")
        );
    }

    let base = format!("{server}/v1");
    let mut failed = false;
    for (tool, model) in &resolved {
        if tool.needs_model() && model.is_none() {
            println!(
                "⏭  {}: no model configured on the server — skipped (pass --model <model>)",
                tool.name()
            );
            failed = true;
            continue;
        }
        match tool.apply(paths, server, &base, &key, *model, claude_models) {
            Ok(files) => println!("✅ {} → {}", tool.name(), display_paths(&files)),
            Err(error) => {
                println!("❌ {}: {}", tool.name(), clean(&error, secrets));
                failed = true;
            }
        }
    }
    println!("   Base URL: {base}");
    for (model, (_, env)) in claude_models.into_iter().zip(MODELS) {
        if let Some(model) = model {
            println!("   {env}={}", clean(model, secrets));
        }
    }
    if let Some(first) = unset.first() {
        println!(
            "   Claude {}: not set on the server — Claude Code's own default applies (or pass --{first} <model>)",
            unset.join(", ")
        );
    }
    for (tool, model) in resolved {
        if let Some(model) = model {
            println!("   {} model: {}", tool.id(), clean(model, secrets));
        }
    }
    if tools.contains(&Tool::Omp) {
        println!("   omp models: every server model, listed under 9router in /model");
    }
    let ids = tools
        .iter()
        .map(|tool| tool.id())
        .collect::<Vec<_>>()
        .join(",");
    println!("   Restart the tools to apply. Undo: npx 9router connect --reset --tools {ids}");
    if options.print {
        println!("\nOpenAI-compatible CLIs (codex, opencode, aider, …):");
        println!("   OPENAI_BASE_URL={base}");
        println!("   OPENAI_API_KEY={key}");
    }
    Ok(i32::from(failed))
}

fn display_paths(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|file| file.to_string_lossy())
        .collect::<Vec<_>>()
        .join(", ")
}

struct Remote<'a> {
    client: Client,
    server: &'a str,
}

struct Http {
    status: u16,
    headers: HeaderMap,
    data: Value,
}

impl<'a> Remote<'a> {
    fn new(server: &'a str) -> Result<Self, String> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| error.without_url().to_string())?;
        Ok(Self { client, server })
    }

    async fn request(
        &self,
        method: Method,
        route: &str,
        body: Option<Value>,
        cookie: Option<&str>,
        api_key: Option<&str>,
    ) -> Result<Http, String> {
        let mut request = self
            .client
            .request(method, format!("{}{route}", self.server))
            .header(ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(cookie) = cookie {
            request = request.header(COOKIE, cookie);
        }
        if let Some(key) = api_key {
            request = request.header(AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("Cannot reach {}: {}", self.server, http_error(error)))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let data = response.json().await.unwrap_or(Value::Null);
        Ok(Http {
            status,
            headers,
            data,
        })
    }

    async fn login(&self, password: &str) -> Result<String, String> {
        let response = self
            .request(
                Method::POST,
                "/api/auth/login",
                Some(json!({"password":password})),
                None,
                None,
            )
            .await?;
        if response.status == 200 && truthy(&response.data["success"]) {
            return auth_cookie(&response.headers)
                .ok_or_else(|| "Login succeeded but server returned no session cookie".into());
        }
        Err(format!(
            "Login failed ({}): {}",
            response.status,
            error_text(&response.data, "unknown error")
        ))
    }

    async fn key(&self, cookie: &str, name: &str) -> Result<(String, bool), String> {
        let response = self
            .request(Method::GET, "/api/keys", None, Some(cookie), None)
            .await?;
        if response.status == 401 {
            return Err(
                "Unauthorized listing API keys — wrong password or session rejected".into(),
            );
        }
        if response.status != 200 {
            return Err(format!(
                "Failed to list API keys ({}): {}",
                response.status,
                error_text(&response.data, "")
            ));
        }
        if truthy(&response.data["keys"]) && !response.data["keys"].is_array() {
            return Err("(list.data?.keys || []).filter is not a function".into());
        }
        if let Some(key) = response.data["keys"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|key| key["isActive"] != false && key["name"] == name)
        {
            return key["key"]
                .as_str()
                .filter(|key| !key.is_empty())
                .map(|key| (key.to_owned(), false))
                .ok_or_else(|| "Server returned an invalid API key".into());
        }
        let created = self
            .request(
                Method::POST,
                "/api/keys",
                Some(json!({"name":name})),
                Some(cookie),
                None,
            )
            .await?;
        if created.status == 201
            && let Some(key) = created.data["key"].as_str().filter(|key| !key.is_empty())
        {
            return Ok((key.to_owned(), true));
        }
        Err(format!(
            "Failed to create API key ({}): {}",
            created.status,
            error_text(&created.data, "")
        ))
    }

    async fn tool_settings(&self, tool: &str, cookie: &str) -> Value {
        self.request(
            Method::GET,
            &format!("/api/cli-tools/{tool}"),
            None,
            Some(cookie),
            None,
        )
        .await
        .ok()
        .filter(|response| response.status == 200)
        .map(|response| response.data)
        .unwrap_or(Value::Null)
    }

    async fn list_models(&self, key: &str) -> Result<Option<HashSet<String>>, String> {
        let response = self
            .request(Method::GET, "/v1/models", None, None, Some(key))
            .await?;
        if response.status == 401 {
            return Err("API key rejected by server (/v1/models returned 401)".into());
        }
        if response.status != 200 {
            return Ok(None);
        }
        if truthy(&response.data["data"]) && !response.data["data"].is_array() {
            return Err("(res.data?.data || []).map is not a function".into());
        }
        Ok(Some(
            response.data["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|model| model["id"].as_str().map(str::to_owned))
                .collect(),
        ))
    }
}

fn auth_cookie(headers: &HeaderMap) -> Option<String> {
    for header in headers.get_all(SET_COOKIE) {
        let Ok(value) = header.to_str() else {
            continue;
        };
        let candidates = std::iter::once(value).chain(
            value
                .match_indices(',')
                .map(|(index, _)| value[index + 1..].trim_start_matches(js_space)),
        );
        for candidate in candidates {
            let Some(rest) = candidate.strip_prefix("auth_token=") else {
                continue;
            };
            let token = rest.split(';').next().unwrap_or_default();
            if !token.is_empty() {
                return Some(format!("auth_token={token}"));
            }
        }
    }
    None
}

fn http_error(error: reqwest::Error) -> String {
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        if let Some(error) = cause.downcast_ref::<io::Error>() {
            let code = match error.kind() {
                ErrorKind::ConnectionRefused => "ECONNREFUSED",
                ErrorKind::ConnectionReset => "ECONNRESET",
                ErrorKind::TimedOut => "ETIMEDOUT",
                _ => "",
            };
            if !code.is_empty() {
                return code.to_owned();
            }
        }
        source = cause.source();
    }
    error.without_url().to_string()
}

fn error_text(value: &Value, fallback: &str) -> String {
    if !truthy(&value["error"]) {
        fallback.to_owned()
    } else if let Some(message) = value["error"].as_str() {
        message.to_owned()
    } else if value["error"].is_object() {
        "[object Object]".into()
    } else {
        value["error"].to_string()
    }
}

fn apply_claude(
    file: &Path,
    base: &str,
    key: &str,
    models: [Option<&str>; MODELS.len()],
) -> Result<(), String> {
    let existing = read_settings(file)?;
    let mut settings = existing
        .as_ref()
        .map(|(_, value)| value.clone())
        .filter(truthy)
        .unwrap_or_else(|| json!({}));
    let Some(root) = settings.as_object_mut() else {
        return Err("Settings must be a JSON object".into());
    };
    root.insert("hasCompletedOnboarding".into(), json!(true));
    let env = root.entry("env").or_insert_with(|| json!({}));
    if !truthy(env) {
        *env = json!({});
    }
    let Some(env) = env.as_object_mut() else {
        return Err("Settings env must be a JSON object".into());
    };
    env.insert("ANTHROPIC_BASE_URL".into(), json!(base));
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(key));
    for ((_, name), value) in MODELS.into_iter().zip(models) {
        if let Some(value) = value {
            env.insert(name.into(), json!(value));
        } else {
            env.shift_remove(name);
        }
    }
    let content = serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?;
    write_config(
        file,
        &content,
        existing.as_ref().map(|(raw, _)| raw.as_slice()),
    )
}

fn reset_claude(file: &Path) -> Result<bool, String> {
    let Some((_, mut settings)) = read_settings(file)? else {
        return Ok(false);
    };
    if !truthy(&settings) {
        return Ok(false);
    }
    if let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) {
        for key in ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"]
            .into_iter()
            .chain(MODELS.map(|(_, env)| env))
        {
            env.shift_remove(key);
        }
        if env.is_empty()
            && let Some(root) = settings.as_object_mut()
        {
            root.shift_remove("env");
        }
    }
    let content = serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?;
    write_private(file, &content, true).map_err(|error| error.to_string())?;
    Ok(true)
}

pub(crate) fn read_settings(path: &Path) -> Result<Option<(Vec<u8>, Value)>, String> {
    let Some(bytes) = read_config(path)? else {
        return Ok(None);
    };
    let input = String::from_utf8_lossy(&bytes);
    let parsed = serde_json::from_slice(&without_trailing_commas(input.as_bytes()))
        .map_err(|_| format!("Cannot parse {}: invalid JSON", path.display()))?;
    Ok(Some((bytes, parsed)))
}

fn read_config(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Cannot parse {}: {}", path.display(), error.kind())),
    }
}

fn write_config(file: &Path, content: &[u8], old_content: Option<&[u8]>) -> Result<(), String> {
    if let Some(raw) = old_content {
        write_backup(file, raw).map_err(|error| error.to_string())?;
    }
    write_private(file, content, false).map_err(|error| error.to_string())
}

fn write_backup(file: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut backup = file.as_os_str().to_owned();
    backup.push(".bak-9router");
    let backup = PathBuf::from(backup);
    match fs::symlink_metadata(&backup) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    let mut tmp = NamedTempFile::new_in(parent(&backup))?;
    tmp.write_all(bytes)?;
    match tmp.persist_noclobber(&backup) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error),
    }
}

pub(crate) fn write_private(file: &Path, contents: &[u8], preserve_mode: bool) -> io::Result<()> {
    let resolved = match fs::symlink_metadata(file) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(file)?,
        Ok(_) => file.to_owned(),
        Err(error) if error.kind() == ErrorKind::NotFound => file.to_owned(),
        Err(error) => return Err(error),
    };
    fs::create_dir_all(parent(&resolved))?;
    let mut tmp = NamedTempFile::new_in(parent(&resolved))?;
    if preserve_mode {
        tmp.as_file()
            .set_permissions(fs::metadata(&resolved)?.permissions())?;
    }
    tmp.write_all(contents)?;
    tmp.persist(resolved).map_err(|error| error.error)?;
    Ok(())
}

fn parent(file: &Path) -> &Path {
    file.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64() != Some(0.0),
        _ => true,
    }
}

fn clean(text: &str, secrets: &[String]) -> String {
    let mut output = text.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            output = output.replace(secret, "[redacted]");
        }
    }
    output.retain(|ch| !ch.is_control());
    output
}
