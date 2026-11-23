use std::sync::LazyLock;

use regex::{NoExpand, Regex};
use serde_json::{Map, Value, json};

use crate::provider_index::js_trim;

const KEY_REF: &str = "${OPENAI_API_KEY}";
const TOP_BODY: &str = r"((?:[ \t]+[^\r\n]*\r?\n?|[ \t]*\r?\n)*)";
const CHILD_BODY: &str = r"((?:[ \t]{4,}[^\r\n]*\r?\n?|[ \t]*\r?\n)*)";

static MODEL: LazyLock<Regex> = LazyLock::new(|| top("model"));
static MODEL_SCALAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^model:[^\r\n]*\r?\n?").unwrap());
static DELEGATION: LazyLock<Regex> = LazyLock::new(|| top("delegation"));
static AUXILIARY: LazyLock<Regex> = LazyLock::new(|| top("auxiliary"));
static ROLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?m)^  ([A-Za-z0-9_]+):[ \t]*\r?\n{CHILD_BODY}")).unwrap()
});
static STT: LazyLock<Regex> = LazyLock::new(|| top("stt"));
static OPENAI: LazyLock<Regex> = LazyLock::new(|| child("openai"));
static FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^[ \t]+([A-Za-z0-9_]+):[ \t]*["']?([^"'\r\n]+)"#).unwrap());
static ENV_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^OPENAI_API_KEY=[^\r\n]*").unwrap());
static DISPLAY_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^display_name:[ \t]*["']?([^"'\r\n]+)"#).unwrap());

fn top(key: &str) -> Regex {
    Regex::new(&format!(r"(?m)^{key}:[ \t]*\r?\n{TOP_BODY}")).unwrap()
}

fn child(key: &str) -> Regex {
    let key = regex::escape(key);
    Regex::new(&format!(r"(?m)^  {key}:[ \t]*\r?\n{CHILD_BODY}")).unwrap()
}

fn contents<'a>(pattern: &Regex, yaml: &'a str) -> Option<&'a str> {
    Some(pattern.captures(yaml)?.get(1)?.as_str())
}

fn fields(body: &str, names: &[&str]) -> Value {
    let mut found = Map::new();
    for capture in FIELD.captures_iter(body) {
        found
            .entry(&capture[1])
            .or_insert_with(|| json!(js_trim(&capture[2])));
    }
    Value::Object(
        names
            .iter()
            .map(|name| {
                (
                    (*name).into(),
                    found.shift_remove(*name).unwrap_or(Value::Null),
                )
            })
            .collect(),
    )
}

pub(super) fn model(yaml: &str) -> Value {
    contents(&MODEL, yaml).map_or(Value::Null, |body| {
        fields(body, &["default", "provider", "base_url", "api_key"])
    })
}

pub(super) fn delegation(yaml: &str) -> Value {
    contents(&DELEGATION, yaml).map_or(Value::Null, |body| {
        fields(body, &["model", "provider", "base_url"])
    })
}

pub(super) fn auxiliary(yaml: &str) -> Map<String, Value> {
    let body = contents(&AUXILIARY, yaml).unwrap_or_default();
    ROLE.captures_iter(body)
        .map(|role| {
            (
                role[1].to_owned(),
                fields(&role[2], &["model", "provider", "base_url"]),
            )
        })
        .collect()
}

pub(super) fn custom(block: &Value) -> bool {
    block["provider"] == "custom"
}

pub(super) fn local(block: &Value) -> bool {
    custom(block)
        && block["base_url"].as_str().is_some_and(|url| {
            ["localhost", "127.0.0.1", "0.0.0.0"]
                .iter()
                .any(|needle| url.contains(needle))
        })
}

pub(super) fn display_name(yaml: &str) -> Option<&str> {
    contents(&DISPLAY_NAME, yaml).map(js_trim)
}

fn string_key(value: &str) -> String {
    json!(value).to_string()
}

