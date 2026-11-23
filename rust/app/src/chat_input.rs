use serde_json::{Value, json};

const THINKING_FIELDS: &[&str] = &[
    "thinking",
    "reasoning_effort",
    "reasoning",
    "thinkingConfig",
    "enable_thinking",
    "thinking_budget",
    "output_config",
];

pub struct PreparedChat {
    pub body: Value,
    pub stream: bool,
}

#[derive(Default)]
struct Preparation {
    translated: bool,
    schema_fallback: bool,
    force_stream: bool,
    preserve_cache_control: bool,
}

pub fn prepare(body: Value, model: &str, accept: &str) -> Option<PreparedChat> {
    prepare_inner(
        body,
        model,
        accept,
        Preparation {
            schema_fallback: true,
            ..Preparation::default()
        },
    )
}

pub fn prepare_translated(body: Value, model: &str, accept: &str) -> Option<PreparedChat> {
    prepare_inner(
        body,
        model,
        accept,
        Preparation {
            translated: true,
            schema_fallback: true,
            ..Preparation::default()
        },
    )
}

pub fn prepare_static(
    body: Value,
    model: &str,
    accept: &str,
    translated: bool,
    force_stream: bool,
    preserve_cache_control: bool,
) -> Option<PreparedChat> {
    prepare_inner(
        body,
        model,
        accept,
        Preparation {
            translated,
            force_stream,
            preserve_cache_control,
            ..Preparation::default()
        },
    )
}

pub(super) fn stream_requested(body: &Value, accept: &str) -> bool {
    body["stream"] != false
        && !(body["stream"] != true
            && accept.contains("application/json")
            && !accept.contains("text/event-stream"))
}

fn prepare_inner(
    mut body: Value,
    model: &str,
    accept: &str,
    options: Preparation,
) -> Option<PreparedChat> {
    if (!options.translated && !is_openai_chat(&body)) || has_thinking(&body, model) {
        return None;
    }
    let stream = options.force_stream || stream_requested(&body, accept);
    let mut messages = std::mem::take(body["messages"].as_array_mut()?);
    if messages.iter().any(unsupported_media) {
        return None;
    }
    if !options.translated {
        preprocess_tool_history(&mut messages)?;
    }

    let mut normalized = Vec::with_capacity(messages.len());
    for message in messages {
        if let Some(msg) = filter_message(message, options.preserve_cache_control, false)? {
            normalized.push(msg);
        }
    }
    body["messages"] = Value::Array(normalized);
    normalize_tools(&mut body)?;
    body["model"] = json!(model);
    strip_responses_translation_fields(&mut body);
    let object = body.as_object_mut()?;

    let lower = model.to_ascii_lowercase();
    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if message["role"] == "assistant"
                && (lower.contains("deepseek")
                    || (lower.starts_with("kimi-")
                        && message["tool_calls"]
                            .as_array()
                            .is_some_and(|v| !v.is_empty())))
                && message["reasoning_content"]
                    .as_str()
                    .is_none_or(str::is_empty)
            {
                message["reasoning_content"] = json!(" ");
            }
        }
    }

    if options.schema_fallback {
        apply_schema_fallback(&mut body)?;
    }
    let object = body.as_object_mut()?;
    if !stream {
        object.shift_remove("stream_options");
    }
    if lower.contains("claude") {
        object.shift_remove("temperature");
    }
    Some(PreparedChat { body, stream })
}

pub(super) fn strip_responses_translation_fields(body: &mut Value) {
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    fields.shift_remove("_customToolNames");
    fields.shift_remove("_toolNameMap");
    if let Some(messages) = fields.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut().filter_map(Value::as_object_mut) {
            message.shift_remove("encrypted_content");
            message.shift_remove("reasoning_encrypted_content");
        }
    }
}

