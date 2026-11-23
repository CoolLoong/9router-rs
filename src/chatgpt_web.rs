use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::{StatusCode, header};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Response, ResponseBuilderExt, Url};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::time::{Instant, sleep};

use crate::chat_input::js_space;
use crate::chatgpt_web_delta::parse_direct_conversation;
use crate::provider_connections::by_id;
use crate::provider_http::Delivery;
use crate::provider_index::js_trim;
use crate::providers::web_login::ManagedPage;
use crate::storage::Storage;

#[path = "chatgpt_web_tools.rs"]
mod tools;

use tools::ClientTools;
pub(crate) use tools::{envelope as parse_tool_object, signed_dsml};

pub(super) const ORIGIN: &str = "https://chatgpt.com";
const SESSION_COOKIE: &str = "__Secure-next-auth.session-token";
const MAX_PROMPT: usize = 4 * 1024 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 10;
const MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_IMAGE_EDGE: u32 = 11264;
const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const SWITCHER: &str = r#"button[data-testid="model-switcher-dropdown-button"]"#;

const COMPOSER_READY: &str = "state.auth && state.ready && state.temporary && !state.dialog";
const PAGE_STATE: &str = r#"(() => {
  const visible = e => !!e && e.getClientRects().length > 0;
  const model = document.querySelector('button[data-testid="model-switcher-dropdown-button"]');
  const profile = document.querySelector('[data-testid="profile-button"]');
  const composer = document.querySelector('#prompt-textarea[contenteditable="true"]');
  const dialog = Array.from(document.querySelectorAll('[role="dialog"][aria-modal="true"]')).some(visible);
  const login = document.querySelector('[data-testid="login-button"]');
  const temporary = new URLSearchParams(location.search).get('temporary-chat') === 'true';
  return {auth:visible(profile),login:visible(login),ready:visible(composer),temporary,dialog,
          model:visible(model) ? (model.innerText || '').trim() || (model.getAttribute('aria-label') || '').trim() : ''};
})()"#;

const READY_IMAGE: &str = r#"(() => {
  const visible = e => !!e && e.getClientRects().length > 0 && getComputedStyle(e).visibility === 'visible';
  if (!visible(document.querySelector('[data-testid="profile-button"]'))
      || visible(document.querySelector('button[data-testid="stop-button"]'))
      || Array.from(document.querySelectorAll('[role="dialog"][aria-modal="true"]')).some(visible))
    return null;
  const reply = Array.from(document.querySelectorAll('[data-message-author-role="assistant"]')).filter(visible).at(-1);
  if (!reply) return null;
  const image = Array.from(reply.querySelectorAll('img')).find(img =>
    visible(img) && img.complete && img.naturalWidth >= 128 && img.naturalHeight >= 128
    && !img.closest('a[href], [aria-hidden="true"]')
    && img.getBoundingClientRect().width >= 128 && img.getBoundingClientRect().height >= 128);
  if (!image) return null;
  let png = null;
  if (image.naturalWidth <= 11264 && image.naturalHeight <= 11264
      && image.naturalWidth * image.naturalHeight <= 40000000) {
    const canvas = document.createElement('canvas');
    canvas.width = image.naturalWidth;
    canvas.height = image.naturalHeight;
    try {
      canvas.getContext('2d').drawImage(image, 0, 0);
      const data = canvas.toDataURL('image/png');
      if (data.startsWith('data:image/png;base64,') && data.length < 16 * 1024 * 1024 - 1024)
        png = data.slice('data:image/png;base64,'.length);
    } catch {}
  }
  if (png) return {png};
  image.scrollIntoView({block:'center', inline:'center', behavior:'instant'});
  const rect = image.getBoundingClientRect();
  if (rect.left < 0 || rect.top < 0 || rect.right > innerWidth || rect.bottom > innerHeight)
    return null;
  if (document.elementFromPoint(rect.x + rect.width / 2, rect.y + rect.height / 2) !== image)
    return null;
  return {clip:{x:rect.x + scrollX, y:rect.y + scrollY, width:rect.width, height:rect.height, scale:1}};
})()"#;

