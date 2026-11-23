use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[allow(dead_code)]
#[path = "../../app/src/chat_input.rs"]
mod chat_input;
#[path = "../../app/src/pxpipe_factsheet.rs"]
mod pxpipe_factsheet;
#[path = "../../app/src/pxpipe_render.rs"]
mod pxpipe_render;
#[path = "../../app/src/pxpipe_static.rs"]
mod pxpipe_static;
#[path = "../../app/src/pxpipe_transform.rs"]
mod pxpipe_transform;

mod provider_connections {
    pub fn truthy(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Null | serde_json::Value::Bool(false) => false,
            serde_json::Value::Number(number) => number.as_f64() != Some(0.0),
            serde_json::Value::String(text) => !text.is_empty(),
            _ => true,
        }
    }
}

mod cli {
    pub(super) fn js_space(ch: char) -> bool {
        matches!(ch, '\u{9}'..='\u{d}' | '\u{20}' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
                 | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
    }
}

fn page_evidence(bytes: Vec<u8>) -> Result<Value, png::DecodingError> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info()?;
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let image = reader.next_frame(&mut pixels)?;
    assert!(matches!(
        image.color_type,
        png::ColorType::Grayscale | png::ColorType::Rgb
    ));
    Ok(json!({
        "width":image.width, "height":image.height,
        "pixelSha256":format!("{:x}", Sha256::digest(&pixels[..image.buffer_size()])),
    }))
}

fn replace_result(value: &mut Value, text: &str) {
    match value {
        Value::String(field) if field.contains("{result}") => {
            *field = field.replace("{result}", text)
        }
        Value::Array(items) => items.iter_mut().for_each(|item| replace_result(item, text)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| replace_result(field, text)),
        _ => (),
    }
}

