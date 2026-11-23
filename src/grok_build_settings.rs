use std::{
    fs,
    io::ErrorKind,
    ops::Range,
    path::{Path, PathBuf},
};

use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::StatusCode,
    response::Response,
    routing::get,
};
use provider_capabilities_rs::port::model_capabilities;
use regex::{NoExpand, Regex};
use serde_json::{Map, Value, json};

use super::{CliTools, executable, in_config, persist, reply, resolve_key};
use crate::{provider_connections::truthy, provider_index::js_trim, thinking_intent::numeric};

const ROLES: [&str; 3] = ["general-purpose", "explore", "plan"];
const UNSET: &str = "__9router_unset__";
const SUBAGENTS: &str = "subagents.models";

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router.route(
        "/api/cli-tools/grok-build-settings",
        get(status).post(apply).delete(reset),
    )
}

pub(super) async fn host_status() -> Option<Value> {
    tokio::task::spawn_blocking(check).await.ok()?.ok()
}

fn path() -> Result<PathBuf, String> {
    Ok(std::env::home_dir()
        .ok_or("Missing home directory")?
        .join(".grok/config.toml"))
}

fn read(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn check() -> Result<Value, String> {
    let path = path()?;
    if !executable("grok")
        && !path.exists()
        && !path
            .parent()
            .is_some_and(|parent| parent.join("bin/grok").is_file())
    {
        return Ok(
            json!({"installed":false,"settings":null,"message":"Grok Build is not installed"}),
        );
    }
    let settings = Document(read(&path)?.unwrap_or_default()).status();
    let configured = truthy(&settings["model"]["base_url"]);
    Ok(json!({"installed":true,"settings":settings,"has9Router":configured,"configPath":path}))
}

async fn status(State(state): State<CliTools>) -> Response {
    response(
        in_config(state.editing, check).await,
        "Failed to check grok-build settings",
    )
}

async fn apply(State(state): State<CliTools>, request: Request) -> Response {
    let failure = "Failed to update grok-build settings";
    let Some(body) = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|value| !value.is_null())
    else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, failure);
    };
    let model = body["model"].as_str().map(js_trim).unwrap_or_default();
    if !truthy(&body["baseUrl"]) || model.is_empty() {
        return error(StatusCode::BAD_REQUEST, "baseUrl and model are required");
    }
    let Some(base) = body["baseUrl"].as_str() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, failure);
    };
    let base = if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    };
    let key = resolve_key(&state.storage, body["apiKey"].as_str()).await;
    let catalog = state.storage.model_catalog();
    let model = model.to_owned();
    let result = in_config(state.editing, move || {
        let path = path()?;
        let mut doc = Document(read(&path)?.unwrap_or_default());
        if let Some(previous) = doc.field("models", "default").filter(|value| !value.is_empty() && value != "9router") {
            doc.remember("default", &previous);
        }
        doc.model("9router", "9Router", &model, &base, &key, window(&body["contextWindow"], &model, catalog.as_deref()));
        doc.set("models", "default", "9router");
        if let Some(overrides) = body.get("subagentModels") {
            for role in ROLES {
                let input = &overrides[role];
                let submodel = input.as_str().or_else(||input["model"].as_str())
                    .map(js_trim).filter(|model| !model.is_empty());
                if let Some(submodel) = submodel {
                    let slot = format!("9router-{role}");
                    let previous = doc.field(SUBAGENTS, role).unwrap_or_else(|| UNSET.into());
                    doc.remember(&format!("subagent-{role}"), &previous);
                    doc.model(&slot, &format!("9Router {role}"), submodel, &base, &key,
                        window(&input["contextWindow"], submodel, catalog.as_deref()));
                    doc.set(SUBAGENTS, role, &slot);
                } else {
                    doc.reset_role(role);
                }
            }
        }
        persist(&path, Some(doc.0.as_bytes()))?;
        Ok(json!({"success":true,"message":"Grok Build settings applied successfully!","configPath":path,"modelSlot":"9router"}))
    }).await;
    response(result, failure)
}

