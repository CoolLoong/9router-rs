#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatFormat {
    Chat,
    Messages,
    Responses,
}

impl ChatFormat {
    pub fn from_path(path: &str) -> Self {
        if path.ends_with("/responses")
            || path.ends_with("/responses/compact")
            || path == "/codex"
            || path.starts_with("/codex/")
        {
            Self::Responses
        } else if path.ends_with("/messages") {
            Self::Messages
        } else {
            Self::Chat
        }
    }
}

pub fn is_compact_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/responses/compact" | "/api/v1/responses/compact" | "/v1/v1/responses/compact"
    )
}
