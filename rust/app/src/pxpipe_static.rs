use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use regex::Regex;
use serde_json::{Value, json};

use crate::cli::js_space;
use crate::pxpipe_factsheet;
use crate::pxpipe_render::{self, Options};
use crate::pxpipe_transform::{ImageInfo, Prepared, Transform};

#[path = "pxpipe_history.rs"]
mod history;
#[path = "pxpipe_pin.rs"]
mod pin;
#[path = "pxpipe_profiles.rs"]
mod profiles;
use profiles::{Geometry, Resolved, Selection};
#[path = "pxpipe_system.rs"]
mod system_sections;
#[path = "pxpipe_tool_results.rs"]
mod tool_results;

const HEADER: &str = concat!(
    "=================== SESSION CONFIGURATION PAGES ===================\n",
    "pxpipe (this user's local proxy) rendered this session's configuration",
    " into the following images to reduce token cost. Read the pages carefully and follow them as",
    " your operating instructions for this session.",
    " For exact identifiers, paths, hashes, version strings, and numbers, use the adjacent",
    " exact-value factsheet; if a value was only visible in an image and is not in that factsheet,",
    " do not guess it — say it is not safe to quote from the image and re-read the source text.",
    " The glyph ↵ (U+21B5) marks an original hard line break in content — treat as a real newline.",
    "\n====================== BEGIN RENDERED CONTEXT ======================\n",
);
const TOOL_HEADER: &str = concat!(
    "=== TOOL REFERENCE ===\n",
    "pxpipe (this user's local proxy) moved the full tool documentation for this",
    " session here to reduce token cost. Each tool in the tools list carries a short",
    " stub description pointing here; the entry under the matching",
    " \"## Tool: <name>\" heading below is the complete description for that tool.\n\n",
);
const READ_FIRST: &str = concat!(
    " Requires a Read of the same file earlier in THIS session when the file",
    " already exists — the call is rejected otherwise; file content recalled",
    " from imaged or prior-session context does not satisfy this.",
);
const DYNAMIC: &[&str] = &[
    "env",
    "context",
    "git_status",
    "directoryStructure",
    "system-reminder",
    "total_tokens",
    "cc_automode_session_rules",
    "cc_automode_permissions",
    "severity",
    "category",
];
const IDENTITIES: &[&str] = &[
    "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.",
    "You are Claude Code, Anthropic's official CLI for Claude.",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
];
const IMAGE_BYTES: usize = 18 * 1024 * 1024;
const IMAGE_LIMIT: usize = 95;
const PAGE_CHARS: usize = 28_080;

static TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        &DYNAMIC
            .iter()
            .map(|tag| format!(r"<{tag}(?:\s[^>]*)?>[\s\S]*?</{tag}>"))
            .collect::<Vec<_>>()
            .join("|"),
    )
    .expect("PXPIPE dynamic tags")
});
pub fn transform(
    body: &Value,
    model: &str,
    prepared: &Prepared,
    scope: Option<&str>,
    overrides: Option<&str>,
) -> Option<Result<Transform, String>> {
    let profile = match profiles::select(model, scope, overrides) {
        Selection::NotSelected => return Some(Ok(skipped("unsupported_model", model.to_owned()))),
        Selection::Deferred => return None,
        Selection::Native(geometry) => *geometry,
    };
    let messages = request(body)?;
    rewrite(body, messages, profile, prepared)
}

pub fn supports(body: &Value, model: &str, scope: Option<&str>, overrides: Option<&str>) -> bool {
    match profiles::select(model, scope, overrides) {
        Selection::NotSelected => true,
        Selection::Deferred => false,
        Selection::Native(_) => request(body).is_some(),
    }
}

fn request(body: &Value) -> Option<&[Value]> {
    if skipped_for_shape(body) {
        return None;
    }
    let messages = body["messages"].as_array()?;
    if messages.is_empty() {
        return None;
    }
    messages
        .iter()
        .all(|message| {
            matches!(message["role"].as_str(), Some("user" | "assistant"))
                && match &message["content"] {
                    Value::String(_) => true,
                    Value::Array(blocks) => blocks.iter().all(|block| {
                        block.is_object() && (block["type"] != "text" || block["text"].is_string())
                    }),
                    _ => false,
                }
        })
        .then_some(messages)
}

