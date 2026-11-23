use std::collections::HashMap;
use std::sync::LazyLock;

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use chrono::Utc;
use regex::Regex;
use reqwest::{Method, Url, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::chat_output::js_text;

use super::{METADATA_TIMEOUT, Service, bounded, send};

const PATH: &str = "/algo/api/v2/image/upload";
const UPLOAD_LIMIT: usize = 10 * 1024 * 1024;
const INLINE_LIMIT: usize = 512 * 1024;
const MESSAGE_LIMIT: usize = 6 * 1024 * 1024;
const DOCUMENT_REASON: &str = "Qoder reads documents via its file API, not inlined bytes";
static TEXT_DATA_URI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"data:[^;]+;base64,[A-Za-z0-9+/=\s]+").expect("Qoder data URI pattern")
});

pub(super) async fn rewrite(messages: &mut [Value], service: &Service<'_>) {
    let mut images = HashMap::new();
    for message in messages.iter_mut().filter(|message| message.is_object()) {
        let mut content = message
            .get_mut("content")
            .map(Value::take)
            .unwrap_or_default();
        if let Some(sidecar) = message.as_object_mut().and_then(|fields| {
            fields.get("images").filter(|value| value.is_array())?;
            fields.shift_remove("images")
        }) {
            if !content.is_array() {
                content = json!([{"type":"text","text":content.as_str().unwrap_or_default()}]);
            }
            if let Some(parts) = content.as_array_mut() {
                parts.extend(
                    sidecar
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|url| image_url(&js_text(url))),
                );
            }
        }
        message["content"] = match content {
            Value::String(text) => json!(strip_text(&text, false)),
            Value::Array(blocks) => {
                let mut output = Vec::with_capacity(blocks.len());
                for mut block in blocks {
                    rewrite_block(&mut block, service, &mut images).await;
                    if !block.is_null() {
                        output.push(block);
                    }
                }
                if output.is_empty() {
                    json!("")
                } else {
                    json!(output)
                }
            }
            other => other,
        };
    }
    if serde_json::to_vec(&messages).is_ok_and(|bytes| bytes.len() > MESSAGE_LIMIT) {
        for message in messages.iter_mut().filter(|message| message.is_object()) {
            if let Some(text) = message["content"].as_str() {
                message["content"] = json!(strip_text(text, true));
            } else if let Some(parts) = message["content"].as_array_mut() {
                for block in parts {
                    if block["type"] == "image_url"
                        && block_url(block).is_some_and(|url| url.starts_with("data:"))
                    {
                        *block = stub("image", 0, "payload over Qoder size budget");
                    } else if let Some(text) = block["text"].as_str() {
                        block["text"] = json!(strip_text(text, true));
                    }
                }
            }
        }
    }
}

type Cache = HashMap<[u8; 32], String>;

async fn rewrite_block(block: &mut Value, service: &Service<'_>, cache: &mut Cache) {
    match block["type"].as_str() {
        Some("image_url") => {
            let Some(url) = block_url(block).filter(|url| !url.is_empty()) else {
                *block = Value::Null;
                return;
            };
            *block = if url.starts_with("http://") || url.starts_with("https://") {
                image_url(url)
            } else if let Some((mime, data)) = data_uri(url) {
                if is_image(mime) {
                    image_block(service, cache, mime, data, url).await
                } else {
                    stub(
                        "file",
                        decoded_size(data),
                        "non-image bytes are not inlined into Qoder context",
                    )
                }
            } else {
                stub("attachment", 0, "unreadable data URI")
            };
        }
        Some("image") => {
            let source = &block["source"];
            if source["type"] == "url" {
                if let Some(url) = source["url"].as_str() {
                    *block = image_url(url);
                }
            } else if source["type"] == "base64"
                && let Some(data) = source["data"].as_str().filter(|data| !data.is_empty())
            {
                let mime = text(&source["media_type"]).unwrap_or("image/png");
                *block = image_block(
                    service,
                    cache,
                    mime,
                    data,
                    &format!("data:{mime};base64,{data}"),
                )
                .await;
            }
        }
        Some("file") if block["file"].is_object() => {
            let file = &block["file"];
            let name = text(&file["filename"])
                .or_else(|| text(&file["name"]))
                .unwrap_or("file");
            let raw = file["file_data"].as_str().unwrap_or_default();
            let (mime, data) = data_uri(raw).unwrap_or_else(|| {
                (
                    text(&file["format"]).unwrap_or("application/octet-stream"),
                    if raw.starts_with("data:") { "" } else { raw },
                )
            });
            *block = document(service, cache, name, mime, data).await;
        }
        Some("document") if block["source"]["type"] == "base64" => {
            let source = &block["source"];
            if let Some(data) = text(&source["data"]) {
                let mime = text(&source["media_type"]).unwrap_or("application/pdf");
                *block = document(
                    service,
                    cache,
                    text(&block["title"]).unwrap_or("document"),
                    mime,
                    data,
                )
                .await;
            }
        }
        _ => {
            if let Some(text) = block["text"].as_str() {
                block["text"] = json!(strip_text(text, false));
            }
        }
    }
}

async fn image_block(
    service: &Service<'_>,
    cache: &mut Cache,
    mime: &str,
    data: &str,
    original: &str,
) -> Value {
    let bytes = decoded_size(data);
    if let Some(url) = upload(service, cache, mime, data).await {
        image_url(&url)
    } else if (1..=INLINE_LIMIT).contains(&bytes) {
        image_url(original)
    } else {
        stub("image", bytes, "upload failed; not inlined")
    }
}

