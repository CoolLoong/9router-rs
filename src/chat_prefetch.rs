use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{
    Client,
    dns::{Addrs, Name, Resolve, Resolving},
    redirect::Policy,
};
use serde_json::{Value, json};
use tokio::time::{Instant, timeout_at};
use url::{Host, Url};

use crate::chat_format::ChatFormat;

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const FORBIDDEN_PORTS: &[u16] = &[
    0, 1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101,
    102, 103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427,
    465, 512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990,
    993, 995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667,
    6668, 6669, 6679, 6697, 10080,
];

#[derive(Clone, Copy)]
pub enum Source {
    Chat,
    Messages,
    Responses,
    Gemini,
}

impl From<ChatFormat> for Source {
    fn from(format: ChatFormat) -> Self {
        match format {
            ChatFormat::Chat => Self::Chat,
            ChatFormat::Messages => Self::Messages,
            ChatFormat::Responses => Self::Responses,
        }
    }
}

pub fn needs_inline(provider: &str) -> bool {
    matches!(
        provider,
        "gemini"
            | "gemini-cli"
            | "vertex"
            | "antigravity"
            | "ollama"
            | "ollama-local"
            | "kiro"
            | "commandcode"
            | "codex"
    )
}

#[derive(Default)]
pub struct Report {
    pub converted: usize,
    pub saw_remote: bool,
    pub refused: bool,
    pub unavailable: bool,
}

pub async fn apply(body: &mut Value, source: Source) -> Report {
    let source = if matches!(source, Source::Chat)
        && (body["contents"].is_array() || body["request"]["contents"].is_array())
    {
        Source::Gemini
    } else {
        source
    };
    let mut report = Report::default();
    let mut remaining = MAX_REQUEST_BYTES;
    let deadline = Instant::now() + FETCH_TIMEOUT;
    for image in collect(body, source) {
        report.saw_remote = true;
        let url = image.url();
        let limit = remaining.min(MAX_IMAGE_BYTES);
        match fetch(url, limit, deadline).await {
            Ok(Some(fetched)) => {
                remaining -= fetched.bytes.len();
                image.replace(fetched);
                report.converted += 1;
            }
            Ok(None) => report.unavailable = true,
            Err(Refused) => {
                report.refused = true;
                break;
            }
        }
    }
    report
}

pub(crate) async fn fetch_image_bytes(url: &str) -> Option<Vec<u8>> {
    fetch(url, MAX_IMAGE_BYTES, Instant::now() + FETCH_TIMEOUT)
        .await
        .ok()?
        .map(|image| image.bytes)
}

enum Reference<'a> {
    DataUrl(&'a mut Value),
    Claude(&'a mut Value),
    Gemini(&'a mut Value),
}

impl Reference<'_> {
    fn url(&self) -> &str {
        let value = match self {
            Self::DataUrl(value) => &**value,
            Self::Claude(source) => &source["url"],
            Self::Gemini(part) => &part["fileData"]["fileUri"],
        };
        value.as_str().unwrap_or_default()
    }

    fn replace(self, fetched: Image) {
        let mime = fetched.mime;
        let data = STANDARD.encode(fetched.bytes);
        match self {
            Self::DataUrl(value) => *value = json!(format!("data:{mime};base64,{data}")),
            Self::Claude(source) => {
                *source = json!({"type":"base64","media_type":mime,"data":data});
            }
            Self::Gemini(part) => {
                if let Some(fields) = part.as_object_mut() {
                    fields.shift_remove("fileData");
                    fields.insert("inlineData".into(), json!({"mimeType":mime,"data":data}));
                }
            }
        }
    }
}

fn remote(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|value| value.starts_with("http://") || value.starts_with("https://"))
}

fn collect(body: &mut Value, source: Source) -> Vec<Reference<'_>> {
    if matches!(source, Source::Gemini) {
        let envelope = if body["request"]["contents"].is_array() {
            &mut body["request"]
        } else {
            body
        };
        return array(envelope, "contents")
            .flat_map(|content| array(content, "parts"))
            .filter(|part| remote(&part["fileData"]["fileUri"]))
            .map(Reference::Gemini)
            .collect();
    }
    if matches!(source, Source::Responses) && body["input"].is_array() {
        return array(body, "input")
            .flat_map(|item| array(item, "content"))
            .filter(|block| block["type"] == "input_image" && remote(&block["image_url"]))
            .filter_map(|block| block.get_mut("image_url"))
            .map(Reference::DataUrl)
            .collect();
    }
    let messages = array(body, "messages").flat_map(|item| array(item, "content"));
    if matches!(source, Source::Messages) {
        return messages
            .filter(|block| {
                block["type"] == "image"
                    && block["source"]["type"] == "url"
                    && remote(&block["source"]["url"])
            })
            .filter_map(|block| block.get_mut("source"))
            .map(Reference::Claude)
            .collect();
    }
    messages
        .filter_map(|block| {
            if block["type"] != "image_url" {
                return None;
            }
            let url = block.get_mut("image_url")?;
            let value = if url.is_string() {
                url
            } else {
                url.get_mut("url")?
            };
            remote(value).then_some(Reference::DataUrl(value))
        })
        .collect()
}

