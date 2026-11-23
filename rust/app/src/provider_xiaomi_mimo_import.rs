use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use chrono::{SecondsFormat, TimeDelta, Utc};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

use super::commandcode::local_authorized;
use super::{body, error, metadata_bytes, metadata_client, nonempty, reply};
use crate::catalog_guard::is_direct_loopback;
use crate::chat_output::js_text;
use crate::provider_connections::{self as accounts, object, truthy};
use crate::provider_index::js_trim;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const PROVIDER: &str = "xiaomi-mimo";
const BASE: &str = "https://api.xiaomimimo.com/v1";

pub(super) async fn read_local(State(storage): State<Storage>, request: Request) -> Response {
    if !local_authorized(&storage, &request) {
        return reply(
            StatusCode::FORBIDDEN,
            not_found("Local only: CLI token required"),
            true,
        );
    }
    let include_session = is_direct_loopback(&request);
    match tokio::task::spawn_blocking(move || local_credentials(include_session)).await {
        Ok(Ok(details)) => reply(StatusCode::OK, details, true),
        _ => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            not_found("Failed to read Xiaomi MiMo credentials"),
            true,
        ),
    }
}

fn not_found(reason: &str) -> Value {
    json!({"found":false,"error":reason})
}

fn local_credentials(include_session: bool) -> Result<Value, ()> {
    let home = std::env::home_dir().ok_or(())?;
    let mut paths = vec![home.join(".local/share/mimocode/auth.json")];
    match std::env::consts::OS {
        "macos" => paths.push(home.join("Library/Application Support/mimocode/auth.json")),
        "windows" => {
            let appdata = std::env::var_os("APPDATA")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Roaming"));
            paths.push(appdata.join("Xiaomi MiMo/auth.json"));
        }
        _ => (),
    }
    let file = paths.iter().find_map(|path| {
        fs::metadata(path).ok()?.is_file().then_some(())?;
        Some((path, fs::File::open(path).ok()?))
    });
    let Some((path, file)) = file else {
        let checked = paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(not_found(&format!(
            "Xiaomi MiMo Desktop auth file not found. Checked:\n{checked}\n\nMake sure Xiaomi MiMo Desktop is installed and you are signed in."
        )));
    };
    const LIMIT: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > LIMIT {
        return Err(());
    }
    let Ok(auth) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(not_found(
            "auth.json is not valid JSON. Please sign in to Xiaomi MiMo Desktop again.",
        ));
    };
    let data = &auth["xiaomi"];
    if !truthy(&data["key"]) {
        return Ok(not_found(
            "No Xiaomi credentials found in auth.json. Please sign in to Xiaomi MiMo Desktop.",
        ));
    }
    let key = js_text(&data["key"]);
    let key = js_trim(&key);
    if !key.starts_with("sk-") {
        return Ok(not_found(
            "Xiaomi key does not appear to be a valid API key (expected sk- prefix).",
        ));
    }
    let meta = &data["metadata"];
    let session = include_session
        .then(|| desktop_session(&home, &meta["uid"]))
        .flatten();
    Ok(json!({
        "found":true,"apiKey":key,
        "uid":meta.get("uid").filter(|value| truthy(value)),
        "baseUrl":nonempty(&meta["base_url"]).unwrap_or(BASE),
        "source":path,
        "mimoPassToken":session.as_ref().map(|session| &session.token),
        "mimoUserId":session.as_ref().map(|session| &session.user),
        "mimoCUserId":session.as_ref().and_then(|session| session.client_user.as_ref()),
    }))
}

struct DesktopSession {
    token: String,
    user: String,
    client_user: Option<String>,
}

fn desktop_session(home: &Path, uid: &Value) -> Option<DesktopSession> {
    let expected = match uid {
        Value::String(value) => value.to_owned(),
        Value::Number(value) if value.is_u64() => value.to_string(),
        _ => return None,
    };
    if expected.len() > 128 || !crate::mimo_account::valid_cookie(&expected) {
        return None;
    }
    let directory = match std::env::consts::OS {
        "macos" => "Library/Application Support/Xiaomi MiMo",
        "windows" => "AppData/Roaming/Xiaomi MiMo",
        _ => ".config/Xiaomi MiMo",
    };
    let file = home
        .join(directory)
        .join("Partitions/xiaomi-account/Network/Cookies");
    if !file.symlink_metadata().is_ok_and(|meta| meta.is_file()) {
        return None;
    }
    let database = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    database.busy_timeout(Duration::ZERO).ok()?;
    let mut query = database
        .prepare(
            "SELECT DISTINCT name, value FROM cookies
         WHERE host_key = ?1 AND name IN ('passToken', 'userId', 'cUserId')
         AND length(value) BETWEEN 1 AND 8192 LIMIT 4",
        )
        .ok()?;
    let mut rows = query.query([".account.xiaomi.com"]).ok()?;
    let (mut token, mut user, mut client_user) = (None, None, None);
    while let Some(row) = rows.next().ok()? {
        let destination = match row.get_ref(0).ok()?.as_str().ok()? {
            "passToken" => &mut token,
            "userId" => &mut user,
            "cUserId" => &mut client_user,
            _ => return None,
        };
        let value: String = row.get(1).ok()?;
        if destination.is_some() || !crate::mimo_account::valid_cookie(&value) {
            return None;
        }
        *destination = Some(value);
    }
    Some(DesktopSession {
        token: token?,
        user: user.filter(|user| user == &expected)?,
        client_user,
    })
}

