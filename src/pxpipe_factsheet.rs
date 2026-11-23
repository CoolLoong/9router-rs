use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::{Regex, RegexSet};

const MAX_SCAN: usize = 262_144;
const PAGE_CHARS: usize = 28_080;
const MAX_CHUNK: usize = 512;
const MAX_TOKENS: usize = 96;
const MAX_URLS: usize = 8;
const MAX_SEEN: usize = 2048;

const UUID: &str = "[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}";
const EMAIL: &str = r"[A-Za-z0-9.!#$%&'*+/=?^_\x60{|}~-]+@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)+";
const IBAN: &str = r"[A-Z]{2}\d{2}[A-Z0-9]{8,30}";
const CURRENCY: &str = r"(?:[$€£¥]|(?:USD|EUR|GBP|CAD|AUD|CHF|JPY))\d(?:[\d,_]*\d)?(?:\.\d{2})?";
const CONST: &str = r"[A-Z][A-Z0-9]{2,}(?:_[A-Z0-9]+)+";
const CAMEL: &str = r"(?:[a-z]+|[A-Z][a-z0-9]+)(?:[A-Z][a-z0-9]*)+";
const TICKET: &str = r"[A-Z][A-Z0-9]+(?:-[A-Z0-9]+)+";
const HEX: &str = r"[0-9a-f]{7,40}";
const FLAG: &str = r"--?[A-Za-z][\w-]+";

fn ascii(regex: &str) -> String {
    regex
        .replace(r"\b", r"(?-u:\b)")
        .replace(r"\d", "[0-9]")
        .replace(r"\w", "[A-Za-z0-9_]")
}

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    let word = |pattern: &str| format!(r"\b{pattern}\b");
    [
        r#"\b[A-Z][A-Z0-9_]{2,}=[^\s)"'<>]+"#.to_owned(),
        r"\b[A-Za-z][A-Za-z0-9_]{2,}=[A-Za-z0-9_.:/+-]{1,64}".to_owned(),
        r#"\bhttps?://[^\s)"'<>]+"#.to_owned(),
        word(EMAIL),
        word(UUID),
        word(IBAN),
        format!(r"{CURRENCY}\b"),
        r"(?:[\w@~+-]+)?(?:/[\w.@+-]+)+\.[A-Za-z]\w{0,8}\b".to_owned(),
        r"/[\w.@+-]+(?:/[\w.@+-]+)+/?".to_owned(),
        word(HEX),
        r"\bv?\d+\.\d+(?:\.\d+)?(?:[-+][\w.]+)?\b".to_owned(),
        format!(r"(?:^|[^\w-])({FLAG})"),
        r"\b\d[\d,_]{3,}\b".to_owned(),
        r"\b\d+\.\d+\b".to_owned(),
        word(CONST),
        word(CAMEL),
        word(TICKET),
    ]
    .into_iter()
    .map(|pattern| Regex::new(&ascii(&pattern)).expect("PXPIPE factsheet regex"))
    .collect()
});

static PRIORITY: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new(
        [
            r"[A-Z][A-Z0-9_]{2,}=\S+",
            UUID,
            EMAIL,
            IBAN,
            CURRENCY,
            CONST,
            FLAG,
            r"(?:\d[\d,_]*|\d+\.\d+)",
            CAMEL,
            TICKET,
            HEX,
        ]
        .into_iter()
        .map(|pattern| format!("^(?:{})$", ascii(pattern))),
    )
    .expect("PXPIPE factsheet priorities")
});

#[derive(Clone, Copy)]
struct Entry<'a> {
    token: &'a str,
    chars: usize,
    count: usize,
    tier: u8,
}

