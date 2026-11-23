//! JS bindings for `open-sse/providers/models/`: generated from the signatures in `port.rs` by rust-anything.
//! Regenerate with `rust-anything refresh` after changing them.

use napi_derive::napi;

pub mod js;
pub mod port;
pub mod wire;

#[allow(unused_imports)]
use serde_json::Value;
pub mod utf16;

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

#[allow(dead_code)]
fn run<T>(f: impl FnOnce() -> Result<T, String>) -> napi::Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result.map_err(napi::Error::from_reason),
        Err(payload) => Err(napi::Error::from_reason(format!(
            "Rust panic: {}",
            panic_message(&*payload)
        ))),
    }
}

/// `withCodexReviewModels`.
#[napi(js_name = "withCodexReviewModels")]
pub fn with_codex_review_models(models: String) -> napi::Result<String> {
    run(move || {
        let models = crate::js::from_json(&models)?;
        let out = port::with_codex_review_models(models)?;
        crate::js::to_json(&out)
    })
}

/// `isMuseSparkModel`.
#[napi(js_name = "isMuseSparkModel")]
pub fn is_muse_spark_model(model_id: String) -> napi::Result<String> {
    run(move || {
        let model_id = crate::js::from_json(&model_id)?;
        let out = port::is_muse_spark_model(model_id)?;
        crate::js::to_json(&out)
    })
}

/// `isDeepSeekModel`.
#[napi(js_name = "isDeepSeekModel")]
pub fn is_deep_seek_model(model_id: String) -> napi::Result<String> {
    run(move || {
        let model_id = crate::js::from_json(&model_id)?;
        let out = port::is_deep_seek_model(model_id)?;
        crate::js::to_json(&out)
    })
}

/// `isOpencodeFreeTierModel`.
#[napi(js_name = "isOpencodeFreeTierModel")]
pub fn is_opencode_free_tier_model(model_id: String) -> napi::Result<String> {
    run(move || {
        let model_id = crate::js::from_json(&model_id)?;
        let out = port::is_opencode_free_tier_model(model_id)?;
        crate::js::to_json(&out)
    })
}

/// `opencodeFamilyFormats`.
#[napi(js_name = "opencodeFamilyFormats")]
pub fn opencode_family_formats(model_id: String) -> napi::Result<String> {
    run(move || {
        let model_id = crate::js::from_json(&model_id)?;
        let out = port::opencode_family_formats(model_id)?;
        crate::js::to_json(&out)
    })
}

/// `deriveModelName`.
#[napi(js_name = "deriveModelName")]
pub fn derive_model_name(id: String) -> napi::Result<String> {
    run(move || {
        let id = crate::js::from_json(&id)?;
        let out = port::derive_model_name(id)?;
        crate::js::to_json(&out)
    })
}

/// `normalizeModelId`.
#[napi(js_name = "normalizeModelId")]
pub fn normalize_model_id(model_id: String) -> napi::Result<String> {
    run(move || {
        let model_id = crate::js::from_json(&model_id)?;
        let out = port::normalize_model_id(model_id)?;
        crate::js::to_json(&out)
    })
}

/// `normalizeModel`.
#[napi(js_name = "normalizeModel")]
pub fn normalize_model(raw: String) -> napi::Result<String> {
    run(move || {
        let raw = crate::js::from_json(&raw)?;
        let out = port::normalize_model(raw)?;
        crate::js::to_json(&out)
    })
}

/// `modelKind`.
#[napi(js_name = "modelKind")]
pub fn model_kind(model: String) -> napi::Result<String> {
    run(move || {
        let model = crate::js::from_json(&model)?;
        let out = port::model_kind(model)?;
        crate::js::to_json(&out)
    })
}

/// `modelQuotaFamily`.
#[napi(js_name = "modelQuotaFamily")]
pub fn model_quota_family(model: String) -> napi::Result<String> {
    run(move || {
        let model = crate::js::from_json(&model)?;
        let out = port::model_quota_family(model)?;
        crate::js::to_json(&out)
    })
}

