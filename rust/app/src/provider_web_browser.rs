use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

pub(super) struct Config {
    pub(super) login: &'static str,
    pub(super) domain: &'static str,
    required: &'static [&'static str],
    pub(super) local_storage: bool,
}

pub(super) fn config(provider: &str) -> Option<Config> {
    let (login, domain, required, local_storage): (_, _, &[_], _) = match provider {
        "grok-web" => ("https://grok.com/", "grok.com", &["sso"], false),
        "chatgpt-web" => (
            "https://chatgpt.com/",
            "chatgpt.com",
            &["__Secure-next-auth.session-token"],
            false,
        ),
        "gemini-web" => (
            "https://gemini.google.com/",
            "google.com",
            &["__Secure-1PSID"],
            false,
        ),
        "muse-web" => ("https://muse.ai/", "muse.ai", &["hatch_sess"], false),
        "deepseek-web" => (
            "https://chat.deepseek.com/sign_in",
            "deepseek.com",
            &[],
            true,
        ),
        "uc-web" => (
            "https://uncensored.com/",
            "uncensored.com",
            &["__client", "__client_uat"],
            false,
        ),
        _ => return None,
    };
    Some(Config {
        login,
        domain,
        required,
        local_storage,
    })
}

pub(super) enum Outcome {
    Captured(String),
    Cancelled,
    TimedOut,
    Failed(&'static str),
}

pub(super) async fn authenticate(
    profile: &Path,
    config: &Config,
    mut cancel: watch::Receiver<bool>,
) -> Outcome {
    let Some(executable) = executable() else {
        return Outcome::Failed(
            "No Chrome/Chromium executable found — set CHROME_PATH or install Google Chrome",
        );
    };
    if !create_profile(profile).await {
        return Outcome::Failed("Could not open the browser profile directory");
    }
    let before_launch = SystemTime::now();
    let mut data_dir = std::ffi::OsString::from("--user-data-dir=");
    data_dir.push(profile.as_os_str());
    let child = Command::new(executable)
        .arg(data_dir)
        .args([
            "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
        ])
        .arg(config.login)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = child else {
        return Outcome::Failed("Could not start Chrome/Chromium");
    };

    let result = tokio::select! {
        _ = cancel.wait_for(|cancelled| *cancelled) => Outcome::Cancelled,
        result = timeout(Duration::from_secs(300), monitor(&mut child, profile, before_launch, config)) => {
            match result {
                Ok(Ok(credential)) => Outcome::Captured(credential),
                Ok(Err(error)) => Outcome::Failed(error),
                Err(_) => Outcome::TimedOut,
            }
        },
    };
    if child.try_wait().is_ok_and(|status| status.is_none()) {
        if let Ok(mut session) = connect(profile, before_launch).await {
            let _ = timeout(
                Duration::from_secs(2),
                session.send("Browser.close", json!({}), None),
            )
            .await;
        }
        if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    }
    result
}

async fn create_profile(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if tokio::fs::create_dir_all(parent).await.is_err()
        || !tokio::fs::symlink_metadata(parent)
            .await
            .is_ok_and(|metadata| metadata.is_dir())
    {
        return false;
    }
    let mut builder = tokio::fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    match builder.create(path).await {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            tokio::fs::symlink_metadata(path)
                .await
                .is_ok_and(|metadata| metadata.is_dir())
        }
        Err(_) => false,
    }
}

fn executable() -> Option<PathBuf> {
    let directions = ["NINEROUTER_BROWSER_BIN", "CHROME_PATH"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from);
    let system = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ]
    .into_iter()
    .map(PathBuf::from);
    let windows = ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|directory| {
            PathBuf::from(directory)
                .join("Google")
                .join("Chrome")
                .join("Application")
                .join("chrome.exe")
        });
    directions
        .chain(system)
        .chain(windows)
        .find(|candidate| candidate.is_file())
}

