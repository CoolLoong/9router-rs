use std::collections::HashSet;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use provider_capabilities_rs::port::{
    combo_capabilities, model_capabilities, service_kind_capabilities,
};
use serde_json::{Map, Value, json};

use crate::combos::{self, Combo};
use crate::key_policy::filter_for_key;
use crate::model_kv;
use crate::observed_response::into_http;
use crate::provider_connections;
use crate::provider_index::{self, Compatible, compatible, js_trim};
use crate::proxy_transport;
use crate::storage::Storage;

const INTERNAL_DISCOVERY: &str = "x-9r-internal-models-fetch";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const DISCOVERY_LIMIT: usize = 4 * 1024 * 1024;
static DATA: OnceLock<Value> = OnceLock::new();
static MEDIA_MODEL_CLIENT: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| {
    crate::provider_http::http_client(DISCOVERY_TIMEOUT)
        .build()
        .ok()
});

pub(crate) fn data() -> &'static Value {
    DATA.get_or_init(|| {
        serde_json::from_str(include_str!("openai-catalog-data.json"))
            .expect("embedded model definitions")
    })
}

#[derive(Clone)]
struct ModelState {
    storage: Storage,
    client: reqwest::Client,
    proxy_http: proxy_transport::Clients,
}

#[derive(Clone, Copy)]
enum DiscoveryMode {
    Live,
    WithoutCompatible,
    Stored,
}

pub fn router(storage: Storage) -> Router {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(DISCOVERY_TIMEOUT)
        .redirect(crate::upstream::redirects())
        .build()
        .expect("model discovery HTTP client");
    let state = ModelState {
        storage,
        client,
        proxy_http: proxy_transport::Clients::default(),
    };
    let mut router = Router::new();
    for base in ["/v1", "/api/v1", "/v1/v1"] {
        router = router
            .route(base, get(list).options(options))
            .route(&format!("{base}/models"), get(list).options(options))
            .route(
                &format!("{base}/models/info"),
                get(crate::model_info::show).options(crate::model_info::options),
            )
            .route(
                &format!("{base}/models/{{*model}}"),
                get(detail).options(options),
            );
    }
    router
        .route("/api/models/access-options", get(access_options))
        .with_state(state)
}

async fn options() -> Response {
    into_http(Ok(json!({
        "status": 200,
        "headers": {
            "access-control-allow-origin": "*",
            "access-control-allow-methods": "GET, OPTIONS",
            "access-control-allow-headers": "*",
        },
        "body": "",
    })))
}

pub(crate) fn respond(status: StatusCode, body: Value, cors: bool) -> Response {
    let mut headers = json!({ "content-type": "application/json" });
    if cors {
        headers["access-control-allow-origin"] = json!("*");
    }
    into_http(Ok(
        json!({ "status": status.as_u16(), "headers": headers, "body": body.to_string() }),
    ))
}

fn catalog_error(message: String, cors: bool) -> Response {
    let message = message.strip_prefix("TypeError: ").unwrap_or(&message);
    respond(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":{"message":message,"type":"server_error"}}),
        cors,
    )
}

async fn list(State(state): State<ModelState>, request: Request) -> Response {
    let skip_dynamic = request
        .headers()
        .get(INTERNAL_DISCOVERY)
        .is_some_and(|value| value == "1");
    match models(
        &state,
        crate::catalog_guard::api_key(&request),
        &["llm"],
        if skip_dynamic {
            DiscoveryMode::WithoutCompatible
        } else {
            DiscoveryMode::Live
        },
    )
    .await
    {
        Ok(models) => respond(
            StatusCode::OK,
            json!({ "object": "list", "data": models }),
            true,
        ),
        Err(message) => catalog_error(message, false),
    }
}

