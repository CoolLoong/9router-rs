use chrono::{DateTime, Utc};
use serde_json::Value;
use url::Url;

const DEFAULT: &str = "https://gitlab.com";

pub fn endpoint(account: &Value) -> Option<String> {
    let account_type = account["authType"].as_str()?;
    if !matches!(account_type, "oauth" | "access_token") {
        return None;
    }
    token(account)?;
    if let Some(expiry) = account["expiresAt"]
        .as_str()
        .filter(|expiry| !expiry.is_empty())
        && DateTime::parse_from_rfc3339(expiry)
            .ok()
            .is_none_or(|time| time.to_utc() <= Utc::now())
    {
        return None;
    }
    Some(format!("{}/api/v4/chat/completions", base(account)?))
}

pub fn user_endpoint(account: &Value) -> Option<String> {
    Some(format!("{}/api/v4/user", base(account)?))
}

fn base(account: &Value) -> Option<&str> {
    let stored = &account["providerSpecificData"]["baseUrl"];
    let base = if stored.is_null() {
        DEFAULT
    } else {
        stored.as_str()?
    };
    let base = if base.is_empty() {
        DEFAULT
    } else {
        base.trim_end_matches('/')
    };
    let url = Url::parse(base).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(base)
}

pub fn token(account: &Value) -> Option<&str> {
    account["accessToken"]
        .as_str()
        .filter(|token| !token.is_empty())
}

pub fn fixture_url(url: &str) -> String {
    if url == "https://gitlab.com/api/v4/chat/completions"
        && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
    {
        return "http://127.0.0.1:52989/gitlab-chat-fixture/api/v4/chat/completions".into();
    }
    url.to_owned()
}

pub fn redact_error(account: &Value, message: &str) -> String {
    token(account).map_or_else(
        || message.to_owned(),
        |token| message.replace(token, "[redacted]"),
    )
}
