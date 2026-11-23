use std::fs;
use std::io::{ErrorKind, IsTerminal};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

const DEFAULT_PORT: u16 = 20128;
const DEFAULT_HOST: &str = "0.0.0.0";
const TOOL_NAMES: &str = "claude, codex, opencode, droid, crush, kilo, cline, pi, omp, all";
const MAIN_HELP: &str = "
Usage: 9router [options]

Options:
  -p, --port <port>   Port to run the server (default: 20128)
  -H, --host <host>   Host to bind (default: 0.0.0.0)
  -n, --no-browser    Don't open browser automatically
  -l, --log           Show server logs (default: hidden)
  -t, --tray          Run in system tray mode (background)
  --skip-update       Skip auto-update check
  -h, --help          Show this help message
  -v, --version       Show version

Commands:
  connect <server-url> Configure Claude Code for a remote 9router server
                      (npx 9router connect http://host:20128 — no install needed)
  show [tool…]        Show each CLI tool's current 9router config and models
                      (e.g. 9router show claude)
  xai video --prompt \"...\" --output video.mp4
                      Generate a Grok Imagine video via the running gateway
                      (see: 9router xai video --help)
";
const SHOW_HELP: &str = "
Usage: 9router show [tool…] [options]

Show the 9router settings each CLI tool on THIS machine is currently using.
Read-only: reads local config files, contacts no server.

  9router show                 # every supported tool
  9router show claude          # one tool
  9router show claude codex    # several (or: --tools claude,codex)

Tools: claude, codex, opencode, droid, crush, kilo, cline, pi, omp, all

Options:
  --tools <list>   Comma-separated tools (same as positional names)
  --json           Machine-readable output (API key still masked)
  -h, --help       Show this help
";
const CONNECT_HELP: &str = "
Usage: 9router connect <server-url> [options]

Configure CLI tools on THIS machine to use a remote 9router server.
No local server is started. Run without installing:

  npx 9router connect http://<server-host>:20128
  npx 9router connect http://<server-host>:20128 --tools claude,codex,opencode

Options:
  --tools <list>         Comma-separated tools to configure (prompted if omitted
                         in a terminal; default: claude). Supported:
                         claude, codex, opencode, droid, crush, kilo, cline, pi, omp, all
  --password <pw>        Dashboard password (or env NINE_ROUTER_PASSWORD;
                         prompted if omitted — preferred, keeps it out of shell history)
  --save                 After a successful login, save the password for this server
                         to ~/.9router/connect.env (plain text, mode 600) so later
                         runs skip the prompt. Delete that file to forget it.
  --key-name <name>      API key name to reuse/create (default: cli-<hostname>)
  --api-key <key>        Use this API key, skip login + key lookup
  --model <model>        Model for all non-Claude tools (default: each tool's own
                         model on the server, else the server's OpenCode model;
                         omp needs none — it discovers every server model)
  --fable|--opus|--sonnet|--haiku <model>
                         Override a Claude Code tier (default: the server's value)
  --print-env            Also print OpenAI-compatible env vars for other CLIs
  --reset                Remove 9router settings from the selected tools and exit
  -h, --help             Show this help

Models come from the server's own CLI-tools config (set on its dashboard).
Nothing is hardcoded: a Claude tier the server leaves unset is left unset,
and a non-Claude tool with no model anywhere is skipped. Flags override:

  npx 9router connect http://<server-host>:20128 --tools claude --sonnet cc/claude-sonnet-5-5
  npx 9router connect http://<server-host>:20128 --tools codex,opencode --model ocg/deepseek-flash
";
const VIDEO_HELP: &str = "
Usage: 9router xai video --prompt \"...\" [options]

Generate a Grok Imagine video via your local 9router gateway
(requires a connected xAI account — Grok Build OAuth or API key).

Options:
  --prompt <text>         Video description (required)
  --output <file>         Output MP4 path (default: video.mp4)
  --model <id>            Model (default: xai/grok-imagine-video)
  --duration <seconds>    Video duration
  --aspect-ratio <ratio>  e.g. 16:9, 9:16, 1:1
  --resolution <res>      480p | 720p | 1080p
  --image <path-or-url>   Image input for image-to-video
  --timeout <seconds>     Max wait for the job (default: 600)
  --port <port>           Gateway port (default: 20128)
  --host <host>           Gateway host (default: 127.0.0.1)
  --api-key <key>         9router API key (or env NINE_ROUTER_API_KEY)
  -h, --help              Show this help
";

#[derive(Default)]
pub struct ServerOptions {
    pub port: Option<u16>,
    pub host: Option<String>,
    pub no_browser: bool,
    pub skip_update: bool,
    pub show_log: bool,
    pub tray: bool,
}

pub enum Invocation {
    Server(ServerOptions),
    Connect(Vec<String>),
    XaiVideo(Vec<String>),
}

pub fn dispatch(args: &[String]) -> Result<Invocation, i32> {
    dispatch_in(
        args,
        std::env::home_dir().as_deref(),
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .as_deref(),
        std::io::stdin().is_terminal(),
    )
}

pub(crate) fn dispatch_in(
    args: &[String],
    home: Option<&Path>,
    xdg: Option<&Path>,
    stdin_tty: bool,
) -> Result<Invocation, i32> {
    match args.first().map(String::as_str) {
        Some("show") => show(&args[1..], home, xdg),
        Some("connect") => connect(&args[1..]),
        Some("xai") if args.get(1).is_some_and(|arg| arg == "video") => video(&args[2..]),
        _ => server(args, stdin_tty),
    }
}

fn server(args: &[String], stdin_tty: bool) -> Result<Invocation, i32> {
    let mut options = ServerOptions::default();
    let mut port = None;
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "--port" | "-p" => port = Some(js_port(args.next())),
            "--host" | "-H" => {
                options.host = Some(
                    args.next()
                        .filter(|host| !host.is_empty())
                        .unwrap_or(DEFAULT_HOST)
                        .to_owned(),
                );
            }
            "--no-browser" | "-n" => options.no_browser = true,
            "--log" | "-l" => options.show_log = true,
            "--skip-update" => options.skip_update = true,
            "--tray" | "-t" => options.tray = true,
            "--help" | "-h" => return help(MAIN_HELP, 0),
            "--version" | "-v" => {
                let manifest: Value =
                    serde_json::from_str(include_str!("../../../cli/package.json"))
                        .map_err(|_| error("Cannot read 9router CLI version"))?;
                let version = manifest["version"]
                    .as_str()
                    .ok_or_else(|| error("Cannot read 9router CLI version"))?;
                println!("{version}");
                return Err(0);
            }
            _ => (),
        }
    }
    options.port = port
        .transpose()
        .map_err(|_| error("Port must be between 1 and 65535"))?;
    options.tray |= options.skip_update && !stdin_tty;
    Ok(Invocation::Server(options))
}