async fn access_options(State(state): State<ModelState>, request: Request) -> Response {
    use crate::settings::reply;
    if !crate::dashboard_session::authenticated(&state.storage, request.headers()).await {
        return reply(
            StatusCode::UNAUTHORIZED,
            json!({"error":"Unauthorized"}),
            true,
        );
    }
    let Ok(available) = models(&state, None, &["llm"], DiscoveryMode::Stored).await else {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"Could not load model access options"}),
            true,
        );
    };
    let models: Vec<_> = available
        .into_iter()
        .filter_map(|entry| {
            let id = entry["id"].as_str()?;
            let provider = entry["owned_by"].as_str()?;
            (provider != "combo" || id.contains('/')).then(|| json!({"id":id,"provider":provider}))
        })
        .collect();
    reply(StatusCode::OK, json!({"models":models}), true)
}

async fn detail(
    State(state): State<ModelState>,
    Path(path): Path<String>,
    request: Request,
) -> Response {
    let path = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    let kinds: &[&str] = match path.as_str() {
        "image" => &["image"],
        "tts" => &["tts"],
        "stt" => &["stt"],
        "embedding" => &["embedding"],
        "image-to-text" => &["imageToText"],
        "web" => &["webSearch", "webFetch"],
        _ => &[],
    };
    if !kinds.is_empty() {
        return match models(
            &state,
            crate::catalog_guard::api_key(&request),
            kinds,
            DiscoveryMode::Live,
        )
        .await
        {
            Ok(models) => respond(
                StatusCode::OK,
                json!({ "object": "list", "data": models }),
                true,
            ),
            Err(message) => catalog_error(message, true),
        };
    }
    match models(
        &state,
        crate::catalog_guard::api_key(&request),
        &["llm"],
        DiscoveryMode::Live,
    )
    .await
    {
        Ok(models) => {
            if let Some(model) = models.into_iter().find(|model| model["id"] == path) {
                return respond(StatusCode::OK, model, true);
            }
            respond(
                StatusCode::NOT_FOUND,
                json!({ "error": {
                    "message": format!("The model '{path}' does not exist or you do not have access to it."),
                    "type": "invalid_request_error", "code": "model_not_found",
                }}),
                true,
            )
        }
        Err(message) => catalog_error(message, true),
    }
}

