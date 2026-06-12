//! ryeos-ui-web — Web renderer for Rye OS.
//!
//! Provides DOM and Canvas rendering from core data structures.
//! WASM bridge for live updates is in the wasm module.

#[cfg(target_arch = "wasm32")]
mod wasm;
