use axum::http::{HeaderValue, StatusCode};
use chrono::{Datelike, Days, Months, SecondsFormat, Utc};
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

use super::{
    Adapter as SearchAdapter, Failure, WebState, domain_filter, or_else, result, search_endpoint,
    set_domains,
};

#[derive(Clone, Copy)]
pub(in crate::web_gateway) enum Adapter {
    YouCom,
    Glm,
    GooglePse,
    Linkup,
    OllamaSearch,
    SearchApi,
}

impl Adapter {
    pub(super) fn resolve(provider: &str) -> Option<Self> {
        match provider {
            "youcom" => Some(Self::YouCom),
            "glm" => Some(Self::Glm),
            "google-pse" => Some(Self::GooglePse),
            "linkup" => Some(Self::Linkup),
            "ollama-search" => Some(Self::OllamaSearch),
            "searchapi" => Some(Self::SearchApi),
            _ => None,
        }
    }

    pub(super) fn id(self) -> &'static str {
        match self {
            Self::YouCom => "youcom",
            Self::Glm => "glm",
            Self::GooglePse => "google-pse",
            Self::Linkup => "linkup",
            Self::OllamaSearch => "ollama-search",
            Self::SearchApi => "searchapi",
        }
    }

    pub(super) fn cost(self) -> f64 {
        match self {
            Self::Glm | Self::OllamaSearch => 0.0,
            Self::SearchApi => 0.004,
            _ => 0.005,
        }
    }

    pub(super) fn max_results(self) -> f64 {
        match self {
            Self::GooglePse | Self::OllamaSearch => 10.0,
            Self::Glm | Self::Linkup => 50.0,
            _ => 100.0,
        }
    }

    pub(super) async fn build(
        self,
        state: &WebState,
        query: &str,
        body: &Value,
        account: &Value,
        token: &str,
        limit: f64,
    ) -> Result<reqwest::RequestBuilder, Failure> {
        let endpoint = match self {
            Self::YouCom => "https://ydc-index.io/v1/search",
            Self::Glm => {
                return Ok(state
                    .protected
                    .post(
                        search_endpoint(
                            account,
                            "https://api.z.ai/api/mcp/web_search_prime/mcp",
                            "",
                        )
                        .await?,
                    )
                    .bearer_auth(token)
                    .json(&json!({
                        "jsonrpc":"2.0",
                        "id":format!("9r-{}",Utc::now().timestamp_millis()),
                        "method":"tools/call",
                        "params":{
                            "name":"web_search_prime",
                            "arguments":{"search_query":query,"count":limit},
                        },
                    })));
            }
            Self::GooglePse => "https://www.googleapis.com/customsearch/v1",
            Self::Linkup => {
                let (include, exclude) = domain_filter(&body["domain_filter"])?;
                let depth = provider_setting(body, account, "depth")
                    .filter(|depth| matches!(*depth, "fast" | "standard" | "deep"))
                    .unwrap_or("standard");
                let mut payload = json!({
                    "q":query,"depth":depth,"outputType":"searchResults","maxResults":limit,
                });
                set_domains(
                    &mut payload,
                    "includeDomains",
                    "excludeDomains",
                    include,
                    exclude,
                );
                let range = &body["time_range"];
                if truthy(range) && range != "any" {
                    let today = Utc::now().date_naive();
                    let from = match range.as_str() {
                        Some("day") => today - Days::new(1),
                        Some("week") => today - Days::new(7),
                        Some("month" | "year") => {
                            let months = if range == "year" { 12 } else { 1 };
                            let first = today.with_day(1).expect("first day of month");
                            first - Months::new(months) + Days::new(u64::from(today.day0()))
                        }
                        _ => today,
                    };
                    payload["fromDate"] = json!(from.to_string());
                    payload["toDate"] = json!(today.to_string());
                }
                return Ok(state
                    .protected
                    .post(search_endpoint(account, "https://api.linkup.so/v1/search", "").await?)
                    .bearer_auth(token)
                    .json(&payload));
            }
            Self::OllamaSearch => {
                let endpoint =
                    search_endpoint(account, "https://ollama.com/api/web_search", "").await?;
                let mut payload = json!({"query":query,"max_results":limit});
                for field in ["country", "language"] {
                    if truthy(&body[field]) {
                        payload[field] = body[field].clone();
                    }
                }
                return Ok(state
                    .protected
                    .post(endpoint)
                    .bearer_auth(token)
                    .json(&payload));
            }
            Self::SearchApi => "https://www.searchapi.io/api/v1/search",
        };
        let mut url = search_endpoint(account, endpoint, "").await?;
        let mut params = url.query_pairs_mut();
        match self {
            Self::YouCom => {
                let (include, exclude) = domain_filter(&body["domain_filter"])?;
                params
                    .append_pair("query", query)
                    .append_pair("count", &number_string(limit));
                if truthy(&body["time_range"]) && body["time_range"] != "any" {
                    params.append_pair("freshness", &js_text(&body["time_range"]));
                }
                if let Some(offset) = offset(body).filter(|_| limit > 0.0) {
                    params.append_pair("offset", &number_string((offset / limit).floor().min(9.0)));
                }
                for field in ["country", "language"] {
                    if truthy(&body[field]) {
                        params.append_pair(field, &js_text(&body[field]));
                    }
                }
                for (field, domains) in [("include_domains", include), ("exclude_domains", exclude)]
                {
                    if !domains.is_empty() {
                        params.append_pair(field, &domains.join(","));
                    }
                }
                let content = &body["content_options"];
                if truthy(&content["full_page"]) {
                    let section = if body["search_type"] == "news" {
                        "news"
                    } else {
                        "web"
                    };
                    let format = if content["format"] == "markdown" {
                        "markdown"
                    } else {
                        "html"
                    };
                    params
                        .append_pair("livecrawl", section)
                        .append_pair("livecrawl_formats", format);
                }
            }
            Self::GooglePse => {
                let cx = provider_setting(body, account, "cx").ok_or_else(|| {
                    Failure::new(
                        StatusCode::BAD_REQUEST,
                        "Google Programmable Search requires both apiKey and cx",
                    )
                })?;
                params
                    .append_pair("key", token)
                    .append_pair("cx", cx)
                    .append_pair("q", query)
                    .append_pair("num", &number_string(limit));
                locale(body, &mut params)?;
                if let Some(restrict) = match body["time_range"].as_str() {
                    Some("day") => Some("d1"),
                    Some("week") => Some("w1"),
                    Some("month") => Some("m1"),
                    Some("year") => Some("y1"),
                    _ => None,
                } {
                    params.append_pair("dateRestrict", restrict);
                }
                if let Some(offset) = offset(body) {
                    params.append_pair("start", &number_string((offset + 1.0).min(91.0)));
                }
            }
            Self::SearchApi => {
                let engine = if body["search_type"] == "news" {
                    "google_news"
                } else {
                    "google"
                };
                params
                    .append_pair("engine", engine)
                    .append_pair("q", query)
                    .append_pair("api_key", token);
                locale(body, &mut params)?;
                if let Some(offset) = offset(body).filter(|_| limit > 0.0) {
                    params.append_pair("page", &number_string((offset / limit).floor() + 1.0));
                }
            }
            Self::Glm | Self::Linkup | Self::OllamaSearch => {
                unreachable!("this provider uses a JSON request")
            }
        }
        drop(params);
        let mut request = state.client.get(url).header("Accept", "application/json");
        if matches!(self, Self::YouCom) {
            let token: String = token
                .chars()
                .filter(|character| *character <= '\u{ff}')
                .collect();
            let token: Vec<_> = js_trim(&token)
                .chars()
                .map(|character| character as u8)
                .collect();
            let value = HeaderValue::from_bytes(&token)
                .map_err(|_| Failure::new(StatusCode::BAD_GATEWAY, "youcom error: fetch failed"))?;
            request = request.header("X-API-Key", value);
        }
        Ok(request)
    }

    pub(super) fn normalize(self, body: &Value, input: &Value) -> (Vec<Value>, Value) {
        let section = or_else(&body["search_type"], &json!("web")).clone();
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let glm_payload = if matches!(self, Self::Glm) {
            input["result"]["content"][0]["text"]
                .as_str()
                .map(|text| serde_json::from_str(text).unwrap_or(Value::Null))
        } else {
            None
        };
        let input = glm_payload.as_ref().unwrap_or(input);
        let rows = match self {
            Self::YouCom => Some(&input["results"][if section == "news" { "news" } else { "web" }]),
            Self::Glm => [&input["results"], &input["news"], input]
                .into_iter()
                .find(|items| items.is_array()),
            Self::GooglePse => Some(&input["items"]),
            Self::Linkup => Some(&input["results"]),
            Self::OllamaSearch => [&input["results"], input]
                .into_iter()
                .find(|items| items.is_array()),
            Self::SearchApi => ["organic_results", "top_stories"]
                .iter()
                .find_map(|key| input.get(*key).filter(|items| items.is_array())),
        };
        let items = rows
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let total = match self {
            Self::YouCom | Self::Glm | Self::Linkup | Self::OllamaSearch => json!(items.len()),
            Self::GooglePse => {
                let total = &input["searchInformation"]["totalResults"];
                numeric_total(if total.is_null() {
                    &input["queries"]["request"][0]["totalResults"]
                } else {
                    total
                })
                .unwrap_or(Value::Null)
            }
            Self::SearchApi => numeric_total(&input["search_information"]["total_results"])
                .unwrap_or_else(|| json!(items.len())),
        };

        let rows = items.iter().enumerate().map(|(rank, item)| {
            let normalized = match self {
                Self::GooglePse => {
                    let image = or_else(
                        &item["pagemap"]["cse_image"][0]["src"],
                        or_else(&item["pagemap"]["cse_thumbnail"][0]["src"], &item["pagemap"]["metatags"][0]["og:image"]),
                    );
                    json!({"title":item["title"],"url":item["link"],"snippet":item["snippet"],"image_url":image})
                }
                Self::SearchApi => json!({
                    "title":item["title"],"url":item["link"],
                    "snippet":or_else(&item["snippet"],&item["description"]),
                    "published_at":or_else(&item["date"],&item["published_at"]),
                    "favicon_url":item["favicon"],"author":item["source"],"image_url":item["thumbnail"],
                }),
                Self::Linkup => json!({
                    "title":or_else(&item["name"],&item["title"]),"url":item["url"],
                    "snippet":or_else(&item["content"],&item["snippet"]),
                    "source_type":or_else(&item["type"],&json!("web")),
                    "image_url":or_else(&item["image_url"],&item["imageUrl"]),
                    "full_text":item["content"],"text_format":"text",
                }),
                Self::Glm => json!({
                    "title":item["title"],"url":or_else(&item["link"],&item["url"]),
                    "snippet":item["content"],"published_at":or_else(&item["publish_date"],&item["published_at"]),
                    "favicon_url":item["icon"],"source_type":item.get("media").filter(|value|truthy(value)),
                }),
                Self::OllamaSearch => json!({
                    "title":item["title"],"url":item["url"],
                    "snippet":or_else(&item["content"],&item["snippet"]),
                    "published_at":item["published_at"],"source_type":item.get("source").filter(|value|truthy(value)),
                    "full_text":item["content"],"text_format":"text",
                }),
                Self::YouCom => {
                    let excerpt = item["snippets"].as_array()
                        .and_then(|rows| rows.iter().find_map(Value::as_str))
                        .or_else(|| item["description"].as_str()).unwrap_or_default();
                    let markdown = item["markdown"].as_str();
                    let text = markdown.or_else(|| item["html"].as_str());
                    json!({
                        "title":item["title"],"url":item["url"],"snippet":excerpt,
                        "published_at":item["page_age"],"favicon_url":item["favicon_url"],
                        "image_url":item["thumbnail_url"],"source_type":section,
                        "full_text":text,"text_format":if markdown.is_some() { "markdown" } else { "html" },
                    })
                }
            };
            result(SearchAdapter::Extra(self), rank, &now, normalized)
        }).collect();
        (rows, total)
    }
}

