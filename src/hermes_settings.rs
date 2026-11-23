use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

use super::{
    CliTools, PendingFile, executable, in_config, persist, persist_private_batch, read, reply,
};

#[path = "hermes_layout.rs"]
mod layout;

const IDENTITY: [&str; 6] = [
    "config.yaml",
    ".env",
    "SOUL.md",
    "profile.yaml",
    "auth.json",
    "state.db",
];
const INVALID_PROFILE: &str =
    "Invalid Hermes profile name (letters, digits, - and _ only, max 64 chars)";

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router
        .route(
            "/api/cli-tools/hermes-settings",
            get(status).post(apply).delete(reset),
        )
        .route("/api/cli-tools/hermes-profiles", get(profiles))
}

pub(super) async fn host_status() -> Option<Value> {
    tokio::task::spawn_blocking(|| {
        let root = hermes_root().ok()?;
        if !installed(&root) {
            return Some(not_installed());
        }
        profile_status(&Profile::default(&root)).ok()
    })
    .await
    .ok()?
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    reply(status, json!({"error":message.into()}))
}

fn finish(result: Result<Response, String>, message: &str) -> Response {
    result.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, message))
}

async fn status(State(state): State<CliTools>, request: Request) -> Response {
    let name = query(&request, "profile");
    finish(
        in_config(state.editing, move || {
            let root = hermes_root()?;
            if !installed(&root) {
                return Ok(reply(StatusCode::OK, not_installed()));
            }
            let profile = match select_profile(&root, name.as_deref()) {
                Ok(profile) => profile,
                Err((status, message)) => return Ok(error(status, message)),
            };
            Ok(reply(StatusCode::OK, profile_status(&profile)?))
        })
        .await,
        "Failed to check hermes settings",
    )
}

async fn profiles(State(state): State<CliTools>) -> Response {
    finish(
        in_config(state.editing, || {
            let mut entries = Vec::new();
            for profile in list_profiles(&hermes_root()?)? {
                let text = read(&profile.config()).map(|raw| decode(raw.as_deref()))?;
                let model = layout::model(&text);
                let delegation = layout::delegation(&text);
                let auxiliary = layout::auxiliary(&text);
                let meta = read(&profile.dir.join("profile.yaml")).map(|raw| decode(raw.as_deref()))?;
                entries.push(json!({
                    "name":profile.name,"dir":profile.dir,"isDefault":profile.is_default,
                    "displayName":layout::display_name(&meta),
                    "command":if profile.is_default { "hermes".into() } else { format!("hermes -p {}",profile.name) },
                    "alias":(!profile.is_default).then_some(&profile.name),
                    "model":model.get("default").filter(|value|truthy(value)),
                    "baseUrl":model.get("base_url").filter(|value|truthy(value)),
                    "has9Router":layout::local(&model)||layout::local(&delegation)||auxiliary.values().any(layout::local),
                }));
            }
            Ok(reply(StatusCode::OK, json!({"profiles":entries})))
        })
        .await,
        "Failed to list hermes profiles",
    )
}