pub(super) struct Failure {
    pub status: StatusCode,
    pub message: String,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unauthorized(message: &'static str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    fn upstream(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }
}

pub(super) fn supports(connection: &Value) -> bool {
    connection["provider"] == "chatgpt-web"
        && matches!(connection["authType"].as_str(), Some("cookie" | "apikey"))
}

pub(super) async fn complete(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
    stream: bool,
) -> Result<Response, Failure> {
    let selection = selection(model, body)?;
    let tools = ClientTools::from_request(body)?;
    let (prompt, attachments) = prompt(body, tools.as_ref())?;
    let cookies = cookies(account)?;
    let (mut page, route) = open(storage, settings, account, &cookies).await?;
    let cadence = Duration::from_millis(if route.use_fixture() { 15 } else { 250 });
    ready(&mut page, Some(selection), cadence).await?;
    let guard = composer_guard(selection);
    let _attachment_files = if attachments.total_bytes == 0 {
        None
    } else {
        let mut scratch = tempfile::Builder::new();
        scratch.prefix("9router-chatgpt-attachments-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            scratch.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = scratch
            .tempdir()
            .map_err(|_| Failure::upstream("Could not create a private attachment directory"))?;
        let files = attachments.stage(directory.path()).await?;
        check_route(storage, account, &route).await?;
        page.attach_chatgpt_files(&guard, &files)
            .await
            .map_err(Failure::upstream)?;
        Some(directory)
    };
    fill(&mut page, &guard, &prompt).await?;
    page.observe_conversation()
        .await
        .map_err(Failure::upstream)?;
    send(storage, account, &route, &mut page, &guard, cadence).await?;
    let (status, capture) = page.conversation_response().await.map_err(|error| {
        if error == "ChatGPT Web conversation timed out" {
            Failure::new(StatusCode::GATEWAY_TIMEOUT, error)
        } else {
            Failure::upstream(error)
        }
    })?;
    match status {
        200 => (),
        401 | 403 => {
            return Err(Failure::unauthorized(
                "ChatGPT Web rejected the browser session",
            ));
        }
        429 => {
            return Err(Failure::new(
                StatusCode::TOO_MANY_REQUESTS,
                "ChatGPT Web returned HTTP 429",
            ));
        }
        _ => return Err(Failure::upstream("ChatGPT Web rejected the conversation")),
    }
    let (body, encoded) =
        capture.ok_or_else(|| Failure::upstream("ChatGPT Web conversation was missing"))?;
    let body = if encoded {
        let bytes = STANDARD
            .decode(body)
            .ok()
            .filter(|bytes| bytes.len() <= MAX_RESPONSE)
            .ok_or_else(|| Failure::upstream("ChatGPT Web conversation encoding was invalid"))?;
        String::from_utf8(bytes)
            .map_err(|_| Failure::upstream("ChatGPT Web conversation was not UTF-8"))?
    } else {
        body
    };
    let turn = parse_direct_conversation(&body)
        .map_err(|_| Failure::upstream("ChatGPT Web returned an incomplete assistant response"))?;
    let reply = cookies.iter().fold(turn.text, |text, (_, secret)| {
        if secret.len() >= 8 {
            text.replace(secret, "[redacted]")
        } else {
            text
        }
    });
    render(model, reply, stream, tools.as_ref())
}

pub(super) async fn image(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
) -> Result<Value, Failure> {
    if !supports(account) {
        return Err(Failure::unauthorized(
            "ChatGPT Web requires a saved session",
        ));
    }
    if model != "gpt-image-2" {
        return Err(Failure::bad("Unsupported ChatGPT Web image model"));
    }
    let prompt = body["prompt"]
        .as_str()
        .map(js_trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| Failure::bad("prompt is required"))?;
    let size = body["size"]
        .as_str()
        .filter(|size| {
            size.split_once('x').is_some_and(|(width, height)| {
                [width, height]
                    .iter()
                    .all(|edge| edge.parse::<u32>().is_ok_and(|n| n > 0))
            })
        })
        .map(|size| format!(" with size {size}"))
        .unwrap_or_default();
    let prompt = format!("Generate an image{size}: {prompt}");
    if prompt.len() > MAX_PROMPT {
        return Err(Failure::bad("ChatGPT Web prompt is too large"));
    }
    let n = body["n"]
        .as_f64()
        .or_else(|| body["n"].as_str().and_then(|value| value.parse().ok()))
        .filter(|n| n.is_finite())
        .unwrap_or(1.0)
        .clamp(1.0, 4.0) as usize;
    let guard = format!("(() => {{ const state = {PAGE_STATE}; return {COMPOSER_READY}; }})()");
    let cookies = cookies(account)?;
    let mut data = Vec::with_capacity(n);
    for _ in 0..n {
        let (mut page, route) = open(storage, settings, account, &cookies).await?;
        let cadence = Duration::from_millis(if route.use_fixture() { 15 } else { 250 });
        ready(&mut page, None, cadence).await?;
        fill(&mut page, &guard, &prompt).await?;
        send(storage, account, &route, &mut page, &guard, cadence).await?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let encoded = loop {
            if Instant::now() >= deadline {
                return Err(Failure::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "ChatGPT Web image did not become available",
                ));
            }
            sleep(Duration::from_secs(1)).await;
            let image = page
                .evaluate(READY_IMAGE)
                .await
                .map_err(Failure::upstream)?;
            if let Some(png) = image["png"].as_str() {
                break png.to_owned();
            }
            if let Some(clip) = image.get("clip") {
                break page
                    .capture_chatgpt_image(clip)
                    .await
                    .map_err(Failure::upstream)?;
            }
        };
        if STANDARD.decode(&encoded).ok().is_none_or(|bytes| {
            bytes.len() > MAX_IMAGE_BYTES
                || !matches!(image_info(&bytes), Some(("png", width, height))
                    if width > 0 && height > 0)
        }) {
            return Err(Failure::upstream("Invalid ChatGPT Web image artifact"));
        }
        data.push(json!({"b64_json":encoded,"mime_type":"image/png"}));
    }
    Ok(json!({"created":chrono::Utc::now().timestamp(),"data":data}))
}

async fn open(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    cookies: &[(String, String)],
) -> Result<(ManagedPage, Delivery), Failure> {
    let route = Delivery::select(storage, settings, account, ORIGIN)
        .await
        .map_err(|_| Failure::upstream("ChatGPT Web connection is unavailable"))?;
    if matches!(&route, Delivery::Tunnel(_)) {
        return Err(Failure::upstream(
            "ChatGPT Web browser cannot use the configured proxy",
        ));
    }
    let page = ManagedPage::open_chatgpt(cookies).await.map_err(|error| {
        if error == "No Chrome/Chromium executable found" {
            Failure::new(StatusCode::SERVICE_UNAVAILABLE, error)
        } else {
            Failure::upstream(error)
        }
    })?;
    Ok((page, route))
}

async fn fill(page: &mut ManagedPage, guard: &str, prompt: &str) -> Result<(), Failure> {
    let prompt = json!(prompt);
    let fill = format!(
        r#"(() => {{
      if (!({guard})) return false;
      const composer = document.querySelector('#prompt-textarea[contenteditable="true"]');
      if (composer.textContent.trim()) return false;
      composer.focus();
      return document.execCommand('insertText', false, {prompt});
    }})()"#
    );
    if page.evaluate(&fill).await.map_err(Failure::upstream)? != true {
        return Err(Failure::upstream(
            "ChatGPT Web composer could not accept text",
        ));
    }
    Ok(())
}

