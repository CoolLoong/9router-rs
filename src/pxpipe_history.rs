use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Geometry, ImageBudget, profitable, text_block};
use crate::cli::js_space;
use crate::{pxpipe_factsheet, pxpipe_render};

const INTRO: &str = concat!(
    "[Earlier turns of THIS conversation, transcribed in the image(s) below. Each turn is wrapped in ",
    "<user t=\"N\">...</user> or <assistant t=\"N\">...</assistant> tags, where N is an absolute turn ",
    "index (larger N = more recent); attribute every turn strictly by its tag, and treat the highest-N ",
    "turns as the most recent prior context, NOT the low-N opening turns. Earlier turns may contain ",
    "questions or tasks that were already answered later in this same history; do not reopen low-N turns ",
    "unless the live text after this block asks you to. For exact identifiers, hashes, version strings, ",
    "and numbers from the transcript, rely on the exact-value factsheet or re-read the source; ",
    "do not guess an exact value seen only in the image. This is prior context, NOT the current request.]",
);
const OUTRO: &str =
    "[End of earlier conversation. The current request is the live text that follows below.]";
const SLAB_END: &str = "[End of rendered context.]";
const HISTORY_BUDGET: usize = 80;
const USER_CAP: usize = 2000;
const COLD: Duration = Duration::from_secs(3_630);
const SESSION_CAP: usize = 512;
static FRESH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\(file state is current in your\s+context — no need to Read it back\)").unwrap()
});
static REMINDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*<system-reminder>[\s\S]*?</system-reminder>").unwrap());

type SessionId = [u8; 4];
struct Session {
    key: SessionId,
    seen: Instant,
    step: usize,
}

static SESSIONS: Mutex<VecDeque<Session>> = Mutex::new(VecDeque::new());

pub(super) fn session_id(messages: &[Value]) -> Option<SessionId> {
    let mut first = None;
    for msg in messages
        .iter()
        .take_while(|message| message["role"] == "user")
    {
        let content = &msg["content"];
        let text = if let Some(text) = content.as_str() {
            vec![text]
        } else {
            content
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect()
        };
        for text in text {
            first.get_or_insert(text);
            let trimmed = text.trim_matches(js_space);
            let wrapped = ["system-notice", "system-reminder"].iter().any(|tag| {
                trimmed.starts_with(&format!("<{tag}>")) && trimmed.ends_with(&format!("</{tag}>"))
            });
            if !trimmed.is_empty() && !wrapped {
                return key(text);
            }
        }
    }
    first.and_then(key)
}

fn key(text: &str) -> Option<SessionId> {
    let text = prefix(text, 4096);
    (!text.is_empty()).then(|| Sha256::digest(text.as_bytes())[..4].try_into().unwrap())
}

fn session(key: Option<SessionId>) -> (bool, usize) {
    let Some(key) = key else {
        return (false, 0);
    };
    let mut entries = SESSIONS.lock().unwrap_or_else(|error| error.into_inner());
    let prior = entries
        .iter()
        .position(|entry| entry.key == key)
        .and_then(|index| entries.remove(index));
    let now = Instant::now();
    let cold = prior
        .as_ref()
        .is_some_and(|entry| now.duration_since(entry.seen) > COLD);
    let step = prior.map_or(0, |entry| entry.step);
    entries.push_back(Session {
        key,
        seen: now,
        step,
    });
    if entries.len() > SESSION_CAP {
        entries.pop_front();
    }
    (cold, step)
}

fn record_step(key: Option<SessionId>, step: usize) {
    let Some(key) = key else {
        return;
    };
    let mut entries = SESSIONS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(entry) = entries.iter_mut().find(|entry| entry.key == key) {
        entry.step = entry.step.max(step);
    }
}

pub(super) fn protected(messages: &[Value], slab: bool) -> usize {
    if slab {
        return messages
            .iter()
            .position(|message| message["role"] == "user")
            .map_or(0, |i| i + 1);
    }
    let Some(first) = messages.first() else {
        return 0;
    };
    let content = &first["content"];
    let reminder = |text: &str| text.contains("<system-reminder>");
    usize::from(
        content.as_str().is_some_and(reminder)
            || content.as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block["type"] == "text" && block["text"].as_str().is_some_and(reminder)
                })
            }),
    )
}

