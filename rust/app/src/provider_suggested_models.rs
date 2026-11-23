use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use reqwest::{
    Client,
    dns::{Addrs, Name, Resolve, Resolving},
    redirect::Policy,
};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;
use url::Url;

use super::{error, respond};
use crate::chat_output::js_text;
use crate::chat_prefetch::public_address;
use crate::provider_connections::truthy;
use crate::provider_index::{self, js_trim};

const MAX_BYTES: usize = 32 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(12);
const KILO_URL: &str = "https://api.kilo.ai/api/gateway/models";
const KILO_TTL: Duration = Duration::from_secs(3600);
type CachedModels = Option<(Instant, Vec<Value>)>;
static KILO_CATALOG: LazyLock<Mutex<CachedModels>> = LazyLock::new(|| Mutex::new(None));

#[derive(Clone, Copy)]
enum Catalog {
    OpenRouter,
    OpenCodeFree,
    OpenCodeGo,
    Mimo,
    Minimax,
    Airforce,
    Nous,
}

impl Catalog {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "openrouter-free" => Self::OpenRouter,
            "opencode-free" => Self::OpenCodeFree,
            "opencode-go" => Self::OpenCodeGo,
            "mimo-free" => Self::Mimo,
            "minimax-code" => Self::Minimax,
            "airforce-free" => Self::Airforce,
            "nous-free" => Self::Nous,
            _ => return None,
        })
    }

    fn approved(input: &str, kind: &str) -> Option<Url> {
        let url = Url::parse(input).ok()?;
        (url.scheme() == "https"
            && url.port().is_none()
            && provider_index::entries().iter().any(|entry| {
                let source = &entry["modelsFetcher"];
                source["type"] == kind && source["url"] == url.as_str()
            }))
        .then_some(url)
    }
}

pub(super) async fn list(request: Request) -> Response {
    let mut input = None;
    let mut filter = None;
    for (name, value) in
        url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
    {
        match name.as_ref() {
            "url" => {
                input.get_or_insert(value);
            }
            "type" => {
                filter.get_or_insert(value);
            }
            _ => {}
        }
    }
    let Some((input, filter)) = input
        .filter(|value| !value.is_empty())
        .zip(filter.filter(|value| !value.is_empty()))
    else {
        return error(StatusCode::BAD_REQUEST, "Missing url or type");
    };
    let Some(catalog) = Catalog::parse(&filter) else {
        return error(StatusCode::BAD_REQUEST, "Unknown filter type");
    };
    let data = match Catalog::approved(&input, &filter) {
        Some(url) => fetch(url, TIMEOUT)
            .await
            .ok()
            .and_then(|body| select(catalog, &body))
            .unwrap_or_default(),
        None => Vec::new(),
    };
    respond(StatusCode::OK, json!({"data":data}))
}

pub(super) async fn kilo() -> Response {
    let mut cache = KILO_CATALOG.lock().await;
    if let Some((timestamp, models)) = cache.as_ref()
        && timestamp.elapsed() < KILO_TTL
    {
        return respond(StatusCode::OK, json!({"models":models,"cached":true}));
    }
    let url = Url::parse(KILO_URL).expect("pinned Kilo model URL");
    match fetch(url, Duration::from_secs(10)).await {
        Ok(body) => {
            let models = body["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|model| model["isFree"] == true)
                .map(|model| {
                    let mut entry = Map::new();
                    for field in ["id", "name"] {
                        if let Some(value) = model.get(field) {
                            entry.insert(field.into(), value.clone());
                        }
                    }
                    entry.insert("isFree".into(), json!(true));
                    entry.insert(
                        "context_length".into(),
                        model
                            .get("context_length")
                            .filter(|value| truthy(value))
                            .cloned()
                            .unwrap_or_else(|| json!(0)),
                    );
                    Value::Object(entry)
                })
                .collect::<Vec<_>>();
            let response = respond(StatusCode::OK, json!({"models":models,"cached":false}));
            *cache = Some((Instant::now(), models));
            response
        }
        Err(reason) => {
            if let Some((_, models)) = cache.as_ref() {
                respond(
                    StatusCode::OK,
                    json!({"models":models,"cached":true,"warning":reason}),
                )
            } else {
                respond(
                    StatusCode::BAD_GATEWAY,
                    json!({"models":[],"error":format!("Failed to fetch Kilo models: {reason}")}),
                )
            }
        }
    }
}

struct PublicDns;

impl Resolve for PublicDns {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let mut approved = Vec::new();
            for address in tokio::net::lookup_host((name.as_str(), 443)).await? {
                let ip = public_address(address.ip())
                    .ok_or_else(|| io::Error::other("catalog resolved to a non-public address"))?;
                approved.push(SocketAddr::new(ip, 443));
            }
            if approved.is_empty() {
                return Err(io::Error::other("catalog hostname has no public address").into());
            }
            Ok(Box::new(approved.into_iter()) as Addrs)
        })
    }
}