/// `modelStrip`.
#[napi(js_name = "modelStrip")]
pub fn model_strip(model: String) -> napi::Result<String> {
    run(move || {
        let model = crate::js::from_json(&model)?;
        let out = port::model_strip(model)?;
        crate::js::to_json(&out)
    })
}

/// `modelTargetFormat`.
#[napi(js_name = "modelTargetFormat")]
pub fn model_target_format(model: String) -> napi::Result<String> {
    run(move || {
        let model = crate::js::from_json(&model)?;
        let out = port::model_target_format(model)?;
        crate::js::to_json(&out)
    })
}

/// `modelSupportedFormats`.
#[napi(js_name = "modelSupportedFormats")]
pub fn model_supported_formats(model: String) -> napi::Result<String> {
    run(move || {
        let model = crate::js::from_json(&model)?;
        let out = port::model_supported_formats(model)?;
        crate::js::to_json(&out)
    })
}

// ---- the binary wire ------------------------------------------------------------------------

thread_local! {
    static WIRE: std::cell::RefCell<Option<(napi::bindgen_prelude::Uint8Array, napi::bindgen_prelude::Uint8Array)>> =
        const { std::cell::RefCell::new(None) };
    static PENDING: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Registers the argument and result buffers `index.cjs` writes to and reads from.
#[napi(js_name = "rustAnythingWireInit")]
pub fn rust_anything_wire_init(
    input: napi::bindgen_prelude::Uint8Array,
    output: napi::bindgen_prelude::Uint8Array,
) {
    WIRE.with(|w| *w.borrow_mut() = Some((input, output)));
}

/// Copies a result that did not fit into the (now larger) result buffer.
#[napi(js_name = "rustAnythingWireFlush")]
pub fn rust_anything_wire_flush() -> f64 {
    let bytes = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    WIRE.with(|w| {
        let mut w = w.borrow_mut();
        let Some((_, output)) = w.as_mut() else {
            return -1.0;
        };
        // SAFETY: the call is synchronous; JS does not touch `output` until it returns.
        let out = unsafe { output.as_mut() };
        let n = bytes.len().min(out.len());
        out[..n].copy_from_slice(&bytes[..n]);
        n as f64
    })
}

/// Reads `len` argument bytes, runs `f`, and writes its result; a negative return is the size
/// of a result that did not fit, kept for `rustAnythingWireFlush`.
fn wire_io(
    len: u32,
    f: impl FnOnce(&mut wire::Reader, &mut wire::Writer) -> Result<(), String>,
) -> napi::Result<f64> {
    WIRE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some((input, output)) = guard.as_mut() else {
            return Err(napi::Error::from_reason(
                "rust-anything: the wire buffers are not registered",
            ));
        };
        let len = (len as usize).min(input.len());
        let mut w = wire::Writer::default();
        match run(|| f(&mut wire::Reader::new(&input[..len]), &mut w)) {
            // A result of another shape than the TS type: JS runs the call through JSON.
            Err(e) if e.reason.starts_with(wire::MISMATCH) => return Ok(f64::NAN),
            other => other?,
        }
        // SAFETY: the call is synchronous; JS does not touch `output` until it returns.
        let out = unsafe { output.as_mut() };
        let n = w.0.len();
        if n > out.len() {
            PENDING.with(|p| *p.borrow_mut() = w.0);
            return Ok(-(n as f64));
        }
        out[..n].copy_from_slice(&w.0);
        Ok(n as f64)
    })
}

static S_WITH_CODEX_REVIEW_MODELS_0: wire::Schema = wire::Schema::Dyn;
static S_WITH_CODEX_REVIEW_MODELS_OUT: wire::Schema = wire::Schema::Dyn;

/// `withCodexReviewModels` over the binary wire.
#[napi(js_name = "withCodexReviewModelsWire")]
pub fn with_codex_review_models_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let models: serde_json::Value = wire::read_value(r, &S_WITH_CODEX_REVIEW_MODELS_0)?;
        let out = &port::with_codex_review_models(models)?;
        wire::write_value(w, out, &S_WITH_CODEX_REVIEW_MODELS_OUT)?;
        Ok(())
    })
}

