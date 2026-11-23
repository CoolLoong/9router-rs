use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use crate::chat_input::{js_space, truthy};
use crate::{chat_output, claude_sse::ClaudeSse};

const DEFAULT: &str = "CLI Command Execution: Clear Terminal";
const TITLE: &str = "Please write a 5-10 word title for the following conversation:";

pub fn respond(body: &Value, model: &str, agent: &str, naming: bool) -> Option<Response> {
    if !agent.contains("claude-cli")
        || body["contents"].is_array()
        || (truthy(&body["request"]["contents"]) && body["userAgent"] == "antigravity")
    {
        return None;
    }
    let messages = body["messages"].as_array()?;
    let first = messages.first()?;
    let last = messages.last()?;
    let user = || messages.iter().filter(|message| message["role"] == "user");
    let bypass = last["role"] == "assistant" && last["content"][0]["text"] == "{"
        || text(&first["content"]) == "Warmup"
        || (messages.len() == 1 && first["role"] == "user" && text(&first["content"]) == "count")
        || user()
            .map(|message| text(&message["content"]))
            .collect::<Vec<_>>()
            .join(" ")
            .contains(TITLE);
    let content = if bypass {
        DEFAULT.to_owned()
    } else if naming {
        let system = messages
            .iter()
            .find(|message| message["role"] == "system")
            .map(|message| text(&message["content"]))
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| text(&body["system"]));
        if !system.contains("isNewTopic") {
            return None;
        }
        let user = user()
            .next()
            .map(|message| text(&message["content"]))
            .unwrap_or_default();
        let title = user
            .split(js_space)
            .filter(|word| !word.is_empty())
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
        json!({"isNewTopic":true,"title":title}).to_string()
    } else {
        return None;
    };
    let millis = chrono::Utc::now().timestamp_millis();
    let id = format!("chatcmpl-{millis}");
    let created = millis.div_euclid(1000);
    let usage = json!({"prompt_tokens":1,"completion_tokens":1,"total_tokens":2});
    let claude = claude_format(body, first, model);
    let stream = body["stream"] != false;
    if !claude && !stream {
        return Some(chat_output::json_response(
            StatusCode::OK,
            json!({
                "id":id,"object":"chat.completion","created":created,"model":model,
                "choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],
                "usage":usage,
            }),
            None,
        ));
    }
    let start = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,
        "choices":[{"index":0,"delta":{"role":"assistant","content":content},"finish_reason":null}]});
    let stop = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":usage});
    if !claude {
        return Some(sse(format!(
            "data: {start}\n\ndata: {stop}\n\ndata: null\n\n"
        )));
    }
    let mut format = ClaudeSse::new();
    let mut frames = format.push(&start);
    frames.extend(format.push(&stop));
    frames.extend(format.finish());
    if stream {
        let mut output = String::new();
        for frame in frames {
            use std::fmt::Write;
            let event = frame["type"].as_str().unwrap_or_default();
            let _ = write!(output, "event: {event}\ndata: {frame}\n\n");
        }
        return Some(sse(output));
    }
    let mut message = frames.first()?["message"].clone();
    if let Some(final_usage) = frames.iter().find(|frame| frame["type"] == "message_delta") {
        message["usage"]["output_tokens"] = final_usage["usage"]["output_tokens"].clone();
    }
    Some(chat_output::json_response(StatusCode::OK, message, None))
}

fn sse(mut output: String) -> Response {
    output.push_str("data: [DONE]\n\n");
    chat_output::reply(StatusCode::OK, Body::from(output), true, None)
}

fn text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "text")
        .map(|item| {
            item.get("text")
                .filter(|value| !value.is_null())
                .map_or_else(String::new, chat_output::js_text)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn claude_format(body: &Value, first: &Value, model: &str) -> bool {
    if ["stream_options", "response_format", "logit_bias", "user"]
        .iter()
        .any(|key| truthy(&body[*key]))
        || [
            "logprobs",
            "top_logprobs",
            "n",
            "presence_penalty",
            "frequency_penalty",
        ]
        .iter()
        .any(|key| body.get(*key).is_some())
    {
        return false;
    }
    if !model.contains('/')
        && first["content"][0]["type"] == "text"
        && let Some(blocks) = first["content"].as_array()
    {
        if truthy(&body["system"])
            || truthy(&body["anthropic_version"])
            || blocks
                .iter()
                .any(|block| block["type"] == "image" && block["source"]["type"] == "base64")
        {
            return true;
        }
        if blocks
            .iter()
            .any(|block| block["type"] == "image_url" && truthy(&block["image_url"]["url"]))
        {
            return false;
        }
        if blocks
            .iter()
            .any(|block| matches!(block["type"].as_str(), Some("tool_use" | "tool_result")))
        {
            return true;
        }
    }
    body.get("system").is_some() || truthy(&body["anthropic_version"])
}
