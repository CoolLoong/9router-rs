use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

use crate::chat_input::js_space;

#[path = "rtk_git.rs"]
mod git;

const WINDOW: usize = 1024;
const MIN_SIZE: usize = 500;
const MAX_SIZE: usize = 10 * 1024 * 1024;

macro_rules! rx {
    ($pattern:expr) => {
        LazyLock::new(|| Regex::new($pattern).expect("valid RTK filter pattern"))
    };
}

static GIT_LOG: LazyLock<Regex> = rx!(r"(?m)^[*|/\\ ]*commit [0-9a-f]{7,40}\r?$");
static GIT_DIFF: LazyLock<Regex> = rx!(r"(?m)^(diff --git |@@ )");
static GIT_STATUS: LazyLock<Regex> =
    rx!(r"(?m)^(On branch |nothing to commit|Changes (not |to be )|Untracked files:)");
static BUILD: LazyLock<Regex> = rx!(
    r"(?im)^(npm (warn|error|ERR!)|yarn (warn|error)|\s*Compiling\s+\S+|\s*Downloading\s+\S+|added [0-9]+ package|\[ERROR\]|BUILD (SUCCESS|FAILED)|\s*Finished\s+|Successfully (installed|built)|ERROR:)"
);
static LS_ROW: LazyLock<Regex> = rx!(r"(?m)^[-dlbcps][rwx-]{9}");
static LS_TOTAL: LazyLock<Regex> = rx!(r"(?m)^total [0-9]+\r?$");
static LS_DATE: LazyLock<Regex> = rx!(
    r"\s+(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+[0-9]{1,2}\s+([0-9]{4}|[0-9]{2}:[0-9]{2})\s+"
);
static SEARCH_HEADER: LazyLock<Regex> =
    rx!(r"^Result of search in '[^']*' \(total ([0-9]+) files?\):");
static NUMBERED: LazyLock<Regex> = rx!(r"^\s*[0-9]+\|");
static CARGO_CONTINUATION: LazyLock<Regex> = rx!(r"^\s*(-->|\||[0-9]+\s*\||=)");
static BUILD_PROGRESS: LazyLock<Regex> =
    rx!(r"(?i)^(?:(compiling)\s+\S+|downloading\s+\S+|fetching\s+)");
static BUILD_SUMMARY: LazyLock<Regex> = rx!(
    r"(?i)^(?:(added|removed|changed|audited|installed)\s+[0-9]+\s+package|finished\s+|build success|[0-9]+\s+(vulnerabilities|packages?|warnings?|errors?)|successfully (installed|built)|to address .* issues|run `npm (audit|fund)`)|packages are looking for funding"
);

#[derive(Clone, Copy)]
enum Filter {
    GitLog,
    GitDiff,
    GitStatus,
    Build,
    Grep,
    Find,
    Tree,
    Ls,
    Search,
    Numbered,
    Dedup,
    Truncate,
}

fn trim(line: &str) -> &str {
    line.trim_matches(js_space)
}

pub fn compress(raw: &str) -> Option<String> {
    let len = raw.encode_utf16().count();
    if !(MIN_SIZE..=MAX_SIZE).contains(&len) {
        return None;
    }
    let output = match detect(raw)? {
        Filter::GitLog => git::log(raw),
        Filter::GitDiff => git::diff(raw),
        Filter::GitStatus => git::status(raw),
        Filter::Build => build(raw),
        Filter::Grep => grep(raw),
        Filter::Find => find(raw),
        Filter::Tree => tree(raw),
        Filter::Ls => ls(raw),
        Filter::Search => search(raw),
        Filter::Numbered => truncate(raw, " (file continues)"),
        Filter::Dedup => dedup(raw),
        Filter::Truncate => truncate(raw, ""),
    };
    (!output.is_empty() && output.encode_utf16().count() < len).then_some(output)
}