pub(super) fn collapse(
    messages: &mut Vec<Value>,
    budget: &mut ImageBudget,
    geometry: Geometry,
    keep: usize,
    session_key: Option<SessionId>,
    slab: bool,
) {
    let headroom = budget.image_headroom();
    if headroom == 0 {
        return;
    }
    let (cold, min_step) = session(session_key);
    let Some(plan) = plan(
        messages,
        headroom.min(HISTORY_BUDGET),
        geometry,
        keep,
        cold,
        min_step,
    ) else {
        return;
    };
    record_step(session_key, plan.step);
    let Some(mut out) = emit(messages, budget, geometry, keep, &plan) else {
        return;
    };
    if slab && let Some(ordinal) = out.carry {
        relocate(&mut out.messages, ordinal);
    }
    budget.info.count += out.images.info.count;
    budget.info.pixels += out.images.info.pixels;
    budget.info.bytes += out.images.info.bytes;
    *messages = out.messages;
}

fn boundary(messages: &[Value], cutoff: usize) -> usize {
    let mut open = HashSet::new();
    let mut closed = 0;
    let mut in_round = false;
    for (i, message) in messages.iter().take(cutoff).enumerate() {
        let assistant = message["role"] == "assistant";
        let new_uses = assistant && any_tool_use(message);
        if in_round && open.is_empty() && !new_uses {
            closed = i;
            in_round = false;
        }
        if assistant {
            in_round |= new_uses;
            for block in blocks(&message["content"]).filter(|item| item["type"] == "tool_use") {
                if let Some(id) = block["id"].as_str() {
                    open.insert(id);
                }
            }
        } else if message["role"] == "user" {
            for block in blocks(&message["content"]).filter(|item| item["type"] == "tool_result") {
                if let Some(id) = block["tool_use_id"].as_str() {
                    open.remove(id);
                }
            }
        }
        if open.is_empty() && !in_round {
            closed = i + 1;
        }
    }
    if in_round
        && open.is_empty()
        && !messages
            .get(cutoff)
            .is_some_and(|next| next["role"] == "assistant" && any_tool_use(next))
    {
        closed = cutoff.min(messages.len());
    }
    closed
}

fn any_tool_use(message: &Value) -> bool {
    blocks(&message["content"]).any(|block| block["type"] == "tool_use")
}

fn blocks(value: &Value) -> impl DoubleEndedIterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

fn marker(message: &Value) -> Option<&Value> {
    blocks(&message["content"])
        .rev()
        .find_map(|block| block.get("cache_control"))
}

struct Plan {
    end: usize,
    step: usize,
    transcript: String,
}

fn plan(
    messages: &[Value],
    budget: usize,
    geometry: Geometry,
    keep: usize,
    cold: bool,
    min_step: usize,
) -> Option<Plan> {
    let raw = messages.len().saturating_sub(4);
    let cutoff = raw.min((raw / 50 * 50).max(keep + 10));
    let mut end = boundary(messages, cutoff);
    if end < keep + 10 {
        return None;
    }
    let lengths: Vec<_> = (keep..end)
        .map(|index| {
            let transcript = segment(messages, index, index + 1).0;
            let typed = typed_text(&messages[index]);
            (
                if transcript.is_empty() {
                    0
                } else {
                    chars(&transcript) + 2
                },
                if chars(&typed) > USER_CAP {
                    chars(&typed) + 20
                } else {
                    0
                },
            )
        })
        .collect();
    let estimate = |slice: &[(usize, usize)]| {
        let (transcript, user) = slice
            .iter()
            .fold((0usize, 0usize), |(a, b), &(c, d)| (a + c, b + d));
        transcript.div_ceil(geometry.page_chars) + user.div_ceil(geometry.page_chars)
    };
    if estimate(&lengths) > budget {
        let mut sum = 0;
        let count = lengths
            .iter()
            .take_while(|&&(a, b)| {
                sum += a + b;
                sum <= budget * geometry.page_chars
            })
            .count();
        end = boundary(messages, keep + count);
        if end < keep + 10 {
            return None;
        }
    }
    let lengths = &lengths[..end - keep];
    let transcript = segment(messages, keep, end).0;
    let rendered = packed(&transcript);
    let cols = pxpipe_render::measure_columns(&rendered, geometry.columns, geometry.style.font);
    if !profitable(&rendered, cols, geometry, None, 2.0, geometry.page_chars) {
        return None;
    }
    let marks = messages[keep..end]
        .iter()
        .filter(|msg| marker(msg).is_some())
        .count();
    let mut step = 10;
    while step < min_step && step < lengths.len() {
        step *= 2;
    }
    let packed_pages = lengths
        .iter()
        .map(|&(a, _)| a)
        .sum::<usize>()
        .div_ceil(geometry.page_chars)
        .max(1);
    let goal = if cold {
        budget.min(packed_pages + 1)
    } else {
        budget
    };
    while step < lengths.len() && lengths.chunks(step).map(estimate).sum::<usize>() + marks > goal {
        step *= 2;
    }
    Some(Plan {
        end,
        step,
        transcript,
    })
}

