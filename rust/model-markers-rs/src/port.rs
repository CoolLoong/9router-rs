//! The Rust port of `open-sse/utils/modelMarkers.js`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

/// Port of `stripModelContextMarker` (`open-sse/utils/modelMarkers.js:14`).
pub fn strip_model_context_marker(
    model_str: serde_json::Value,
) -> Result<serde_json::Value, String> {
    if let Some(units) = crate::js::string_units(&model_str) {
        let trimmed = trim_js(&units);
        if let Some(model) = trimmed
            .strip_suffix(&[91, 49, 109, 93])
            .or_else(|| trimmed.strip_suffix(&[91, 49, 77, 93]))
        {
            return Ok(serde_json::json!({
                "model": crate::js::utf16_string(model),
                "contextMarker": "1m",
            }));
        }
    }
    Ok(serde_json::json!({ "model": model_str, "contextMarker": null }))
}

fn trim_js(units: &[u16]) -> &[u16] {
    let whitespace = |unit: &u16| {
        matches!(
            unit,
            0x0009..=0x000D | 0x0020 | 0x00A0 | 0x1680 | 0x2000..=0x200A
                | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
        )
    };
    let Some(start) = units.iter().position(|unit| !whitespace(unit)) else {
        return &[];
    };
    let end = units
        .iter()
        .rposition(|unit| !whitespace(unit))
        .map_or(start, |index| index + 1);
    &units[start..end]
}