fn js_port(raw: Option<&str>) -> Result<u16, ()> {
    let text = raw.unwrap_or_default().trim_start();
    let (negative, digits) = if let Some(text) = text.strip_prefix('-') {
        (true, text)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    };
    let end = digits
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(digits.len());
    let prefix = digits[..end].trim_start_matches('0');
    if prefix.is_empty() {
        return Ok(DEFAULT_PORT);
    }
    if negative {
        return Err(());
    }
    prefix.parse().map_err(|_| ())
}

fn connect(args: &[String]) -> Result<Invocation, i32> {
    let mut positional = None;
    let mut tools = None;
    let mut reset = false;
    let mut help_requested = false;
    let mut options = args.iter().map(String::as_str);
    while let Some(arg) = options.next() {
        match arg {
            "--password" | "--key-name" | "--api-key" | "--model" | "--fable" | "--opus"
            | "--sonnet" | "--haiku" => {
                options
                    .next()
                    .ok_or_else(|| error(&format!("Missing value for {arg}")))?;
            }
            "--tools" => {
                tools = Some(
                    options
                        .next()
                        .ok_or_else(|| error("Missing value for --tools"))?,
                )
            }
            "--reset" => reset = true,
            "--save" | "--print-env" => (),
            "--help" | "-h" => help_requested = true,
            arg if !arg.starts_with('-') && positional.is_none_or(str::is_empty) => {
                positional = Some(arg)
            }
            _ => return Err(error(&format!("Unknown option: {arg}"))),
        }
    }
    if help_requested {
        return help(CONNECT_HELP, 0);
    }
    if !reset && positional.is_none_or(str::is_empty) {
        return help(CONNECT_HELP, 1);
    }
    if let Some(tools) = tools {
        resolve_tools(tools.split(',')).map_err(|err| error(&err))?;
    }
    if !reset && let Some(raw) = positional {
        let trimmed = raw.trim();
        let lower = trimmed.to_ascii_lowercase();
        let url = if lower.starts_with("http://") || lower.starts_with("https://") {
            url::Url::parse(trimmed)
        } else {
            url::Url::parse(&format!("http://{trimmed}"))
        };
        if url.is_err() {
            return Err(error("Invalid URL"));
        }
    }
    Ok(Invocation::Connect(args.to_vec()))
}

