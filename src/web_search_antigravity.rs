use std::collections::HashMap;
use std::time::Instant;

use axum::http::StatusCode;
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use url::Url;

use super::super::{Failure, WebState};
use super::js_space;
use crate::antigravity_transport;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

const PROVIDER: &str = "antigravity";
const BODY_LIMIT: usize = 4 * 1024 * 1024;

pub(super) async fn request(
    state: &WebState,
    query: &str,
    body: &Value,
    settings: &Value,
    account: &Value,
) -> Result<Value, Failure> {
    let start = Instant::now();
    let model = antigravity_transport::search_model().ok_or_else(|| {
        Failure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Antigravity search model is unavailable",
        )
    })?;
    let protected = crate::provider_http::requires_protection(settings, account);
    let classify = |failure: Failure| {
        if protected {
            failure.terminal()
        } else {
            failure
        }
    };
    let (mut response, secrets) =
        antigravity_transport::search(&state.storage, settings, account, query, model)
            .await
            .map_err(|failure| classify(Failure::new(failure.status, failure.message)))?;
    let latency = start.elapsed().as_millis() as u64;
    let status = response.status();
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
    let mut parsed: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    crate::provider_tools::scrub_payload(&mut parsed, &secrets);
    let response = if truthy(&parsed["response"]) {
        &parsed["response"]
    } else {
        &parsed
    };
    let candidate = &response["candidates"][0];
    let answer = array(&candidate["content"]["parts"])
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<String>();
    let limit = body["max_results"]
        .as_f64()
        .filter(|limit| limit.is_finite() && *limit > 0.0)
        .map_or(10, |limit| limit.floor() as usize);
    let results = citations(&candidate["groundingMetadata"], &answer, limit);
    let tokens = response["usageMetadata"]
        .get("totalTokenCount")
        .filter(|value| truthy(value))
        .cloned()
        .unwrap_or_else(|| json!(0));
    Ok(json!({
        "provider":PROVIDER,"query":query,"results":results,
        "answer":{"source":PROVIDER,"text":answer,"model":model},
        "usage":{"queries_used":1,"search_cost_usd":0,"llm_tokens":tokens},
        "metrics":{"response_time_ms":start.elapsed().as_millis() as u64,"upstream_latency_ms":latency,"total_results_available":null},
        "errors":[],
    }))
}

struct Source<'a> {
    url: &'a str,
    title: &'a str,
    snippets: Vec<&'a str>,
    contexts: Vec<String>,
}

fn citations(grounding: &Value, answer: &str, limit: usize) -> Vec<Value> {
    let mut sources = Vec::new();
    let mut by_url = HashMap::new();
    let by_index: Vec<Option<usize>> = array(&grounding["groundingChunks"])
        .iter()
        .map(|chunk| {
            let web = &chunk["web"];
            let url = web["uri"]
                .as_str()
                .filter(|value| !value.is_empty())
                .or_else(|| web["url"].as_str())?;
            if !Url::parse(url).is_ok_and(|target| {
                matches!(target.scheme(), "http" | "https") && target.has_host()
            }) {
                return None;
            }
            let index = by_url.entry(url).or_insert_with(|| {
                let index = sources.len();
                sources.push(Source {
                    url,
                    title: web["title"].as_str().unwrap_or_default(),
                    snippets: Vec::new(),
                    contexts: Vec::new(),
                });
                index
            });
            Some(*index)
        })
        .collect();

    let text: Vec<_> = answer.encode_utf16().collect();
    for support in array(&grounding["groundingSupports"]) {
        let segment = &support["segment"];
        let grounded = segment["text"].as_str().unwrap_or_default();
        let expanded = expand_segment(&text, segment)
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| grounded.to_owned());
        for index in array(&support["groundingChunkIndices"]) {
            let Some(index) =
                index_value(index).and_then(|index| by_index.get(index).copied().flatten())
            else {
                continue;
            };
            let source = &mut sources[index];
            if !grounded.is_empty() && !source.snippets.contains(&grounded) {
                source.snippets.push(grounded);
            }
            if !expanded.is_empty() && !source.contexts.contains(&expanded) {
                source.contexts.push(expanded.clone());
            }
        }
    }

    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    sources.into_iter().take(limit).enumerate().map(|(index, source)| {
        let rank = index + 1;
        let snippets = source.snippets.join(" | ");
        let snippet = if js_trim(&snippets).is_empty() { source.title } else { js_trim(&snippets) };
        let contexts = source.contexts.join("\n\n");
        let content = if js_trim(&contexts).is_empty() { snippet } else { js_trim(&contexts) };
        json!({
            "title":source.title,"url":source.url,"snippet":snippet,"position":rank,"score":null,
            "published_at":null,"favicon_url":null,"content":(!content.is_empty()).then_some(content),
            "metadata":{},"citation":{"provider":PROVIDER,"retrieved_at":now,"rank":rank},"provider_raw":null,
        })
    }).collect()
}

fn expand_segment(text: &[u16], segment: &Value) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let start = index_value(&segment["startIndex"])?.saturating_sub(150);
    let end = index_value(&segment["endIndex"])?
        .saturating_add(250)
        .min(text.len());
    let window = String::from_utf16_lossy(text.get(start..end)?);
    let mut out = js_trim(&window).to_owned();
    if start > 0 {
        out = format!(
            "...{}",
            out.find(js_space).map_or("", |space| &out[space..])
        );
    }
    if end < text.len() {
        let retain = out.rfind(js_space).map_or(0, |space| {
            space + out[space..].chars().next().map_or(0, char::len_utf8)
        });
        out.truncate(retain);
        out.push_str("...");
    }
    Some(js_trim(&out).to_owned())
}

fn index_value(value: &Value) -> Option<usize> {
    let index = value.as_f64()?;
    (index >= 0.0 && index.fract() == 0.0).then_some(index as usize)
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}
