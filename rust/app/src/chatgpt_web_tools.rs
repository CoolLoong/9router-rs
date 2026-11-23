use std::collections::HashSet;
use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::chat_input::truthy;
use crate::provider_index::js_trim;

use super::Failure;

static TOOL: LazyLock<Regex> = LazyLock::new(|| Regex::new("<tool>").expect("static tool tag"));
static TOOL_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<tool_call(?:\s[^<>]*)?>").expect("static alternate tool tag"));
static DSML_INVOKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<(\|DSML\||｜｜DSML｜｜)\s*invoke\s+name=(?:"([^"'<>]+)"|'([^"'<>]+)')\s*>"#)
        .expect("static DSML invoke tag")
});
static DSML_INVOKE_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"</(\|DSML\||｜｜DSML｜｜)\s*invoke\s*>").expect("static DSML invoke closing tag")
});
static DSML_PARAMETER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\A<(\|DSML\||｜｜DSML｜｜)\s*parameter\s+name=(?:"([^"'<>]+)"|'([^"'<>]+)')(?:\s+string=(?:"(?:true|false)"|'(?:true|false)'))?\s*>"#)
        .expect("static DSML parameter tag")
});
static DSML_PARAMETER_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"</(\|DSML\||｜｜DSML｜｜)\s*parameter\s*>")
        .expect("static DSML parameter closing tag")
});

pub(super) struct ClientTools<'a> {
    definitions: Vec<(&'a str, &'a Value)>,
    names: HashSet<&'a str>,
    nonce: String,
}