fn video(args: &[String]) -> Result<Invocation, i32> {
    let mut help_requested = false;
    let mut prompt = None;
    let mut options = args.iter().map(String::as_str);
    while let Some(arg) = options.next() {
        match arg {
            "--prompt" => prompt = options.next(),
            "--output" | "-o" | "--model" | "--duration" | "--aspect-ratio" | "--resolution"
            | "--image" | "--timeout" | "--port" | "-p" | "--host" | "-H" | "--api-key"
            | "--poll-interval-ms" => {
                options.next();
            }
            "--help" | "-h" => help_requested = true,
            _ => {
                error(&format!("Unknown option: {arg}"));
                return help(VIDEO_HELP, 1);
            }
        }
    }
    if help_requested {
        return help(VIDEO_HELP, 0);
    }
    if prompt.is_none_or(str::is_empty) {
        error("--prompt is required");
        return help(VIDEO_HELP, 1);
    }
    Ok(Invocation::XaiVideo(args.to_vec()))
}

fn help(text: &str, exit: i32) -> Result<Invocation, i32> {
    println!("{text}");
    Err(exit)
}

fn error(message: &str) -> i32 {
    eprintln!("❌ {message}");
    1
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Tool {
    Claude,
    Codex,
    OpenCode,
    Droid,
    Crush,
    Kilo,
    Cline,
    Pi,
    Omp,
}

impl Tool {
    pub(crate) const ALL: [Self; 9] = [
        Self::Claude,
        Self::Codex,
        Self::OpenCode,
        Self::Droid,
        Self::Crush,
        Self::Kilo,
        Self::Cline,
        Self::Pi,
        Self::Omp,
    ];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Droid => "droid",
            Self::Crush => "crush",
            Self::Kilo => "kilo",
            Self::Cline => "cline",
            Self::Pi => "pi",
            Self::Omp => "omp",
        }
    }

    pub(crate) fn files(self, home: &Path, xdg: Option<&Path>) -> Vec<PathBuf> {
        match self {
            Self::Claude => vec![home.join(".claude/settings.json")],
            Self::Codex => vec![home.join(".codex/config.toml")],
            Self::OpenCode => vec![home.join(".config/opencode/opencode.json")],
            Self::Droid => vec![home.join(".factory/settings.json")],
            Self::Crush => vec![
                xdg.filter(|path| !path.as_os_str().is_empty())
                    .map_or_else(|| home.join(".config"), Path::to_path_buf)
                    .join("crush/crush.json"),
            ],
            Self::Kilo => vec![home.join(".local/share/kilo/auth.json")],
            Self::Cline => vec![
                home.join(".cline/data/globalState.json"),
                home.join(".cline/data/secrets.json"),
            ],
            Self::Pi => {
                let agent = home.join(".pi/agent/models.json");
                let root = home.join(".pi/models.json");
                vec![if !agent.exists() && root.exists() {
                    root
                } else {
                    agent
                }]
            }
            Self::Omp => vec![home.join(".omp/agent/models.yml")],
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "OpenAI Codex CLI",
            Self::OpenCode => "OpenCode",
            Self::Droid => "Factory Droid",
            Self::Crush => "Crush",
            Self::Kilo => "Kilo Code CLI",
            Self::Cline => "Cline CLI",
            Self::Pi => "Pi",
            Self::Omp => "Oh My Pi",
        }
    }
}

