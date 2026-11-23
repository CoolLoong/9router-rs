use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

use crate::provider_index::js_trim;

pub(crate) struct Account<'a> {
    pub(crate) token: &'a str,
    pub(crate) machine: &'a str,
    pub(crate) ghost: bool,
}

impl<'a> Account<'a> {
    pub(crate) fn from_connection(connection: &'a Value) -> Option<Self> {
        let token = connection["accessToken"].as_str().map(js_trim)?;
        let token = if token.contains("::") {
            token.split("::").nth(1)?
        } else {
            token
        };
        let details = &connection["providerSpecificData"];
        let machine = details["machineId"].as_str().map(js_trim)?;
        if token.is_empty()
            || token.len() > 16 * 1024
            || !token.bytes().all(|byte| byte.is_ascii_graphic())
            || !(32..=128).contains(&machine.len())
            || machine.bytes().filter(u8::is_ascii_hexdigit).count() < 32
            || !machine
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        {
            return None;
        }
        Some(Self {
            token,
            machine,
            ghost: details["ghostMode"] != false,
        })
    }

    pub(crate) fn checksum(&self) -> Option<String> {
        let timestamp =
            (SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() / 1000) as u32;
        let mut previous = 165_u8;
        let mut bytes = [8, 0, 24, 16, 8, 0].map(|shift| (timestamp >> shift) as u8);
        for (index, byte) in bytes.iter_mut().enumerate() {
            previous = (*byte ^ previous).wrapping_add(index as u8);
            *byte = previous;
        }
        Some(format!("{}{}", URL_SAFE_NO_PAD.encode(bytes), self.machine))
    }
}
