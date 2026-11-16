use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_runtime::RuntimeJsonArrayBudget;
use ryeos_runtime::checkpoint::{checkpoint_shape_limits, validate_checkpoint_shape};
use ryeos_state::AddThreadWithEventSuccessorRequest;
use ryeos_state::CreateChainWithEventSuccessorRequest;
use ryeos_state::StateDb;
use ryeos_state::UsageSubject;
use ryeos_state::chain::{ChainLock, SnapshotUpdate};
use ryeos_state::objects::thread_snapshot::{ThreadStatus, parse_canonical_timestamp};
use ryeos_state::objects::{StateManifest, StateManifestBlob, ThreadSnapshot, ThreadUsage};
use ryeos_state::queries;
use ryeos_state::signer::Signer;

use crate::projection_health::ThreadProjectionHealth;
use crate::runtime_db;
use crate::write_barrier::{WriteBarrier, WritePermit};
pub use runtime_db::{
    CommandRecord, CredentialProfileRecord, CredentialProfileReservationRecord,
    DedicatedCandidateDisposition, DedicatedSessionApprovalRecord, DedicatedSessionCommandRecord,
    DedicatedSessionRecord, HookDispatchReservation, LaunchPlanningAlreadyReserved,
    LaunchPlanningCapacityExceeded, LaunchPlanningRecord, NewCommandRecord, NewCredentialProfile,
    NewCredentialProfileReservation, NewDedicatedSession, NewDedicatedSessionApproval,
    NewDedicatedSessionCommand, NewHookDispatch, ObservationBatchReservation, ProcessQuarantine,
    ProcessQuarantineReason, QuarantinedProcess, RuntimeInfo, StopIntent, WorkerProcessRecord,
};

mod live_destination_join;
mod projection_access;
pub use live_destination_join::{
    quote_store_leaf_transfers, quote_store_parent_transfer, quote_store_receiver,
};
mod live_outside;
pub use live_outside::{
    LiveStateStoreOutsideCustody, LiveStateStoreOutsideSource,
    MAX_LIVE_ACCOUNTING_NATIVE_PACKET_SOURCES, MAX_LIVE_AUXILIARY_NATIVE_PACKET_SOURCES,
    MAX_LIVE_SCHEDULER_NATIVE_PACKET_SOURCES, MAX_LIVE_SELECTED_NATIVE_PACKET_SOURCES,
    StoreExhausted, quote_destination_retries, quote_packet_metadata_pin_retirements,
    quote_packet_parts, quote_retired_packet_metadata_pin_receipts,
    quote_retired_packet_source_parts,
};
mod live_close;
pub use live_close::{ClosedLiveStateStore, LiveStateStoreCloseFailure, StateStoreCensus};
mod live_native;
pub use live_native::{
    LiveAccountingNativeAdmission, LiveSchedulerNativeAdmission,
    LiveSchedulerNativeAdmissionFailure, LiveSchedulerNativeAdmissionOutsideSources,
    LiveStateStoreOpenFailure, LiveStateStoreStartupRecoveryFailure, ReleasedLiveStateStoreStartup,
    abandoned_state_store_openings, quote_store_child_parent_alias_retirement,
    quote_store_completed_parent_root_membership, quote_store_destination_join_preparation,
    quote_store_direct_operational_handoff, quote_store_operator_lock_membership,
    quote_store_operator_lock_registration, quote_store_packet_vector_handoff,
    quote_store_parent_alias_retirement, quote_store_parent_authority_handoff,
    quote_store_parent_facade_retirement, quote_store_parent_finalization_parts,
    quote_store_parent_root_membership, quote_store_physical_parent,
    quote_store_root_registration_handoff,
};
mod scoped_child;
#[cfg(any(test, feature = "test-support"))]
pub mod test_native;

mod types;
pub use types::*;
mod terminal_envelope;
pub(crate) use terminal_envelope::*;
mod commit_readback;
use commit_readback::*;
mod validation;
use validation::*;
mod runtime_files;
pub use runtime_files::*;
mod lifecycle;
pub use lifecycle::*;
mod in_process_handlers;
pub use in_process_handlers::*;
mod launch_capsule;
use launch_capsule::*;
mod authoritative_reads;
pub use authoritative_reads::*;
mod credential_profiles;
use credential_profiles::*;
mod dedicated_candidates;
mod dedicated_sessions;
mod process_resources;
pub(crate) use process_resources::*;
mod process_attachment;
use process_attachment::*;
mod worker_handoff;
mod workspaces;
use worker_handoff::*;
mod remote_adoption;
pub use remote_adoption::*;
mod thread_create;
use thread_create::*;
mod thread_finalize;
pub use thread_finalize::*;
mod continuation;
use continuation::*;
mod launch_planning;
pub use launch_planning::*;
mod chain_retirement;
mod seat_leases;
mod thread_queries;
use thread_queries::*;
mod external_allocation;
mod publications;
mod selected_metadata;
pub(crate) use external_allocation::*;
mod external_protocol;
mod external_qualification;
mod external_snapshot_ops;
#[cfg(any(test, feature = "test-support"))]
mod external_test_fixtures;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use external_test_fixtures::*;
mod commands_windows;
mod events;
mod follow;
mod runtime_actions;

use projection_access::committed_value;