async fn document(
    service: &Service<'_>,
    cache: &mut Cache,
    name: &str,
    mime: &str,
    data: &str,
) -> Value {
    if is_image(mime)
        && let Some(url) = upload(service, cache, mime, data).await
    {
        return image_url(&url);
    }
    stub(name, decoded_size(data), DOCUMENT_REASON)
}

async fn upload(
    service: &Service<'_>,
    cache: &mut Cache,
    mime: &str,
    base64: &str,
) -> Option<String> {
    if !is_image(mime)
        || !mime
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'+' | b'-' | b'.' | b'_'))
        || !(1..=UPLOAD_LIMIT).contains(&decoded_size(base64))
    {
        return None;
    }
    let clean: String = base64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = STANDARD
        .decode(&clean)
        .or_else(|_| STANDARD_NO_PAD.decode(&clean))
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    if let Some(url) = cache.get(&digest) {
        return Some(url.clone());
    }

    let mut target = Url::parse(&format!("{}{PATH}", service.origin)).ok()?;
    target
        .query_pairs_mut()
        .append_pair("request_id", &Uuid::new_v4().to_string());
    let boundary = format!("----9routerQoder{}", Uuid::new_v4().simple());
    let ext = extension(mime);
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"image.{ext}\"\r\nContent-Type: {mime}\r\n\r\n",
    ).into_bytes();
    body.extend(&bytes);
    body.extend(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = service
        .request(&target, Method::PUT, &body)
        .await
        .ok()?
        .header(header::ACCEPT, "application/json")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::CONTENT_LENGTH, body.len())
        .header("AI-CLIENT-TIMESTAMP", Utc::now().timestamp())
        .body(body);
    let response = send(request, METADATA_TIMEOUT)
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let parsed: Value = serde_json::from_slice(&bounded(response).await?).ok()?;
    let url = response_url(parsed, 0)?;
    cache.insert(digest, url.clone());
    Some(url)
}

fn response_url(json: Value, depth: u8) -> Option<String> {
    let result = json
        .get("result")
        .filter(|value| value.is_object())
        .unwrap_or(&json);
    let url = [result, json.as_object().map_or(&Value::Null, |_| &json)]
        .into_iter()
        .flat_map(|value| ["imageUrls", "image_urls"].map(|key| &value[key][0]))
        .find_map(text)
        .or_else(|| {
            [
                "imageUrl",
                "image_url",
                "url",
                "ossUrl",
                "oss_url",
                "originalUrl",
                "originUrl",
                "link",
                "image",
            ]
            .into_iter()
            .find_map(|key| {
                text(
                    result
                        .get(key)
                        .filter(|value| !value.is_null())
                        .unwrap_or(&json[key]),
                )
            })
        });
    if let Some(url) = url {
        return Some(url.to_owned());
    }
    if depth >= 8 {
        return None;
    }
    response_url(
        serde_json::from_str(json["body"].as_str()?).ok()?,
        depth + 1,
    )
}

fn block_url(block: &Value) -> Option<&str> {
    block["image_url"]
        .as_str()
        .or_else(|| block["image_url"]["url"].as_str())
}

fn image_url(url: &str) -> Value {
    json!({"type":"image_url","image_url":{"url":url}})
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

fn data_uri(url: &str) -> Option<(&str, &str)> {
    let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    (!mime.is_empty() && !mime.contains(';') && !data.is_empty()).then_some((mime, data))
}

fn is_image(mime: &str) -> bool {
    mime.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
}

fn extension(mime: &str) -> &'static str {
    let mime = mime.to_ascii_lowercase();
    ["png", "jpeg", "jpg", "gif", "webp", "bmp", "pdf"]
        .into_iter()
        .find(|ext| mime.contains(ext))
        .map_or("bin", |ext| if ext == "jpeg" { "jpg" } else { ext })
}

fn decoded_size(base64: &str) -> usize {
    base64
        .chars()
        .filter(|char| !char.is_whitespace())
        .count()
        .saturating_mul(3)
        / 4
}

fn omit(name: &str, bytes: usize, reason: &str) -> String {
    let size = if bytes == 0 {
        String::new()
    } else {
        format!(", {bytes} bytes")
    };
    format!("[file omitted: {name}{size} — {reason}]")
}

fn stub(name: &str, bytes: usize, reason: &str) -> Value {
    json!({"type":"text","text":omit(name,bytes,reason)})
}

fn strip_text(text: &str, oversized: bool) -> String {
    if !text.contains("data:") || (!oversized && text.encode_utf16().count() <= 8192) {
        return text.to_owned();
    }
    TEXT_DATA_URI
        .replace_all(text, |part: &regex::Captures<'_>| {
            let raw = &part[0];
            if oversized {
                omit(
                    "attachment",
                    raw.encode_utf16().count(),
                    "payload over Qoder size budget",
                )
            } else {
                let parsed = data_uri(raw.trim());
                let bytes = parsed.map_or_else(
                    || raw.encode_utf16().count(),
                    |(_, data)| decoded_size(data),
                );
                if bytes <= INLINE_LIMIT {
                    raw.to_owned()
                } else {
                    omit(
                        parsed.map_or("attachment", |(mime, _)| mime),
                        bytes,
                        "inlined data URI stripped from Qoder context",
                    )
                }
            }
        })
        .into_owned()
}