fn offset(body: &Value) -> Option<f64> {
    body["offset"].as_f64().filter(|value| *value > 0.0)
}

fn locale(
    body: &Value,
    params: &mut url::form_urlencoded::Serializer<'_, url::UrlQuery<'_>>,
) -> Result<(), Failure> {
    if truthy(&body["country"]) {
        let country = body["country"].as_str().ok_or_else(|| {
            Failure::new(
                StatusCode::BAD_REQUEST,
                "b.country.toLowerCase is not a function",
            )
        })?;
        params.append_pair("gl", &country.to_lowercase());
    }
    if truthy(&body["language"]) {
        params.append_pair("hl", &js_text(&body["language"]));
    }
    Ok(())
}

pub(super) fn provider_setting<'a>(
    body: &'a Value,
    account: &'a Value,
    key: &str,
) -> Option<&'a str> {
    [
        &body["provider_options"][key],
        &account["providerSpecificData"][key],
    ]
    .into_iter()
    .filter_map(|value| value.as_str().map(js_trim))
    .find(|value| !value.is_empty())
}

pub(super) fn number_string(number: f64) -> String {
    if number.is_nan() {
        "NaN".into()
    } else if number == f64::INFINITY {
        "Infinity".into()
    } else if number == f64::NEG_INFINITY {
        "-Infinity".into()
    } else if number == 0.0 {
        "0".into()
    } else {
        number.to_string()
    }
}

fn numeric_total(value: &Value) -> Option<Value> {
    if value.is_number() {
        return Some(value.clone());
    }
    let text = js_trim(value.as_str()?);
    let prefix = text.get(..2).map(str::to_ascii_lowercase);
    let amount = match prefix.as_deref() {
        Some("0x") => u64::from_str_radix(&text[2..], 16).ok()? as f64,
        Some("0o") => u64::from_str_radix(&text[2..], 8).ok()? as f64,
        Some("0b") => u64::from_str_radix(&text[2..], 2).ok()? as f64,
        _ if text.is_empty() => 0.0,
        _ => text.parse::<f64>().ok()?,
    };
    if !amount.is_finite() {
        return None;
    }
    Some(if amount.fract() == 0.0 && amount.abs() < i64::MAX as f64 {
        json!(amount as i64)
    } else {
        json!(amount)
    })
}
