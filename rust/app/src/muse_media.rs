use std::time::Duration;

use axum::http::StatusCode;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use reqwest::Url;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

use super::{CHAT_STATE, Failure, QUOTA_HINTS, open, send};
use crate::chat_output::js_text;
use crate::chat_prefetch::{fetch_image_bytes, mime};
use crate::provider_connections::truthy;
use crate::provider_http::Delivery;
use crate::providers::web_login::MusePage;
use crate::storage::Storage;

const MAX_IMAGE: usize = 16 * 1024 * 1024;
const MAX_VIDEO: usize = 32 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Image,
    Video,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
        }
    }

    fn mime(self) -> &'static str {
        match self {
            Self::Image => "image/png",
            Self::Video => "video/mp4",
        }
    }

    fn limit(self) -> usize {
        match self {
            Self::Image => MAX_IMAGE,
            Self::Video => MAX_VIDEO,
        }
    }

    fn seconds(self) -> u64 {
        match self {
            Self::Image => 240,
            Self::Video => 600,
        }
    }

    fn grace(self, fixture: bool) -> Duration {
        if fixture {
            return Duration::from_millis(60);
        }
        Duration::from_secs(match self {
            Self::Image => 8,
            Self::Video => 90,
        })
    }
}

pub(crate) async fn image(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    _model: &str,
    body: &Value,
) -> Result<Value, Failure> {
    let prompt = media_prompt(body, Kind::Image, false)?;
    let n = body["n"]
        .as_f64()
        .or_else(|| body["n"].as_str()?.trim().parse().ok())
        .filter(|n| !n.is_nan())
        .unwrap_or(1.0)
        .clamp(1.0, 4.0)
        .ceil() as usize;
    let mut data = Vec::with_capacity(n);
    for _ in 0..n {
        let media = generate(storage, settings, account, Kind::Image, &prompt, &[]).await?;
        data.push(json!({"b64_json":media.base64,"mime_type":media.mime}));
    }
    Ok(json!({"created":chrono::Utc::now().timestamp(),"data":data}))
}

pub(crate) async fn video(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
) -> Result<Value, Failure> {
    let sources = video_refs(body);
    let prompt = media_prompt(body, Kind::Video, !sources.is_empty())?;
    let media = generate(storage, settings, account, Kind::Video, &prompt, &sources).await?;
    let now = chrono::Utc::now();
    let mut video = json!({"url":format!("data:{};base64,{}",media.mime,media.base64)});
    for (name, value) in [("width", media.width), ("height", media.height)] {
        if let Some(value) = value {
            video[name] = json!(value);
        }
    }
    Ok(json!({
        "id":format!("video-muse-web-{}",now.timestamp_millis()),"object":"video.generation",
        "created":now.timestamp(),"model":if model.is_empty(){"muse-web"}else{model},
        "status":"completed","video":video,
    }))
}

