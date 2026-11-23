use std::collections::BTreeSet;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use rusqlite::TransactionBehavior;
use serde_json::{Map, Value, json};

use super::{ProviderState, connections, error, provider_index, respond, truthy};
use crate::provider_oauth::token_claims;

const FIELDS: &[&str] = &[
    "id",
    "provider",
    "authType",
    "name",
    "email",
    "displayName",
    "priority",
    "globalPriority",
    "isActive",
    "defaultModel",
    "testStatus",
    "lastError",
    "lastErrorAt",
    "errorCode",
    "expiresAt",
    "lastUsedAt",
    "consecutiveUseCount",
    "createdAt",
    "updatedAt",
];
const DETAILS: &[&str] = &[
    "baseUrl",
    "azureEndpoint",
    "deployment",
    "apiVersion",
    "accountId",
    "region",
    "projectId",
    "resourceUrl",
    "proxyPoolId",
    "connectionProxyEnabled",
    "connectionProxyUrl",
    "connectionNoProxy",
    "githubLogin",
    "githubName",
    "githubEmail",
    "githubUserId",
    "username",
    "firstName",
    "lastName",
    "authMethod",
    "authKind",
    "profileArn",
];

pub(super) async fn list(State(state): State<ProviderState>, request: Request) -> Response {
    let _ = state
        .backfilled
        .get_or_try_init(|| backfill(&state.storage))
        .await;
    let query = request.uri().query().unwrap_or_default();
    let pairs = url::form_urlencoded::parse(query.as_bytes());
    let value = |field: &str, default: &str| {
        pairs
            .clone()
            .find(|(name, _)| name == field)
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default.to_owned())
    };
    let provider_filter = value("provider", "all");
    let status = value("accountStatus", "all");
    let by_provider = value("sort", "priority") == "provider";
    let requested_page = positive(&value("page", ""), 1);
    let page_size = positive(&value("pageSize", ""), 20).min(500);
    let result = state.storage.run(false, move |db| {
        let mut eligible = connections::read_all(db, None)?.into_iter().filter_map(|connection| {
            let definition = provider_index::find(connection["provider"].as_str()?)?;
            let order = definition["usageOrder"].as_u64()?;
            (matches!(connection["authType"].as_str(), Some("oauth" | "mcode-lease"))
                || definition["usageApiKey"] == true).then_some((connection, order))
        }).collect::<Vec<_>>();
        let options: BTreeSet<_> = eligible.iter().filter_map(|(connection, _)| {
            connection["provider"].as_str().map(str::to_owned)
        }).collect();
        let eligible_count = eligible.len();
        eligible.retain(|(connection, _)| provider_filter == "all" || connection["provider"] == provider_filter);
        let for_provider = eligible.len();
        eligible.retain(|(connection, _)| match status.as_str() {
            "active" => connection["isActive"] != false,
            "inactive" => connection["isActive"] == false,
            _ => true,
        });
        eligible.sort_by(|(a, a_order), (b, b_order)| {
            let priority = |row: &Value| row["priority"].as_f64().unwrap_or(9_007_199_254_740_991.0);
            let order = if by_provider { a_order.cmp(b_order) } else { priority(a).total_cmp(&priority(b)) };
            order.then_with(|| a["provider"].as_str().cmp(&b["provider"].as_str()))
        });
        let total = eligible.len();
        let pages = total.div_ceil(page_size).max(1);
        let page = requested_page.min(pages);
        let connections = eligible.into_iter().skip((page - 1) * page_size).take(page_size)
            .map(|(connection, _)| sanitize(&connection)).collect::<Vec<_>>();
        Ok(json!({
            "connections":connections,"providerOptions":options,
            "pagination":{"page":page,"pageSize":page_size,"total":total,"totalPages":pages},
            "totals":{"eligibleConnections":eligible_count,"providerFilteredConnections":for_provider},
        }))
    }).await;
    match result {
        Ok(value) => respond(StatusCode::OK, value),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch providers",
        ),
    }
}

fn positive(text: &str, fallback: usize) -> usize {
    let text = text.trim_start_matches(crate::chat_input::js_space);
    text.strip_prefix('+')
        .unwrap_or(text)
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0usize, |value, digit| {
            value
                .saturating_mul(10)
                .saturating_add((digit - b'0') as usize)
        })
        .checked_sub(1)
        .map_or(fallback, |value| value + 1)
}

fn selected(connection: &Value, fields: &[&str]) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|&field| {
            connection
                .get(field)
                .map(|value| (field.into(), value.clone()))
        })
        .collect()
}

fn sanitize(connection: &Value) -> Value {
    let mut safe = selected(connection, FIELDS);
    if let Some(name) = safe.get_mut("name")
        && let Some(text) = name.as_str()
        && text
            .split(|character: char| {
                !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            })
            .any(|token| token.len() >= 32)
    {
        let start = String::from_utf16_lossy(&text.encode_utf16().take(8).collect::<Vec<_>>());
        *name = json!(format!("{start}***"));
    }
    if let Some(value) = safe.get_mut("lastError")
        && let Some(text) = value.as_str()
    {
        let mut text = crate::cli_video::redact_bearer(text);
        for field in ["apiKey", "accessToken", "refreshToken", "idToken"] {
            if let Some(secret) = connection[field].as_str().filter(|value| !value.is_empty()) {
                text = text.replace(secret, "[redacted]");
            }
        }
        *value = json!(text);
    }
    if truthy(&connection["providerSpecificData"]) {
        safe.insert(
            "providerSpecificData".into(),
            Value::Object(selected(&connection["providerSpecificData"], DETAILS)),
        );
    }
    Value::Object(safe)
}

async fn backfill(storage: &crate::storage::Storage) -> Result<(), String> {
    storage
        .run(true, |db| {
            let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for connection in connections::read_all(&transaction, Some("codex"))? {
                if connection["authType"] != "oauth"
                    || (truthy(&connection["email"])
                        && truthy(&connection["providerSpecificData"]["chatgptAccountId"]))
                {
                    continue;
                }
                let Some(token) = connection["idToken"]
                    .as_str()
                    .filter(|token| !token.is_empty())
                else {
                    continue;
                };
                let claims = token_claims(token);
                let auth = &claims["https://api.openai.com/auth"];
                let account = if truthy(&auth["chatgpt_account_id"]) {
                    &auth["chatgpt_account_id"]
                } else {
                    &claims["account_id"]
                };
                let plan = if truthy(&auth["chatgpt_plan_type"]) {
                    &auth["chatgpt_plan_type"]
                } else {
                    &claims["plan_type"]
                };
                let email = &claims["email"];
                if !truthy(email) && !truthy(account) {
                    continue;
                }
                let mut patch = Map::new();
                if !truthy(&connection["email"]) && truthy(email) {
                    patch.insert("email".into(), email.clone());
                }
                if truthy(account) || truthy(plan) {
                    let mut details =
                        connections::object(connection["providerSpecificData"].clone());
                    for (field, claim) in [("chatgptAccountId", account), ("chatgptPlanType", plan)]
                    {
                        if claim.is_null() {
                            details.shift_remove(field);
                        } else {
                            details.insert(field.into(), claim.clone());
                        }
                    }
                    patch.insert("providerSpecificData".into(), Value::Object(details));
                }
                if !patch.is_empty() {
                    connections::write(
                        &transaction,
                        &connections::patch(connection, patch, &connections::now()),
                    )?;
                }
            }
            transaction.commit()
        })
        .await
}
