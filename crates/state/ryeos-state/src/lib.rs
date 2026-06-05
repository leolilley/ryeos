//! ryeos-state — CAS-as-truth persistence plane for RyeOS.
//!
//! This crate owns the answer to "what happened and how to prove it."
//! It provides CAS object type definitions, chain operations, signed ref
//! management, and will eventually include SQLite projection and sync.
//!
//! The crate does NOT own the signing key (daemon passes a [`Signer`] trait),
//! item resolution (engine), or low-level CAS primitives (lillux).

pub mod admission;
pub mod chain;
pub mod domain_events;
pub mod domain_outbox;
pub mod domain_projection;
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
pub mod user_sync;
pub mod verify;

pub use admission::{admit_root, AdmissionRequest, AdmissionResult};
pub use chain::{AppendResult, CreateResult, ReadSnapshotResult, SnapshotUpdate};
pub use domain_events::{DomainEventAppendRequest, DomainEventAppendResult, DomainEventRecord};
pub use domain_outbox::{
    claim_domain_outbox_messages, enqueue_domain_outbox_message, ensure_domain_outbox_schema,
    get_domain_outbox_message, mark_domain_outbox_delivered, mark_domain_outbox_failed,
    DomainOutboxMessage,
};
pub use domain_projection::{
    DomainProjectionCursor, DomainProjectionDb, DomainProjectionSyncReport,
};
pub use head_cache::{CachedHead, HeadCache};
pub use locators::ThreadLocator;
pub use objects::{
    thread_event::ThreadEvent, thread_snapshot::ThreadSnapshot, thread_snapshot::ThreadUsage,
    Attestation, ChainState, DomainEventAttribution, DomainEventObject,
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
