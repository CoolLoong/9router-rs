use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

use super::managed_relay::{
    NETLIFY_FUNCTION_PATH, Platform, api_client, api_error, dns_label, fixture, request_json, save,
    token,
};
use super::{StateData, error};

const API: &str = "https://api.netlify.com/api/v1";
const FIXTURE_API: &str = "http://127.0.0.1:52989/netlify-management/api/v1";
const INDEX: &str = "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>9Router Relay</title></head>\n<body><p>9Router relay function lives at <code>/.netlify/functions/relay</code>.</p></body></html>\n";
const FILES: &[(&str, &str)] = &[
    (
        "relay.js",
        include_str!("../assets/relay/netlify_relay_function.cjs"),
    ),
    (
        "relay.mjs",
        include_str!("../assets/relay/cloudflare_relay_worker.mjs"),
    ),
    (
        "pinned-https.mjs",
        include_str!("../assets/relay/pinned_relay_https.mjs"),
    ),
];

fn permitted_id(value: &Value) -> Option<&str> {
    value.as_str().filter(|id| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    })
}

fn site_name(value: &Value) -> String {
    let desired = value.as_str().unwrap_or_default().trim().to_lowercase();
    let mut name = String::with_capacity(desired.len());
    for character in desired.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            name.push(character);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    if name.ends_with('-') {
        name.pop();
    }
    if name.is_empty() {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        return format!("relay-{}", &suffix[..20]);
    }
    name
}

fn archive() -> Option<Vec<u8>> {
    let mut contents = Vec::new();
    let mut directory = Vec::new();
    for (name, content) in FILES {
        let offset = u32::try_from(contents.len()).ok()?;
        let bytes = u32::try_from(content.len()).ok()?.to_le_bytes();
        let filename = u16::try_from(name.len()).ok()?.to_le_bytes();
        let mut crc = flate2::Crc::new();
        crc.update(content.as_bytes());
        let checksum = crc.sum().to_le_bytes();
        let flag = 0x0800_u16.to_le_bytes();

        let mut local = [0; 30];
        local[..4].copy_from_slice(&0x0403_4b50_u32.to_le_bytes());
        local[4..6].copy_from_slice(&20_u16.to_le_bytes());
        local[6..8].copy_from_slice(&flag);
        local[14..18].copy_from_slice(&checksum);
        local[18..22].copy_from_slice(&bytes);
        local[22..26].copy_from_slice(&bytes);
        local[26..28].copy_from_slice(&filename);
        contents.extend_from_slice(&local);
        contents.extend_from_slice(name.as_bytes());
        contents.extend_from_slice(content.as_bytes());

        let mut entry = [0; 46];
        entry[..4].copy_from_slice(&0x0201_4b50_u32.to_le_bytes());
        entry[8..10].copy_from_slice(&flag);
        entry[16..20].copy_from_slice(&checksum);
        entry[20..24].copy_from_slice(&bytes);
        entry[24..28].copy_from_slice(&bytes);
        entry[28..30].copy_from_slice(&filename);
        entry[42..46].copy_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(&entry);
        directory.extend_from_slice(name.as_bytes());
    }
    let mut end = [0; 22];
    end[..4].copy_from_slice(&0x0605_4b50_u32.to_le_bytes());
    let count = u16::try_from(FILES.len()).ok()?.to_le_bytes();
    end[8..10].copy_from_slice(&count);
    end[10..12].copy_from_slice(&count);
    end[12..16].copy_from_slice(&u32::try_from(directory.len()).ok()?.to_le_bytes());
    end[16..20].copy_from_slice(&u32::try_from(contents.len()).ok()?.to_le_bytes());
    contents.extend(directory);
    contents.extend_from_slice(&end);
    Some(contents)
}

struct Netlify<'a> {
    client: Client,
    base: &'static str,
    token: &'a str,
}

impl Netlify<'_> {
    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(self.token)
            .header(header::USER_AGENT, "9Router")
    }

    async fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<(), StatusCode> {
        let response = self
            .request(Method::PUT, path)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        if !response.status().is_success() {
            return Err(response.status());
        }
        Ok(())
    }
}

