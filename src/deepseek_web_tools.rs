use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;
use reqwest::StatusCode;
use serde_json::{Map, Value, json};

use crate::chatgpt_web::{parse_tool_object, signed_dsml};
use crate::provider_index::js_trim;

use super::Failure;

static TOOL_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<(tool_call|tool)(?::([A-Za-z0-9_-]+))?((?:\s[^<>]*)?)>")
        .expect("static DeepSeek tool tag")
});
static TOOL_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"</(tool_call|tool)\s*>").expect("static DeepSeek tool closing tag")
});
static TAG_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\A\s+(?:name|id)=(?:"([A-Za-z0-9_-]+)"|'([A-Za-z0-9_-]+)')\s*\z"#)
        .expect("static DeepSeek tool name attribute")
});
static DSML_WRAPPER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"</?(?:\|DSML\||｜｜DSML｜｜)\s*calls\s*>").expect("static DeepSeek DSML wrapper")
});

pub(super) struct ClientTools {
    declared: HashMap<String, ToolSchema>,
    nonce: String,
    manifest: String,
    parallel: bool,
}

impl ClientTools {
    pub(super) fn from_request(body: &Value) -> Result<Option<Self>, Failure> {
        let bad = |message| Failure::new(StatusCode::BAD_REQUEST, message);
        if body["tool_choice"] == "none" {
            return Ok(None);
        }
        if !body["functions"].is_null()
            && body["functions"]
                .as_array()
                .is_none_or(|items| !items.is_empty())
        {
            return Err(bad("DeepSeek Web requires function declarations in tools"));
        }
        if body["tools"].is_null() {
            return Ok(None);
        }
        let tools = body["tools"]
            .as_array()
            .ok_or_else(|| bad("Invalid DeepSeek Web tools"))?;
        let choice = &body["tool_choice"];
        let named = if choice.is_null() || matches!(choice.as_str(), Some("auto" | "required")) {
            None
        } else if choice["type"] == "function" {
            Some(
                choice["function"]["name"]
                    .as_str()
                    .ok_or_else(|| bad("Invalid DeepSeek Web named tool choice"))?,
            )
        } else {
            return Err(bad("Invalid DeepSeek Web tool choice"));
        };
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let options = jsonschema::draft202012::options()
            .should_validate_formats(false)
            .offline();
        let mut declared = HashMap::new();
        let mut definitions = Vec::new();
        for tool in tools {
            if !tool["type"].is_null() && tool["type"] != "function" {
                return Err(bad("DeepSeek Web supports client function tools"));
            }
            let function = &tool["function"];
            let name = function["name"]
                .as_str()
                .map(js_trim)
                .filter(|name| {
                    (1..=64).contains(&name.len())
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                })
                .ok_or_else(|| bad("Invalid DeepSeek Web tool name"))?;
            if named.is_some_and(|named| named != name) || declared.contains_key(name) {
                continue;
            }
            let schema = function.get("parameters");
            let mut line = format!("- {name}");
            if let Some(description) = function["description"]
                .as_str()
                .filter(|description| !description.is_empty())
            {
                line.push_str(": ");
                line.push_str(description);
            }
            if let Some(schema) = schema {
                let mut parameter_schema = schema.clone();
                if let Some(schema) = parameter_schema.as_object_mut() {
                    let mut properties = schema
                        .get("properties")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    properties.insert("_nonce".into(), json!({"type":"string","const":nonce}));
                    let mut required = schema
                        .get("required")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if !required.iter().any(|entry| entry == "_nonce") {
                        required.push(json!("_nonce"));
                    }
                    schema.insert("properties".into(), Value::Object(properties));
                    schema.insert("required".into(), Value::Array(required));
                }
                line.push_str("\n  parameters: ");
                line.push_str(&parameter_schema.to_string());
            }
            declared.insert(name.to_owned(), ToolSchema::compile(schema, &options));
            definitions.push(line);
        }
        if declared.is_empty() {
            return if named.is_some() || choice == "required" {
                Err(bad("DeepSeek Web tool choice requires a declared function"))
            } else {
                Ok(None)
            };
        }
        let manifest = format!(
            "You can call tools. To call a tool, output ONLY this exact block (no markdown fence):\n\
             <tool>{{\"name\": \"<tool_name>\", \"arguments\": {{ ... }}, \"_nonce\": \"{nonce}\"}}</tool>\n\
             Rules:\n\
             - Use exactly <tool>...</tool>.\n\
             - Include the secret binding \"_nonce\": \"{nonce}\" exactly as shown.\n\
             - \"name\" must be one of the tools below. \"arguments\" must be a JSON object.\n\
             - Emit one block per call. After tool results, answer normally.\n\
             - If your runtime forces DSML, every invoke must include this parameter:\n\
             <｜｜DSML｜｜ parameter name=\"_nonce\" string=\"true\">{nonce}</｜｜DSML｜｜ parameter>\n\
             \nAvailable tools:\n{}",
            definitions.join("\n")
        );
        Ok(Some(Self {
            declared,
            nonce,
            manifest,
            parallel: body["parallel_tool_calls"] != false,
        }))
    }