fn rewrite(
    body: &Value,
    messages: &[Value],
    profile: Resolved,
    prepared: &Prepared,
) -> Option<Result<Transform, String>> {
    let geometry = profile.standard;
    let mut budget = ImageBudget::new(messages);
    let mut output = body.clone();
    let pins = pin::prepare(&mut output);
    let session = history::session_id(output["messages"].as_array()?);
    let mut system = extract_system(&output["system"])?;
    let identity = system.keep.iter().position(|block| {
        block["type"] == "text"
            && IDENTITIES.contains(
                &block["text"]
                    .as_str()
                    .unwrap_or_default()
                    .trim_matches(js_space),
            )
    });
    let identity = identity.map(|index| {
        system.keep.remove(index)["text"]
            .as_str()
            .unwrap()
            .trim_matches(js_space)
            .to_owned()
    });
    system.keep.retain_mut(|block| {
        let Some(text) = block["text"].as_str().filter(|_| block["type"] == "text") else {
            return true;
        };
        let std::borrow::Cow::Owned(text) = system_sections::strip_billing(text) else {
            return true;
        };
        if text.trim_matches(js_space).is_empty() {
            return false;
        }
        block["text"] = Value::String(text);
        true
    });
    let stripped = system_sections::strip_billing(&system.text);
    let (text_without_environment, environment) = system_sections::split_environment(&stripped);
    let (text, dynamic) = static_and_dynamic(&text_without_environment);
    let inline = IDENTITIES
        .iter()
        .find(|identity| text.starts_with(**identity))
        .copied();
    let identity = identity.or_else(|| inline.map(str::to_owned));
    let static_text = inline.map_or(text.as_str(), |identity| {
        text[identity.len()..].trim_start_matches(js_space)
    });
    let tools = match body["tools"].as_array().filter(|tools| !tools.is_empty()) {
        Some(tools) => Some(tool_reference(tools)?),
        None => None,
    };
    let documentation = tools
        .as_ref()
        .map(|(_, docs)| docs.as_str())
        .unwrap_or_default();
    let combined = [static_text, documentation]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let packed = packed_text(&combined);
    let chars = packed.encode_utf16().count();
    let slab_skip = if (chars as f64) < prepared.min_chars {
        Some(skipped(
            "below_min_chars",
            format!("below_min_chars ({chars} < {})", prepared.min_chars),
        ))
    } else if budget.image_headroom() == 0 {
        Some(skipped("passthrough", "image_budget".to_owned()))
    } else {
        let image_text = format!("{HEADER}{packed}");
        let columns =
            pxpipe_render::measure_columns(&image_text, geometry.columns, geometry.style.font);
        if !profitable(&image_text, columns, geometry, None, 2.0, PAGE_CHARS) {
            Some(skipped(
                "not_profitable",
                format!("not_profitable (slab={chars} chars)"),
            ))
        } else {
            let pages = pxpipe_render::render(
                &image_text,
                &Options {
                    columns,
                    shrink: false,
                    style: geometry.style,
                    max_height: geometry.max_height,
                    page_limit: IMAGE_LIMIT * 2,
                    ..Default::default()
                },
            )
            .ok()?;
            let bytes: usize = pages.iter().map(|page| page.png.len()).sum();
            if pages.len() > budget.image_headroom() {
                Some(skipped(
                    "passthrough",
                    format!(
                        "image_budget (slab needs {}, headroom {})",
                        pages.len(),
                        budget.image_headroom()
                    ),
                ))
            } else if bytes > budget.byte_headroom() {
                Some(skipped(
                    "passthrough",
                    format!(
                        "image_bytes (slab needs {bytes}, headroom {})",
                        budget.byte_headroom()
                    ),
                ))
            } else {
                let blocks = budget.admit(pages, &combined, system.cache.as_ref());
                let slab = Slab {
                    system,
                    identity,
                    dynamic,
                    environment,
                    blocks,
                    combined,
                    tools,
                };
                slab.apply(&mut output);
                None
            }
        }
    };
    let current = output["messages"].as_array_mut()?;
    let slab = slab_skip.is_none();
    let protected = history::protected(current, slab);
    history::collapse(
        current,
        &mut budget,
        profile.history,
        protected,
        session,
        slab,
    );
    tool_results::compress(current, &mut budget, geometry);
    if budget.info.count == 0
        && let Some(skip) = slab_skip
    {
        return Some(Ok(skip));
    }
    pin::append(current, &pins);
    let owns_cache_control = cache_marked(&output);
    let body = crate::chat_input::js_json(&output, false)
        .map(String::into_bytes)
        .unwrap_or_else(|| output.to_string().into_bytes());
    Some(Ok(Transform::Applied {
        body,
        info: budget.info,
        owns_cache_control,
    }))
}

