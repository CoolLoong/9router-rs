use serde_json::Value;
use url::Url;

pub const OPENAI_NODE_PATHS: &[&str] = &["/chat/completions", "/responses"];
pub const ANTHROPIC_NODE_PATHS: &[&str] = &["/messages"];
pub const EMBEDDING_NODE_PATHS: &[&str] = &["/embeddings"];

pub fn compatible_paths(kind: crate::provider_index::Compatible) -> &'static [&'static str] {
    use crate::provider_index::Compatible;
    match kind {
        Compatible::OpenAi => OPENAI_NODE_PATHS,
        Compatible::Anthropic => ANTHROPIC_NODE_PATHS,
        Compatible::Embedding => EMBEDDING_NODE_PATHS,
    }
}

pub fn connection_base<'a>(account: &'a Value, default: Option<&'a str>) -> Option<&'a str> {
    let saved = &account["providerSpecificData"]["baseUrl"];
    if !crate::provider_connections::truthy(saved) {
        return default;
    }
    saved
        .as_str()
        .map(crate::provider_index::js_trim)
        .filter(|base| !base.is_empty())
}

pub fn node_url(base: &str, path: &str, prior_paths: &[&str]) -> Option<Url> {
    let mut url = Url::parse(base).ok().filter(|url| {
        matches!(url.scheme(), "https" | "http")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })?;
    let base = url.path().trim_end_matches('/');
    let base = prior_paths
        .iter()
        .find_map(|suffix| base.strip_suffix(suffix))
        .unwrap_or(base);
    let base = base.strip_suffix(path).unwrap_or(base);
    url.set_path(&format!("{base}{path}"));
    Some(url)
}

pub fn anthropic_official(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str() == Some("api.anthropic.com") && url.port().is_none()
}

pub fn redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > 20 {
            return attempt.error("redirect limit exceeded");
        }
        if attempt
            .previous()
            .last()
            .is_some_and(|url| url.origin() == attempt.url().origin())
        {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}