async fn body(request: Request) -> Option<Value> {
    let bytes = to_bytes(request.into_body(), 128 * 1024 * 1024)
        .await
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn query(request: &Request, field: &str) -> Option<String> {
    url::form_urlencoded::parse(request.uri().query()?.as_bytes())
        .find_map(|(name, value)| (name == field).then_some(value.into_owned()))
        .filter(|value| !value.is_empty())
}

fn field_or_query<'a>(body: &'a Value, field: &str, query: Option<&'a str>) -> Option<&'a str> {
    match body.get(field).filter(|value| !value.is_null()) {
        Some(value) => value.as_str(),
        None => query,
    }
}

fn enabled(value: &Value) -> bool {
    value == true || value == "true" || value == "1"
}

async fn apply(State(state): State<CliTools>, request: Request) -> Response {
    let selected = query(&request, "profile");
    let bulk = query(&request, "applyToAll");
    let failed = "Failed to update hermes settings";
    let Some(body) = body(request).await.filter(|value| !value.is_null()) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, failed);
    };
    let mut selections: Vec<_> = body["selections"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| truthy(&item["role"]) && truthy(&item["model"]))
        .map(|item| (js_text(&item["role"]), js_text(&item["model"])))
        .collect();
    if selections.is_empty() && truthy(&body["model"]) {
        selections.push(("default".into(), js_text(&body["model"])));
    }
    let main = selections.iter().find(|(role, _)| role == "default");
    let audio = truthy(&body["audio"]).then_some(&body["audio"]);
    if !truthy(&body["baseUrl"]) || (main.is_none() && audio.is_none()) {
        return error(StatusCode::BAD_REQUEST, "baseUrl and model are required");
    }
    let transcription = audio.map(|audio| {
        let model = audio["model"]
            .as_str()
            .map(js_trim)
            .unwrap_or_default()
            .to_owned();
        (enabled(&audio["enabled"]), model)
    });
    if transcription
        .as_ref()
        .is_some_and(|(enabled, model)| *enabled && model.is_empty())
    {
        return error(
            StatusCode::BAD_REQUEST,
            "audio.model is required when enabling audio transcription",
        );
    }
    let Some(base) = body["baseUrl"].as_str() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, failed);
    };
    let base = if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    };
    let key = body.get("apiKey").filter(|key| truthy(key)).map(js_text);
    if key
        .as_deref()
        .is_some_and(|key| key.contains(['\r', '\n', '\0']))
    {
        return error(StatusCode::BAD_REQUEST, "apiKey must fit on one line");
    }
    let all = body
        .get("applyToAll")
        .filter(|value| !value.is_null())
        .map_or_else(|| matches!(bulk.as_deref(), Some("true" | "1")), enabled);
    let name = field_or_query(&body, "profile", selected.as_deref()).map(str::to_owned);
    finish(
        in_config(state.editing, move || {
            let root = hermes_root()?;
            if all {
                let main = selections.iter().find(|(role, _)| role == "default").map(|(_, model)| model.as_str());
                return Ok(reply(StatusCode::OK, apply_all(&root, &base, key.as_deref(), main)?));
            }
            if selections.iter().any(|(role, _)| !role.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))) {
                return Ok(error(StatusCode::BAD_REQUEST, "Invalid Hermes role"));
            }
            let profile = match select_profile(&root, name.as_deref()) {
                Ok(profile) => profile,
                Err((status, message)) => return Ok(error(status, message)),
            };
            let previous = read(&profile.config())?;
            let mut yaml = decode(previous.as_deref());
            for (role, model) in selections {
                yaml = match role.as_str() {
                    "default" => layout::apply_model(&yaml, &model, &base),
                    "delegation" => layout::apply_delegation(&yaml, &model, &base),
                    role => layout::apply_role(&yaml, role, &model, &base),
                };
            }
            match transcription {
                Some((true, model)) => yaml = layout::apply_stt(&yaml, &model, &base),
                Some((false, _)) if layout::managed_stt(&layout::stt(&yaml)) => yaml = layout::remove_stt(&yaml),
                _ => (),
            }
            save_config(&profile, previous, &yaml, key.as_deref())?;
            let message = if profile.is_default {
                "Hermes settings applied successfully!".to_owned()
            } else {
                format!("Hermes settings applied to profile \"{}\"!", profile.name)
            };
            Ok(reply(StatusCode::OK, json!({
                "success":true,"message":message,"profile":profile.identity(),
                "stt9Router":layout::managed_stt(&layout::stt(&yaml)),"configPath":profile.config(),
            })))
        })
        .await,
        failed,
    )
}

async fn reset(State(state): State<CliTools>, request: Request) -> Response {
    let selected = query(&request, "profile");
    let body = body(request).await.unwrap_or_default();
    let name = field_or_query(&body, "profile", selected.as_deref()).map(str::to_owned);
    finish(
        in_config(state.editing, move || {
            let profile = match select_profile(&hermes_root()?, name.as_deref()) {
                Ok(profile) => profile,
                Err((status, message)) => return Ok(error(status, message)),
            };
            let path = profile.config();
            let Some(raw) = read(&path)? else {
                return Ok(reply(StatusCode::OK, json!({
                    "success":true,"profile":profile.name,"message":"No config file to reset",
                })));
            };
            let mut yaml = decode(Some(&raw));
            let mut kept = Vec::new();
            let model = layout::model(&yaml);
            let removed_model = layout::custom(&model);
            if removed_model {
                yaml = layout::remove_model(&yaml);
            } else if !model.is_null() {
                kept.push("model");
            }
            let delegation = layout::delegation(&yaml);
            let removed_delegation = layout::custom(&delegation);
            if removed_delegation {
                yaml = layout::remove_delegation(&yaml);
            } else if !delegation.is_null() {
                kept.push("delegation");
            }
            let mut removed_auxiliary = Vec::new();
            for (role, settings) in layout::auxiliary(&yaml) {
                if layout::custom(&settings) {
                    yaml = layout::remove_role(&yaml, &role);
                    removed_auxiliary.push(role);
                }
            }
            let stt = layout::stt(&yaml);
            let removed_stt = layout::managed_stt(&stt);
            if removed_stt {
                yaml = layout::remove_stt(&yaml);
            }
            if if removed_stt { !layout::stt(&yaml).is_null() } else { !stt.is_null() } {
                kept.push("stt");
            }
            persist(&path, Some(yaml.trim_start_matches('\n').as_bytes()))?;
            let scope = if profile.is_default { String::new() } else { format!(" in profile \"{}\"", profile.name) };
            Ok(reply(StatusCode::OK, json!({
                "success":true,"profile":profile.name,
                "removed":{"model":removed_model,"delegation":removed_delegation,"auxiliary":removed_auxiliary,"stt":removed_stt},
                "kept":kept,"message":format!("9router model blocks removed{scope}"),
            })))
        })
        .await,
        "Failed to reset hermes settings",
    )
}

