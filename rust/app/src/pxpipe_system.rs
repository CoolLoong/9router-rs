use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use crate::cli::js_space;

static BILLING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|\n)(x-anthropic-billing-header:[^\n]*)(\n?)").unwrap());
static ENVIRONMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\n)(# Environment(?-u:\b))").unwrap());

pub(super) fn strip_billing(text: &str) -> Cow<'_, str> {
    let Some(captures) = BILLING.captures(text) else {
        return Cow::Borrowed(text);
    };
    let span = captures.get(0).unwrap();
    let start = if captures[3].is_empty() {
        span.start()
    } else {
        span.start() + captures[1].len()
    };
    Cow::Owned(format!("{}{}", &text[..start], &text[span.end()..]))
}

pub(super) fn split_environment(text: &str) -> (Cow<'_, str>, String) {
    let Some(captures) = ENVIRONMENT.captures(text) else {
        return (Cow::Borrowed(text), String::new());
    };
    let header = captures.get(1).unwrap();
    let next_heading = text[header.end()..]
        .match_indices('\n')
        .find_map(|(relative, _)| {
            let offset = header.end() + relative;
            let line = &text[offset + 1..];
            let count = line.bytes().take_while(|byte| *byte == b'#').count();
            ((1..=6).contains(&count) && line[count..].chars().next().is_some_and(js_space))
                .then_some(offset)
        })
        .unwrap_or(text.len());
    let before = captures.get(0).unwrap().start();
    (
        Cow::Owned(format!("{}{}", &text[..before], &text[next_heading..])),
        text[header.start()..next_heading]
            .trim_end_matches(js_space)
            .to_owned(),
    )
}