struct Slab {
    system: System,
    identity: Option<String>,
    dynamic: String,
    environment: String,
    blocks: Vec<Value>,
    combined: String,
    tools: Option<(Vec<Value>, String)>,
}

impl Slab {
    fn apply(self, output: &mut Value) {
        let Self {
            system,
            identity,
            dynamic,
            environment,
            mut blocks,
            combined,
            tools,
        } = self;
        let fact = pxpipe_factsheet::text(&combined);
        if !fact.is_empty() {
            blocks.push(text_block(&fact));
        }
        blocks.push(text_block("[End of rendered context.]"));
        let mut tail = Vec::new();
        if let Some(identity) = identity {
            tail.push(text_block(&identity));
        }
        if !dynamic.is_empty() {
            tail.push(text_block(&dynamic));
        }
        if !environment.is_empty() {
            tail.push(text_block(&environment));
        }
        tail.extend(system.keep);
        if tail.is_empty() {
            output
                .as_object_mut()
                .expect("a request object")
                .shift_remove("system");
        } else {
            output["system"] = Value::Array(tail);
        }
        if let Some(user) = output["messages"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|message| message["role"] == "user")
        {
            let content = &mut user["content"];
            match content.take() {
                Value::String(text) => blocks.push(text_block(&text)),
                Value::Array(items) => blocks.extend(items),
                _ => (),
            }
            *content = Value::Array(blocks);
        }
        if let Some((rewritten, _)) = tools {
            output["tools"] = Value::Array(rewritten);
        }
    }
}

struct ImageBudget {
    info: ImageInfo,
    native_count: usize,
    native_bytes: usize,
}

impl ImageBudget {
    fn new(messages: &[Value]) -> Self {
        let mut counter = Self {
            info: ImageInfo::default(),
            native_count: 0,
            native_bytes: 0,
        };
        for block in messages
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
        {
            if block["type"] == "tool_result" {
                for part in block["content"].as_array().into_iter().flatten() {
                    counter.account_image(part);
                }
            } else {
                counter.account_image(block);
            }
        }
        counter
    }

    fn account_image(&mut self, block: &Value) {
        if block["type"] != "image" {
            return;
        }
        self.native_count += 1;
        if let Some(data) = block["source"]["data"].as_str() {
            let padding = if data.ends_with("==") {
                2
            } else {
                usize::from(data.ends_with('='))
            };
            self.native_bytes += (data.encode_utf16().count() * 3 / 4).saturating_sub(padding);
        }
    }

    fn image_headroom(&self) -> usize {
        IMAGE_LIMIT.saturating_sub(self.native_count + self.info.count)
    }

    fn byte_headroom(&self) -> usize {
        IMAGE_BYTES.saturating_sub(self.native_bytes + self.info.bytes)
    }

    fn admit(
        &mut self,
        pages: Vec<pxpipe_render::Page>,
        raw: &str,
        cache: Option<&Value>,
    ) -> Vec<Value> {
        self.info.count += pages.len();
        self.info.bytes += pages.iter().map(|page| page.png.len()).sum::<usize>();
        self.info.pixels += pages
            .iter()
            .map(|page| page.width * page.height)
            .sum::<usize>();
        self.info.compressed_chars += raw.encode_utf16().count();
        let mut blocks: Vec<_> = pages.into_iter().map(|page| {
            json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":STANDARD.encode(page.png)}})
        }).collect();
        if let Some(cache) = cache
            && let Some(last) = blocks.last_mut()
        {
            let mut value = cache.clone();
            if let Some(fields) = value.as_object_mut() {
                fields.shift_remove("scope");
            }
            last["cache_control"] = value;
        }
        blocks
    }
}
fn text_block(text: &str) -> Value {
    json!({"type":"text","text":text})
}

