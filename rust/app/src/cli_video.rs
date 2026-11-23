use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Method, Response};
use serde_json::{Value, json};
use tempfile::Builder;
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

const HOST: &str = "127.0.0.1";
const PORT: i64 = 20128;
const TIMEOUT: i64 = 600;
const POLL: i64 = 5000;
const MODEL: &str = "xai/grok-imagine-video";

pub async fn run(args: &[String]) -> i32 {
    let opts = match Options::parse(args) {
        Ok(opts) => opts,
        Err(error) => {
            eprintln!("❌ {error}");
            return 1;
        }
    };
    let tty = io::stdout().is_terminal();
    let mut task = std::pin::pin!(workflow(&opts, tty));
    let result = tokio::select! {
        result = &mut task => result,
        signal = tokio::signal::ctrl_c() => match signal {
            Ok(()) => {
                eprintln!("\n✋ Cancelled");
                return 130;
            }
            Err(_) => task.await,
        },
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            if tty {
                println!();
            }
            eprintln!("❌ {}", sanitized(&error, opts.api_key.as_deref()));
            1
        }
    }
}

struct Options<'a> {
    prompt: &'a str,
    model: Option<&'a str>,
    output: &'a Path,
    duration: Option<i64>,
    aspect: Option<&'a str>,
    resolution: Option<&'a str>,
    image: Option<&'a str>,
    host: &'a str,
    port: u16,
    timeout: i64,
    poll: i64,
    api_key: Option<String>,
}

impl<'a> Options<'a> {
    fn parse(args: &'a [String]) -> Result<Self, String> {
        let mut args = args.iter().map(String::as_str);
        let mut options = Self {
            prompt: "",
            model: Some(MODEL),
            output: Path::new("video.mp4"),
            duration: None,
            aspect: None,
            resolution: None,
            image: None,
            host: HOST,
            port: PORT as u16,
            timeout: TIMEOUT,
            poll: POLL,
            api_key: std::env::var("NINE_ROUTER_API_KEY")
                .ok()
                .filter(|key| !key.is_empty()),
        };
        while let Some(arg) = args.next() {
            match arg {
                "--prompt" => options.prompt = args.next().unwrap_or_default(),
                "--model" => options.model = args.next(),
                "--output" | "-o" => {
                    options.output = Path::new(args.next().ok_or("Missing value for --output")?)
                }
                "--duration" => options.duration = nonzero_int(args.next()),
                "--aspect-ratio" => options.aspect = next_nonempty(&mut args),
                "--resolution" => options.resolution = next_nonempty(&mut args),
                "--image" => options.image = next_nonempty(&mut args),
                "--host" | "-H" => options.host = next_nonempty(&mut args).unwrap_or(HOST),
                "--port" | "-p" => {
                    let value = nonzero_int(args.next()).unwrap_or(PORT);
                    options.port =
                        u16::try_from(value).map_err(|_| "Port must be between 1 and 65535")?;
                }
                "--timeout" => options.timeout = nonzero_int(args.next()).unwrap_or(TIMEOUT),
                "--poll-interval-ms" => options.poll = nonzero_int(args.next()).unwrap_or(POLL),
                "--api-key" => options.api_key = next_nonempty(&mut args).map(str::to_owned),
                _ => return Err(format!("Unknown option: {arg}")),
            }
        }
        if options.prompt.is_empty() {
            return Err("--prompt is required".into());
        }
        Ok(options)
    }

    fn payload(&self) -> Result<Value, String> {
        let mut payload = json!({});
        if let Some(model) = self.model {
            payload["model"] = json!(model);
        }
        payload["prompt"] = json!(self.prompt);
        if let Some(duration) = self.duration {
            payload["duration"] = json!(duration);
        }
        if let Some(aspect) = self.aspect {
            payload["aspect_ratio"] = json!(aspect);
        }
        if let Some(resolution) = self.resolution {
            payload["resolution"] = json!(resolution);
        }
        if let Some(image) = self.image {
            payload["image"] = json!({ "url": image_url(image)? });
        }
        Ok(payload)
    }
}

fn next_nonempty<'a>(args: &mut impl Iterator<Item = &'a str>) -> Option<&'a str> {
    args.next().filter(|value| !value.is_empty())
}

fn nonzero_int(value: Option<&str>) -> Option<i64> {
    let text = value?.trim_start_matches(js_space);
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    let number: i64 = if end == 0 {
        return None;
    } else {
        text[..end + text.len() - digits.len()].parse().ok()?
    };
    (number != 0).then_some(number)
}