    pub(super) fn manifest(&self) -> &str {
        &self.manifest
    }

    pub(super) fn redact(&self, value: &str) -> String {
        value.replace(&self.nonce, "[redacted]")
    }

    pub(super) fn parse(&self, text: &str) -> (String, Vec<Value>) {
        let mut candidates = self.text_calls(text);
        candidates.extend(
            signed_dsml(text, &self.nonce)
                .map(|(range, name, args)| (range, name.to_owned(), args)),
        );
        candidates.sort_unstable_by_key(|candidate| candidate.0.start);
        let mut content = String::new();
        let mut cursor = 0;
        let mut calls = Vec::new();
        for (range, name, arguments) in candidates {
            if range.start < cursor {
                continue;
            }
            let Some(schema) = self.declared.get(&name) else {
                continue;
            };
            if !schema.accepts(&arguments) {
                continue;
            }
            let arguments = arguments.to_string();
            if arguments.contains(&self.nonce) {
                continue;
            }
            content.push_str(&text[cursor..range.start]);
            cursor = range.end;
            calls.push(
                json!({"id":format!("call_{}", uuid::Uuid::new_v4()),"type":"function",
                "function":{"name":name,"arguments":arguments}}),
            );
            if !self.parallel {
                break;
            }
        }
        content.push_str(&text[cursor..]);
        if !calls.is_empty() {
            content = js_trim(&DSML_WRAPPER.replace_all(&content, "")).to_owned();
        }
        (self.redact(&content), calls)
    }

    fn text_calls(&self, text: &str) -> Vec<(Range<usize>, String, Value)> {
        let mut calls = Vec::new();
        let mut position = 0;
        while let Some(opening) = TOOL_TAG.captures_at(text, position) {
            let (Some(open), Some(kind)) = (opening.get(0), opening.get(1)) else {
                break;
            };
            let Some(closing) = TOOL_END.captures_at(text, open.end()) else {
                break;
            };
            let (Some(close), Some(closing_kind)) = (closing.get(0), closing.get(1)) else {
                break;
            };
            position = close.end();
            if kind.as_str() != closing_kind.as_str() {
                continue;
            }
            let attrs = opening.get(3).map_or("", |attrs| attrs.as_str());
            let suffix = opening.get(2).map(|name| name.as_str());
            let attribute = TAG_NAME
                .captures(attrs)
                .and_then(|attrs| attrs.get(1).or_else(|| attrs.get(2)))
                .map(|value| value.as_str());
            if attribute.is_none() && !js_trim(attrs).is_empty()
                || suffix
                    .zip(attribute)
                    .is_some_and(|(suffix, attribute)| suffix != attribute)
            {
                continue;
            }
            let tagged = suffix.or(attribute);
            let raw = js_trim(&text[open.end()..close.start()]);
            let Some(parsed) = parse_tool_object(raw).or_else(|| parse_xml_call(raw)) else {
                continue;
            };
            let name = parsed["name"]
                .as_str()
                .or_else(|| parsed["type"].as_str())
                .filter(|name| !name.is_empty());
            let Some(name) = name.or(tagged) else {
                continue;
            };
            if tagged.is_some_and(|tagged| tagged != name) {
                continue;
            }
            let mut arguments = match parsed.get("arguments").or_else(|| parsed.get("params")) {
                Some(Value::Object(args)) => args.clone(),
                Some(Value::String(raw)) => {
                    match parse_tool_object(raw).and_then(|args| args.as_object().cloned()) {
                        Some(args) => args,
                        None => continue,
                    }
                }
                Some(_) => continue,
                None => {
                    let Some(mut args) = parsed.as_object().cloned() else {
                        continue;
                    };
                    for key in ["name", "type", "id", "_nonce"] {
                        args.shift_remove(key);
                    }
                    args
                }
            };
            let nonce = parsed.get("_nonce");
            let nested = arguments.shift_remove("_nonce");
            if nonce.is_none() && nested.is_none()
                || nonce.is_some_and(|nonce| nonce != &self.nonce)
                || nested.is_some_and(|nonce| nonce != self.nonce)
            {
                continue;
            }
            calls.push((
                open.start()..close.end(),
                name.to_owned(),
                Value::Object(arguments),
            ));
        }
        calls
    }
}

