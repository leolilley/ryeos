//! ryeos-tui-web — Web/WASM renderer for the TUI.
//!
//! V1: provides DOM rendering from TextSurface.
//! WASM bridge and Canvas rendering deferred.

pub mod render_dom;

/// Stub — web crate entry point.
pub fn hello() -> &'static str {
    "ryeos-tui-web"
}

#[cfg(target_arch = "wasm32")]
mod wasm;
