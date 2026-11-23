//! JS bindings for `src/shared/utils/keyAccess.js`: generated from the signatures in `port.rs` by rust-anything.
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

/// `normalizeAllowList`.
#[napi(js_name = "normalizeAllowList")]
pub fn normalize_allow_list(list: String) -> napi::Result<String> {
    run(move || {
        let list = crate::js::from_json(&list)?;
        let out = port::normalize_allow_list(list)?;
        crate::js::to_json(&out)
    })
}

/// `keyAccessFromColumns`.
#[napi(js_name = "keyAccessFromColumns")]
pub fn key_access_from_columns(restricted_col: String, allow_col: String) -> napi::Result<String> {
    run(move || {
        let restricted_col = crate::js::from_json(&restricted_col)?;
        let allow_col = crate::js::from_json(&allow_col)?;
        let out = port::key_access_from_columns(restricted_col, allow_col)?;
        crate::js::to_json(&out)
    })
}

/// `keyAccessToColumns`.
#[napi(js_name = "keyAccessToColumns")]
pub fn key_access_to_columns(access: String) -> napi::Result<String> {
    run(move || {
        let access = crate::js::from_json(&access)?;
        let out = port::key_access_to_columns(access)?;
        crate::js::to_json(&out)
    })
}

/// `validateKeyAccessInput`.
#[napi(js_name = "validateKeyAccessInput")]
pub fn validate_key_access_input(input: String) -> napi::Result<String> {
    run(move || {
        let input = crate::js::from_json(&input)?;
        let out = port::validate_key_access_input(input)?;
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

static S_NORMALIZE_ALLOW_LIST_0: wire::Schema = wire::Schema::Dyn;
static S_NORMALIZE_ALLOW_LIST_OUT: wire::Schema = wire::Schema::Dyn;

/// `normalizeAllowList` over the binary wire.
#[napi(js_name = "normalizeAllowListWire")]
pub fn normalize_allow_list_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let list: serde_json::Value = wire::read_value(r, &S_NORMALIZE_ALLOW_LIST_0)?;
        let out = &port::normalize_allow_list(list)?;
        wire::write_value(w, out, &S_NORMALIZE_ALLOW_LIST_OUT)?;
        Ok(())
    })
}

static S_KEY_ACCESS_FROM_COLUMNS_0: wire::Schema = wire::Schema::Dyn;
static S_KEY_ACCESS_FROM_COLUMNS_1: wire::Schema = wire::Schema::Dyn;
static S_KEY_ACCESS_FROM_COLUMNS_OUT: wire::Schema = wire::Schema::Dyn;

/// `keyAccessFromColumns` over the binary wire.
#[napi(js_name = "keyAccessFromColumnsWire")]
pub fn key_access_from_columns_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let restricted_col: serde_json::Value = wire::read_value(r, &S_KEY_ACCESS_FROM_COLUMNS_0)?;
        let allow_col: serde_json::Value = wire::read_value(r, &S_KEY_ACCESS_FROM_COLUMNS_1)?;
        let out = &port::key_access_from_columns(restricted_col, allow_col)?;
        wire::write_value(w, out, &S_KEY_ACCESS_FROM_COLUMNS_OUT)?;
        Ok(())
    })
}

static S_KEY_ACCESS_TO_COLUMNS_0: wire::Schema = wire::Schema::Dyn;
static S_KEY_ACCESS_TO_COLUMNS_OUT: wire::Schema = wire::Schema::Dyn;

/// `keyAccessToColumns` over the binary wire.
#[napi(js_name = "keyAccessToColumnsWire")]
pub fn key_access_to_columns_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let access: serde_json::Value = wire::read_value(r, &S_KEY_ACCESS_TO_COLUMNS_0)?;
        let out = &port::key_access_to_columns(access)?;
        wire::write_value(w, out, &S_KEY_ACCESS_TO_COLUMNS_OUT)?;
        Ok(())
    })
}

static S_VALIDATE_KEY_ACCESS_INPUT_0: wire::Schema = wire::Schema::Dyn;
static S_VALIDATE_KEY_ACCESS_INPUT_OUT: wire::Schema = wire::Schema::Dyn;

/// `validateKeyAccessInput` over the binary wire.
#[napi(js_name = "validateKeyAccessInputWire")]
pub fn validate_key_access_input_wire(len: u32) -> napi::Result<f64> {
    wire_io(len, |r, w| {
        let input: serde_json::Value = wire::read_value(r, &S_VALIDATE_KEY_ACCESS_INPUT_0)?;
        let out = &port::validate_key_access_input(input)?;
        wire::write_value(w, out, &S_VALIDATE_KEY_ACCESS_INPUT_OUT)?;
        Ok(())
    })
}
