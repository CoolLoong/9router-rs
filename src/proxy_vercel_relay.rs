use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout_at};

use super::managed_relay::{
    Platform, VERCEL_FUNCTION_PATH, api_client, api_error, dns_label, fixture, request_json, save,
    token,
};
use super::{StateData, error};

const API: &str = "https://api.vercel.com";
const FIXTURE_API: &str = "http://127.0.0.1:52989/vercel-management";
const ASSETS: &[(&str, &str)] = &[
    (
        "api/relay.mjs",
        include_str!("../assets/relay/vercel_relay_function.mjs"),
    ),
    (
        "lib/relay.mjs",
        include_str!("../assets/relay/cloudflare_relay_worker.mjs"),
    ),
    (
        "lib/pinned-https.mjs",
        include_str!("../assets/relay/pinned_relay_https.mjs"),
    ),
];

fn id<'a>(value: &'a Value, prefix: &str) -> Option<&'a str> {
    value.as_str().filter(|id| {
        id.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix.len() <= 128
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    })
}

struct Vercel<'a> {
    client: Client,
    base: &'static str,
    pat: &'a str,
    team: Option<&'a str>,
}

impl Vercel<'_> {
    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(self.pat);
        if let Some(team) = self.team {
            request.query(&[("teamId", team)])
        } else {
            request
        }
    }
}

fn secret_confirmed(response: &Value) -> bool {
    let bound = &response["created"];
    response["failed"].as_array().is_some_and(Vec::is_empty)
        && bound["key"] == "RELAY_AUTH"
        && bound["type"] == "sensitive"
        && (bound["target"] == "production" || bound["target"] == json!(["production"]))
}

async fn challenge(request: RequestBuilder, status: StatusCode, reason: &str) -> bool {
    let Ok(mut response) = request.timeout(Duration::from_secs(8)).send().await else {
        return false;
    };
    if response.status() != status {
        return false;
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(None) => break,
            Ok(Some(chunk)) if chunk.len() <= 1024 - body.len() => body.extend_from_slice(&chunk),
            _ => return false,
        }
    }
    serde_json::from_slice::<Value>(&body).is_ok_and(|value| value == json!({"error":reason}))
}

async fn closed_relay(client: &Client, url: &str, secret: &str) -> bool {
    let Some(wire) = Platform::Vercel.request_url(url) else {
        return false;
    };
    let Ok(mut authentication) = HeaderValue::from_str(secret) else {
        return false;
    };
    authentication.set_sensitive(true);
    challenge(
        client.get(&wire),
        StatusCode::FORBIDDEN,
        "Relay authorization required",
    )
    .await
        && challenge(
            client.get(&wire).header("x-relay-auth", authentication),
            StatusCode::BAD_REQUEST,
            "A public HTTPS destination is required",
        )
        .await
}

