use axum::http::{HeaderValue, StatusCode};
use chrono::{SecondsFormat, Utc};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::provider_connections::truthy;

use super::extra::{number_string, provider_setting};
use super::{Adapter, Failure, WebState, result, search_endpoint};

const URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

pub(super) async fn build(
    state: &WebState,
    query: &str,
    body: &Value,
    account: &Value,
    token: &str,
    limit: f64,
) -> Result<reqwest::RequestBuilder, Failure> {
    let query_type = provider_setting(body, account, "queryType");
    if query_type.is_some_and(|kind| !matches!(kind, "Latest" | "Top")) {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "Xquik queryType must be Latest or Top",
        ));
    }
    let mut url = search_endpoint(account, "https://xquik.com/api/v1/x/tweets/search", "").await?;
    {
        let mut params = url.query_pairs_mut();
        params
            .append_pair("q", query)
            .append_pair("limit", &number_string(limit));
        if let Some(cursor) = provider_setting(body, account, "cursor") {
            params.append_pair("cursor", cursor);
        }
        if let Some(kind) = query_type {
            params.append_pair("queryType", kind);
        }
        if truthy(&body["language"]) {
            params.append_pair("language", &js_text(&body["language"]));
        }
    }
    let mut key = HeaderValue::from_str(token).map_err(|_| {
        Failure::new(StatusCode::BAD_GATEWAY, "xquik error: fetch failed").terminal()
    })?;
    key.set_sensitive(true);
    Ok(state
        .protected
        .get(url)
        .header("Accept", "application/json")
        .header("x-api-key", key))
}

pub(super) fn normalize(data: &Value) -> Vec<Value> {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    data["tweets"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, item)| {
            let username = item["author"]["username"]
                .as_str()
                .filter(|name| !name.is_empty());
            let author = username.map(|name| format!("@{name}")).or_else(|| {
                item["author"]["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            });
            let title = match &author {
                Some(name) => format!("{name} on X"),
                None => "X post".to_owned(),
            };
            let id = item
                .get("id")
                .filter(|id| truthy(id))
                .map(js_text)
                .filter(|id| !id.is_empty());
            let url = match (username, id.as_deref()) {
                (Some(user), Some(id)) => format!(
                    "https://x.com/{}/status/{}",
                    utf8_percent_encode(user, URI_COMPONENT),
                    utf8_percent_encode(id, URI_COMPONENT)
                ),
                (None, Some(id)) => format!(
                    "https://x.com/i/web/status/{}",
                    utf8_percent_encode(id, URI_COMPONENT)
                ),
                (_, None) => String::new(),
            };
            let media = item["media"]
                .as_array()
                .and_then(|items| items.iter().find_map(|media| media["mediaUrl"].as_str()));
            let text = item["text"].as_str().unwrap_or_default();
            result(
                Adapter::Xquik,
                index,
                &now,
                json!({
                    "title":title,"url":url,"snippet":text,
                    "published_at":item["createdAt"].as_str(),"author":author,"image_url":media,
                    "source_type":"x_post","full_text":text,"text_format":"text",
                }),
            )
        })
        .collect()
}

pub(super) fn pagination(data: &Value) -> Value {
    json!({
        "has_more":data["has_next_page"] == true,
        "next_cursor":data["next_cursor"].as_str().filter(|cursor| !cursor.is_empty()),
    })
}