fn parse_xml_call(raw: &str) -> Option<Value> {
    if !raw.starts_with('<') {
        return None;
    }
    let document = format!("<call>{raw}</call>");
    let parsed = roxmltree::Document::parse(&document).ok()?;
    let mut envelope = Map::new();
    let mut parameters = Map::new();
    for node in parsed.root_element().children() {
        if node.is_text() && node.text().is_some_and(|text| js_trim(text).is_empty()) {
            continue;
        }
        if !node.is_element() || !node.children().all(|child| child.is_text()) {
            return None;
        }
        let raw = node.text().map(js_trim).unwrap_or_default();
        let tag = node.tag_name().name();
        let (field, value) = match tag {
            "name" | "_nonce" => (tag, json!(raw)),
            "arguments" | "parameters" => ("arguments", parse_tool_object(raw)?),
            "parameter" => {
                let name = node.attribute("name")?;
                let raw = node.attribute("content").unwrap_or(raw);
                let value = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!(raw));
                if parameters.insert(name.to_owned(), value).is_some() {
                    return None;
                }
                continue;
            }
            _ => return None,
        };
        if envelope.insert(field.to_owned(), value).is_some() {
            return None;
        }
    }
    if !parameters.is_empty()
        && envelope
            .insert("arguments".into(), Value::Object(parameters))
            .is_some()
    {
        return None;
    }
    Some(Value::Object(envelope))
}

enum ToolSchema {
    Unconstrained,
    Compiled(jsonschema::Validator),
    Invalid,
}

impl ToolSchema {
    fn compile(schema: Option<&Value>, options: &jsonschema::ValidationOptions<'_>) -> Self {
        let Some(schema) = schema else {
            return Self::Unconstrained;
        };
        let mut schema = schema.clone();
        if !normalize_nullable(&mut schema) {
            return Self::Invalid;
        }
        match options.build(&schema) {
            Ok(validator) => Self::Compiled(validator),
            Err(_) => Self::Invalid,
        }
    }

    fn accepts(&self, arguments: &Value) -> bool {
        arguments.is_object()
            && match self {
                Self::Unconstrained => true,
                Self::Compiled(validator) => validator.is_valid(arguments),
                Self::Invalid => false,
            }
    }
}

fn normalize_nullable(schema: &mut Value) -> bool {
    let Some(fields) = schema.as_object_mut() else {
        return schema.is_boolean();
    };
    if let Some(nullable) = fields.shift_remove("nullable") {
        let (Some(nullable), Some(kind)) = (nullable.as_bool(), fields.get_mut("type")) else {
            return false;
        };
        let includes_null = kind.as_str() == Some("null")
            || kind
                .as_array()
                .is_some_and(|types| types.iter().any(|kind| kind == "null"));
        if !nullable && includes_null {
            return false;
        }
        if nullable && !includes_null {
            if let Some(types) = kind.as_array_mut() {
                if types.is_empty() {
                    return false;
                }
                types.push(json!("null"));
            } else if kind.is_string() {
                *kind = json!([kind.take(), "null"]);
            } else {
                return false;
            }
        }
    }
    for key in [
        "$defs",
        "definitions",
        "properties",
        "patternProperties",
        "dependentSchemas",
    ] {
        if let Some(children) = fields.get_mut(key).and_then(Value::as_object_mut)
            && !children.values_mut().all(normalize_nullable)
        {
            return false;
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = fields.get_mut(key).and_then(Value::as_array_mut)
            && !children.iter_mut().all(normalize_nullable)
        {
            return false;
        }
    }
    for key in [
        "additionalProperties",
        "unevaluatedProperties",
        "propertyNames",
        "items",
        "contains",
        "unevaluatedItems",
        "if",
        "then",
        "else",
        "not",
        "contentSchema",
    ] {
        if let Some(child) = fields.get_mut(key)
            && !normalize_nullable(child)
        {
            return false;
        }
    }
    true
}
