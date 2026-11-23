use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{SecondsFormat, TimeDelta, Utc};
use reqwest::Client;
use rusqlite::TransactionBehavior;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::provider_connections as connections;
use crate::provider_index::js_trim;
use crate::storage::Storage;
use crate::{dashboard_session, kiro_models};

const LIMIT: usize = 64 * 1024;
const KEY_ERROR: Failure = Failure(
    StatusCode::INTERNAL_SERVER_ERROR,
    "API key validation failed",
);
const IMPORT_ERROR: Failure = Failure(
    StatusCode::INTERNAL_SERVER_ERROR,
    "Kiro IAM Identity Center import failed",
);
const SOCIAL_IMPORT_ERROR: Failure = Failure(
    StatusCode::INTERNAL_SERVER_ERROR,
    "Kiro refresh token import failed",
);
const SAVE_ERROR: Failure = Failure(
    StatusCode::INTERNAL_SERVER_ERROR,
    "Could not save the provider connection",
);
const BAD_REGION: Failure = Failure(StatusCode::INTERNAL_SERVER_ERROR, "Invalid region");

type ResultBody = Result<Value, Failure>;
type ImportResult = (Option<String>, ResultBody);
type ImportSlot = AsyncMutex<Option<ImportResult>>;
type ActiveImports = HashMap<[u8; 32], Weak<ImportSlot>>;

#[derive(Clone)]
struct KiroAuth {
    storage: Storage,
    client: Client,
    fixture: Option<String>,
    imports: Arc<Mutex<ActiveImports>>,
}

#[derive(Clone, Copy)]
struct Failure(StatusCode, &'static str);

impl Failure {
    fn bad(message: &'static str) -> Self {
        Self(StatusCode::BAD_REQUEST, message)
    }
}

pub fn router(storage: Storage) -> Router {
    let fixture = (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"))
        .then(|| "http://127.0.0.1:52989/kiro".into());
    build_router(storage, fixture).expect("Kiro credential HTTP client")
}

pub(super) fn build_router(
    storage: Storage,
    fixture: Option<String>,
) -> Result<Router, reqwest::Error> {
    Ok(Router::new()
        .route("/api/oauth/kiro/api-key", post(api_key))
        .route("/api/oauth/kiro/import", post(import))
        .with_state(KiroAuth {
            storage,
            fixture,
            client: kiro_models::client()?,
            imports: Arc::default(),
        }))
}

async fn api_key(State(state): State<KiroAuth>, request: Request) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return unauthorized();
    }
    let result = async {
        let bytes = to_bytes(request.into_body(), LIMIT)
            .await
            .map_err(|_| KEY_ERROR)?;
        let payload = serde_json::from_slice::<Value>(&bytes).map_err(|_| KEY_ERROR)?;
        if payload.is_null() {
            return Err(KEY_ERROR);
        }
        let key =
            nonempty(&payload["apiKey"]).ok_or_else(|| Failure::bad("API key is required"))?;
        let region = region(&payload["region"]).ok_or(KEY_ERROR)?;
        if !kiro_models::validate_api_key(&state.client, key, region, state.fixture.as_deref())
            .await
        {
            return Err(KEY_ERROR);
        }
        let fields = credentials(
            key,
            "api_key",
            31_536_000,
            json!({
                "region":region, "authMethod":"api_key", "provider":"API Key",
            }),
        )?;
        save(&state.storage, fields).await.map_err(|_| KEY_ERROR)
    }
    .await;
    reply(result)
}

async fn import(State(state): State<KiroAuth>, request: Request) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return unauthorized();
    }
    let body = request.into_body();
    let invalid_body = || Failure::bad("Invalid or empty request body");
    let Ok(bytes) = to_bytes(body, LIMIT).await else {
        return reply(Err(invalid_body()));
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        return reply(Err(invalid_body()));
    };
    let Some(refresh) = nonempty(&payload["refreshToken"]) else {
        return reply(Err(Failure::bad("Refresh token is required")));
    };
    let grant = match Grant::from_request(&payload, refresh) {
        Ok(grant) => grant,
        Err(error) => return reply(Err(error)),
    };
    let result = state.import(grant).await;
    reply(result)
}

impl KiroAuth {
    async fn import(self, grant: Grant) -> ResultBody {
        let slot = self.import_slot(grant.key())?;
        tokio::spawn(async move {
            let mut current = slot.lock().await;
            if let Some((profile, outcome)) = &*current {
                if *profile != grant.profile {
                    return Err(Failure(
                        StatusCode::CONFLICT,
                        "Kiro credential import is already in progress",
                    ));
                }
                return outcome.clone();
            }
            let result = self.exchange(&grant).await;
            *current = Some((grant.profile, result.clone()));
            result
        })
        .await
        .map_err(|_| IMPORT_ERROR)?
    }