struct Emitted {
    messages: Vec<Value>,
    images: ImageBudget,
    carry: Option<usize>,
}

fn emit(
    messages: &[Value],
    budget: &ImageBudget,
    geometry: Geometry,
    keep: usize,
    plan: &Plan,
) -> Option<Emitted> {
    let mut pending = ImageBudget {
        info: Default::default(),
        native_count: budget.native_count + budget.info.count,
        native_bytes: budget.native_bytes + budget.info.bytes,
    };
    let mut ends = BTreeMap::new();
    let mut grid_end = None;
    for end in (keep + plan.step..plan.end).step_by(plan.step) {
        ends.insert(end, None);
        grid_end = Some(end);
    }
    for (index, message) in messages.iter().enumerate().take(plan.end).skip(keep) {
        if let Some(marker) = marker(message) {
            ends.insert(index + 1, Some(marker));
        }
    }
    ends.entry(plan.end).or_insert(None);
    let mut data = vec![text_block(INTRO)];
    let mut start = keep;
    let mut carry = None;
    for (end, anchor) in ends {
        let (text, slots) = segment(messages, start, end);
        if !text.is_empty() {
            let pages = pxpipe_render::render_roles(
                &packed(&text),
                &packed(&slots),
                &pxpipe_render::Options {
                    style: geometry.style,
                    columns: geometry.columns,
                    max_height: geometry.max_height,
                    page_limit: pending.image_headroom(),
                    shrink: false,
                    ..Default::default()
                },
            )
            .ok()?;
            let mut images = pending.admit(pages, "", None);
            if let Some(marker) = anchor
                && let Some(last) = images.last_mut()
            {
                last["cache_control"] = marker.clone();
            }
            data.extend(images);
            if grid_end == Some(end) {
                carry = pending.info.count.checked_sub(1);
            }
        }
        user_blocks(&mut data, messages, start, end, &mut pending)?;
        start = end;
    }
    if pending.info.count == 0 || pending.info.bytes > budget.byte_headroom() {
        return None;
    }
    if let Some(pointer) = latest_user(messages, plan.end, keep) {
        data.push(text_block(&pointer));
    }
    let fact = pxpipe_factsheet::text(&plan.transcript);
    if !fact.is_empty() {
        data.push(text_block(&fact));
    }
    data.push(text_block(OUTRO));
    let mut output = messages[..keep]
        .iter()
        .enumerate()
        .map(|(index, message)| demote(message, index))
        .collect::<Vec<_>>();
    output.push(json!({"role":"user","content":data}));
    output.extend_from_slice(&messages[plan.end..]);
    Some(Emitted {
        messages: output,
        images: pending,
        carry,
    })
}

fn packed(text: &str) -> String {
    pxpipe_render::pack(&text.replace('↵', "⏎"))
}

fn segment(messages: &[Value], start: usize, end: usize) -> (String, String) {
    let mut text = Vec::new();
    let mut slots = Vec::new();
    for (idx, message) in messages.iter().enumerate().take(end).skip(start) {
        let assistant = message["role"] == "assistant";
        let content = &message["content"];
        let body = if assistant {
            transcript(content, |_| true)
        } else if content.is_array() {
            let (_, typed) = split_typed(content);
            transcript(content, |i| !typed.contains(&i))
        } else {
            String::new()
        };
        if body.trim_matches(js_space).is_empty() {
            continue;
        }
        let role = if assistant { "assistant" } else { "user" };
        let mark = if assistant { '\u{2}' } else { '\u{1}' };
        let start = format!("<{role} t=\"{idx}\">");
        let end = format!("</{role}>");
        let neutral = body.replace(['\u{1}', '\u{2}'], "\u{3}");
        let marker = |length| std::iter::repeat_n(mark, length).collect::<String>();
        slots.push(format!(
            "{}\n{neutral}\n{}",
            marker(start.len()),
            marker(end.len())
        ));
        text.push(format!("{start}\n{body}\n{end}"));
    }
    (text.join("\n\n"), slots.join("\n\n"))
}