fn detect(raw: &str) -> Option<Filter> {
    let preview = head(raw);
    for (pattern, rule) in [
        (&GIT_LOG, Filter::GitLog),
        (&GIT_DIFF, Filter::GitDiff),
        (&GIT_STATUS, Filter::GitStatus),
        (&BUILD, Filter::Build),
    ] {
        if pattern.is_match(preview) {
            return Some(rule);
        }
    }

    let nonempty = preview
        .split('\n')
        .filter(|line| !trim(line).is_empty())
        .collect::<Vec<_>>();
    if nonempty.len() >= 3
        && nonempty
            .iter()
            .filter(|line| {
                porcelain(line) && line[3..].chars().next().is_some_and(|ch| !js_space(ch))
            })
            .count()
            * 5
            >= nonempty.len() * 3
    {
        return Some(Filter::GitStatus);
    }
    if nonempty
        .iter()
        .take(5)
        .any(|line| grep_fields(line).is_some())
    {
        return Some(Filter::Grep);
    }
    if nonempty.len() >= 3 && nonempty.iter().all(|line| path_like(line)) {
        return Some(Filter::Find);
    }
    if ["├──", "└──", "│  "]
        .iter()
        .any(|glyph| preview.contains(glyph))
    {
        return Some(Filter::Tree);
    }
    if LS_TOTAL.is_match(preview) || LS_ROW.find_iter(preview).take(3).count() == 3 {
        return Some(Filter::Ls);
    }
    if SEARCH_HEADER.is_match(preview) {
        return Some(Filter::Search);
    }
    if preview.split('\n').count() >= 250 {
        let sample = preview
            .split('\n')
            .take(100)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if sample.len() >= 5
            && sample.iter().filter(|line| NUMBERED.is_match(line)).count() * 10 >= sample.len() * 7
        {
            return Some(Filter::Numbered);
        }
    }
    if nonempty.len() >= 5 {
        return Some(Filter::Dedup);
    }
    (raw.split('\n').count() >= 250).then_some(Filter::Truncate)
}

fn head(raw: &str) -> &str {
    let mut units = 0;
    for (offset, ch) in raw.char_indices() {
        units += ch.len_utf16();
        if units > WINDOW {
            return &raw[..offset];
        }
    }
    raw
}

pub(super) fn porcelain(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() >= 3
        && b" MADRCU?!".contains(&bytes[0])
        && b" MADRCU?!".contains(&bytes[1])
        && bytes[2] == b' '
}

fn path_like(path: &str) -> bool {
    let path = trim(path);
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
    {
        return true;
    }
    !path.contains(':') && (path.starts_with('.') || path.contains('/'))
}

fn grep_fields(line: &str) -> Option<(&str, &str, &str)> {
    let (file, remainder) = line.split_once(':')?;
    let (index, content) = remainder.split_once(':')?;
    (!index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some((file, index, content))
}

fn sorted<T>(map: HashMap<&str, T>) -> Vec<(&str, T)> {
    let mut list: Vec<_> = map.into_iter().collect();
    list.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
    list
}

fn grep(raw: &str) -> String {
    let mut by_file = HashMap::<_, Vec<_>>::new();
    let mut count = 0;
    for (file, index, text) in raw.split('\n').filter_map(grep_fields) {
        by_file.entry(file).or_default().push((index, text));
        count += 1;
    }
    if by_file.is_empty() {
        return raw.into();
    }
    let mut output = format!("{count} matches in {}F:\n\n", by_file.len());
    for (file, lines) in sorted(by_file) {
        let _ = writeln!(output, "[file] {file} ({}):", lines.len());
        for (index, text) in lines.iter().take(10) {
            let _ = writeln!(output, "  {index:>4}: {}", trim(text));
        }
        short_tail(&mut output, lines.len());
        output.push('\n');
    }
    output
}

fn short_tail(output: &mut String, length: usize) {
    if length > 10 {
        let _ = writeln!(output, "  +{}", length - 10);
    }
}

fn find(raw: &str) -> String {
    let lines = raw
        .split('\n')
        .filter(|line| !trim(line).is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return raw.into();
    }
    directories(&lines, None)
}

fn search(raw: &str) -> String {
    let mut lines = raw.split('\n');
    let start = lines.next().unwrap_or_default();
    let paths = lines
        .filter_map(|path| trim(path).strip_prefix("- "))
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return raw.into();
    }
    directories(&paths, Some(start))
        .trim_end_matches('\n')
        .to_owned()
}