    fn import_slot(&self, key: [u8; 32]) -> Result<Arc<ImportSlot>, Failure> {
        let mut entries = self.imports.lock().map_err(|_| IMPORT_ERROR)?;
        entries.retain(|_, slot| slot.strong_count() > 0);
        if let Some(slot) = entries.get(&key).and_then(Weak::upgrade) {
            return Ok(slot);
        }
        let slot = Arc::new(AsyncMutex::new(None));
        entries.insert(key, Arc::downgrade(&slot));
        Ok(slot)
    }

    async fn exchange(&self, grant: &Grant) -> ResultBody {
        let (request, mut details, error) = match &grant.identity {
            Some(identity) => {
                let region = &identity.region;
                let url = self.fixture.as_ref().map_or_else(
                    || format!("https://oidc.{region}.amazonaws.com/token"),
                    |base| format!("{base}/oidc/{region}/token"),
                );
                (
                    self.client.post(url).json(&json!({
                        "clientId":identity.client,
                        "clientSecret":identity.secret,
                        "refreshToken":grant.refresh,
                        "grantType":"refresh_token",
                    })),
                    json!({
                        "authMethod":"idc","provider":"Enterprise",
                        "clientId":identity.client,"clientSecret":identity.secret,"region":region,
                    }),
                    IMPORT_ERROR,
                )
            }
            None => {
                let endpoint = crate::provider_oauth::KIRO_SOCIAL_REFRESH;
                let settings = self
                    .storage
                    .read_settings()
                    .await
                    .ok_or(SOCIAL_IMPORT_ERROR)?;
                let proxy = crate::proxy_transport::vendor_metadata_proxy(
                    &self.storage,
                    &settings,
                    &Value::Null,
                    endpoint,
                )
                .await
                .map_err(|_| SOCIAL_IMPORT_ERROR)?;
                let request = if let Some(proxy) = proxy {
                    if proxy.scheme() == "socks5" {
                        return Err(SOCIAL_IMPORT_ERROR);
                    }
                    crate::proxy_transport::vendor_metadata_client(
                        proxy,
                        kiro_models::client_builder(),
                    )
                    .ok_or(SOCIAL_IMPORT_ERROR)?
                    .post(endpoint)
                } else if let Some(base) = &self.fixture {
                    self.client.post(format!(
                        "{base}-refresh/prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
                    ))
                } else {
                    self.client.post(endpoint)
                };
                (
                    request.json(&json!({"refreshToken":grant.refresh})),
                    json!({"authMethod":"imported","provider":"Imported"}),
                    SOCIAL_IMPORT_ERROR,
                )
            }
        };
        let response = request
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| error)?;
        if !response.status().is_success() {
            return Err(error);
        }
        let tokens: Value = response.json().await.map_err(|_| error)?;
        let access = nonempty(&tokens["accessToken"])
            .filter(|token| token.bytes().all(|byte| byte.is_ascii_graphic()))
            .ok_or(error)?;
        let refresh = nonempty(&tokens["refreshToken"]).unwrap_or(&grant.refresh);
        let profile = grant
            .profile
            .as_deref()
            .or_else(|| nonempty(&tokens["profileArn"]).filter(|arn| profile_arn(arn)));
        let ttl = tokens["expiresIn"]
            .as_i64()
            .filter(|duration| *duration > 0)
            .unwrap_or(3600);
        details["profileArn"] = json!(profile);
        let mut fields = credentials(access, "oauth", ttl, details)?;
        fields["refreshToken"] = json!(refresh);
        save(&self.storage, fields).await
    }
}

struct Grant {
    identity: Option<IdentityCenter>,
    refresh: String,
    profile: Option<String>,
}

struct IdentityCenter {
    client: String,
    secret: String,
    region: String,
}

impl Grant {
    fn from_request(payload: &Value, refresh: &str) -> Result<Self, Failure> {
        let client = &payload["clientId"];
        let secret = &payload["clientSecret"];
        let identity = if connections::truthy(client) && connections::truthy(secret) {
            let invalid = || Failure::bad("Invalid AWS IAM Identity Center client credentials");
            Some(IdentityCenter {
                client: nonempty(client).ok_or_else(invalid)?.into(),
                secret: nonempty(secret).ok_or_else(invalid)?.into(),
                region: region(&payload["region"]).ok_or(BAD_REGION)?.into(),
            })
        } else {
            None
        };
        let profile = &payload["profileArn"];
        let profile = if connections::truthy(profile) {
            Some(
                nonempty(profile)
                    .filter(|arn| profile_arn(arn))
                    .ok_or_else(|| Failure::bad("Invalid Kiro profile ARN"))?
                    .to_owned(),
            )
        } else {
            None
        };
        Ok(Self {
            identity,
            refresh: refresh.into(),
            profile,
        })
    }