const MAX_THREAD_ARTIFACT_ITEMS: usize = 512;
const MAX_THREAD_ARTIFACT_TYPE_BYTES: usize = 1024;
const MAX_THREAD_ARTIFACT_METADATA_BYTES: usize = 256 * 1024;
const MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_THREAD_ARTIFACT_RESPONSE_BYTES: usize = 6 * 1024 * 1024;
const MAX_THREAD_FACET_ITEMS: usize = 128;
const MAX_THREAD_FACET_KEY_BYTES: usize = 4 * 1024;
const MAX_THREAD_FACET_VALUE_BYTES: usize = 256 * 1024;
const MAX_THREAD_FACET_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_THREAD_FACET_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_THREAD_LIST_ENRICHMENT_THREADS: usize = 2_000;
const MAX_THREAD_LIST_FACET_ITEMS: usize = 8 * 1024;
const MAX_THREAD_LIST_FACET_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_THREAD_LIST_FACET_RESPONSE_BYTES: usize = 6 * 1024 * 1024;
const MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_THREAD_LIST_ERROR_PREVIEW_BYTES: usize = 2 * 1024;
const MAX_ACTIVE_LAUNCH_SIGNALS: usize = 4_096;
/// Exact JSON budget for the response-facing thread result record. Terminal
/// content remains within the state-owned aggregate ceiling; the remainder
/// covers the fixed record keys and bounded outcome code.
const MAX_THREAD_RESULT_RESPONSE_BYTES: usize =
    ryeos_state::objects::MAX_THREAD_RESULT_CONTENT_BYTES + 64 * 1024;
const MAX_TERMINAL_EVENT_INLINE_ERROR_BYTES: usize = 64 * 1024;
const MAX_STATE_RESTORE_BYTES: usize = 256 * 1024;
const ACTIVE_WORKER_HANDOFF_SCAN_PAGE: usize = 64;
// A portable-state tree stores file bytes as canonical base64.  The signed
// selector contract currently permits at most 16 MiB of raw selected state,
// so retain enough room for base64 expansion plus bounded tree metadata.
const MAX_STATE_MANIFEST_INPUT_BYTES: usize = 24 * 1024 * 1024;
const MAX_STATE_MANIFEST_TOTAL_INPUT_BYTES: usize = 24 * 1024 * 1024;
const MAX_STATE_ANCHOR_METADATA_BYTES: usize = 192 * 1024;

struct Inner {
    // Exact live owners share the Store linearization point with durable claims.
    active_launch_owners: HashSet<String>,
    state_db: StateDb,
    runtime_db: runtime_db::RuntimeDb,
    signer: Arc<dyn Signer>,
}

/// StateStore guard. Holding it never closes the node's fork window: the
/// chain, head, large-object and CAS locks taken beneath the store mutex are
/// closed in every direct-attachment fork child before its pre-exec hold and
/// release explicitly on drop, so a long hold here stalls no process creation.
struct StateStoreGuard<'a> {
    store_mutex: &'a Mutex<Inner>,
    inner: std::sync::MutexGuard<'a, Inner>,
    acquired_at: lillux::time::MonotonicTimer,
    caller: &'static std::panic::Location<'static>,
}

/// Only StateStore can mint this capability while its exact guard is held.
/// No registry snapshot, raw connection or caller generation is authority.
///
/// ```compile_fail
/// let permit = ryeos_app::state_store::SelectedMutationPermit::default();
/// ```
/// ```compile_fail
/// let authority = ryeos_app::execution_history_reset::OfflineResetAuthority::default();
/// ```
pub(crate) struct SelectedMutationPermit<'guard, 'store> {
    guard: &'guard StateStoreGuard<'store>,
    db: &'guard runtime_db::RuntimeDb,
    operation: SelectedMutationOperation<'guard>,
}

