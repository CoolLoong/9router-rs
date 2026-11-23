use serde_json::{Value, json};

const SESSION_MASK: u64 = (1 << 48) - 1;
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

pub(super) const ENDPOINT: &str = "https://opencode.ai/zen/v1/systemone";
pub(super) const FIXTURE: &str = "/opencode/zen/v1/systemone";

pub(super) fn is_lane(provider: &str) -> bool {
    matches!(provider, "opencode" | "opencode-zen")
}

pub(super) async fn public_account(
    storage: &crate::storage::Storage,
    settings: &Value,
) -> Result<Value, ()> {
    let mut account =
        crate::proxy_transport::public_proxy_account(storage, settings, "opencode").await?;
    account["id"] = json!("");
    account["accessToken"] = json!("public");
    Ok(account)
}

pub(super) fn headers(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::RequestBuilder, getrandom::Error> {
    let millis = chrono::Utc::now().timestamp_millis() as u64;
    let prefix = !(millis.wrapping_shl(12).wrapping_add(1)) & SESSION_MASK;
    let mut random = [0u8; 14];
    getrandom::fill(&mut random)?;
    let suffix: String = random
        .into_iter()
        .map(|byte| char::from(ALPHABET[usize::from(byte) % ALPHABET.len()]))
        .collect();
    let session = format!("ses_{prefix:012x}{suffix}");
    Ok(request
        .header("x-opencode-session", session)
        .header("x-opencode-client", "desktop")
        .header("User-Agent", "opencode/1.18.31"))
}