fn image_url(input: &str) -> Result<String, String> {
    let lower = input.to_ascii_lowercase();
    if ["http:", "https:", "data:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return Ok(input.to_owned());
    }
    let bytes =
        std::fs::read(input).map_err(|error| format!("Cannot read image {input}: {error}"))?;
    let extension = Path::new(input)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let mime = if extension.eq_ignore_ascii_case("png") {
        "image/png"
    } else if extension.eq_ignore_ascii_case("webp") {
        "image/webp"
    } else {
        "image/jpeg"
    };
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

async fn workflow(opts: &Options<'_>, tty: bool) -> Result<i32, String> {
    let payload = opts.payload()?;
    let gateway = Gateway::new(opts)?;
    println!(
        "🎬 Requesting video ({})…",
        opts.model.unwrap_or("undefined")
    );
    let create = gateway
        .request(Method::POST, "/v1/videos/generations", Some(&payload), None)
        .await?;
    let request_id = &create.body["request_id"];
    if create.status != 200 || !truthy(request_id) {
        let fallback = if create.raw.is_empty() {
            format!("HTTP {}", create.status)
        } else {
            create.raw
        };
        let reason = detail(&create.body, &fallback);
        let safe = sanitized(&reason, opts.api_key.as_deref());
        eprintln!("❌ Create failed: {}", truncate(&safe, 500));
        let hint_detail = error_value(&create.body).map_or(fallback, js_text);
        if create.status == 400 && hint_detail.to_ascii_lowercase().contains("no credentials") {
            eprintln!("   Connect an xAI account first: dashboard → Providers → xAI (Grok).");
        }
        return Ok(1);
    }

    let request_id = js_text(request_id);
    if matches!(request_id.as_str(), "." | "..") {
        return Err("Invalid video job ID".into());
    }
    let affinity = create
        .headers
        .get("x-9router-connection-id")
        .filter(|value| !value.as_bytes().is_empty());
    println!(
        "📋 Job accepted: {}",
        sanitized(&request_id, opts.api_key.as_deref())
    );
    let result = poll(&gateway, opts, &request_id, affinity, tty).await?;
    if tty {
        println!();
    }
    let raw_url = if truthy(&result["video"]["url"]) {
        &result["video"]["url"]
    } else {
        &result["video"]["file_output"]["public_url"]
    };
    if !truthy(raw_url) {
        eprintln!("❌ Job finished but no video URL was returned");
        return Ok(1);
    }
    let url = raw_url.as_str().ok_or("Invalid video URL")?;
    println!("⬇️  Downloading…");
    download(&gateway.client, url, opts.output).await?;
    println!("✅ Saved {}", opts.output.display());
    Ok(0)
}

struct Gateway<'a> {
    url: Url,
    client: Client,
    api_key: Option<&'a str>,
}

impl<'a> Gateway<'a> {
    fn new(opts: &'a Options<'_>) -> Result<Self, String> {
        let mut url = Url::parse("http://127.0.0.1").map_err(|error| error.to_string())?;
        if let Ok(ip) = opts.host.parse::<std::net::IpAddr>() {
            url.set_ip_host(ip).map_err(|_| "Invalid gateway host")?;
        } else {
            url.set_host(Some(opts.host))
                .map_err(|_| "Invalid gateway host".to_owned())?;
        }
        url.set_port(Some(opts.port))
            .map_err(|_| "Invalid gateway port")?;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            url,
            client,
            api_key: opts.api_key.as_deref(),
        })
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        affinity: Option<&HeaderValue>,
    ) -> Result<GatewayResponse, String> {
        let url = self.url.join(path).map_err(|error| error.to_string())?;
        let mut request = self
            .client
            .request(method, url)
            .header("accept", "application/json");
        if let Some(api_key) = self.api_key {
            request = request.bearer_auth(api_key);
        }
        if let Some(affinity) = affinity {
            request = request.header("x-connection-id", affinity.clone());
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| self.transport_error(error))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| self.transport_error(error))?;
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        Ok(GatewayResponse {
            status,
            headers,
            body,
            raw,
        })
    }

    fn transport_error(&self, error: reqwest::Error) -> String {
        let mut cause = std::error::Error::source(&error);
        while let Some(next) = cause {
            if let Some(io_error) = next.downcast_ref::<io::Error>()
                && io_error.kind() == io::ErrorKind::ConnectionRefused
            {
                let host = self.url.host_str().unwrap_or(HOST);
                let port = self.url.port_or_known_default().unwrap_or(PORT as u16);
                return format!("connect ECONNREFUSED {host}:{port}");
            }
            cause = next.source();
        }
        reqwest_error(error)
    }
}

struct GatewayResponse {
    status: u16,
    headers: HeaderMap,
    body: Value,
    raw: String,
}