#[derive(PartialEq, Eq)]
enum SelectedMutationOperation<'a> {
    Claim(&'a str, &'a str, &'a str),
    Release(&'a str, &'a str),
    Rotate(&'a str, &'a str, &'a str, &'a str, u32),
    ClearStale(&'a str),
    DeleteThread(&'a str),
    DiscardBirth(&'a str),
    DeleteChain(&'a str, &'a [String]),
}

impl SelectedMutationPermit<'_, '_> {
    fn authorize(
        &self,
        db: &runtime_db::RuntimeDb,
        operation: SelectedMutationOperation<'_>,
    ) -> Result<()> {
        if !std::ptr::eq(db, self.db)
            || !std::ptr::eq(self.db, &self.guard.runtime_db)
            || self.operation != operation
        {
            bail!("selected mutation permit target or operation changed");
        }
        Ok(())
    }

    pub(crate) fn authorize_claim(
        &self,
        db: &runtime_db::RuntimeDb,
        thread: &str,
        claim: &str,
        owner: &str,
    ) -> Result<()> {
        self.authorize(db, SelectedMutationOperation::Claim(thread, claim, owner))
    }
    pub(crate) fn authorize_release(
        &self,
        db: &runtime_db::RuntimeDb,
        thread: &str,
        claim: &str,
    ) -> Result<()> {
        self.authorize(db, SelectedMutationOperation::Release(thread, claim))
    }
    pub(crate) fn authorize_rotate(
        &self,
        db: &runtime_db::RuntimeDb,
        thread: &str,
        owner: &str,
        claim: &str,
        generation: &str,
        attempts: u32,
    ) -> Result<()> {
        self.authorize(
            db,
            SelectedMutationOperation::Rotate(thread, owner, claim, generation, attempts),
        )
    }
    pub(crate) fn authorize_sweep(
        &self,
        db: &runtime_db::RuntimeDb,
        generation: &str,
    ) -> Result<()> {
        self.authorize(db, SelectedMutationOperation::ClearStale(generation))
    }
    pub(crate) fn authorize_delete_thread(
        &self,
        db: &runtime_db::RuntimeDb,
        thread: &str,
    ) -> Result<()> {
        self.authorize(db, SelectedMutationOperation::DeleteThread(thread))
    }
    pub(crate) fn authorize_discard_birth(
        &self,
        db: &runtime_db::RuntimeDb,
        thread: &str,
    ) -> Result<()> {
        self.authorize(db, SelectedMutationOperation::DiscardBirth(thread))
    }
    pub(crate) fn authorize_delete_chain(
        &self,
        db: &runtime_db::RuntimeDb,
        root: &str,
        threads: &[String],
    ) -> Result<()> {
        self.authorize(db, SelectedMutationOperation::DeleteChain(root, threads))
    }
}

impl std::ops::Deref for StateStoreGuard<'_> {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for StateStoreGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for StateStoreGuard<'_> {
    fn drop(&mut self) {
        let held = self.acquired_at.elapsed();
        if held >= std::time::Duration::from_millis(100) {
            record_store_contention(StoreContentionKind::Hold, held, self.caller);
        }
    }
}

#[derive(Clone, Copy)]
enum StoreContentionKind {
    Wait,
    Hold,
}

struct StoreContentionCounters {
    count: std::sync::atomic::AtomicU64,
    total_ms: std::sync::atomic::AtomicU64,
    max_ms: std::sync::atomic::AtomicU64,
    last_report_ms: std::sync::atomic::AtomicU64,
}

impl StoreContentionCounters {
    const fn new() -> Self {
        Self {
            count: std::sync::atomic::AtomicU64::new(0),
            total_ms: std::sync::atomic::AtomicU64::new(0),
            max_ms: std::sync::atomic::AtomicU64::new(0),
            last_report_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

static STORE_WAIT_CONTENTION: StoreContentionCounters = StoreContentionCounters::new();
static STORE_HOLD_CONTENTION: StoreContentionCounters = StoreContentionCounters::new();

fn contention_monotonic_millis() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    u64::try_from(
        EPOCH
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Aggregate lock pressure into one bounded diagnostic per interval. Emitting
/// one warning for every delayed callback turned overload into additional
/// synchronous stderr/NDJSON work and obscured the actual queue shape.
fn record_store_contention(
    kind: StoreContentionKind,
    duration: std::time::Duration,
    caller: &'static std::panic::Location<'static>,
) {
    use std::sync::atomic::Ordering;

    const REPORT_INTERVAL_MS: u64 = 5_000;
    let counters = match kind {
        StoreContentionKind::Wait => &STORE_WAIT_CONTENTION,
        StoreContentionKind::Hold => &STORE_HOLD_CONTENTION,
    };
    let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    counters.count.fetch_add(1, Ordering::Relaxed);
    counters.total_ms.fetch_add(duration_ms, Ordering::Relaxed);
    counters.max_ms.fetch_max(duration_ms, Ordering::Relaxed);

    let now_ms = contention_monotonic_millis();
    let previous = counters.last_report_ms.load(Ordering::Relaxed);
    let report_at = now_ms.max(1);
    if (previous != 0 && now_ms.saturating_sub(previous) < REPORT_INTERVAL_MS)
        || counters
            .last_report_ms
            .compare_exchange(previous, report_at, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
    {
        return;
    }

    let count = counters.count.swap(0, Ordering::AcqRel);
    let total_ms = counters.total_ms.swap(0, Ordering::AcqRel);
    let max_ms = counters.max_ms.swap(0, Ordering::AcqRel);
    let average_ms = total_ms.checked_div(count).unwrap_or(0);
    tracing::warn!(
        contention = match kind {
            StoreContentionKind::Wait => "wait",
            StoreContentionKind::Hold => "hold",
        },
        samples = count,
        total_ms,
        average_ms,
        max_ms,
        latest_caller_file = caller.file(),
        latest_caller_line = caller.line(),
        latest_caller_column = caller.column(),
        "StateStore contention summary"
    );
}

/// Run a short, potentially blocking synchronization boundary without parking
/// one of Tokio's asynchronous worker threads.
///
/// StateStore remains a synchronous authority used by startup, offline tools,
/// tests, and blocking workers as well as by daemon request tasks.  A plain
/// `std::sync::Mutex::lock` is correct in all of those contexts, but on a
/// multi-thread Tokio worker it can starve unrelated lifecycle and HTTP work
/// when many runtime callbacks queue behind the store at once.  `block_in_place`
/// lets Tokio replace that worker while preserving the mutex's existing exact
/// ordering and linearization point.  Current-thread runtimes cannot perform
/// that handoff, so they retain the ordinary synchronous behavior.
fn cooperate_while_blocking<T>(operation: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(operation)
        }
        _ => operation(),
    }
}

enum SelectedMetadataAccess {
    Start,
    Return,
}

/// Current signer trust that admits installed Bundles and their native
/// executors at launch. Retained Bundle member-source proofs are revalidated
/// against this authority, never against head trust, which authenticates only
/// state-chain and execution-realization signatures. A store opened without
/// this binding refuses every retained Bundle executor.
#[derive(Clone, Debug)]
pub struct BundleAdmissionTrust(Option<Arc<ryeos_engine::trust::TrustStore>>);

impl BundleAdmissionTrust {
    /// Bind the exact node trust snapshot the launch path admits Bundles with.
    pub fn bound(node_trust: ryeos_engine::trust::TrustStore) -> Self {
        Self(Some(Arc::new(node_trust)))
    }

    /// No admitting trust: offline and fixture stores.
    pub fn unbound() -> Self {
        Self(None)
    }

    fn require(&self) -> Result<&ryeos_engine::trust::TrustStore> {
        self.0.as_deref().ok_or_else(|| {
            anyhow!("this StateStore has no Bundle admission trust; retained Bundle executors are refused")
        })
    }
}

pub struct StateStore {
    live_native_open: Option<Arc<live_native::LiveStateOpen>>,
    state_authority: ryeos_state::PinnedStateAuthority,
    bundle_admission_trust: BundleAdmissionTrust,
    thread_runtime_authority: Option<ThreadRuntimeAuthority>,
    inner: Mutex<Inner>,
    projection_health: Arc<ThreadProjectionHealth>,
    read_only: bool,
    allow_projection_rebuild: bool,
    /// Kept outside the state mutex so lock order is always write permit then
    /// StateStore mutex (never a mutex probe followed by permit acquisition).
    write_barrier: WriteBarrier,
    process_attachment_admission_open: AtomicBool,
    external_guest_owner_reports: Mutex<
        std::collections::BTreeMap<
            String,
            ryeos_external_execution_contract::guest_owner_report::GuestOwnerActivationReport,
        >,
    >,
    process_attachment_admission_closed: tokio::sync::Notify,
    /// Kinds whose terminal chains retire only through a fence, installed once
    /// by the composition that declares them. The ordinary retention path
    /// leaves their chains alone; the fenced path refuses without this set.
    fenced_chain_kinds: std::sync::OnceLock<ryeos_state::FencedChainKinds>,
    /// Exact process attachments currently recorded in durable runtime state.
    ///
    /// Durable `thread_runtime.process_identity` rows remain authoritative and
    /// seed this set at open. Keeping their live projection outside the global
    /// store mutex prevents periodic recovery and shutdown audits from scanning
    /// the accumulated runtime table while blocking unrelated chain commits.
    /// Attach and clear update it at the same existing StateStore linearization
    /// points as the durable row. Each entry keeps the exact identity and
    /// cancellation policy committed with it, so a stop whose service SQL is
    /// refused can still prove the process dead without reading the database.
    attached_processes: Mutex<BTreeMap<String, AttachedProcess>>,
    /// Dedicated worker processes recorded live in durable runtime state,
    /// keyed by worker instance. Inserted after the durable attach commits and
    /// removed after the settle or fence commits, so it never claims a process
    /// the durable rows do not; it may briefly hold one already settled.
    live_workers: Mutex<BTreeMap<String, runtime_db::WorkerProcessRecord>>,
    /// Shared across all AppState clones; metadata owns no native resources.
    /// Active-owner data lives inside Inner under the same Store mutex.
    selected_invocations:
        Mutex<crate::selected_invocation_bookkeeping::SelectedInvocationBookkeeping>,
    /// Process-local cancellation signals for durable, still-unbound launch
    /// admissions. Persisted planning state remains the authority; this map
    /// only stops work promptly after that state commits cancelled.
    launch_task_abort_handles: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// Recorded in-process handlers currently owned by this daemon. Durable
    /// launch metadata identifies these rows across restart; this volatile map
    /// proves current ownership and gives cancellation/shutdown one exact task
    /// control instead of terminalizing a row while its handler keeps running.
    active_in_process_handlers: Mutex<HashMap<String, InProcessHandlerControl>>,
}

/// Enforces the global mutation order for every StateStore write: the
/// cross-process CAS/GC guard is acquired before the daemon write permit and
/// both remain held until the operation has released its store/chain locks.
struct StateMutationPermit {
    cas_guard: ryeos_state::CasMutationGuard,
    _write_permit: WritePermit,
}

impl StateMutationPermit {
    fn cas_guard(&self) -> &ryeos_state::CasMutationGuard {
        &self.cas_guard
    }
}

impl std::fmt::Debug for StateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateStore")
            .field("inner", &"<Mutex<Inner>>")
            .finish()
    }
}

impl StateStore {
    pub fn is_launch_owner_active(&self, launch_owner: &str) -> bool {
        self.lock()
            .map(|g| Self::is_launch_owner_active_locked(&g, launch_owner))
            .unwrap_or(false)
    }

    // Callers already holding Store must not acquire it recursively.
    fn is_launch_owner_active_locked(g: &Inner, launch_owner: &str) -> bool {
        g.active_launch_owners.contains(launch_owner)
    }

    /// Persist a bounded post-exit settlement observation only while the
    /// exact current-daemon launch owner remains registered and claimed.
    pub fn observe_active_owner_dead_process(
        &self,
        thread_id: &str,
        claim_id: &str,
        launch_owner: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        observed_at_ms: i64,
    ) -> Result<Option<i64>> {
        let g = self.lock()?;
        let claim = g.runtime_db.get_launch_claim(thread_id)?;
        if !claim.as_ref().is_some_and(|claim| {
            claim.claim_id == claim_id
                && claim.claimed_by == launch_owner
                && Self::is_launch_owner_active_locked(&g, launch_owner)
        }) {
            return Ok(None);
        }
        g.runtime_db
            .observe_dead_process_if_matches(thread_id, process_identity, observed_at_ms)
    }

    /// Get the CAS root path for raw CAS access.
    pub fn cas_root(&self) -> Result<std::path::PathBuf> {
        Ok(self.state_authority.cas_directory().path().to_path_buf())
    }

    /// Get the refs root path for ref system access.
    pub fn refs_root(&self) -> Result<std::path::PathBuf> {
        Ok(self.state_authority.refs_directory().path().to_path_buf())
    }

    /// Duplicate the descriptor-pinned state authority without entering the
    /// serialized database boundary. The authority is immutable for the life
    /// of this store and is safe to use for CAS verification outside the
    /// StateStore mutex.
    pub fn pinned_state_authority(&self) -> Result<ryeos_state::PinnedStateAuthority> {
        self.state_authority.try_clone()
    }

    /// Private controller-owned staging root for secret-bearing external guest
    /// packages. It is rooted in the already-pinned runtime state generation,
    /// never in project content, a worker workspace, or ambient temporary
    /// storage. Each package producer creates its own unique private child.
    pub(crate) fn external_guest_package_parent(&self) -> Result<lillux::PinnedDirectory> {
        anyhow::ensure!(
            !self.read_only,
            "read-only state cannot stage external guests"
        );
        let parent = self
            .state_authority
            .runtime_directory()
            .open_or_create_child(std::ffi::OsStr::new("external-guest-packages"), 0o700)?;
        parent.require_owner_private_directory()?;
        Ok(parent)
    }

    /// Private one-use Q input namespace beneath the original pinned state
    /// generation. A child is created only after its placement-keyed intent
    /// is durable; the parent path cannot reconstruct a lost child owner.
    pub(crate) fn qualification_guest_scratch_parent(&self) -> Result<lillux::PinnedDirectory> {
        anyhow::ensure!(
            !self.read_only,
            "read-only state cannot create Q guest scratch"
        );
        let parent = self
            .state_authority
            .runtime_directory()
            .open_or_create_child(std::ffi::OsStr::new("qualification-guest-inputs"), 0o700)?;
        parent.require_owner_private_directory()?;
        Ok(parent)
    }

    /// Resolve current admitted persistent-session authority from the exact
    /// pinned CAS generation. Callers never reopen a project or source path.
    pub fn admitted_persistent_session_capsule(
        &self,
        capsule_hash: &str,
    ) -> Result<ryeos_state::objects::AdmittedPersistentSessionCapsule> {
        if !lillux::valid_hash(capsule_hash) {
            bail!("admitted session capsule hash is not canonical");
        }
        let authority = self.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        authority.ensure_guard(&guard)?;
        let value = authority
            .cas_store()?
            .get_object_bounded(
                capsule_hash,
                ryeos_state::objects::MAX_PERSISTENT_SESSION_CAPSULE_BYTES as u64,
            )?
            .ok_or_else(|| anyhow!("admitted session capsule disappeared"))?;
        authority.ensure_guard(&guard)?;
        if ryeos_state::objects::canonical_value_digest(&value)? != capsule_hash {
            bail!("admitted session capsule content hash changed");
        }
        let capsule =
            ryeos_state::objects::AdmittedPersistentSessionCapsule::from_current_value(&value)?;
        if capsule.content_hash()? != capsule_hash {
            bail!("admitted session capsule content hash changed");
        }
        Ok(capsule)
    }

    /// Verify immutable replay evidence without holding the node-wide state
    /// mutex. The index is read under the mutex, CAS verification happens on
    /// descriptor-pinned authority, and an exact-row retention touch closes
    /// the operation. Movement during verification is unavailable, never a
    /// miss that could re-execute an already-recorded effect.
    pub fn lookup_replay_record(
        &self,
        namespace: &ryeos_state::ReplayIndexNamespace,
        cache_key: &str,
        mut verify: impl FnMut(&ryeos_state::ReplayIndexRecord) -> ryeos_state::ReplayRecordVerification,
    ) -> Result<ryeos_state::ReplayLookupOutcome> {
        let indexed = {
            let g = self.lock()?;
            match g.state_db.lookup_replay_index(namespace, cache_key)? {
                ryeos_state::ReplayLookupOutcome::Absent => {
                    return Ok(ryeos_state::ReplayLookupOutcome::Absent);
                }
                ryeos_state::ReplayLookupOutcome::Present(indexed) => indexed,
                failure => return Ok(failure),
            }
        };
        match verify(&indexed) {
            ryeos_state::ReplayRecordVerification::Verified => {
                let retained = {
                    let g = self.lock()?;
                    g.state_db
                        .touch_replay_record_if_same(namespace, &indexed)?
                };
                if !retained {
                    return Ok(ryeos_state::ReplayLookupOutcome::Unavailable {
                        reason: "replay index moved while its record was being verified"
                            .to_string(),
                    });
                }
                Ok(ryeos_state::ReplayLookupOutcome::Present(indexed))
            }
            ryeos_state::ReplayRecordVerification::Unavailable { reason } => {
                Ok(ryeos_state::ReplayLookupOutcome::Unavailable { reason })
            }
            ryeos_state::ReplayRecordVerification::IntegrityFailure { reason } => {
                Ok(ryeos_state::ReplayLookupOutcome::IntegrityFailure { reason })
            }
        }
    }

    pub fn pending_head_transition_status(&self) -> Result<PendingHeadTransitionStatus> {
        let g = self.lock()?;
        let transitions = g.state_db.pending_chain_transitions()?;
        let oldest_prepared_at = transitions
            .iter()
            .map(|transition| transition.prepared_at.as_str())
            .min()
            .map(str::to_owned);
        let oldest_age_seconds = oldest_prepared_at
            .as_deref()
            .map(|prepared_at| -> Result<u64> {
                let prepared_at = parse_canonical_timestamp(prepared_at)
                    .context("invalid pending transition prepared_at")?;
                let now = parse_canonical_timestamp(&lillux::time::iso8601_now())
                    .context("invalid current time while reading transition diagnostics")?;
                Ok(now.signed_duration_since(prepared_at).num_seconds().max(0) as u64)
            })
            .transpose()?;
        Ok(PendingHeadTransitionStatus {
            pending: transitions.len(),
            pending_sets: transitions
                .iter()
                .filter(|transition| transition.operation == ryeos_state::HeadOperation::Set)
                .count(),
            pending_removes: transitions
                .iter()
                .filter(|transition| transition.operation == ryeos_state::HeadOperation::Remove)
                .count(),
            oldest_prepared_at,
            oldest_age_seconds,
        })
    }

    /// Run a closure with access to the underlying StateDb.
    pub fn with_state_db<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&StateDb) -> Result<T>,
    {
        let g = self.lock()?;
        f(&g.state_db)
    }

    /// Prove a returning locally owned chain against one exact target-authored
    /// head before target-private placement state is consulted.
    pub fn verify_returning_chain_owner_anchor(
        &self,
        chain_root_id: &str,
        candidate_head: &str,
        owner_principal: &str,
        origin_site_id: &str,
        required_signer: &str,
    ) -> Result<String> {
        let guard = self.state_authority.acquire_shared_guard()?;
        let g = self.lock()?;
        g.state_db.verify_current_chain_anchor_for_candidate(
            chain_root_id,
            candidate_head,
            required_signer,
            owner_principal,
            origin_site_id,
            &guard,
        )
    }

    /// Resolve the sole durable source-role handoff currently fencing one
    /// hosted placement. The existing sync-job ledger is authoritative; this
    /// query deliberately does not introduce a second placement registry.
    pub fn active_source_worker_handoff_for_placement(
        &self,
        placement_thread_id: &str,
    ) -> Result<
        Option<(
            ryeos_state::SyncJobRecord,
            crate::worker_handoff::WorkerSessionHandoffJobOperation,
        )>,
    > {
        let g = self.lock()?;
        active_source_worker_handoff_for_placement_in_db(&g.state_db, placement_thread_id)
    }

    /// Strict, non-mutating verification of the selected projection against a
    /// stable snapshot of trusted heads and CAS.
    pub fn verify_projection_generation(
        &self,
    ) -> Result<ryeos_state::rebuild::ProjectionVerificationReport> {
        let _cas_guard = self
            .state_authority
            .acquire_exclusive_guard(!self.read_only)?;
        let _write_permit = self
            .write_barrier
            .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|error| {
                anyhow!("cannot acquire write permit for projection verification: {error}")
            })?;
        let mut g = self.lock()?;
        g.state_db.verify_projection_generation()
    }

    /// Publish a freshly rebuilt projection generation and switch
    /// this live store to it. The offline service path still obeys the global
    /// mutation hierarchy so no direct/import publisher can overlap the head
    /// snapshot or generation publication.
    pub fn rebuild_projection_generation(&self) -> Result<ryeos_state::rebuild::RebuildReport> {
        if !self.allow_projection_rebuild {
            bail!(
                "projection rebuild is available only in the authored offline rebuild bootstrap mode"
            );
        }
        let cas_guard = self.state_authority.acquire_exclusive_guard(true)?;
        let _write_permit = self
            .write_barrier
            .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|error| {
                anyhow!("cannot acquire write permit for projection rebuild: {error}")
            })?;
        let mut g = self.lock()?;
        let Inner {
            state_db,
            runtime_db,
            ..
        } = &mut *g;
        state_db.rebuild_projection_generation_admitted(Some(&*runtime_db), &cas_guard)
    }

    /// Consume a staged remote chain import through the normal mutation
    /// hierarchy. The CAS guard is acquired before the StateStore mutex and is
    /// passed into the journaled head publisher; StateDb never reacquires it
    /// from beneath the mutex.
    pub fn finalize_staged_chain_import(
        &self,
        staged: ryeos_state::sync::StagedChainImport,
    ) -> Result<ryeos_state::sync::ImportResult> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        ryeos_state::sync::finalize_import(
            &g.state_db,
            staged,
            g.signer.as_ref(),
            permit.cas_guard(),
        )
    }

    /// Publish the large, secret-free source launch ledger under a bounded
    /// CAS manifest and create the existing sync-job row that pins it. CAS
    /// publication precedes the SQLite root inside one write-permit window;
    /// a crash in the narrow gap leaves only collectable unreachable bytes,
    /// never a job naming absent authority.
    pub fn create_worker_handoff_source_job(
        &self,
        prepared: &crate::worker_handoff::PreparedPlacementTransferManifest,
        job: &ryeos_state::NewSyncJob,
        progress: &crate::worker_handoff::WorkerSessionHandoffProgress,
    ) -> Result<ryeos_state::SyncJobRecord> {
        prepared.object.validate()?;
        progress.validate()?;
        let manifest_hash = prepared.object_hash()?;
        if job.operation_type != crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
            || !job.roots.iter().any(|hash| hash == &manifest_hash)
            || progress.phase != crate::worker_handoff::WorkerHandoffPhase::SourceExported
        {
            bail!("source-exported worker handoff job does not pin its transfer manifest");
        }
        let operation = crate::worker_handoff::WorkerSessionHandoffJobOperation::from_value(
            job.operation.clone(),
        )?;
        if operation.role != crate::worker_handoff::WorkerHandoffJobRole::Source
            || operation.operation_id != progress.operation_id
        {
            bail!("source-exported progress belongs to another handoff operation");
        }
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if let Some(existing) = g.state_db.get_sync_job(&job.job_id)? {
            if existing.operation_type != job.operation_type
                || existing.operation != job.operation
                || !existing.roots.iter().any(|hash| hash == &manifest_hash)
            {
                bail!("worker handoff job identity is already bound to another operation");
            }
            return Ok(existing);
        }
        if let Some((existing, existing_operation)) =
            active_source_worker_handoff_for_placement_in_db(
                &g.state_db,
                &operation.source_placement_thread_id,
            )?
        {
            bail!(
                "source placement {} is already fenced by active handoff {} ({})",
                operation.source_placement_thread_id,
                existing_operation.operation_id,
                existing.job_id
            );
        }
        self.state_authority.ensure_guard(permit.cas_guard())?;
        let cas = self.state_authority.cas_store()?;
        let object_value = prepared.object.to_value()?;
        let stored_manifest = cas.put_object(&object_value)?;
        if stored_manifest.hash != manifest_hash {
            bail!("stored placement transfer manifest digest changed");
        }
        g.state_db
            .record_cas_entry(&ryeos_state::NewCasEntryAttribution {
                hash: manifest_hash,
                entry_kind: ryeos_state::CasEntryKind::Object,
                bytes: u64::try_from(lillux::canonical_json(&object_value)?.len())?,
                source_principal: Some(format!("fp:{}", g.signer.fingerprint())),
                source_peer: None,
                job_id: Some(job.job_id.clone()),
                state: ryeos_state::CasEntryState::Local,
            })?;
        let progress_value = progress.to_value()?;
        g.state_db.create_sync_job_with_initial_progress(
            job,
            ryeos_state::SyncJobState::Running,
            progress.phase.as_str(),
            Some(&progress_value),
        )
    }

    /// Bound the StateStore mutex acquisition by the caller's original deadline.
    #[track_caller]
    fn lock_until(&self, deadline: lillux::time::MonotonicDeadline) -> Result<StateStoreGuard<'_>> {
        let started = lillux::time::MonotonicTimer::start();
        let caller = std::panic::Location::caller();
        let inner = cooperate_while_blocking(|| {
            loop {
                if deadline.has_elapsed() {
                    bail!("original StateStore deadline expired waiting for mutex");
                }
                match self.inner.try_lock() {
                    Ok(inner) => {
                        if deadline.has_elapsed() {
                            bail!("original StateStore deadline expired at mutex acquisition");
                        }
                        break Ok(inner);
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => bail!("StateStore lock poisoned"),
                    Err(std::sync::TryLockError::WouldBlock) => {
                        lillux::time::sleep(
                            deadline
                                .remaining()
                                .min(lillux::time::Duration::from_millis(1)),
                        );
                    }
                }
            }
        })?;
        let waited = started.elapsed();
        if waited >= std::time::Duration::from_millis(100) {
            record_store_contention(StoreContentionKind::Wait, waited, caller);
        }
        Ok(StateStoreGuard {
            store_mutex: &self.inner,
            inner,
            acquired_at: lillux::time::MonotonicTimer::start(),
            caller,
        })
    }

    fn with_runtime_original_deadline<T>(
        &self,
        deadline: lillux::time::MonotonicDeadline,
        operation: impl FnOnce(&runtime_db::RuntimeDb) -> Result<T>,
    ) -> Result<T> {
        let g = self.lock_until(deadline)?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("original RuntimeDb operation is fenced for process attachment");
        }
        g.runtime_db.with_original_deadline(
            deadline,
            &self.process_attachment_admission_open,
            &self.process_attachment_admission_closed,
            operation,
        )
    }

    fn attached_process_registry(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<String, AttachedProcess>> {
        self.attached_processes.lock().unwrap_or_else(|poisoned| {
            tracing::error!(
                "attached process registry mutex was poisoned; retaining its derived state"
            );
            poisoned.into_inner()
        })
    }

    fn live_worker_registry(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<String, runtime_db::WorkerProcessRecord>> {
        self.live_workers.lock().unwrap_or_else(|poisoned| {
            tracing::error!("live worker registry mutex was poisoned; retaining its derived state");
            poisoned.into_inner()
        })
    }

    /// The in-memory attachment records, read without SQL.
    pub fn attached_process_snapshot(&self) -> Vec<AttachedProcess> {
        self.attached_process_registry().values().cloned().collect()
    }

    /// Drop the in-memory attachment records of processes a stop proved dead
    /// without writing their rows, each only while it still holds exactly
    /// the proved identity. The durable rows stay attached for next-start
    /// recovery; the live Store's close then sees no live attachment.
    pub fn forget_proven_dead_attachments(
        &self,
        proved: &BTreeMap<String, crate::process::ExecutionProcessIdentity>,
    ) {
        let mut attached = self.attached_process_registry();
        for (thread_id, identity) in proved {
            if attached
                .get(thread_id)
                .is_some_and(|record| record.process_identity.as_ref() == Some(identity))
            {
                attached.remove(thread_id);
            }
        }
    }

    /// The in-memory live dedicated worker records, read without SQL.
    pub fn live_worker_snapshot(&self) -> Vec<runtime_db::WorkerProcessRecord> {
        self.live_worker_registry().values().cloned().collect()
    }

    /// Check occupancy under this exact Store guard, then retain only the
    /// lexical operation capability; no registry lock crosses SQL or files.
    fn selected_mutation_permit<'guard, 'store>(
        &self,
        g: &'guard StateStoreGuard<'store>,
        operation: SelectedMutationOperation<'guard>,
    ) -> Result<SelectedMutationPermit<'guard, 'store>> {
        if !self.owns_store_mutex(g.store_mutex) {
            bail!("selected mutation requires this SAME StateStore guard");
        }
        let selected = self
            .selected_invocations
            .lock()
            .map_err(|_| anyhow!("selected invocation registry poisoned; mutation refused"))?;
        let blocked = match &operation {
            SelectedMutationOperation::Claim(thread, ..)
            | SelectedMutationOperation::Release(thread, ..)
            | SelectedMutationOperation::Rotate(thread, ..)
            | SelectedMutationOperation::DeleteThread(thread)
            | SelectedMutationOperation::DiscardBirth(thread) => selected.has_thread(thread),
            SelectedMutationOperation::ClearStale(_)
            | SelectedMutationOperation::DeleteChain(..) => selected.has_any(),
        };
        if blocked {
            bail!("selected invocation retains destructive mutation ownership");
        }
        drop(selected);
        Ok(SelectedMutationPermit {
            guard: g,
            db: &g.runtime_db,
            operation,
        })
    }

    /// Whether `store_mutex` is this Store's own mutex. A guard of any other
    /// Store mints no selected permit and admits no selected cleanup.
    fn owns_store_mutex(&self, store_mutex: *const Mutex<Inner>) -> bool {
        std::ptr::eq(store_mutex, &self.inner)
    }

    fn require_selected_bulk_exclusion(&self, g: &StateStoreGuard<'_>) -> Result<()> {
        if !self.owns_store_mutex(g.store_mutex) {
            bail!("selected cleanup requires this SAME StateStore guard");
        }
        let selected = self
            .selected_invocations
            .lock()
            .map_err(|_| anyhow!("selected invocation registry poisoned; cleanup refused"))?;
        if selected.has_any() {
            bail!("selected invocation retains bulk cleanup ownership");
        }
        Ok(())
    }

    /// Delete one auxiliary runtime row and keep the process-attachment
    /// projection at the same StateStore linearization point. Runtime rows are
    /// normally removed before target release, but rollback must remain exact
    /// if a future caller reaches this path after an attachment was recorded.
    fn delete_thread_runtime_locked(
        &self,
        g: &StateStoreGuard<'_>,
        thread_id: &str,
    ) -> Result<usize> {
        let permit =
            self.selected_mutation_permit(g, SelectedMutationOperation::DeleteThread(thread_id))?;
        let deleted = g.runtime_db.delete_thread_runtime(&permit, thread_id)?;
        if deleted > 0 {
            self.attached_process_registry().remove(thread_id);
        }
        Ok(deleted)
    }

    /// Rebuild the derived attachment projection after a transactional
    /// chain-runtime retirement. That operation deliberately removes the
    /// signed chain members plus any auxiliary rows attributed to the same
    /// chain, so its exact affected thread-id set is owned by RuntimeDb.
    fn refresh_attached_process_registry_locked(&self, g: &Inner) -> Result<()> {
        let attached = attached_processes_from_runtime(&g.runtime_db)?;
        *self.attached_process_registry() = attached;
        Ok(())
    }

    /// Acquire a write permit from the write barrier.
    /// Fails if the daemon is quiescing for GC.
    #[track_caller]
    fn acquire_write_permit(&self) -> Result<StateMutationPermit> {
        if self.read_only {
            bail!("state store is open for strict read-only verification");
        }
        let cas_guard = self.state_authority.acquire_shared_guard()?;
        let write_permit = self
            .write_barrier
            .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|e| anyhow!("cannot acquire write permit: {e}"))?;
        Ok(StateMutationPermit {
            cas_guard,
            _write_permit: write_permit,
        })
    }

    /// Serialize a terminal-GC dry-run with ordinary writers using only
    /// already-established lock anchors. This retains the normal barrier and
    /// lock order but cannot create recovery state merely by inspecting it.
    #[track_caller]
    fn acquire_gc_inspection_permit(&self) -> Result<StateMutationPermit> {
        if self.read_only {
            bail!("state store is open for strict read-only verification");
        }
        let cas_guard = self.state_authority.acquire_shared_guard()?;
        let write_permit = self
            .write_barrier
            .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|e| anyhow!("cannot acquire GC inspection permit: {e}"))?;
        Ok(StateMutationPermit {
            cas_guard,
            _write_permit: write_permit,
        })
    }

    /// Narrow mutation permit for converging journaled Remove records as part
    /// of the authored offline projection-rebuild bootstrap. It does not widen
    /// the common write gate, so unrelated StateStore mutations remain denied.
    #[track_caller]
    fn acquire_recovery_cleanup_permit(&self, _dry_run: bool) -> Result<StateMutationPermit> {
        if self.read_only && !self.allow_projection_rebuild {
            bail!("state store is open for strict read-only verification");
        }
        let cas_guard = self.state_authority.acquire_shared_guard()?;
        let write_permit = self
            .write_barrier
            .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|e| anyhow!("cannot acquire recovery cleanup permit: {e}"))?;
        Ok(StateMutationPermit {
            cas_guard,
            _write_permit: write_permit,
        })
    }

    fn authorize_runtime_pin_for_thread(
        g: &Inner,
        thread_id: &str,
    ) -> Result<ryeos_state::AuthoritativeRuntimePinAdmission> {
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("runtime-pin thread {thread_id} does not exist"))?;
        g.state_db.authorize_runtime_pin(&thread.chain_root_id)
    }

    #[tracing::instrument(
        name = "state:create_thread",
        skip(self, thread),
        fields(
            thread_id = %thread.thread_id,
            chain_root_id = %thread.chain_root_id,
            kind = %thread.kind,
            item_ref = %thread.item_ref,
        )
    )]
    pub(crate) fn create_child_thread_admitted(
        &self,
        thread: &NewThreadRecord,
    ) -> Result<Vec<PersistedEventRecord>> {
        if thread.thread_id == thread.chain_root_id {
            bail!("child persistence requires thread_id != chain_root_id");
        }
        if thread.captured_history_policy.is_some() {
            bail!("non-root threads cannot carry a captured history policy");
        }
        self.create_thread_inner(thread)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "state_store/qualification_transcript_publication_tests.rs"]
mod qualification_transcript_publication_tests;

#[cfg(test)]
#[path = "state_store/seat_cut_tests.rs"]
mod seat_cut_tests;

/// The durable standing of a placement's local connector, read by the Q
/// runner between the Worker's turn and its retirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalConnectorSettlement {
    Absent,
    Open,
    Closed { reason: Option<String> },
}