pub(super) async fn import(State(storage): State<Storage>, request: Request) -> Response {
    let input = match body(&storage, request).await {
        Ok(input) => input,
        Err((status, message)) => return error(status, message),
    };
    let key = input["apiKey"]
        .as_str()
        .map(js_trim)
        .filter(|key| !key.is_empty());
    let session = nonempty(&input["mimoPassToken"]).is_some();
    if key.is_none() && !session {
        return error(StatusCode::BAD_REQUEST, "API key is required");
    }
    if key.is_some_and(|key| !key.starts_with("sk-")) {
        return error(
            StatusCode::BAD_REQUEST,
            "Invalid key format — expected sk- prefix",
        );
    }
    let base = nonempty(&input["baseUrl"])
        .unwrap_or(BASE)
        .trim_end_matches('/');
    if !Url::parse(base).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    }) {
        return error(StatusCode::BAD_REQUEST, "Invalid MiMo base URL");
    }
    let count = match key {
        Some(key) => model_count(&storage, base, key).await,
        None => None,
    };
    let record = Import {
        key: key.map(str::to_owned),
        base: base.to_owned(),
        model_count: count,
        provider_data: object(input),
        authorized: false,
    };
    match storage.run(true, move |db| record.save(db)).await {
        Ok(value) => reply(StatusCode::OK, value, true),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "API key import failed"),
    }
}

async fn model_count(storage: &Storage, base: &str, key: &str) -> Option<usize> {
    let endpoint = format!("{base}/models");
    let settings = storage.read_settings().await?;
    let route = proxy_transport::select(storage, &settings, &Value::Null, &endpoint)
        .await
        .ok()?;
    let client = metadata_client(&route, Duration::from_secs(10))?;
    let url = match &route {
        Route::Relay { url, .. } => url,
        _ => &endpoint,
    };
    let mut response = route
        .relay(
            client
                .get(url)
                .bearer_auth(key)
                .header("X-Mimo-Source", "mimocode-cli"),
        )
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload: Value = serde_json::from_slice(&metadata_bytes(&mut response).await?).ok()?;
    Some(payload["data"].as_array().map_or(0, Vec::len))
}

struct Import {
    key: Option<String>,
    base: String,
    model_count: Option<usize>,
    provider_data: serde_json::Map<String, Value>,
    authorized: bool,
}

pub(crate) async fn save_authorized(
    storage: &Storage,
    key: String,
    uid: Value,
    base: String,
    include_session: bool,
) -> Result<Value, ()> {
    let provider_data = tokio::task::spawn_blocking(move || {
        let session = if include_session {
            std::env::home_dir().and_then(|home| desktop_session(&home, &uid))
        } else {
            None
        };
        object(json!({
            "uid":uid,
            "mimoPassToken":session.as_ref().map(|session| &session.token),
            "mimoUserId":session.as_ref().map(|session| &session.user),
            "mimoCUserId":session.and_then(|session| session.client_user),
        }))
    })
    .await
    .map_err(|_| ())?;
    let record = Import {
        key: Some(key),
        base,
        model_count: None,
        provider_data,
        authorized: true,
    };
    storage
        .run(true, move |db| record.save(db))
        .await
        .map_err(|_| ())
}

pub(crate) async fn save_web_session(
    storage: &Storage,
    region: &str,
    token: &str,
    user: &str,
    client_user: Option<&str>,
) -> Result<Value, ()> {
    let record = Import {
        key: None,
        base: BASE.to_owned(),
        model_count: None,
        provider_data: object(json!({
            "region":region,"uid":user,"mimoPassToken":token,"mimoUserId":user,"mimoCUserId":client_user,
        })),
        authorized: false,
    };
    storage
        .run(true, move |db| record.save(db))
        .await
        .map_err(|_| ())
}

