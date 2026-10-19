//! ryeos-state — CAS-as-truth persistence plane for RyeOS.
//!
//! This crate owns the answer to "what happened and how to prove it."
//! It provides CAS object type definitions, chain operations, signed ref
//! management, and will eventually include SQLite projection and sync.
//!
//! The crate does NOT own the signing key (daemon passes a [`Signer`] trait),
//! item resolution (engine), or low-level CAS primitives (lillux).

/// Owner token carried by the secure temporaries this crate publishes.
pub const STATE_TEMP_PURPOSE: lillux::TempPurpose = lillux::TempPurpose::new("state");

pub mod admission;
pub mod bundle_events;
pub mod bundle_outbox;
pub mod capability;
pub mod chain;
pub mod event_types;
pub mod external_content;
pub mod external_execution;
pub mod gc;
pub mod head_cache;
pub mod ignore;
pub mod immutable_testimony;
pub mod large_object_store;
pub mod locators;
pub mod object_closure;
pub mod objects;
pub mod operational;
mod pending_publication;
pub mod private_cas;
pub mod project_discovery;
pub mod project_materialization;
pub mod project_observation;
pub mod project_sync;
pub mod projection;
pub mod provider_call_observation;
pub mod queries;
pub mod reachability;
pub mod rebuild;
pub mod recovery;
pub mod refs;
pub mod signer;
pub mod source_verification;
pub mod sqlite_schema;
pub mod state_db;
pub mod sync;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

/// Whether an owner recorded in process `process` is only a copy here: this
/// is a forked child, supervised or not, or another process altogether. Reads
/// one atomic and the process id; no lock and no allocation.
pub(crate) fn foreign_process(process: u32) -> bool {
    lillux::exec::in_forked_child() || process != std::process::id()
}
pub mod verify;