fn configured(value: &Value, url: &str) -> bool {
    let Some(bindings) = value.as_array() else {
        return false;
    };
    let variable = |name: &str| {
        bindings.iter().find(|binding| {
            binding["key"] == name
                && binding.get("scopes").is_none_or(|scopes| {
                    scopes.as_array().is_some_and(|scopes| {
                        scopes.is_empty() || scopes.iter().any(|scope| scope == "functions")
                    })
                })
                && binding["values"]
                    .as_array()
                    .is_some_and(|values| values.len() == 1 && values[0]["context"] == "production")
        })
    };
    variable("RELAY_AUTH").is_some_and(|variable| variable["is_secret"] == true)
        && variable("RELAY_URL").is_some_and(|variable| {
            variable["is_secret"] == false && variable["values"][0]["value"] == url
        })
}

fn required(value: &Value, digest: &str) -> Option<bool> {
    let items = value.as_array()?;
    items
        .iter()
        .all(|item| item == digest)
        .then_some(!items.is_empty())
}

fn ssl_url(site: &Value) -> Option<Url> {
    site["ssl_url"]
        .as_str()
        .or_else(|| site["url"].as_str())
        .and_then(|value| Url::parse(value).ok())
}

pub(super) async fn deploy(State(state): State<StateData>, request: Request) -> Response {
    let Ok(bytes) = to_bytes(request.into_body(), 16 * 1024).await else {
        return error(
            StatusCode::BAD_REQUEST,
            "Invalid Netlify deployment request",
        );
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Invalid Netlify deployment request",
        );
    };
    let Some(pat) = body["netlifyToken"]
        .as_str()
        .map(str::trim)
        .filter(|token| {
            !token.is_empty()
                && token.len() <= 512
                && token.bytes().all(|byte| byte.is_ascii_graphic())
        })
    else {
        return error(StatusCode::BAD_REQUEST, "Netlify API token is required");
    };
    let name = site_name(
        body.get("projectName")
            .filter(|value| !value.is_null())
            .unwrap_or(&body["siteName"]),
    );
    if !dns_label(&name) {
        return error(StatusCode::BAD_REQUEST, "Invalid Netlify project name");
    }
    let (Some(secret), Some(bundle), Some(client)) = (token(), archive(), api_client()) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not initialize the Netlify relay deployment",
        );
    };
    let api = Netlify {
        client,
        base: if fixture() { FIXTURE_API } else { API },
        token: pat,
    };
    let created = match request_json(
        api.request(Method::POST, "/sites")
            .json(&json!({"name":name})),
    )
    .await
    {
        Ok(site) => site,
        Err(StatusCode::UNPROCESSABLE_ENTITY | StatusCode::CONFLICT) => {
            return error(
                StatusCode::CONFLICT,
                "A Netlify site with this name already exists",
            );
        }
        Err(status) => return api_error(status, "Could not create the Netlify site"),
    };
    let Some(id) = uuid::Uuid::parse_str(
        created["id"]
            .as_str()
            .or_else(|| created["site_id"].as_str())
            .unwrap_or_default(),
    )
    .ok() else {
        return error(
            StatusCode::BAD_GATEWAY,
            "Netlify did not confirm a new site ID",
        );
    };
    let Some(account) =
        permitted_id(&created["account_id"]).or_else(|| permitted_id(&created["account_slug"]))
    else {
        return error(
            StatusCode::BAD_GATEWAY,
            "Netlify did not return the site's account",
        );
    };
    let origin = format!("https://{name}.netlify.app");
    let expected = Url::parse(&origin).ok();
    if created["name"] != name || ssl_url(&created).is_some_and(|url| Some(url) != expected) {
        return error(
            StatusCode::BAD_GATEWAY,
            "Netlify returned a different site or URL",
        );
    }
    let url = format!("{origin}{NETLIFY_FUNCTION_PATH}");
    let account_env = api.request(Method::POST, &format!("/accounts/{account}/env"))
        .query(&[("site_id", id.to_string())])
        .json(&json!([
            {"key":"RELAY_AUTH","is_secret":true,"values":[{"context":"production","value":secret}]},
            {"key":"RELAY_URL","is_secret":false,"values":[{"context":"production","value":url}]},
        ]));
    match request_json(account_env).await {
        Ok(bindings) if configured(&bindings, &url) => {}
        Ok(_) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "Netlify did not confirm the production relay secrets",
            );
        }
        Err(status) => {
            return api_error(
                status,
                "Site created but Netlify could not bind the production relay secrets",
            );
        }
    }
    let digest = format!("{:x}", Sha1::digest(INDEX.as_bytes()));
    let function = format!("{:x}", Sha256::digest(&bundle));
    let site_deploys = format!("/sites/{id}/deploys");
    let mut deployment = match request_json(
        api.request(Method::POST, &site_deploys)
            .query(&[("production", true)])
            .json(&json!({"files":{"/index.html":digest},"functions":{"relay":function}})),
    )
    .await
    {
        Ok(deployment) => deployment,
        Err(status) => {
            return api_error(
                status,
                "Site created but Netlify could not start the relay deploy",
            );
        }
    };
    let Some(deploy_id) = permitted_id(&deployment["id"]).map(str::to_owned) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "Netlify did not return a relay deploy ID",
        );
    };
    let (Some(file_needed), Some(function_needed)) = (
        required(&deployment["required"], &digest),
        required(&deployment["required_functions"], &function),
    ) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "Netlify returned an unexpected deploy digest",
        );
    };
    if deployment["site_id"] != id.to_string() {
        return error(
            StatusCode::BAD_GATEWAY,
            "Netlify returned a deploy for a different site",
        );
    }
    let upload = format!("/deploys/{deploy_id}");
    if file_needed
        && let Err(status) = api
            .upload(
                &format!("{upload}/files/index.html"),
                INDEX.as_bytes().to_vec(),
            )
            .await
    {
        return api_error(status, "Netlify rejected the relay index upload");
    }
    if function_needed
        && let Err(status) = api
            .upload(&format!("{upload}/functions/relay?runtime=js"), bundle)
            .await
    {
        return api_error(status, "Netlify rejected the relay function upload");
    }
    let interval = if fixture() {
        Duration::from_millis(25)
    } else {
        Duration::from_secs(3)
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    let deploy_status = format!("{site_deploys}/{deploy_id}");
    loop {
        match deployment["state"].as_str() {
            Some("ready") => break,
            Some(
                "new" | "pending_review" | "accepted" | "enqueued" | "building" | "uploading"
                | "uploaded" | "preparing" | "prepared" | "processing" | "processed" | "retrying",
            ) => {}
            _ => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "The Netlify relay deployment failed",
                );
            }
        }
        deployment = match timeout_at(deadline, async {
            sleep(interval).await;
            request_json(api.request(Method::GET, &deploy_status)).await
        })
        .await
        {
            Ok(Ok(next)) if next["id"] == deploy_id && next["site_id"] == id.to_string() => next,
            Ok(Ok(_)) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Netlify returned a different deployment",
                );
            }
            Ok(Err(status)) => {
                return api_error(status, "Could not confirm the Netlify relay deployment");
            }
            Err(_) => {
                return error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "The Netlify relay deploy did not finish within 120 seconds",
                );
            }
        };
    }
    let site = match request_json(api.request(Method::GET, &format!("/sites/{id}"))).await {
        Ok(site) => site,
        Err(status) => {
            return api_error(status, "Netlify could not confirm the production relay URL");
        }
    };
    if site["id"] != id.to_string()
        || site["name"] != name
        || site["published_deploy"]["id"] != deploy_id
        || ssl_url(&site) != expected
        || Platform::Netlify.url(&url).is_none()
    {
        return error(
            StatusCode::BAD_GATEWAY,
            "Netlify did not confirm this deploy on the exact production relay URL",
        );
    }
    save(state, Platform::Netlify, name, url, secret).await
}
