use axum::http::{HeaderValue, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};
use url::Url;

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::thinking_intent::numeric;
use crate::vertex_transport::{
    ORIGIN, ServiceAccount, bearer_header, fixture_url, is_fixture, valid_segment,
};

const REGION: &str = "us-central1";
const MISSING_PROJECT: &str = "Vertex video requires a project_id — use Service Account JSON or set providerSpecificData.projectId";
const MISSING_TOKEN: &str = "Vertex video requires Service Account JSON or an OAuth access token (raw API keys are not supported)";

pub enum Job<'a> {
    Create {
        action: &'a str,
        content_type: Option<&'a str>,
        raw_body: &'a [u8],
    },
    Poll(&'a str),
}

pub struct Draft {
    url: Url,
    payload: Value,
    credential: Credential,
}

pub struct Plan {
    payload: Value,
    authorization: HeaderValue,
}

impl Plan {
    pub fn request_at(&self, client: &reqwest::Client, endpoint: &str) -> reqwest::RequestBuilder {
        client
            .post(endpoint)
            .header(header::ACCEPT, "application/json")
            .header(header::AUTHORIZATION, self.authorization.clone())
            .json(&self.payload)
    }

    pub fn redact_error(&self, text: &str) -> String {
        let token = self
            .authorization
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|token| !token.is_empty());
        token.map_or_else(
            || text.to_owned(),
            |token| text.replace(token, "[redacted]"),
        )
    }
}

impl Draft {
    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    pub fn direct_url(&self) -> String {
        if is_fixture() {
            fixture_url(&self.url)
        } else {
            self.url.to_string()
        }
    }

    pub fn needs_token_exchange(&self) -> bool {
        matches!(self.credential, Credential::Service(_))
    }

    pub async fn authorize(
        self,
        client: &reqwest::Client,
        fixture: bool,
    ) -> Result<Plan, &'static str> {
        let authorization = match self.credential {
            Credential::Service(service) => service.token(client, fixture).await.map_err(
                |_| "Vertex video: failed to mint access token from service account JSON",
            )?,
            Credential::Bearer(authorization) => authorization,
        };
        Ok(Plan {
            payload: self.payload,
            authorization,
        })
    }
}

