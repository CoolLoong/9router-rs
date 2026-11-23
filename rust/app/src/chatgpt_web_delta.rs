use serde_json::{Map, Value, json};

use crate::chat_output::js_text;
use crate::provider_index::js_trim;

const FINISHED: &str = "finished_successfully";

pub(crate) struct DirectConversation {
    #[cfg(chatgpt_web_replay)]
    pub(crate) conversation_id: String,
    #[cfg(chatgpt_web_replay)]
    pub(crate) turn_exchange_id: String,
    pub(crate) text: String,
    #[cfg(chatgpt_web_replay)]
    pub(crate) status: &'static str,
    #[cfg(chatgpt_web_replay)]
    pub(crate) end_turn: bool,
}

pub(crate) fn parse_direct_conversation(sse: &str) -> Result<DirectConversation, String> {
    if js_trim(sse).is_empty() {
        return Err("ChatGPT Web direct conversation returned an empty stream".into());
    }
    let mut decoder = Decoder::new();
    #[cfg(chatgpt_web_replay)]
    let mut conversation_id = String::new();
    #[cfg(chatgpt_web_replay)]
    let mut turn_exchange_id = String::new();
    let mut latest = None;
    for event in parse_encoded_item(sse) {
        #[cfg(chatgpt_web_replay)]
        if let Some(object) = event.json.as_ref().and_then(Value::as_object) {
            if let Some(id) = object.get("conversation_id").and_then(Value::as_str) {
                conversation_id = id.to_owned();
            }
            if let Some(id) = object.get("turn_exchange_id").and_then(Value::as_str) {
                turn_exchange_id = id.to_owned();
            }
        }
        let mut encoded = String::new();
        if event.event != "message" {
            encoded.push_str("event: ");
            encoded.push_str(&event.event);
            encoded.push('\n');
        }
        encoded.push_str("data: ");
        encoded.push_str(&event.data);
        encoded.push_str("\n\n");
        if decoder.ingest(&encoded)?.changed
            && let Some(text) = finished_text(&decoder.document)
        {
            latest = Some(text);
        }
    }
    Ok(DirectConversation {
        #[cfg(chatgpt_web_replay)]
        conversation_id,
        #[cfg(chatgpt_web_replay)]
        turn_exchange_id,
        text: latest.ok_or_else(|| incomplete(&decoder.document))?,
        #[cfg(chatgpt_web_replay)]
        status: FINISHED,
        #[cfg(chatgpt_web_replay)]
        end_turn: true,
    })
}

fn finished_text(document: &Node) -> Option<String> {
    let message = document.field("message")?;
    let author = message.field("author")?;
    let content = message.field("content")?;
    if author.field("role").and_then(Node::as_str) != Some("assistant")
        || content.field("content_type").and_then(Node::as_str) != Some("text")
        || message.field("status").and_then(Node::as_str) != Some(FINISHED)
        || !matches!(
            message.field("end_turn"),
            Some(Node::Scalar(Value::Bool(true)))
        )
    {
        return None;
    }
    parts(Some(content)).iter().map(Node::as_str).collect()
}

fn parts(content: Option<&Node>) -> &[Node] {
    match content.and_then(|node| node.field("parts")) {
        Some(Node::Array(parts)) => parts,
        _ => &[],
    }
}

fn sorted_keys(fields: &[(String, Node)]) -> Vec<&str> {
    let mut keys = fields
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>();
    keys.sort_unstable_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    keys
}

fn incomplete(document: &Node) -> String {
    let Some(message @ Node::Object(fields)) = document.field("message") else {
        let keys = match document {
            Node::Object(fields) => sorted_keys(fields).join(","),
            _ => "non-object".into(),
        };
        return format!("ChatGPT Web assistant document is incomplete (root={keys})");
    };
    let author = message.field("author");
    let content = message.field("content");
    let parts = parts(content);
    let field_value = |object: Option<&Node>, key| {
        object
            .and_then(|object| object.field(key))
            .and_then(Node::to_json)
    };
    let summary = json!({
        "messageKeys": sorted_keys(fields),
        "role": field_value(author, "role"),
        "contentType": field_value(content, "content_type"),
        "partCount": parts.len(),
        "partTypes": parts.iter().map(Node::js_type).collect::<Vec<_>>(),
        "status": field_value(Some(message), "status"),
        "endTurn": field_value(Some(message), "end_turn"),
    });
    format!("ChatGPT Web assistant document is incomplete ({summary})")
}

pub(crate) struct Event {
    pub(crate) event: String,
    pub(crate) data: String,
    pub(crate) json: Option<Value>,
    #[cfg(chatgpt_web_replay)]
    pub(crate) done: bool,
}

pub(crate) fn parse_encoded_item(item: &str) -> Vec<Event> {
    let normalized = item.replace("\r\n", "\n").replace('\r', "\n");
    let mut events = Vec::new();
    let mut event = "message";
    let mut lines = Vec::new();
    for line in normalized.split('\n').chain(std::iter::once("")) {
        if line.is_empty() {
            if !lines.is_empty() {
                let data = lines.join("\n");
                events.push(Event {
                    event: event.to_owned(),
                    json: serde_json::from_str(&data).ok(),
                    #[cfg(chatgpt_web_replay)]
                    done: data == "[DONE]",
                    data,
                });
                lines.clear();
            }
            event = "message";
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => event = if value.is_empty() { "message" } else { value },
                "data" => lines.push(value),
                _ => (),
            }
        }
    }
    events
}