async fn reset(State(state): State<CliTools>) -> Response {
    response(
        in_config(state.editing, || {
            let path = path()?;
            let Some(document) = read(&path)? else {
                return Ok(json!({"success":true,"message":"No config file to reset"}));
            };
            let mut doc = Document(document);
            for role in ROLES {
                doc.reset_role(role);
            }
            doc.remove("model.9router");
            let previous = doc
                .take_marker("default")
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "grok-build".into());
            if doc.field("models", "default").as_deref() == Some("9router") {
                doc.set("models", "default", &previous);
            }
            doc.compact();
            persist(&path, Some(doc.0.as_bytes()))?;
            Ok(json!({"success":true,"message":"9router model slots removed from Grok Build"}))
        })
        .await,
        "Failed to reset grok-build settings",
    )
}

fn window(value: &Value, model: &str, catalog: Option<&Value>) -> Option<u64> {
    if let Some(explicit) = numeric(value).filter(|value| *value > 0.0) {
        return Some(explicit.floor() as u64);
    }
    let (provider, model) = model
        .split_once('/')
        .filter(|(provider, _)| !provider.is_empty())
        .map_or((None, model), |(provider, model)| (Some(provider), model));
    model_capabilities(provider, model, catalog)["contextWindow"].as_u64()
}

struct Document(String);

impl Document {
    fn ranges(&self, name: &str) -> Option<(Range<usize>, Range<usize>)> {
        let header = format!("[{name}]");
        let mut offset = 0;
        let mut matched = None;
        for line in self.0.split_inclusive('\n') {
            if let Some((start, content)) = matched {
                if line.starts_with('[') {
                    return Some((start..offset, content..offset));
                }
            } else if line.ends_with('\n')
                && line.trim_end_matches(['\r', '\n', ' ', '\t']) == header
            {
                matched = Some((offset, offset + line.len()));
            }
            offset += line.len();
        }
        matched.map(|(start, content)| (start..offset, content..offset))
    }

