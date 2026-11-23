use std::time::{Duration, Instant};

use axum::http::{StatusCode, header};
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

use super::{Failure, WebState, fit_text, or_else, redact, redact_secrets, upstream_token};

#[derive(Clone, Copy)]
pub(super) enum Adapter {
    Firecrawl,
    Jina,
    Tavily,
    Exa,
    Ollama,
    Tinyfish,
}

impl Adapter {
    pub(super) fn resolve(provider: &str) -> Option<Self> {
        match provider {
            "firecrawl" => Some(Self::Firecrawl),
            "jina-reader" => Some(Self::Jina),
            "tavily" => Some(Self::Tavily),
            "exa" => Some(Self::Exa),
            "ollama" => Some(Self::Ollama),
            "tinyfish" => Some(Self::Tinyfish),
            _ => None,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Firecrawl => "firecrawl",
            Self::Jina => "jina-reader",
            Self::Tavily => "tavily",
            Self::Exa => "exa",
            Self::Ollama => "ollama",
            Self::Tinyfish => "tinyfish",
        }
    }

    fn config(
        self,
    ) -> (
        &'static str,
        &'static str,
        &'static [&'static str],
        u64,
        Option<f64>,
    ) {
        match self {
            Self::Firecrawl => (
                "https://api.firecrawl.dev/v1/scrape",
                "/firecrawl",
                &["markdown", "html", "text"],
                30,
                Some(0.002),
            ),
            Self::Jina => (
                "https://r.jina.ai/",
                "/jina",
                &["markdown", "text", "html"],
                30,
                Some(0.0),
            ),
            Self::Tavily => (
                "https://api.tavily.com/extract",
                "/tavily",
                &["markdown", "text"],
                15,
                Some(0.008),
            ),
            Self::Exa => (
                "https://api.exa.ai/contents",
                "/exa",
                &["text", "markdown"],
                15,
                Some(0.001),
            ),
            Self::Ollama => (
                "https://ollama.com/api/web_fetch",
                "/ollama",
                &["markdown"],
                30,
                None,
            ),
            Self::Tinyfish => (
                "https://api.fetch.tinyfish.ai",
                "/tinyfish",
                &["markdown", "html"],
                150,
                Some(0.0),
            ),
        }
    }
}

pub(super) async fn request(
    state: &WebState,
    adapter: Adapter,
    url: &str,
    body: &Value,
    settings: &Value,
    account: &Value,
) -> Result<Value, Failure> {
    let (endpoint, fixture, formats, timeout, cost) = adapter.config();
    let name = adapter.id();
    let format = if truthy(&body["format"]) {
        body["format"].as_str().unwrap_or("")
    } else {
        "markdown"
    };
    if !formats.contains(&format) {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            format!(
                "Unsupported format for {name}. Supported: {}",
                formats.join(", ")
            ),
        ));
    }
    let start = Instant::now();
    let token = upstream_token(account, &["apiKey", "key", "token"]);
    let mut request = state
        .client
        .post(endpoint)
        .timeout(Duration::from_secs(timeout));
    if let Some(token) = token {
        request = match adapter {
            Adapter::Exa | Adapter::Tinyfish => request.header("x-api-key", token),
            _ => request.bearer_auth(token),
        };
    } else if matches!(adapter, Adapter::Tinyfish) {
        request = request.header("x-api-key", "");
    }
    if matches!(adapter, Adapter::Jina) {
        request = request.header("x-respond-with", format);
    }
    let payload = match adapter {
        Adapter::Firecrawl => json!({"url":url,"formats":[format]}),
        Adapter::Jina | Adapter::Ollama => json!({"url":url}),
        Adapter::Tavily => json!({"urls":[url],"extract_depth":"basic","format":format}),
        Adapter::Exa => json!({"ids":[url],"text":true}),
        Adapter::Tinyfish => json!({"urls":[url],"format":format}),
    };
    let upstream_start = Instant::now();
    let request = request.json(&payload);
    let (request, delivery) = state
        .route_request(request, fixture, settings, account, None)
        .await?;
    let classify = |failure| delivery.failure(failure);
    let response = request.send().await.map_err(|error| {
        classify(if error.is_timeout() {
            Failure::new(StatusCode::GATEWAY_TIMEOUT, "This operation was aborted")
        } else {
            Failure::new(StatusCode::BAD_GATEWAY, "fetch failed")
        })
    })?;
    let upstream = upstream_start.elapsed().as_millis() as u64;
    let status = response.status();
    if delivery.protected && status.is_redirection() {
        return Err(Failure::new(
            StatusCode::BAD_GATEWAY,
            "Web provider proxy returned an unexpected redirect",
        )
        .terminal());
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/json"));
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) if delivery.protected => {
            return Err(Failure::new(
                StatusCode::BAD_GATEWAY,
                "Could not read the protected web response",
            )
            .terminal());
        }
        Err(_) => Default::default(),
    };
    let raw = String::from_utf8_lossy(&bytes);
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let parsed: Value = if is_json && !matches!(adapter, Adapter::Jina) {
        serde_json::from_str(raw).unwrap_or_default()
    } else {
        Value::Null
    };
    if !status.is_success() {
        return Err(classify(Failure::new(
            status,
            upstream_error(
                adapter,
                status,
                &parsed,
                &delivery.redact(raw),
                is_json,
                account,
            ),
        )));
    }
    let page = page(adapter, &parsed, raw).map_err(classify)?;
    let text = truncate(
        &redact_secrets(page.text.to_owned(), account),
        &body["max_characters"],
    );
    let mut result = json!({
        "provider":name,"url":url,"title":page.title,
        "content":{"format":format,"text":text,"length":text.encode_utf16().count()},
        "metadata":page.metadata,
        "usage":{"fetch_cost_usd":cost},
        "metrics":{"response_time_ms":start.elapsed().as_millis() as u64,"upstream_latency_ms":upstream},
    });
    if page.links.is_array() {
        result["links"] = page.links.clone();
    }
    Ok(result)
}