async fn send(
    storage: &Storage,
    account: &Value,
    route: &Delivery,
    page: &mut ManagedPage,
    guard: &str,
    cadence: Duration,
) -> Result<(), Failure> {
    let send = format!(
        r#"(() => {{
      if (!({guard})) return 'unavailable';
      const button = document.querySelector('button[data-testid="send-button"]');
      if (!button || button.disabled || button.getClientRects().length === 0) return 'waiting';
      button.click();
      return 'sent';
    }})()"#
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        check_route(storage, account, route).await?;
        match page
            .evaluate(&send)
            .await
            .map_err(Failure::upstream)?
            .as_str()
        {
            Some("sent") => return Ok(()),
            Some("waiting") if Instant::now() < deadline => sleep(cadence).await,
            _ => {
                return Err(Failure::upstream(
                    "ChatGPT Web send control was unavailable",
                ));
            }
        }
    }
}

async fn check_route(storage: &Storage, prior: &Value, route: &Delivery) -> Result<(), Failure> {
    let id = prior["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Failure::unauthorized("ChatGPT Web has no saved connection"))?
        .to_owned();
    let account = storage
        .run(false, move |db| by_id(db, &id))
        .await
        .map_err(|_| Failure::upstream("Could not verify the ChatGPT Web connection"))?
        .ok_or_else(|| Failure::unauthorized("ChatGPT Web connection was removed"))?;
    if !supports(&account)
        || account["isActive"] == false
        || ["authType", "apiKey", "providerSpecificData"]
            .into_iter()
            .any(|key| account[key] != prior[key])
    {
        return Err(Failure::unauthorized(
            "ChatGPT Web connection changed before sending",
        ));
    }
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(|| Failure::upstream("Could not verify the ChatGPT Web connection settings"))?;
    if !matches!(Delivery::select(storage, &settings, &account, ORIGIN).await, Ok(current) if &current == route)
    {
        return Err(Failure::upstream(
            "ChatGPT Web route changed before sending",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Selection {
    labels: [&'static str; 2],
    picker: bool,
}

fn selection(model: &str, body: &Value) -> Result<Selection, Failure> {
    let model = js_trim(model).to_ascii_lowercase().replace('.', "-");
    let model = model
        .strip_prefix("chatgpt-web/")
        .or_else(|| model.strip_prefix("cgpt-web/"))
        .unwrap_or(&model);
    match model {
        "gpt-5-6-luna-free" => Ok(Selection {
            labels: ["gpt-5.6 luna — free", "gpt-5.6 luna free"],
            picker: false,
        }),
        "gpt-5-6-luna-free-thinking" => Ok(Selection {
            labels: ["gpt-5.6 luna — free thinking", "gpt-5.6 luna free thinking"],
            picker: true,
        }),
        "gpt-5-6-thinking" | "gpt-5-6-sol" if reasoning(body)? => Ok(Selection {
            labels: ["gpt-5.6 sol — thinking", "gpt-5.6 sol thinking"],
            picker: true,
        }),
        "gpt-5-6" | "gpt-5-6-instant" | "gpt-5-6-thinking" | "gpt-5-6-sol" => Ok(Selection {
            labels: ["gpt-5.6 sol — instant", "gpt-5.6 sol instant"],
            picker: true,
        }),
        "gpt-5-6-pro" => Ok(Selection {
            labels: ["gpt-5.6 sol — pro", "gpt-5.6 sol pro"],
            picker: true,
        }),
        "gpt-5-5" | "gpt-5-5-thinking" if reasoning(body)? => Ok(Selection {
            labels: ["gpt-5.5 — thinking", "gpt-5.5 thinking"],
            picker: true,
        }),
        "gpt-5-5" | "gpt-5-5-instant" | "gpt-5-5-thinking" => Ok(Selection {
            labels: ["gpt-5.5 — instant", "gpt-5.5 instant"],
            picker: true,
        }),
        "gpt-5-5-pro" => Ok(Selection {
            labels: ["gpt-5.5 — pro", "gpt-5.5 pro"],
            picker: true,
        }),
        _ => Err(Failure::bad("Unsupported ChatGPT Web model")),
    }
}

fn reasoning(body: &Value) -> Result<bool, Failure> {
    let effort = body["reasoning_effort"]
        .as_str()
        .or_else(|| body["reasoning"]["effort"].as_str())
        .map(str::to_ascii_lowercase);
    match effort.as_deref() {
        Some("none" | "off" | "minimal" | "low") => Ok(false),
        None | Some("medium" | "high" | "xhigh" | "max") => Ok(true),
        Some(_) => Err(Failure::bad("Unsupported ChatGPT Web reasoning effort")),
    }
}

fn composer_guard(selection: Selection) -> String {
    format!(
        r#"(() => {{
      const state = {PAGE_STATE}, model = state.model.toLowerCase().trim();
      return {COMPOSER_READY} && {}.includes(model);
    }})()"#,
        json!(selection.labels)
    )
}

async fn ready(
    page: &mut ManagedPage,
    selection: Option<Selection>,
    cadence: Duration,
) -> Result<(), Failure> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(state) = page.evaluate(PAGE_STATE).await {
            if state["login"] == true && state["auth"] != true {
                return Err(Failure::unauthorized(
                    "ChatGPT Web browser session is not signed in",
                ));
            }
            let model = state["model"].as_str().unwrap_or_default().trim();
            if state["auth"] == true
                && state["ready"] == true
                && (selection.is_none() || !model.is_empty())
            {
                if state["temporary"] != true || state["dialog"] == true {
                    return Err(Failure::upstream(
                        "ChatGPT Web temporary composer was not available",
                    ));
                }
                let Some(selection) = selection else {
                    return Ok(());
                };
                if selection
                    .labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case(model))
                    || (selection.picker && choose_model(page, selection, cadence).await?)
                {
                    return Ok(());
                }
                return Err(Failure::bad(
                    "ChatGPT Web did not confirm the requested model selection",
                ));
            }
        }
        sleep(cadence).await;
    }
    Err(Failure::upstream(
        "ChatGPT Web authenticated composer was not available",
    ))
}

