use std::borrow::Cow;
use std::time::{Duration, Instant};

use axum::http::{HeaderValue, StatusCode};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use url::Url;

use crate::provider_connections::truthy;

use crate::provider_oauth::{self, RefreshError};
use crate::proxy_transport::{Route, Selection};

use super::{Failure, WebState, or_else, redact, upstream_token};
use crate::web_gateway::redact_payload;

const BODY_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(in crate::web_gateway) enum Adapter {
    Gemini,
    Perplexity,
    PerplexityAgent,
    OpenAi,
    Xai,
    Kimi,
    MiniMax,
}

impl Adapter {
    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::Perplexity => "perplexity",
            Self::PerplexityAgent => "perplexity-agent",
            Self::OpenAi => "openai",
            Self::Xai => "xai",
            Self::Kimi => "kimi",
            Self::MiniMax => "minimax",
        }
    }

    pub(super) async fn request(
        self,
        state: &WebState,
        query: &str,
        body: &Value,
        settings: &Value,
        account: &Value,
    ) -> Result<Value, Failure> {
        let start = Instant::now();
        let name = self.id();
        let (registry, fixture, label) = match self {
            Self::Gemini => (
                include_str!("../../../open-sse/providers/registry/gemini.js"),
                "/gemini-search",
                "Gemini",
            ),
            Self::Perplexity => (
                include_str!("../../../open-sse/providers/registry/perplexity.js"),
                "/perplexity-search",
                "Perplexity",
            ),
            Self::PerplexityAgent => (
                include_str!("../../../open-sse/providers/registry/perplexity-agent.js"),
                "/perplexity-agent-search",
                "Perplexity Agent",
            ),
            Self::OpenAi => (
                include_str!("../../../open-sse/providers/registry/openai.js"),
                "/openai-search",
                "OpenAI",
            ),
            Self::Xai => (
                include_str!("../../../open-sse/providers/registry/xai.js"),
                "/xai-search",
                "xAI",
            ),
            Self::Kimi => (
                include_str!("../../../open-sse/providers/registry/kimi.js"),
                "/kimi-search",
                "Kimi",
            ),
            Self::MiniMax => (
                include_str!("../../../open-sse/providers/registry/minimax.js"),
                "/minimax-search",
                "MiniMax",
            ),
        };
        let config = registry
            .split_once("searchViaChat: {")
            .map_or("", |(_, config)| config);
        let setting = |key: &str| {
            config
                .lines()
                .map(str::trim)
                .take_while(|line| !line.starts_with('}'))
                .find_map(|line| {
                    line.strip_prefix(key)?
                        .split_once('"')
                        .map(|(value, _)| value)
                })
        };
        let endpoint = if matches!(self, Self::OpenAi) {
            Some("https://api.openai.com/v1/responses")
        } else {
            setting("endpoint: \"")
        };
        let (Some(model), Some(endpoint)) = (setting("defaultModel: \""), endpoint) else {
            return Err(Failure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{label} search configuration is unavailable"),
            ));
        };
        if matches!(self, Self::Kimi)
            && !matches!(account["authType"].as_str(), Some("apikey" | "api_key"))
        {
            return Err(Failure::new(
                StatusCode::UNAUTHORIZED,
                "Kimi web search requires a Moonshot API key",
            ));
        }
        let endpoint = endpoint.replace("{model}", model);
        let payload = match self {
            Self::Gemini => {
                json!({"contents":[{"role":"user","parts":[{"text":query}]}],"tools":[{"google_search":{}}]})
            }
            Self::Perplexity => json!({"model":model,"messages":[{"role":"user","content":query}]}),
            Self::PerplexityAgent => {
                json!({"model":model,"input":query,"tools":[{"type":"web_search"}]})
            }
            Self::OpenAi => {
                json!({"model":model,"input":query,"tools":[{"type":"web_search"}],"tool_choice":"required"})
            }
            Self::Xai => {
                json!({"model":model,"input":[{"role":"user","content":query}],"tools":[{"type":"web_search"}]})
            }
            Self::Kimi => {
                json!({"model":model,"messages":[{"role":"user","content":query}],"tools":[{"type":"builtin_function","function":{"name":"$web_search"}}]})
            }
            Self::MiniMax => {
                json!({"model":model,"messages":[{"role":"user","content":query}],"tools":[{"type":"web_search"}]})
            }
        };
        let xai_oauth = matches!(self, Self::Xai) && account["authType"] == "oauth";
        let route_error = || {
            Failure::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "xAI OAuth outbound route is unavailable",
            )
            .terminal()
        };
        let expected = if xai_oauth {
            let route = crate::provider_http::select(&state.storage, settings, account, &endpoint)
                .await
                .map_err(|_| route_error())?;
            if matches!(route, Selection::Routed(Route::MissingStrict)) {
                return Err(route_error());
            }
            Some(route)
        } else {
            None
        };
        let renewable = xai_oauth && upstream_token(account, &["refreshToken"]).is_some();
        let mut account = Cow::Borrowed(account);
        let mut renewed = false;
        if renewable
            && let Some(route) = &expected
            && (upstream_token(&account, &["accessToken"]).is_none()
                || provider_oauth::refresh_due_for_provider("xai", &account))
        {
            match provider_oauth::refresh_xai_inference(&state.storage, &account, &endpoint, route)
                .await
            {
                Ok(updated) => {
                    account = Cow::Owned(updated);
                    renewed = true;
                }
                Err(RefreshError::Unsupported | RefreshError::Storage) => return Err(route_error()),
                Err(_) => (),
            }
        }
        let (mut response, delivery, latency) = loop {
            let fields: &[&str] = if xai_oauth {
                &["accessToken"]
            } else if matches!(self, Self::Kimi) {
                &["apiKey"]
            } else {
                &["apiKey", "accessToken"]
            };
            let token = upstream_token(&account, fields).ok_or_else(|| {
                Failure::new(
                    StatusCode::UNAUTHORIZED,
                    "Missing credentials (apiKey or accessToken)",
                )
            })?;
            let mut request = state
                .protected
                .post(&endpoint)
                .json(&payload)
                .timeout(Duration::from_secs(15));
            if matches!(self, Self::Gemini) {
                let mut key = HeaderValue::from_str(token).map_err(|_| {
                    Failure::new(StatusCode::BAD_GATEWAY, "Network error: fetch failed")
                })?;
                key.set_sensitive(true);
                request = request.header("x-goog-api-key", key);
            } else {
                request = request.bearer_auth(token);
            }
            let current_settings;
            let settings = if expected.is_some() {
                current_settings = state
                    .storage
                    .read_settings()
                    .await
                    .ok_or_else(route_error)?;
                &current_settings
            } else {
                settings
            };
            let (request, delivery) = state
                .route_request(request, fixture, settings, &account, expected.as_ref())
                .await?;
            let upstream_start = Instant::now();
            let response = request.send().await.map_err(|error| {
                let failure = if error.is_timeout() {
                    Failure::new(StatusCode::GATEWAY_TIMEOUT, "Upstream timeout")
                } else {
                    Failure::new(StatusCode::BAD_GATEWAY, "Network error: fetch failed")
                };
                delivery.failure(failure)
            })?;
            let latency = upstream_start.elapsed().as_millis() as u64;
            if renewable
                && !renewed
                && matches!(
                    response.status(),
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                )
                && let Some(route) = &expected
                && let Ok(updated) = provider_oauth::refresh_xai_inference(
                    &state.storage,
                    &account,
                    &endpoint,
                    route,
                )
                .await
                && updated["accessToken"] != account["accessToken"]
            {
                account = Cow::Owned(updated);
                renewed = true;
                continue;
            }
            break (response, delivery, latency);
        };
        let classify = |failure| delivery.failure(failure);
        let status = response.status();
        if status.is_redirection() {
            return Err(classify(Failure::new(
                StatusCode::BAD_GATEWAY,
                format!("{label} search returned an unexpected redirect"),
            )));
        }
        let invalid = || {
            classify(Failure::new(
                StatusCode::BAD_GATEWAY,
                format!("Invalid upstream response (status {})", status.as_u16()),
            ))
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| invalid())? {
            if chunk.len() > BODY_LIMIT - bytes.len() {
                return Err(invalid());
            }
            bytes.extend_from_slice(&chunk);
        }
        let data: Value =
            serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes))
                .map_err(|_| invalid())?;
        if !status.is_success() {
            let fallback = json!(format!("Upstream HTTP {}", status.as_u16()));
            let message = or_else(
                &data["error"]["message"],
                or_else(&data["error"], or_else(&data["message"], &fallback)),
            );
            return Err(classify(Failure::new(
                status,
                redact(
                    &message
                        .as_str()
                        .map_or_else(|| message.to_string(), str::to_owned),
                    &account,
                ),
            )));
        }

        let limit = body["max_results"]
            .as_f64()
            .filter(|limit| limit.is_finite() && *limit > 0.0)
            .map_or(10, |limit| limit.floor() as usize);
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let tool_results = match self {
            Self::Kimi => tool_citations(&data, &["search_results", "results", "references"]),
            Self::MiniMax
                if !data["web_search_results"].as_array().is_some_and(|items| {
                    items.iter().any(|item| search_citation(item).is_some())
                }) =>
            {
                tool_citations(&data, &["results", "search_results"])
            }
            _ => Vec::new(),
        };
        let (answer, citations, tokens) = match self {
            Self::Gemini => {
                let candidate = &data["candidates"][0];
                let answer = candidate["content"]["parts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<String>();
                let citations = candidate["groundingMetadata"]["groundingChunks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|chunk| {
                        let web = &chunk["web"];
                        Citation::new(
                            or_else(&web["uri"], &web["url"]).as_str()?,
                            web["title"].as_str().unwrap_or_default(),
                        )
                    })
                    .take(limit)
                    .collect::<Vec<_>>();
                (
                    json!(answer),
                    citations,
                    &data["usageMetadata"]["totalTokenCount"],
                )
            }
            Self::Perplexity => {
                let answer = or_else(&data["choices"][0]["message"]["content"], &json!("")).clone();
                let citations = data["citations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Citation::from_value)
                    .take(limit)
                    .collect::<Vec<_>>();
                (answer, citations, &data["usage"]["total_tokens"])
            }
            Self::PerplexityAgent => {
                let (answer, citations) = responses_answer(&data, limit, true);
                (json!(answer), citations, &data["usage"]["total_tokens"])
            }
            Self::OpenAi | Self::Xai => {
                let (answer, citations) = responses_answer(&data, limit, false);
                (json!(answer), citations, &data["usage"]["total_tokens"])
            }
            Self::Kimi | Self::MiniMax => {
                let answer = or_else(&data["choices"][0]["message"]["content"], &json!("")).clone();
                let direct = data["web_search_results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(search_citation)
                    .take(limit)
                    .collect::<Vec<_>>();
                let citations = if matches!(self, Self::MiniMax) && !direct.is_empty() {
                    direct
                } else {
                    tool_results
                        .iter()
                        .filter_map(search_citation)
                        .take(limit)
                        .collect()
                };
                (answer, citations, &data["usage"]["total_tokens"])
            }
        };
        let results: Vec<Value> = citations.into_iter().enumerate().map(|(index, citation)| {
        let rank = index + 1;
        json!({
            "title":citation.title,"url":citation.url,
            "snippet":or_else(citation.snippet,&json!("")),"position":rank,"score":null,
            "published_at":null,"favicon_url":null,"content":or_else(citation.content,&Value::Null),"metadata":{},
            "citation":{"provider":name,"retrieved_at":now,"rank":rank},"provider_raw":null,
        })
    }).collect();
        let tokens = if truthy(tokens) {
            tokens.clone()
        } else {
            json!(0)
        };
        let mut output = json!({
            "provider":name,"query":query,"results":results,
            "answer":{"source":name,"text":answer,"model":model},
            "usage":{"queries_used":1,"search_cost_usd":0,"llm_tokens":tokens},
            "metrics":{"response_time_ms":start.elapsed().as_millis() as u64,"upstream_latency_ms":latency,"total_results_available":null},
            "errors":[],
        });
        redact_payload(&mut output, &account);
        Ok(output)
    }
}