fn video_refs(body: &Value) -> Vec<&str> {
    let first = body
        .get("image")
        .filter(|value| !value.is_null())
        .unwrap_or(&body["image_url"]);
    let first = first
        .as_array()
        .map_or_else(|| std::slice::from_ref(first), Vec::as_slice);
    first
        .iter()
        .chain(body["images"].as_array().into_iter().flatten())
        .chain(body["image_urls"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .filter(|text| !text.is_empty())
        .take(MAX_ATTACHMENTS)
        .collect()
}

fn media_prompt(body: &Value, kind: Kind, has_image: bool) -> Result<String, Failure> {
    let prompt = body["prompt"]
        .as_str()
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| {
            Failure::new(
                StatusCode::BAD_REQUEST,
                "Muse media generation requires a prompt",
            )
        })?;
    let mut hints = Vec::new();
    if truthy(&body["size"]) {
        hints.push(format!("aspect ratio {}", js_text(&body["size"])));
    }
    if kind == Kind::Video {
        let duration = body
            .get("seconds")
            .filter(|time| !time.is_null())
            .unwrap_or(&body["duration"]);
        if truthy(duration) {
            hints.push(format!("{} seconds long", js_text(duration)));
        }
    }
    let verb = match (kind, has_image) {
        (Kind::Image, _) => "Generate an image",
        (Kind::Video, true) => "Animate the attached image into a video",
        (Kind::Video, false) => "Generate a video",
    };
    let suffix = if hints.is_empty() {
        String::new()
    } else {
        format!(" ({})", hints.join(", "))
    };
    Ok(format!("{verb}{suffix}: {prompt}"))
}

struct Media {
    base64: String,
    mime: String,
    width: Option<u64>,
    height: Option<u64>,
}

async fn generate(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    kind: Kind,
    prompt: &str,
    sources: &[&str],
) -> Result<Media, Failure> {
    let (mut page, fixture) = open(storage, settings, account).await?;
    let cadence = Duration::from_millis(if fixture { 15 } else { 600 });
    let expression = format!(
        r#"(() => {{
      const chat = {CHAT_STATE};
      const attachments = Array.from(document.querySelectorAll('[data-testid^="hatch-chat-attachment-presentation-"]')).map(node => {{
        const video = node.querySelector('video'), image = node.querySelector('img');
        const media = video || image;
        return {{tid:node.getAttribute('data-testid') || '',kind:video?'video':image?'image':'unknown',
                src:media ? (media.currentSrc || media.src || '') : '',
                width:media ? (media.videoWidth || media.naturalWidth || 0) : 0,
                height:media ? (media.videoHeight || media.naturalHeight || 0) : 0}};
      }});
      return {{...chat,attachments}};
    }})()"#
    );
    let base = page
        .evaluate(&expression)
        .await
        .map_err(Failure::upstream)?;
    ensure_turn(&base)?;
    if !sources.is_empty() {
        attach(&mut page, storage, settings, account, sources).await?;
    }
    send(&mut page, prompt, cadence).await?;
    let started = Instant::now();
    let timeout = Duration::from_secs(kind.seconds());
    let max = timeout.mul_f64(2.5);
    let mut busy = false;
    let mut base_count = base["agentCount"].as_u64().unwrap_or_default();
    let previous = base["attachments"].as_array().map_or(0, Vec::len);
    let mut awaiting = None;
    let mut asked = false;
    while started.elapsed() < timeout || (busy && started.elapsed() < max) {
        sleep(cadence).await;
        let state = page
            .evaluate(&expression)
            .await
            .map_err(Failure::upstream)?;
        ensure_turn(&state)?;
        let attachments = state["attachments"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let fresh = attachments.get(previous..).unwrap_or_default();
        if let Some(asset) = fresh.iter().rev().find(|value| {
            value["kind"] == kind.name()
                && value["src"].as_str().is_some_and(|value| !value.is_empty())
        }) {
            return download(&mut page, asset, kind).await;
        }
        let loading = fresh.iter().any(|item| {
            item["tid"]
                .as_str()
                .is_some_and(|tid| tid.contains(kind.name()))
        });
        busy = state["generating"] == true || loading;
        let count = state["agentCount"].as_u64().unwrap_or_default();
        if busy
            || count <= base_count
            || !state["lastText"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        {
            awaiting = None;
            continue;
        }
        let since = *awaiting.get_or_insert_with(Instant::now);
        if since.elapsed() >= kind.grace(fixture) {
            if asked {
                return Err(Failure::upstream(format!(
                    "Muse did not attach the generated {}",
                    kind.name()
                )));
            }
            send(
                &mut page,
                &format!(
                    "Please attach the final {} file here in the chat, as a single attachment.",
                    kind.name()
                ),
                cadence,
            )
            .await?;
            asked = true;
            base_count = count;
            awaiting = None;
        }
    }
    Err(Failure::timed_out("Muse media generation timed out"))
}

fn ensure_turn(state: &Value) -> Result<(), Failure> {
    if !state.is_object() {
        return Err(Failure::upstream("Muse media thread navigated away"));
    }
    if state["approval"] == true {
        return Err(Failure::new(
            StatusCode::FORBIDDEN,
            "Muse media requires approval in the provider interface",
        ));
    }
    let reply = state["lastText"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if state["generating"] != true && QUOTA_HINTS.iter().any(|hint| reply.contains(*hint)) {
        return Err(Failure::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Muse reports that this account is out of quota",
        ));
    }
    Ok(())
}

async fn attach(
    page: &mut MusePage,
    storage: &Storage,
    settings: &Value,
    account: &Value,
    sources: &[&str],
) -> Result<(), Failure> {
    let mut attached = 0;
    for (index, source) in sources.iter().enumerate() {
        let Some(data) = input_image(storage, settings, account, source).await else {
            continue;
        };
        let Some(media_type) = mime(&data) else {
            continue;
        };
        let ext = media_type
            .trim_start_matches("image/")
            .replace("jpeg", "jpg");
        let bytes = json!(STANDARD.encode(data));
        let mime = json!(media_type);
        let name = json!(format!("input_{index}.{ext}"));
        let expression = format!(
            r#"(() => {{
          const input = document.querySelector('input[type="file"]');
          if (!input) return false;
          const raw = atob({bytes});
          const bytes = Uint8Array.from(raw, ch => ch.charCodeAt(0));
          const transfer = new DataTransfer();
          transfer.items.add(new File([bytes], {name}, {{type:{mime}}}));
          input.files = transfer.files;
          input.dispatchEvent(new Event('change', {{bubbles:true}}));
          return true;
        }})()"#
        );
        if page
            .evaluate(&expression)
            .await
            .map_err(Failure::upstream)?
            != true
        {
            return Err(Failure::upstream("Muse could not attach the input image"));
        }
        attached += 1;
    }
    if attached == 0 {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "Muse could not load any input image",
        ));
    }
    Ok(())
}

