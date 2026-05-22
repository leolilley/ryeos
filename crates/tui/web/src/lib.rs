//! ryeos-tui-web — Web/WASM renderer for the TUI.
//!
//! Provides DOM and Canvas 2D rendering from core data structures.
//! WASM bridge for live updates is in the wasm module.

pub mod render_canvas;
pub mod render_dom;

#[cfg(target_arch = "wasm32")]
mod wasm;
