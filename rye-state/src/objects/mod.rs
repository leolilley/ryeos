//! CAS object type definitions.
//!
//! Three core object types for execution state:
//! - [`ThreadEvent`] — immutable journal fact
//! - [`ThreadSnapshot`] — current durable state of one thread
//! - [`ChainState`] — authoritative root per execution chain

pub mod chain_state;
pub mod thread_event;
pub mod thread_snapshot;

pub use chain_state::{ChainState, ChainStateBuilder, ChainThreadEntry};
pub use thread_event::{EventDurability, ThreadEvent};
pub use thread_snapshot::{ThreadSnapshot, ThreadSnapshotBuilder, ThreadStatus};

/// Schema version shared across all CAS object types.
/// Bump when the object format changes in a backward-incompatible way.
pub const SCHEMA_VERSION: u32 = 1;

/// Validate that an object kind matches the expected value.
pub fn validate_object_kind(kind: &str, expected: &str) -> anyhow::Result<()> {
    if kind != expected {
        anyhow::bail!(
            "object kind mismatch: expected '{expected}', got '{kind}'"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_object_kind_matches() {
        assert!(validate_object_kind("thread_event", "thread_event").is_ok());
    }

    #[test]
    fn validate_object_kind_rejects_mismatch() {
        assert!(validate_object_kind("thread_snapshot", "thread_event").is_err());
    }
}