    fn key(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        for field in std::iter::once(&self.refresh).chain(
            self.identity
                .iter()
                .flat_map(|identity| [&identity.region, &identity.client, &identity.secret]),
        ) {
            hash.update(field.len().to_le_bytes());
            hash.update(field.as_bytes());
        }
        hash.finalize().into()
    }
}

fn credentials(access: &str, auth: &str, seconds: i64, details: Value) -> Result<Value, Failure> {
    let expires = TimeDelta::try_seconds(seconds)
        .and_then(|ttl| Utc::now().checked_add_signed(ttl))
        .ok_or(IMPORT_ERROR)?;
    let claims = crate::provider_oauth::token_claims(access);
    let email = ["email", "preferred_username", "sub"]
        .into_iter()
        .find_map(|field| nonempty(&claims[field]));
    Ok(json!({
        "authType":auth,"accessToken":access,"email":email,
        "expiresAt":expires.to_rfc3339_opts(SecondsFormat::Millis, true),
        "providerSpecificData":details,"testStatus":"active",
    }))
}

async fn save(storage: &Storage, mut fields: Value) -> ResultBody {
    let account = storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let now = connections::now();
            let existing = if fields["authType"] == "oauth" && fields["email"].is_string() {
                connections::read_all(&tx, Some("kiro"))?
                    .into_iter()
                    .find(|account| same_grant(account, &fields))
            } else {
                None
            };
            let account = if let Some(existing) = existing {
                connections::patch(existing, connections::object(fields), &now)
            } else {
                let (count, priority) = tx.query_row(
                    "SELECT COUNT(*), COALESCE(MAX(priority),0) + 1
                 FROM providerConnections WHERE provider = 'kiro'",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )?;
                if fields["authType"] == "oauth" {
                    fields["name"] = fields["email"].as_str().map_or_else(
                        || json!(format!("Account {}", count + 1)),
                        |email| json!(email),
                    );
                }
                fields["id"] = json!(Uuid::new_v4().to_string());
                fields["provider"] = json!("kiro");
                fields["priority"] = json!(priority);
                fields["isActive"] = json!(true);
                fields["createdAt"] = json!(now);
                fields["updatedAt"] = json!(now);
                fields
            };
            connections::write(&tx, &account)?;
            tx.commit()?;
            Ok(account)
        })
        .await
        .map_err(|_| SAVE_ERROR)?;
    Ok(json!({
        "success":true,
        "connection":{"id":account["id"],"provider":"kiro","email":account["email"]},
    }))
}

fn same_grant(a: &Value, b: &Value) -> bool {
    a["authType"] == "oauth"
        && a["email"] == b["email"]
        && ["authMethod", "clientId", "region", "profileArn"]
            .iter()
            .all(|key| a["providerSpecificData"][*key] == b["providerSpecificData"][*key])
}

fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(js_trim)
        .filter(|value| !value.is_empty())
}

fn region(value: &Value) -> Option<&str> {
    if !connections::truthy(value) {
        return Some(kiro_models::DEFAULT_REGION);
    }
    value
        .as_str()
        .filter(|region| kiro_models::valid_region(region))
}

fn profile_arn(arn: &str) -> bool {
    let Some((region, resource)) = arn
        .strip_prefix("arn:aws:codewhisperer:")
        .and_then(|value| value.split_once(':'))
    else {
        return false;
    };
    let Some((account, profile)) = resource.split_once(":profile/") else {
        return false;
    };
    kiro_models::valid_region(region)
        && account.len() == 12
        && account.bytes().all(|digit| digit.is_ascii_digit())
        && !profile.is_empty()
        && profile
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn unauthorized() -> Response {
    reply(Err(Failure(StatusCode::UNAUTHORIZED, "Unauthorized")))
}

fn reply(result: ResultBody) -> Response {
    let (status, body) = match result {
        Ok(body) => (StatusCode::OK, body),
        Err(Failure(status, error)) => (status, json!({"error":error})),
    };
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::VARY, "Accept-Encoding"),
        ],
        axum::Json(body),
    )
        .into_response()
}
