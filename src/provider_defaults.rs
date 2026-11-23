use std::sync::LazyLock;

use serde_json::Value;

pub(super) const USER_AGENT: &str = concat!("9Router/", env!("CARGO_PKG_VERSION"));

static DATA: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("provider-defaults.json"))
        .expect("built-in provider defaults")
});

pub(super) fn section(name: &str) -> &'static Value {
    &DATA[name]
}

pub(super) fn text(section_name: &str, key: &str) -> Option<&'static str> {
    section(section_name)[key].as_str()
}
