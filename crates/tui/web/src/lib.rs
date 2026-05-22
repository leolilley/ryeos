//! ryeos-tui-web — Web/WASM renderer for the TUI.
//!
//! V1: thin stub. Full Canvas + DOM rendering deferred.

#[cfg(target_arch = "wasm32")]
mod wasm;

/// Placeholder — web crate is a thin stub in V1.
pub fn hello() -> &'static str {
    "ryeos-tui-web stub"
}
