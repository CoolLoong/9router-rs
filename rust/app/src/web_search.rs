use std::time::{Duration, Instant};

use axum::http::StatusCode;
use chrono::{SecondsFormat, Utc};
use icu_normalizer::ComposingNormalizerBorrowed;
use serde_json::{Value, json};
use url::Url;

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

use super::{Failure, WebState, fit_text, or_else, redact, upstream_token};

#[path = "web_search_antigravity.rs"]
mod antigravity;
#[path = "web_search_extra.rs"]
mod extra;
#[path = "web_search_grounded.rs"]
mod grounded;
#[path = "web_search_tinyfish.rs"]
mod tinyfish;
#[path = "web_search_xquik.rs"]
mod xquik;

#[derive(Clone, Copy)]
pub(super) enum Adapter {
    Antigravity,
    Grounded(grounded::Adapter),
    Serper,
    Brave,
    Exa,
    Tavily,
    Tinyfish,
    Xquik,
    Searxng,
    Extra(extra::Adapter),
}

impl Adapter {
    pub(super) fn resolve(provider: &str) -> Option<Self> {
        match provider {
            "antigravity" => Some(Self::Antigravity),
            "gemini" => Some(Self::Grounded(grounded::Adapter::Gemini)),
            "perplexity" => Some(Self::Grounded(grounded::Adapter::Perplexity)),
            "perplexity-agent" => Some(Self::Grounded(grounded::Adapter::PerplexityAgent)),
            "openai" => Some(Self::Grounded(grounded::Adapter::OpenAi)),
            "xai" => Some(Self::Grounded(grounded::Adapter::Xai)),
            "kimi" => Some(Self::Grounded(grounded::Adapter::Kimi)),
            "minimax" => Some(Self::Grounded(grounded::Adapter::MiniMax)),
            "serper" => Some(Self::Serper),
            "brave-search" => Some(Self::Brave),
            "exa" => Some(Self::Exa),
            "tavily" => Some(Self::Tavily),
            "tinyfish" => Some(Self::Tinyfish),
            "xquik" => Some(Self::Xquik),
            "searxng" => Some(Self::Searxng),
            _ => extra::Adapter::resolve(provider).map(Self::Extra),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Antigravity => "antigravity",
            Self::Grounded(adapter) => adapter.id(),
            Self::Serper => "serper",
            Self::Brave => "brave-search",
            Self::Exa => "exa",
            Self::Tavily => "tavily",
            Self::Tinyfish => "tinyfish",
            Self::Xquik => "xquik",
            Self::Searxng => "searxng",
            Self::Extra(adapter) => adapter.id(),
        }
    }

    fn cost(self) -> Option<f64> {
        Some(match self {
            Self::Antigravity | Self::Grounded(_) | Self::Tinyfish | Self::Searxng => 0.0,
            Self::Serper => 0.001,
            Self::Brave => 0.005,
            Self::Exa => 0.007,
            Self::Tavily => 0.008,
            Self::Xquik => return None,
            Self::Extra(adapter) => adapter.cost(),
        })
    }

    fn max_results(self) -> f64 {
        match self {
            Self::Antigravity | Self::Grounded(_) | Self::Tinyfish => 10.0,
            Self::Brave | Self::Tavily => 20.0,
            Self::Serper | Self::Exa | Self::Xquik => 100.0,
            Self::Searxng => 50.0,
            Self::Extra(adapter) => adapter.max_results(),
        }
    }
}

async fn search_endpoint(account: &Value, default: &str, suffix: &str) -> Result<Url, Failure> {
    let Some(saved) = super::setting(&account["providerSpecificData"], "baseUrl") else {
        return Ok(Url::parse(&format!("{default}{suffix}")).expect("fixed search endpoint"));
    };
    let mut endpoint = Url::parse(saved.trim_end_matches('/'))
        .ok()
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && (suffix.is_empty() || url.query().is_none())
        })
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "Invalid saved search base URL"))?;
    if !suffix.is_empty() {
        let path = format!("{}{suffix}", endpoint.path().trim_end_matches('/'));
        endpoint.set_path(&path);
    }
    super::public_url(endpoint.as_str())
        .await
        .map_err(|message| Failure::new(StatusCode::BAD_REQUEST, message))?;
    Ok(endpoint)
}