static S_IS_MUSE_SPARK_MODEL_0: wire::Schema = wire::Schema::Dyn;
static S_IS_MUSE_SPARK_MODEL_OUT: wire::Schema = wire::Schema::Dyn;

/// `isMuseSparkModel` over the binary wire.
#[napi(js_name = "isMuseSparkModelWire")]
pub fn is_muse_spark_model_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model_id: serde_json::Value = wire::read_value(r, &S_IS_MUSE_SPARK_MODEL_0)?;
        let out = &port::is_muse_spark_model(model_id)?;
        wire::write_value(w, out, &S_IS_MUSE_SPARK_MODEL_OUT)?;
        Ok(())
    })
}

static S_IS_DEEP_SEEK_MODEL_0: wire::Schema = wire::Schema::Dyn;
static S_IS_DEEP_SEEK_MODEL_OUT: wire::Schema = wire::Schema::Dyn;

/// `isDeepSeekModel` over the binary wire.
#[napi(js_name = "isDeepSeekModelWire")]
pub fn is_deep_seek_model_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model_id: serde_json::Value = wire::read_value(r, &S_IS_DEEP_SEEK_MODEL_0)?;
        let out = &port::is_deep_seek_model(model_id)?;
        wire::write_value(w, out, &S_IS_DEEP_SEEK_MODEL_OUT)?;
        Ok(())
    })
}

static S_IS_OPENCODE_FREE_TIER_MODEL_0: wire::Schema = wire::Schema::Dyn;
static S_IS_OPENCODE_FREE_TIER_MODEL_OUT: wire::Schema = wire::Schema::Dyn;

/// `isOpencodeFreeTierModel` over the binary wire.
#[napi(js_name = "isOpencodeFreeTierModelWire")]
pub fn is_opencode_free_tier_model_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model_id: serde_json::Value = wire::read_value(r, &S_IS_OPENCODE_FREE_TIER_MODEL_0)?;
        let out = &port::is_opencode_free_tier_model(model_id)?;
        wire::write_value(w, out, &S_IS_OPENCODE_FREE_TIER_MODEL_OUT)?;
        Ok(())
    })
}

static S_OPENCODE_FAMILY_FORMATS_0: wire::Schema = wire::Schema::Dyn;
static S_OPENCODE_FAMILY_FORMATS_OUT: wire::Schema = wire::Schema::Dyn;

/// `opencodeFamilyFormats` over the binary wire.
#[napi(js_name = "opencodeFamilyFormatsWire")]
pub fn opencode_family_formats_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model_id: serde_json::Value = wire::read_value(r, &S_OPENCODE_FAMILY_FORMATS_0)?;
        let out = &port::opencode_family_formats(model_id)?;
        wire::write_value(w, out, &S_OPENCODE_FAMILY_FORMATS_OUT)?;
        Ok(())
    })
}

static S_DERIVE_MODEL_NAME_0: wire::Schema = wire::Schema::Dyn;
static S_DERIVE_MODEL_NAME_OUT: wire::Schema = wire::Schema::Dyn;

/// `deriveModelName` over the binary wire.
#[napi(js_name = "deriveModelNameWire")]
pub fn derive_model_name_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let id: serde_json::Value = wire::read_value(r, &S_DERIVE_MODEL_NAME_0)?;
        let out = &port::derive_model_name(id)?;
        wire::write_value(w, out, &S_DERIVE_MODEL_NAME_OUT)?;
        Ok(())
    })
}

static S_NORMALIZE_MODEL_ID_0: wire::Schema = wire::Schema::Dyn;
static S_NORMALIZE_MODEL_ID_OUT: wire::Schema = wire::Schema::Dyn;

