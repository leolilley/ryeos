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

pub mod admitted_launch_capsule;
pub mod attestation;
pub mod bundle_event;
pub mod chain_state;
pub mod execution_project_authority;
pub mod item_source;
pub mod live_input;
pub mod project_file;
pub mod project_snapshot;
pub mod project_snapshot_policy;
pub mod project_tree;
pub mod source_manifest;
pub mod thread_event;
pub mod thread_snapshot;

pub use admitted_launch_capsule::{
    AdmittedLaunchArtifactIdentity, AdmittedLaunchCapsule, DirectExecutableIdentity,
    ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
};
pub use attestation::Attestation;
pub use bundle_event::{
    hash_bundle_event, validate_bundle_identifier, BundleEventAttachment, BundleEventAttribution,
    BundleEventObject, BUNDLE_EVENT_KIND, MAX_BUNDLE_EVENT_ATTACHMENTS,
    MAX_BUNDLE_EVENT_ATTACHMENT_BYTES, MAX_BUNDLE_EVENT_SERIALIZED_BYTES,
};
pub use chain_state::{ChainState, ChainStateBuilder, ChainThreadEntry};
pub use execution_project_authority::{
    ChildProjectAuthorityPolicy, EnvironmentAuthority, EnvironmentNameAuthority,
    ExecutionLaunchDriver, ExecutionLifecycleAuthority, ExecutionOwnershipAuthority,
    ExecutionProjectAuthority, ExecutionRecoveryAuthority, LiveAccessAuthority, LiveProjectAccess,
    LiveSymlinkPolicy, OperationalProjectAuthorityTransition, PinnedChildProjectRealization,
    PinnedProjectRealization, PinnedTerminalPublication,
};
pub use item_source::ItemSource;
pub use live_input::{LiveInput, LiveInputIntent};
pub use project_file::ProjectFile;
pub use project_snapshot::ProjectSnapshot;
pub use project_snapshot_policy::ProjectSnapshotPolicy;
pub use project_tree::ProjectTree;
pub use source_manifest::SourceManifest;
pub use thread_event::{EventDurability, ThreadEvent, MAX_THREAD_EVENT_SERIALIZED_BYTES};
pub use thread_snapshot::{
    parse_canonical_timestamp, CapturedEffectiveTrustClass, CapturedItemSpace,
    CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
    CapturedThreadHistoryMinimumClamp, CapturedThreadHistoryPolicy, ThreadHistoryRetention,
    ThreadSnapshot, ThreadSnapshotBuilder, ThreadStatus, ThreadUsage, UsageSubject,
    MAX_TERMINAL_DURATION_SECONDS, THREAD_SNAPSHOT_SCHEMA_VERSION,
};

/// Schema version shared across all CAS object types.
/// Bump when the object format changes in a incompatible way.
pub const SCHEMA_VERSION: u32 = 1;

/// Validate the canonical, contained project-relative path used as the source
/// manifest key and embedded `ItemSource.item_ref`. These fields identify
/// files, not executable item kinds: they deliberately remain kind-agnostic.
pub(crate) fn validate_canonical_project_relative_path(value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("project-relative source path must not be empty");
    }
    if value.contains('\\') || value.chars().any(char::is_control) {
        anyhow::bail!("project-relative source path has a non-canonical character: {value:?}");
    }
    if value.starts_with('/') || value.ends_with('/') {
        anyhow::bail!("project-relative source path must be contained and name a file: {value}");
    }
    if value
        .split('/')
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        anyhow::bail!("project-relative source path has a non-canonical component: {value}");
    }
    for component in std::path::Path::new(value).components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            anyhow::bail!("project-relative source path is not contained: {value}");
        }
    }
    Ok(())
}

pub(crate) fn validate_trimmed_control_free(
    label: &str,
    value: &str,
    allow_empty: bool,
) -> anyhow::Result<()> {
    if (!allow_empty && value.is_empty()) || value.trim() != value {
        anyhow::bail!("{label} must be non-empty and have no surrounding whitespace");
    }
    if value.chars().any(char::is_control) {
        anyhow::bail!("{label} must not contain control characters");
    }
    Ok(())
}

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

    #[test]
    fn project_source_paths_are_kind_agnostic_but_structurally_strict() {
        assert!(validate_canonical_project_relative_path(".ai/tools/run.sh").is_ok());
        assert!(validate_canonical_project_relative_path("src/lib.rs").is_ok());
        for invalid in [
            "",
            "/absolute",
            "a/../b",
            "a/./b",
            "a//b",
            "trailing/",
            "windows\\path",
        ] {
            assert!(
                validate_canonical_project_relative_path(invalid).is_err(),
                "{invalid}"
            );
        }
    }
}
