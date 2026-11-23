use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout_at};

use super::managed_relay::{
    Platform, api_client, api_error, dns_label, fixture, request_json, save, token,
};
use super::{StateData, error};

const API: &str = "https://api.deno.com/v2";
const FIXTURE_API: &str = "http://127.0.0.1:52989/deno-management/v2";
const RUNTIME: &str = include_str!("relay_assets/deno_relay_runtime.mjs");
const HANDLER: &str = include_str!("relay_assets/cloudflare_relay_worker.mjs");
const PINNED_HTTPS: &str = include_str!("relay_assets/pinned_relay_https.mjs");

fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn app_slug(value: &str) -> bool {
    (3..=32).contains(&value.len())
        && dns_label(value)
        && value.as_bytes().get(2..4).is_none_or(|pair| pair != b"--")
}

fn production(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|value| value.eq_ignore_ascii_case("production"))
}

fn bound(app: &Value, url: &str) -> bool {
    let Some(vars) = app["env_vars"].as_array() else {
        return false;
    };
    let binding = |name: &str| {
        vars.iter().find(|var| {
            var["key"] == name
                && var["contexts"]
                    .as_array()
                    .is_some_and(|contexts| contexts.len() == 1 && production(&contexts[0]))
        })
    };
    binding("RELAY_AUTH").is_some_and(|var| var["secret"] == true && var.get("value").is_none())
        && binding("RELAY_URL").is_some_and(|var| var["secret"] == false && var["value"] == url)
}

fn confirmed(revision: &Value, host: &str) -> bool {
    revision["timelines"].as_array().is_some_and(|timelines| {
        timelines.iter().any(|timeline| {
            production(&timeline["context"])
                && timeline["hostnames"]
                    .as_array()
                    .is_some_and(|hosts| hosts.iter().any(|item| item == host))
        })
    })
}

fn revision_id(value: &Value) -> Option<&str> {
    value.as_str().filter(|id| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    })
}

pub(super) async fn deploy(State(state): State<StateData>, request: Request) -> Response {
    let Ok(bytes) = to_bytes(request.into_body(), 16 * 1024).await else {
        return error(StatusCode::BAD_REQUEST, "Invalid Deno deployment request");
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return error(StatusCode::BAD_REQUEST, "Invalid Deno deployment request");
    };
    let Some(org) = nonempty(&body["orgDomain"]) else {
        return error(StatusCode::BAD_REQUEST, "Organization domain is required");
    };
    let Some(api_token) = nonempty(&body["denoToken"]) else {
        return error(StatusCode::BAD_REQUEST, "Deno Deploy API token is required");
    };
    let org = org.strip_suffix(".deno.net").unwrap_or(org);
    if !dns_label(org) {
        return error(StatusCode::BAD_REQUEST, "Invalid Deno organization domain");
    }
    if api_token.len() > 512 || !api_token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return error(StatusCode::BAD_REQUEST, "Invalid Deno Deploy API token");
    }
    let name = nonempty(&body["projectName"])
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            format!("relay-{}", &suffix[..20])
        });
    if !app_slug(&name) {
        return error(StatusCode::BAD_REQUEST, "Invalid Deno app name");
    }
    let host = format!("{name}.{org}.deno.net");
    let url = format!("https://{host}");
    let Some(secret) = token() else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not generate the relay token",
        );
    };
    let Some(client) = api_client() else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not initialize the Deno API client",
        );
    };
    let base = if fixture() { FIXTURE_API } else { API };
    let created = request_json(
        client
            .post(format!("{base}/apps"))
            .bearer_auth(api_token)
            .json(&json!({
                "slug":name,
                "labels":{"custom.kind":"9router-relay"},
                "config":{"runtime":{"type":"dynamic","entrypoint":"main.mjs"}},
                "env_vars":[
                    {"key":"RELAY_AUTH","value":secret,"secret":true,"contexts":["production"]},
                    {"key":"RELAY_URL","value":url,"secret":false,"contexts":["production"]},
                ],
            })),
    )
    .await;
    let app = match created {
        Ok(app) => app,
        Err(StatusCode::CONFLICT) => {
            return error(
                StatusCode::CONFLICT,
                &format!("App \"{name}\" already exists. Choose a different name."),
            );
        }
        Err(status) => return api_error(status, "Could not create the Deno app"),
    };
    let Some(id) = app["id"]
        .as_str()
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
    else {
        return error(
            StatusCode::BAD_GATEWAY,
            "Deno did not confirm the new app identity",
        );
    };
    if app["slug"] != name || !bound(&app, &url) {
        return error(
            StatusCode::BAD_GATEWAY,
            "Deno did not confirm the app relay bindings",
        );
    }
    let mut revision = match request_json(
        client
            .post(format!("{base}/apps/{id}/deploy"))
            .bearer_auth(api_token)
            .json(&json!({
                "assets":{
                    "main.mjs":{"kind":"file","encoding":"utf-8","content":RUNTIME},
                    "relay.mjs":{"kind":"file","encoding":"utf-8","content":HANDLER},
                    "pinned-https.mjs":{"kind":"file","encoding":"utf-8","content":PINNED_HTTPS},
                },
                "production":true,"preview":false,
            })),
    )
    .await
    {
        Ok(revision) => revision,
        Err(status) => return api_error(status, "App created but Deno could not deploy the relay"),
    };
    let Some(id) = revision_id(&revision["id"]).map(str::to_owned) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "Deno did not confirm the deployment revision",
        );
    };
    let interval = if fixture() {
        Duration::from_millis(25)
    } else {
        Duration::from_secs(2)
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while matches!(revision["status"].as_str(), Some("queued" | "building")) {
        revision = match timeout_at(deadline, async {
            sleep(interval).await;
            request_json(
                client
                    .get(format!("{base}/revisions/{id}"))
                    .bearer_auth(api_token),
            )
            .await
        })
        .await
        {
            Ok(Ok(revision)) if revision["id"] == id => revision,
            Ok(Ok(_)) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Deno returned a different deployment revision",
                );
            }
            Ok(Err(status)) => {
                return api_error(status, "Could not confirm the Deno deployment status");
            }
            Err(_) => {
                return error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "The Deno deployment did not finish within 60 seconds",
                );
            }
        };
    }
    if revision["status"] != "succeeded" {
        return error(
            StatusCode::BAD_GATEWAY,
            "App created but the Deno relay deployment failed",
        );
    }
    if !confirmed(&revision, &host) {
        return error(
            StatusCode::BAD_GATEWAY,
            "Deno did not confirm the exact production relay URL",
        );
    }
    save(state, Platform::Deno, name, url, secret).await
}