pub(crate) fn resolve_tools<'a>(
    requested: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<Tool>, String> {
    let mut active = [false; Tool::ALL.len()];
    for raw in requested {
        let key = raw.trim_matches(js_space).to_lowercase();
        let id = match key.as_str() {
            "" => continue,
            "all" => {
                active.fill(true);
                continue;
            }
            "claude-code" | "claudecode" => "claude",
            "factory" => "droid",
            "kilocode" => "kilo",
            "pi-coding-agent" => "pi",
            "oh-my-pi" => "omp",
            id => id,
        };
        let index = Tool::ALL
            .iter()
            .position(|tool| tool.id() == id)
            .ok_or_else(|| format!("Unknown tool \"{raw}\". Supported: {TOOL_NAMES}"))?;
        active[index] = true;
    }
    Ok(Tool::ALL
        .into_iter()
        .zip(active)
        .filter_map(|(tool, enabled)| enabled.then_some(tool))
        .collect())
}

fn show(args: &[String], home: Option<&Path>, xdg: Option<&Path>) -> Result<Invocation, i32> {
    let mut tools = Vec::new();
    let mut machine_readable = false;
    let mut help_requested = false;
    let mut options = args.iter().map(String::as_str);
    while let Some(arg) = options.next() {
        match arg {
            "--tools" => {
                let requested = options
                    .next()
                    .ok_or_else(|| error("Missing value for --tools"))?;
                tools.extend(requested.split(','));
            }
            "--json" => machine_readable = true,
            "--help" | "-h" => help_requested = true,
            arg if !arg.starts_with('-') => tools.push(arg),
            _ => return Err(error(&format!("Unknown option: {arg}"))),
        }
    }
    if help_requested {
        return help(SHOW_HELP, 0);
    }
    let tools = if tools.is_empty() {
        Tool::ALL.to_vec()
    } else {
        resolve_tools(tools).map_err(|err| error(&err))?
    };
    let home = home.ok_or_else(|| error("Cannot determine home directory"))?;
    let records: Vec<_> = tools
        .into_iter()
        .map(|tool| inspect(tool, home, xdg))
        .collect();
    if machine_readable {
        let rendered =
            serde_json::to_string_pretty(&records).map_err(|err| error(&err.to_string()))?;
        println!("{rendered}");
    } else {
        for (index, record) in records.iter().enumerate() {
            if index != 0 {
                println!();
            }
            print_record(record);
        }
    }
    Err(i32::from(
        records.iter().any(|record| record.get("error").is_some()),
    ))
}

#[derive(Default)]
struct ToolConfig {
    base: Value,
    key: Value,
    models: Map<String, Value>,
    active: Option<bool>,
}

fn inspect(tool: Tool, home: &Path, xdg: Option<&Path>) -> Value {
    let files = tool.files(home, xdg);
    let mut record = json!({ "id": tool.id(), "name": tool.name(), "files": files });
    let config = match read_config(tool.id(), &files) {
        Ok(config) => config,
        Err(error) => {
            record["error"] = json!(error);
            return record;
        }
    };
    let configured = truthy(&config.base) || truthy(&config.key) || !config.models.is_empty();
    let masked = if truthy(&config.key) {
        let Some(key) = config.key.as_str() else {
            record["configured"] = json!(configured);
            record["baseUrl"] = config.base;
            record["error"] = json!("key.slice is not a function");
            return record;
        };
        json!(mask_key(key))
    } else {
        Value::Null
    };
    record["configured"] = json!(configured);
    record["baseUrl"] = if truthy(&config.base) {
        config.base
    } else {
        Value::Null
    };
    record["apiKey"] = masked;
    record["models"] = Value::Object(config.models);
    if let Some(active) = config.active {
        record["active"] = json!(active);
    }
    record
}

