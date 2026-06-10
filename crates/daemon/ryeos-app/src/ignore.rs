//! Ingest ignore matcher — re-export from ryeos_state.
//!
//! The matcher implementation lives in `ryeos_state::ignore` so both
//! the daemon and CLI can use it without circular dependencies.

pub use ryeos_state::ignore::{
    builtin_patterns, matcher_from_builtins, IgnoreConfig, IgnoreMatcher,
};

/// Path to the ingest ignore config relative to app root.
pub const IGNORE_CONFIG_RELATIVE: &str = ".ai/node/ingest/ignore.yaml";

/// Load the ignore matcher from the app rootectory.
///
/// Returns an error if the config file is missing or invalid.
pub fn load_from_app_root(app_root: &std::path::Path) -> anyhow::Result<IgnoreMatcher> {
    let path = app_root.join(IGNORE_CONFIG_RELATIVE);
    IgnoreMatcher::load(&path)
}
