use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

use super::{porcelain, trim};
use crate::chat_input::js_space;

macro_rules! regex {
    ($pattern:expr) => {
        LazyLock::new(|| Regex::new($pattern).expect("valid git output pattern"))
    };
}

static HEADER: LazyLock<Regex> =
    regex!(r"(?i)^(commit [0-9a-f]{7,40}$|[*|/\\ ]+commit [0-9a-f]{7,40})");
static AUTHOR_DATE: LazyLock<Regex> = regex!(r"(?i)^[*|/\\ ]*(Author|Date):");
static SUBJECT: LazyLock<Regex> = regex!(r"^[*|/\\ ]*    \S");
static STAT_SUMMARY: LazyLock<Regex> = regex!(r"^[0-9]+ file[A-Za-z0-9_]* changed");
static GRAPH_COMMIT: LazyLock<Regex> = regex!(r"(?i)^[*|/\\ ]+([0-9a-f]{7,40}\s+.+)");
static SHORT_COMMIT: LazyLock<Regex> = regex!(r"^[0-9a-f]{7,40}\s+");
static LONG_STATUS: LazyLock<Regex> =
    regex!(r"^\s*(modified|new file|deleted|renamed|both modified):\s+(.+)$");

pub fn log(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut output = Vec::new();
    let mut skipped = 0;
    let mut in_commit = false;
    let mut seen_subject = false;
    let mut write = |line: String| {
        if output.len() < 200 {
            output.push(line)
        } else {
            skipped += 1
        }
    };
    for line in raw.split('\n') {
        let raw_line = line.trim_end_matches(js_space);
        let line = trim(raw_line);
        if HEADER.is_match(line) {
            in_commit = true;
            seen_subject = false;
            write(raw_line.into());
        } else if in_commit {
            if AUTHOR_DATE.is_match(line) {
                write(line.into());
            } else if !line.is_empty() && !seen_subject && SUBJECT.is_match(raw_line) {
                write(format!("  Subject: {line}"));
                seen_subject = true;
            } else if STAT_SUMMARY.is_match(line) {
                write(format!("  {line}"));
            } else if line.starts_with("diff --git ") {
                write("  ... diff body omitted".into());
            }
        } else if let Some(graph) = GRAPH_COMMIT
            .captures(line)
            .and_then(|captures| captures.get(1))
        {
            write(graph.as_str().to_owned());
        } else if SHORT_COMMIT.is_match(line)
            || !line.contains(['*', '|', '/', '\\'])
            || line.chars().any(|ch| !"*|/\\ ".contains(ch))
        {
            write(line.into());
        }
    }
    if skipped > 0 {
        output.push(format!("... ({skipped} more lines)"));
    }
    let result = output.join("\n");
    if result.is_empty() || result.encode_utf16().count() > raw.encode_utf16().count() {
        raw.into()
    } else {
        result
    }
}

pub fn diff(raw: &str) -> String {
    let mut output = Vec::new();
    let mut current = "";
    let mut added = 0;
    let mut removed = 0;
    let mut shown = 0;
    let mut hidden = 0;
    let mut in_hunk = false;
    let mut truncated = false;
    for line in raw.split('\n') {
        if line.starts_with("diff --git") {
            finish_hunk(&mut output, &mut hidden, &mut truncated);
            summarize(&mut output, current, added, removed);
            current = line.split_once(" b/").map_or("unknown", |(_, path)| path);
            output.push(format!("\n{current}"));
            added = 0;
            removed = 0;
            shown = 0;
            in_hunk = false;
        } else if line.starts_with("@@") {
            finish_hunk(&mut output, &mut hidden, &mut truncated);
            in_hunk = true;
            shown = 0;
            output.push(format!("  {line}"));
        } else if in_hunk {
            let is_add = line.starts_with('+') && !line.starts_with("+++");
            let is_remove = line.starts_with('-') && !line.starts_with("---");
            added += usize::from(is_add);
            removed += usize::from(is_remove);
            if is_add || is_remove {
                if shown < 100 {
                    output.push(format!("  {line}"));
                    shown += 1
                } else {
                    hidden += 1
                }
            } else if (1..100).contains(&shown) && !line.starts_with('\\') {
                output.push(format!("  {line}"));
                shown += 1;
            }
        }
        if output.len() >= 500 {
            output.push("\n... (more changes truncated)".into());
            truncated = true;
            break;
        }
    }
    finish_hunk(&mut output, &mut hidden, &mut truncated);
    summarize(&mut output, current, added, removed);
    if truncated {
        output.push("[full diff: rtk git diff --no-compact]".into());
    }
    output.join("\n")
}

fn finish_hunk(output: &mut Vec<String>, hidden: &mut usize, truncated: &mut bool) {
    if *hidden > 0 {
        output.push(format!("  ... ({hidden} lines truncated)"));
        *truncated = true;
        *hidden = 0;
    }
}

fn summarize(output: &mut Vec<String>, file: &str, added: usize, removed: usize) {
    if !file.is_empty() && (added > 0 || removed > 0) {
        output.push(format!("  +{added} -{removed}"));
    }
}

pub fn status(raw: &str) -> String {
    if !raw.contains('\n') && trim(raw).is_empty() {
        return "Clean working tree".into();
    }
    let mut branch = "";
    let mut staged = Vec::new();
    let mut modified = Vec::new();
    let mut untracked = Vec::new();
    let mut conflicts = 0;
    for line in raw.split('\n').filter(|line| !trim(line).is_empty()) {
        if let Some(value) = line.strip_prefix("On branch ")
            && let Some(name) = value.split(js_space).next().filter(|name| !name.is_empty())
        {
            branch = name;
            continue;
        }
        if let Some(name) = line.strip_prefix("##") {
            branch = name.trim_start_matches(js_space);
            continue;
        }
        if porcelain(line) {
            let file = &line[3..];
            let mark = &line.as_bytes()[..2];
            if mark == b"??" {
                untracked.push(file);
                continue;
            }
            if b"MADRC".contains(&mark[0]) {
                staged.push(file);
            } else if mark[0] == b'U' {
                conflicts += 1;
            }
            if matches!(mark[1], b'M' | b'D') {
                modified.push(file);
            }
            continue;
        }
        if let Some(details) = LONG_STATUS.captures(line) {
            let kind = details
                .get(1)
                .map(|value| value.as_str())
                .unwrap_or_default();
            let path = details
                .get(2)
                .map(|value| trim(value.as_str()))
                .unwrap_or_default();
            match kind {
                "both modified" => conflicts += 1,
                "modified" | "deleted" => modified.push(path),
                _ => staged.push(path),
            }
        }
    }
    let mut output = String::new();
    if !branch.is_empty() {
        let _ = writeln!(output, "* {branch}");
    }
    for (name, files) in [
        ("+ Staged", &staged),
        ("~ Modified", &modified),
        ("? Untracked", &untracked),
    ] {
        if files.is_empty() {
            continue;
        }
        let _ = writeln!(output, "{name}: {} files", files.len());
        for file in files.iter().take(10) {
            let _ = writeln!(output, "   {file}");
        }
        if files.len() > 10 {
            let _ = writeln!(output, "   ... +{} more", files.len() - 10);
        }
    }
    if conflicts > 0 {
        let _ = writeln!(output, "conflicts: {conflicts} files");
    } else if staged.is_empty() && modified.is_empty() && untracked.is_empty() {
        output.push_str("clean — nothing to commit\n");
    }
    output.trim_end_matches('\n').to_owned()
}
