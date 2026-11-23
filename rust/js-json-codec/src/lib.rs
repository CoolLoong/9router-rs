#![forbid(unsafe_code)]

use serde_bytes::ByteBuf;
use serde_json::value::RawValue;
use std::fmt::{self, Write};

pub fn parse_array_strings(source: &[u16]) -> Option<Vec<Vec<u16>>> {
    let source = json_source(source)?;
    let values = serde_json::from_str::<Vec<&RawValue>>(&source).ok()?;
    Some(
        values
            .iter()
            .filter_map(|value| decode_json_string(value.get()))
            .collect(),
    )
}

pub fn stringify_string_array<T: AsRef<[u16]>>(
    entries: impl IntoIterator<Item = T>,
) -> Result<String, fmt::Error> {
    let mut text = String::from("[");
    for (index, entry) in entries.into_iter().enumerate() {
        if index != 0 {
            text.push(',');
        }
        write_json_string(&mut text, entry.as_ref())?;
    }
    text.push(']');
    Ok(text)
}

fn json_source(units: &[u16]) -> Option<String> {
    let mut text = String::new();
    let mut escaped = false;
    for scalar in char::decode_utf16(units.iter().copied()) {
        match scalar {
            Ok(ch) => {
                text.push(ch);
                escaped = !escaped && ch == '\\';
            }
            Err(surrogate) if !escaped => {
                write!(text, "\\u{:04x}", surrogate.unpaired_surrogate()).ok()?;
            }
            Err(_) => return None,
        }
    }
    Some(text)
}

fn decode_json_string(raw: &str) -> Option<Vec<u16>> {
    if !raw.starts_with('"') {
        return None;
    }
    let bytes = serde_json::from_str::<ByteBuf>(raw).ok()?;
    let mut units = Vec::new();
    let mut start = 0;
    for (index, chunk) in bytes.windows(3).enumerate() {
        if let [0xed, second @ 0xa0..=0xbf, third @ 0x80..=0xbf] = chunk {
            units.extend(str::from_utf8(&bytes[start..index]).ok()?.encode_utf16());
            units.push(0xd000 | u16::from(second & 0x3f) << 6 | u16::from(third & 0x3f));
            start = index + 3;
        }
    }
    units.extend(str::from_utf8(&bytes[start..]).ok()?.encode_utf16());
    Some(units)
}

fn write_json_string(text: &mut String, units: &[u16]) -> fmt::Result {
    text.push('"');
    for scalar in char::decode_utf16(units.iter().copied()) {
        match scalar {
            Ok('"') => text.push_str("\\\""),
            Ok('\\') => text.push_str("\\\\"),
            Ok('\u{8}') => text.push_str("\\b"),
            Ok('\u{c}') => text.push_str("\\f"),
            Ok('\n') => text.push_str("\\n"),
            Ok('\r') => text.push_str("\\r"),
            Ok('\t') => text.push_str("\\t"),
            Ok(ch @ '\0'..='\u{1f}') => write!(text, "\\u{:04x}", u32::from(ch))?,
            Ok(ch) => text.push(ch),
            Err(surrogate) => write!(text, "\\u{:04x}", surrogate.unpaired_surrogate())?,
        }
    }
    text.push('"');
    Ok(())
}