    fn field(&self, section: &str, key: &str) -> Option<String> {
        let (_, content) = self.ranges(section)?;
        field_pattern(key, r#""([^"]*)""#)
            .captures(&self.0[content])?
            .get(1)
            .map(|value| value.as_str().to_owned())
    }

    fn append(&mut self, lines: &str) {
        if !self.0.is_empty() && !self.0.ends_with('\n') {
            self.0.push('\n');
        }
        self.0.push_str(lines);
    }

    fn set(&mut self, section: &str, key: &str, value: &str) {
        let line = format!("{key} = {}", quote(value));
        let Some((whole, content)) = self.ranges(section) else {
            self.append(&format!("\n[{section}]\n{line}\n"));
            return;
        };
        let raw = &self.0[content];
        let pattern = field_pattern(key, r#""[^"]*""#);
        let updated = if pattern.is_match(raw) {
            pattern.replace(raw, NoExpand(&line)).into_owned()
        } else {
            format!("{line}\n{raw}")
        };
        self.0
            .replace_range(whole, &format!("[{section}]\n{updated}"));
    }

    fn model(
        &mut self,
        slot: &str,
        name: &str,
        model: &str,
        base: &str,
        key: &str,
        context: Option<u64>,
    ) {
        use std::fmt::Write;
        let section = format!("model.{slot}");
        let mut lines = format!(
            "[{section}]\nmodel = {}\nbase_url = {}\nname = {}\ndescription = \"Routed via 9Router gateway\"\napi_backend = \"chat_completions\"\n",
            quote(model),
            quote(base),
            quote(name)
        );
        if !key.is_empty() {
            let _ = writeln!(lines, "api_key = {}", quote(key));
        }
        if let Some(context) = context.filter(|context| *context > 0) {
            let _ = writeln!(lines, "context_window = {context}");
        }
        if let Some((whole, _)) = self.ranges(&section) {
            self.0.replace_range(whole, &lines);
        } else {
            self.append(&format!("\n{lines}"));
        }
    }

    fn read_model(&self, slot: &str) -> Value {
        let section = format!("model.{slot}");
        let Some((_, content)) = self.ranges(&section) else {
            return Value::Null;
        };
        let raw = &self.0[content];
        let context = field_pattern("context_window", r"([0-9]+(?:\.[0-9]+)?)")
            .captures(raw)
            .and_then(|captures| captures.get(1)?.as_str().parse::<f64>().ok())
            .filter(|value| *value > 0.0 && value.is_finite());
        let raw =
            field_pattern("api_key", r"[^\r\n]*").replace_all(raw, r#"api_key = "[redacted]""#);
        let string = |key| self.field(&section, key);
        json!({"model":string("model"),"base_url":string("base_url"),"name":string("name"),
            "api_key":null,"api_backend":string("api_backend"),"context_window":context,"raw":raw})
    }

    fn status(&self) -> Value {
        let mut models = Map::new();
        let mut mappings = Map::new();
        for role in ROLES {
            let mapping = self.field(SUBAGENTS, role);
            let slot = format!("9router-{role}");
            let model = if mapping.as_deref() == Some(&slot) {
                self.read_model(&slot)
            } else {
                Value::Null
            };
            models.insert(role.into(), model);
            mappings.insert(role.into(), json!(mapping));
        }
        json!({"model":self.read_model("9router"),"default":self.field("models","default"),
            "subagentModels":models,"subagentMappings":mappings})
    }

    fn remember(&mut self, key: &str, value: &str) {
        if marker_pattern(key).is_match(&self.0) {
            return;
        }
        let marker = format!("# 9router-prev-{key} = {}\n", quote(value));
        if let Some((whole, _)) = self.ranges("model.9router") {
            self.0.insert_str(whole.start, &marker);
        } else {
            self.append(&marker);
        }
    }

    fn take_marker(&mut self, key: &str) -> Option<String> {
        let pattern = marker_pattern(key);
        let captures = pattern.captures(&self.0)?;
        let value = captures.get(1)?.as_str().to_owned();
        let whole = captures.get(0)?;
        self.0.replace_range(whole.start()..whole.end(), "");
        Some(value)
    }

    fn reset_role(&mut self, role: &str) {
        let slot = format!("9router-{role}");
        let previous = self
            .take_marker(&format!("subagent-{role}"))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| UNSET.into());
        if self.field(SUBAGENTS, role).as_deref() == Some(&slot) {
            if previous == UNSET {
                if let Some((whole, content)) = self.ranges(SUBAGENTS) {
                    let next = field_pattern(role, r"[^\r\n]*\r?\n?")
                        .replace(&self.0[content], "")
                        .into_owned();
                    if next.trim().is_empty() {
                        self.0.replace_range(whole, "");
                    } else {
                        self.0
                            .replace_range(whole, &format!("[{SUBAGENTS}]\n{next}"));
                    }
                }
            } else {
                self.set(SUBAGENTS, role, &previous);
            }
        }
        self.remove(&format!("model.{slot}"));
    }

    fn remove(&mut self, name: &str) {
        if let Some((whole, _)) = self.ranges(name) {
            self.0.replace_range(whole, "");
        }
        self.compact();
    }

    fn compact(&mut self) {
        self.0 = Regex::new(r"\n{3,}")
            .unwrap()
            .replace_all(&self.0, "\n\n")
            .into_owned();
    }
}

fn field_pattern(key: &str, value: &str) -> Regex {
    Regex::new(&format!(
        r"(?m)^[ \t]*{}[ \t]*=[ \t]*{value}",
        regex::escape(key)
    ))
    .unwrap()
}

fn marker_pattern(key: &str) -> Regex {
    Regex::new(&format!(
        r#"(?m)^# 9router-prev-{} = "([^"]*)"[ \t]*\r?\n?"#,
        regex::escape(key)
    ))
    .unwrap()
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("TOML string")
}

fn response(result: Result<Value, String>, failure: &str) -> Response {
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, failure),
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}))
}