fn replace_png(value: &mut Value) -> Result<(), Box<dyn std::error::Error>> {
    match value {
        Value::Object(_)
            if value["type"] == "image"
                && value["source"]["media_type"] == "image/png"
                && value["source"]["data"].is_string() =>
        {
            let bytes = STANDARD.decode(value["source"]["data"].as_str().unwrap())?;
            value["source"]["data"] = page_evidence(bytes)?;
        }
        Value::Object(fields) => {
            for field in fields.values_mut() {
                replace_png(field)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                replace_png(item)?;
            }
        }
        _ => (),
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input: Value = serde_json::from_reader(std::io::stdin().lock())?;
    if let Some(render) = input.get("render") {
        let number =
            |key: &str, default: usize| render[key].as_u64().map_or(default, |n| n as usize);
        let pages = pxpipe_render::render(
            &render["text"].as_str().unwrap().repeat(number("repeat", 1)),
            &pxpipe_render::Options {
                columns: number("cols", 312),
                max_height: number("maxHeightPx", 728),
                max_chars_per_page: number("maxCharsPerImage", 28_080),
                style: pxpipe_render::Style {
                    row_spacing: number("rowSpacing", 0),
                    ..Default::default()
                }
                .override_with(&render["style"]),
                shrink: render["shrink"] != false,
                reflow: render["reflow"] == true,
                ..Default::default()
            },
        )?;
        let dropped: usize = pages.iter().map(|p| p.dropped).sum();
        let mut output = Vec::with_capacity(pages.len());
        for page in pages {
            let evidence = page_evidence(page.png)?;
            assert_eq!(evidence["width"], page.width);
            assert_eq!(evidence["height"], page.height);
            output.push(evidence);
        }
        println!("{}", json!({"pages":output,"droppedChars":dropped}));
        return Ok(());
    }
    if let Some(factsheet) = input.get("factsheet") {
        let text = factsheet["text"]
            .as_str()
            .unwrap()
            .repeat(factsheet["repeat"].as_u64().unwrap_or(1) as usize);
        println!("{}", json!(pxpipe_factsheet::text(&text)));
        return Ok(());
    }
    if let Some(actual) = input.get("actual") {
        let mut body = actual["body"].clone();
        let repeat = actual["repeat"].as_u64().unwrap_or(1) as usize;
        if let Some(system) = body["system"].as_str() {
            body["system"] = json!(system.repeat(repeat));
        } else if let Some(blocks) = body.get_mut("system").and_then(Value::as_array_mut) {
            for block in blocks {
                let text = block["text"].as_str().unwrap().replace(
                    "{fixture}",
                    &"Fixture repetition with docs and ids. ".repeat(repeat),
                );
                block["text"] = json!(text);
            }
        }
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools {
                if let Some(description) = tool["description"].as_str() {
                    tool["description"] = json!(description.replace(
                        "{fixture}",
                        &"Fixture repetition with docs and ids. ".repeat(repeat),
                    ));
                }
            }
        }
        if let Some(result_text) = actual["resultText"].as_str() {
            let repeat = actual["resultRepeat"].as_u64().unwrap_or(1) as usize;
            replace_result(&mut body["messages"], &result_text.repeat(repeat));
        }
        let mut result = match pxpipe_transform::prepare(
            &body,
            true,
            true,
            Some("claude"),
            actual["minChars"].as_f64().unwrap_or_default(),
            0.0,
        ) {
            Ok(prepared) => {
                assert!(pxpipe_static::supports(
                    &body,
                    actual["model"].as_str().unwrap(),
                    actual["scope"].as_str(),
                    actual["overrides"].as_str(),
                ));
                let transform = pxpipe_static::transform(
                    &body,
                    actual["model"].as_str().unwrap(),
                    &prepared,
                    actual["scope"].as_str(),
                    actual["overrides"].as_str(),
                )
                .expect("supported transform case");
                prepared.complete(transform.map(Some), 0)
            }
            Err(skipped) => skipped,
        };
        for name in ["durationMs", "imageBytes", "compressedBodyChars"] {
            result.summary.as_object_mut().unwrap().shift_remove(name);
        }
        if let Some(body) = &mut result.body {
            replace_png(&mut body["messages"])?;
        }
        println!("{}", json!({"body":result.body,"summary":result.summary}));
        return Ok(());
    }
    let number = |key: &str| {
        input[key]
            .as_f64()
            .or_else(|| input[key].as_str()?.trim().parse().ok())
            .unwrap_or_default()
    };
    let prepared = pxpipe_transform::prepare(
        &input["body"],
        input["enabled"] == true,
        input["available"] == true,
        input["format"].as_str(),
        number("minChars"),
        number("timeoutMs"),
    );
    let mut request = None;
    let mut result = match prepared {
        Ok(prepared) => {
            let info = &input["outcome"]["info"];
            let metric = |key: &str| info[key].as_u64().unwrap_or_default() as usize;
            let outcome = &input["outcome"];
            let transformed = if let Some(error) = outcome["error"].as_str() {
                Err(error.to_owned())
            } else if outcome["timeout"] == true {
                assert!(!prepared.timeout.is_zero());
                Ok(None)
            } else if outcome["applied"] == true {
                Ok(Some(pxpipe_transform::Transform::Applied {
                    body: serde_json::to_vec(&outcome["body"])?,
                    info: pxpipe_transform::ImageInfo {
                        compressed_chars: metric("compressedChars"),
                        count: metric("imageCount"),
                        bytes: metric("imageBytes"),
                        tokens: metric("imageTokens"),
                        pixels: metric("imagePixels"),
                        baseline_tokens: metric("baselineTokens"),
                    },
                    owns_cache_control: outcome["cache"]["ownsCacheControl"] == true,
                }))
            } else {
                Ok(Some(pxpipe_transform::Transform::Skipped {
                    reason: outcome["reason"].as_str().map(str::to_owned),
                    detail: outcome["detail"].as_str().map(str::to_owned),
                }))
            };
            let min = if prepared.min_chars.fract() == 0.0 {
                json!(prepared.min_chars as u64)
            } else {
                json!(prepared.min_chars)
            };
            let mut captured = json!({"body":input["body"]});
            if let Some(model) = input.get("model") {
                captured["model"] = model.clone();
            }
            captured["options"] = json!({"minCompressChars":min});
            request = Some(captured);
            prepared.complete(transformed, 0)
        }
        Err(skipped) => skipped,
    };
    result
        .summary
        .as_object_mut()
        .unwrap()
        .shift_remove("durationMs");
    let mut output = json!({"body":result.body,"summary":result.summary});
    if let Some(request) = request {
        output["request"] = request;
    }
    println!("{output}");
    Ok(())
}