async fn models(
    state: &ModelState,
    key: Option<String>,
    kinds: &[&str],
    mode: DiscoveryMode,
) -> Result<Vec<Value>, String> {
    let (connections, combos, custom, aliases, disabled, settings) = tokio::join!(
        state
            .storage
            .run(false, |db| provider_connections::read_all(db, None)),
        combos::all(&state.storage),
        model_kv::custom_models(&state.storage),
        model_kv::model_aliases(&state.storage),
        model_kv::disabled_models(&state.storage),
        state.storage.read_settings(),
    );
    let connections: Vec<_> = connections
        .map_err(|_| "Could not load provider connections")?
        .into_iter()
        .filter(|c| c["isActive"] != false)
        .collect();
    let combos = combos.map_err(|_| "Could not load model combos")?;
    let custom = custom.map_err(|_| "Could not load custom models")?;
    let aliases = aliases.map_err(|_| "Could not load model aliases")?;
    let disabled = disabled.map_err(|_| "Could not load disabled models")?;
    let settings = settings.ok_or("Could not load model settings")?;
    let mut active = Vec::new();
    let mut covered = HashSet::new();
    for conn in &connections {
        if let Some(provider) = conn["provider"].as_str()
            && covered.insert(provider)
        {
            active.push(conn.clone());
        }
    }
    for provider in provider_index::entries()
        .iter()
        .filter(|entry| entry["noAuthFree"] == true)
    {
        if let Some(id) = provider["id"].as_str()
            && covered.insert(id)
        {
            active.push(json!({ "provider": provider["id"] }));
        }
    }
    let catalog = state.storage.model_catalog();
    let mut result = combo_entries(&combos, kinds, catalog.as_deref())?;

    let live_discovery = !matches!(mode, DiscoveryMode::Stored);

    if connections.is_empty() {
        static_entries(&mut result, kinds, &custom, &disabled, catalog.as_deref());
    } else {
        let overrides = CatalogOverrides {
            custom: &custom,
            aliases: &aliases,
            disabled: &disabled,
            catalog: catalog.as_deref(),
        };
        for conn in &active {
            let Some(provider) = conn["provider"].as_str() else {
                continue;
            };
            if !provider_matches(provider, kinds) {
                continue;
            }
            let mut discovery = ids_for(conn, provider);
            let live = if live_discovery && enabled_models(conn).is_none() {
                if provider == "github" {
                    crate::copilot_models::for_catalog(&state.storage, &settings, conn).await
                } else if crate::qoder_models::supports(provider) {
                    crate::qoder_models::models(&state.storage, &settings, conn, false).await
                } else if provider == "cursor" {
                    cursor_live(&state.storage, &settings, conn, false).await
                } else if provider == "kimchi" {
                    crate::kimchi_models::models(&state.storage, &settings, conn, false).await
                } else if provider == "commandcode" {
                    crate::commandcode_transport::models(&state.storage, &settings)
                        .await
                        .map(|models| {
                            models
                                .into_iter()
                                .map(|model| json!({"id":model["id"],"name":model["name"]}))
                                .collect()
                        })
                } else if provider == "grok-cli" {
                    use crate::grok_cli_models::Discovery;
                    match crate::grok_cli_models::discover(&state.storage, conn).await {
                        Discovery::Models(models) => Some(models),
                        Discovery::Unavailable(_) => None,
                    }
                } else if provider == "kiro" {
                    use crate::kiro_models::Discovery;
                    match crate::kiro_models::discover(&state.storage, &settings, conn).await {
                        Discovery::Models(models) => Some(models),
                        Discovery::Unavailable => None,
                    }
                } else if provider == "zed" {
                    crate::zed_transport::models(&state.storage, &settings, conn, false)
                        .await
                        .ok()
                        .map(|mut models| {
                            for model in &mut models {
                                if model["supportsTools"] == true {
                                    model["capabilities"] = json!({"tools":true});
                                }
                            }
                            models
                        })
                } else if crate::cline_transport::is_provider(provider) {
                    crate::cline_transport::routed_models(&state.storage, provider, conn).await
                } else if media_models_endpoint(provider).is_some() {
                    media_models(&state.storage, conn).await
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(models) = live.as_ref().filter(|models| !models.is_empty()) {
                let ids = models
                    .iter()
                    .filter_map(|model| model["id"].as_str())
                    .map(str::to_owned);
                if media_models_endpoint(provider).is_some() {
                    discovery.extend(ids);
                } else {
                    discovery = ids.collect();
                }
            }
            if discovery.is_empty()
                && matches!(mode, DiscoveryMode::Live)
                && matches!(
                    compatible(provider),
                    Some(Compatible::OpenAi | Compatible::Anthropic)
                )
            {
                discovery = discover_compatible(state, &settings, provider, conn).await;
            }
            add_active(
                &mut result,
                conn,
                provider,
                kinds,
                discovery,
                live.as_deref().unwrap_or(&[]),
                &overrides,
            );
        }
    }
    let mut seen = HashSet::new();
    result.retain(|model| {
        model["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty() && seen.insert(id.to_owned()))
    });
    if let Some(key) = key.filter(|key| !key.is_empty()) {
        let filtered = state
            .storage
            .run(false, move |db| filter_for_key(db, &key, result))
            .await;
        return filtered.map_err(|_| "Could not filter the model catalog".into());
    }
    Ok(result)
}

pub(crate) async fn cursor_live(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    force: bool,
) -> Option<Vec<Value>> {
    let delivery = crate::provider_http::Delivery::select(
        storage,
        settings,
        account,
        crate::cursor_models::ENDPOINT,
    )
    .await
    .ok()?;
    crate::cursor_models::models_routed(account, force, &delivery).await
}

fn provider_matches(provider: &str, kinds: &[&str]) -> bool {
    if let Some(service_kinds) = provider_index::find(provider)
        .and_then(|entry| entry["serviceKinds"].as_array())
        .filter(|kinds| !kinds.is_empty())
    {
        kinds
            .iter()
            .any(|kind| service_kinds.iter().any(|entry| entry == *kind))
    } else {
        kinds.contains(&"llm")
    }
}

fn enabled_models(conn: &Value) -> Option<&[Value]> {
    conn["providerSpecificData"]["enabledModels"]
        .as_array()
        .filter(|ids| !ids.is_empty())
        .map(Vec::as_slice)
}

fn ids_for(conn: &Value, provider: &str) -> Vec<String> {
    let alias = data()["transportAlias"][provider]
        .as_str()
        .unwrap_or(provider);
    let enabled = enabled_models(conn);
    let mut seen = HashSet::new();
    if let Some(enabled) = enabled {
        return enabled
            .iter()
            .filter_map(Value::as_str)
            .filter(|id| !js_trim(id).is_empty() && seen.insert(*id))
            .map(str::to_owned)
            .collect();
    }
    list_values(&data()["models"][alias])
        .filter_map(|model| model["id"].as_str())
        .map(str::to_owned)
        .collect()
}

fn list_values(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

pub(crate) fn model_kind(model: &Value) -> &str {
    model["kind"]
        .as_str()
        .filter(|kind| !kind.is_empty())
        .or_else(|| model["type"].as_str().filter(|kind| !kind.is_empty()))
        .unwrap_or("llm")
}

fn static_kind(model: &Value) -> &str {
    match model_kind(model) {
        kind @ ("image" | "tts" | "embedding" | "stt" | "imageToText" | "video") => kind,
        _ => "llm",
    }
}

fn inferred_kind(model_id: &str) -> &'static str {
    let name = model_id.to_lowercase();
    if name.contains("embed") {
        return "embedding";
    }
    if ["tts", "speech", "audio", "voice"]
        .iter()
        .any(|s| name.contains(s))
    {
        return "tts";
    }
    if [
        "image",
        "imagen",
        "dall-e",
        "dalle",
        "flux",
        "sdxl",
        "sd-",
        "stable-diffusion",
    ]
    .iter()
    .any(|s| name.contains(s))
    {
        return "image";
    }
    "llm"
}

pub(crate) fn media_models_endpoint(provider: &str) -> Option<(&'static str, &'static str)> {
    match provider {
        "venice" => Some((
            "https://api.venice.ai/api/v1/models",
            "http://127.0.0.1:52989/provider-catalog/venice",
        )),
        "tokenrouter" => Some((
            "https://api.tokenrouter.com/v1/models",
            "http://127.0.0.1:52989/provider-catalog/tokenrouter",
        )),
        _ => None,
    }
}

pub(crate) async fn media_models(storage: &Storage, account: &Value) -> Option<Vec<Value>> {
    let provider = account["provider"].as_str()?;
    let (endpoint, fixture) = media_models_endpoint(provider)?;
    if !matches!(account["authType"].as_str(), Some("apikey" | "api_key")) {
        return None;
    }
    let key = account["apiKey"].as_str().filter(|key| !key.is_empty())?;
    let bound = crate::provider_http::bind(
        storage,
        account,
        endpoint,
        fixture,
        MEDIA_MODEL_CLIENT.as_ref()?,
        DISCOVERY_TIMEOUT,
        None,
    )
    .await?;
    let mut response = bound
        .request(reqwest::Method::GET)
        .bearer_auth(key)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > DISCOVERY_LIMIT - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = serde_json::from_slice::<Value>(&bytes).ok()?;
    let alias = data()["transportAlias"][provider]
        .as_str()
        .unwrap_or(provider);
    let pinned: std::collections::HashMap<_, _> = list_values(&data()["models"][alias])
        .filter_map(|model| Some((model["id"].as_str()?, static_kind(model))))
        .collect();
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for row in body["data"].as_array().or_else(|| body.as_array())? {
        let Some(id) = row["id"]
            .as_str()
            .map(js_trim)
            .filter(|id| !id.is_empty() && *id != key && (key.len() < 6 || !id.contains(key)))
        else {
            continue;
        };
        let id = strip_provider_prefix(id, &[provider, alias]).unwrap_or(id);
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        let kind = match model_kind(row) {
            kind @ ("embedding" | "image" | "tts" | "stt" | "video" | "imageToText") => kind,
            "text" | "chat" => "llm",
            _ => pinned.get(id).copied().unwrap_or_else(|| inferred_kind(id)),
        };
        let name = row["name"]
            .as_str()
            .filter(|name| {
                !name.is_empty() && *name != key && (key.len() < 6 || !name.contains(key))
            })
            .unwrap_or(id);
        models.push(json!({"id":id,"name":name,"kind":kind}));
    }
    Some(models)
}

fn is_disabled(disabled: &Map<String, Value>, aliases: &[&str], model: &str) -> bool {
    aliases.iter().any(|alias| {
        disabled
            .get(*alias)
            .is_some_and(|ids| list_values(ids).any(|id| id == model))
    })
}

fn add_limits(model: &mut Value, context: Option<u64>, max: Option<u64>) {
    if let Some(context) = context {
        model["context_length"] = json!(context);
    }
    if let Some(max) = max {
        model["max_completion_tokens"] = json!(max);
    }
}

fn static_entries(
    result: &mut Vec<Value>,
    kinds: &[&str],
    custom: &[Value],
    disabled: &Map<String, Value>,
    catalog: Option<&Value>,
) {
    if let Some(models) = data()["models"].as_object() {
        for (alias, buckets) in models {
            let provider = data()["providerByAlias"][alias].as_str().unwrap_or(alias);
            if !provider_matches(provider, kinds) {
                continue;
            }
            for model in list_values(buckets) {
                let Some(id) = model["id"].as_str() else {
                    continue;
                };
                if !kinds.contains(&static_kind(model))
                    || is_disabled(disabled, &[alias, provider], id)
                {
                    continue;
                }
                result.push(json!({
                    "id": format!("{alias}/{id}"), "object": "model", "owned_by": alias,
                    "capabilities": model_capabilities(Some(alias), id, catalog),
                }));
            }
        }
    }
    if kinds.contains(&"llm") {
        for model in custom {
            if model["type"]
                .as_str()
                .is_some_and(|kind| !kind.is_empty() && kind != "llm")
            {
                continue;
            }
            let Some(alias) = model["providerAlias"]
                .as_str()
                .filter(|alias| !alias.is_empty())
            else {
                continue;
            };
            let Some(id) = model["id"]
                .as_str()
                .map(js_trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            result.push(
                json!({ "id": format!("{alias}/{id}"), "object": "model", "owned_by": alias }),
            );
        }
    }
}

struct CatalogOverrides<'a> {
    custom: &'a [Value],
    aliases: &'a Map<String, Value>,
    disabled: &'a Map<String, Value>,
    catalog: Option<&'a Value>,
}

pub(crate) fn catalog_owner<'a>(conn: &'a Value, provider: &'a str) -> &'a str {
    let alias = provider_index::find(provider)
        .and_then(|entry| entry["alias"].as_str())
        .unwrap_or(provider);
    js_trim(
        conn["providerSpecificData"]["prefix"]
            .as_str()
            .filter(|prefix| !prefix.is_empty())
            .unwrap_or(alias),
    )
}

fn add_active(
    result: &mut Vec<Value>,
    conn: &Value,
    provider: &str,
    kinds: &[&str],
    discovered: Vec<String>,
    live: &[Value],
    overrides: &CatalogOverrides<'_>,
) {
    let &CatalogOverrides {
        custom,
        aliases,
        disabled,
        catalog,
    } = overrides;
    let transport = data()["transportAlias"][provider]
        .as_str()
        .unwrap_or(provider);
    let output = catalog_owner(conn, provider);
    let prefixes = [output, transport, provider];
    let mut ids: Vec<String> = discovered
        .into_iter()
        .map(|id| {
            strip_provider_prefix(&id, &prefixes)
                .unwrap_or(&id)
                .to_owned()
        })
        .filter(|id| !js_trim(id).is_empty())
        .collect();
    let live: std::collections::HashMap<_, _> = live
        .iter()
        .filter_map(|model| Some((model["id"].as_str()?, model)))
        .collect();
    let mut custom_kinds = std::collections::HashMap::new();
    for model in custom {
        let kind = model_kind(model);
        if !kind_allowed(kinds, kind)
            || !prefixes.contains(&model["providerAlias"].as_str().unwrap_or_default())
        {
            continue;
        }
        if let Some(id) = model["id"]
            .as_str()
            .map(js_trim)
            .filter(|id| !id.is_empty())
        {
            ids.push(id.to_owned());
            custom_kinds.insert(id, kind);
        }
    }
    ids.extend(
        aliases
            .values()
            .filter_map(Value::as_str)
            .filter_map(|id| strip_provider_prefix(id, &prefixes))
            .filter(|id| !js_trim(id).is_empty())
            .map(str::to_owned),
    );
    let mut visited = HashSet::new();
    for id in ids {
        if !visited.insert(id.clone()) || is_disabled(disabled, &prefixes, &id) {
            continue;
        }
        let custom_kind = custom_kinds.get(id.as_str()).copied();
        let live_model = live.get(id.as_str()).copied();
        let known = list_values(&data()["models"][transport])
            .filter(|entry| entry["id"] == id)
            .last();
        let kind = custom_kind
            .or_else(|| live_model.map(static_kind))
            .or_else(|| known.map(static_kind))
            .unwrap_or_else(|| inferred_kind(&id));
        if !kind_allowed(kinds, kind) {
            continue;
        }
        let caps = model_capabilities(Some(provider), &id, catalog);
        let mut model =
            json!({ "id": format!("{output}/{id}"), "object": "model", "owned_by": output });
        let live_caps = live_model
            .and_then(|model| model.get("capabilities"))
            .filter(|caps| !caps.is_null());
        let service_caps = custom_kind.and_then(service_kind_capabilities);
        if let Some(value) = live_caps {
            model["capabilities"] = value.clone();
        } else if let Some(value) = service_caps {
            model["capabilities"] = value;
        } else if kind == "llm" {
            model["capabilities"] = caps.clone();
        }
        if matches!(kind, "llm" | "imageToText") && kinds.contains(&"llm") {
            add_limits(
                &mut model,
                live_caps
                    .and_then(|caps| caps["contextWindow"].as_u64())
                    .or_else(|| caps["contextWindow"].as_u64()),
                live_caps
                    .and_then(|caps| caps["maxOutput"].as_u64())
                    .or_else(|| caps["maxOutput"].as_u64()),
            );
            if let Some(levels) = super::thinking_catalog::levels(provider, &id, &caps, data()) {
                model["thinking_levels"] = json!(levels);
            }
        }
        result.push(model);
    }
    for (kind, flag, suffix) in [
        ("webSearch", "providersWithSearch", "search"),
        ("webFetch", "providersWithFetch", "fetch"),
    ] {
        if kinds.contains(&kind) && list_values(&data()[flag]).any(|id| id == provider) {
            result.push(json!({ "id": format!("{output}/{suffix}"), "object": "model", "kind": kind, "owned_by": output }));
        }
    }
}

fn kind_allowed(kinds: &[&str], kind: &str) -> bool {
    kinds.contains(&kind) || (kind == "imageToText" && kinds.contains(&"llm"))
}

fn strip_provider_prefix<'a>(model: &'a str, known: &[&str]) -> Option<&'a str> {
    known.iter().find_map(|prefix| {
        model
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('/'))
    })
}