pub(super) async fn request(
    state: &WebState,
    adapter: Adapter,
    query: &str,
    body: &Value,
    settings: &Value,
    account: &Value,
) -> Result<Value, Failure> {
    let query = clean_query(query)?;
    match adapter {
        Adapter::Antigravity => {
            return antigravity::request(state, &query, body, settings, account).await;
        }
        Adapter::Grounded(adapter) => {
            return adapter
                .request(state, &query, body, settings, account)
                .await;
        }
        _ => {}
    }
    let name = adapter.id();
    let token = if matches!(adapter, Adapter::Searxng) {
        Some("")
    } else {
        upstream_token(account, &["apiKey", "accessToken"])
    };
    let Some(token) = token else {
        return Err(Failure::new(
            StatusCode::UNAUTHORIZED,
            format!("No credentials for provider: {name}"),
        ));
    };
    let start = Instant::now();
    let limit = result_limit(&body["max_results"], adapter.max_results());
    let count = if limit.fract() == 0.0 && limit.abs() < i64::MAX as f64 {
        json!(limit as i64)
    } else {
        json!(limit)
    };
    let news = body["search_type"] == "news";
    let mut request = match adapter {
        Adapter::Antigravity | Adapter::Grounded(_) => unreachable!("grounded search"),
        Adapter::Extra(adapter) => {
            adapter
                .build(state, &query, body, account, token, limit)
                .await?
        }
        Adapter::Tinyfish => tinyfish::build(state, &query, body, token, limit)?,
        Adapter::Xquik => xquik::build(state, &query, body, account, token, limit).await?,
        Adapter::Searxng => state
            .client
            .get(searxng_url(&query, body, limit, news)?)
            .header(reqwest::header::ACCEPT, "application/json"),
        Adapter::Serper => {
            let mut payload = json!({"q":query,"num":count});
            if truthy(&body["country"]) {
                let country = body["country"].as_str().ok_or_else(|| {
                    Failure::new(
                        StatusCode::BAD_REQUEST,
                        "params.country.toLowerCase is not a function",
                    )
                })?;
                payload["gl"] = json!(country.to_lowercase());
            }
            if truthy(&body["language"]) {
                payload["hl"] = body["language"].clone();
            }
            let url = search_endpoint(
                account,
                "https://google.serper.dev",
                if news { "/news" } else { "/search" },
            )
            .await?;
            state
                .client
                .post(url)
                .header("X-API-Key", token)
                .json(&payload)
        }
        Adapter::Brave => {
            let mut url = search_endpoint(
                account,
                "https://api.search.brave.com/res/v1",
                if news { "/news/search" } else { "/web/search" },
            )
            .await?;
            let limit_param = if limit.is_nan() {
                "NaN".to_owned()
            } else if limit == f64::NEG_INFINITY {
                "-Infinity".to_owned()
            } else {
                count.to_string()
            };
            {
                let mut params = url.query_pairs_mut();
                params
                    .append_pair("q", &query)
                    .append_pair("count", &limit_param);
                for (field, parameter) in [("country", "country"), ("language", "search_lang")] {
                    if truthy(&body[field]) {
                        params.append_pair(parameter, &js_text(&body[field]));
                    }
                }
            }
            state
                .client
                .get(url)
                .header("Accept", "application/json")
                .header("X-Subscription-Token", token)
        }
        Adapter::Exa | Adapter::Tavily => {
            let (includes, excludes) = domain_filter(&body["domain_filter"])?;
            if matches!(adapter, Adapter::Exa) {
                let mut payload = json!({"query":query,"numResults":count,"type":"auto","text":true,"highlights":true});
                set_domains(
                    &mut payload,
                    "includeDomains",
                    "excludeDomains",
                    includes,
                    excludes,
                );
                if news {
                    payload["category"] = json!("news");
                }
                let url = search_endpoint(account, "https://api.exa.ai/search", "").await?;
                state
                    .client
                    .post(url)
                    .header("x-api-key", token)
                    .json(&payload)
            } else {
                let topic = if news { "news" } else { "general" };
                let mut payload = json!({"query":query,"max_results":count,"topic":topic});
                set_domains(
                    &mut payload,
                    "include_domains",
                    "exclude_domains",
                    includes,
                    excludes,
                );
                if truthy(&body["country"]) {
                    payload["country"] = body["country"].clone();
                }
                let url = search_endpoint(account, "https://api.tavily.com/search", "").await?;
                state.client.post(url).bearer_auth(token).json(&payload)
            }
        }
    };
    request = request.timeout(Duration::from_secs(10));
    let fixture = match adapter {
        Adapter::Antigravity | Adapter::Grounded(_) => unreachable!("grounded search"),
        Adapter::Serper => "/serper",
        Adapter::Brave => "/brave",
        Adapter::Exa => "/exa",
        Adapter::Tavily => "/tavily",
        Adapter::Tinyfish => "/tinyfish-search",
        Adapter::Xquik => "/xquik",
        Adapter::Searxng => "/searxng",
        Adapter::Extra(extra::Adapter::YouCom) => "/youcom",
        Adapter::Extra(extra::Adapter::Glm) => "/glm-search",
        Adapter::Extra(extra::Adapter::GooglePse) => "/google-pse",
        Adapter::Extra(extra::Adapter::Linkup) => "/linkup",
        Adapter::Extra(extra::Adapter::OllamaSearch) => "/ollama-search",
        Adapter::Extra(extra::Adapter::SearchApi) => "/searchapi",
    };
    let (request, delivery) = state
        .route_request(request, fixture, settings, account, None)
        .await?;
    let classify = |failure| delivery.failure(failure);
    let response = request.send().await.map_err(|error| {
        classify(if error.is_timeout() {
            Failure::new(
                StatusCode::GATEWAY_TIMEOUT,
                format!("{name} timeout: This operation was aborted"),
            )
        } else {
            Failure::new(
                StatusCode::BAD_GATEWAY,
                format!("{name} error: fetch failed"),
            )
        })
    })?;
    let status = response.status();
    if delivery.protected && status.is_redirection() {
        return Err(Failure::new(
            StatusCode::BAD_GATEWAY,
            "Web provider proxy returned an unexpected redirect",
        )
        .terminal());
    }
    let bytes = response.bytes().await.unwrap_or_default();
    let raw = String::from_utf8_lossy(&bytes);
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    if !status.is_success() {
        return Err(classify(Failure::new(
            status,
            format!(
                "{name} returned {}: {}",
                status.as_u16(),
                fit_text(&redact(&delivery.redact(raw), account), 200)
            ),
        )));
    }
    let data: Value = serde_json::from_str(raw).map_err(|error| {
        classify(Failure::new(
            StatusCode::BAD_GATEWAY,
            format!(
                "{name} error: {}",
                json_error(&redact(&delivery.redact(raw), account), &error)
            ),
        ))
    })?;
    let (mut results, total) = match adapter {
        Adapter::Extra(adapter) => adapter.normalize(body, &data),
        Adapter::Tinyfish => tinyfish::normalize(&data),
        Adapter::Xquik => (xquik::normalize(&data), Value::Null),
        _ => normalize(adapter, news, &data),
    };
    let offset = if matches!(adapter, Adapter::Tinyfish) {
        tinyfish::offset(body) % 10
    } else {
        0
    };
    let end = (offset as f64 + limit).trunc() as isize;
    let end = if end >= 0 {
        end as usize
    } else {
        results.len().saturating_sub(end.unsigned_abs())
    };
    results.truncate(end);
    results.drain(..offset.min(results.len()));
    let elapsed = start.elapsed().as_millis() as u64;
    let credits = results.len();
    let mut output = json!({
        "provider":name,"query":query,"results":results,"answer":null,
        "usage":{"queries_used":1,"search_cost_usd":adapter.cost()},
        "metrics":{"response_time_ms":elapsed,"upstream_latency_ms":elapsed,"total_results_available":total},
        "errors":[],
    });
    if matches!(adapter, Adapter::Xquik) {
        output["usage"]["provider_credits_used"] = json!(credits);
        output["pagination"] = xquik::pagination(&data);
    }
    Ok(output)
}

