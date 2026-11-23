//! JS bindings for `open-sse/providers/capabilities.js`: generated from the signatures in `port.rs` by rust-anything.
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

/// `capabilitiesFromServiceKind`.
#[napi(js_name = "capabilitiesFromServiceKind")]
pub fn capabilities_from_service_kind(kind: String) -> napi::Result<String> {
    run(move || {
        let kind = crate::js::from_json(&kind)?;
        let out = port::capabilities_from_service_kind(kind)?;
        crate::js::to_json(&out)
    })
}

/// `getCapabilitiesForModel`.
#[napi(js_name = "getCapabilitiesForModel")]
pub fn get_capabilities_for_model(
    provider: String,
    model: String,
    catalog: String,
) -> napi::Result<String> {
    run(move || {
        let provider = crate::js::from_json(&provider)?;
        let model = crate::js::from_json(&model)?;
        let catalog = crate::js::from_json(&catalog)?;
        let out = port::get_capabilities_for_model(provider, model, catalog)?;
        crate::js::to_json(&out)
    })
}

/// `aggregateComboCapabilities`.
#[napi(js_name = "aggregateComboCapabilities")]
pub fn aggregate_combo_capabilities(
    models: String,
    lookup: String,
    resolve_caps: String,
    depth: Option<f64>,
) -> napi::Result<String> {
    run(move || {
        let models = crate::js::from_json(&models)?;
        let lookup = crate::js::from_json(&lookup)?;
        let resolve_caps = crate::js::from_json(&resolve_caps)?;
        let out = port::aggregate_combo_capabilities(models, lookup, resolve_caps, depth)?;
        crate::js::to_json(&out)
    })
}

/// `isGemini4Model`.
#[napi(js_name = "isGemini4Model")]
pub fn is_gemini4_model(model: String) -> napi::Result<String> {
    run(move || {
        let model = crate::js::from_json(&model)?;
        let out = port::is_gemini4_model(model)?;
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

static S_CAPABILITIES_FROM_SERVICE_KIND_0: wire::Schema = wire::Schema::Dyn;
static S_CAPABILITIES_FROM_SERVICE_KIND_OUT: wire::Schema = wire::Schema::Dyn;

/// `capabilitiesFromServiceKind` over the binary wire.
#[napi(js_name = "capabilitiesFromServiceKindWire")]
pub fn capabilities_from_service_kind_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let kind: Value = wire::read_value(r, &S_CAPABILITIES_FROM_SERVICE_KIND_0)?;
        let out = &port::capabilities_from_service_kind(kind)?;
        wire::write_value(w, out, &S_CAPABILITIES_FROM_SERVICE_KIND_OUT)?;
        Ok(())
    })
}

static S_AGGREGATE_COMBO_CAPABILITIES_0: wire::Schema = wire::Schema::Dyn;
static S_AGGREGATE_COMBO_CAPABILITIES_OUT: wire::Schema = wire::Schema::Dyn;

/// `aggregateComboCapabilities` over the binary wire.
#[napi(js_name = "aggregateComboCapabilitiesWire")]
pub fn aggregate_combo_capabilities_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let models: Value = wire::read_value(r, &S_AGGREGATE_COMBO_CAPABILITIES_0)?;
        let lookup: Option<Value> =
            serde_json::from_value(wire::read_dyn(r)?).map_err(|e| format!("TypeError: {e}"))?;
        let resolve_caps: Option<Value> =
            serde_json::from_value(wire::read_dyn(r)?).map_err(|e| format!("TypeError: {e}"))?;
        let depth: Option<f64> = r.opt_js(|r| r.f64())?;
        let out = &port::aggregate_combo_capabilities(models, lookup, resolve_caps, depth)?;
        wire::write_value(w, out, &S_AGGREGATE_COMBO_CAPABILITIES_OUT)?;
        Ok(())
    })
}

static S_IS_GEMINI4_MODEL_0: wire::Schema = wire::Schema::Dyn;
static S_IS_GEMINI4_MODEL_OUT: wire::Schema = wire::Schema::Dyn;

/// `isGemini4Model` over the binary wire.
#[napi(js_name = "isGemini4ModelWire")]
pub fn is_gemini4_model_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let model: Value = wire::read_value(r, &S_IS_GEMINI4_MODEL_0)?;
        let out = &port::is_gemini4_model(model)?;
        wire::write_value(w, out, &S_IS_GEMINI4_MODEL_OUT)?;
        Ok(())
    })
}