pub(super) fn preprocess_tool_history(messages: &mut Vec<Value>) -> Option<()> {
    for (index, message) in messages.iter_mut().enumerate() {
        normalize_tool_history(message, index)?;
    }
    let mut normalized = Vec::with_capacity(messages.len());
    let mut source = std::mem::take(messages).into_iter().peekable();
    while let Some(message) = source.next() {
        let missing = source.peek().map_or_else(Vec::new, |next| {
            let ids = tool_ids(&message);
            if ids.is_empty() || matches_tool_result(next, &ids) {
                Vec::new()
            } else {
                ids.into_iter()
                    .map(|id| json!({ "role": "tool", "tool_call_id": id, "content": "" }))
                    .collect()
            }
        });
        normalized.push(message);
        normalized.extend(missing);
    }
    *messages = normalized;
    Some(())
}

fn is_openai_chat(body: &Value) -> bool {
    if !body["messages"].is_array()
        || body["input"].is_array()
        || body["contents"].is_array()
        || body["userAgent"] == "antigravity"
        || truthy(&body["conversationState"])
    {
        return false;
    }
    let openai_indicator = ["stream_options", "response_format", "logit_bias", "user"]
        .iter()
        .any(|key| truthy(&body[*key]))
        || [
            "logprobs",
            "top_logprobs",
            "n",
            "presence_penalty",
            "frequency_penalty",
        ]
        .iter()
        .any(|key| body.get(*key).is_some());
    if openai_indicator {
        return true;
    }
    if body.get("system").is_some() || truthy(&body["anthropic_version"]) {
        return false;
    }
    if !body["model"]
        .as_str()
        .is_some_and(|model| model.contains('/'))
        && let Some(first_parts) = body["messages"]
            .get(0)
            .and_then(|message| message["content"].as_array())
        && first_parts
            .first()
            .is_some_and(|part| part["type"] == "text")
        && first_parts
            .iter()
            .any(|part| matches!(part["type"].as_str(), Some("tool_use" | "tool_result")))
    {
        return false;
    }
    true
}

fn has_thinking(body: &Value, model: &str) -> bool {
    model
        .trim_end_matches(js_space)
        .strip_suffix(')')
        .and_then(|base| base.rsplit_once('('))
        .is_some_and(|(_, suffix)| !suffix.is_empty() && !suffix.contains(')'))
        || THINKING_FIELDS.iter().any(|key| body.get(*key).is_some())
        || body["generationConfig"].get("thinkingConfig").is_some()
        || body["request"]["generationConfig"]
            .get("thinkingConfig")
            .is_some()
        || body["params"].get("reasoning_effort").is_some()
        || body["params"].get("thinking").is_some()
}

fn unsupported_media(message: &Value) -> bool {
    ["images", "attachments", "experimental_attachments"]
        .iter()
        .any(|key| message[*key].is_array())
        || message["content"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                matches!(
                    part["type"].as_str(),
                    Some("image" | "input_audio" | "audio_url" | "file")
                )
            })
        })
}

fn normalize_tool_history(message: &mut Value, index: usize) -> Option<()> {
    message.as_object()?;
    if message["role"] == "assistant"
        && let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut)
    {
        for (i, call) in calls.iter_mut().enumerate() {
            call.as_object()?;
            normalize_id(call, "id", index, i, true, "function")?;
            if !truthy(&call["type"]) {
                call["type"] = json!("function");
            }
            if let Some(args) = call["function"].get("arguments")
                && !args.is_string()
                && truthy(args)
            {
                call["function"]["arguments"] = json!(js_json(args, false)?);
            }
        }
    }
    if message["role"] == "tool" && truthy(&message["tool_call_id"]) {
        normalize_id(message, "tool_call_id", index, 0, false, "")?;
    }
    if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
        for (i, part) in parts.iter_mut().enumerate() {
            if part.is_null() {
                return None;
            }
            match part["type"].as_str() {
                Some("tool_use") => normalize_id(part, "id", index, i, false, "name")?,
                Some("tool_result") => normalize_id(part, "tool_use_id", index, i, false, "")?,
                _ => (),
            }
        }
    }
    Some(())
}