pub(super) async fn deploy(State(state): State<StateData>, request: Request) -> Response {
    let Ok(bytes) = to_bytes(request.into_body(), 16 * 1024).await else {
        return error(StatusCode::BAD_REQUEST, "Invalid Vercel deployment request");
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return error(StatusCode::BAD_REQUEST, "Invalid Vercel deployment request");
    };
    let Some(pat) = body["vercelToken"].as_str().map(str::trim).filter(|token| {
        !token.is_empty() && token.len() <= 512 && token.bytes().all(|byte| byte.is_ascii_graphic())
    }) else {
        return error(StatusCode::BAD_REQUEST, "Vercel API token is required");
    };
    let team = id(&body["teamId"], "team_");
    if body.get("teamId").is_some_and(|value| !value.is_null()) && team.is_none() {
        return error(StatusCode::BAD_REQUEST, "Invalid Vercel Team ID");
    }
    let name = body["projectName"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            format!("relay-{}", &suffix[..20])
        });
    if !dns_label(&name) {
        return error(StatusCode::BAD_REQUEST, "Invalid Vercel project name");
    }
    let (Some(secret), Some(client)) = (token(), api_client()) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not initialize the Vercel relay deployment",
        );
    };
    let api = Vercel {
        client,
        base: if fixture() { FIXTURE_API } else { API },
        pat,
        team,
    };
    let created = match request_json(api.request(Method::POST, "/v11/projects").json(&json!({
        "name":name,"framework":null,
    })))
    .await
    {
        Ok(project) => project,
        Err(StatusCode::CONFLICT) => {
            return error(
                StatusCode::CONFLICT,
                "A Vercel project with this name already exists",
            );
        }
        Err(status) => return api_error(status, "Could not create the Vercel project"),
    };
    let Some(project) = id(&created["id"], "prj_").filter(|_| created["name"] == name) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "Vercel did not confirm the new project identity",
        );
    };
    if created["autoExposeSystemEnvs"] == false {
        match request_json(
            api.request(Method::PATCH, &format!("/v9/projects/{project}"))
                .json(&json!({"autoExposeSystemEnvs":true})),
        )
        .await
        {
            Ok(result) if result["id"] == project && result["autoExposeSystemEnvs"] == true => {}
            _ => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Vercel did not enable the new project's production URL binding",
                );
            }
        }
    }
    match request_json(
        api.request(Method::POST, &format!("/v10/projects/{project}/env"))
            .json(&json!({
                "key":"RELAY_AUTH","value":secret,"type":"sensitive","target":["production"],
            })),
    )
    .await
    {
        Ok(response) if secret_confirmed(&response) => {}
        Ok(_) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "Vercel did not confirm the production relay secret",
            );
        }
        Err(status) => {
            return api_error(
                status,
                "Project created but Vercel could not bind the production relay secret",
            );
        }
    }
    let mut files: Vec<_> = ASSETS.iter().map(|(path, contents)| {
        json!({"file":path,"data":STANDARD.encode(contents),"encoding":"base64"})
    }).collect();
    files.push(json!({
        "file":"package.json","encoding":"base64",
        "data":STANDARD.encode(json!({"name":name,"version":"1.0.0","type":"module","private":true}).to_string()),
    }));
    let deploy = match request_json(api.request(Method::POST, "/v13/deployments").json(&json!({
        "name":name,"project":project,"target":"production","projectSettings":{"framework":null},
        "files":files,
    })))
    .await
    {
        Ok(deployment) => deployment,
        Err(status) => {
            return api_error(
                status,
                "Project created but Vercel could not start the relay deploy",
            );
        }
    };
    let Some(deployment) = id(&deploy["id"], "dpl_").or_else(|| id(&deploy["uid"], "dpl_")) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "Vercel did not confirm the relay deployment ID",
        );
    };
    let interval = if fixture() {
        Duration::from_millis(25)
    } else {
        Duration::from_secs(3)
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    let poll = format!("/v13/deployments/{deployment}");
    let ready = loop {
        let current = match timeout_at(deadline, request_json(api.request(Method::GET, &poll)))
            .await
        {
            Ok(Ok(current)) if current["id"] == deployment && current["target"] == "production" => {
                current
            }
            Ok(Ok(_)) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Vercel returned a different production deployment",
                );
            }
            Ok(Err(status)) => {
                return api_error(status, "Could not verify the Vercel relay deployment");
            }
            Err(_) => {
                return error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "The Vercel relay did not finish within 120 seconds",
                );
            }
        };
        if current["aliasError"].is_object() {
            return error(
                StatusCode::BAD_GATEWAY,
                "Vercel could not assign the production relay URL",
            );
        }
        match current["readyState"].as_str() {
            Some("READY") if current["aliasAssigned"] == true => break current,
            Some("QUEUED" | "INITIALIZING" | "BUILDING" | "READY") => {}
            _ => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Project created but the Vercel relay deployment failed",
                );
            }
        }
        if timeout_at(deadline, sleep(interval)).await.is_err() {
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "The Vercel relay did not finish within 120 seconds",
            );
        }
    };
    let domains = match request_json(
        api.request(Method::GET, &format!("/v9/projects/{project}/domains"))
            .query(&[
                ("production", "true"),
                ("verified", "true"),
                ("redirects", "false"),
            ]),
    )
    .await
    {
        Ok(result) => result,
        Err(status) => {
            return api_error(status, "Could not read the Vercel production relay domains");
        }
    };
    let aliases = ready["alias"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut hosts = domains["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|domain| {
            domain["verified"] == true
                && domain["redirect"].is_null()
                && domain["gitBranch"].is_null()
                && domain["customEnvironmentId"].is_null()
                && aliases.contains(&domain["name"])
        })
        .filter_map(|domain| domain["name"].as_str())
        .filter(|host| host.strip_suffix(".vercel.app").is_some_and(dns_label))
        .collect::<Vec<_>>();
    hosts.sort_unstable_by_key(|host| (host.len(), *host));
    hosts.dedup();
    for host in hosts {
        let alias = match request_json(
            api.request(Method::GET, &format!("/v4/aliases/{host}"))
                .query(&[("projectId", project)]),
        )
        .await
        {
            Ok(alias) => alias,
            Err(_) => continue,
        };
        if alias["alias"] != host
            || alias["projectId"] != project
            || alias["deploymentId"] != deployment
            || !alias["deletedAt"].is_null()
            || !alias["redirect"].is_null()
        {
            continue;
        }
        let url = format!("https://{host}{VERCEL_FUNCTION_PATH}");
        if closed_relay(&api.client, &url, &secret).await {
            return save(state, Platform::Vercel, name, url, secret).await;
        }
    }
    error(
        StatusCode::BAD_GATEWAY,
        "Vercel did not confirm an accessible protected production relay",
    )
}