static HTTP: LazyLock<Option<Client>> = LazyLock::new(|| {
    Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .dns_resolver(Arc::new(PublicDns))
        .timeout(TIMEOUT)
        .build()
        .ok()
});

async fn fetch(approved: Url, timeout: Duration) -> Result<Value, String> {
    let failure = || "fetch failed".to_owned();
    let url = if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
        let mut local = Url::parse(&format!(
            "http://127.0.0.1:52989/catalog-suggestions/{}{}",
            approved.host_str().ok_or_else(failure)?,
            approved.path()
        ))
        .map_err(|_| failure())?;
        local.set_query(approved.query());
        local
    } else {
        approved
    };
    let mut response = HTTP
        .as_ref()
        .ok_or_else(failure)?
        .get(url)
        .header("Accept", "application/json")
        .timeout(timeout)
        .send()
        .await
        .map_err(|_| failure())?;
    if !response.status().is_success() {
        return Err(format!("Kilo API returned {}", response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BYTES as u64)
    {
        return Err("Model catalog exceeds the size limit".into());
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| failure())? {
        if chunk.len() > MAX_BYTES - data.len() {
            return Err("Model catalog exceeds the size limit".into());
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).map_err(|_| "Invalid model catalog".into())
}

fn select(catalog: Catalog, body: &Value) -> Option<Vec<Value>> {
    let raw = ["data", "models"]
        .into_iter()
        .find_map(|name| body.get(name).filter(|value| !value.is_null()))
        .unwrap_or(body);
    let rows = match raw {
        Value::Array(rows) => rows.as_slice(),
        Value::Object(_) => std::slice::from_ref(raw),
        _ => &[],
    };
    let mut selected = Vec::new();
    for row in rows {
        let model = match catalog {
            Catalog::OpenRouter => {
                if row.is_null() {
                    return None;
                }
                (row["pricing"]["prompt"] == "0"
                    && row["pricing"]["completion"] == "0"
                    && number(&row["context_length"]) >= 200_000.0)
                    .then(|| project(row, false, true))
            }
            Catalog::OpenCodeFree => {
                if row.is_null() {
                    return None;
                }
                optional_string(&row["id"])?
                    .filter(|id| {
                        (id.ends_with("-free") || *id == "big-pickle")
                            && *id != "deepseek-v4-flash-free"
                    })
                    .map(|id| json!({"id":id,"name":id}))
            }
            Catalog::OpenCodeGo => row["id"].as_str().map(|id| json!({"id":id,"name":id})),
            Catalog::Mimo => {
                if row.is_null() {
                    return None;
                }
                let matched = optional_string(&row["id"])?.is_some_and(|id| id.starts_with("mimo"))
                    || optional_string(&row["name"])?
                        .is_some_and(|name| name.to_lowercase().contains("mimo"));
                matched.then(|| project(row, true, false))
            }
            Catalog::Airforce => {
                if row.is_null() {
                    return None;
                }
                let free = row["tier"] == "free"
                    || optional_string(&row["id"])?.is_some_and(|id| id.ends_with(":free"));
                let media = &row["media_type"];
                (free
                    && row["supports_chat"] == true
                    && (!truthy(media) || media == "chat" || media == "text"))
                    .then(|| project(row, true, true))
            }
            Catalog::Nous => nous(row),
            Catalog::Minimax => return Some(minimax(rows)),
        };
        selected.extend(model);
    }
    match catalog {
        Catalog::OpenRouter => sort_context(&mut selected),
        Catalog::Airforce => selected.sort_by_cached_key(|row| {
            let id = row
                .get("id")
                .map(js_text)
                .unwrap_or_else(|| "undefined".to_owned());
            id_order(&id)
        }),
        Catalog::Nous => {
            selected.sort_by_key(|row| {
                !row["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("nousresearch/"))
            });
            sort_context(&mut selected);
        }
        _ => (),
    }
    Some(selected)
}

fn optional_string(value: &Value) -> Option<Option<&str>> {
    if value.is_null() {
        Some(None)
    } else {
        value.as_str().map(Some)
    }
}

fn project(row: &Value, name_fallback: bool, context: bool) -> Value {
    let mut output = Map::new();
    if let Some(id) = row.get("id") {
        output.insert("id".into(), id.clone());
    }
    let name = if name_fallback && !truthy(&row["name"]) {
        row.get("id")
    } else {
        row.get("name")
    };
    if let Some(name) = name {
        output.insert("name".into(), name.clone());
    }
    if context && let Some(value) = row.get("context_length") {
        output.insert("contextLength".into(), value.clone());
    }
    Value::Object(output)
}

fn minimax(rows: &[Value]) -> Vec<Value> {
    let Some(models) = rows.first().and_then(|env| {
        env["providers"]
            .as_array()?
            .iter()
            .find(|row| row["providerId"] == "minimax")
            .map(|row| &row["config"]["models"])
    }) else {
        return Vec::new();
    };
    let entries: Vec<_> = match models {
        Value::Object(models) => {
            let mut entries: Vec<_> = models.iter().collect();
            entries.sort_by_key(|(id, _)| {
                id.parse::<u32>()
                    .ok()
                    .filter(|index| index.to_string() == **id)
                    .unwrap_or(u32::MAX)
            });
            entries
                .into_iter()
                .map(|(id, row)| (id.clone(), row))
                .collect()
        }
        Value::Array(rows) => rows
            .iter()
            .enumerate()
            .map(|(id, row)| (id.to_string(), row))
            .collect(),
        _ => return Vec::new(),
    };
    let mut selected = Vec::new();
    for (id, row) in entries {
        if js_trim(&id).is_empty() || !matches!(row, Value::Object(_) | Value::Array(_)) {
            continue;
        }
        let name = row
            .get("name")
            .filter(|value| truthy(value))
            .cloned()
            .unwrap_or_else(|| json!(id));
        let mut model = json!({"id":id,"name":name});
        let context = number(&row["limit"]["context"]);
        if context != 0.0 && !context.is_nan() {
            model["contextLength"] = json_number(context);
        }
        selected.push(model);
    }
    sort_context(&mut selected);
    selected
}

fn nous(row: &Value) -> Option<Value> {
    if ["prompt", "completion"].iter().any(|side| {
        let price = &row["pricing"][*side];
        price.is_null() || number(price) != 0.0
    }) {
        return None;
    }
    normalize_nous_model(row).filter(|model| number(&model["contextLength"]) >= 200_000.0)
}

pub(crate) fn normalize_nous_models(body: &Value) -> Vec<Value> {
    let rows = if body.is_array() {
        body.as_array()
    } else {
        ["data", "models", "results"]
            .iter()
            .map(|field| &body[*field])
            .find(|value| truthy(value))
            .and_then(Value::as_array)
    };
    let mut models: Vec<_> = rows
        .into_iter()
        .flatten()
        .filter_map(normalize_nous_model)
        .collect();
    models.sort_by_key(|model| {
        !model["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("nousresearch/"))
    });
    models
}

fn normalize_nous_model(row: &Value) -> Option<Value> {
    let output = row["architecture"]["output_modalities"].as_array();
    if output.is_some_and(|types| !types.is_empty() && !types.iter().any(|kind| kind == "text")) {
        return None;
    }
    let context = positive(
        row.get("context_length")
            .filter(|value| !value.is_null())
            .unwrap_or(&row["top_provider"]["context_length"]),
    );
    let id = js_trim(row["id"].as_str()?);
    if id.is_empty() {
        return None;
    }
    let name = row
        .get("name")
        .filter(|value| truthy(value))
        .cloned()
        .unwrap_or_else(|| json!(id));
    let mut model = json!({"id":id,"name":name});
    if let Some(context) = context {
        model["contextLength"] = json_number(context);
    }
    if let Some(value) = positive(&row["top_provider"]["max_completion_tokens"]) {
        model["maxOutputTokens"] = json_number(value);
    }
    if let Some(description) = row.get("description").filter(|value| truthy(value)) {
        model["description"] = description.clone();
    }
    Some(model)
}

fn positive(value: &Value) -> Option<f64> {
    let parsed = number(value);
    (parsed.is_finite() && parsed.fract() == 0.0 && parsed > 0.0).then_some(parsed)
}

fn number(value: &Value) -> f64 {
    match value {
        Value::Array(_) | Value::String(_) => {
            let string = js_text(value);
            let input = js_trim(&string);
            if input.is_empty() {
                return 0.0;
            }
            for (prefixes, radix) in [(["0x", "0X"], 16), (["0o", "0O"], 8), (["0b", "0B"], 2)] {
                if let Some(tail) = prefixes
                    .into_iter()
                    .find_map(|prefix| input.strip_prefix(prefix))
                {
                    return u64::from_str_radix(tail, radix).map_or(f64::NAN, |value| value as f64);
                }
            }
            match input {
                "Infinity" | "+Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                _ => input
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .unwrap_or(f64::NAN),
            }
        }
        _ => crate::thinking_intent::numeric(value).unwrap_or(f64::NAN),
    }
}

fn json_number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn sort_context(rows: &mut [Value]) {
    rows.sort_by(|a, b| {
        let left = number(&a["contextLength"]);
        let right = number(&b["contextLength"]);
        right
            .partial_cmp(&left)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn id_order(id: &str) -> (Vec<u32>, Vec<bool>) {
    const PUNCTUATION: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    let primary = id
        .chars()
        .flat_map(char::to_lowercase)
        .map(|glyph| {
            PUNCTUATION
                .find(glyph)
                .map_or(glyph as u32 + 50, |position| position as u32)
        })
        .collect();
    (primary, id.chars().map(char::is_uppercase).collect())
}