fn normalize_id(
    object: &mut Value,
    field: &str,
    message: usize,
    call: usize,
    required: bool,
    name_path: &str,
) -> Option<()> {
    let current = &object[field];
    if !required && !truthy(current) {
        return Some(());
    }
    let raw = if truthy(current) {
        current.as_str()?
    } else {
        ""
    };
    let id = sanitized_id(raw);
    if id.is_empty() {
        let name = match name_path {
            "function" => &object["function"]["name"],
            "name" => &object["name"],
            _ => &Value::Null,
        };
        let suffix = if truthy(name) {
            format!("_{}", sanitized_id(name.as_str()?))
        } else {
            String::new()
        };
        object[field] = json!(format!("call_msg{message}_tc{call}{suffix}"));
    } else if id != raw {
        object[field] = json!(id);
    }
    Some(())
}

fn sanitized_id(text: &str) -> String {
    text.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .collect()
}

fn tool_ids(message: &Value) -> Vec<&Value> {
    if message["role"] != "assistant" {
        return Vec::new();
    }
    let mut ids: Vec<_> = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|call| &call["id"])
        .filter(|id| truthy(id))
        .collect();
    ids.extend(
        message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|part| part["type"] == "tool_use" && truthy(&part["id"]))
            .map(|part| &part["id"]),
    );
    ids
}

fn matches_tool_result(message: &Value, ids: &[&Value]) -> bool {
    (message["role"] == "tool" && ids.contains(&&message["tool_call_id"]))
        || (message["role"] == "user"
            && message["content"].as_array().is_some_and(|parts| {
                parts.iter().any(|part| {
                    part["type"] == "tool_result" && ids.contains(&&part["tool_use_id"])
                })
            }))
}

pub(super) fn filter_message(
    mut message: Value,
    preserve_cache_control: bool,
    all_openai_blocks: bool,
) -> Option<Option<Value>> {
    if message["role"] == "developer" {
        message["role"] = json!("system");
    }
    if message["role"] == "tool"
        || (message["role"] == "assistant" && truthy(&message["tool_calls"]))
    {
        return Some(Some(message));
    }
    match message.get_mut("content") {
        Some(Value::String(text)) => {
            if text.trim_matches(js_space).is_empty() {
                return Some(None);
            }
        }
        Some(Value::Array(parts)) => {
            if parts.iter().any(Value::is_null) {
                return None;
            }
            parts.retain(|part| match part["type"].as_str() {
                Some("text" | "tool_result" | "image_url") => true,
                Some("image" | "input_audio" | "audio_url" | "file") => all_openai_blocks,
                _ => false,
            });
            for part in parts.iter_mut() {
                let object = part.as_object_mut()?;
                if !preserve_cache_control || !object.get("cache_control").is_some_and(truthy) {
                    object.shift_remove("cache_control");
                }
                object.shift_remove("signature");
            }
            let mut keep = false;
            for part in parts.iter() {
                if part["type"] != "text" {
                    keep = true;
                } else if let Some(text) = part["text"].as_str() {
                    keep |= !text.trim_matches(js_space).is_empty();
                } else if !part["text"].is_null() {
                    return None;
                }
            }
            if !keep {
                return Some(None);
            }
        }
        _ => (),
    }
    Some(Some(message))
}