async fn monitor(
    child: &mut Child,
    profile: &Path,
    launched: SystemTime,
    config: &Config,
) -> Result<String, &'static str> {
    let startup = Instant::now() + Duration::from_secs(20);
    let mut session = loop {
        if child.try_wait().map_or(true, |status| status.is_some()) {
            return Err("Chrome/Chromium closed before sign-in was complete");
        }
        if let Ok(session) = connect(profile, launched).await {
            break session;
        }
        if Instant::now() >= startup {
            return Err("Chrome/Chromium did not open the sign-in session");
        }
        sleep(Duration::from_millis(150)).await;
    };
    loop {
        sleep(Duration::from_millis(1500)).await;
        if let Some(credential) = session.capture(config).await? {
            return Ok(credential);
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) type MusePage = ManagedPage;

const CHATGPT_ORIGIN: &str = "https://chatgpt.com";
const CHATGPT_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) struct ManagedPage {
    devtools: DevTools,
    page: String,
    origin: &'static str,
    _child: Child,
    _profile: tempfile::TempDir,
}

impl ManagedPage {
    pub(crate) async fn open(cookies: &[(String, String)]) -> Result<Self, &'static str> {
        Self::open_provider(
            cookies,
            "https://muse.ai",
            "https://muse.ai/thread/new",
            "9router-muse-",
        )
        .await
    }

    pub(crate) async fn open_chatgpt(cookies: &[(String, String)]) -> Result<Self, &'static str> {
        Self::open_provider(
            cookies,
            CHATGPT_ORIGIN,
            "https://chatgpt.com/?temporary-chat=true",
            "9router-chatgpt-",
        )
        .await
    }

    pub(crate) async fn open_gemini(cookies: &[(String, String)]) -> Result<Self, &'static str> {
        Self::open_provider(
            cookies,
            "https://gemini.google.com",
            "https://gemini.google.com/app",
            "9router-gemini-",
        )
        .await
    }

    pub(crate) async fn submit_gemini(&mut self, prompt: &str) -> Result<(), &'static str> {
        if self.origin != "https://gemini.google.com" {
            return Err("Invalid Gemini Web page");
        }
        let composer = r#"(() => {
            const node = [...document.querySelectorAll('.ql-editor, [contenteditable="true"]')].find(e => e.getClientRects().length);
            if (!node || document.querySelector('[aria-modal="true"]')) return false;
            node.focus();
            return node === document.activeElement;
        })()"#;
        if self.evaluate(composer).await? != true {
            return Err("Gemini Web composer is unavailable");
        }
        self.devtools
            .send("Input.insertText", json!({"text":prompt}), Some(&self.page))
            .await?;
        if self.evaluate(composer).await? != true {
            return Err("Gemini Web composer changed before submission");
        }
        for event in ["keyDown", "keyUp"] {
            self.devtools
                .send(
                    "Input.dispatchKeyEvent",
                    json!({"type":event,"key":"Enter","code":"Enter","windowsVirtualKeyCode":13}),
                    Some(&self.page),
                )
                .await?;
        }
        Ok(())
    }

    async fn open_provider(
        cookies: &[(String, String)],
        origin: &'static str,
        start: &'static str,
        prefix: &str,
    ) -> Result<Self, &'static str> {
        let executable = executable().ok_or("No Chrome/Chromium executable found")?;
        let profile = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .map_err(|_| "Could not create a managed browser profile")?;
        let mut data_dir = std::ffi::OsString::from("--user-data-dir=");
        data_dir.push(profile.path().as_os_str());
        let started = SystemTime::now();
        let mut child = Command::new(executable)
            .arg(data_dir)
            .args([
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--headless=new",
                "--no-proxy-server",
                "about:blank",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "Could not start the managed browser")?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut devtools = loop {
            if child.try_wait().map_or(true, |status| status.is_some()) {
                return Err("Browser closed before opening the page");
            }
            if let Ok(devtools) = connect(profile.path(), started).await {
                break devtools;
            }
            if Instant::now() >= deadline {
                return Err("Browser did not open a DevTools session");
            }
            sleep(Duration::from_millis(150)).await;
        };
        let target = devtools
            .send("Target.createTarget", json!({"url":"about:blank"}), None)
            .await?;
        let target_id = target["targetId"]
            .as_str()
            .ok_or("Managed browser has no page")?;
        let page = devtools
            .send(
                "Target.attachToTarget",
                json!({"targetId":target_id,"flatten":true}),
                None,
            )
            .await?;
        let page = page["sessionId"]
            .as_str()
            .ok_or("Could not attach the managed browser page")?
            .to_owned();
        let origin_url = format!("{origin}/");
        let cookies: Vec<_> = cookies
            .iter()
            .map(|(name, value)| {
                json!({"name":name,"value":value,"url":origin_url,"secure":true,"sameSite":"Lax",
                   "httpOnly":name.starts_with("__Secure-") || name.starts_with("__Host-")})
            })
            .collect();
        devtools
            .send(
                "Network.setCookies",
                json!({"cookies":cookies}),
                Some(&page),
            )
            .await?;
        let navigation = devtools
            .send("Page.navigate", json!({"url":start}), Some(&page))
            .await?;
        if navigation["errorText"].is_string() {
            return Err("Could not open the provider page");
        }
        Ok(Self {
            devtools,
            page,
            origin,
            _child: child,
            _profile: profile,
        })
    }

    pub(crate) async fn evaluate(&mut self, expression: &str) -> Result<Value, &'static str> {
        self.evaluate_with_timeout(expression, Duration::from_secs(6))
            .await
    }

    pub(crate) async fn evaluate_with_timeout(
        &mut self,
        expression: &str,
        limit: Duration,
    ) -> Result<Value, &'static str> {
        let expression = format!(
            "(() => location.origin === '{}' ? ({expression}) : null)()",
            self.origin
        );
        let result = self
            .devtools
            .send_with_timeout(
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
                Some(&self.page),
                limit,
            )
            .await?;
        if result.get("exceptionDetails").is_some() {
            return Err("Browser page operation failed");
        }
        result["result"]
            .get("value")
            .cloned()
            .ok_or("Invalid browser page result")
    }

    pub(crate) async fn capture_chatgpt_image(
        &mut self,
        clip: &Value,
    ) -> Result<String, &'static str> {
        if self.origin != CHATGPT_ORIGIN
            || ["x", "y"]
                .iter()
                .any(|key| !clip[key].as_f64().is_some_and(|value| value >= 0.0))
            || ["width", "height"].iter().any(|key| {
                !clip[key]
                    .as_f64()
                    .is_some_and(|value| (128.0..=4096.0).contains(&value))
            })
            || clip["scale"] != 1
        {
            return Err("Invalid ChatGPT Web image bounds");
        }
        let result = self
            .devtools
            .send(
                "Page.captureScreenshot",
                json!({"format":"png","clip":clip,"captureBeyondViewport":true}),
                Some(&self.page),
            )
            .await?;
        result["data"]
            .as_str()
            .map(str::to_owned)
            .ok_or("Browser image capture did not return a PNG")
    }

    pub(crate) async fn attach_chatgpt_files(
        &mut self,
        guard: &str,
        attachments: &[crate::chatgpt_web::StagedAttachment],
    ) -> Result<(), &'static str> {
        const ACCEPTS: &str = r#"((accept, name, mime) => {
            const tokens = accept.toLowerCase().split(',').map(token => token.trim()).filter(Boolean);
            name = name.toLowerCase();
            mime = (mime || '').toLowerCase();
            return !tokens.length || tokens.some(token => {
                if (token.startsWith('.')) return name.endsWith(token);
                return mime && (token === mime
                    || (['image/*','audio/*','video/*'].includes(token) && mime.startsWith(token.slice(0, -1))));
            });
        })"#;
        if self.origin != CHATGPT_ORIGIN || attachments.is_empty() {
            return Err("Attachment selection is not available");
        }
        let selected: Vec<_> = attachments
            .iter()
            .map(|attachment| {
                let name = attachment.path.file_name()?.to_str()?;
                Some(json!({"name":name,"size":attachment.size,"mime":attachment.mime}))
            })
            .collect::<Option<_>>()
            .ok_or("Attachment filename was invalid")?;
        let paths: Vec<_> = attachments
            .iter()
            .map(|attachment| attachment.path.to_str())
            .collect::<Option<_>>()
            .ok_or("Attachment path was invalid")?;
        let selected = json!(selected);
        let expression = format!(
            r#"(() => {{
            if (location.origin !== '{CHATGPT_ORIGIN}' || !({guard})) return null;
            const form = document.querySelector('#prompt-textarea[contenteditable="true"]')?.closest('form');
            if (!form) return null;
            const selected = {selected}, accepts = {ACCEPTS};
            const visible = e => e.getClientRects().length && getComputedStyle(e).visibility === 'visible';
            const files = Array.from(form.querySelectorAll('input[type="file"]')).filter(input => {{
                if (input.form !== form || input.matches(':disabled')
                    || input.closest('[inert],[aria-disabled="true"],[role="dialog"]')
                    || input.files.length || (selected.length > 1 && !input.multiple)) return false;
                if (!visible(input) && !Array.from(input.labels || []).some(label => visible(label)
                    && !label.closest('[inert],[aria-disabled="true"],[aria-hidden="true"],[role="dialog"]')))
                    return false;
                return selected.every(file => accepts(input.accept, file.name, file.mime));
            }});
            return files.length === 1 ? files[0] : null;
            }})()"#
        );
        let element = self
            .devtools
            .send(
                "Runtime.evaluate",
                json!({"expression":expression}),
                Some(&self.page),
            )
            .await?;
        let object_id = element["result"]["objectId"]
            .as_str()
            .filter(|_| element.get("exceptionDetails").is_none())
            .ok_or("ChatGPT Web did not offer an eligible attachment input")?;
        self.devtools
            .send(
                "DOM.setFileInputFiles",
                json!({"files":paths,"objectId":object_id}),
                Some(&self.page),
            )
            .await?;
        let function = format!(
            r#"function () {{
            if (location.origin !== '{CHATGPT_ORIGIN}' || !({guard}) || !this.isConnected
                || this.ownerDocument !== document || this.type !== 'file') return false;
            const selected = {selected}, accepts = {ACCEPTS};
            return this.files.length === selected.length && Array.from(this.files).every((file, index) =>
                file.name === selected[index].name && file.size === selected[index].size
                && accepts(this.accept, file.name, file.type));
            }}"#
        );
        let selected = self
            .devtools
            .send(
                "Runtime.callFunctionOn",
                json!({"functionDeclaration":function,"objectId":object_id,"returnByValue":true}),
                Some(&self.page),
            )
            .await?;
        (selected.get("exceptionDetails").is_none() && selected["result"]["value"] == true)
            .then_some(())
            .ok_or("ChatGPT Web did not confirm the selected attachments")
    }

    pub(crate) async fn observe_conversation(&mut self) -> Result<(), &'static str> {
        if self.origin != CHATGPT_ORIGIN {
            return Err("Conversation capture is not available for this provider");
        }
        self.devtools.send("Network.enable",json!({
            "maxTotalBufferSize":CHATGPT_RESPONSE_LIMIT*2,"maxResourceBufferSize":CHATGPT_RESPONSE_LIMIT,
        }), Some(&self.page)).await?;
        self.devtools.conversation = Some(Conversation {
            session: self.page.clone(),
            request: None,
            response: None,
            done: false,
            failed: false,
        });
        Ok(())
    }

    pub(crate) async fn conversation_response(
        &mut self,
    ) -> Result<(u16, Option<(String, bool)>), &'static str> {
        let (id, status, mime) = timeout(Duration::from_secs(180), async {
            loop {
                let observed = self
                    .devtools
                    .conversation
                    .as_ref()
                    .ok_or("Conversation capture is missing")?;
                if observed.failed {
                    return Err("ChatGPT Web conversation was interrupted");
                }
                if let Some((status, mime)) = &observed.response
                    && (*status != 200 || observed.done)
                {
                    let id = observed
                        .request
                        .as_ref()
                        .ok_or("ChatGPT Web conversation request was not identified")?;
                    break Ok((id.clone(), *status, mime.clone()));
                }
                if observed.done {
                    return Err("ChatGPT Web conversation response was not identified");
                }
                self.devtools.read_message().await?;
            }
        })
        .await
        .map_err(|_| "ChatGPT Web conversation timed out")??;
        if status != 200 {
            return Ok((status, None));
        }
        if mime != "text/event-stream" {
            return Err("ChatGPT Web conversation did not return an event stream");
        }
        let body = self
            .devtools
            .send(
                "Network.getResponseBody",
                json!({"requestId":id}),
                Some(&self.page),
            )
            .await?;
        let encoded = body["base64Encoded"] == true;
        let limit = if encoded {
            CHATGPT_RESPONSE_LIMIT.div_ceil(3) * 4
        } else {
            CHATGPT_RESPONSE_LIMIT
        };
        let text = body["body"]
            .as_str()
            .filter(|text| !text.is_empty() && text.len() <= limit)
            .ok_or("ChatGPT Web conversation body was missing or too large")?;
        Ok((200, Some((text.to_owned(), encoded))))
    }
}

