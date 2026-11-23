use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::{Geometry, ImageBudget, packed_text, profitable, text_block, visual_rows};
use crate::cli::js_space;
use crate::{pxpipe_factsheet, pxpipe_render};

const TOOL_RESULT_PAGES: usize = 10;
const MIN_CHARS: usize = 6000;

pub(super) fn compress(messages: &mut [Value], budget: &mut ImageBudget, geometry: Geometry) {
    for message in messages
        .iter_mut()
        .filter(|message| message["role"] == "user")
    {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for result in blocks
            .iter_mut()
            .filter(|block| block["type"] == "tool_result" && block["is_error"] != true)
        {
            let next = match &result["content"] {
                Value::String(text) => render(text, None, budget, geometry),
                Value::Array(parts) => {
                    let mut changed = false;
                    let mut next = Vec::new();
                    for part in parts {
                        let images = part["text"]
                            .as_str()
                            .filter(|_| part["type"] == "text")
                            .and_then(|text| {
                                render(text, part.get("cache_control"), budget, geometry)
                            });
                        if let Some(images) = images {
                            changed = true;
                            next.extend(images);
                        } else {
                            next.push(part.clone());
                        }
                    }
                    changed.then_some(next)
                }
                _ => None,
            };
            if let Some(content) = next {
                result["content"] = Value::Array(content);
            }
        }
    }
}

fn render(
    text: &str,
    cache: Option<&Value>,
    budget: &mut ImageBudget,
    geometry: Geometry,
) -> Option<Vec<Value>> {
    let headroom = budget.image_headroom();
    if headroom == 0 {
        return None;
    }
    let packed = packed_text(text);
    let columns = pxpipe_render::measure_columns(&packed, geometry.columns, geometry.style.font);
    if chars(&packed) < MIN_CHARS
        || !profitable(
            &packed,
            columns,
            geometry,
            Some(TOOL_RESULT_PAGES),
            4.0,
            geometry.page_chars,
        )
    {
        return None;
    }
    let paged = paginate(&packed, TOOL_RESULT_PAGES.min(headroom), geometry);
    let columns = pxpipe_render::measure_columns(&paged, geometry.columns, geometry.style.font);
    let pages = pxpipe_render::render(
        &paged,
        &pxpipe_render::Options {
            columns,
            max_height: geometry.max_height,
            style: geometry.style,
            shrink: false,
            page_limit: headroom,
            ..Default::default()
        },
    )
    .ok()?;
    if pages.iter().map(|page| page.png.len()).sum::<usize>() > budget.byte_headroom() {
        return None;
    }
    let mut blocks = budget.admit(pages, text, cache);
    let sheet = pxpipe_factsheet::text(text);
    if !sheet.is_empty() {
        blocks.push(text_block(&sheet));
    }
    Some(blocks)
}

fn chars(text: &str) -> usize {
    text.encode_utf16().count()
}

fn paginate(text: &str, max_pages: usize, geometry: Geometry) -> Cow<'_, str> {
    let columns = geometry.columns;
    let rows_per_page = geometry
        .hard_rows()
        .min((geometry.page_chars / columns).max(1));
    let total_chars = chars(text);
    let pages = visual_rows(text, columns)
        .div_ceil(rows_per_page)
        .max(total_chars.div_ceil(geometry.page_chars))
        .max(1);
    if pages <= max_pages {
        return Cow::Borrowed(text);
    }
    let row_budget = (max_pages * rows_per_page).saturating_sub(6).max(8);
    let char_budget = (max_pages * geometry.page_chars)
        .saturating_sub(512)
        .max(128);
    let delimiter = if text.contains('\n') { '\n' } else { '↵' };
    let lines: Vec<_> = text.split(delimiter).collect();
    let page_break = delimiter.to_string();
    let head_chars =
        |limit, rows| fit(lines.iter().copied(), delimiter, columns, limit, rows).max(1);
    let (head_count, tail_count) = if structured(text) {
        (head_chars(char_budget, row_budget), 0)
    } else {
        let head_char_budget = char_budget * 3 / 5;
        let head_row_budget = row_budget * 3 / 5;
        let head = head_chars(head_char_budget, head_row_budget);
        let tail = fit(
            lines[head..].iter().rev().copied(),
            delimiter,
            columns,
            char_budget - head_char_budget,
            row_budget - head_row_budget,
        );
        (head, tail)
    };
    let head = lines[..head_count].join(&page_break);
    let tail = lines[lines.len() - tail_count..].join(&page_break);
    let tail_note = if tail_count > 0 {
        format!(" Showing first {head_count} lines and last {tail_count} lines.")
    } else {
        format!(" Showing first {head_count} lines (tail elided).")
    };
    let marker = format!(
        "\n\n[ pxpipe paging: omitted {} lines ({} chars) of content here. Original length: {} chars ({} lines, ~{pages} images).{tail_note} ]\n\n",
        grouped(lines.len() - head_count - tail_count),
        grouped(total_chars - chars(&head) - chars(&tail)),
        grouped(total_chars),
        grouped(lines.len()),
    );
    Cow::Owned(head + &marker + &tail)
}

fn fit<'a>(
    lines: impl Iterator<Item = &'a str>,
    delimiter: char,
    columns: usize,
    char_limit: usize,
    row_limit: usize,
) -> usize {
    let mut budget_chars: usize = 0;
    let mut budget_rows: usize = 0;
    let mut accepted = 0;
    for (index, line) in lines.enumerate() {
        let length = chars(line);
        let cost_chars = length + usize::from(index > 0);
        let cost_rows = if delimiter == '↵' {
            (budget_chars + cost_chars).div_ceil(columns) - budget_chars.div_ceil(columns)
        } else {
            length.div_ceil(columns).max(1)
        };
        if budget_chars + cost_chars > char_limit || budget_rows + cost_rows > row_limit {
            break;
        }
        budget_chars += cost_chars;
        budget_rows += cost_rows;
        accepted += 1;
    }
    accepted
}

fn grouped(number: usize) -> String {
    let raw = number.to_string();
    let mut result = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, character) in raw.chars().enumerate() {
        if index > 0 && (raw.len() - index) % 3 == 0 {
            result.push(',');
        }
        result.push(character);
    }
    result
}

static STRUCTURED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"^(?:\{\s*(?:"|\})|\[\s*(?:"|\{|\[|-?[0-9]|true\b|false\b|null\b|\])|diff --git |---\s+\S)"#
).unwrap()
});

fn structured(text: &str) -> bool {
    let end = text
        .char_indices()
        .scan(0, |n, (index, character)| {
            *n += character.len_utf16();
            Some((index, *n))
        })
        .find_map(|(index, n)| (n > 4096).then_some(index))
        .unwrap_or(text.len());
    let head = text[..end].trim_start_matches(js_space);
    head.starts_with("---\n") || head.starts_with("---\r\n") || STRUCTURED.is_match(head)
}