pub fn prepare(account: &Value, job: Job<'_>) -> Result<Draft, &'static str> {
    validate_media_type(&job)?;
    let Identity {
        project,
        location,
        credential,
    } = identity(account)?;
    let (url, payload) = request_body(job, &project, &location)?;
    Ok(Draft {
        url,
        payload,
        credential,
    })
}

fn validate_media_type(job: &Job<'_>) -> Result<(), &'static str> {
    if let Job::Create {
        content_type: Some(mime),
        ..
    } = job
        && !mime.is_empty()
        && !mime.contains("application/json")
    {
        return Err("Vertex video requires an application/json body");
    }
    Ok(())
}

fn request_body(job: Job<'_>, project: &str, location: &str) -> Result<(Url, Value), &'static str> {
    let (path, payload) = match job {
        Job::Poll(id) => {
            let operation = decode_job_id(id).ok_or("Invalid Vertex video job id")?;
            let (path, _) = operation
                .rsplit_once("/operations/")
                .ok_or("Invalid Vertex video job id")?;
            (
                format!("/v1/{path}:fetchPredictOperation"),
                json!({"operationName":operation}),
            )
        }
        Job::Create {
            action: "generations",
            raw_body,
            ..
        } => {
            let raw = String::from_utf8_lossy(raw_body);
            let body: Value = serde_json::from_str(&raw).map_err(|_| "Invalid JSON body")?;
            let source = &body["model"];
            if !truthy(source) {
                return Err("Vertex video requires a model (e.g. vertex/veo-3.1-generate-preview)");
            }
            let model = js_text(source);
            if !valid_segment(&model) || model.contains([':', '@']) {
                return Err("Invalid Vertex video model id");
            }
            if !["prompt", "image", "image_url"]
                .iter()
                .any(|field| truthy(&body[*field]))
            {
                return Err("Vertex video requires a prompt or an image");
            }
            (
                format!(
                    "/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:predictLongRunning"
                ),
                create_payload(&body),
            )
        }
        Job::Create {
            action: "edits", ..
        } => return Err("Vertex video supports 'generations' only (got 'edits')"),
        Job::Create {
            action: "extensions",
            ..
        } => return Err("Vertex video supports 'generations' only (got 'extensions')"),
        Job::Create { .. } => return Err("Vertex video supports 'generations' only"),
    };
    let url =
        Url::parse(&format!("{ORIGIN}{path}")).map_err(|_| "Invalid Vertex video endpoint")?;
    Ok((url, payload))
}

enum Credential {
    Service(ServiceAccount),
    Bearer(HeaderValue),
}

struct Identity {
    project: String,
    location: String,
    credential: Credential,
}

fn identity(account: &Value) -> Result<Identity, &'static str> {
    let service = account["apiKey"].as_str().and_then(ServiceAccount::parse);
    let details = &account["providerSpecificData"];
    let project = service
        .as_ref()
        .map(|service| service.project.clone())
        .or_else(|| {
            [&account["projectId"], &details["projectId"]]
                .into_iter()
                .find(|value| truthy(value))
                .map(js_text)
        })
        .ok_or(MISSING_PROJECT)?;
    if !valid_segment(&project) {
        return Err("Invalid Vertex video project ID");
    }
    let location = if truthy(&details["location"]) {
        js_text(&details["location"])
    } else {
        REGION.to_owned()
    };
    if !valid_segment(&location) {
        return Err("Invalid Vertex video location");
    }
    let credential = if let Some(service) = service {
        Credential::Service(service)
    } else {
        let token = account["accessToken"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or(MISSING_TOKEN)?;
        Credential::Bearer(bearer_header(token)?)
    };
    Ok(Identity {
        project,
        location,
        credential,
    })
}

fn decode_job_id(id: &str) -> Option<String> {
    if id.is_empty()
        || id.len() > 1024
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return None;
    }
    let decoded = String::from_utf8(URL_SAFE_NO_PAD.decode(id).ok()?).ok()?;
    if URL_SAFE_NO_PAD.encode(decoded.as_bytes()) != id {
        return None;
    }
    let mut segments = decoded.split('/');
    for prefix in [
        "projects",
        "locations",
        "publishers",
        "models",
        "operations",
    ] {
        if segments.next() != Some(prefix) || !valid_segment(segments.next()?) {
            return None;
        }
    }
    segments.next().is_none().then_some(decoded)
}

fn create_payload(body: &Value) -> Value {
    let mut instance = Map::new();
    if let Some(prompt) = body.get("prompt") {
        instance.insert("prompt".to_owned(), prompt.clone());
    }
    let image = body
        .get("image")
        .filter(|image| !image.is_null())
        .unwrap_or(&body["image_url"]);
    if image.is_object() || image.is_array() {
        instance.insert("image".to_owned(), image.clone());
    } else if let Some(image) = image.as_str() {
        let image = if let Some((mime, payload)) = image
            .strip_prefix("data:")
            .and_then(|data| data.split_once(";base64,"))
            && !mime.is_empty()
            && !mime.contains(';')
        {
            json!({"bytesBase64Encoded":payload,"mimeType":mime})
        } else {
            json!({"gcsUri":image})
        };
        instance.insert("image".to_owned(), image);
    }
    if body["video"].is_object() || body["video"].is_array() {
        instance.insert("video".to_owned(), body["video"].clone());
    }
    let mut parameters = Map::new();
    for (field, parameter) in [("n", "sampleCount"), ("duration", "durationSeconds")] {
        if !body[field].is_null() {
            parameters.insert(parameter.to_owned(), json!(javascript_number(&body[field])));
        }
    }
    for (field, parameter) in [
        ("aspect_ratio", "aspectRatio"),
        ("resolution", "resolution"),
        ("negative_prompt", "negativePrompt"),
        ("storage_uri", "storageUri"),
    ] {
        if truthy(&body[field]) {
            parameters.insert(parameter.to_owned(), body[field].clone());
        }
    }
    if !body["seed"].is_null() {
        parameters.insert("seed".to_owned(), body["seed"].clone());
    }
    if !body["generate_audio"].is_null() {
        parameters.insert(
            "generateAudio".to_owned(),
            json!(truthy(&body["generate_audio"])),
        );
    }
    let mut payload = json!({"instances":[instance]});
    if !parameters.is_empty() {
        payload["parameters"] = Value::Object(parameters);
    }
    payload
}

fn javascript_number(value: &Value) -> Option<f64> {
    let text = if value.is_array() {
        js_text(value)
    } else if let Some(text) = value.as_str() {
        text.to_owned()
    } else {
        return numeric(value);
    };
    let text = js_trim(&text);
    let prefix = text.get(..2).map(str::to_ascii_lowercase);
    let radix = match prefix.as_deref() {
        Some("0x") => Some(16),
        Some("0o") => Some(8),
        Some("0b") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        return u64::from_str_radix(&text[2..], radix)
            .ok()
            .map(|value| value as f64);
    }
    numeric(&json!(text))
}

pub fn normalize(raw: &str) -> Option<String> {
    let output: Value = serde_json::from_str(raw).ok()?;
    if !truthy(&output["name"]) {
        return Some(output.to_string());
    }
    let name = output["name"].as_str()?;
    let id = URL_SAFE_NO_PAD.encode(name.as_bytes());
    if truthy(&output["error"]) {
        return Some(
            json!({"id":id,"request_id":id,"status":"failed","error":output["error"]}).to_string(),
        );
    }
    if !truthy(&output["done"]) {
        return Some(json!({"id":id,"request_id":id,"status":"pending"}).to_string());
    }
    let fallback_mime = json!("video/mp4");
    let empty = json!([]);
    let samples = choose([
        &output["response"]["videos"],
        &output["response"]["generateVideoResponse"]["generatedSamples"],
        &empty,
    ])
    .as_array()?;
    let mut videos = Vec::with_capacity(samples.len());
    for sample in samples {
        if sample.is_null() {
            return None;
        }
        videos.push(json!({
            "url":choose([&sample["gcsUri"],&sample["video"]["uri"],&sample["uri"]]),
            "b64_json":choose([&sample["bytesBase64Encoded"],&sample["video"]["bytesBase64Encoded"]]),
            "mime_type":choose([&sample["mimeType"],&sample["video"]["mimeType"],&fallback_mime]),
        }));
    }
    Some(json!({"id":id,"request_id":id,"status":"completed","video":videos.first(),"videos":videos}).to_string())
}

fn choose<'a>(values: impl IntoIterator<Item = &'a Value>) -> &'a Value {
    values
        .into_iter()
        .find(|value| truthy(value))
        .unwrap_or(&Value::Null)
}