fn read_config(id: &str, files: &[PathBuf]) -> Result<ToolConfig, String> {
    let Some(file) = files.first() else {
        return Ok(ToolConfig::default());
    };
    let raw = read(file, id)?;
    if id == "codex" && raw.is_none() {
        return Ok(ToolConfig::default());
    }
    let input = raw.unwrap_or_default();
    let mut config = ToolConfig::default();
    let (base, key) = match id {
        "claude" => {
            let env = &input["env"];
            for (name, field) in [
                ("main", "ANTHROPIC_MODEL"),
                ("fable", "ANTHROPIC_DEFAULT_FABLE_MODEL"),
                ("opus", "ANTHROPIC_DEFAULT_OPUS_MODEL"),
                ("sonnet", "ANTHROPIC_DEFAULT_SONNET_MODEL"),
                ("haiku", "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            ] {
                add_model(&mut config, name, &env[field]);
            }
            (
                env["ANTHROPIC_BASE_URL"].clone(),
                env["ANTHROPIC_AUTH_TOKEN"].clone(),
            )
        }
        "codex" => {
            let provider = &input["model_providers"]["9router"];
            add_model(&mut config, "main", &input["model"]);
            add_model(
                &mut config,
                "subagent",
                &input["agents"]["default_subagent_model"],
            );
            config.active = Some(input["model_provider"] == "9router");
            let auth = provider["http_headers"]["Authorization"]
                .as_str()
                .filter(|auth| !auth.is_empty())
                .map_or(Value::Null, |auth| json!(strip_bearer(auth)));
            (provider["base_url"].clone(), auth)
        }
        "opencode" => {
            let provider = &input["provider"]["9router"];
            add_model(&mut config, "main", &input["model"]);
            add_model(
                &mut config,
                "explorer",
                &input["agent"]["explorer"]["model"],
            );
            if let Some(models) = provider["models"]
                .as_object()
                .filter(|models| !models.is_empty())
            {
                let mut names: Vec<&str> = models.keys().map(String::as_str).collect();
                names.sort_by_key(|name| {
                    name.parse::<u32>()
                        .ok()
                        .filter(|n| n.to_string() == *name)
                        .unwrap_or(u32::MAX)
                });
                config
                    .models
                    .insert("available".into(), json!(names.join(", ")));
            }
            config.active = Some(
                input["model"]
                    .as_str()
                    .is_some_and(|model| model.starts_with("9router/")),
            );
            (
                provider["options"]["baseURL"].clone(),
                provider["options"]["apiKey"].clone(),
            )
        }
        "droid" => {
            let models: Vec<_> = input["customModels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|model| {
                    model["id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("custom:9Router"))
                })
                .collect();
            if let Some(first) = models.first() {
                if let Some(main) = first.get("model") {
                    config.models.insert("main".into(), main.clone());
                }
                if models.len() > 1 {
                    let ids = models
                        .iter()
                        .map(|model| model["model"].as_str().unwrap_or_default())
                        .collect::<Vec<_>>();
                    config
                        .models
                        .insert("available".into(), json!(ids.join(", ")));
                }
                (first["baseUrl"].clone(), first["apiKey"].clone())
            } else {
                (Value::Null, Value::Null)
            }
        }
        "crush" | "pi" => {
            let provider = &input["providers"]["9router"];
            let ids: Vec<_> = provider["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|model| {
                    if id == "pi" && model.is_string() {
                        model.as_str()
                    } else {
                        model["id"].as_str()
                    }
                })
                .filter(|id| !id.is_empty())
                .collect();
            if let Some(first) = ids.first() {
                config.models.insert("main".into(), json!(first));
            }
            if id == "pi" && ids.len() > 1 {
                config
                    .models
                    .insert("available".into(), json!(ids.join(", ")));
            }
            if id == "pi" {
                (provider["baseUrl"].clone(), provider["apiKey"].clone())
            } else {
                (provider["base_url"].clone(), provider["api_key"].clone())
            }
        }
        "kilo" => {
            let auth = &input["openai-compatible"];
            add_model(&mut config, "main", &auth["model"]);
            (auth["baseUrl"].clone(), auth["apiKey"].clone())
        }
        "cline" => {
            add_model(&mut config, "act", &input["openAiModelId"]);
            add_model(&mut config, "plan", &input["planModeOpenAiModelId"]);
            config.active = Some(input["actModeApiProvider"] == "openai");
            let secrets = read(&files[1], id)?.unwrap_or_default();
            (
                input["openAiBaseUrl"].clone(),
                secrets["openAiApiKey"].clone(),
            )
        }
        "omp" => {
            let provider = &input["providers"]["9router"];
            if provider["discovery"]["type"] == "proxy" {
                config
                    .models
                    .insert("main".into(), json!("(all server models, via discovery)"));
            }
            (provider["baseUrl"].clone(), provider["apiKey"].clone())
        }
        _ => (Value::Null, Value::Null),
    };
    config.base = base;
    config.key = key;
    Ok(config)
}

