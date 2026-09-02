//! Ingest ignore matcher — re-export from ryeos_state.
//!
//! The matcher implementation lives in `ryeos_state::ignore` so both
//! the daemon and CLI can use it without circular dependencies.

pub use ryeos_state::ignore::{
    IgnoreConfig, IgnoreMatcher, builtin_patterns, matcher_from_builtins,
};

/// Diagnostic coordinate of the exact node-signed policy source.
pub const INGEST_IGNORE_POLICY_RELATIVE: &str = ".ai/node/policies/ingest_ignore.yaml";