pub(crate) struct Batch {
    #[cfg(chatgpt_web_replay)]
    pub(crate) events: Vec<Event>,
    pub(crate) changed: bool,
    #[cfg(chatgpt_web_replay)]
    pub(crate) done: bool,
}

pub(crate) struct Decoder {
    document: Node,
    path: Option<String>,
    operation: Option<Operation>,
    #[cfg(chatgpt_web_replay)]
    done: bool,
}

impl Decoder {
    pub(crate) fn new() -> Self {
        Self {
            document: Node::Scalar(Value::Null),
            path: None,
            operation: None,
            #[cfg(chatgpt_web_replay)]
            done: false,
        }
    }

    #[cfg(chatgpt_web_replay)]
    pub(crate) fn snapshot(&self) -> Option<Value> {
        self.document.to_json()
    }

    pub(crate) fn ingest(&mut self, item: &str) -> Result<Batch, String> {
        let events = parse_encoded_item(item);
        let mut changed = false;
        for event in &events {
            match event.event.as_str() {
                "delta_encoding" => {
                    if event.json.as_ref().and_then(Value::as_str) != Some("v1") {
                        return Err(format!(
                            "Unsupported ChatGPT Web delta encoding: {}",
                            text(event.json.as_ref())
                        ));
                    }
                    *self = Self::new();
                }
                "delta" => {
                    let value = event
                        .json
                        .as_ref()
                        .and_then(Value::as_object)
                        .ok_or("ChatGPT Web delta payload must be an object")?;
                    let operation = match value.get("o") {
                        Some(value) => Some(Operation::parse(Some(value))?),
                        None => self.operation,
                    };
                    let path = match value.get("p") {
                        Some(value) => value.as_str(),
                        None => self.path.as_deref(),
                    };
                    let (Some(path), Some(operation)) = (path, operation) else {
                        return Err(
                            "ChatGPT Web delta requires a current or inherited path and operation"
                                .into(),
                        );
                    };
                    let path = path.to_owned();
                    self.path = Some(path.clone());
                    self.operation = Some(operation);
                    apply_at(&mut self.document, &path, operation, value.get("v"))?;
                    changed = true;
                }
                _ => (),
            }
            #[cfg(chatgpt_web_replay)]
            {
                self.done |= event.done;
            }
        }
        Ok(Batch {
            #[cfg(chatgpt_web_replay)]
            events,
            changed,
            #[cfg(chatgpt_web_replay)]
            done: self.done,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Add,
    Append,
    Patch,
    Replace,
}

impl Operation {
    fn parse(value: Option<&Value>) -> Result<Self, String> {
        match value.and_then(Value::as_str) {
            Some("add") => Ok(Self::Add),
            Some("append") => Ok(Self::Append),
            Some("patch") => Ok(Self::Patch),
            Some("replace") => Ok(Self::Replace),
            _ => Err(format!("Unsupported delta operation: {}", text(value))),
        }
    }
}

enum Node {
    Undefined,
    Scalar(Value),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
}

impl Node {
    fn field(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .find(|(name, _)| key == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::Scalar(Value::String(text)) => Some(text),
            _ => None,
        }
    }

    fn from_json(value: Option<&Value>) -> Self {
        match value {
            None => Self::Undefined,
            Some(Value::Array(values)) => Self::Array(
                values
                    .iter()
                    .map(|value| Self::from_json(Some(value)))
                    .collect(),
            ),
            Some(Value::Object(fields)) => Self::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), Self::from_json(Some(value))))
                    .collect(),
            ),
            Some(value) => Self::Scalar(value.clone()),
        }
    }

    fn to_json(&self) -> Option<Value> {
        match self {
            Self::Undefined => None,
            Self::Scalar(value) => Some(value.clone()),
            Self::Array(values) => Some(Value::Array(
                values
                    .iter()
                    .map(|value| value.to_json().unwrap_or(Value::Null))
                    .collect(),
            )),
            Self::Object(fields) => Some(Value::Object(
                fields
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.to_json()?)))
                    .collect::<Map<_, _>>(),
            )),
        }
    }

    fn js_type(&self) -> &'static str {
        match self {
            Self::Undefined => "undefined",
            Self::Scalar(Value::String(_)) => "string",
            Self::Scalar(Value::Bool(_)) => "boolean",
            Self::Scalar(Value::Number(_)) => "number",
            _ => "object",
        }
    }
}

