use serde_json::Value;

pub fn url(account: &Value, model: &str) -> String {
    let options = &account["providerSpecificData"];
    let endpoint = configured(options, "azureEndpoint", "AZURE_ENDPOINT")
        .unwrap_or_else(|| "https://api.openai.com".into());
    let endpoint = endpoint.strip_suffix('/').unwrap_or(&endpoint);
    let version = configured(options, "apiVersion", "AZURE_API_VERSION")
        .unwrap_or_else(|| "2024-10-01-preview".into());
    let deployment = value(options, "deployment")
        .or_else(|| (!model.is_empty()).then(|| model.to_owned()))
        .or_else(|| env("AZURE_DEPLOYMENT"))
        .unwrap_or_else(|| "gpt-4".into());
    format!("{endpoint}/openai/deployments/{deployment}/chat/completions?api-version={version}")
}

pub fn headers(mut request: reqwest::RequestBuilder, account: &Value) -> reqwest::RequestBuilder {
    if let Some(key) = credential(account) {
        request = request.header("api-key", key);
    }
    if let Some(org) = configured(
        &account["providerSpecificData"],
        "organization",
        "AZURE_ORGANIZATION",
    ) {
        request = request.header("OpenAI-Organization", org);
    }
    request
}

pub(crate) fn credential(account: &Value) -> Option<String> {
    value(account, "apiKey")
        .or_else(|| value(account, "accessToken"))
        .or_else(|| env("OPENAI_API_KEY"))
}

fn configured(options: &Value, field: &str, name: &str) -> Option<String> {
    value(options, field).or_else(|| env(name))
}

fn value(options: &Value, field: &str) -> Option<String> {
    options[field]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