async fn connect(profile: &Path, launched: SystemTime) -> Result<DevTools, ()> {
    let path = profile.join("DevToolsActivePort");
    let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|_| ())?;
    if !metadata.is_file()
        || metadata.len() > 512
        || metadata.modified().map_err(|_| ())? < launched
    {
        return Err(());
    }
    let content = tokio::fs::read_to_string(path).await.map_err(|_| ())?;
    let mut lines = content.lines();
    let port = lines.next().ok_or(())?.parse::<u16>().map_err(|_| ())?;
    let path = lines.next().ok_or(())?;
    let id = path.strip_prefix("/devtools/browser/").ok_or(())?;
    if port == 0
        || id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(());
    }
    let (socket, _) = timeout(
        Duration::from_secs(2),
        connect_async(format!("ws://127.0.0.1:{port}{path}")),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    Ok(DevTools {
        socket,
        command_id: 0,
        conversation: None,
    })
}

struct DevTools {
    socket: Socket,
    command_id: u64,
    conversation: Option<Conversation>,
}

struct Conversation {
    session: String,
    request: Option<String>,
    response: Option<(u16, String)>,
    done: bool,
    failed: bool,
}

impl Conversation {
    fn record(&mut self, event: &Value) {
        if event["sessionId"] != self.session {
            return;
        }
        let params = &event["params"];
        let Some(request) = params["requestId"].as_str().filter(|id| !id.is_empty()) else {
            return;
        };
        let method = event["method"].as_str().unwrap_or_default();
        if method == "Network.requestWillBeSent"
            && self.request.is_none()
            && params["request"]["method"] == "POST"
            && params["request"]["url"]
                .as_str()
                .is_some_and(conversation_url)
        {
            self.request = Some(request.to_owned());
        } else if self.request.as_deref() == Some(request) {
            match method {
                "Network.responseReceived" => {
                    let response = &params["response"];
                    if !response["url"].as_str().is_some_and(conversation_url) {
                        self.failed = true;
                        return;
                    }
                    self.response = response["status"]
                        .as_u64()
                        .and_then(|code| u16::try_from(code).ok())
                        .map(|code| {
                            (
                                code,
                                response["mimeType"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .split(';')
                                    .next()
                                    .unwrap_or_default()
                                    .to_owned(),
                            )
                        });
                }
                "Network.loadingFinished" => self.done = true,
                "Network.loadingFailed" => self.failed = true,
                "Network.requestWillBeSent" if params.get("redirectResponse").is_some() => {
                    self.failed = true
                }
                _ => (),
            }
        }
    }
}

fn conversation_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("chatgpt.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(
                url.path(),
                "/backend-api/f/conversation" | "/backend-api/conversation"
            )
    })
}

