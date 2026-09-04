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

use serde::Deserialize as _;

/// Deserialize one field whose wire presence is mandatory while `null`
/// remains a meaningful explicit value. Unlike Serde's default `Option`
/// handling, a missing field therefore fails the current-schema clean cut.
pub(crate) fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub mod admitted_launch_capsule;
pub mod attestation;
pub mod bundle_event;
pub mod chain_state;
pub mod chain_writer_transition;
pub mod effect_record;
pub mod execution_identity;
pub mod execution_project_authority;
pub mod execution_realization;
pub mod external_content_activation;
pub mod external_content_binding;
pub mod external_content_manifest;
pub mod external_large_content_manifest;
pub mod item_source;
pub mod live_input;
pub mod persistent_session_capsule;
pub mod placement_runtime_seed;
pub mod placement_transfer_manifest;
pub mod portable_state_tree;
pub mod project_file;
pub mod project_snapshot;
pub mod project_snapshot_policy;
pub mod project_tree;
pub mod source_closure;
pub mod source_manifest;
pub mod state_anchor;
pub mod state_manifest;
pub mod thread_event;
pub mod thread_snapshot;
pub mod worker_session_restore;

pub use admitted_launch_capsule::{
    ADMITTED_DIRECT_COMMAND_ROOT, ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION, AdmittedAccountingScope,
    AdmittedDirectCommandClosure, AdmittedExecutionClosure, AdmittedLaunchArtifactIdentity,
    AdmittedLaunchAuthority, AdmittedLaunchCapsule, DirectExecutableIdentity,
    DirectRootSourceIdentity, DirectRuntimeIdentity, DirectRuntimeSourceSpace,
    admitted_direct_command_execution_path,
};
pub use attestation::Attestation;
pub use bundle_event::{
    BUNDLE_EVENT_KIND, BundleEventAttachment, BundleEventAttribution, BundleEventObject,
    MAX_BUNDLE_EVENT_ATTACHMENT_BYTES, MAX_BUNDLE_EVENT_ATTACHMENTS,
    MAX_BUNDLE_EVENT_SERIALIZED_BYTES, hash_bundle_event, validate_bundle_identifier,
};
pub use chain_state::{ChainState, ChainStateBuilder, ChainThreadEntry};
pub use chain_writer_transition::{
    CHAIN_WRITER_TRANSITION_CLAIM, CHAIN_WRITER_TRANSITION_POLICY, CHAIN_WRITER_TRANSITION_SCHEMA,
    ChainWriterTransitionEvidence,
};
pub use effect_record::{
    AdmittedDispatchSubject, AdmittedEffectAuthorization, DispatchEffectAnswer,
    DispatchEffectIdentity, DispatchEffectRecord, EFFECT_KEY_SCHEMA, EFFECT_RECORD_KIND,
    EFFECT_RECORD_SCHEMA_VERSION, EffectClass, EffectFirstObservation, RECORDABLE_EFFECT_CLASSES,
    canonical_value_digest,
};
pub use execution_identity::{
    EXECUTION_IDENTITY_ATTESTATION_CLAIM, EXECUTION_IDENTITY_ATTESTATION_POLICY,
    EXECUTION_IDENTITY_KIND, EXECUTION_IDENTITY_SCHEMA_VERSION, ExecutionCpuIdentity,
    ExecutionIdentity, ExecutionOperatingSystemIdentity, ExecutionSubstrateBuild,
    MAX_EXECUTION_IDENTITY_BYTES,
};
pub use execution_project_authority::{
    ChildProjectAuthorityPolicy, EnvironmentAuthority, EnvironmentNameAuthority,
    ExecutionLaunchDriver, ExecutionLifecycleAuthority, ExecutionOwnershipAuthority,
    ExecutionProjectAuthority, ExecutionRecoveryAuthority, LiveAccessAuthority,
    LiveFilesystemConfinement, LiveProjectAccess, LiveSymlinkPolicy,
    OperationalProjectAuthorityTransition, PinnedChildProjectRealization, PinnedProjectRealization,
    PinnedTerminalPublication,
};
pub use execution_realization::{
    ADMITTED_EXECUTION_REALIZATION_KIND, AdmittedExecutionRealization,
    EXECUTION_REALIZATION_SCHEMA_VERSION, ExecutionComponentReference, ExecutionComponentStorage,
    MAX_EXECUTION_COMPONENTS, MAX_EXECUTION_PROPERTIES, MAX_EXECUTION_REALIZATION_BYTES,
    OBSERVED_EXECUTION_REALIZATION_KIND, ObservedExecutionRealization,
};
pub use external_content_activation::{
    EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE, EXTERNAL_CONTENT_ACTIVATION_KIND,
    EXTERNAL_CONTENT_ACTIVATION_SCHEMA, ExternalContentActivationComponentReceipt,
    ExternalContentActivationReceipt, MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS,
};
pub use external_content_binding::{
    EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE, EXTERNAL_CONTENT_BINDING_KIND,
    EXTERNAL_CONTENT_BINDING_SCHEMA, EXTERNAL_CONTENT_BINDING_SCHEMA_EPOCH, ExternalContentBinding,
    ExternalContentBindingState,
};
pub use external_content_manifest::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_CONTENT_TREE_SCHEMA,
    EXTERNAL_REALIZATIONS_DERIVED_KEY, ExternalContentKind, ExternalContentManifestEntry,
    ExternalContentManifestEntryKind, ExternalContentManifestObject, ExternalContentMode,
    ExternalContentRealization, ExternalContentRealizationSet, FILE_REALIZATION_ENTRY_PATH,
    MAX_EXTERNAL_CONTENT_ENTRIES, MAX_EXTERNAL_CONTENT_FILE_BYTES,
    MAX_EXTERNAL_CONTENT_MANIFEST_BYTES, MAX_EXTERNAL_CONTENT_PATH_BYTES,
    MAX_EXTERNAL_CONTENT_TOTAL_BYTES, MAX_INLINE_SYMLINK_TARGET_BYTES,
    MAX_INTERNAL_SYMLINK_EXPANSIONS, MAX_REALIZATION_CLAIMED_BYTES, MAX_SYMLINK_TARGET_BYTES,
    validate_internal_symlink_graph, validate_internal_symlink_target,
};
pub use external_large_content_manifest::{
    EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_SCHEMA,
    ExternalLargeContentManifestEntry, ExternalLargeContentManifestObject,
    LARGE_CONTENT_CHUNK_BYTES, MAX_LARGE_CONTENT_CHUNK_BYTES, MAX_LARGE_CONTENT_FILE_BYTES,
    MAX_LARGE_CONTENT_MANIFEST_BYTES, MAX_LARGE_CONTENT_MANIFEST_ENTRIES,
    MAX_LARGE_CONTENT_TOTAL_BYTES, MIN_LARGE_CONTENT_CHUNK_BYTES, load_if_large_content_manifest,
};
pub use item_source::ItemSource;
pub use live_input::{LiveInput, LiveInputIntent};
pub use persistent_session_capsule::{
    AdmittedPersistentSessionCapsule, AdmittedStructuredSessionProfile,
    CredentialSubjectProjectionContract, ExecutableSearchPathEntry,
    MAX_EXECUTABLE_SEARCH_PATH_ENTRIES, MAX_PERSISTENT_SESSION_EXACT_PROGRAM_BYTES,
    MAX_SESSION_PROCESS_ENVIRONMENT_ENCODED_BYTES, MAX_SESSION_PROCESS_ENVIRONMENT_ENTRIES,
    PERSISTENT_SESSION_CAPSULE_KIND, PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
    PersistentSessionAuthority, PersistentSessionLifecycleContract, PersistentSessionWireContract,
    PortableSessionStateClass, PortableSessionStateContract, PortableSessionStateSelector,
    SESSION_PROCESS_ENVIRONMENT_ENV, SessionProcessEnvironmentPathKind,
    SessionProcessEnvironmentValue, validate_session_process_environment,
    validate_session_process_environment_name, validate_session_process_environment_relative_path,
};
pub use placement_runtime_seed::{
    MAX_PLACEMENT_RUNTIME_METADATA_BYTES, PLACEMENT_RUNTIME_SEED_KIND,
    PLACEMENT_RUNTIME_SEED_SCHEMA, PlacementRuntimeSeed,
};
pub use placement_transfer_manifest::{
    PLACEMENT_TRANSFER_MANIFEST_KIND, PLACEMENT_TRANSFER_MANIFEST_SCHEMA, PlacementTransferManifest,
};
pub use portable_state_tree::{
    PORTABLE_STATE_TREE_KIND, PORTABLE_STATE_TREE_MEDIA_TYPE, PORTABLE_STATE_TREE_SCHEMA,
    PortableStateTree, PortableStateTreeFile, classify_portable_state_path,
    selector_matches as portable_state_selector_matches,
};
pub use project_file::ProjectFile;
pub use project_snapshot::ProjectSnapshot;
pub use project_snapshot_policy::ProjectSnapshotPolicy;
pub use project_tree::ProjectTree;
pub use source_closure::{
    EFFECTIVE_SOURCE_BINDING_KIND, EFFECTIVE_SOURCE_BINDING_SCHEMA, EffectiveSourceBinding,
    EffectiveSourceClosureProjection, LogicalSourceRoot, MAX_SOURCE_BINDING_BYTES,
    MAX_SOURCE_FILE_BYTES, MAX_SOURCE_FILES, MAX_SOURCE_MANIFEST_BYTES, MAX_SOURCE_PATH_BYTES,
    MAX_SOURCE_ROOTS, MAX_SOURCE_TOTAL_BYTES, SOURCE_CLOSURE_DERIVED_KEY,
    SOURCE_CLOSURE_MANIFEST_KIND, SOURCE_CLOSURE_MANIFEST_SCHEMA, SignedKindSourceCeiling,
    SourceClosureFile, SourceClosureManifest, SourceClosureTotals, SourceExecutionPolicyIdentity,
    SourceFileMode, SourceLoaderRoot, SourceLogicalBinding, SourceOwnerIdentity,
    SourceRootIdentity, SourceSpaceIdentity, SourceTestimonyProof,
};
pub use source_manifest::SourceManifest;
pub use state_anchor::{
    STATE_ANCHOR_SCHEMA_VERSION, StateAnchorMilestone, StateAnchorPayload, StateAnchorSubject,
};
pub use state_manifest::{
    MAX_STATE_MANIFEST_OBJECTS, STATE_MANIFEST_KIND, STATE_MANIFEST_SCHEMA_VERSION, StateManifest,
    StateManifestBlob,
};
pub use thread_event::{
    ACCOUNTING_ALLOWANCE_TRANSFER_KIND, ACCOUNTING_ALLOWANCE_TRANSFER_SCHEMA,
    AccountingAllowanceTransfer, EventDurability, MAX_HOSTED_SESSION_OBSERVATION_EVENTS,
    MAX_STRUCTURED_OBSERVATION_BATCH_BYTES, MAX_THREAD_EVENT_SERIALIZED_BYTES,
    REMOTE_CONTINUATION_AUTHORITY_SCHEMA, RemoteContinuationAuthority, ThreadEvent,
};
pub use thread_snapshot::{
    CapturedEffectiveTrustClass, CapturedItemSpace, CapturedItemTrustClass,
    CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
    CapturedThreadHistoryMinimumClamp, CapturedThreadHistoryPolicy, MAX_TERMINAL_DURATION_SECONDS,
    MAX_THREAD_RESULT_CONTENT_BYTES, ManagedRuntimeTerminalSupplement,
    THREAD_SNAPSHOT_SCHEMA_VERSION, ThreadHistoryRetention, ThreadSnapshot, ThreadSnapshotBuilder,
    ThreadStatus, ThreadUsage, UsageSubject, parse_canonical_timestamp,
    validate_thread_result_content,
};
pub use worker_session_restore::{
    WORKER_SESSION_RESTORE_CONTRACT, WORKER_SESSION_RESTORE_KIND, WORKER_SESSION_RESTORE_SCHEMA,
    WorkerSessionCheckpointPosition, WorkerSessionDependencyRestore,
    WorkerSessionPortableStateRestore, WorkerSessionRestore,
};

/// Schema version shared across all CAS object types.
/// Bump when the object format changes in a incompatible way.
pub const SCHEMA_VERSION: u32 = 1;

/// Typed clean-cut rejection for an immutable CAS object whose outer schema
/// differs from the only contract this release is allowed to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompatibleCurrentObjectSchema {
    object: &'static str,
    stored: u64,
    current: u32,
}

impl IncompatibleCurrentObjectSchema {
    pub fn new(object: &'static str, stored: u64, current: u32) -> Self {
        Self {
            object,
            stored,
            current,
        }
    }

    pub fn is_predecessor(&self) -> bool {
        self.stored < u64::from(self.current)
    }
}

impl std::fmt::Display for IncompatibleCurrentObjectSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} is not the exact current contract: stored schema={}, current schema={}",
            self.object, self.stored, self.current
        )
    }
}

impl std::error::Error for IncompatibleCurrentObjectSchema {}

/// Validate the canonical, contained project-relative path used as the source
/// manifest key and embedded `ItemSource.item_ref`. These fields identify
/// files, not executable item kinds: they deliberately remain kind-agnostic.
pub fn validate_canonical_project_relative_path(value: &str) -> anyhow::Result<()> {
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