async fn discover_compatible(
    state: &ModelState,
    settings: &Value,
    provider: &str,
    conn: &Value,
) -> Vec<String> {
    let Some(key) = ["apiKey", "accessToken"]
        .into_iter()
        .find_map(|field| conn[field].as_str().filter(|key| !key.is_empty()))
    else {
        return vec![];
    };
    let Some(base) = crate::upstream::connection_base(conn, None) else {
        return vec![];
    };
    let kind = compatible(provider);
    let paths = if kind == Some(Compatible::Anthropic) {
        crate::upstream::ANTHROPIC_NODE_PATHS
    } else {
        crate::upstream::OPENAI_NODE_PATHS
    };
    let Some(url) = crate::upstream::node_url(base, "/models", paths) else {
        return vec![];
    };
    let mut request = state
        .client
        .get(url)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .header(INTERNAL_DISCOVERY, "1");
    if kind == Some(Compatible::Anthropic) {
        request = request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01");
    }
    let Some(mut response) = crate::provider_http::fetch(
        &state.storage,
        settings,
        conn,
        &state.proxy_http,
        request,
        DISCOVERY_TIMEOUT,
    )
    .await
    .ok()
    .filter(|r| r.status().is_success()) else {
        return vec![];
    };
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if chunk.len() <= DISCOVERY_LIMIT - bytes.len() => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => return vec![],
        }
    }
    let Ok(json) = serde_json::from_slice::<Value>(&bytes) else {
        return vec![];
    };
    let rows = if json.is_array() {
        &json
    } else {
        ["data", "models", "results"]
            .iter()
            .find_map(|field| json.get(*field).filter(|v| provider_connections::truthy(v)))
            .unwrap_or(&Value::Null)
    };
    let mut seen = HashSet::new();
    list_values(rows)
        .filter_map(|row| {
            ["id", "name", "model"]
                .iter()
                .find_map(|key| row.get(*key).filter(|v| provider_connections::truthy(v)))
                .and_then(Value::as_str)
        })
        .filter(|id| {
            !js_trim(id).is_empty()
                && *id != key
                && (key.len() < 6 || !id.contains(key))
                && seen.insert(*id)
        })
        .map(str::to_owned)
        .collect()
}