async fn choose_model(
    page: &mut ManagedPage,
    selection: Selection,
    cadence: Duration,
) -> Result<bool, Failure> {
    let switcher = json!(SWITCHER);
    let open = format!(
        r#"(() => {{
        const state = {PAGE_STATE};
        if (!({COMPOSER_READY})) return false;
        const button = document.querySelector({switcher});
        if (!button || !button.getClientRects().length || getComputedStyle(button).visibility !== 'visible'
            || button.disabled || button.closest('[inert],[aria-disabled="true"],[aria-hidden="true"]')
            || !['menu', 'listbox'].includes(button.getAttribute('aria-haspopup'))) return false;
        const expanded = button.getAttribute('aria-expanded');
        if (expanded === 'false') button.click();
        else if (expanded !== 'true') return false;
        return true;
        }})()"#
    );
    if page.evaluate(&open).await.map_err(Failure::upstream)? != true {
        return Ok(false);
    }

    let labels = json!(selection.labels);
    let choose = format!(
        r#"(() => {{
        const state = {PAGE_STATE};
        if (!({COMPOSER_READY})) return 'unavailable';
        const button = document.querySelector({switcher});
        if (!button || !button.getClientRects().length || getComputedStyle(button).visibility !== 'visible'
            || button.disabled || button.closest('[inert],[aria-disabled="true"],[aria-hidden="true"]'))
            return 'unavailable';
        const kind = button.getAttribute('aria-haspopup');
        if (!['menu', 'listbox'].includes(kind)) return 'unavailable';
        const id = button.getAttribute('aria-controls');
        const menu = id && document.getElementById(id);
        if (button.getAttribute('aria-expanded') !== 'true' || !menu || !menu.getClientRects().length)
            return 'waiting';
        if (menu.getAttribute('role') !== kind || menu.closest('[inert],[aria-hidden="true"],[role="dialog"]'))
            return 'unavailable';
        const roles = kind === 'listbox' ? '[role="option"]' : '[role="menuitem"],[role="menuitemradio"]';
        const options = Array.from(menu.querySelectorAll(roles)).filter(option => {{
            const label = (option.innerText || '').trim() || (option.getAttribute('aria-label') || '').trim();
            return option.getClientRects().length && getComputedStyle(option).visibility === 'visible' && !option.disabled
                && !option.closest('[inert],[aria-disabled="true"],[aria-hidden="true"],[role="dialog"]')
                && option.closest('[role="menu"],[role="listbox"]') === menu
                && {labels}.includes(label.toLowerCase());
        }});
        if (options.length !== 1) return 'unavailable';
        options[0].click();
        return 'selected';
        }})()"#
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match page
            .evaluate(&choose)
            .await
            .map_err(Failure::upstream)?
            .as_str()
        {
            Some("selected") => break,
            Some("waiting") if Instant::now() < deadline => sleep(cadence).await,
            _ => return Ok(false),
        }
    }

    let guard = composer_guard(selection);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if page.evaluate(&guard).await.map_err(Failure::upstream)? == true {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(cadence).await;
    }
}