async fn input_image(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    source: &str,
) -> Option<Vec<u8>> {
    if let Some((media_type, encoded)) = source
        .strip_prefix("data:")
        .and_then(|data| data.split_once(";base64,"))
    {
        if !media_type.starts_with("image/") || encoded.len() > MAX_IMAGE.div_ceil(3) * 4 {
            return None;
        }
        let data = STANDARD
            .decode(encoded)
            .or_else(|_| STANDARD_NO_PAD.decode(encoded))
            .ok()?;
        return (data.len() <= MAX_IMAGE).then_some(data);
    }
    let route = Delivery::select(storage, settings, account, source)
        .await
        .ok()?;
    if matches!(route, Delivery::Tunnel(_)) {
        return None;
    }
    let bytes = fetch_image_bytes(source).await?;
    (bytes.len() <= MAX_IMAGE).then_some(bytes)
}

async fn download(page: &mut MusePage, asset: &Value, kind: Kind) -> Result<Media, Failure> {
    let src = asset["src"]
        .as_str()
        .filter(|src| trusted_asset(src))
        .ok_or_else(|| Failure::upstream("Muse attachment is outside the provider origin"))?;
    let src = json!(src);
    let limit = kind.limit();
    let expression = format!(
        r#"(async () => {{
      const response = await fetch({src}, {{credentials:'same-origin',redirect:'error'}});
      if (!response.ok || !response.body) return null;
      const reader = response.body.getReader();
      let size = 0;
      const chunks = [];
      while (true) {{
        const {{done,value}} = await reader.read();
        if (done) break;
        size += value.length;
        if (size > {limit}) {{await reader.cancel();return null;}}
        chunks.push(value);
      }}
      let binary = '';
      for (const bytes of chunks) {{
        for (let n=0;n<bytes.length;n+=32768) {{
          binary += String.fromCharCode(...bytes.subarray(n,n+32768));
        }}
      }}
      return {{base64:btoa(binary),mime:(response.headers.get('content-type') || '').split(';')[0]}};
    }})()"#
    );
    let result = page
        .evaluate_with_timeout(&expression, Duration::from_secs(90))
        .await
        .map_err(Failure::upstream)?;
    let data = result["base64"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= limit.div_ceil(3) * 4)
        .ok_or_else(|| Failure::upstream("Muse returned no valid media bytes"))?;
    if !STANDARD
        .decode(data)
        .is_ok_and(|bytes| bytes.len() <= limit)
    {
        return Err(Failure::upstream("Muse returned invalid base64 media"));
    }
    let mime = result["mime"]
        .as_str()
        .filter(|value| !value.is_empty() && *value != "application/octet-stream")
        .unwrap_or(kind.mime());
    if !mime.split_once('/').is_some_and(|(name, subtype)| {
        name == kind.name()
            && !subtype.is_empty()
            && subtype
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".+-".contains(&byte))
    }) {
        return Err(Failure::upstream(
            "Muse attachment has an invalid media type",
        ));
    }
    Ok(Media {
        base64: data.to_owned(),
        mime: mime.to_owned(),
        width: asset["width"].as_u64().filter(|value| *value > 0),
        height: asset["height"].as_u64().filter(|value| *value > 0),
    })
}

fn trusted_asset(src: &str) -> bool {
    let raw = src.strip_prefix("blob:").unwrap_or(src);
    Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("muse.ai")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}