fn combo_entries(
    combos: &[Combo],
    kinds: &[&str],
    catalog: Option<&Value>,
) -> Result<Vec<Value>, String> {
    let lookup: Map<String, Value> = combos
        .iter()
        .map(|combo| (combo.name.clone(), combo.models.clone()))
        .collect();
    let mut result = Vec::new();
    for combo in combos {
        let kind = combo
            .kind
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("llm");
        if !kinds.contains(&kind) {
            continue;
        }
        let mut model = json!({ "id": combo.name, "object": "model", "owned_by": "combo" });
        if matches!(kind, "webSearch" | "webFetch") {
            model["kind"] = json!(kind);
        } else {
            let seats = combo
                .models
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            if let Some(caps) = combo_capabilities(seats, Some(&lookup), catalog, |seat| {
                seat_caps(seat, catalog)
            })? {
                model["capabilities"] = caps;
            }
            let (context, max) = combo_limits(&combo.name, &lookup, catalog, &mut HashSet::new());
            add_limits(&mut model, context, max);
        }
        result.push(model);
    }
    Ok(result)
}

fn seat_caps(seat: &str, catalog: Option<&Value>) -> Option<Value> {
    let (alias, model) = seat.split_once('/')?;
    if alias.is_empty() {
        return None;
    }
    let provider = data()["providerByAlias"][alias].as_str().unwrap_or(alias);
    Some(model_capabilities(Some(provider), model, catalog))
}

fn combo_limits(
    name: &str,
    combos: &Map<String, Value>,
    catalog: Option<&Value>,
    visiting: &mut HashSet<String>,
) -> (Option<u64>, Option<u64>) {
    if !visiting.insert(name.to_owned()) {
        return (None, None);
    }
    let mut context = None;
    let mut max = None;
    if let Some(seats) = combos.get(name) {
        for seat in list_values(seats).filter_map(Value::as_str) {
            let slash = seat.find('/').unwrap_or(0);
            let (ctx, output) = if slash == 0 && combos.contains_key(seat) {
                combo_limits(seat, combos, catalog, visiting)
            } else {
                let caps = seat_caps(seat, catalog)
                    .unwrap_or_else(|| model_capabilities(None, seat, catalog));
                (caps["contextWindow"].as_u64(), caps["maxOutput"].as_u64())
            };
            if let Some(ctx) = ctx {
                context = Some(context.map_or(ctx, |current: u64| current.min(ctx)));
            }
            if let Some(output) = output {
                max = Some(max.map_or(output, |current: u64| current.min(output)));
            }
        }
    }
    visiting.remove(name);
    (context, max)
}