fn directories(paths: &[&str], header: Option<&str>) -> String {
    let mut dirs = HashMap::<_, Vec<_>>::new();
    for path in paths {
        let separator = path.rfind(|ch| ch == '/' || header.is_none() && ch == '\\');
        let (dir, file) = separator.map_or((".", *path), |index| {
            let dir = &path[..index];
            (if dir.is_empty() { "/" } else { dir }, &path[index + 1..])
        });
        dirs.entry(dir).or_default().push(file);
    }
    let count = dirs.len();
    let mut output = String::new();
    if let Some(header) = header {
        let _ = writeln!(output, "{header}");
    }
    let _ = writeln!(output, "{} files in {count} dirs:\n", paths.len());
    for (dir, files) in sorted(dirs).into_iter().take(20) {
        if header.is_some() {
            let _ = writeln!(output, "{dir}/ ({}):", files.len());
        } else {
            let _ = writeln!(output, "{}/  ({})", dir.replace('\\', "/"), files.len());
        }
        for file in files.iter().take(10) {
            let _ = writeln!(output, "  {file}");
        }
        short_tail(&mut output, files.len());
        if header.is_some() {
            output.push('\n');
        }
    }
    if count > 20 {
        if header.is_none() {
            output.push('\n');
        }
        let _ = writeln!(output, "+{} more dirs", count - 20);
    }
    output
}

fn tree(raw: &str) -> String {
    let lines = raw
        .split('\n')
        .filter(|line| !(line.contains("director") && line.contains("file")))
        .skip_while(|line| trim(line).is_empty())
        .collect::<Vec<_>>();
    let count = lines
        .iter()
        .rposition(|line| !trim(line).is_empty())
        .map_or(0, |index| index + 1);
    let mut output = lines[..count.min(200)].join("\n");
    if count > 200 {
        let _ = write!(output, "\n... +{} more lines", count - 200);
    }
    output
}

fn truncate(raw: &str, suffix: &str) -> String {
    let lines = raw.split('\n').collect::<Vec<_>>();
    if lines.len() < 250 {
        return raw.into();
    }
    let count = lines.len();
    format!(
        "{}\n... +{} lines truncated{}\n{}",
        lines[..120].join("\n"),
        count - 180,
        suffix,
        lines[count - 60..].join("\n")
    )
}

fn dedup(raw: &str) -> String {
    let mut result = Vec::new();
    let mut previous = None;
    let mut repeats = 0;
    let mut blank = false;
    for line in raw.split('\n') {
        if trim(line).is_empty() {
            if !blank {
                result.push(line.to_owned());
            }
            blank = true;
            repeat(&mut result, &mut repeats);
            previous = None;
            continue;
        }
        blank = false;
        if previous == Some(line) {
            repeats += 1;
            continue;
        }
        repeat(&mut result, &mut repeats);
        result.push(line.to_owned());
        previous = Some(line);
        if result.len() >= 2000 {
            result.push("... (truncated at 2000 lines)".into());
            return result.join("\n");
        }
    }
    repeat(&mut result, &mut repeats);
    result.join("\n")
}

fn repeat(output: &mut Vec<String>, repeats: &mut usize) {
    if *repeats > 0 {
        output.push(format!("  ... ({repeats} duplicate lines)"));
        *repeats = 0;
    }
}

fn build(raw: &str) -> String {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut deprecations = Vec::new();
    let mut summary = Vec::new();
    let mut compiling = 0;
    let mut downloading = 0;
    let mut in_cargo = false;
    for line in raw.split('\n') {
        let trimmed = trim(line);
        if in_cargo {
            if trimmed.is_empty() {
                in_cargo = false;
                continue;
            }
            if CARGO_CONTINUATION.is_match(line) {
                errors.push(line);
                continue;
            }
            in_cargo = false;
        }
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("npm err!")
            || lower.starts_with("npm error")
            || lower.starts_with("yarn error")
        {
            errors.push(line);
        } else if lower.starts_with("npm warn deprecated") {
            deprecations.push(line);
        } else if lower.starts_with("npm warn") || lower.starts_with("yarn warn") {
            warnings.push(line);
        } else if lower.starts_with("error[")
            || lower.starts_with("error:")
            || trimmed.starts_with("error -->")
        {
            errors.push(line);
            in_cargo = true;
        } else if lower.starts_with("warning[")
            || lower.starts_with("warning:")
            || trimmed.starts_with("warning -->")
        {
            warnings.push(line);
            in_cargo = true;
        } else if lower.starts_with("[error]") || lower.starts_with("build failed") {
            errors.push(line);
        } else if lower.starts_with("[warning]") {
            warnings.push(line);
        } else if let Some(progress) = BUILD_PROGRESS.captures(trimmed) {
            if progress.get(1).is_some() {
                compiling += 1
            } else {
                downloading += 1
            }
        } else if BUILD_SUMMARY.is_match(trimmed) {
            summary.push(line);
        }
    }
    let mut output = String::new();
    print_block(&mut output, &deprecations, 3, "deprecated packages");
    if compiling > 0 {
        let _ = writeln!(output, "Compiled {compiling} packages");
    }
    if downloading > 0 {
        let _ = writeln!(output, "Downloaded {downloading} packages");
    }
    for line in errors {
        let _ = writeln!(output, "{line}");
    }
    print_block(&mut output, &warnings, 5, "warnings");
    for line in summary {
        let _ = writeln!(output, "{line}");
    }
    let output = output.trim_end_matches('\n');
    if output.is_empty() {
        raw.into()
    } else {
        output.into()
    }
}