pub fn text(source: &str) -> String {
    let entries = if source.encode_utf16().count() <= MAX_SCAN {
        extract(source)
    } else {
        let mut remaining = source;
        let mut counts = HashMap::new();
        while !remaining.is_empty() {
            let (page, rest) = remaining.split_at(utf16_end(remaining, PAGE_CHARS));
            for entry in extract(page) {
                *counts.entry(entry.token).or_default() += entry.count;
            }
            remaining = rest;
        }
        rank(
            counts
                .into_iter()
                .map(|(token, count)| entry(token, count))
                .collect(),
        )
    };
    if entries.is_empty() {
        return String::new();
    }
    let counts = entries.iter().any(|entry| entry.count > 1);
    let mut output = String::from(
        "[Exact identifiers from the rendered context above (paths, ids, versions, numbers) — quote these verbatim instead of transcribing them from the image",
    );
    if counts {
        output.push_str("; ×N marks a token that occurs N times within the imaged content");
    }
    output.push_str(": ");
    for (index, entry) in entries.iter().enumerate() {
        if index != 0 {
            output.push_str(" · ");
        }
        output.push_str(entry.token);
        if entry.count > 1 {
            use std::fmt::Write;
            write!(output, " ×{}", entry.count).expect("writing to a string");
        }
    }
    output.push(']');
    output
}

fn utf16_end(text: &str, max: usize) -> usize {
    let mut count = 0;
    for (index, ch) in text.char_indices() {
        count += ch.len_utf16();
        if count > max {
            return index;
        }
    }
    text.len()
}

fn extract(source: &str) -> Vec<Entry<'_>> {
    let mut counts = HashMap::new();
    for chunk in source.split(crate::cli::js_space) {
        if !(3..=MAX_CHUNK).contains(&chunk.encode_utf16().count()) {
            continue;
        }
        let mut spans = HashSet::new();
        for (index, regex) in PATTERNS.iter().enumerate() {
            for captures in regex.captures_iter(chunk) {
                let whole = captures.get(0).expect("a matched pattern");
                let token = captures
                    .get(1)
                    .unwrap_or(whole)
                    .as_str()
                    .trim_matches(crate::cli::js_space)
                    .trim_end_matches(['.', ',', ';', ':', '!', '?']);
                if !(3..=120).contains(&token.encode_utf16().count())
                    || index == 9 && !has_digit(token)
                    || index == 16 && !ticket_has_digit(&chunk[whole.start()..])
                {
                    continue;
                }
                if spans.insert((token, whole.start())) {
                    *counts.entry(token).or_default() += 1;
                }
            }
        }
        if counts.len() >= MAX_SEEN {
            break;
        }
    }
    let mut all = counts
        .into_iter()
        .map(|(token, count)| entry(token, count))
        .collect::<Vec<_>>();
    all.sort_unstable_by(longer_first);
    let mut specific: Vec<Entry<'_>> = Vec::new();
    for entry in all {
        if !specific
            .iter()
            .any(|longer| longer.token.contains(entry.token))
        {
            specific.push(entry);
        }
    }
    rank(specific)
}

fn has_digit(value: &str) -> bool {
    value.bytes().any(|b| b.is_ascii_digit())
}

fn ticket_has_digit(value: &str) -> bool {
    value
        .bytes()
        .take(120)
        .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'-')
        .any(|b| b.is_ascii_digit())
}

fn entry(token: &str, count: usize) -> Entry<'_> {
    let chars = token.encode_utf16().count();
    let matches = PRIORITY.matches(token);
    let tier = if matches.iter().any(|i| match i {
        8 => chars >= 8,
        9 | 10 => has_digit(token),
        _ => true,
    }) {
        0
    } else if token.starts_with("https://") || token.starts_with("http://") {
        2
    } else {
        1
    };
    Entry {
        token,
        chars,
        count,
        tier,
    }
}

fn longer_first(a: &Entry<'_>, b: &Entry<'_>) -> Ordering {
    b.chars
        .cmp(&a.chars)
        .then_with(|| a.token.encode_utf16().cmp(b.token.encode_utf16()))
}

fn rank(mut entries: Vec<Entry<'_>>) -> Vec<Entry<'_>> {
    entries.sort_unstable_by(|a, b| a.tier.cmp(&b.tier).then_with(|| longer_first(a, b)));
    let mut urls = 0;
    entries
        .into_iter()
        .filter(|entry| {
            if entry.tier == 2 {
                urls += 1;
                urls <= MAX_URLS
            } else {
                true
            }
        })
        .take(MAX_TOKENS)
        .collect()
}
