//! ryeos-state — CAS-as-truth persistence plane for RyeOS.
//!
//! This crate owns the answer to "what happened and how to prove it."
//! It provides CAS object type definitions, chain operations, signed ref
//! management, and will eventually include SQLite projection and sync.
//!
//! The crate does NOT own the signing key (daemon passes a [`Signer`] trait),
//! item resolution (engine), or low-level CAS primitives (lillux).

pub mod admission;
pub mod bundle_events;
pub mod bundle_outbox;
pub mod bundle_projection;
pub mod chain;
pub mod gc;
pub mod head_cache;
pub mod ignore;
pub mod locators;
pub mod object_closure;
pub mod objects;
pub mod project_discovery;
pub mod project_sync;
pub mod projection;
pub mod queries;
pub mod reachability;
pub mod rebuild;
pub mod refs;
pub mod signer;
pub mod sqlite_schema;
pub mod state_db;
pub mod sync;
pub mod verify;

pub use admission::{admit_root, AdmissionRequest, AdmissionResult};
pub use bundle_events::{BundleEventAppendRequest, BundleEventAppendResult, BundleEventRecord};
pub use bundle_outbox::{
    claim_bundle_outbox_messages, enqueue_bundle_outbox_message, ensure_bundle_outbox_schema,
    get_bundle_outbox_message, mark_bundle_outbox_delivered, mark_bundle_outbox_failed,
    BundleOutboxMessage,
};
pub use bundle_projection::{
    BundleProjectionCursor, BundleProjectionDb, BundleProjectionSyncReport,
};
pub use chain::{AppendResult, CreateResult, ReadSnapshotResult, SnapshotUpdate};
pub use head_cache::{CachedHead, HeadCache};
pub use locators::ThreadLocator;
pub use objects::{
    thread_event::ThreadEvent, thread_snapshot::ThreadSnapshot, thread_snapshot::ThreadUsage,
    thread_snapshot::UsageSubject, Attestation, BundleEventAttribution, BundleEventObject,
    ChainState,
};
pub use projection::{
    AdmissionAttestationRecord, AdmissionAttestationState, CasEntriesByStateSummary,
    CasEntryAttribution, CasEntryKind, CasEntryState, FinishSyncJobAttempt,
    NewAdmissionAttestationRecord, NewCasEntryAttribution, NewSyncJob, NewSyncJobAttempt,
    ProjectionDb, SyncJobAttemptRecord, SyncJobAttemptState, SyncJobRecord, SyncJobState,
    SyncJobUpdate,
};
pub use refs::{verify_signed_ref, GenericHeadRef, SignedRef, TrustStore};
pub use signer::Signer;
pub use state_db::StateDb;
pub use sync::ImportAttribution;