fn apply_all(
    root: &Path,
    base: &str,
    key: Option<&str>,
    requested: Option<&str>,
) -> Result<Value, String> {
    let profiles = list_profiles(root)?;
    for profile in &profiles {
        read(&profile.config())?;
        read(&profile.dir.join("profile.yaml"))?;
    }
    let results: Vec<_> = profiles
        .into_iter()
        .map(|profile| {
            update_profile_endpoint(&profile, base, key, requested).unwrap_or_else(
                |reason| json!({"profile":profile.name,"status":"failed","reason":reason}),
            )
        })
        .collect();
    let updated = results
        .iter()
        .filter(|item| item["status"] == "updated")
        .count();
    Ok(json!({
        "success":updated>0,"bulk":true,"results":results,"updated":updated,
        "skipped":results.len()-updated,"message":format!("Updated endpoint on {updated} profile(s)"),
    }))
}

fn update_profile_endpoint(
    profile: &Profile,
    base: &str,
    key: Option<&str>,
    requested: Option<&str>,
) -> Result<Value, String> {
    let previous = read(&profile.config())?;
    let mut yaml = decode(previous.as_deref());
    let main = layout::model(&yaml);
    let model = main["default"].as_str().filter(|model| !model.is_empty());
    let keep_existing = layout::custom(&main) && model.is_some();
    let next = match (keep_existing, model, requested) {
        (true, Some(model), _) => model,
        (false, Some(_), _) => {
            let provider = main["provider"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or("unknown");
            return Ok(json!({
                "profile":profile.name,"status":"skipped",
                "reason":format!("main model uses provider \"{provider}\" — configure this profile individually"),
            }));
        }
        (_, None, Some(model)) if !model.is_empty() => model,
        _ => {
            return Ok(
                json!({"profile":profile.name,"status":"skipped","reason":"no main model configured"}),
            );
        }
    };
    yaml = layout::apply_model(&yaml, next, base);
    if keep_existing {
        let delegation = layout::delegation(&yaml);
        if layout::custom(&delegation)
            && let Some(model) = delegation["model"]
                .as_str()
                .filter(|model| !model.is_empty())
        {
            yaml = layout::apply_delegation(&yaml, model, base);
        }
        for (role, settings) in layout::auxiliary(&yaml) {
            if layout::custom(&settings)
                && let Some(model) = settings["model"].as_str().filter(|model| !model.is_empty())
            {
                yaml = layout::apply_role(&yaml, &role, model, base);
            }
        }
    }
    save_config(profile, previous, &yaml, key)?;
    Ok(json!({"profile":profile.name,"status":"updated","model":next}))
}

fn decode(raw: Option<&[u8]>) -> String {
    String::from_utf8_lossy(raw.unwrap_or_default()).into_owned()
}

fn save_config(
    profile: &Profile,
    previous: Option<Vec<u8>>,
    yaml: &str,
    key: Option<&str>,
) -> Result<(), String> {
    let path = profile.config();
    if let Some(key) = key {
        let env = profile.dir.join(".env");
        if let (Ok(config), Ok(credentials)) = (path.canonicalize(), env.canonicalize())
            && config == credentials
        {
            return Err("Hermes config and credential file must be distinct".into());
        }
        let env_raw = read(&env)?;
        let next = layout::apply_env(&decode(env_raw.as_deref()), key).into_bytes();
        persist_private_batch(
            &[PendingFile {
                path,
                previous,
                next: yaml.as_bytes().to_vec(),
            }],
            &PendingFile {
                path: env,
                previous: env_raw,
                next,
            },
        )
    } else {
        persist(&path, Some(yaml.as_bytes()))
    }
}

fn hermes_root() -> Result<PathBuf, String> {
    std::env::home_dir()
        .map(|home| home.join(".hermes"))
        .ok_or_else(|| "Unable to locate the home directory".into())
}

fn installed(root: &Path) -> bool {
    executable("hermes") || root.join("config.yaml").exists()
}

fn not_installed() -> Value {
    json!({"installed":false,"settings":null,"message":"Hermes Agent is not installed"})
}

struct Profile {
    dir: PathBuf,
    name: String,
    is_default: bool,
}

impl Profile {
    fn default(root: &Path) -> Self {
        Self {
            dir: root.to_owned(),
            name: "default".into(),
            is_default: true,
        }
    }

    fn named(root: &Path, name: &str) -> Self {
        Self {
            dir: root.join("profiles").join(name),
            name: name.to_owned(),
            is_default: false,
        }
    }

    fn config(&self) -> PathBuf {
        self.dir.join("config.yaml")
    }

    fn identity(&self) -> Value {
        json!({"name":self.name,"dir":self.dir,"isDefault":self.is_default})
    }
}

fn valid_profile(name: &str) -> bool {
    name.len() <= 64
        && !name.eq_ignore_ascii_case("default")
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn select_profile(root: &Path, name: Option<&str>) -> Result<Profile, (StatusCode, String)> {
    let name = name.map(js_trim).unwrap_or_default();
    if name.is_empty() || name == "default" {
        return Ok(Profile::default(root));
    }
    if !valid_profile(name) {
        return Err((StatusCode::BAD_REQUEST, INVALID_PROFILE.to_owned()));
    }
    let profile = Profile::named(root, name);
    if !profile.dir.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Hermes profile \"{name}\" not found"),
        ));
    }
    let within_root = fs::canonicalize(root)
        .ok()
        .zip(fs::canonicalize(&profile.dir).ok())
        .is_some_and(|(root, dir)| dir.starts_with(root.join("profiles")));
    if !within_root {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid Hermes profile path".to_owned(),
        ));
    }
    Ok(profile)
}

