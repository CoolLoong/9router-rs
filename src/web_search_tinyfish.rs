use axum::http::{HeaderValue, StatusCode};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use url::Url;

use crate::chat_output::js_text;
use crate::provider_connections::truthy;

use super::{Adapter, Failure, WebState, domain_filter, result};

pub(super) fn offset(body: &Value) -> usize {
    body["offset"]
        .as_f64()
        .filter(|offset| offset.is_finite() && *offset > 0.0 && offset.fract() == 0.0)
        .map_or(0, |offset| offset as usize)
}

pub(super) fn build(
    state: &WebState,
    query: &str,
    body: &Value,
    token: &str,
    limit: f64,
) -> Result<reqwest::RequestBuilder, Failure> {
    let search_type = &body["search_type"];
    let kind = if truthy(search_type) {
        search_type.as_str().unwrap_or_default()
    } else {
        "web"
    };
    if !matches!(kind, "web" | "news" | "research_paper") {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "Unsupported TinyFish search type",
        ));
    }
    let (includes, excludes) = domain_filter(&body["domain_filter"])?;
    let offset = offset(body);
    if offset >= 110 {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "TinyFish search offset exceeds available pages",
        ));
    }
    if offset > 0 && (offset % 10) as f64 + limit > 10.0 {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "TinyFish search offset and max_results must fit within one page",
        ));
    }
    let mut url = Url::parse("https://api.search.tinyfish.ai/").expect("fixed TinyFish search URL");
    {
        let mut params = url.query_pairs_mut();
        params.append_pair("query", query);
        if kind != "web" {
            params.append_pair("domain_type", kind);
        }
        for (field, parameter) in [("country", "location"), ("language", "language")] {
            if truthy(&body[field]) {
                params.append_pair(parameter, &js_text(&body[field]));
            }
        }
        for (parameter, domains) in [("include_domains", includes), ("exclude_domains", excludes)] {
            if !domains.is_empty() {
                params.append_pair(parameter, &domains.join(","));
            }
        }
        if offset > 0 {
            params.append_pair("page", &(offset / 10).to_string());
        }
    }
    let mut key = HeaderValue::from_str(token).map_err(|_| {
        Failure::new(StatusCode::BAD_GATEWAY, "tinyfish error: fetch failed").terminal()
    })?;
    key.set_sensitive(true);
    Ok(state
        .protected
        .get(url)
        .header("Accept", "application/json")
        .header("X-API-Key", key))
}

pub(super) fn normalize(data: &Value) -> (Vec<Value>, Value) {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let results = data["results"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, item)| {
            let authors = item["authors"].as_array().map(|items| {
                items
                    .iter()
                    .map(|item| {
                        if item.is_null() {
                            String::new()
                        } else {
                            js_text(item)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            });
            result(
                Adapter::Tinyfish,
                index,
                &now,
                json!({
                    "title":item["title"],"url":item["url"],"snippet":item["snippet"],
                    "published_at":item["date"],"source_type":item.get("publisher").filter(|value| truthy(value)),
                    "author":authors,
                }),
            )
        })
        .collect();
    (results, data["total_results"].clone())
}