fn print_block(output: &mut String, lines: &[&str], keep: usize, label: &str) {
    for line in lines.iter().take(keep) {
        let _ = writeln!(output, "{line}");
    }
    if lines.len() > keep {
        let _ = writeln!(output, "... +{} more {label}", lines.len() - keep);
    }
}

fn ls(raw: &str) -> String {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut extensions = HashMap::<_, (usize, usize)>::new();
    const SKIP: &[&str] = &[
        ".",
        "..",
        "node_modules",
        ".git",
        "target",
        "__pycache__",
        ".next",
        "dist",
        "build",
        ".cache",
        ".turbo",
        ".vercel",
        ".pytest_cache",
        ".mypy_cache",
        ".tox",
        ".venv",
        "venv",
        "env",
        "coverage",
        ".nyc_output",
        ".DS_Store",
        "Thumbs.db",
        ".idea",
        ".vscode",
        ".vs",
        "*.egg-info",
        ".eggs",
    ];
    for line in raw
        .split('\n')
        .filter(|line| !line.starts_with("total ") && !line.is_empty())
    {
        let Some(date) = LS_DATE.find(line) else {
            continue;
        };
        let name = &line[date.end()..];
        if SKIP.contains(&name) {
            continue;
        }
        let fields = line[..date.start()]
            .split(js_space)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        if fields.len() < 4 {
            continue;
        }
        match fields[0].as_bytes().first() {
            Some(b'd') => dirs.push(name),
            Some(b'-' | b'l') => {
                let bytes = fields
                    .iter()
                    .rev()
                    .find_map(|field| {
                        field
                            .parse::<i64>()
                            .ok()
                            .filter(|value| value.to_string() == **field)
                    })
                    .unwrap_or(0);
                let key = name
                    .rfind('.')
                    .filter(|index| *index > 0)
                    .map_or("no ext", |index| &name[index..]);
                let index = extensions.len();
                extensions.entry(key).or_insert((index, 0)).1 += 1;
                files.push((name, bytes));
            }
            _ => (),
        }
    }
    if dirs.is_empty() && files.is_empty() {
        return raw.into();
    }
    let mut output = String::new();
    for dir in &dirs {
        let _ = writeln!(output, "{dir}/");
    }
    for (name, bytes) in &files {
        let size = if *bytes >= 1_048_576 {
            unit_size(*bytes, 1_048_576, "M")
        } else if *bytes >= 1024 {
            unit_size(*bytes, 1024, "K")
        } else {
            format!("{bytes}B")
        };
        let _ = writeln!(output, "{name}  {size}");
    }
    let _ = write!(
        output,
        "\nSummary: {} files, {} dirs",
        files.len(),
        dirs.len()
    );
    if !extensions.is_empty() {
        let mut ext: Vec<_> = extensions.into_iter().collect();
        ext.sort_by_key(|(_, (index, count))| (std::cmp::Reverse(*count), *index));
        let shown = ext
            .iter()
            .take(5)
            .map(|(name, (_, count))| format!("{count} {name}"))
            .collect::<Vec<_>>();
        let _ = write!(output, " ({})", shown.join(", "));
        if ext.len() > 5 {
            output.pop();
            let _ = write!(output, ", +{} more)", ext.len() - 5);
        }
    }
    output
}

fn unit_size(bytes: i64, divisor: i128, unit: &str) -> String {
    let tenths = (i128::from(bytes) * 10 + divisor / 2) / divisor;
    format!("{}.{}{unit}", tenths / 10, tenths % 10)
}