fn append_block(yaml: &str, block: &str) -> String {
    let newline = if yaml.is_empty() || yaml.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{yaml}{newline}{block}")
}

pub(super) fn apply_model(yaml: &str, model: &str, base: &str) -> String {
    let block = format!(
        "model:\n  default: {}\n  provider: \"custom\"\n  base_url: {}\n  api_key: {KEY_REF}\n",
        string_key(model),
        string_key(base)
    );
    if MODEL.is_match(yaml) {
        MODEL.replace(yaml, NoExpand(&block)).into_owned()
    } else if MODEL_SCALAR.is_match(yaml) {
        MODEL_SCALAR.replace(yaml, NoExpand(&block)).into_owned()
    } else if yaml.is_empty() {
        block
    } else {
        format!("{block}\n{yaml}")
    }
}

pub(super) fn apply_delegation(yaml: &str, model: &str, base: &str) -> String {
    let block = format!(
        "delegation:\n  model: {}\n  provider: \"custom\"\n  base_url: {}\n  api_key: {KEY_REF}\n",
        string_key(model),
        string_key(base)
    );
    if DELEGATION.is_match(yaml) {
        DELEGATION.replace(yaml, NoExpand(&block)).into_owned()
    } else {
        append_block(yaml, &block)
    }
}

pub(super) fn apply_role(yaml: &str, role: &str, model: &str, base: &str) -> String {
    let block = format!(
        "  {role}:\n    provider: \"custom\"\n    model: {}\n    base_url: {}\n    api_key: {KEY_REF}\n",
        string_key(model),
        string_key(base)
    );
    let Some(body) = contents(&AUXILIARY, yaml) else {
        return append_block(yaml, &format!("auxiliary:\n{block}"));
    };
    let pattern = child(role);
    let body = if pattern.is_match(body) {
        pattern.replace(body, NoExpand(&block)).into_owned()
    } else {
        format!("{body}{block}")
    };
    AUXILIARY
        .replace(yaml, NoExpand(&format!("auxiliary:\n{body}")))
        .into_owned()
}

pub(super) fn remove_model(yaml: &str) -> String {
    MODEL.replace(yaml, "").trim_start_matches('\n').to_owned()
}

pub(super) fn remove_delegation(yaml: &str) -> String {
    DELEGATION.replace(yaml, "").into_owned()
}

pub(super) fn remove_role(yaml: &str, role: &str) -> String {
    let Some(body) = contents(&AUXILIARY, yaml) else {
        return yaml.to_owned();
    };
    let body = child(role).replace(body, "");
    if js_trim(&body).is_empty() {
        AUXILIARY.replace(yaml, "").into_owned()
    } else {
        AUXILIARY
            .replace(yaml, NoExpand(&format!("auxiliary:\n{body}")))
            .into_owned()
    }
}

pub(super) fn apply_env(env: &str, key: &str) -> String {
    let line = format!("OPENAI_API_KEY={key}");
    if ENV_KEY.is_match(env) {
        ENV_KEY.replace(env, NoExpand(&line)).into_owned()
    } else {
        append_block(env, &format!("{line}\n"))
    }
}

fn unquote(value: &str) -> &str {
    let raw = value.trim_start_matches([' ', '\t']);
    let raw = raw.strip_prefix(['"', '\'']).unwrap_or(raw);
    let end = raw.trim_end_matches([' ', '\t']);
    let raw = end.strip_suffix(['"', '\'']).unwrap_or(raw);
    js_trim(raw)
}

fn direct_field(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    Some((key, value.trim_start_matches([' ', '\t'])))
}

