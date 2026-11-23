use std::collections::HashSet;

use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::provider_index::js_trim;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Client {
    Claude,
    DeepSeekTui,
    Other,
}

impl Client {
    pub(super) fn detect(headers: &HeaderMap, body: &Value) -> Self {
        let field = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
        };
        let agent = field("user-agent").to_ascii_lowercase();
        if body["userAgent"] == "antigravity"
            || agent.contains("githubcopilotchat")
            || field("openai-intent").eq_ignore_ascii_case("conversation-panel")
            || field("x-initiator").eq_ignore_ascii_case("user")
        {
            return Self::Other;
        }
        if agent.contains("claude-cli")
            || agent.contains("claude-code")
            || field("x-app").eq_ignore_ascii_case("cli")
        {
            return Self::Claude;
        }
        if agent.contains("deepseek-tui")
            && ![
                "gemini-cli",
                "codex-tui",
                "codex-cli",
                "codex_cli_rs",
                "codex desktop",
            ]
            .into_iter()
            .any(|marker| agent.contains(marker))
            && !field("originator")
                .to_ascii_lowercase()
                .starts_with("codex_")
        {
            Self::DeepSeekTui
        } else {
            Self::Other
        }
    }

    pub(super) fn dedupe_tools(self, body: &mut Value, model: &str) {
        let deepseek = is_deepseek(model) || body["model"].as_str().is_some_and(is_deepseek);
        if self != Self::Claude && !deepseek {
            return;
        }
        let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
            return;
        };
        let web_mcp = self == Self::Claude
            && tools.iter().filter_map(tool_name).any(|name| {
                matches!(
                    name,
                    "mcp__exa__web_search_exa"
                        | "mcp__exa__web_fetch_exa"
                        | "mcp__tavily__tavily_search"
                        | "mcp__tavily__tavily_extract"
                )
            });
        let browser_mcp = self == Self::Claude
            && tools
                .iter()
                .filter_map(tool_name)
                .any(|name| name.starts_with("mcp__browsermcp__"));
        let mut seen = HashSet::new();
        let mut stripped = HashSet::new();
        tools.retain(|tool| {
            let Some(name) = tool_name(tool) else {
                return true;
            };
            if (web_mcp && matches!(name, "WebSearch" | "WebFetch" | "mcp__workspace__web_fetch"))
                || (browser_mcp && name.starts_with("mcp__Claude_in_Chrome__"))
            {
                stripped.insert(name.to_owned());
                return false;
            }
            !deepseek || seen.insert(name.to_owned())
        });
        if !stripped.is_empty() {
            clean_references(body, &stripped);
        }
    }
}

fn tool_name(tool: &Value) -> Option<&str> {
    tool["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .or_else(|| {
            tool["function"]["name"]
                .as_str()
                .filter(|name| !name.is_empty())
        })
}

fn is_deepseek(model: &str) -> bool {
    let model = js_trim(model);
    let model = model
        .strip_suffix(')')
        .and_then(|base| base.rsplit_once('('))
        .filter(|(_, suffix)| !suffix.is_empty() && !suffix.contains(')'))
        .map_or(model, |(base, _)| js_trim(base));
    model
        .rsplit('/')
        .next()
        .and_then(|name| name.get(..9))
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("deepseek-"))
}

fn clean_references(body: &mut Value, stripped: &HashSet<String>) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content
            .iter_mut()
            .filter(|block| block["type"] == "tool_result")
        {
            let Some(parts) = block.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            let mut notes = Vec::new();
            parts.retain(|part| {
                if part["type"] == "tool_reference"
                    && let Some(name) = part["tool_name"]
                        .as_str()
                        .filter(|name| stripped.contains(*name))
                {
                    notes.push(format!("[Tool {name} is unavailable]"));
                    false
                } else {
                    true
                }
            });
            if parts.is_empty() && !notes.is_empty() {
                parts.push(json!({"type":"text","text":notes.join("\n")}));
            }
        }
    }
}