impl Import {
    fn save(self, db: &mut rusqlite::Connection) -> rusqlite::Result<Value> {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pool = accounts::read_all(&tx, Some(PROVIDER))?;
        let request = Value::Object(self.provider_data);
        let email = truthy(&request["uid"]).then(|| format!("{}@xiaomi", js_text(&request["uid"])));
        let region = nonempty(&request["region"]);
        let session_only = self.key.is_none();
        let existing = pool.iter().find(|account| {
            let data = &account["providerSpecificData"];
            email
                .as_deref()
                .is_some_and(|email| account["email"] == email)
                || self
                    .key
                    .as_deref()
                    .is_some_and(|key| account["accessToken"] == key)
                || session_only
                    && truthy(&request["mimoUserId"])
                    && data["mimoUserId"] == request["mimoUserId"]
                    && region
                        .is_none_or(|region| nonempty(&data["region"]).unwrap_or("cn") == region)
        });
        let previous = existing
            .map(|account| &account["providerSpecificData"])
            .unwrap_or(&Value::Null);
        let mut data = object(previous.clone());
        let base = if session_only {
            nonempty(&previous["baseUrl"]).unwrap_or(&self.base)
        } else {
            &self.base
        };
        let auth = if self.authorized {
            "oauth"
        } else if session_only {
            "session"
        } else {
            nonempty(&previous["authMethod"]).unwrap_or("api_key")
        };
        for (field, value) in [
            ("baseUrl", json!(base)),
            (
                "region",
                json!(
                    region
                        .or_else(|| nonempty(&previous["region"]))
                        .unwrap_or("cn")
                ),
            ),
            ("authMethod", json!(auth)),
            ("modelCount", json!(self.model_count.unwrap_or(0))),
        ] {
            data.insert(field.into(), value);
        }
        for field in ["uid", "mimoPassToken", "mimoUserId", "mimoCUserId"] {
            let value = if truthy(&request[field]) {
                &request[field]
            } else {
                &previous[field]
            };
            data.insert(field.into(), value.clone());
        }
        let now = Utc::now();
        let stamp = now.to_rfc3339_opts(SecondsFormat::Millis, true);
        let active = self.authorized || self.model_count.is_some();
        let account = if let Some(current) = existing {
            let mut patch = object(json!({
                "accessToken": self.key.as_deref().map_or_else(|| current["accessToken"].clone(), |key| json!(key)),
                "providerSpecificData":data,
            }));
            if active {
                patch.insert("testStatus".into(), json!("active"));
            }
            if self.authorized {
                patch.insert("authType".into(), json!("oauth"));
            }
            accounts::patch(current.clone(), patch, &stamp)
        } else {
            let priority = tx.query_row(
                "SELECT COALESCE(MAX(priority),0) + 1 FROM providerConnections WHERE provider=?1",
                [PROVIDER],
                |row| row.get::<_, i64>(0),
            )?;
            let name = if session_only {
                email
                    .clone()
                    .unwrap_or_else(|| format!("Account {}", pool.len() + 1))
            } else {
                String::new()
            };
            let display = if truthy(&request["uid"]) {
                let suffix = if session_only { " (Session)" } else { "" };
                format!("Xiaomi {}{suffix}", js_text(&request["uid"]))
            } else {
                "Xiaomi MiMo".to_owned()
            };
            data.insert(
                "provider".into(),
                json!(if self.authorized {
                    "OAuth"
                } else if session_only {
                    "Session Login"
                } else {
                    "API Key"
                }),
            );
            let mut account = json!({
                "id":Uuid::new_v4().to_string(),"provider":PROVIDER,
                "authType":if session_only || self.authorized { "oauth" } else { "api_key" },
                "priority":priority,"isActive":true,"createdAt":stamp,"updatedAt":stamp,
                "name":(!name.is_empty()).then_some(name),"email":email,"displayName":display,
                "expiresAt":(now + TimeDelta::days(365)).to_rfc3339_opts(SecondsFormat::Millis, true),
                "testStatus":if active || session_only { "active" } else { "untested" },
                "providerSpecificData":data,
            });
            if let Some(key) = &self.key {
                account["accessToken"] = json!(key);
            }
            account
        };
        accounts::write(&tx, &account)?;
        tx.commit()?;
        let mut result =
            json!({"success":true,"validated":active,"modelCount":self.model_count.unwrap_or(0)});
        if existing.is_some() {
            result["updated"] = json!(true);
        }
        result["connection"] = json!({
            "id":account["id"],"provider":PROVIDER,"email":account["email"],"displayName":account["displayName"],
        });
        Ok(result)
    }
}
