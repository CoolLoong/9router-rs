use reqwest::{RequestBuilder, header};
use serde_json::Value;
use std::sync::LazyLock;

use crate::provider_http::Bound;
use crate::provider_oauth::RefreshMode;

static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let package: Value =
        serde_json::from_str(include_str!("../../../package.json")).unwrap_or_default();
    format!("9Router/{}", package["version"].as_str().unwrap_or("0.0.0"))
});

pub(super) fn identity_headers(
    request: RequestBuilder,
    token: &str,
    email: Option<&str>,
    user: Option<&str>,
) -> RequestBuilder {
    let mut request = request
        .bearer_auth(token)
        .header(header::USER_AGENT, USER_AGENT.as_str())
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("x-grok-client-mode", "headless");
    for (name, text) in [("x-email", email), ("x-userid", user)] {
        if let Some(mut value) = text
            .filter(|text| !text.is_empty() && text.len() <= 1024)
            .and_then(|text| header::HeaderValue::from_str(text).ok())
        {
            value.set_sensitive(true);
            request = request.header(name, value);
        }
    }
    request
}

pub(super) fn account_access(account: &Value) -> Option<&str> {
    account["accessToken"].as_str().filter(|token| {
        !token.is_empty()
            && token.len() <= 16 * 1024
            && token.bytes().all(|byte| byte.is_ascii_graphic())
    })
}

pub(super) fn refresh_mode(binding: &Bound) -> RefreshMode {
    if let Some(identity) = binding.route.relay_identity() {
        RefreshMode::RelayRouted(identity)
    } else if binding.protected {
        RefreshMode::GrokProtected
    } else {
        RefreshMode::Rejected
    }
}