fn transcript(content: &Value, include: impl Fn(usize) -> bool) -> String {
    if let Some(content) = content.as_str() {
        return content.to_owned();
    }
    let parts = blocks(content).enumerate().filter(|(i,_)| include(*i)).filter_map(|(_, block)| {
        match block["type"].as_str()? {
            "text" => Some(block["text"].as_str().unwrap_or_default().to_owned()),
            "tool_use" => {
                let input = block.get("input").map_or("undefined".to_owned(), |input| {
                    crate::chat_input::js_json(input, false).unwrap_or_else(|| input.to_string())
                });
                Some(format!("[tool_use {}]\n{input}", block["name"].as_str().unwrap_or("undefined")))
            }
            "tool_result" => {
                let content = &block["content"];
                let inner = content.as_str().map_or_else(|| {
                    blocks(content).filter_map(|part| match part["type"].as_str()? {
                        "text" => part["text"].as_str(),
                        "image" => Some("[image]"),
                        _ => None,
                    }).collect::<Vec<_>>().join("\n")
                }, str::to_owned);
                let corrected = FRESH.replace_all(&inner, "(state as of this PRIOR turn — the file may have changed since; Read it again before editing)");
                let error = if block["is_error"] == true { " (error)" } else {""};
                Some(format!("[tool_result{error}]\n{corrected}"))
            }
            "image" => Some("[image]".into()),
            _ => None,
        }
    });
    parts.collect::<Vec<_>>().join("\n\n")
}