pub(super) fn stt(yaml: &str) -> Value {
    let Some(body) = contents(&STT, yaml) else {
        return Value::Null;
    };
    let mut direct = Map::new();
    let mut openai = Map::new();
    let mut inside_openai = false;
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("  ") else {
            continue;
        };
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            let entry = direct_field(rest);
            inside_openai = entry == Some(("openai", ""));
            if let Some((key, value)) = entry.filter(|(_, value)| !value.is_empty()) {
                direct.insert(key.into(), json!(unquote(value)));
            }
        } else if inside_openai {
            let nested = line.trim_start_matches(' ');
            if line.len() - nested.len() >= 4
                && let Some((key, value)) =
                    direct_field(nested).filter(|(_, value)| !value.is_empty())
            {
                openai.insert(key.into(), json!(unquote(value)));
            }
        }
    }
    json!({
        "enabled":direct.get("enabled"),"provider":direct.get("provider"),
        "model":openai.get("model"),"base_url":openai.get("base_url"),"api_key":openai.get("api_key"),
    })
}

pub(super) fn managed_stt(stt: &Value) -> bool {
    stt["provider"] == "openai"
        && stt["base_url"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        && stt["api_key"] == KEY_REF
}

fn upsert_line(body: &str, spaces: usize, key: &str, value: &str) -> String {
    let spacing = " ".repeat(spaces);
    let pattern = Regex::new(&format!(r"(?m)^{spacing}{key}:[^\r\n]*")).unwrap();
    let line = format!("{spacing}{key}: {value}");
    if pattern.is_match(body) {
        pattern.replace(body, NoExpand(&line)).into_owned()
    } else {
        append_block(body, &format!("{line}\n"))
    }
}

pub(super) fn apply_stt(yaml: &str, model: &str, base: &str) -> String {
    let (model, base) = (string_key(model), string_key(base));
    let fields = format!("    model: {model}\n    base_url: {base}\n    api_key: {KEY_REF}\n");
    let Some(body) = contents(&STT, yaml) else {
        return append_block(
            yaml,
            &format!("stt:\n  enabled: true\n  provider: \"openai\"\n  openai:\n{fields}"),
        );
    };
    let body = upsert_line(body, 2, "enabled", "true");
    let body = upsert_line(&body, 2, "provider", "\"openai\"");
    let body = if let Some(existing) = contents(&OPENAI, &body) {
        let nested = upsert_line(existing, 4, "model", &model);
        let nested = upsert_line(&nested, 4, "base_url", &base);
        let nested = upsert_line(&nested, 4, "api_key", KEY_REF);
        OPENAI
            .replace(&body, NoExpand(&format!("  openai:\n{nested}")))
            .into_owned()
    } else {
        append_block(&body, &format!("  openai:\n{fields}"))
    };
    STT.replace(yaml, NoExpand(&format!("stt:\n{body}")))
        .into_owned()
}

pub(super) fn remove_stt(yaml: &str) -> String {
    static ENABLED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)^  enabled:[ \t]*["']?true["']?[ \t]*(?:\r?\n|$)"#).unwrap()
    });
    static PROVIDER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)^  provider:[ \t]*["']?openai["']?[ \t]*(?:\r?\n|$)"#).unwrap()
    });
    let Some(body) = contents(&STT, yaml) else {
        return yaml.to_owned();
    };
    let body = ENABLED.replace(body, "");
    let body = PROVIDER.replace(&body, "");
    let output;
    let body = if let Some(nested) = contents(&OPENAI, &body) {
        let mut nested = nested.to_owned();
        for key in ["model", "base_url", "api_key"] {
            let pattern = Regex::new(&format!(r"(?m)^    {key}:[^\r\n]*\r?\n?")).unwrap();
            nested = pattern.replace(&nested, "").into_owned();
        }
        let block = if js_trim(&nested).is_empty() {
            String::new()
        } else {
            format!("  openai:\n{nested}")
        };
        output = OPENAI.replace(&body, NoExpand(&block));
        &output
    } else {
        &body
    };
    if js_trim(body).is_empty() {
        STT.replace(yaml, "").trim_start_matches('\n').to_owned()
    } else {
        STT.replace(yaml, NoExpand(&format!("stt:\n{body}")))
            .into_owned()
    }
}
