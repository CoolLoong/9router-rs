use std::time::Duration;

use provider_capabilities_rs::port::model_capabilities;
use reqwest::{RequestBuilder, Response};
use serde_json::Value;

use crate::anthropic_request;
use crate::anthropic_upstream::ClaudeToChat;
use crate::chat_format::ChatFormat;
use crate::chat_input::truthy;
use crate::chat_protocol_input;
use crate::claude_thinking;
use crate::thinking_intent::{model_suffix, thinking_intent};

pub fn matches_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude")
}

pub fn supported_body(body: &Value) -> bool {
    body.is_object()
        && !truthy(&body["input"])
        && !truthy(&body["contents"])
        && (!truthy(&body["messages"]) || body["messages"].is_array())
        && !anthropic_request::unsupported(body, false, true)
}

pub(super) fn to_chat(format: ChatFormat, body: Value, accept: &str) -> Option<Value> {
    if format == ChatFormat::Chat {
        return body["messages"].is_array().then_some(body);
    }
    let controls: Vec<_> = [
        "thinking",
        "reasoning_effort",
        "reasoning",
        "enable_thinking",
        "thinking_budget",
        "output_config",
        "thinkingConfig",
    ]
    .into_iter()
    .filter_map(|field| body.get(field).map(|value| (field, value.clone())))
    .collect();
    let mut converted = match format {
        ChatFormat::Chat => body,
        ChatFormat::Messages => {
            if anthropic_request::unsupported(&body, true, true) {
                return None;
            }
            chat_protocol_input::anthropic_messages_for_responses(body, accept)?
        }
        ChatFormat::Responses => {
            if truthy(&body["previous_response_id"])
                || truthy(&body["conversation"])
                || body["store"] == true
            {
                return None;
            }
            chat_protocol_input::responses_preview(body)?
        }
    };
    for (field, value) in controls {
        converted[field] = value;
    }
    supported_body(&converted).then_some(converted)
}

pub fn prepare(mut body: Value, model: &str) -> Option<Value> {
    if !supported_body(&body) {
        return None;
    }
    if body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .is_none_or(|message| !matches!(message["role"].as_str(), Some("user" | "tool")))
    {
        body.as_object_mut()?.shift_remove("thinking");
    }
    let prefilled = anthropic_request::last_source_assistant(&body);
    anthropic_request::preflight_messages(&mut body)?;
    let (model, override_intent) = model_suffix(model);
    let caps = model_capabilities(Some("github"), model, None);
    let mut translated = anthropic_request::from_openai(&body, model, true)?;
    claude_thinking::apply(
        &mut translated,
        "github",
        model,
        &caps,
        override_intent.or_else(|| thinking_intent(&body)),
        claude_thinking::capture_display(&body),
    );
    anthropic_request::prepare_native(&mut translated, &caps, prefilled, false)?;
    Some(translated)
}

pub fn url() -> &'static str {
    crate::github_executor::messages_url()
}

pub fn fixture_url() -> Option<String> {
    (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"))
        .then(|| "http://127.0.0.1:52989/github/v1/messages".into())
}

pub fn headers(request: RequestBuilder, account: &Value, streaming: bool) -> RequestBuilder {
    crate::github_executor::headers(request, account, &Value::Null, streaming)
}

pub fn adapt(response: Response, model: &str, streaming: bool, stall: Duration) -> Response {
    let mut translate = ClaudeToChat::new();
    translate.set_model(model);
    crate::github_responses::adapt_events(response, streaming, stall, move |event| {
        translate.push(None, event)
    })
}
