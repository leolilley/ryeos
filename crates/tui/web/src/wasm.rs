//! WASM bindings for the TUI web renderer.
//!
//! V1 stub: compiles but does not implement full rendering.

use wasm_bindgen::prelude::*;

/// Stub entry point for WASM.
#[wasm_bindgen]
pub fn start(_initial_json: &str, _width: u32, _height: u32) -> Result<(), JsValue> {
    Ok(())
}

/// Stub tick.
#[wasm_bindgen]
pub fn tick(_now_ms: u64) -> Result<(), JsValue> {
    Ok(())
}