pub(super) fn normalize_tools(body: &mut Value) -> Option<()> {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        let was_empty = tools.is_empty();
        if was_empty {
            body.as_object_mut()?.shift_remove("tools");
        } else {
            let mut mapped = Vec::new();
            for tool in std::mem::take(tools) {
                if tool.is_null() {
                    return None;
                }
                if tool["type"] == "function" && truthy(&tool["function"]) {
                    mapped.push(tool);
                } else if truthy(&tool["name"])
                    && (truthy(&tool["input_schema"]) || truthy(&tool["description"]))
                {
                    mapped.push(openai_tool(&tool, "input_schema")?);
                } else if let Some(declarations) = tool["functionDeclarations"].as_array() {
                    for decl in declarations {
                        mapped.push(openai_tool(decl, "parameters")?);
                    }
                } else if let Value::Array(items) = tool {
                    mapped.extend(items);
                } else {
                    mapped.push(tool);
                }
            }
            *tools = mapped;
        }
    }

    let choice = &body["tool_choice"];
    match choice["type"].as_str() {
        Some("auto") => body["tool_choice"] = json!("auto"),
        Some("any") => body["tool_choice"] = json!("required"),
        Some("tool") if truthy(&choice["name"]) => {
            body["tool_choice"] =
                json!({ "type": "function", "function": { "name": choice["name"] } });
        }
        _ => (),
    }
    Some(())
}

fn openai_tool(tool: &Value, schema_field: &str) -> Option<Value> {
    tool.as_object()?;
    let description = if truthy(&tool["description"]) {
        tool["description"].as_str()?
    } else {
        ""
    };
    let parameters = if truthy(&tool[schema_field]) {
        tool[schema_field].clone()
    } else {
        json!({ "type": "object", "properties": {} })
    };
    let mut function = serde_json::Map::new();
    if let Some(name) = tool.get("name") {
        function.insert("name".into(), name.clone());
    }
    function.insert("description".into(), json!(description));
    function.insert("parameters".into(), parameters);
    Some(json!({ "type": "function", "function": function }))
}

pub(crate) fn apply_schema_fallback(body: &mut Value) -> Option<()> {
    let format = &body["response_format"];
    let schema = &format["json_schema"]["schema"];
    if format["type"] != "json_schema" || !truthy(schema) {
        return Some(());
    }
    let prompt = format!(
        "You must respond with valid JSON that strictly follows this JSON schema:\n```json\n{}\n```\nRespond ONLY with the JSON object, no other text.",
        js_json(schema, true)?
    );
    let messages = body["messages"].as_array_mut()?;
    if let Some(system) = messages
        .iter_mut()
        .find(|message| message["role"] == "system")
    {
        match system.get_mut("content") {
            Some(Value::String(text)) => {
                text.push_str("\n\n");
                text.push_str(&prompt);
            }
            Some(Value::Array(parts)) => {
                parts.push(json!({ "type": "text", "text": format!("\n\n{prompt}") }))
            }
            _ => (),
        }
    } else {
        messages.insert(0, json!({ "role": "system", "content": prompt }));
    }
    body["response_format"] = json!({ "type": "json_object" });
    Some(())
}

pub(super) fn js_json(value: &Value, pretty: bool) -> Option<String> {
    fn canonical(value: &Value) -> Option<Value> {
        match value {
            Value::Array(items) => Some(Value::Array(
                items.iter().map(canonical).collect::<Option<Vec<_>>>()?,
            )),
            Value::Object(fields) => {
                let mut entries: Vec<_> = fields.iter().collect();
                entries.sort_by_key(|(name, _)| {
                    name.parse::<u32>()
                        .ok()
                        .filter(|n| n.to_string() == **name)
                        .unwrap_or(u32::MAX)
                });
                Some(Value::Object(
                    entries
                        .into_iter()
                        .map(|(key, val)| Some((key.clone(), canonical(val)?)))
                        .collect::<Option<_>>()?,
                ))
            }
            Value::Number(number) => {
                let float = number.as_f64()?;
                if float.fract() == 0.0 && float.abs() <= 9_007_199_254_740_991.0 {
                    Some(json!(float as i64))
                } else if float.abs() < 1e-6
                    || float.abs() > 9_007_199_254_740_991.0
                    || number.to_string().contains(['e', 'E'])
                {
                    None
                } else {
                    Some(value.clone())
                }
            }
            value => Some(value.clone()),
        }
    }
    let value = canonical(value)?;
    if pretty {
        serde_json::to_string_pretty(&value).ok()
    } else {
        serde_json::to_string(&value).ok()
    }
}

pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

pub(super) fn js_space(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}