impl<'a> ClientTools<'a> {
    pub(super) fn from_request(body: &'a Value) -> Result<Option<Self>, Failure> {
        if !body["functions"].is_null()
            && body["functions"]
                .as_array()
                .is_none_or(|tools| !tools.is_empty())
        {
            return Err(Failure::bad(
                "ChatGPT Web requires function declarations in tools",
            ));
        }
        if body["tools"].is_null() || body["tool_choice"] == "none" {
            return Ok(None);
        }
        let tools = body["tools"]
            .as_array()
            .ok_or_else(|| Failure::bad("Invalid ChatGPT Web tool declarations"))?;
        if tools.is_empty() {
            return Ok(None);
        }
        let choice = &body["tool_choice"];
        let chosen = if choice.is_null() || matches!(choice.as_str(), Some("auto" | "required")) {
            None
        } else if choice["type"] == "function" {
            Some(
                choice["function"]["name"]
                    .as_str()
                    .ok_or_else(|| Failure::bad("Invalid ChatGPT Web named tool choice"))?,
            )
        } else {
            return Err(Failure::bad("Invalid ChatGPT Web tool choice"));
        };
        let mut definitions = Vec::new();
        let mut names = HashSet::new();
        for tool in tools {
            if !tool["type"].is_null() && tool["type"] != "function" {
                return Err(Failure::bad("ChatGPT Web supports client function tools"));
            }
            let definition = &tool["function"];
            let name = definition["name"]
                .as_str()
                .map(js_trim)
                .filter(|name| {
                    (1..=64).contains(&name.len())
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
                .ok_or_else(|| Failure::bad("Invalid ChatGPT Web function name"))?;
            if chosen.is_none_or(|chosen| chosen == name) && names.insert(name) {
                definitions.push((name, definition));
            }
        }
        if definitions.is_empty() {
            return Err(Failure::bad("Chosen ChatGPT Web tool was not declared"));
        }
        Ok(Some(Self {
            definitions,
            names,
            nonce: uuid::Uuid::new_v4().simple().to_string(),
        }))
    }

    pub(super) fn contract(&self) -> String {
        let nonce = &self.nonce;
        let mut contract = format!(
            "The client application provides tools beyond your built-in ones. They are NOT in your native tool registry; they are invoked via a plain-text protocol: the client parses your reply and executes the tool on the user machine. Treat these client tools as fully available to you; never claim they are unavailable. To invoke one, reply with a single line containing a <tool> block\nwith JSON that includes the secret binding \"_nonce\": \"{nonce}\":\n<tool>{{\"name\": \"<tool_name>\", \"arguments\": {{ ... }}, \"_nonce\": \"{nonce}\"}}</tool>\nThese client tools ARE available to you in this conversation. Only emit the <tool> block when you actually want to call a tool; otherwise answer normally.\n\nAvailable tools:"
        );
        for (name, tool) in &self.definitions {
            contract.push_str(&format!("\n- {name}"));
            if let Some(description) = tool["description"].as_str().filter(|text| !text.is_empty())
            {
                contract.push_str(": ");
                contract.push_str(description);
            }
            if truthy(&tool["parameters"]) {
                contract.push_str("\n  parameters: ");
                contract.push_str(&tool["parameters"].to_string());
            }
        }
        contract
    }

    pub(super) fn reminder(&self) -> String {
        let names = self
            .definitions
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "\n\n[Client protocol reminder: the client-tool contract in the system instructions is active in this conversation. These client tools ARE available via the <tool> block protocol: {names}.]"
        )
    }

    pub(super) fn redact(&self, text: String) -> String {
        if text.contains(&self.nonce) {
            text.replace(&self.nonce, "[redacted]")
        } else {
            text
        }
    }

    pub(super) fn calls(&self, text: &str) -> Vec<Value> {
        let mut accepted: Vec<_> = blocks(text, &TOOL, "</tool>")
            .chain(blocks(text, &TOOL_CALL, "</tool_call>"))
            .filter_map(|(offset, payload)| {
                let parsed = envelope(payload)?;
                let name = parsed["name"]
                    .as_str()
                    .or_else(|| parsed["command"].as_str())?;
                if !self.names.contains(name) || parsed["_nonce"] != self.nonce {
                    return None;
                }
                let arguments = match parsed.get("arguments") {
                    None => "{}".to_owned(),
                    Some(Value::String(raw)) => envelope(raw)
                        .filter(Value::is_object)
                        .map_or_else(|| raw.clone(), |value| value.to_string()),
                    Some(value) => value.to_string(),
                };
                if arguments.contains(&self.nonce) {
                    return None;
                }
                Some((offset, json!({"name":name,"arguments":arguments})))
            })
            .collect();
        accepted.extend(
            signed_dsml(text, &self.nonce).filter_map(|(range, name, arguments)| {
                let arguments = arguments.to_string();
                (self.names.contains(name) && !arguments.contains(&self.nonce))
                    .then(|| (range.start, json!({"name":name,"arguments":arguments})))
            }),
        );
        accepted.sort_unstable_by_key(|(offset, _)| *offset);
        let seed = chrono::Utc::now().timestamp_millis();
        accepted
            .into_iter()
            .enumerate()
            .map(|(index, (_, function))| {
                json!({"id":format!("cgpt-{seed}_{index}"),"type":"function","function":function})
            })
            .collect()
    }
}

pub(crate) fn signed_dsml<'text>(
    text: &'text str,
    nonce: &str,
) -> impl Iterator<Item = (Range<usize>, &'text str, Value)> {
    let mut position = 0;
    std::iter::from_fn(move || {
        loop {
            let opening = DSML_INVOKE.captures_at(text, position)?;
            let tag = opening.get(0)?;
            let ending = DSML_INVOKE_END.captures_at(text, tag.end())?;
            let end = ending.get(0)?;
            position = end.end();
            let marker = opening.get(1)?.as_str();
            let name = opening.get(2).or_else(|| opening.get(3))?.as_str();
            if ending.get(1)?.as_str() != marker {
                continue;
            }
            if let Some(arguments) = dsml_arguments(&text[tag.end()..end.start()], marker, nonce) {
                return Some((tag.start()..end.end(), name, arguments));
            }
        }
    })
}