fn js_space(ch: char) -> bool {
    ch == '\u{feff}' || (ch.is_whitespace() && ch != '\u{85}')
}

fn clean_query(input: &str) -> Result<String, Failure> {
    if input
        .chars()
        .any(|ch| matches!(ch, '\0'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}'))
    {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "Query contains invalid control characters",
        ));
    }
    let clean = ComposingNormalizerBorrowed::new_nfkc()
        .normalize(input)
        .split(js_space)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if clean.is_empty() {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "Query is empty after normalization",
        ));
    }
    Ok(clean)
}

fn result_limit(input: &Value, max: f64) -> f64 {
    if !truthy(input) {
        return 5.0;
    }
    let limit = match input {
        Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
        Value::Bool(_) => 1.0,
        _ => {
            let text = js_text(input);
            let value = js_trim(&text);
            if value.is_empty() {
                0.0
            } else {
                value.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
    };
    if limit.is_nan() {
        limit
    } else {
        limit.min(max)
    }
}

fn searxng_url(query: &str, body: &Value, limit: f64, news: bool) -> Result<Url, Failure> {
    let configured = std::env::var("SEARXNG_URL").ok();
    let endpoint = configured
        .as_deref()
        .map(js_trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http://localhost:8888/search");
    let invalid =
        || Failure::new(StatusCode::BAD_REQUEST, "Invalid configured SearXNG URL").terminal();
    let mut url = Url::parse(endpoint).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    if !url.path().trim_end_matches('/').ends_with("/search") {
        url.path_segments_mut()
            .map_err(|_| invalid())?
            .pop_if_empty()
            .push("search");
    } else {
        url.path_segments_mut()
            .map_err(|_| invalid())?
            .pop_if_empty();
    }
    let mut params = url.query_pairs_mut();
    params
        .append_pair("q", query)
        .append_pair("format", "json")
        .append_pair("categories", if news { "news" } else { "general" });
    for field in ["language", "time_range"] {
        if let Some(value) = body[field]
            .as_str()
            .filter(|value| !value.is_empty() && (field != "time_range" || *value != "any"))
        {
            params.append_pair(field, value);
        }
    }
    if let Some(offset) = body["offset"]
        .as_f64()
        .filter(|offset| *offset > 0.0 && limit > 0.0)
    {
        params.append_pair("pageno", &((offset / limit).floor() + 1.0).to_string());
    }
    drop(params);
    Ok(url)
}

fn domain_filter(value: &Value) -> Result<(Vec<&str>, Vec<&str>), Failure> {
    let Some(values) = value.as_array() else {
        if value.as_str().is_some_and(|value| !value.is_empty()) {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "domainFilter.filter is not a function",
            ));
        }
        return Ok((Vec::new(), Vec::new()));
    };
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    for value in values {
        let Some(domain) = value.as_str() else {
            let message = if value.is_null() {
                "Cannot read properties of null (reading 'startsWith')"
            } else {
                "d.startsWith is not a function"
            };
            return Err(Failure::new(StatusCode::BAD_REQUEST, message));
        };
        if let Some(domain) = domain.strip_prefix('-') {
            excludes.push(domain);
        } else {
            includes.push(domain);
        }
    }
    Ok((includes, excludes))
}

fn set_domains(
    payload: &mut Value,
    include_key: &str,
    exclude_key: &str,
    includes: Vec<&str>,
    excludes: Vec<&str>,
) {
    if !includes.is_empty() {
        payload[include_key] = json!(includes);
    }
    if !excludes.is_empty() {
        payload[exclude_key] = json!(excludes);
    }
}

fn normalize(adapter: Adapter, news: bool, input: &Value) -> (Vec<Value>, Value) {
    let container = if news {
        or_else(&input["news"], input)
    } else {
        &input["web"]
    };
    let items = match adapter {
        Adapter::Serper => &input[if news { "news" } else { "organic" }],
        Adapter::Brave => &container["results"],
        Adapter::Exa | Adapter::Tavily | Adapter::Searxng => &input["results"],
        Adapter::Antigravity
        | Adapter::Grounded(_)
        | Adapter::Tinyfish
        | Adapter::Xquik
        | Adapter::Extra(_) => {
            unreachable!("specialized search result")
        }
    };
    let Some(items) = items.as_array() else {
        return (
            vec![],
            if matches!(adapter, Adapter::Searxng) {
                json!(0)
            } else {
                Value::Null
            },
        );
    };
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let total = match adapter {
        Adapter::Serper if input["searchParameters"]["totalResults"].is_number() => {
            input["searchParameters"]["totalResults"].clone()
        }
        Adapter::Serper => Value::Null,
        Adapter::Brave => container["totalCount"].clone(),
        Adapter::Exa | Adapter::Tavily | Adapter::Searxng => json!(items.len()),
        Adapter::Antigravity
        | Adapter::Grounded(_)
        | Adapter::Tinyfish
        | Adapter::Xquik
        | Adapter::Extra(_) => {
            unreachable!("specialized search total")
        }
    };
    let results = items.iter().enumerate().map(|(index, item)| match adapter {
        Adapter::Serper => result(adapter, index, &now, json!({
            "title":item["title"],"url":item["link"],"snippet":or_else(&item["snippet"],&item["description"]),"published_at":item["date"],
        })),
        Adapter::Brave => result(adapter, index, &now, json!({
            "title":item["title"],"url":item["url"],"snippet":item["description"],
            "published_at":or_else(&item["page_age"],&item["age"]),"favicon_url":or_else(&item["meta_url"]["favicon"],&item["favicon"]),
        })),
        Adapter::Exa => result(adapter, index, &now, json!({
            "title":item["title"],"url":item["url"],"snippet":or_else(&item["highlights"][0], &json!(item["text"].as_str().map(|text|fit_text(text, 300)))),
            "score":item["score"],"published_at":item["publishedDate"],"favicon_url":item["favicon"],"author":item["author"],"image_url":item["image"],"full_text":item["text"],
        })),
        Adapter::Tavily => result(adapter, index, &now, json!({
            "title":item["title"],"url":item["url"],"snippet":item["content"],"score":item["score"],"published_at":item["published_date"],"full_text":item["raw_content"],
        })),
        Adapter::Searxng => {
            let source = item["engines"].as_array().map(|engines| json!(engines.iter().map(js_text).collect::<Vec<_>>().join(", ")))
                .unwrap_or_else(|| or_else(&item["engine"], &item["category"]).clone());
            result(adapter,index,&now,json!({
                "title":item["title"],"url":item["url"],"snippet":or_else(&item["content"], &item["snippet"]),
                "published_at":or_else(&item["publishedDate"],&item["published_date"]),
                "source_type":source,"image_url":or_else(&item["thumbnail"], &item["img_src"]),
            }))
        }
        Adapter::Antigravity | Adapter::Grounded(_) | Adapter::Tinyfish | Adapter::Xquik | Adapter::Extra(_) => unreachable!("specialized search row"),
    }).collect();
    (results, total)
}

fn result(adapter: Adapter, index: usize, now: &str, item: Value) -> Value {
    let url = item["url"].as_str().unwrap_or("");
    let content = item["full_text"]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(|text| {
            json!({"format":item["text_format"].as_str().unwrap_or("text"),
            "text":text,"length":text.encode_utf16().count()})
        });
    let mut result = json!({
        "title":or_else(&item["title"], &json!("")),"url":url,"snippet":or_else(&item["snippet"], &json!("")),"position":index+1,
        "score":item["score"].as_f64().map(|score|score.clamp(0.0,1.0)),
        "published_at":item.get("published_at").filter(|value|truthy(value)),"favicon_url":item.get("favicon_url").filter(|value|truthy(value)),
        "content":content,"metadata":{"author":item.get("author").filter(|value|truthy(value)),"language":null,"source_type":item["source_type"],"image_url":item.get("image_url").filter(|value|truthy(value))},
        "citation":{"provider":adapter.id(),"retrieved_at":now,"rank":index+1},"provider_raw":null,
    });
    if !url.is_empty() {
        let shown = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .map(|host| host.strip_prefix("www.").unwrap_or(host))
            .unwrap_or(url);
        result
            .as_object_mut()
            .expect("search result object")
            .shift_insert(
                2,
                "display_url".into(),
                json!(shown.split('?').next().unwrap_or("")),
            );
    }
    result
}

fn json_error(raw: &str, error: &serde_json::Error) -> String {
    if error.is_eof() {
        "Unexpected end of JSON input".to_owned()
    } else if raw.trim_start().starts_with('<') {
        format!(
            "Unexpected token '<', \"{}\" is not valid JSON",
            fit_text(raw, 10)
        )
    } else {
        "Upstream returned invalid JSON".to_owned()
    }
}