fn array<'a>(body: &'a mut Value, field: &str) -> impl Iterator<Item = &'a mut Value> {
    body.get_mut(field)
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
}

struct Image {
    bytes: Vec<u8>,
    mime: &'static str,
}

struct Refused;

async fn fetch(input: &str, limit: usize, deadline: Instant) -> Result<Option<Image>, Refused> {
    let Ok(mut url) = Url::parse(input) else {
        return Err(Refused);
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Refused);
    }
    url.set_fragment(None);
    let port = url.port_or_known_default().ok_or(Refused)?;
    if FORBIDDEN_PORTS.binary_search(&port).is_ok() {
        return Err(Refused);
    }
    let host = url.host().ok_or(Refused)?;
    let pinned = match host {
        Host::Ipv4(ip) => Some(SocketAddr::new(
            public_address(IpAddr::V4(ip)).ok_or(Refused)?,
            port,
        )),
        Host::Ipv6(ip) => Some(SocketAddr::new(
            public_address(IpAddr::V6(ip)).ok_or(Refused)?,
            port,
        )),
        Host::Domain(host) => {
            if forbidden_host(host) {
                return Err(Refused);
            }
            if deadline <= Instant::now() {
                return Ok(None);
            }
            let Ok(Ok(addresses)) =
                timeout_at(deadline, tokio::net::lookup_host((host, port))).await
            else {
                return Ok(None);
            };
            let mut first = None;
            for address in addresses {
                let ip = public_address(address.ip()).ok_or(Refused)?;
                first.get_or_insert_with(|| SocketAddr::new(ip, port));
            }
            first
        }
    };
    let Some(address) = pinned.filter(|_| limit != 0 && deadline > Instant::now()) else {
        return Ok(None);
    };
    let image = timeout_at(deadline, download(url, address, limit))
        .await
        .ok()
        .flatten();
    Ok(image)
}

fn forbidden_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    !host.contains('.')
        || matches!(host, "localhost" | "ip6-localhost" | "ip6-loopback")
        || [".local", ".internal", ".localhost"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
}

pub(crate) fn public_address(address: IpAddr) -> Option<IpAddr> {
    let canonical = address.to_canonical();
    let allowed = match canonical {
        IpAddr::V4(address) => public_v4(address),
        IpAddr::V6(address) => public_v6(address),
    };
    allowed.then_some(canonical)
}

fn public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a >= 224
        || address.is_loopback()
        || address.is_link_local()
        || address.is_private()
        || address.is_documentation()
        || a == 100 && (64..=127).contains(&b)
        || (a, b, c) == (192, 0, 0)
        || (a, b, c) == (192, 88, 99)
        || a == 198 && matches!(b, 18 | 19))
}

fn public_v6(address: Ipv6Addr) -> bool {
    let [a, b, ..] = address.segments();
    a & 0xe000 == 0x2000
        && !(a == 0x2001 && (b < 0x200 || b == 0xdb8))
        && !matches!(a, 0x2002 | 0x3ffe | 0x3fff)
}

async fn download(url: Url, address: SocketAddr, limit: usize) -> Option<Image> {
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .dns_resolver(Arc::new(ApprovedAddress {
            host: url.host_str()?.to_owned(),
            address,
        }))
        .build()
        .ok()?;
    let mut response = client.get(url).send().await.ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|size| size > limit as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > limit - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let mime = mime(&bytes)?;
    Some(Image { bytes, mime })
}

struct ApprovedAddress {
    host: String,
    address: SocketAddr,
}

impl Resolve for ApprovedAddress {
    fn resolve(&self, name: Name) -> Resolving {
        let approved = self.host.eq_ignore_ascii_case(name.as_str());
        let address = self.address;
        Box::pin(async move {
            if !approved {
                return Err(io::Error::other("unexpected image server hostname").into());
            }
            Ok(Box::new(std::iter::once(address)) as Addrs)
        })
    }
}

pub(crate) fn mime(bytes: &[u8]) -> Option<&'static str> {
    [
        (b"\x89PNG".as_slice(), "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF8", "image/gif"),
        (b"BM", "image/bmp"),
    ]
    .into_iter()
    .find_map(|(prefix, mime)| bytes.starts_with(prefix).then_some(mime))
    .or_else(|| {
        (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP")).then_some("image/webp")
    })
}