fn apply_at(
    document: &mut Node,
    path: &str,
    operation: Operation,
    value: Option<&Value>,
) -> Result<(), String> {
    if let Some(value) = value {
        assert_safe(value)?;
    }
    if path.is_empty() {
        match operation {
            Operation::Patch => {
                let entries = value
                    .and_then(Value::as_array)
                    .ok_or("ChatGPT Web patch value must be an array")?;
                for entry in entries {
                    let path = entry
                        .as_object()
                        .and_then(|entry| entry.get("p"))
                        .and_then(Value::as_str)
                        .ok_or("ChatGPT Web patch entry requires an explicit path and operation")?;
                    apply_at(
                        document,
                        path,
                        Operation::parse(entry.get("o"))?,
                        entry.get("v"),
                    )?;
                }
            }
            Operation::Append => append(document, Node::from_json(value))?,
            Operation::Add | Operation::Replace => *document = Node::from_json(value),
        }
        return Ok(());
    }
    let segments = pointer_segments(path)?;
    let (key, parents) = segments
        .split_last()
        .ok_or("Invalid ChatGPT Web JSON Pointer")?;
    let mut target = document;
    for parent in parents {
        target = match target {
            Node::Array(array) => {
                let index = array_index(parent, array.len(), false)?;
                &mut array[index]
            }
            Node::Object(fields) => fields
                .iter_mut()
                .find(|(key, _)| key == parent)
                .map(|(_, value)| value)
                .ok_or_else(|| format!("ChatGPT Web delta path does not exist: {path}"))?,
            _ => return Err(format!("ChatGPT Web delta path does not exist: {path}")),
        };
    }
    match target {
        Node::Array(array) => {
            if operation == Operation::Patch {
                return Err("Nested ChatGPT Web patch is unsupported".into());
            }
            let index = array_index(key, array.len(), operation == Operation::Add)?;
            let incoming = Node::from_json(value);
            match operation {
                Operation::Add => array.insert(index, incoming),
                Operation::Append => append(&mut array[index], incoming)?,
                _ => array[index] = incoming,
            }
        }
        Node::Object(fields) => {
            if operation == Operation::Patch {
                return Err("Nested ChatGPT Web patch is unsupported".into());
            }
            if let Some((_, current)) = fields.iter_mut().find(|(name, _)| name == key) {
                let incoming = Node::from_json(value);
                if operation == Operation::Append {
                    append(current, incoming)?;
                } else {
                    *current = incoming;
                }
            } else if operation == Operation::Add {
                fields.push((key.clone(), Node::from_json(value)));
            } else {
                return Err(format!("ChatGPT Web delta target does not exist: {key}"));
            }
        }
        _ => return Err(format!("ChatGPT Web delta path is not mutable: {path}")),
    }
    Ok(())
}

fn append(current: &mut Node, incoming: Node) -> Result<(), String> {
    match (current, incoming) {
        (current @ (Node::Undefined | Node::Scalar(Value::Null)), incoming) => *current = incoming,
        (Node::Scalar(Value::String(current)), Node::Scalar(Value::String(incoming))) => {
            current.push_str(&incoming)
        }
        (Node::Array(current), Node::Array(incoming)) => current.extend(incoming),
        (Node::Array(current), incoming) => current.push(incoming),
        (Node::Object(current), Node::Object(incoming)) => {
            for (key, value) in incoming {
                if let Some((_, prior)) = current.iter_mut().find(|(name, _)| name == &key) {
                    *prior = value;
                } else {
                    current.push((key, value));
                }
            }
        }
        (current, incoming) => {
            return Err(format!(
                "Cannot append ChatGPT Web delta values ({}, {})",
                current.js_type(),
                incoming.js_type()
            ));
        }
    }
    Ok(())
}

fn pointer_segments(pointer: &str) -> Result<Vec<String>, String> {
    pointer
        .strip_prefix('/')
        .ok_or_else(|| format!("Invalid ChatGPT Web JSON Pointer: {pointer}"))?
        .split('/')
        .map(|segment| {
            let decoded = segment.replace("~1", "/").replace("~0", "~");
            if unsafe_key(&decoded) {
                return Err(format!("Unsafe JSON Pointer segment: {decoded}"));
            }
            Ok(decoded)
        })
        .collect()
}

fn array_index(segment: &str, length: usize, allow_end: bool) -> Result<usize, String> {
    if allow_end && segment == "-" {
        return Ok(length);
    }
    if segment.is_empty()
        || (segment.starts_with('0') && segment.len() > 1)
        || !segment.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("Invalid ChatGPT Web array index: {segment}"));
    }
    segment
        .parse::<usize>()
        .ok()
        .filter(|index| *index < length || allow_end && *index == length)
        .ok_or_else(|| format!("ChatGPT Web array index is out of bounds: {segment}"))
}

fn assert_safe(value: &Value) -> Result<(), String> {
    match value {
        Value::Array(values) => values.iter().try_for_each(assert_safe),
        Value::Object(fields) => fields.iter().try_for_each(|(key, value)| {
            if unsafe_key(key) {
                return Err(format!("Unsafe object key in ChatGPT Web delta: {key}"));
            }
            assert_safe(value)
        }),
        _ => Ok(()),
    }
}

fn unsafe_key(key: &str) -> bool {
    matches!(key, "__proto__" | "constructor" | "prototype")
}

fn text(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".into(), js_text)
}