fn dsml_arguments(body: &str, marker: &str, nonce: &str) -> Option<Value> {
    let mut body = js_trim(body);
    let mut arguments = Map::new();
    let mut bound = false;
    while !body.is_empty() {
        let opening = DSML_PARAMETER.captures(body)?;
        if opening.get(1)?.as_str() != marker {
            return None;
        }
        let name = opening.get(2).or_else(|| opening.get(3))?.as_str();
        let tail = &body[opening.get(0)?.end()..];
        let ending = DSML_PARAMETER_END.captures(tail)?;
        if ending.get(1)?.as_str() != marker {
            return None;
        }
        let end = ending.get(0)?;
        let raw = js_trim(&tail[..end.start()]);
        if raw.contains("<|DSML|") || raw.contains("<｜｜DSML｜｜") {
            return None;
        }
        if name == "_nonce" {
            if bound
                || (raw != nonce
                    && serde_json::from_str::<String>(raw).ok().as_deref() != Some(nonce))
            {
                return None;
            }
            bound = true;
        } else {
            let value = serde_json::from_str::<Value>(raw)
                .unwrap_or_else(|_| Value::String(raw.to_owned()));
            if arguments.insert(name.to_owned(), value).is_some() {
                return None;
            }
        }
        body = js_trim(&tail[end.end()..]);
    }
    bound.then_some(Value::Object(arguments))
}

fn blocks<'a>(
    text: &'a str,
    open: &'a Regex,
    close: &'static str,
) -> impl Iterator<Item = (usize, &'a str)> {
    let mut position = 0;
    std::iter::from_fn(move || {
        let opening = open.find_at(text, position)?;
        let tail = &text[opening.end()..];
        let end = tail.find(close)?;
        position = opening.end() + end + close.len();
        Some((opening.start(), &tail[..end]))
    })
}

pub(crate) fn envelope(raw: &str) -> Option<Value> {
    let mut raw = js_trim(raw);
    if let Some(fenced) = raw
        .strip_prefix("```")
        .and_then(|text| text.strip_suffix("```"))
    {
        let fenced = ["json", "javascript", "js", "python"]
            .into_iter()
            .find_map(|language| {
                fenced
                    .get(..language.len())
                    .filter(|prefix| prefix.eq_ignore_ascii_case(language))
                    .map(|_| &fenced[language.len()..])
            })
            .unwrap_or(fenced);
        raw = js_trim(fenced);
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value.is_object().then_some(value),
        Err(_) => serde_json::from_slice::<Value>(&normalize_loose_json(raw.as_bytes())?)
            .ok()
            .filter(Value::is_object),
    }
}

fn normalize_loose_json(input: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0;
    let mut previous = 0;
    while let Some(&byte) = input.get(cursor) {
        match byte {
            b'"' | b'\'' => {
                cursor = write_quoted(input, cursor, &mut output)?;
                previous = b'"';
            }
            b',' if matches!(
                input[cursor + 1..].trim_ascii_start().first(),
                Some(b'}' | b']')
            ) =>
            {
                cursor += 1;
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let end = input[cursor + 1..]
                    .iter()
                    .position(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-'))
                    .map_or(input.len(), |offset| cursor + 1 + offset);
                let token = &input[cursor..end];
                if matches!(previous, b'{' | b',')
                    && input[end..].trim_ascii_start().first() == Some(&b':')
                {
                    output.push(b'"');
                    output.extend_from_slice(token);
                    output.push(b'"');
                } else {
                    output.extend_from_slice(match token {
                        b"True" => b"true",
                        b"False" => b"false",
                        b"None" => b"null",
                        _ => token,
                    });
                }
                cursor = end;
                previous = b'"';
            }
            byte => {
                output.push(byte);
                if !byte.is_ascii_whitespace() {
                    previous = byte;
                }
                cursor += 1;
            }
        }
    }
    Some(output)
}

fn write_quoted(input: &[u8], start: usize, output: &mut Vec<u8>) -> Option<usize> {
    let quote = input[start];
    let mut cursor = start + 1;
    output.push(b'"');
    while let Some(&byte) = input.get(cursor) {
        cursor += 1;
        match byte {
            byte if byte == quote => {
                output.push(b'"');
                return Some(cursor);
            }
            b'\\' => {
                let &escaped = input.get(cursor)?;
                if quote != b'\'' || escaped != b'\'' {
                    output.push(b'\\');
                }
                output.push(escaped);
                cursor += 1;
            }
            b'"' => output.extend_from_slice(b"\\\""),
            byte => output.push(byte),
        }
    }
    None
}