fn cookies(account: &Value) -> Result<Vec<(String, String)>, Failure> {
    let raw = account["providerSpecificData"]
        .get("storageState")
        .filter(|value| !value.is_null())
        .unwrap_or(&account["apiKey"]);
    let mut cookies = Vec::new();
    if let Some(text) = raw.as_str() {
        let encoded = js_trim(text);
        if encoded.starts_with('{') {
            let parsed: Value = serde_json::from_str(encoded)
                .map_err(|_| Failure::bad("Invalid ChatGPT Web storage state"))?;
            storage_cookies(&parsed, &mut cookies)?;
        } else {
            let header = encoded
                .get(..7)
                .filter(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
                .map_or(encoded, |_| js_trim(&encoded[7..]));
            if !header.contains('=') {
                remember(&mut cookies, SESSION_COOKIE, header)?;
            } else {
                for (name, value) in header.split(';').filter_map(|part| part.split_once('=')) {
                    remember(&mut cookies, js_trim(name), js_trim(value))?;
                }
            }
        }
    } else if raw.is_object() {
        storage_cookies(raw, &mut cookies)?;
    }
    if cookies.is_empty() {
        return Err(Failure::unauthorized("Missing ChatGPT Web session cookie"));
    }
    Ok(cookies)
}

fn storage_cookies(state: &Value, output: &mut Vec<(String, String)>) -> Result<(), Failure> {
    let cookies = state["cookies"]
        .as_array()
        .ok_or_else(|| Failure::bad("Invalid ChatGPT Web storage-state cookies"))?;
    for cookie in cookies {
        let Some(name) = cookie["name"].as_str() else {
            continue;
        };
        let Some(value) = cookie["value"].as_str() else {
            continue;
        };
        if cookie["domain"]
            .as_str()
            .is_some_and(|host| host.trim_start_matches('.') == "chatgpt.com")
            && cookie["path"] == "/"
            && cookie["secure"] == true
        {
            remember(output, name, value)?;
        }
    }
    Ok(())
}

fn remember(cookies: &mut Vec<(String, String)>, name: &str, value: &str) -> Result<(), Failure> {
    let chunked = name
        .strip_prefix(SESSION_COOKIE)
        .and_then(|name| name.strip_prefix('.'))
        .is_some_and(|part| {
            !part.is_empty() && part.len() <= 4 && part.bytes().all(|byte| byte.is_ascii_digit())
        });
    if name != SESSION_COOKIE && !chunked {
        return Ok(());
    }
    if value.is_empty()
        || value.len() > 16 * 1024
        || !value
            .bytes()
            .all(|byte| matches!(byte, 0x21|0x23..=0x2b|0x2d..=0x3a|0x3c..=0x5b|0x5d..=0x7e))
    {
        return Err(Failure::unauthorized("Invalid ChatGPT Web session cookie"));
    }
    if let Some((_, existing)) = cookies.iter_mut().find(|(key, _)| key == name) {
        *existing = value.to_owned();
    } else {
        cookies.push((name.to_owned(), value.to_owned()));
    }
    Ok(())
}

fn prompt(body: &Value, tools: Option<&ClientTools<'_>>) -> Result<(String, Attachments), Failure> {
    let messages = body["messages"]
        .as_array()
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| Failure::bad("ChatGPT Web requires messages"))?;
    let only = messages.len() == 1 && messages[0]["role"] == "user" && tools.is_none();
    let latest_user = tools.and_then(|_| {
        messages
            .iter()
            .rposition(|message| message["role"] == "user")
    });
    let mut system = Vec::new();
    let mut history = Vec::new();
    let mut names = HashMap::new();
    let mut saw_tools = false;
    let mut attachments = Attachments::default();
    for (index, message) in messages.iter().enumerate() {
        let role = message["role"]
            .as_str()
            .ok_or_else(|| Failure::bad("Invalid ChatGPT Web message role"))?;
        let mut text = text_content(&message["content"], &mut attachments)?;
        if only {
            return Ok((check_prompt(text)?, attachments));
        }
        if let Some(tools) = tools
            && latest_user == Some(index)
            && (message["content"].is_string() || message["content"].is_array())
        {
            text.push_str(&tools.reminder());
        }
        let text = js_trim(&text);
        match role {
            "system" | "developer" if !text.is_empty() => system.push(text.to_owned()),
            "user" if !text.is_empty() => history.push(format!("User:\n{text}")),
            "assistant" => {
                let mut parts = Vec::new();
                if !text.is_empty() {
                    parts.push(text.to_owned());
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let name = call["function"]["name"].as_str().unwrap_or_default();
                    let arguments = &call["function"]["arguments"];
                    let arguments =
                        if let Some(text) = arguments.as_str().filter(|text| !text.is_empty()) {
                            text.to_owned()
                        } else if arguments.is_null() {
                            "{}".into()
                        } else {
                            arguments.to_string()
                        };
                    if let Some(id) = call["id"].as_str().filter(|id| !id.is_empty()) {
                        names.insert(id, name);
                    }
                    parts.push(format!(
                        "<tool>{{\"name\": {}, \"arguments\": {arguments}}}</tool>",
                        json!(name)
                    ));
                    saw_tools = true;
                }
                if !parts.is_empty() {
                    history.push(format!("Assistant:\n{}", parts.join("\n")));
                }
            }
            "tool" => {
                let name = message["tool_call_id"]
                    .as_str()
                    .and_then(|id| names.get(id).copied())
                    .filter(|name| !name.is_empty())
                    .or_else(|| message["name"].as_str().filter(|name| !name.is_empty()))
                    .unwrap_or("tool");
                history.push(format!(
                    "Tool result ({name}): {}",
                    if text.is_empty() { "(no output)" } else { text }
                ));
                saw_tools = true;
            }
            "system" | "developer" | "user" => (),
            _ => {
                return Err(Failure::bad(
                    "ChatGPT Web received an unsupported message role",
                ));
            }
        }
    }
    if saw_tools {
        history.push("Continue the task using the tool results above. Do NOT repeat tool calls that already succeeded; perform the next step or give the final answer.".into());
    }
    if let Some(tools) = tools {
        system.push(tools.contract());
    }
    system.extend(history);
    Ok((check_prompt(system.join("\n\n"))?, attachments))
}

