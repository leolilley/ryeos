//! CAS object type definitions.
//!
//! Core object types:
//! - [`ThreadEvent`] — immutable journal fact
//! - [`ThreadSnapshot`] — current durable state of one thread
//! - [`ChainState`] — authoritative root per execution chain
//!
//! Project source types:
//! - [`ProjectSnapshot`] — snapshot of a project's source state
//! - [`SourceManifest`] — mapping of item refs to content blobs
//! - [`ItemSource`] — individual item with integrity metadata
//!
//! Distributed trust types:
//! - [`Attestation`] — signed claim about a CAS object

pub mod attestation;
pub mod bundle_event;
pub mod chain_state;
pub mod item_source;
pub mod project_snapshot;
pub mod source_manifest;
pub mod thread_event;
pub mod thread_snapshot;

pub use attestation::Attestation;
pub use bundle_event::{
    hash_bundle_event, validate_bundle_identifier, BundleEventAttribution, BundleEventObject,
    BUNDLE_EVENT_KIND,
};
pub use chain_state::{ChainState, ChainStateBuilder, ChainThreadEntry};
pub use item_source::ItemSource;
pub use project_snapshot::ProjectSnapshot;
pub use source_manifest::SourceManifest;
pub use thread_event::{EventDurability, ThreadEvent};
pub use thread_snapshot::{ThreadSnapshot, ThreadSnapshotBuilder, ThreadStatus, ThreadUsage};

/// Schema version shared across all CAS object types.
/// Bump when the object format changes in a incompatible way.
pub const SCHEMA_VERSION: u32 = 1;

/// Validate that an object kind matches the expected value.
pub fn validate_object_kind(kind: &str, expected: &str) -> anyhow::Result<()> {
    if kind != expected {
        anyhow::bail!("object kind mismatch: expected '{expected}', got '{kind}'");
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