fn skipped(reason: &str, detail: String) -> Transform {
    Transform::Skipped {
        reason: Some(reason.to_owned()),
        detail: Some(detail),
    }
}

fn cache_marked(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            !value["cache_control"].is_null() || fields.values().any(cache_marked)
        }
        Value::Array(entries) => entries.iter().any(cache_marked),
        _ => false,
    }
}

struct System {
    text: String,
    keep: Vec<Value>,
    cache: Option<Value>,
}

fn extract_system(value: &Value) -> Option<System> {
    let mut keep = Vec::new();
    let mut cache = None;
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Array(blocks) => {
            let marked = blocks
                .iter()
                .any(|block| block["type"] == "text" && block.get("cache_control").is_some());
            let mut text = Vec::new();
            for block in blocks {
                if block["type"] == "text" && (!marked || block.get("cache_control").is_some()) {
                    let content = block["text"].as_str()?;
                    text.push(content);
                    if let Some(marker) = block.get("cache_control")
                        && !static_and_dynamic(&system_sections::strip_billing(content))
                            .0
                            .is_empty()
                    {
                        cache = Some(marker.clone());
                    }
                } else {
                    keep.push(block.clone());
                }
            }
            text.join("\n\n")
        }
        _ => return None,
    };
    Some(System { text, keep, cache })
}

fn skipped_for_shape(body: &Value) -> bool {
    !body.is_object()
        || !matches!(&body["system"], Value::String(_) | Value::Null | Value::Array(_))
        || body["tools"].as_array().is_some_and(|tools| tools.iter().any(|tool| {
            !tool.is_object()
                || matches!(tool.get("name"), Some(name) if !name.is_null() && !name.is_string())
                || matches!(tool.get("description"), Some(text) if !text.is_null() && !text.is_string())
                || tool.get("input_schema").is_some_and(|schema| crate::chat_input::js_json(schema, false).is_none())
        }))
}

const STRUCTURAL: &[&str] = &[
    "properties",
    "patternProperties",
    "oneOf",
    "anyOf",
    "allOf",
    "items",
    "$ref",
    "enum",
    "const",
];

fn tool_reference(tools: &[Value]) -> Option<(Vec<Value>, String)> {
    let mut docs = Vec::new();
    let mut rewritten = Vec::with_capacity(tools.len());
    for tool in tools {
        if tool.get("type").is_some_and(|kind| kind != "custom") || tool["defer_loading"] == true {
            rewritten.push(tool.clone());
            continue;
        }
        let name = tool["name"].as_str().unwrap_or("?");
        let mut document = format!("## Tool: {name}");
        if let Some(description) = tool["description"]
            .as_str()
            .filter(|description| !description.is_empty())
        {
            document.push('\n');
            document.push_str(description);
        }
        let schema = tool.get("input_schema");
        if let Some(schema) = schema {
            document.push_str("\n\u{0060}\u{0060}\u{0060}json\n");
            document.push_str(&crate::chat_input::js_json(schema, false)?);
            document.push_str("\n\u{0060}\u{0060}\u{0060}");
        }
        docs.push(document);
        let mut stub = tool.clone();
        let read_first = if matches!(name, "Edit" | "Write" | "NotebookEdit") {
            READ_FIRST
        } else {
            ""
        };
        stub["description"] = json!(format!(
            "ⓘ Full docs: see \"## Tool: {name}\" in the Tool Reference section.{read_first}"
        ));
        if let Some(schema) = schema.filter(|schema| schema.is_object()) {
            let stripped = strip_schema(schema, 0);
            if STRUCTURAL.iter().any(|key| stripped.get(*key).is_some()) {
                stub["input_schema"] = stripped;
            }
        }
        rewritten.push(stub);
    }
    let documentation = if docs.is_empty() {
        String::new()
    } else {
        format!(
            "{TOOL_HEADER}{}\n=== END TOOL REFERENCE ===",
            docs.join("\n\n")
        )
    };
    Some((rewritten, documentation))
}

