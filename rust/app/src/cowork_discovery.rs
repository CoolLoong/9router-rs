use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use reqwest::{Client, Url};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::CliTools;
use crate::catalog_guard::is_direct_loopback;
use crate::chat_prefetch::public_address;
use crate::provider_connections::truthy;
use crate::settings::reply;

const REGISTRY_URL: &str = "https://api.anthropic.com/mcp-registry/v0/servers";
const PROTOCOL: &str = "2025-06-18";
const VISIBILITY: &str = "commercial,gsuite,gsuite-google";
const TTL: Duration = Duration::from_secs(3600);
const TIMEOUT: Duration = Duration::from_secs(8);
const BODY_LIMIT: usize = 4 * 1024 * 1024;
struct RegistryCache {
    timestamp: Instant,
    servers: Vec<Value>,
}

static REGISTRY: LazyLock<Mutex<Option<RegistryCache>>> = LazyLock::new(|| Mutex::new(None));

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router
        .route("/api/cli-tools/cowork-mcp-registry", get(registry))
        .route("/api/cli-tools/cowork-mcp-tools", post(tools))
}

async fn registry(request: Request) -> Response {
    let force = request.uri().query().is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(key, value)| (key == "refresh").then_some(value == "1"))
            .unwrap_or(false)
    });
    let mut slot = REGISTRY.lock().await;
    if let Some(RegistryCache { timestamp, servers }) = slot.as_ref()
        && !force
        && timestamp.elapsed() < TTL
    {
        return reply(
            StatusCode::OK,
            json!({"cached":true,"servers":servers,"total":servers.len()}),
            true,
        );
    }
    match fetch_catalog().await {
        Ok(servers) => {
            let body = json!({"cached":false,"servers":servers,"total":servers.len()});
            *slot = Some(RegistryCache {
                timestamp: Instant::now(),
                servers,
            });
            reply(StatusCode::OK, body, true)
        }
        Err(reason) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":reason,"servers":[],"total":0}),
            true,
        ),
    }
}

async fn fetch_catalog() -> Result<Vec<Value>, &'static str> {
    const FAILURE: &str = "Failed to fetch MCP registry";
    let base = if fixture() {
        "http://127.0.0.1:52989/cowork-registry"
    } else {
        REGISTRY_URL
    };
    static CLIENT: OnceLock<Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| client().build().expect("Cowork registry client"));
    let mut cursor = String::new();
    let mut servers = Vec::new();
    let mut urls = HashSet::new();
    let mut cursors = HashSet::new();
    for _ in 0..20 {
        let mut request = client
            .get(base)
            .header("accept", "application/json")
            .query(&[("limit", "500"), ("visibility", VISIBILITY)]);
        if !cursor.is_empty() {
            request = request.query(&[("cursor", &cursor)]);
        }
        let response = request.send().await.map_err(|_| FAILURE)?;
        if !response.status().is_success() {
            break;
        }
        let bytes = read(response).await.map_err(|_| FAILURE)?;
        let document: Value = serde_json::from_slice(&bytes).map_err(|_| FAILURE)?;
        for entry in document["servers"].as_array().into_iter().flatten() {
            if let Some(server) = catalog_entry(entry)
                && let Some(url) = server["url"].as_str()
                && urls.insert(url.to_owned())
            {
                servers.push(server);
            }
        }
        let Some(next) = document["metadata"]["nextCursor"]
            .as_str()
            .filter(|next| !next.is_empty())
        else {
            break;
        };
        if !cursors.insert(next.to_owned()) {
            break;
        }
        cursor = next.to_owned();
    }
    Ok(servers)
}

fn catalog_entry(entry: &Value) -> Option<Value> {
    let server = &entry["server"];
    let meta = &entry["_meta"]["com.anthropic.api/mcp-registry"];
    let remote = server["remotes"].as_array()?.first()?;
    let address = remote["url"].as_str()?;
    let url = Url::parse(address).ok()?;
    let host = url.host_str()?;
    if url.scheme() != "https"
        || !clean_url(&url)
        || host == "mcp.claude.com"
        || host.ends_with(".mcp.claude.com")
        || host == "api.anthropic.com" && url.path().starts_with("/mcp")
        || address.contains(['<', '{'])
        || meta["requiredFields"]
            .as_array()
            .is_some_and(|fields| !fields.is_empty())
    {
        return None;
    }
    let name = server["name"].as_str().filter(|name| !name.is_empty())?;
    let pick = |value: &Value, default: &str| {
        value
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or(default)
            .to_owned()
    };
    let slug = pick(&meta["slug"], name);
    let transport = if remote["type"] == "sse" {
        "sse"
    } else {
        "http"
    };
    let title = pick(&server["title"], &pick(&meta["displayName"], name));
    let description = pick(&server["description"], &pick(&meta["oneLiner"], ""));
    let names = meta["toolNames"].as_array().map_or(&[][..], Vec::as_slice);
    Some(json!({
        "name":name,"slug":slug,"title":title,"description":description,"url":address,
        "transport":transport,"oauth":!truthy(&meta["isAuthless"]),"toolNames":names,
        "toolCount":names.len(),"iconUrl":meta.get("iconUrl").filter(|value| truthy(value)),
    }))
}

