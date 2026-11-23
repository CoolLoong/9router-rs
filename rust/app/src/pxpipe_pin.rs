use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::text_block;
use crate::cli::js_space;

const OPEN: &str = "<system-reminder>";
const CLOSE: &str = "</system-reminder>";
const REPLY: &str = "@pxpipe ";
const MAX_PIN: usize = 300;
const MAX_TOTAL: usize = 2000;

static COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:>[ \t]*)?@?pxpipe[ \t]+(pin|unpin)(?-u:\b)([^\r\n\u{2028}\u{2029}]*)$")
        .unwrap()
});

pub(super) struct Pin {
    text: String,
    file: bool,
}

pub(super) fn prepare(body: &mut Value) -> Vec<Pin> {
    let Some(messages) = body["messages"].as_array() else {
        return Vec::new();
    };
    if !messages.last().is_some_and(|last| {
        last["role"] == "user" && (last["content"].is_string() || last["content"].is_array())
    }) {
        return Vec::new();
    }
    let text = text_blocks(&body["system"]).chain(
        messages
            .iter()
            .filter(|message| message["role"] == "user")
            .flat_map(|message| text_blocks(&message["content"])),
    );
    if !text.into_iter().any(has_command) {
        return Vec::new();
    }
    let mut rules = Vec::new();
    for text in text_blocks(&body["system"]) {
        apply_lines(&mut rules, text, true);
    }
    for (index, msg) in messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message["role"] == "user")
    {
        let mut leading = index == 0;
        for text in text_blocks(&msg["content"]) {
            let mut remainder = text;
            while let Some(end) = reminder_end(remainder) {
                apply_lines(&mut rules, &remainder[..end], leading);
                remainder = &remainder[end..];
            }
            if remainder.len() == text.len() || !remainder.trim_matches(js_space).is_empty() {
                leading = false;
                apply_lines(&mut rules, remainder, false);
            }
        }
    }
    body["messages"] = Value::Array(strip_messages(messages));
    if let Some(changed) = strip_body_system(&body["system"]) {
        body["system"] = changed;
    }
    rules
}

pub(super) fn append(messages: &mut [Value], pins: &[Pin]) {
    let Some(last) = messages.last_mut().filter(|last| last["role"] == "user") else {
        return;
    };
    let mut remaining = MAX_TOTAL;
    let rules = pins
        .iter()
        .filter_map(|pin| {
            remaining = remaining.checked_sub(pin.text.encode_utf16().count())?;
            Some(if pin.file {
                pin.text.clone()
            } else {
                format!("- {}", pin.text)
            })
        })
        .collect::<Vec<_>>();
    if rules.is_empty() {
        return;
    }
    let text = format!(
        "{OPEN}\n[pxpipe pin] The user pinned these instructions and pxpipe relocated them here,\nlast in the request, because rules stated far above get read as background. They\nare the user's own words and they govern this reply; on conflict they win.\n{}\n{CLOSE}",
        rules.join("\n")
    );
    if let Some(content) = last["content"].as_str() {
        last["content"] = Value::Array(vec![text_block(content)]);
    }
    if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
        content.push(text_block(&text));
    }
}

fn text_blocks(value: &Value) -> impl Iterator<Item = &str> {
    value.as_str().into_iter().chain(
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str()),
    )
}

fn reminder_end(text: &str) -> Option<usize> {
    let trimmed = text.trim_start_matches(js_space);
    let body = trimmed.strip_prefix(OPEN)?;
    let close = body.find(CLOSE)?;
    Some(text.len() - body.len() + close + CLOSE.len())
}

fn parse(line: &str) -> Option<(&str, &str)> {
    let matches = COMMAND.captures(line.trim_matches(js_space))?;
    Some((
        matches.get(1)?.as_str(),
        matches.get(2)?.as_str().trim_matches(js_space),
    ))
}

fn apply_lines(rules: &mut Vec<Pin>, text: &str, file: bool) {
    for line in text.split('\n') {
        let Some((verb, raw)) = parse(line) else {
            continue;
        };
        if verb == "unpin" {
            remove(rules, raw);
            continue;
        }
        let string = if raw.encode_utf16().count() > MAX_PIN {
            format!("{}… [pxpipe: pin truncated]", prefix(raw, MAX_PIN))
        } else {
            raw.to_owned()
        };
        let skip = if file {
            string.is_empty() && rules.last().is_none_or(|pin| pin.text.is_empty())
        } else {
            string.is_empty() || rules.iter().any(|pin| pin.text == string)
        };
        if !skip {
            rules.push(Pin { text: string, file });
        }
    }
}