fn text_content(content: &Value, attachments: &mut Attachments) -> Result<String, Failure> {
    match content {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                if part.as_str().is_some_and(|source| {
                    source
                        .get(..11)
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:image/"))
                }) {
                    continue;
                }
                match part["type"].as_str() {
                    Some("text" | "input_text") if part["text"].is_string() => {
                        text.push_str(part["text"].as_str().unwrap_or_default());
                    }
                    Some(kind) => match js_trim(kind).to_ascii_lowercase().as_str() {
                        "file" | "input_file" => attachments.push_file(part)?,
                        "image" => attachments.push_image(&part["source"], part)?,
                        "image_url" => attachments.push_image(&part["image_url"]["url"], part)?,
                        "input_image" => attachments.push_image(
                            part.get("image_url")
                                .filter(|url| !url.is_null())
                                .unwrap_or(&part["url"]),
                            part,
                        )?,
                        _ => return Err(unsupported_content()),
                    },
                    None => {
                        return Err(unsupported_content());
                    }
                }
            }
            Ok(text)
        }
        _ => Err(unsupported_content()),
    }
}

fn unsupported_content() -> Failure {
    Failure::bad("ChatGPT Web native composer mode supports text, inline files, and inline images")
}

#[derive(Default)]
struct Attachments {
    images: Vec<(String, Vec<u8>)>,
    files: Vec<(String, Vec<u8>)>,
    total_bytes: usize,
}

pub(crate) struct StagedAttachment {
    pub(crate) path: PathBuf,
    pub(crate) size: usize,
    pub(crate) mime: Option<&'static str>,
}

impl Attachments {
    fn check_count(&self) -> Result<(), Failure> {
        if self.files.len() + self.images.len() >= MAX_ATTACHMENTS {
            return Err(Failure::bad(
                "ChatGPT Web accepts at most 10 attachments per request",
            ));
        }
        Ok(())
    }

    fn add_bytes(&mut self, size: usize) -> Result<(), Failure> {
        self.total_bytes += size;
        if self.total_bytes > MAX_ATTACHMENT_BYTES {
            return Err(Failure::bad(
                "Combined ChatGPT Web attachments are too large",
            ));
        }
        Ok(())
    }

