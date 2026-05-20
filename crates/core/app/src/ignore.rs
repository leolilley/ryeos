//! Ingest ignore matcher — re-export from ryeos_state.
//!
//! The matcher implementation lives in `ryeos_state::ignore` so both
//! the daemon and CLI can use it without circular dependencies.

pub use ryeos_state::ignore::{
    IgnoreConfig, IgnoreMatcher, builtin_patterns, matcher_from_builtins,
};

/// Path to the ingest ignore config relative to system space root.
pub const IGNORE_CONFIG_RELATIVE: &str = ".ai/node/ingest/ignore.yaml";

/// Load the ignore matcher from the system space directory.
///
/// Returns an error if the config file is missing or invalid.
pub fn load_from_system_space(system_space_dir: &std::path::Path) -> anyhow::Result<IgnoreMatcher> {
    let path = system_space_dir.join(IGNORE_CONFIG_RELATIVE);
    IgnoreMatcher::load(&path)
}
