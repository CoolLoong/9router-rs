use provider_capabilities_rs::port::model_capabilities;
use serde_json::{Value, json};

use crate::chat_format::ChatFormat;

pub fn strip_unsupported(
    body: &mut Value,
    source: ChatFormat,
    provider: &str,
    model: &str,
    catalog: Option<&Value>,
) {
    let caps = model_capabilities(Some(provider), model, catalog);
    if !["vision", "audioInput", "pdf"]
        .iter()
        .any(|key| caps[*key] == false)
    {
        return;
    }
    let field = if source == ChatFormat::Responses {
        "input"
    } else {
        "messages"
    };
    let Some(messages) = body.get_mut(field).and_then(Value::as_array_mut) else {
        return;
    };
    let last = messages.len().saturating_sub(1);
    for (index, message) in messages.iter_mut().enumerate() {
        if source == ChatFormat::Chat
            && caps["vision"] == false
            && let Some(fields) = message.as_object_mut()
        {
            if fields.get("images").is_some_and(Value::is_array) {
                fields.shift_remove("images");
            }
            for field in ["attachments", "experimental_attachments"] {
                if let Some(items) = fields.get_mut(field).and_then(Value::as_array_mut) {
                    items.retain(|item| {
                        !item["contentType"]
                            .as_str()
                            .is_some_and(|mime| mime.starts_with("image/"))
                            && !item["url"]
                                .as_str()
                                .is_some_and(|url| url.starts_with("data:image/"))
                    });
                }
            }
        }
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut removed = Vec::new();
        blocks.retain(|block| {
            let capability = match (source, block["type"].as_str()) {
                (ChatFormat::Chat, Some("image_url" | "image"))
                | (ChatFormat::Messages, Some("image"))
                | (ChatFormat::Responses, Some("input_image")) => "vision",
                (ChatFormat::Chat, Some("input_audio" | "audio_url")) => "audioInput",
                (ChatFormat::Chat, Some("file"))
                | (ChatFormat::Messages, Some("document"))
                | (ChatFormat::Responses, Some("input_file")) => "pdf",
                _ => return true,
            };
            if caps[capability] != false {
                return true;
            }
            if !removed.contains(&capability) {
                removed.push(capability);
            }
            false
        });
        let typ = if source == ChatFormat::Responses {
            "input_text"
        } else {
            "text"
        };
        for capability in removed {
            let text = match (capability, index == last) {
                ("vision", true) => "[image omitted: model has no vision support]",
                ("vision", false) => "[Previous image omitted from context.]",
                ("audioInput", true) => "[audio omitted: model has no audio support]",
                ("audioInput", false) => "[Previous audio omitted from context.]",
                (_, true) => "[file omitted: model has no document support]",
                _ => "[Previous file omitted from context.]",
            };
            blocks.push(json!({"type":typ,"text":text}));
        }
    }
}

pub(super) fn supported(
    body: &Value,
    source: ChatFormat,
    target: Option<ChatFormat>,
    provider: &str,
    model: &str,
    catalog: Option<&Value>,
) -> bool {
    let mut vision = None;
    for field in ["messages", "input"] {
        for message in body[field].as_array().into_iter().flatten() {
            let format = if field == "input" {
                ChatFormat::Responses
            } else {
                source
            };
            let content = &message["content"];
            let blocks = content.as_array().map(Vec::as_slice).unwrap_or_else(|| {
                if content.is_object() {
                    std::slice::from_ref(content)
                } else {
                    &[]
                }
            });
            for block in blocks {
                let block_type = block["type"].as_str();
                if !matches!(block_type, Some("image" | "image_url" | "input_image")) {
                    continue;
                }
                let tool_for_claude = format == ChatFormat::Chat
                    && target == Some(ChatFormat::Messages)
                    && message["role"] == "tool";
                if !(message["role"] == "user" || tool_for_claude)
                    || !*vision.get_or_insert_with(|| {
                        model_capabilities(Some(provider), model, catalog)["vision"] == true
                    })
                {
                    return false;
                }
                let accepted = match (format, block_type) {
                    (ChatFormat::Chat, Some("image_url")) if content.is_array() => {
                        block["image_url"]["url"].as_str().is_some_and(|url| {
                            if target == Some(ChatFormat::Messages) {
                                inline_url(url)
                            } else {
                                inline_url(url) || remote_url(url)
                            }
                        })
                    }
                    (ChatFormat::Messages, Some("image")) => {
                        let data = &block["source"];
                        data["type"] == "base64"
                            && data["media_type"]
                                .as_str()
                                .is_some_and(|mime| mime.starts_with("image/"))
                            && data["data"].as_str().is_some_and(|data| !data.is_empty())
                    }
                    (ChatFormat::Responses, Some("input_image")) if content.is_array() => {
                        block["image_url"].as_str().is_some_and(|url| {
                            inline_url(url)
                                || target != Some(ChatFormat::Messages) && remote_url(url)
                        }) || (target == Some(ChatFormat::Responses)
                            && block["file_id"].as_str().is_some_and(|id| !id.is_empty()))
                    }
                    _ => false,
                };
                if !accepted {
                    return false;
                }
            }
        }
    }
    true
}

fn inline_url(url: &str) -> bool {
    url.strip_prefix("data:image/")
        .and_then(|rest| rest.split_once(";base64,"))
        .is_some_and(|(subtype, data)| !subtype.is_empty() && !data.is_empty())
}

fn remote_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}