fn strip_schema(schema: &Value, depth: usize) -> Value {
    let Some(fields) = schema.as_object().filter(|_| depth <= 20) else {
        return schema.clone();
    };
    let iter = fields.iter().filter_map(|(key, value)| {
        let val = match key.as_str() {
            "description" | "title" | "examples" | "default" | "$id" | "$comment" => return None,
            "format"
                if value
                    .as_str()
                    .is_some_and(|format| format.encode_utf16().count() > 32) =>
            {
                return None;
            }
            "$schema" | "required" | "enum" | "const" | "type" | "$ref" | "minimum" | "maximum"
            | "exclusiveMinimum" | "exclusiveMaximum" | "minLength" | "maxLength" | "minItems"
            | "maxItems" | "minProperties" | "maxProperties" | "multipleOf" | "uniqueItems"
            | "pattern" => value.clone(),
            "properties" | "patternProperties" | "definitions" | "$defs" if value.is_object() => {
                Value::Object(
                    value
                        .as_object()
                        .unwrap()
                        .iter()
                        .map(|(name, schema)| (name.clone(), strip_schema(schema, depth + 1)))
                        .collect(),
                )
            }
            "oneOf" | "anyOf" | "allOf" if value.is_array() => Value::Array(
                value
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| strip_schema(item, depth + 1))
                    .collect(),
            ),
            "items"
            | "additionalProperties"
            | "not"
            | "contains"
            | "propertyNames"
            | "unevaluatedItems"
            | "unevaluatedProperties"
            | "if"
            | "then"
            | "else"
                if value.is_array() =>
            {
                Value::Array(
                    value
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|item| strip_schema(item, depth + 1))
                        .collect(),
                )
            }
            _ => strip_schema(value, depth + 1),
        };
        Some((key.clone(), val))
    });
    Value::Object(iter.collect())
}

fn static_and_dynamic(text: &str) -> (String, String) {
    let mut kept = String::with_capacity(text.len());
    let mut dynamic = Vec::new();
    let mut cursor = 0;
    for span in TAGS.find_iter(text) {
        kept.push_str(&text[cursor..span.start()]);
        dynamic.push(span.as_str());
        cursor = span.end();
    }
    kept.push_str(&text[cursor..]);
    (
        collapse_newlines(&kept).trim_matches(js_space).to_owned(),
        dynamic.join("\n\n"),
    )
}

fn collapse_newlines(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut newlines = 0;
    for ch in text.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines > 2 {
                continue;
            }
        } else {
            newlines = 0;
        }
        output.push(ch);
    }
    output
}

fn packed_text(text: &str) -> String {
    pxpipe_render::pack(&pxpipe_render::compact(text, 2).replace('↵', "⏎"))
}

fn visual_rows(text: &str, columns: usize) -> usize {
    text.split('\n')
        .map(|line| {
            let width = line.encode_utf16().count() + line.matches('↵').count() * 2;
            width.div_ceil(columns).max(1)
        })
        .sum()
}

fn profitable(
    text: &str,
    columns: usize,
    geometry: Geometry,
    max_images: Option<usize>,
    chars_per_token: f64,
    max_chars: usize,
) -> bool {
    if text.is_empty() {
        return false;
    }
    let rows = visual_rows(text, columns);
    let (cell_width, cell_height) = geometry.style.cell();
    let rows_per_page = geometry.hard_rows().min((max_chars / columns).max(1));
    let width = 8 + columns * cell_width;
    let full = rows
        .div_ceil(rows_per_page)
        .min(max_images.unwrap_or(usize::MAX))
        - 1;
    let final_rows = (rows - full * rows_per_page).clamp(1, rows_per_page);
    let tokens = full as f64
        * geometry
            .price
            .tokens(width, 8 + rows_per_page * cell_height)
        + geometry.price.tokens(width, 8 + final_rows * cell_height);
    (tokens * 1.10).ceil() < text.encode_utf16().count() as f64 / chars_per_token
}