impl DevTools {
    async fn send(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, &'static str> {
        self.send_with_timeout(method, params, session, Duration::from_secs(6))
            .await
    }

    async fn send_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
        limit: Duration,
    ) -> Result<Value, &'static str> {
        self.command_id += 1;
        let id = self.command_id;
        let mut command = json!({"id":id, "method":method, "params":params});
        if let Some(session) = session {
            command["sessionId"] = json!(session);
        }
        timeout(limit, async {
            self.socket
                .send(Message::Text(command.to_string().into()))
                .await
                .map_err(|_| "Browser session disconnected")?;
            loop {
                let response = self.read_message().await?;
                if response["id"] == id {
                    return response
                        .get("result")
                        .cloned()
                        .ok_or("Browser session command failed");
                }
            }
        })
        .await
        .map_err(|_| "Browser session command timed out")?
    }

    async fn read_message(&mut self) -> Result<Value, &'static str> {
        while let Some(message) = self.socket.next().await {
            match message.map_err(|_| "Browser session disconnected")? {
                Message::Text(text) => {
                    let response = serde_json::from_str(text.as_str())
                        .map_err(|_| "Invalid browser session response")?;
                    if let Some(conversation) = &mut self.conversation {
                        conversation.record(&response);
                    }
                    return Ok(response);
                }
                Message::Close(_) => return Err("Browser session disconnected"),
                _ => (),
            }
        }
        Err("Browser session disconnected")
    }

    async fn capture(&mut self, config: &Config) -> Result<Option<String>, &'static str> {
        if config.local_storage {
            return self.user_token().await;
        }
        let response = self.send("Storage.getCookies", json!({}), None).await?;
        let mut cookies: Vec<_> = response["cookies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|cookie| {
                let domain = cookie["domain"].as_str()?.trim_start_matches('.');
                let name = cookie["name"].as_str()?;
                let value = cookie["value"].as_str()?;
                (belongs_to(domain, config.domain) && !value.is_empty()).then_some((name, value))
            })
            .collect();
        let values: HashMap<_, _> = cookies.iter().copied().collect();
        if !config.required.iter().all(|name| {
            let direct = values
                .get(name)
                .or_else(|| values.get(format!("{name}.0").as_str()));
            direct.is_some_and(|value| {
                *name != "__client_uat" || value.parse::<f64>().is_ok_and(|time| time > 0.0)
            })
        }) {
            return Ok(None);
        }
        cookies.sort_by(|left, right| {
            left.0
                .to_ascii_lowercase()
                .cmp(&right.0.to_ascii_lowercase())
                .then(right.0.cmp(left.0))
        });
        let credential = cookies
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        Ok(Some(credential))
    }

    async fn user_token(&mut self) -> Result<Option<String>, &'static str> {
        let targets = self.send("Target.getTargets", json!({}), None).await?;
        for target in targets["targetInfos"].as_array().into_iter().flatten() {
            if target["type"] != "page"
                || !target["url"]
                    .as_str()
                    .and_then(|url| url::Url::parse(url).ok())
                    .is_some_and(|url| {
                        url.scheme() == "https"
                            && url.host_str() == Some("chat.deepseek.com")
                            && url.port().is_none()
                    })
            {
                continue;
            }
            let Some(target) = target["targetId"].as_str() else {
                continue;
            };
            let attached = self
                .send(
                    "Target.attachToTarget",
                    json!({"targetId":target,"flatten":true}),
                    None,
                )
                .await?;
            let Some(session) = attached["sessionId"].as_str() else {
                continue;
            };
            let observed = self.send("Runtime.evaluate", json!({
                "expression":"location.origin === 'https://chat.deepseek.com' ? localStorage.getItem('userToken') : null",
                "returnByValue":true,
            }), Some(session)).await;
            let _ = self
                .send(
                    "Target.detachFromTarget",
                    json!({"sessionId":session}),
                    None,
                )
                .await;
            if let Ok(result) = observed
                && let Some(raw) = result["result"]["value"].as_str()
            {
                let token = serde_json::from_str::<Value>(raw)
                    .ok()
                    .and_then(|data| data["value"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| raw.to_owned());
                if token.encode_utf16().count() >= 20 && !token.trim().is_empty() {
                    return Ok(Some(token));
                }
            }
        }
        Ok(None)
    }
}

fn belongs_to(host: &str, domain: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}