    fn push_file(&mut self, part: &Value) -> Result<(), Failure> {
        self.check_count()?;
        let file = part
            .get("file")
            .filter(|file| file.is_object())
            .unwrap_or(part);
        let field = |key: &str| {
            file.get(key)
                .filter(|value| !value.is_null())
                .unwrap_or(&part[key])
                .as_str()
                .map(js_trim)
                .filter(|value| !value.is_empty())
        };
        let encoded = field("file_data")
            .ok_or_else(|| Failure::bad("ChatGPT Web file input requires base64 file_data"))?;
        let payload = data_url(encoded)?.map_or(encoded, |(_, payload)| payload);
        let bytes = decode_attachment(payload, MAX_ATTACHMENT_BYTES)?;
        self.add_bytes(bytes.len())?;
        self.files
            .push((attachment_name(field("filename"), "attachment.bin"), bytes));
        Ok(())
    }

    fn push_image(&mut self, source: &Value, part: &Value) -> Result<(), Failure> {
        let Some(source) = source
            .as_str()
            .map(js_trim)
            .filter(|source| !source.is_empty())
        else {
            return Ok(());
        };
        self.check_count()?;
        let (declared, payload) = data_url(source)?
            .ok_or_else(|| Failure::bad("ChatGPT Web attachments must be data URLs"))?;
        let bytes = decode_attachment(payload, MAX_IMAGE_BYTES)?;
        self.add_bytes(bytes.len())?;
        let (media, width, height) = image_info(&bytes)
            .filter(|(_, width, height)| *width != 0 && *height != 0)
            .ok_or_else(|| Failure::bad("Image attachment is undecodable or unsupported"))?;
        let declared = js_trim(declared);
        if declared
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
            && !declared[6..].eq_ignore_ascii_case(media)
            && !(media == "jpeg" && declared[6..].eq_ignore_ascii_case("jpg"))
        {
            return Err(Failure::bad(
                "Image attachment type does not match its data",
            ));
        }
        if width.max(height) > MAX_IMAGE_EDGE
            || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
        {
            return Err(Failure::bad("Image attachment dimensions are too large"));
        }
        let extension = if media == "jpeg" { "jpg" } else { media };
        let fallback = format!("image-{}.{extension}", self.images.len() + 1);
        let raw = part
            .get("filename")
            .filter(|name| !name.is_null())
            .unwrap_or(&part["name"]);
        let mut name = attachment_name(raw.as_str(), &fallback);
        let has_extension = |name: &str| {
            name.rsplit_once('.').is_some_and(|(_, suffix)| {
                suffix.eq_ignore_ascii_case(extension)
                    || (media == "jpeg" && suffix.eq_ignore_ascii_case("jpeg"))
            })
        };
        if !has_extension(&name) {
            let stem = name
                .rsplit_once('.')
                .map_or(name.as_str(), |(stem, _)| stem);
            name = attachment_name(Some(&format!("{stem}.{extension}")), &fallback);
            if !has_extension(&name) {
                name = fallback;
            }
        }
        self.images.push((name, bytes));
        Ok(())
    }

    async fn stage(self, directory: &Path) -> Result<Vec<StagedAttachment>, Failure> {
        let failure = |_| Failure::upstream("Could not stage the ChatGPT Web attachment");
        let count = self.images.len() + self.files.len();
        let mut files = Vec::with_capacity(count);
        for (index, (name, bytes)) in self.images.into_iter().chain(self.files).enumerate() {
            let directory = directory.join(index.to_string());
            tokio::fs::create_dir(&directory).await.map_err(failure)?;
            let path = directory.join(&name);
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&path).await.map_err(failure)?;
            file.write_all(&bytes).await.map_err(failure)?;
            file.flush().await.map_err(failure)?;
            files.push(StagedAttachment {
                mime: mime_guess::from_path(&path).first_raw(),
                path,
                size: bytes.len(),
            });
        }
        Ok(files)
    }
}

fn data_url(value: &str) -> Result<Option<(&str, &str)>, Failure> {
    if !value
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return Ok(None);
    }
    let (header, payload) = value[5..]
        .split_once(',')
        .ok_or_else(|| Failure::bad("Attachment data URL is malformed"))?;
    if !header
        .split(';')
        .any(|token| token.eq_ignore_ascii_case("base64"))
    {
        return Err(Failure::bad("Attachment data URL must be base64 encoded"));
    }
    Ok(Some((
        header.split(';').next().unwrap_or_default(),
        payload,
    )))
}

fn decode_attachment(encoded: &str, limit: usize) -> Result<Vec<u8>, Failure> {
    if encoded.len() > MAX_ATTACHMENT_BYTES * 2 {
        return Err(Failure::bad("Attachment is too large"));
    }
    let normalized: String = encoded.chars().filter(|ch| !js_space(*ch)).collect();
    let bytes = STANDARD
        .decode(normalized)
        .ok()
        .filter(|bytes| !bytes.is_empty())
        .ok_or_else(|| Failure::bad("Attachment contains invalid base64 data"))?;
    if bytes.len() > limit {
        return Err(Failure::bad("Attachment is too large"));
    }
    Ok(bytes)
}