async fn tools(request: Request) -> Response {
    let local = is_direct_loopback(&request);
    let input = to_bytes(request.into_body(), 32 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let Some(input) = input else {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"Invalid JSON body"}),
            true,
        );
    };
    let Some(url) = input["url"].as_str().filter(|url| !url.is_empty()) else {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"url required"}),
            true,
        );
    };
    let Some(url) = Url::parse(url).ok().filter(clean_url) else {
        return rejected();
    };
    let result = tokio::time::timeout(TIMEOUT, async {
        let client = tool_client(&url, local).await?;
        Ok(mcp_tools(&client, &url).await)
    })
    .await;
    match result {
        Ok(Ok(value)) => reply(StatusCode::OK, value, true),
        Ok(Err(())) => rejected(),
        Err(_) => reply(StatusCode::OK, json!({"error":"timeout","tools":[]}), true),
    }
}

fn rejected() -> Response {
    reply(
        StatusCode::BAD_REQUEST,
        json!({"error":"URL not allowed"}),
        true,
    )
}

fn fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

fn clean_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

fn client() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
}

async fn tool_client(url: &Url, local: bool) -> Result<Client, ()> {
    let port = url.port_or_known_default().ok_or(())?;
    let allowed = |address: IpAddr| {
        let address = address.to_canonical();
        public_address(address).is_some()
            || local
                && match address {
                    IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
                    IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
                }
    };
    let builder = client();
    let sockets: Vec<_> = match url.host().ok_or(())? {
        url::Host::Ipv4(ip) => vec![SocketAddr::new(ip.into(), port)],
        url::Host::Ipv6(ip) => vec![SocketAddr::new(ip.into(), port)],
        url::Host::Domain(host) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| ())?
            .collect(),
    };
    if sockets.is_empty() || !sockets.iter().all(|socket| allowed(socket.ip())) {
        return Err(());
    }
    let builder = if let Some(domain) = url.domain() {
        builder.resolve_to_addrs(domain, &sockets)
    } else {
        builder
    };
    builder.build().map_err(|_| ())
}

async fn mcp_tools(client: &Client, url: &Url) -> Value {
    const FAILED: &str = "MCP request failed";
    let init = send(client, url, &HeaderMap::new(), json!({
        "jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"9router","version":"1"}},
    })).await;
    let Ok(response) = init else {
        return failed(FAILED);
    };
    if unauthorized(response.status()) {
        return json!({"requiresAuth":true,"tools":[]});
    }
    if !response.status().is_success() {
        return failed(format!("init {}", response.status().as_u16()));
    }
    let mut session = HeaderMap::new();
    if let Some(id) = response
        .headers()
        .get("mcp-session-id")
        .filter(|id| id.as_bytes().len() <= 512)
    {
        let mut id: HeaderValue = id.clone();
        id.set_sensitive(true);
        session.insert("mcp-session-id", id);
    }
    if message(response, 1).await.is_err() {
        return failed(FAILED);
    }
    let _ = send(
        client,
        url,
        &session,
        json!({
            "jsonrpc":"2.0","method":"notifications/initialized","params":{},
        }),
    )
    .await;
    let listing = send(
        client,
        url,
        &session,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/list",
        }),
    )
    .await;
    let Ok(response) = listing else {
        return failed(FAILED);
    };
    if unauthorized(response.status()) {
        return json!({"requiresAuth":true,"tools":[]});
    }
    let Ok(result) = message(response, 2).await else {
        return failed(FAILED);
    };
    let tools = result["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let name = item["name"].as_str().filter(|name| !name.is_empty())?;
            let description = item["description"].as_str().unwrap_or_default();
            Some(json!({"name":name,"description":description}))
        })
        .collect::<Vec<_>>();
    json!({"tools":tools})
}

fn failed(reason: impl Into<String>) -> Value {
    json!({"error":reason.into(),"tools":[]})
}

fn unauthorized(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

async fn send(
    client: &Client,
    url: &Url,
    session: &HeaderMap,
    body: Value,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .post(url.clone())
        .headers(session.clone())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL)
        .json(&body)
        .send()
        .await
}

async fn read(mut response: reqwest::Response) -> Result<Vec<u8>, ()> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if bytes.len() + chunk.len() > BODY_LIMIT {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn message(mut response: reqwest::Response, id: u8) -> Result<Value, ()> {
    let sse = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"));
    if !sse {
        return Ok(serde_json::from_slice(&read(response).await?).unwrap_or_default());
    }
    let mut buffered = Vec::new();
    let mut observed = 0;
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        observed += chunk.len();
        if observed > BODY_LIMIT {
            return Err(());
        }
        buffered.extend_from_slice(&chunk);
        let Some(last_line) = buffered.iter().rposition(|byte| *byte == b'\n') else {
            continue;
        };
        for line in buffered[..=last_line].split(|byte| *byte == b'\n') {
            if let Some(raw) = line.strip_prefix(b"data:")
                && let Ok(value) = serde_json::from_slice::<Value>(raw)
                && value["id"] == id
                && value.get("result").is_some()
            {
                return Ok(value);
            }
        }
        buffered.drain(..=last_line);
    }
    Ok(Value::Null)
}