fn list_profiles(root: &Path) -> Result<Vec<Profile>, String> {
    let mut profiles = vec![Profile::default(root)];
    let dir = root.join("profiles");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(profiles),
        Err(error) => return Err(error.to_string()),
    };
    let base = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let canonical = fs::canonicalize(&dir).map_err(|error| error.to_string())?;
    if canonical != base.join("profiles") {
        return Err("Invalid Hermes profile path".into());
    }
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        if !entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if valid_profile(&name) && IDENTITY.iter().any(|file| entry.path().join(file).exists()) {
            names.push(name);
        }
    }
    let weight = |byte: u8| match byte {
        b'_' => 1,
        b'-' => 2,
        byte => byte.to_ascii_lowercase(),
    };
    names.sort_by(|left, right| {
        left.bytes()
            .map(weight)
            .cmp(right.bytes().map(weight))
            .then_with(|| {
                left.bytes()
                    .map(|byte| byte.is_ascii_uppercase())
                    .cmp(right.bytes().map(|byte| byte.is_ascii_uppercase()))
            })
    });
    profiles.extend(names.iter().map(|name| Profile::named(root, name)));
    Ok(profiles)
}

fn profile_status(profile: &Profile) -> Result<Value, String> {
    let raw = read(&profile.config())?;
    let yaml = decode(raw.as_deref());
    let mut model = layout::model(&yaml);
    let delegation = layout::delegation(&yaml);
    let auxiliary = layout::auxiliary(&yaml);
    let mut stt = layout::stt(&yaml);
    let has_router_stt = layout::managed_stt(&stt);
    for block in [&mut model, &mut stt] {
        if let Some(key) = block.get_mut("api_key")
            && key.as_str().is_some_and(|value| {
                !value
                    .strip_prefix("${")
                    .and_then(|value| value.strip_suffix('}'))
                    .is_some_and(|id| {
                        !id.is_empty()
                            && id
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    })
            })
        {
            *key = Value::Null;
        }
    }
    Ok(json!({
        "installed":true,"profile":profile.identity(),
        "settings":{"model":model,"delegation":delegation,"auxiliary":auxiliary,"stt":stt},
        "has9Router":layout::local(&model)||layout::local(&delegation)||auxiliary.values().any(layout::local),
        "has9RouterStt":has_router_stt,"configPath":profile.config(),
    }))
}