fn image_info(bytes: &[u8]) -> Option<(&'static str, u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.get(12..16) == Some(b"IHDR") {
        let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
        let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
        return Some(("png", width, height));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        let width = u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?);
        let height = u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?);
        return Some(("gif", u32::from(width), u32::from(height)));
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        let (width, height) = jpeg_dimensions(bytes)?;
        return Some(("jpeg", u32::from(width), u32::from(height)));
    }
    if bytes.len() >= 30 && bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        let (width, height) = match &bytes[12..16] {
            b"VP8X" => (
                u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]) + 1,
                u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]) + 1,
            ),
            b"VP8 " => (
                u32::from(u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff),
                u32::from(u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff),
            ),
            b"VP8L" if bytes[20] == 0x2f => {
                let size = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
                ((size & 0x3fff) + 1, ((size >> 14) & 0x3fff) + 1)
            }
            _ => return None,
        };
        return Some(("webp", width, height));
    }
    None
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u16, u16)> {
    let mut offset = 2;
    while offset + 9 < bytes.len() {
        if bytes[offset] != 0xff || bytes[offset + 1] == 0xff {
            offset += 1;
            continue;
        }
        let marker = bytes[offset + 1];
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let height = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]);
            let width = u16::from_be_bytes([bytes[offset + 7], bytes[offset + 8]]);
            return Some((width, height));
        }
        if matches!(marker, 0xd9 | 0xda) {
            break;
        }
        offset += match marker {
            0x01 | 0xd0..=0xd8 => 2,
            _ => 2 + usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]])),
        };
    }
    None
}

fn attachment_name(raw: Option<&str>, fallback: &str) -> String {
    let leaf = raw
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let clean: String = leaf
        .chars()
        .filter(|ch| !ch.is_ascii_control())
        .map(|ch| {
            if matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let mut name = String::new();
    let mut chars = 0;
    for ch in js_trim(&clean).chars() {
        chars += ch.len_utf16();
        if chars > 180 || name.len() + ch.len_utf8() > 255 {
            break;
        }
        name.push(ch);
    }
    let name = name.trim_end_matches(['.', ' ']);
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if name.is_empty()
        || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(*prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        })
    {
        fallback.to_owned()
    } else {
        name.to_owned()
    }
}

fn check_prompt(prompt: String) -> Result<String, Failure> {
    if js_trim(&prompt).is_empty() {
        return Err(Failure::bad("ChatGPT Web requires non-empty text"));
    }
    if prompt.len() > MAX_PROMPT {
        return Err(Failure::bad("ChatGPT Web prompt is too large"));
    }
    Ok(prompt)
}

fn render(
    model: &str,
    text: String,
    stream: bool,
    tools: Option<&ClientTools<'_>>,
) -> Result<Response, Failure> {
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let created = chrono::Utc::now().timestamp();
    let (text, calls) = if let Some(tools) = tools {
        let calls = tools.calls(&text);
        (tools.redact(text), calls)
    } else {
        (text, Vec::new())
    };
    let (media, body) = if stream {
        let mut sse = String::new();
        let mut push = |delta: Value, finish: Option<&str>| {
            let mut chunk = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,
                "choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
            if tools.is_some() {
                chunk["system_fingerprint"] = Value::Null;
                chunk["choices"][0]["logprobs"] = Value::Null;
            }
            sse.push_str(&format!("data: {chunk}\n\n"));
        };
        push(json!({"role":"assistant"}), None);
        if !calls.is_empty() {
            for (index, mut call) in calls.into_iter().enumerate() {
                call["index"] = json!(index);
                push(json!({"tool_calls":[call]}), None);
            }
            push(json!({}), Some("tool_calls"));
        } else if tools.is_some() {
            push(json!({"content":text}), Some("stop"));
        } else {
            push(json!({"content":text}), None);
            push(json!({}), Some("stop"));
        }
        sse.push_str("data: [DONE]\n\n");
        (
            if tools.is_some() {
                "text/event-stream"
            } else {
                "text/event-stream; charset=utf-8"
            },
            sse,
        )
    } else {
        let (message, finish) = if calls.is_empty() {
            (json!({"role":"assistant","content":text}), "stop")
        } else {
            (
                json!({"role":"assistant","content":null,"tool_calls":calls}),
                "tool_calls",
            )
        };
        (
            "application/json",
            json!({"id":id,"object":"chat.completion","created":created,"model":model,
            "choices":[{"index":0,"message":message,"finish_reason":finish}]})
            .to_string(),
        )
    };
    let mut response = axum::http::Response::builder()
        .url(Url::parse(ORIGIN).expect("fixed ChatGPT Web origin"))
        .header(header::CONTENT_TYPE, media);
    if stream && tools.is_some() {
        response = response
            .header(header::CACHE_CONTROL, "no-cache")
            .header("X-Accel-Buffering", "no");
    }
    response
        .body(reqwest::Body::from(body))
        .map(Into::into)
        .map_err(|_| Failure::upstream("ChatGPT Web response could not be created"))
}