struct Page<'a> {
    title: Value,
    text: &'a str,
    links: &'a Value,
    metadata: Value,
}

fn page<'a>(adapter: Adapter, parsed: &'a Value, raw: &'a str) -> Result<Page<'a>, Failure> {
    let first = &parsed["results"][0];
    let (title, text, links) = match adapter {
        Adapter::Firecrawl => {
            let data = &parsed["data"];
            let text = or_else(&data["markdown"], or_else(&data["html"], &data["text"]));
            (
                data["metadata"]["title"].clone(),
                text.as_str().unwrap_or(""),
                &Value::Null,
            )
        }
        Adapter::Jina => (json!(jina_title(raw)), raw, &Value::Null),
        Adapter::Tavily => (
            Value::Null,
            first["raw_content"].as_str().unwrap_or(""),
            &Value::Null,
        ),
        Adapter::Exa => (
            first["title"].clone(),
            first["text"].as_str().unwrap_or(""),
            &Value::Null,
        ),
        Adapter::Ollama => {
            let Some(text) = parsed["content"].as_str() else {
                return Err(Failure::new(
                    StatusCode::BAD_GATEWAY,
                    "Ollama returned an empty or invalid web fetch response",
                ));
            };
            (parsed["title"].clone(), text, &parsed["links"])
        }
        Adapter::Tinyfish => {
            let failure = &parsed["errors"][0];
            if truthy(failure) {
                let status = failure["status"]
                    .as_u64()
                    .and_then(|status| u16::try_from(status).ok())
                    .and_then(|status| StatusCode::from_u16(status).ok())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                return Err(Failure::new(
                    status,
                    format!(
                        "TinyFish fetch failed: {}",
                        js_text(or_else(&failure["error"], &json!("unknown error")))
                    ),
                ));
            }
            let Some(text) = first["text"].as_str() else {
                return Err(Failure::new(
                    StatusCode::BAD_GATEWAY,
                    "TinyFish returned no extractable content",
                ));
            };
            (first["title"].clone(), text, &first["links"])
        }
    };
    let metadata = if matches!(adapter, Adapter::Tinyfish) {
        json!({"author": optional(first, "author"), "published_at": optional(first, "published_date"), "language": optional(first, "language")})
    } else {
        json!({"author":null,"published_at":null,"language":null})
    };
    Ok(Page {
        title: if truthy(&title) { title } else { Value::Null },
        text,
        links,
        metadata,
    })
}

fn optional<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    data.get(key).filter(|value| truthy(value))
}

fn upstream_error(
    adapter: Adapter,
    status: StatusCode,
    value: &Value,
    raw: &str,
    is_json: bool,
    account: &Value,
) -> String {
    let (name, message) = match adapter {
        Adapter::Firecrawl => ("Firecrawl", optional(value, "error")),
        Adapter::Jina => {
            if !raw.is_empty() {
                return fit_text(&redact(raw, account), 500);
            }
            ("Jina", None)
        }
        Adapter::Tavily => ("Tavily", optional(value, "error")),
        Adapter::Exa => ("Exa", optional(value, "error")),
        Adapter::Ollama => {
            let message = optional(value, "error").or_else(|| optional(value, "message"));
            if message.is_none() && !is_json && !raw.is_empty() {
                return fit_text(&redact(raw, account), 500);
            }
            ("Ollama", message)
        }
        Adapter::Tinyfish => ("TinyFish", optional(&value["error"], "message")),
    };
    message
        .map(js_text)
        .unwrap_or_else(|| format!("{name} error: {}", status.as_u16()))
}

fn jina_title(text: &str) -> Option<&str> {
    let title = text.lines().find_map(|line| {
        let line = js_trim(line);
        line.get(..6)
            .filter(|prefix| prefix.eq_ignore_ascii_case("title:"))?;
        Some(js_trim(&line[6..])).filter(|value| !value.is_empty())
    });
    title.or_else(|| {
        text.lines().find_map(|line| {
            let line = js_trim(line).strip_prefix('#')?;
            let title = js_trim(line);
            (title.len() < line.len() && !title.is_empty()).then_some(title)
        })
    })
}

fn truncate(text: &str, limit: &Value) -> String {
    let limit = if limit == true {
        Some(1.0)
    } else {
        limit
            .as_f64()
            .or_else(|| js_trim(&js_text(limit)).parse::<f64>().ok())
    };
    if let Some(limit) =
        limit.filter(|limit| *limit > 0.0 && (*limit as usize) < text.encode_utf16().count())
    {
        fit_text(text, limit as usize)
    } else {
        text.to_owned()
    }
}