fn add_model(config: &mut ToolConfig, name: &str, value: &Value) {
    if value.as_str().is_some_and(|value| !value.is_empty()) {
        config.models.insert(name.into(), value.clone());
    }
}

fn read(file: &Path, tool: &str) -> Result<Option<Value>, String> {
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot parse {}: {}", file.display(), error.kind())),
    };
    let text = String::from_utf8_lossy(&bytes);
    let parsed = match tool {
        "codex" => toml::from_str::<toml::Value>(&text)
            .ok()
            .and_then(|value| serde_json::to_value(value).ok()),
        "omp" => serde_yaml::from_str(&text).ok(),
        _ => serde_json::from_slice(&without_trailing_commas(text.as_bytes())).ok(),
    };
    parsed.map(Some).ok_or_else(|| {
        let syntax = match tool {
            "codex" => "TOML",
            "omp" => "YAML",
            _ => "JSON",
        };
        format!("Cannot parse {}: invalid {syntax}", file.display())
    })
}

pub(crate) fn without_trailing_commas(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut quoted = false;
    let mut escaped = false;
    for (index, &byte) in input.iter().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b','
            && input[index + 1..]
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|b| matches!(b, b'}' | b']'))
        {
            continue;
        }
        output.push(byte);
    }
    output
}

fn strip_bearer(auth: &str) -> &str {
    if auth
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer"))
        && auth[6..].chars().next().is_some_and(char::is_whitespace)
    {
        auth[6..].trim_start()
    } else {
        auth
    }
}

pub(crate) fn mask_key(key: &str) -> String {
    let units: Vec<_> = key.encode_utf16().collect();
    if units.len() < 12 {
        return "****".to_owned();
    }
    format!(
        "{}…{}",
        String::from_utf16_lossy(&units[..6]),
        String::from_utf16_lossy(&units[units.len() - 4..])
    )
}

fn print_record(record: &Value) {
    let name = record["name"].as_str().unwrap_or_default();
    if let Some(error) = record["error"].as_str() {
        println!("❌ {name}: {error}");
        return;
    }
    let files: Vec<_> = record["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if record["configured"] != true {
        println!(
            "• {name}: not configured for 9router ({})",
            files.first().copied().unwrap_or_default()
        );
        return;
    }
    let inactive = if record["active"] == false {
        "  \x1b[33m(9router present but not the active provider)\x1b[0m"
    } else {
        ""
    };
    println!("✅ {name}{inactive}");
    println!("   File:     {}", files.join(", "));
    println!(
        "   Base URL: {}",
        record["baseUrl"].as_str().unwrap_or("(none)")
    );
    println!(
        "   API key:  {}",
        record["apiKey"].as_str().unwrap_or("(none)")
    );
    let models = record["models"].as_object();
    if let Some(models) = models.filter(|models| !models.is_empty()) {
        let width = models
            .keys()
            .map(|name| name.len())
            .max()
            .unwrap_or_default();
        println!("   Models:");
        for (key, value) in models {
            let rendered = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            println!("     {key:<width$}  {rendered}");
        }
    } else {
        println!("   Models:   (none set — the tool's own default applies)");
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        _ => true,
    }
}

pub(crate) fn js_space(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}