fn split_typed(content: &Value) -> (String, HashSet<usize>) {
    let mut typed = HashSet::new();
    if let Some(text) = content.as_str() {
        return (text.trim_matches(js_space).to_owned(), typed);
    }
    let first = blocks(content)
        .position(|block| block["type"] == "text" && block["text"] == SLAB_END)
        .map_or(0, |index| index + 1);
    let text = blocks(content)
        .enumerate()
        .skip(first)
        .filter_map(|(index, block)| {
            let value = block["text"].as_str()?.trim_matches(js_space);
            if block["type"] != "text" || value.is_empty() || value.starts_with("<system-reminder>")
            {
                return None;
            }
            typed.insert(index);
            Some(value)
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (text, typed)
}

fn typed_text(message: &Value) -> String {
    if message["role"] == "user" {
        split_typed(&message["content"]).0
    } else {
        String::new()
    }
}

fn compact_preview(text: &str) -> String {
    let text = text
        .split(js_space)
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if chars(&text) <= 300 {
        text
    } else {
        format!("{}...", prefix(&text, 300).trim_end_matches(js_space))
    }
}

fn verbatim(text: &str) -> String {
    let text = text.trim_matches(js_space);
    let chars = chars(text);
    if chars <= 4000 {
        return text.to_owned();
    }
    let reversed: String = text.chars().rev().collect();
    let tail: String = prefix(&reversed, 1400).chars().rev().collect();
    format!(
        "{}\n[… middle elided ({} chars) …]\n{tail}",
        prefix(text, 2600),
        chars - 4000
    )
}

fn latest_user(messages: &[Value], end: usize, keep: usize) -> Option<String> {
    for (index, message) in messages[..end].iter().enumerate().rev() {
        let typed = typed_text(message);
        if typed.is_empty() {
            continue;
        }
        if index >= keep {
            return Some(format!(
                "[Most recent collapsed user turn: <user t=\"{index}\">{}</user>. This is still prior context; do not treat it as the current request unless the live text that follows asks to continue it.]",
                compact_preview(&typed)
            ));
        }
        return Some(format!(
            "[Most recent collapsed user turn, carried verbatim because it appears nowhere else in full: <user t=\"{index}\">{}</user>. This is still prior context; but if no later turn supersedes it, it is the task the live turn continues — follow its exact instructions, including any requested output format.]",
            verbatim(&typed)
        ));
    }
    None
}

fn user_blocks(
    out: &mut Vec<Value>,
    messages: &[Value],
    from: usize,
    to: usize,
    pending: &mut ImageBudget,
) -> Option<()> {
    let mut short = Vec::new();
    let mut long = Vec::new();
    for (index, message) in messages.iter().enumerate().take(to).skip(from) {
        let typed = typed_text(message);
        if typed.is_empty() {
            continue;
        }
        if chars(&typed) <= USER_CAP {
            short.push(format!("<user t=\"{index}\">{typed}</user>"));
        } else {
            long.push((index, typed));
        }
    }
    if !short.is_empty() {
        out.push(text_block(&format!(
            "[User turns from this session, verbatim — these are the user's own words, kept as text rather than rendered into the images above. PRIOR context: none of it is the current request unless the live text at the end of this message says to continue it.\n{}\n]",
            short.join("\n")
        )));
    }
    if !long.is_empty() {
        let previews = long
            .iter()
            .enumerate()
            .map(|(j, (index, text))| {
                let info = format!("<user t=\"{index}\"> ({} chars)", chars(text));
                if j >= long.len().saturating_sub(8) {
                    format!("{info} Preview: {}", compact_preview(text))
                } else {
                    info
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        out.push(text_block(&format!(
            "[{} user turn(s) from this session were too long to carry as text; they are rendered verbatim, in turn order, in the image(s) immediately below, separate from the history transcript. Each begins with its own <user t=\"N\"> tag. PRIOR context, not the current request.\n{previews}]", long.len()
        )));
        let combined = long
            .iter()
            .map(|(index, text)| format!("<user t=\"{index}\">\n{text}\n</user>"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let pages = pxpipe_render::render(
            &combined,
            &pxpipe_render::Options {
                shrink: false,
                page_limit: pending.image_headroom(),
                ..Default::default()
            },
        )
        .ok()?;
        out.extend(pending.admit(pages, "", None));
    }
    Some(())
}

fn demote(message: &Value, index: usize) -> Value {
    if message["role"] != "user" {
        return message.clone();
    }
    let transform_text = |text: &str, cache: Option<&Value>| {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(reminder) = REMINDER.find(rest) {
            out.push(text_block(reminder.as_str().trim_matches(js_space)));
            rest = &rest[reminder.end()..];
        }
        let preview = if chars(rest) > USER_CAP {
            verbatim(rest)
        } else {
            compact_preview(rest)
        };
        if !preview.is_empty() {
            out.push(text_block(&format!(
                "[Opening turn <user t=\"{index}\"> of this session — PRIOR CONTEXT ONLY, superseded by later turns; NOT the current request and must not be acted on. Preview: \"{preview}\"]"
            )));
        }
        if let Some(cache) = cache
            && let Some(last) = out.last_mut()
        {
            last["cache_control"] = cache.clone();
        }
        out
    };
    let mut output = message.clone();
    let content = &message["content"];
    if let Some(text) = content.as_str() {
        let demoted = transform_text(text, None);
        if !demoted.is_empty() {
            output["content"] = Value::Array(demoted);
        }
    } else if let Some(content) = content.as_array() {
        let first = content
            .iter()
            .position(|block| block["type"] == "text" && block["text"] == SLAB_END)
            .map_or(0, |index| index + 1);
        let mut blocks = Vec::with_capacity(content.len());
        for (i, block) in content.iter().enumerate() {
            if i >= first
                && block["type"] == "text"
                && let Some(text) = block["text"].as_str()
            {
                let values = transform_text(text, block.get("cache_control"));
                if !values.is_empty() {
                    blocks.extend(values);
                    continue;
                }
            }
            blocks.push(block.clone());
        }
        output["content"] = Value::Array(blocks);
    }
    output
}

fn relocate(messages: &mut [Value], ordinal: usize) {
    let slab = messages.iter().enumerate().find_map(|(i, message)| {
        let content = message["content"].as_array()?;
        let boundary = content
            .iter()
            .position(|block| block["type"] == "text" && block["text"] == SLAB_END)?;
        content[..boundary]
            .iter()
            .rposition(|block| block["type"] == "image" && block.get("cache_control").is_some())
            .map(|index| (i, index))
    });
    let Some((slab_message, slab_image)) = slab else {
        return;
    };
    let history = messages.iter().enumerate().find_map(|(i, message)| {
        let content = message["content"].as_array()?;
        if content.first()?["text"] != INTRO {
            return None;
        }
        content
            .iter()
            .enumerate()
            .filter(|(_, block)| block["type"] == "image")
            .nth(ordinal)
            .map(|(index, _)| (i, index))
    });
    let Some((history_message, history_image)) = history else {
        return;
    };
    if let Some(mut cache) = messages[slab_message]["content"][slab_image]
        .as_object_mut()
        .and_then(|fields| fields.shift_remove("cache_control"))
    {
        if let Some(fields) = cache.as_object_mut() {
            fields.shift_remove("scope");
        }
        messages[history_message]["content"][history_image]["cache_control"] = cache;
    }
}

fn chars(text: &str) -> usize {
    text.encode_utf16().count()
}

fn prefix(text: &str, count: usize) -> &str {
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        used += ch.len_utf16();
        if used > count {
            return &text[..index];
        }
    }
    text
}