/// `normalizeModelId` over the binary wire.
#[napi(js_name = "normalizeModelIdWire")]
pub fn normalize_model_id_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model_id: serde_json::Value = wire::read_value(r, &S_NORMALIZE_MODEL_ID_0)?;
        let out = &port::normalize_model_id(model_id)?;
        wire::write_value(w, out, &S_NORMALIZE_MODEL_ID_OUT)?;
        Ok(())
    })
}

static S_NORMALIZE_MODEL_0: wire::Schema = wire::Schema::Dyn;
static S_NORMALIZE_MODEL_OUT: wire::Schema = wire::Schema::Dyn;

/// `normalizeModel` over the binary wire.
#[napi(js_name = "normalizeModelWire")]
pub fn normalize_model_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let raw: serde_json::Value = wire::read_value(r, &S_NORMALIZE_MODEL_0)?;
        let out = &port::normalize_model(raw)?;
        wire::write_value(w, out, &S_NORMALIZE_MODEL_OUT)?;
        Ok(())
    })
}

static S_MODEL_KIND_0: wire::Schema = wire::Schema::Dyn;
static S_MODEL_KIND_OUT: wire::Schema = wire::Schema::Dyn;

/// `modelKind` over the binary wire.
#[napi(js_name = "modelKindWire")]
pub fn model_kind_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model: serde_json::Value = wire::read_value(r, &S_MODEL_KIND_0)?;
        let out = &port::model_kind(model)?;
        wire::write_value(w, out, &S_MODEL_KIND_OUT)?;
        Ok(())
    })
}

static S_MODEL_QUOTA_FAMILY_0: wire::Schema = wire::Schema::Dyn;
static S_MODEL_QUOTA_FAMILY_OUT: wire::Schema = wire::Schema::Dyn;

/// `modelQuotaFamily` over the binary wire.
#[napi(js_name = "modelQuotaFamilyWire")]
pub fn model_quota_family_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model: serde_json::Value = wire::read_value(r, &S_MODEL_QUOTA_FAMILY_0)?;
        let out = &port::model_quota_family(model)?;
        wire::write_value(w, out, &S_MODEL_QUOTA_FAMILY_OUT)?;
        Ok(())
    })
}

static S_MODEL_STRIP_0: wire::Schema = wire::Schema::Dyn;
static S_MODEL_STRIP_OUT: wire::Schema = wire::Schema::Dyn;

/// `modelStrip` over the binary wire.
#[napi(js_name = "modelStripWire")]
pub fn model_strip_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model: serde_json::Value = wire::read_value(r, &S_MODEL_STRIP_0)?;
        let out = &port::model_strip(model)?;
        wire::write_value(w, out, &S_MODEL_STRIP_OUT)?;
        Ok(())
    })
}

static S_MODEL_TARGET_FORMAT_0: wire::Schema = wire::Schema::Dyn;
static S_MODEL_TARGET_FORMAT_OUT: wire::Schema = wire::Schema::Dyn;

/// `modelTargetFormat` over the binary wire.
#[napi(js_name = "modelTargetFormatWire")]
pub fn model_target_format_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model: serde_json::Value = wire::read_value(r, &S_MODEL_TARGET_FORMAT_0)?;
        let out = &port::model_target_format(model)?;
        wire::write_value(w, out, &S_MODEL_TARGET_FORMAT_OUT)?;
        Ok(())
    })
}

static S_MODEL_SUPPORTED_FORMATS_0: wire::Schema = wire::Schema::Dyn;
static S_MODEL_SUPPORTED_FORMATS_OUT: wire::Schema = wire::Schema::Dyn;

/// `modelSupportedFormats` over the binary wire.
#[napi(js_name = "modelSupportedFormatsWire")]
pub fn model_supported_formats_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model: serde_json::Value = wire::read_value(r, &S_MODEL_SUPPORTED_FORMATS_0)?;
        let out = &port::model_supported_formats(model)?;
        wire::write_value(w, out, &S_MODEL_SUPPORTED_FORMATS_OUT)?;
        Ok(())
    })
}