fn remove(rules: &mut Vec<Pin>, what: &str) {
    if what.is_empty() {
        return;
    }
    if what == "all" {
        rules.retain(|pin| pin.file);
        return;
    }
    let session = || rules.iter().enumerate().filter(|(_, pin)| !pin.file);
    let index = if what.bytes().all(|ch| ch.is_ascii_digit()) {
        what.parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|offset| session().nth(offset))
            .map(|(index, _)| index)
    } else {
        let lowered = what.to_lowercase();
        session()
            .find(|(_, pin)| pin.text == what)
            .or_else(|| session().find(|(_, pin)| pin.text.to_lowercase().starts_with(&lowered)))
            .map(|(index, _)| index)
    };
    if let Some(index) = index {
        rules.remove(index);
    }
}

fn strip_messages(messages: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    let mut iter = messages.iter().peekable();
    while let Some(message) = iter.next() {
        if message["role"] != "user" {
            out.push(message.clone());
            continue;
        }
        let next = iter.peek();
        if next.is_some_and(|next| confirmation(next)) && command_only(message) {
            iter.next();
            continue;
        }
        match strip_content(&message["content"], false) {
            Some(stripped) if empty_content(&stripped) => {
                if next.is_some_and(|next| next["role"] == "assistant") {
                    iter.next();
                } else {
                    out.push(message.clone());
                }
            }
            Some(content) => {
                let mut updated = message.clone();
                updated["content"] = content;
                out.push(updated);
            }
            None => out.push(message.clone()),
        }
    }
    out
}

fn empty_content(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| text.trim_matches(js_space).is_empty())
        || value.as_array().is_some_and(Vec::is_empty)
}

fn confirmation(msg: &Value) -> bool {
    let content = &msg["content"];
    msg["role"] == "assistant"
        && (content.as_str().is_some_and(|text| text.starts_with(REPLY))
            || content.as_array().is_some_and(|blocks| {
                let [block] = blocks.as_slice() else {
                    return false;
                };
                block["type"] == "text"
                    && block["text"]
                        .as_str()
                        .is_some_and(|text| text.starts_with(REPLY))
            }))
}

fn command_only(message: &Value) -> bool {
    let content = &message["content"];
    if content.as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|block| block["type"] != "text" || !block["text"].is_string())
    }) {
        return false;
    }
    let mut seen = false;
    for text in text_blocks(content) {
        if reminder_end(text).is_some() {
            return false;
        }
        let stripped = strip_reminders(text);
        for line in stripped
            .split('\n')
            .filter(|line| !line.trim_matches(js_space).is_empty())
        {
            if parse(line).is_none() {
                return false;
            }
            seen = true;
        }
    }
    seen
}

fn strip_reminders(text: &str) -> String {
    let mut kept = String::new();
    let mut remaining = text;
    while let Some(open) = remaining.find(OPEN) {
        let rest = &remaining[open + OPEN.len()..];
        let Some(close) = rest.find(CLOSE) else {
            break;
        };
        kept.push_str(&remaining[..open]);
        remaining = &rest[close + CLOSE.len()..];
    }
    kept.push_str(remaining);
    kept
}

fn strip_body_system(value: &Value) -> Option<Value> {
    strip_content(value, true)
}

fn strip_content(value: &Value, system: bool) -> Option<Value> {
    if let Some(text) = value.as_str() {
        return match strip_lines(text) {
            Cow::Borrowed(_) => None,
            Cow::Owned(text) => Some(Value::String(text)),
        };
    }
    let blocks = value.as_array()?;
    let mut output: Vec<Value> = Vec::with_capacity(blocks.len());
    let mut changed = false;
    for block in blocks {
        let text = block["text"].as_str().filter(|_| block["type"] == "text");
        let Some(Cow::Owned(clean)) = text.map(strip_lines) else {
            output.push(block.clone());
            continue;
        };
        changed = true;
        if clean.trim_matches(js_space).is_empty() {
            if let Some(marker) = block.get("cache_control") {
                if let Some(prior) = output
                    .last_mut()
                    .filter(|prior| !system || prior["type"] == "text")
                {
                    prior["cache_control"] = marker.clone();
                } else {
                    output.push(block.clone());
                }
            }
        } else {
            let mut block = block.clone();
            block["text"] = Value::String(clean);
            output.push(block);
        }
    }
    changed.then_some(Value::Array(output))
}

fn strip_lines(text: &str) -> Cow<'_, str> {
    if has_command(text) {
        Cow::Owned(
            text.split('\n')
                .filter(|line| parse(line).is_none())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        Cow::Borrowed(text)
    }
}

fn has_command(text: &str) -> bool {
    text.contains("pxpipe") && text.split('\n').any(|line| parse(line).is_some())
}

fn prefix(text: &str, max: usize) -> &str {
    let mut chars = 0;
    for (index, ch) in text.char_indices() {
        chars += ch.len_utf16();
        if chars > max {
            return &text[..index];
        }
    }
    text
}