pub use admission::{AdmissionRequest, AdmissionResult, admit_root};
pub use bundle_events::{
    BundleEventAppendRequest, BundleEventAppendResult, BundleEventChainPage, BundleEventCursor,
    BundleEventRecord, BundleEventScanPage,
};
pub use bundle_outbox::{
    BundleOutboxMessage, claim_bundle_outbox_messages, enqueue_bundle_outbox_message,
    ensure_bundle_outbox_schema, get_bundle_outbox_message, mark_bundle_outbox_delivered,
    mark_bundle_outbox_failed,
};
pub use chain::{AppendResult, CreateResult, ReadSnapshotResult, SnapshotUpdate};
pub use external_content::{
    DigestOnlyExternalContentSink, ExternalCapturePolicy, ExternalContentBlobSink,
    ExternalContentCaptureKind, ExternalLargeContentSink, LargeContentCaptureBounds,
    LargeContentCapturePolicy, LaunchCaptureBudget, MAX_CAPTURE_BYTES, MAX_CAPTURE_DEPTH,
    MAX_CAPTURE_ENTRIES, MAX_CAPTURE_FILE_BYTES, VerifiedExternalContentClosure,
    capture_external_content_at, capture_file_at, capture_large_file, capture_large_tree,
    capture_tree, capture_tree_exact, external_content_manifest_digest,
    observe_external_content_tree_exact,
};
pub use head_cache::{CachedHead, HeadCache};
pub use large_object_store::{
    IngestedLargeObject, LargeObjectIntegrityFinding, LargeObjectScrubReport,
    LargeObjectStagingSweepReport, LargeObjectStore, LargeObjectSweepReport, LeasedLargeObject,
    PinnedLargeObjectSourceIdentity,
};
pub use locators::ThreadLocator;
pub use objects::{
    Attestation, BundleEventAttachment, BundleEventAttribution, BundleEventObject,
    CapturedEffectiveTrustClass, CapturedItemSpace, CapturedItemTrustClass,
    CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
    CapturedThreadHistoryMinimumClamp, CapturedThreadHistoryPolicy, ChainState, EvidenceCoordinate,
    FencedChainKinds, IncompatibleCurrentObjectSchema, MAX_RETIRED_CHAIN_CLOSURE_PAGE_BYTES,
    MAX_RETIRED_CHAIN_CLOSURE_PAGE_ENTRIES, MAX_TERMINAL_DURATION_SECONDS, PrefixTip,
    RETIRED_CHAIN_CLOSURE_PAGE_KIND, RETIRED_CHAIN_MANIFEST_KIND, RetiredChainClosurePage,
    RetiredChainManifest, ThreadHistoryRetention, parse_canonical_timestamp,
    thread_event::ThreadEvent, thread_snapshot::ThreadSnapshot, thread_snapshot::ThreadUsage,
    thread_snapshot::UsageSubject,
};
pub use operational::{
    AdmissionAttestationRecord, AdmissionAttestationState, CasEntriesByStateSummary,
    CasEntryAttribution, CasEntryKind, CasEntryState, ClosedOriginalOperational,
    FinishSyncJobAttempt, NewAdmissionAttestationRecord, NewCasEntryAttribution, NewSyncJob,
    NewSyncJobAttempt, OperationalCredentialProfileRecord, OperationalDb,
    OriginalOperationalFailure, OriginalOperationalMutationAuthority, PreparedReplayIndexReset,
    ReplayIndexActivationRequired, ReplayIndexNamespace, ReplayIndexRecord, ReplayLookupOutcome,
    ReplayPublishOutcome, ReplayRecordVerification, SYNC_JOB_UNBOUNDED_ATTEMPTS,
    SYNC_JOB_UNBOUNDED_RETAINED_TERMINAL_ATTEMPTS, SyncJobAttemptRecord, SyncJobAttemptState,
    SyncJobRecord, SyncJobState, SyncJobUpdate, sync_job_attempt_count_is_valid,
    sync_job_attempts_are_unbounded, sync_job_attempts_exhausted,
};
pub use pending_publication::PendingCasPublication;
pub use project_materialization::PinnedProjectMaterialization;
pub use project_observation::{
    MAX_PROJECT_OBSERVATION_JSON_DEPTH, MAX_PROJECT_OBSERVATION_JSON_VALUES,
    MAX_PROJECT_OBSERVATION_NAMESPACE_BYTES, MAX_PROJECT_OBSERVATION_PAYLOAD_BYTES,
    MAX_PROJECT_OBSERVATION_STABLE_ID_BYTES, MAX_PROJECT_OBSERVATIONS_PER_ACTION,
    PROJECT_OBSERVATION_SCHEMA, ProjectObservationOccurrence, ProjectObservationRecordedPayload,
    ProjectObservationRequest, project_observation_id,
};
pub use projection::{
    ChainRetentionProjection, DueTerminalChain, DueTerminalChainCursor, MAX_DUE_FENCED_CHAIN_SLICE,
    ProjectionDb,
};
pub use provider_call_observation::{
    PROVIDER_CALL_OBSERVATION_SCHEMA, ProviderCallObservationDraft,
    ProviderCallObservationPublication, ProviderCallObservationRecordedPayload,
    ProviderCallObservationSource, ProviderCallReplaySource, provider_call_observation_id,
};
pub use recovery::{
    CasMutationGuard, DurableCasPublicationKey, DurableCasUploadStage,
    DurableExternalCandidateReceipt, EXECUTION_HISTORY_SCRATCH_REPAIR, EvidenceValidator,
    HeadOperation, PendingChainHeadTransition, PendingTransitionCursor,
    ProjectionRecoveryGeneration, RECOVERY_PROTOCOL_GENERATION, RecoveryStore, RetiredChainClosure,
    RetiredChainClosureDraft, RetiredChainFence, RetiredThreadHistoryDiscardMarker,
    RetirementEvidenceDiscard, StagedCasRootHashes, StagedCasRootLease,
    ThreadHistoryDiscardAcknowledgementRefused, ThreadHistoryDiscardOperationRef, TransitionPhase,
    read_retired_chain_closure,
};
pub use refs::{GenericHeadRef, SignedRef, TrustStore, verify_signed_ref};
pub use signer::Signer;
pub use state_db::LiveStateDatabaseOpenFailure;
pub use state_db::{
    AddThreadWithEventSuccessorRequest, AnchoredThreadAppend,
    AuthoritativeContinuationPredecessorReadback, AuthoritativeRootBirthReadback,
    AuthoritativeRootGenesisReadback, AuthoritativeRootThreadReadback,
    AuthoritativeRuntimePinAdmission, AuthoritativeTerminalChain,
    AuthoritativeThreadAppendAnchorReadback, AuthoritativeThreadAppendHistoryReadback,
    AuthoritativeThreadEventChainReadback, AuthoritativeThreadEventReadAnchor,
    AuthoritativeThreadEventReadPage, AuthoritativeThreadEventReadProgress,
    AuthoritativeThreadHistoryDiscardProgress, AuthoritativeThreadHistoryDiscardReport,
    AuthoritativeThreadPairReadback, AuthoritativeThreadSnapshotReadback,
    AuthoritativeThreadSnapshotWithLastEventReadback, CommittedWrite,
    ConditionalThreadEventAppendResult, CreateChainWithEventSuccessorRequest, MixedCutDependencies,
    PendingRemoveHeadState, PendingReplayReport, PinnedStateAuthority, PreparedOfflineProjection,
    ProjectionRecoveryObserver, ProjectionRecoveryProgress, ProjectionRecoveryStage,
    ProjectionRepairRequest, ProjectionRepairSink, ProjectionStatus, RetiredChainDependency,
    RuntimeLivenessInspector, StateDb, TwoAnchorAdmissionView, TwoAnchorThreadAppend,
};
pub use sync::ImportAttribution;