fn responses_answer(data: &Value, limit: usize, tool_results: bool) -> (String, Vec<Citation<'_>>) {
    let mut answer = String::new();
    let mut citations = Vec::new();
    for item in data["output"].as_array().into_iter().flatten() {
        for part in item["content"].as_array().into_iter().flatten() {
            if let Some(text) = part["text"].as_str() {
                answer.push_str(text);
            }
            citations.extend(
                part["annotations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|annotation| {
                        let value = if truthy(&annotation["url"]) {
                            annotation
                        } else {
                            &annotation["url_citation"]
                        };
                        Citation::from_value(value)
                    })
                    .take(limit - citations.len()),
            );
        }
        if tool_results {
            citations.extend(
                item["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| {
                        let mut citation = Citation::new(
                            or_else(&value["url"], &value["link"]).as_str()?,
                            value["title"].as_str().unwrap_or_default(),
                        )?;
                        citation.snippet = &value["snippet"];
                        Some(citation)
                    })
                    .take(limit - citations.len()),
            );
        }
    }
    if citations.is_empty() {
        citations.extend(
            data["citations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Citation::from_value)
                .take(limit),
        );
    }
    (answer, citations)
}

fn tool_citations(data: &Value, fields: &[&str]) -> Vec<Value> {
    let mut results = Vec::new();
    for call in data["choices"][0]["message"]["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let arguments = &call["function"]["arguments"];
        let parsed;
        let arguments = if let Some(text) = arguments.as_str() {
            let Ok(value) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            parsed = value;
            &parsed
        } else {
            arguments
        };
        if let Some(items) = fields.iter().find_map(|field| arguments[*field].as_array()) {
            results.extend(items.iter().cloned());
        }
    }
    results
}

fn search_citation(result: &Value) -> Option<Citation<'_>> {
    let mut item = Citation::new(
        or_else(&result["url"], &result["link"]).as_str()?,
        result["title"].as_str().unwrap_or_default(),
    )?;
    item.snippet = or_else(&result["snippet"], &result["summary"]);
    Some(item)
}

struct Citation<'a> {
    url: &'a str,
    title: &'a str,
    snippet: &'a Value,
    content: &'a Value,
}

impl<'a> Citation<'a> {
    fn from_value(value: &'a Value) -> Option<Self> {
        let mut citation = Self::new(
            value.as_str().or_else(|| value["url"].as_str())?,
            value["title"].as_str().unwrap_or_default(),
        )?;
        citation.snippet = &value["snippet"];
        citation.content = &value["content"];
        Some(citation)
    }

    fn new(url: &'a str, title: &'a str) -> Option<Self> {
        let valid = Url::parse(url)
            .is_ok_and(|target| matches!(target.scheme(), "http" | "https") && target.has_host());
        valid.then_some(Self {
            url,
            title,
            snippet: &Value::Null,
            content: &Value::Null,
        })
    }
}