async fn poll(
    gateway: &Gateway<'_>,
    opts: &Options<'_>,
    id: &str,
    affinity: Option<&HeaderValue>,
    tty: bool,
) -> Result<Value, String> {
    let deadline = if opts.timeout < 0 {
        None
    } else {
        Instant::now().checked_add(Duration::from_secs(opts.timeout as u64))
    };
    let delay = if (1..=i32::MAX as i64).contains(&opts.poll) {
        opts.poll as u64
    } else {
        1
    };
    let path = format!("/v1/videos/{}", component(id));
    let mut last = String::new();
    let timed_out = || {
        format!(
            "Timed out after {}s waiting for video job {id}",
            opts.timeout
        )
    };
    loop {
        if opts.timeout < 0 || deadline.is_some_and(|deadline| Instant::now() > deadline) {
            return Err(timed_out());
        }
        let request = gateway.request(Method::GET, &path, None, affinity);
        let response = match deadline {
            Some(deadline) => timeout_at(deadline, request)
                .await
                .map_err(|_| timed_out())??,
            None => request.await?,
        };
        if response.status == 200 && truthy(&response.body) {
            let status = &response.body["status"];
            let status = if truthy(status) {
                js_text(status).to_lowercase()
            } else {
                "".to_owned()
            };
            let display = if status.is_empty() {
                "pending"
            } else {
                &status
            };
            let display = sanitized(display, opts.api_key.as_deref());
            let progress = &response.body["progress"];
            let line = if progress.as_f64().is_some_and(f64::is_finite) {
                format!("⏳ {display} {}%", js_text(progress))
            } else {
                format!("⏳ {display}")
            };
            if line != last {
                print_progress(&line, tty);
                last = line;
            }
            match status.as_str() {
                "failed" | "error" | "expired" | "cancelled" => {
                    let reason = detail(&response.body, "video generation failed");
                    return Err(format!("Job {id} failed: {reason}"));
                }
                "done" | "completed" => return Ok(response.body),
                _ => (),
            }
        } else if response.status >= 400 && !matches!(response.status, 429 | 503) {
            return Err(format!(
                "Polling failed (HTTP {}): {}",
                response.status,
                truncate(&response.raw, 300)
            ));
        }
        let wait = sleep(Duration::from_millis(delay));
        match deadline {
            Some(deadline) => timeout_at(deadline, wait).await.map_err(|_| timed_out())?,
            None => wait.await,
        }
    }
}

fn print_progress(line: &str, tty: bool) {
    if tty {
        print!("\r\x1b[K{line}");
        let _ = io::stdout().flush();
    } else {
        println!("{line}");
    }
}

async fn download(client: &Client, url: &str, output: &Path) -> Result<(), String> {
    let mut url = Url::parse(url).map_err(|_| "Invalid video URL")?;
    let mut redirects = 5;
    let mut response: Response = loop {
        if !matches!(url.scheme(), "http" | "https") {
            return Err("Invalid video URL".into());
        }
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(reqwest_error)?;
        if response.status().is_redirection()
            && redirects > 0
            && let Some(location) = response.headers().get("location")
            && !location.as_bytes().is_empty()
        {
            let location = location.to_str().map_err(|_| "Invalid video redirect")?;
            url = url.join(location).map_err(|_| "Invalid video redirect")?;
            redirects -= 1;
            continue;
        }
        if response.status().as_u16() != 200 {
            return Err(format!(
                "Download failed: HTTP {}",
                response.status().as_u16()
            ));
        }
        break response;
    };
    let directory = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = Builder::new()
        .prefix(".9router-video-")
        .suffix(".part")
        .tempfile_in(directory)
        .map_err(|error| format!("Cannot initialize video output: {error}"))?;
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        if error.is_body() || error.is_decode() {
            "aborted".to_owned()
        } else {
            reqwest_error(error)
        }
    })? {
        temporary
            .write_all(&chunk)
            .map_err(|error| error.to_string())?;
    }
    temporary.flush().map_err(|error| error.to_string())?;
    temporary
        .persist(output)
        .map_err(|error| format!("Cannot save video: {}", error.error))?;
    Ok(())
}

fn reqwest_error(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

fn error_value(json: &Value) -> Option<&Value> {
    [&json["error"]["message"], &json["error"]]
        .into_iter()
        .find(|value| truthy(value))
}

fn detail(json: &Value, fallback: &str) -> String {
    error_value(json)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_else(|| fallback.to_owned())
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64() != Some(0.0),
        _ => true,
    }
}

fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(_)
            if let Some(number) = value.as_f64()
                && number.fract() == 0.0
                && number.abs() < 1e21 =>
        {
            format!("{number:.0}")
        }
        Value::Object(_) => "[object Object]".to_owned(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    js_text(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => value.to_string(),
    }
}

pub(crate) fn component(text: &str) -> String {
    let mut output = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            output.push(char::from(byte));
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn truncate(text: &str, units: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(units).collect::<Vec<_>>())
}

fn js_space(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

pub(crate) fn redact_bearer(text: &str) -> String {
    let mut output = String::new();
    let mut tail = text;
    while let Some(ch) = tail.chars().next() {
        if tail
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer"))
        {
            let value = tail[6..].trim_start_matches(js_space);
            let offset = tail.len() - value.len();
            let count = value
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || b"._~+/=-".contains(byte))
                .count();
            if offset > 6 && count >= 8 {
                output.push_str("Bearer [redacted]");
                tail = &value[count..];
                continue;
            }
        }
        output.push(ch);
        tail = &tail[ch.len_utf8()..];
    }
    output
}

fn sanitized(text: &str, key: Option<&str>) -> String {
    let mut output = redact_bearer(text);
    output.retain(|ch| !ch.is_control() || ch == '\n' || ch == '\t');
    if let Some(key) = key.filter(|key| !key.is_empty()) {
        return output.replace(key, "[redacted]");
    }
    output
}
