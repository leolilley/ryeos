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
    NewDedicatedSessionCommand, NewHookDispatch, ObservationBatchReservation, RuntimeInfo,
    StopIntent, WorkerProcessRecord,
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
mod tests {
    use super::test_native::TestStateStore;
    use super::*;
    use ryeos_engine::contracts::{EffectivePrincipal, ExecutionHints, Principal, ProjectContext};
    use std::io::Write as _;
    use tempfile::tempdir;

    fn projected_event_fixture(
        durability: ryeos_state::objects::EventDurability,
    ) -> (ryeos_state::objects::ThreadEvent, queries::EventRow) {
        let event = ryeos_state::objects::ThreadEvent {
            schema: 1,
            kind: "thread_event".into(),
            chain_root_id: "T-projected".into(),
            chain_seq: 3,
            thread_id: "T-projected-child".into(),
            thread_seq: 2,
            event_type: "test_observation".into(),
            durability,
            ts: "2026-10-09T00:00:00Z".into(),
            prev_chain_event_hash: Some("a".repeat(64)),
            prev_thread_event_hash: Some("b".repeat(64)),
            payload: json!({"operation_id": "projection-control", "value": 7}),
        };
        let row = queries::EventRow {
            event_id: 3,
            event_hash: ryeos_state::objects::thread_event::hash_event(&event).unwrap(),
            chain_root_id: event.chain_root_id.clone(),
            chain_seq: 3,
            thread_id: event.thread_id.clone(),
            thread_seq: 2,
            event_type: event.event_type.clone(),
            durability: event.durability.to_string(),
            ts: event.ts.clone(),
            prev_chain_event_hash: event.prev_chain_event_hash.clone(),
            prev_thread_event_hash: event.prev_thread_event_hash.clone(),
            payload: serde_json::to_vec(&event.payload).unwrap(),
        };
        (event, row)
    }

    #[test]
    fn projection_records_match_canonical_events_without_rewriting_evidence() {
        use ryeos_state::objects::EventDurability::{Durable, Journal};
        for (durability, storage_class) in [(Durable, "indexed"), (Journal, "journal")] {
            let (event, row) = projected_event_fixture(durability);
            let authoritative =
                persisted_event_from_authoritative(row.event_hash.clone(), event).unwrap();
            let projected = persisted_event_from_projection_row(row.clone()).unwrap();
            assert_eq!(projected.storage_class, storage_class);
            assert_eq!(
                serde_json::to_value(&projected).unwrap(),
                serde_json::to_value(&authoritative).unwrap(),
            );

            // Conversion translates vocabulary only; it must not repair altered
            // payloads or hashes and thereby conceal a later authentication failure.
            let mut changed = row;
            changed.payload = br#"{"value":8}"#.to_vec();
            let changed = persisted_event_from_projection_row(changed).unwrap();
            assert_eq!(changed.event_hash, authoritative.event_hash);
            assert_ne!(changed.payload, authoritative.payload);
        }
    }

    #[test]
    fn projection_conversion_refuses_invalid_persistent_evidence() {
        use ryeos_state::objects::EventDurability::{Durable, Ephemeral};
        let (_, row) = projected_event_fixture(Durable);
        for durability in ["indexed", "ephemeral", "unknown", "", "Durable", "durable "] {
            let mut changed = row.clone();
            changed.durability = durability.into();
            assert!(persisted_event_from_projection_row(changed).is_err());
        }
        let (event, ephemeral) = projected_event_fixture(Ephemeral);
        assert!(persisted_event_from_authoritative(ephemeral.event_hash, event).is_err());
        let mut malformed = row;
        malformed.payload = b"invalid JSON".to_vec();
        assert!(persisted_event_from_projection_row(malformed).is_err());
    }

    fn append_indexed(
        store: &TestStateStore,
        thread_id: &str,
        count: usize,
    ) -> Vec<PersistedEventRecord> {
        let authored = (0..count)
            .map(|index| NewEventRecord {
                event_type: "test.history.event".into(),
                storage_class: "indexed".into(),
                payload: json!({"index": index}),
            })
            .collect::<Vec<_>>();
        store
            .append_events(thread_id, thread_id, &authored)
            .expect("append events")
    }

    #[test]
    fn anchored_page_reads_ignore_later_appends_and_refuse_unbounded_work() {
        let store = test_store();
        let thread_id = "T-anchored-page";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .unwrap();
        append_indexed(&store, thread_id, 2);
        let anchor = store
            .capture_thread_event_read_anchor(thread_id, thread_id)
            .unwrap()
            .expect("thread anchor exists");
        let (tip_hash, tip_seq) = anchor.tip().expect("anchor has a tip");
        let tip_hash = tip_hash.to_owned();
        let later = append_indexed(&store, thread_id, 1);
        let later_hash = later[0].event_hash.clone().unwrap();

        assert!(
            store
                .read_thread_event_page(&anchor, None, 0, 1024)
                .is_err()
        );
        assert!(store.read_thread_event_page(&anchor, None, 1, 0).is_err());
        assert!(store.read_thread_event_page(&anchor, None, 1, 1).is_err());

        let mut progress = None;
        let mut observed = Vec::new();
        loop {
            let page = store
                .read_thread_event_page(&anchor, progress.as_ref(), 1, 1024 * 1024)
                .unwrap();
            assert!(page.events.len() <= 1);
            observed.extend(page.events.into_iter().map(|(hash, _)| hash));
            progress = page.progress;
            if progress.is_none() {
                break;
            }
        }
        assert_eq!(observed.len() as u64, tip_seq);
        assert_eq!(observed.first(), Some(&tip_hash));
        assert!(!observed.contains(&later_hash));
        assert!(
            store
                .capture_thread_event_read_anchor(thread_id, "T-absent-thread")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn recent_thread_events_decode_through_the_durability_helper() {
        let store = test_store();
        let thread_id = "T-recent-events";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .unwrap();
        let appended = append_indexed(&store, thread_id, 3);
        let page = store
            .recent_thread_events(thread_id, thread_id, None, 2, 1024 * 1024)
            .unwrap();
        assert!(page.has_more);
        assert_eq!(page.events.len(), 2);
        assert!(
            page.events
                .iter()
                .all(|event| event.storage_class == "indexed")
        );
        assert_eq!(page.events[1].event_hash, appended[2].event_hash);
        assert!(
            store
                .recent_thread_events(thread_id, thread_id, None, 0, 1024)
                .is_err()
        );
    }

    #[test]
    fn execution_candidate_proof_returns_signed_records_across_bounded_pages() {
        let store = test_store();
        let thread_id = "T-execution-candidate-page";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .unwrap();
        let records = append_indexed(&store, thread_id, 70);
        let candidates = vec![records[0].clone(), records[69].clone(), records[35].clone()];
        let authenticated = store
            .authenticate_execution_event_candidates(thread_id, &candidates)
            .unwrap();
        assert_eq!(authenticated.len(), candidates.len());
        for (candidate, signed) in candidates.iter().zip(&authenticated) {
            assert!(execution_event_candidate_matches(candidate, signed));
        }
        assert_eq!(authenticated[0].payload["index"], 0);
        assert_eq!(authenticated[1].payload["index"], 69);
        assert_eq!(authenticated[2].payload["index"], 35);
    }

    #[test]
    fn execution_candidate_proof_refuses_contradicted_or_malformed_candidates() {
        let store = test_store();
        let thread_id = "T-execution-candidate-refusal";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .unwrap();
        let candidate = append_indexed(&store, thread_id, 1).remove(0);
        let refuses = |mutate: &dyn Fn(&mut PersistedEventRecord)| {
            let mut changed = candidate.clone();
            mutate(&mut changed);
            store
                .authenticate_execution_event_candidates(thread_id, &[changed])
                .is_err()
        };
        assert!(!refuses(&|_| {}));
        assert!(refuses(
            &|c| c.payload["index"] = json!("projection-forgery")
        ));
        assert!(refuses(&|c| c.thread_id = "T-foreign".into()));
        assert!(refuses(&|c| c.chain_root_id = "T-foreign".into()));
        assert!(refuses(&|c| c.storage_class = "journal".into()));
        assert!(refuses(&|c| c.ts = "2001-01-01T00:00:00Z".into()));
        assert!(refuses(&|c| c.event_hash = None));
        assert!(refuses(&|c| c.event_hash = Some("A".repeat(64))));
        assert!(refuses(&|c| c.event_hash = Some("c".repeat(64))));
        assert!(refuses(&|c| c.chain_seq = 0));
        // The same hash named twice in one thread is contradictory input.
        assert!(
            store
                .authenticate_execution_event_candidates(
                    thread_id,
                    &[candidate.clone(), candidate.clone()]
                )
                .is_err()
        );
        let too_many = vec![candidate; 2_001];
        assert!(
            store
                .authenticate_execution_event_candidates(thread_id, &too_many)
                .is_err()
        );
    }

    #[test]
    fn projected_event_read_surfaces_preserve_indexed_class_and_thread_scope() {
        let store = test_store();
        let thread = "T-projection-read-surfaces";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let foreign = "T-projection-foreign";
        store
            .create_thread_for_test(&thread_record(foreign, foreign))
            .unwrap();
        let expected = store
            .latest_thread_event_by_type(thread, ryeos_state::event_types::THREAD_CREATED)
            .unwrap()
            .unwrap();
        assert_eq!(expected.storage_class, "indexed");
        let scoped = [
            store
                .replay_events(thread, Some(thread), None, 10, 1024 * 1024)
                .unwrap()
                .events,
            store
                .replay_events_through(
                    thread,
                    Some(thread),
                    None,
                    expected.chain_seq,
                    10,
                    1024 * 1024,
                )
                .unwrap()
                .events,
            store.latest_thread_events(thread, 10).unwrap(),
            vec![
                store
                    .latest_thread_event_by_type_prefix(thread, "thread_")
                    .unwrap()
                    .unwrap(),
            ],
        ];
        for events in scoped {
            assert_eq!(events.len(), 1);
            assert_eq!(
                serde_json::to_value(&events[0]).unwrap(),
                serde_json::to_value(&expected).unwrap()
            );
        }
        let node_events = store.latest_node_events(10, &[]).unwrap();
        assert_eq!(node_events.len(), 2);
        assert!(
            node_events
                .iter()
                .all(|event| event.storage_class == "indexed")
        );
        let owned = node_events
            .iter()
            .find(|event| event.thread_id == thread)
            .unwrap();
        assert_eq!(
            serde_json::to_value(owned).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
    }

    fn rooted_bundle_proof_fixture(
        key: &lillux::crypto::SigningKey,
        fingerprint: &str,
    ) -> ryeos_state::objects::AdmittedLaunchCapsule {
        use ryeos_state::objects::{
            AdmittedDirectCommandClosure as Command, AdmittedExecutionClosure as Closure,
            AdmittedLaunchArtifactIdentity as Artifact, DirectExecutableIdentity as Executable,
            DirectRootSourceIdentity, DirectRuntimeIdentity, DirectRuntimeSourceSpace,
            ExecutionLaunchDriver, ExecutionLifecycleAuthority, ExecutionOwnershipAuthority,
            ExecutionProjectAuthority, ExecutionRecoveryAuthority, RetainedItemSpaceV1 as Space,
            RetainedTrustClassV1 as Trust,
        };

        let triple = "x86_64-unknown-linux-gnu";
        let item_ref = format!("bin/{triple}/runner");
        let item_source = json!({
            "kind": "item_source",
            "item_ref": item_ref,
            "content_blob_hash": "f".repeat(64),
            "integrity": format!("sha256:{}", "f".repeat(64)),
            "signature_info": null,
            "mode": 0o755,
        });
        let item_source_bytes = lillux::canonical_json(&item_source).unwrap();
        let item_source_hash = lillux::sha256_hex(item_source_bytes.as_bytes());
        let manifest = json!({
            "kind": "source_manifest",
            "item_source_hashes": { item_ref.clone(): item_source_hash.clone() },
        });
        let manifest_hash =
            lillux::sha256_hex(lillux::canonical_json(&manifest).unwrap().as_bytes());
        let manifest_ref_body = format!("ryeos:bundle-executor-manifest\n{manifest_hash}\n");
        let signed_manifest_ref =
            lillux::signature::sign_content(&manifest_ref_body, key, "#", None);
        let signed_sidecar = lillux::signature::sign_content(&item_source_bytes, key, "#", None);
        let proof = ryeos_state::objects::BundleExecutorMemberSourceProofV1 {
            requested_binary_ref: "bin:runner".to_owned(),
            request_origin: ryeos_state::objects::BundleExecutorRequestOriginV1::WrapperBundle {
                wrapper_bundle_name: "fixture".to_owned(),
            },
            selected_bundle: ryeos_state::objects::SelectedBundleIdentityV1 {
                name: "fixture".to_owned(),
                body_digest: "a".repeat(64),
                signer_fingerprint: fingerprint.to_owned(),
            },
            runtime_selection: ryeos_state::objects::RetainedRuntimeBinarySelectionV1 {
                definition_owner: ryeos_state::objects::RetainedRuntimeDefinitionOwnerV1 {
                    requested_id: "runtime:fixture".to_owned(),
                    canonical_ref: "runtime:fixture".to_owned(),
                    source_space: Space::Bundle,
                    trust_class: Trust::TrustedBundle,
                    signer_fingerprint: Some(fingerprint.to_owned()),
                    content_hash: "b".repeat(64),
                    raw_content_digest: "c".repeat(64),
                },
                definition_owner_chain_index: 1,
                authored_command: "bin:runner".to_owned(),
                expanded_command: "bin:runner".to_owned(),
            },
            selected_item_ref: item_ref,
            target_triple: triple.to_owned(),
            item_source_hash,
            expected_mode: 0o755,
            signed_executor_manifest_ref: signed_manifest_ref.into_bytes(),
            executor_manifest_object: manifest,
            item_source_object: item_source,
            signed_item_source_sidecar: signed_sidecar.into_bytes(),
        };
        let exact_program = json!({});
        let exact_program_hash =
            lillux::sha256_hex(lillux::canonical_json(&exact_program).unwrap().as_bytes());
        let sealed_invocation = json!({});
        let plan = json!({});
        let execution_plan_hash =
            lillux::sha256_hex(lillux::canonical_json(&plan).unwrap().as_bytes());
        let protocol_document = String::new();
        let protocol_hash = lillux::sha256_hex(protocol_document.as_bytes());

        ryeos_state::objects::AdmittedLaunchCapsule {
            schema: ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
            kind: "admitted_launch_capsule".to_owned(),
            exact_program,
            exact_program_hash,
            sealed_invocation,
            project_authority: ExecutionProjectAuthority::PROJECTLESS,
            lifecycle_authority: ExecutionLifecycleAuthority {
                ownership: ExecutionOwnershipAuthority::DaemonOwned,
                recovery: ExecutionRecoveryAuthority::RestartRecoverable,
            },
            launch_driver: ExecutionLaunchDriver::DirectItemExecutor,
            artifact_identity: Artifact::DirectItemExecutor {
                executor_ref: "tool:test/executor".to_owned(),
                root_subject_source_content_digest: "d".repeat(64),
                root_subject_signer_fingerprint: Some(fingerprint.to_owned()),
                root_subject_source_identity: DirectRootSourceIdentity::Project,
                protocol_ref: "protocol:test".to_owned(),
                protocol_content_hash: protocol_hash,
                protocol_signer_fingerprint: fingerprint.to_owned(),
                execution_plan_hash,
                executable_identity: Executable::BundleExecutor {
                    content_hash: "f".repeat(64),
                    executor_manifest_hash: manifest_hash,
                    executor_manifest_signer_fingerprint: fingerprint.to_owned(),
                    member_source_proof: proof,
                },
                runtime_identity: DirectRuntimeIdentity {
                    runtime_ref: "runtime:fixture".to_owned(),
                    runtime_source_space: DirectRuntimeSourceSpace::Bundle,
                    runtime_content_hash: "e".repeat(64),
                    runtime_signer_fingerprint: fingerprint.to_owned(),
                    runtime_bundle_manifest_hash: Some("1".repeat(64)),
                    runtime_bundle_signer_fingerprint: Some(fingerprint.to_owned()),
                },
            },
            execution_closure: Closure::DirectItemExecutor {
                execution_plan: plan,
                protocol_descriptor_document: protocol_document,
                command: Command::NodePolicy,
                admitted_project_root: None,
            },
            execution_realization_hash: "2".repeat(64),
            source_binding_hash: None,
            accounting_scope: None,
            effective_caps: Vec::new(),
            parent_delegation_caps: None,
            runtime_ref: "runtime:fixture".to_owned(),
            executor_ref: "tool:test/executor".to_owned(),
        }
    }

    #[test]
    fn rooted_bundle_member_proof_rechecks_current_owner_and_signatures() {
        let key = lillux::crypto::SigningKey::from_bytes(&[41; 32]);
        let fingerprint = lillux::crypto::fingerprint(&key.verifying_key());
        let trust = bundle_admission_trust_for(&[&key]);
        let capsule = rooted_bundle_proof_fixture(&key, &fingerprint);
        verify_rooted_bundle_member_source_proof(&capsule, &trust)
            .expect("rooted proof verifies with current owner and source signer");

        let mut bad_signature = capsule.clone();
        let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executable_identity:
                ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
                    member_source_proof,
                    ..
                },
            ..
        } = &mut bad_signature.artifact_identity
        else {
            unreachable!("fixture uses BundleExecutor identity");
        };
        let mut sidecar =
            String::from_utf8(member_source_proof.signed_item_source_sidecar.clone()).unwrap();
        let header_end = sidecar.find('\n').unwrap();
        let signer_separator = sidecar[..header_end].rfind(':').unwrap();
        let signature_separator = sidecar[..signer_separator].rfind(':').unwrap();
        let signature_start = signature_separator + 1;
        let replacement = if sidecar.as_bytes()[signature_start] == b'A' {
            b'B'
        } else {
            b'A'
        };
        sidecar.replace_range(
            signature_start..signature_start + 1,
            &(replacement as char).to_string(),
        );
        member_source_proof.signed_item_source_sidecar = sidecar.into_bytes();
        assert!(verify_rooted_bundle_member_source_proof(&bad_signature, &trust).is_err());

        let mut revoked_owner = capsule;
        let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executable_identity:
                ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
                    member_source_proof,
                    ..
                },
            ..
        } = &mut revoked_owner.artifact_identity
        else {
            unreachable!("fixture uses BundleExecutor identity");
        };
        member_source_proof
            .runtime_selection
            .definition_owner
            .signer_fingerprint = Some("0".repeat(64));
        assert!(verify_rooted_bundle_member_source_proof(&revoked_owner, &trust).is_err());
    }

    fn bundle_admission_trust_for(keys: &[&lillux::crypto::SigningKey]) -> BundleAdmissionTrust {
        BundleAdmissionTrust::bound(ryeos_engine::trust::TrustStore::from_signers(
            keys.iter()
                .map(|key| ryeos_engine::trust::TrustedSigner {
                    fingerprint: lillux::crypto::fingerprint(&key.verifying_key()),
                    verifying_key: key.verifying_key(),
                    label: None,
                })
                .collect(),
        ))
    }

    #[test]
    fn rooted_bundle_member_proof_follows_admitting_trust_not_head_trust() {
        let bundle_key = lillux::crypto::SigningKey::from_bytes(&[41; 32]);
        let node_key = lillux::crypto::SigningKey::from_bytes(&[42; 32]);
        let bundle_fingerprint = lillux::crypto::fingerprint(&bundle_key.verifying_key());
        let capsule = rooted_bundle_proof_fixture(&bundle_key, &bundle_fingerprint);

        verify_rooted_bundle_member_source_proof(
            &capsule,
            &bundle_admission_trust_for(&[&node_key, &bundle_key]),
        )
        .expect("Bundle-signed capsule verifies under the trust that admitted the Bundle");

        let removed = verify_rooted_bundle_member_source_proof(
            &capsule,
            &bundle_admission_trust_for(&[&node_key]),
        )
        .expect_err("Bundle signer removed from admitting trust must refuse");
        assert!(
            removed.to_string().contains("no longer trusted"),
            "unexpected refusal: {removed:#}"
        );

        let unbound =
            verify_rooted_bundle_member_source_proof(&capsule, &BundleAdmissionTrust::unbound())
                .expect_err("a store without admitting trust must refuse Bundle executors");
        assert!(
            unbound.to_string().contains("no Bundle admission trust"),
            "unexpected refusal: {unbound:#}"
        );
    }

    #[test]
    fn abandoned_runtime_snapshot_stage_is_removed_under_pinned_state_authority() {
        let temporary = tempdir().unwrap();
        let state = lillux::PinnedDirectory::open_or_create(temporary.path()).unwrap();
        let stages = state
            .create_child(std::ffi::OsStr::new("runtime-snapshot-staging"), 0o700)
            .unwrap();
        let name = format!("snapshot-stage-{}", uuid::Uuid::new_v4());
        let stage = stages
            .create_child(std::ffi::OsStr::new(&name), 0o700)
            .unwrap();
        let product = stage
            .create_child(std::ffi::OsStr::new("product"), 0o700)
            .unwrap();
        let mut payload = product
            .open_regular_create(std::ffi::OsStr::new("payload"), true, true, 0o600)
            .unwrap();
        payload.write_all(b"disposable upload bytes").unwrap();
        drop(payload);
        discard_abandoned_runtime_snapshot_staging(&state).unwrap();
        assert!(
            stages
                .open_child_directory(std::ffi::OsStr::new(&name))
                .unwrap()
                .is_none()
        );
        discard_abandoned_runtime_snapshot_staging(&state).unwrap();
    }

    #[test]
    fn runtime_snapshot_stage_sweep_refuses_foreign_entry() {
        let temporary = tempdir().unwrap();
        let state = lillux::PinnedDirectory::open_or_create(temporary.path()).unwrap();
        let stages = state
            .create_child(std::ffi::OsStr::new("runtime-snapshot-staging"), 0o700)
            .unwrap();
        stages
            .create_child(std::ffi::OsStr::new("unrelated"), 0o700)
            .unwrap();
        assert!(discard_abandoned_runtime_snapshot_staging(&state).is_err());
        assert!(
            stages
                .open_child_directory(std::ffi::OsStr::new("unrelated"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn abandoned_external_guest_package_is_removed_under_pinned_state_authority() {
        let temporary = tempdir().unwrap();
        let state = lillux::PinnedDirectory::open_or_create(temporary.path()).unwrap();
        let packages = state
            .create_child(std::ffi::OsStr::new("external-guest-packages"), 0o700)
            .unwrap();
        let (name, package) = packages
            .create_unique_child("guest-package", 0o700)
            .unwrap();
        let mut payload = package
            .open_regular_create(std::ffi::OsStr::new("payload"), true, true, 0o600)
            .unwrap();
        payload.write_all(b"secret bootstrap").unwrap();
        drop(payload);
        discard_abandoned_external_guest_packages(&state).unwrap();
        assert!(packages.open_child_directory(&name).unwrap().is_none());
        discard_abandoned_external_guest_packages(&state).unwrap();
    }

    #[test]
    fn abandoned_external_guest_package_sweep_refuses_foreign_entry() {
        let temporary = tempdir().unwrap();
        let state = lillux::PinnedDirectory::open_or_create(temporary.path()).unwrap();
        let packages = state
            .create_child(std::ffi::OsStr::new("external-guest-packages"), 0o700)
            .unwrap();
        packages
            .create_child(std::ffi::OsStr::new("unrelated"), 0o700)
            .unwrap();
        assert!(discard_abandoned_external_guest_packages(&state).is_err());
        assert!(
            packages
                .open_child_directory(std::ffi::OsStr::new("unrelated"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn abandoned_external_guest_package_sweep_progresses_beyond_one_batch() {
        let temporary = tempdir().unwrap();
        let state = lillux::PinnedDirectory::open_or_create(temporary.path()).unwrap();
        let packages = state
            .create_child(std::ffi::OsStr::new("external-guest-packages"), 0o700)
            .unwrap();
        for _ in 0..1025 {
            packages
                .create_unique_child("guest-package", 0o700)
                .unwrap();
        }
        discard_abandoned_external_guest_packages(&state).unwrap();
        assert!(packages.entry_names_bounded(1).unwrap().is_empty());
    }

    #[test]
    fn abandoned_external_guest_package_is_removed_on_state_store_reopen() {
        let temporary = tempdir().unwrap();
        let app_root = temporary.path().to_path_buf();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&app_root.join("node-key.pem"))
                .expect("test node identity");
        let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let trust = Arc::new(trust);
        let open = || {
            TestStateStore::open(&app_root, Arc::clone(&signer), Arc::clone(&trust))
                .expect("state store")
        };

        let store = open();
        let packages = store.external_guest_package_parent().unwrap();
        let (name, package) = packages
            .create_unique_child("guest-package", 0o700)
            .unwrap();
        let mut payload = package
            .open_regular_create(std::ffi::OsStr::new("payload"), true, true, 0o600)
            .unwrap();
        payload.write_all(b"secret bootstrap").unwrap();
        drop(payload);
        drop(package);
        drop(store);

        let reopened = open();
        assert!(packages.open_child_directory(&name).unwrap().is_none());
        drop(reopened);
    }

    fn remote_follow_delivery_request()
    -> crate::federated_follow::RemoteFollowTerminalDeliveryRequest {
        crate::federated_follow::RemoteFollowTerminalDeliveryRequest {
            operation_id: "1".repeat(64),
            reservation_attestation_hash: "2".repeat(64),
            terminal_attestation_hash: "3".repeat(64),
            child_chain_root_id: "T-child".to_owned(),
            target_chain_head_hash: "4".repeat(64),
            parent_site_id: "site:parent".to_owned(),
            target_site_id: "site:target".to_owned(),
        }
    }

    fn remote_follow_delivery_job(
        state: ryeos_state::SyncJobState,
        phase: &str,
        result: Option<Value>,
    ) -> ryeos_state::SyncJobRecord {
        ryeos_state::SyncJobRecord {
            job_id: format!("remote-follow-terminal-target:{}", "1".repeat(64)),
            operation_type: crate::federated_follow::REMOTE_FOLLOW_DELIVERY_OPERATION.to_owned(),
            operation: json!({
                "operation_type":crate::federated_follow::REMOTE_FOLLOW_DELIVERY_OPERATION
            }),
            peer: Some("site:parent".to_owned()),
            state,
            phase: phase.to_owned(),
            roots: vec!["2".repeat(64), "3".repeat(64), "4".repeat(64)],
            heads: vec!["4".repeat(64)],
            uploaded_hashes: Vec::new(),
            fetched_hashes: Vec::new(),
            attempt_count: 0,
            max_attempts: ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS,
            last_error: None,
            result,
            created_at: "2026-08-29T00:00:00Z".to_owned(),
            updated_at: "2026-08-29T00:00:00Z".to_owned(),
            finished_at: None,
        }
    }

    #[test]
    fn remote_follow_terminal_job_distinguishes_pending_request_and_completed_response() {
        let request = remote_follow_delivery_request();
        let pending = remote_follow_delivery_job(
            ryeos_state::SyncJobState::Retryable,
            "delivery_retryable",
            Some(serde_json::to_value(&request).unwrap()),
        );
        assert_eq!(
            classify_retained_remote_follow_terminal_job_result(&pending, &request).unwrap(),
            RetainedRemoteFollowTerminalJobResult::Pending
        );

        let response = crate::federated_follow::RemoteFollowTerminalDeliveryResponse {
            operation_id: request.operation_id.clone(),
            child_chain_root_id: request.child_chain_root_id.clone(),
            parent_chain_root_id: "T-parent".to_owned(),
            parent_successor_thread_id: "T-parent-successor".to_owned(),
            delivery: "settled".to_owned(),
        };
        let completed = remote_follow_delivery_job(
            ryeos_state::SyncJobState::Completed,
            "settled",
            Some(serde_json::to_value(response).unwrap()),
        );
        assert_eq!(
            classify_retained_remote_follow_terminal_job_result(&completed, &request).unwrap(),
            RetainedRemoteFollowTerminalJobResult::Completed
        );

        let completed_with_request = remote_follow_delivery_job(
            ryeos_state::SyncJobState::Completed,
            "settled",
            Some(serde_json::to_value(&request).unwrap()),
        );
        assert!(
            classify_retained_remote_follow_terminal_job_result(&completed_with_request, &request)
                .is_err()
        );
    }

    #[test]
    fn remote_follow_terminal_job_repairs_only_pristine_missing_request() {
        let request = remote_follow_delivery_request();
        let pristine =
            remote_follow_delivery_job(ryeos_state::SyncJobState::Planned, "planned", None);
        assert_eq!(
            classify_retained_remote_follow_terminal_job_result(&pristine, &request).unwrap(),
            RetainedRemoteFollowTerminalJobResult::RepairMissingRequest
        );

        let mut progressed = pristine;
        progressed.attempt_count = 1;
        assert!(
            classify_retained_remote_follow_terminal_job_result(&progressed, &request).is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_store_wait_does_not_starve_unrelated_async_work() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);

        let waiter = tokio::spawn(async move {
            cooperate_while_blocking(|| {
                started_tx.send(()).expect("test still awaits the waiter");
                release_rx.recv().expect("test releases the waiter");
            });
        });
        started_rx.await.expect("blocking boundary started");

        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            tokio::task::yield_now().await;
        })
        .await
        .expect("the sole async worker remains available");

        release_tx.send(()).expect("release blocking boundary");
        waiter.await.expect("waiter task");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blocking_store_boundary_is_valid_from_spawn_blocking_workers() {
        tokio::task::spawn_blocking(|| cooperate_while_blocking(|| 42usize))
            .await
            .expect("blocking worker must not panic at the cooperative boundary");
    }

    #[test]
    fn state_anchor_publish_wire_contract_requires_typed_subject() {
        let mut request = json!({
            "thread_id": "T-root",
            "subject": {
                "kind": "graph",
                "graph_run_id": "G-test",
                "definition_ref": "graph:test/solve",
                "effective_definition_digest": "d".repeat(64),
                "node": "solve",
                "step": 3
            },
            "contract": "domain.restore.v1",
            "restore": {"contract": "domain.restore.v1"},
            "restore_digest": format!("sha256:{}", "a".repeat(64)),
            "objects": [],
            "anchor": {
                "label": "checkpoint",
                "runtime": {},
                "metadata": {}
            }
        });
        let parsed: StateAnchorPublishParams =
            serde_json::from_value(request.clone()).expect("current state-anchor request");
        assert!(matches!(
            parsed.subject,
            ryeos_state::objects::StateAnchorSubject::Graph { .. }
        ));

        request.as_object_mut().unwrap().remove("subject");
        request["graph_run_id"] = json!("G-test");
        assert!(serde_json::from_value::<StateAnchorPublishParams>(request).is_err());
    }

    #[test]
    fn immutable_execution_schema_mismatch_gets_the_explicit_cutover_command() {
        let mismatch = anyhow::Error::new(ryeos_state::IncompatibleCurrentObjectSchema::new(
            "thread snapshot",
            7,
            8,
        ))
        .context("decode current snapshot");
        let error = with_execution_schema_cutover_hint(mismatch);
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<ryeos_state::IncompatibleCurrentObjectSchema>())
        );

        let unrelated = with_execution_schema_cutover_hint(anyhow!("untrusted signed head"));
        assert!(
            !format!("{unrelated:#}")
                .contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );

        let newer = with_execution_schema_cutover_hint(anyhow::Error::new(
            ryeos_state::IncompatibleCurrentObjectSchema::new("thread snapshot", 9, 8),
        ));
        assert!(
            !format!("{newer:#}")
                .contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );
    }

    #[test]
    fn original_state_store_mutex_deadline_refuses_while_actual_guard_remains_held() {
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let original_guard = store.inner.lock().unwrap();
        let original =
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(40));
        let waiting = Arc::clone(&store);
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sent.send(waiting.lock_until(original).map(|_| ())).unwrap();
        });
        let result = received.recv_timeout(std::time::Duration::from_secs(2));
        drop(original_guard); // always release actual original mutex before joining/assertion
        worker.join().unwrap();
        assert!(
            result
                .expect("actual StateStore waiter must stop before guard release")
                .is_err()
        );
        assert!(original.has_elapsed());
        assert!(store.lock().is_ok());
    }

    #[test]
    fn original_state_store_expiry_cannot_enter_runtime_sql_operation() {
        let store = test_store();
        let entered = AtomicBool::new(false);
        let result = store.with_runtime_original_deadline(
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
            |db| {
                entered.store(true, Ordering::Release);
                db.process_resource_reservations()
            },
        );
        assert!(result.is_err());
        assert!(!entered.load(Ordering::Acquire));
        assert!(store.lock().is_ok());
    }

    fn test_store() -> TestStateStore {
        let tmp = tempdir().expect("tempdir").keep();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
                .expect("test node identity");
        let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        TestStateStore::open(&tmp, signer, Arc::new(head_trust)).expect("state store")
    }

    #[test]
    fn store_owned_active_access_and_release_share_actual_guard() {
        let store = test_store();
        let thread = "T-store-active-release";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread, "claim", "daemon:test")
            .unwrap()
            .unwrap();
        {
            let g = store.lock().unwrap();
            assert!(StateStore::is_launch_owner_active_locked(
                &g,
                &claim.claimed_by
            ));
            assert_eq!(
                g.runtime_db
                    .get_launch_claim(thread)
                    .unwrap()
                    .unwrap()
                    .claimed_by,
                claim.claimed_by
            );
        }
        assert!(
            store
                .release_active_thread_launch_claim(thread, &claim.claim_id, &claim.claimed_by)
                .unwrap()
        );
        let g = store.lock().unwrap();
        assert!(g.runtime_db.get_launch_claim(thread).unwrap().is_none());
        assert!(!StateStore::is_launch_owner_active_locked(
            &g,
            &claim.claimed_by
        ));
    }

    #[test]
    fn prepared_rotation_mismatch_rolls_back_epoch_attempt_and_preserves_active_owner() {
        let store = test_store();
        let thread = "T-prepared-rotation";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread, "first", "daemon:test")
            .unwrap()
            .unwrap();
        let g = store.lock().unwrap();
        let permit = store
            .selected_mutation_permit(
                &g,
                SelectedMutationOperation::Rotate(
                    thread,
                    &claim.claimed_by,
                    "next",
                    "daemon:test",
                    3,
                ),
            )
            .unwrap();
        let wrong = runtime_db::LaunchOwner {
            thread_id: thread.to_owned(),
            monotonic_launch_epoch: claim.owner.monotonic_launch_epoch + 2,
            unpredictable_nonce: "next".to_owned(),
            daemon_generation_id: "daemon:test".to_owned(),
        };
        assert!(
            g.runtime_db
                .rotate_thread_launch_claim_after_runtime_recovery(
                    &permit,
                    thread,
                    &claim.claimed_by,
                    "next",
                    "daemon:test",
                    3,
                    &wrong
                )
                .is_err()
        );
        assert_eq!(
            g.runtime_db.fixture_launch_epoch(thread).unwrap(),
            Some(claim.owner.monotonic_launch_epoch as i64)
        );
        assert_eq!(g.runtime_db.get_resume_attempts(thread).unwrap(), 0);
        assert_eq!(
            g.runtime_db
                .get_launch_claim(thread)
                .unwrap()
                .unwrap()
                .claimed_by,
            claim.claimed_by
        );
        assert!(StateStore::is_launch_owner_active_locked(
            &g,
            &claim.claimed_by
        ));
    }

    #[test]
    fn prepared_rotation_publishes_exact_replacement_under_same_store() {
        let store = test_store();
        let thread = "T-prepared-rotation-success";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let first = store
            .claim_thread_launch_active(thread, "first", "daemon:test")
            .unwrap()
            .unwrap();
        let outcome = store
            .rotate_active_thread_launch_claim_after_runtime_recovery(
                thread,
                &first.claimed_by,
                "next",
                "daemon:test",
                3,
            )
            .unwrap();
        let runtime_db::RuntimeRecoveryClaimRotation::Rotated {
            claim,
            next_attempt,
        } = outcome
        else {
            panic!("expected rotation");
        };
        let g = store.lock().unwrap();
        assert_eq!(next_attempt, 1);
        assert_eq!(
            claim.owner.monotonic_launch_epoch,
            first.owner.monotonic_launch_epoch + 1
        );
        assert_eq!(
            g.runtime_db
                .get_launch_claim(thread)
                .unwrap()
                .unwrap()
                .claimed_by,
            claim.claimed_by
        );
        assert!(!StateStore::is_launch_owner_active_locked(
            &g,
            &first.claimed_by
        ));
        assert!(StateStore::is_launch_owner_active_locked(
            &g,
            &claim.claimed_by
        ));
    }

    // Row-only fixtures; no selected issuer, native contact or settlement proof.
    fn selected_fence_fixture(
        phase: crate::selected_invocation_bookkeeping::SelectedFixturePhase,
        refused: bool,
    ) -> (TestStateStore, runtime_db::LaunchClaim) {
        let store = test_store();
        let thread = "T-selected-fence";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
            .unwrap()
            .unwrap();
        let borrower = store.fixture_selected_restored_metadata(thread).unwrap();
        {
            let _g = store.lock().unwrap();
            store
                .selected_invocations
                .lock()
                .unwrap()
                .fixture_retained_phase(borrower.invocation(), phase, refused);
        }
        (store, claim)
    }

    #[test]
    fn selected_fences_all_phases_and_refusal_before_claim_release_rotation_and_sweep() {
        use crate::selected_invocation_bookkeeping::SelectedFixturePhase::*;
        for phase in [Pending, Occupied, Borrowed, RestoredAwaitingBorrowAck] {
            for refused in [false, true] {
                let (store, claim) = selected_fence_fixture(phase, refused);
                assert!(
                    store
                        .release_thread_launch_claim(&claim.thread_id, &claim.claim_id)
                        .is_err()
                );
                assert!(
                    store
                        .release_thread_launch_claim(&claim.thread_id, "stale")
                        .is_err()
                );
                assert!(
                    store
                        .release_active_thread_launch_claim(&claim.thread_id, "stale", "wrong")
                        .is_err()
                );
                assert!(
                    store
                        .release_active_thread_launch_claim(
                            &claim.thread_id,
                            &claim.claim_id,
                            &claim.claimed_by
                        )
                        .is_err()
                );
                assert!(
                    store
                        .claim_thread_launch(&claim.thread_id, "new", "foreign")
                        .is_err()
                );
                assert!(
                    store
                        .claim_thread_launch_active(
                            &claim.thread_id,
                            "new",
                            runtime_db::daemon_generation_id()
                        )
                        .is_err()
                );
                assert!(
                    store
                        .rotate_active_thread_launch_claim_after_runtime_recovery(
                            &claim.thread_id,
                            &claim.claimed_by,
                            "next",
                            runtime_db::daemon_generation_id(),
                            3
                        )
                        .is_err()
                );
                assert!(
                    store
                        .clear_stale_launch_claims("foreign-generation")
                        .is_err()
                );
                let retained = store.get_launch_claim(&claim.thread_id).unwrap().unwrap();
                assert_eq!(retained.claim_id, claim.claim_id);
                assert_eq!(retained.claimed_by, claim.claimed_by);
                assert!(
                    store
                        .lock()
                        .unwrap()
                        .active_launch_owners
                        .contains(&claim.claimed_by)
                );
                let g = store.lock().unwrap();
                assert_eq!(
                    g.runtime_db.get_resume_attempts(&claim.thread_id).unwrap(),
                    0
                );
                assert!(
                    store
                        .delete_thread_runtime_locked(&g, &claim.thread_id)
                        .is_err()
                );
                assert!(store.require_selected_bulk_exclusion(&g).is_err());
            }
        }
    }

    #[test]
    fn selected_fences_missing_claim_and_unpublished_id_without_new_epoch_or_rollback() {
        use crate::selected_invocation_bookkeeping::SelectedFixturePhase::Pending;
        let (store, claim) = selected_fence_fixture(Pending, false);
        {
            let g = store.lock().unwrap();
            g.runtime_db
                .fixture_release_thread_launch_claim(&claim.thread_id, &claim.claim_id)
                .unwrap();
        }
        let before_epoch = store
            .lock()
            .unwrap()
            .runtime_db
            .fixture_launch_epoch(&claim.thread_id)
            .unwrap();
        assert!(
            store
                .claim_thread_launch(&claim.thread_id, "replacement", "foreign")
                .is_err()
        );
        assert!(store.get_launch_claim(&claim.thread_id).unwrap().is_none());
        assert_eq!(
            store
                .lock()
                .unwrap()
                .runtime_db
                .fixture_launch_epoch(&claim.thread_id)
                .unwrap(),
            before_epoch
        );
        let fresh = "T-selected-unpublished";
        let fresh_claim = store
            .reserve_fresh_thread_launch_active(fresh, "fresh", runtime_db::daemon_generation_id())
            .unwrap()
            .unwrap();
        let borrower = store.fixture_selected_restored_metadata(fresh).unwrap();
        {
            let g = store.lock().unwrap();
            g.runtime_db
                .fixture_release_thread_launch_claim(fresh, &fresh_claim.claim_id)
                .unwrap();
        }
        let fresh_epoch = store
            .lock()
            .unwrap()
            .runtime_db
            .fixture_launch_epoch(fresh)
            .unwrap();
        assert!(
            store
                .reserve_fresh_thread_launch(fresh, "new", "foreign")
                .is_err()
        );
        assert!(
            store
                .reserve_fresh_thread_launch_active(
                    fresh,
                    "new",
                    runtime_db::daemon_generation_id()
                )
                .is_err()
        );
        assert!(store.get_launch_claim(fresh).unwrap().is_none());
        assert_eq!(
            store
                .lock()
                .unwrap()
                .runtime_db
                .fixture_launch_epoch(fresh)
                .unwrap(),
            fresh_epoch
        );
        assert!(store.mark_selected_metadata_restored(&borrower).is_err());
        assert!(
            store
                .lock()
                .unwrap()
                .active_launch_owners
                .contains(&fresh_claim.claimed_by)
        );
    }

    #[test]
    fn selected_live_permit_refuses_wrong_store_db_target_and_operation() {
        let store = test_store();
        // One Store is live per process, so the other Store is its identity
        // alone: an address that is not this Store's mutex. The other Runtime
        // is a real store opened beside it on the same test host.
        let other_root = tempdir().unwrap();
        let other_db = test_native::with_store_host(|host| {
            runtime_db::test_native::open_live(host, &other_root.path().join("runtime.sqlite3"))
        });
        let g = store.lock().unwrap();
        assert!(store.owns_store_mutex(g.store_mutex));
        assert!(!store.owns_store_mutex(std::ptr::null()));
        let permit = store
            .selected_mutation_permit(&g, SelectedMutationOperation::Release("T", "claim"))
            .unwrap();
        assert!(
            other_db
                .release_thread_launch_claim(&permit, "T", "claim")
                .is_err()
        );
        assert!(
            g.runtime_db
                .release_thread_launch_claim(&permit, "other", "claim")
                .is_err()
        );
        assert!(
            g.runtime_db
                .claim_thread_launch(&permit, "T", "claim", "generation")
                .is_err()
        );
        assert!(
            !g.runtime_db
                .release_thread_launch_claim(&permit, "T", "claim")
                .unwrap()
        );
        drop(g);
        test_native::with_store_host(|host| runtime_db::test_native::retire(host, other_db));
    }

    #[test]
    fn selected_registry_poison_refuses_without_durable_claim_change() {
        const TEST: &str = concat!(
            module_path!(),
            "::selected_registry_poison_refuses_without_durable_claim_change"
        );
        // A poisoned registry refuses the store's close, so its native owners
        // end only with the process: the scenario runs in a child.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "poisoned registry kept the durable claim");
            return;
        }
        let store = test_store();
        let thread = "T-selected-poison";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
            .unwrap()
            .unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Deliberate private poison injection isolates registry refusal;
            // no ordinary SQL or selected capability is issued by this fixture.
            let _selected = store.selected_invocations.lock().unwrap();
            panic!("injected selected registry poison");
        }));
        assert!(
            store
                .release_thread_launch_claim(thread, &claim.claim_id)
                .is_err()
        );
        {
            let g = store.lock().unwrap();
            assert_eq!(
                g.runtime_db
                    .get_launch_claim(thread)
                    .unwrap()
                    .unwrap()
                    .claim_id,
                claim.claim_id
            );
        }
        let host = crate::state_store::test_native::take_shared_host();
        lillux_sqlite::test_host::end_isolated_process(
            (claim, store, host),
            "poisoned registry kept the durable claim",
        )
    }

    #[test]
    fn selected_pending_birth_veto_retains_reservation_and_runtime_row() {
        let store = test_store();
        let thread = "T-selected-pending-birth";
        let claim = store
            .reserve_fresh_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
            .unwrap()
            .unwrap();
        {
            let g = store.lock().unwrap();
            g.runtime_db
                .reserve_in_process_handler_birth(thread, thread, &in_process_launch_metadata())
                .unwrap();
        }
        store.fixture_selected_restored_metadata(thread).unwrap();
        assert!(
            store
                .discard_uncommitted_in_process_handler_birth(thread)
                .is_err()
        );
        let g = store.lock().unwrap();
        assert!(
            store
                .selected_mutation_permit(&g, SelectedMutationOperation::DeleteThread(thread))
                .is_err()
        );
        assert!(
            g.runtime_db
                .in_process_handler_reservation(thread)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            g.runtime_db
                .get_launch_claim(thread)
                .unwrap()
                .unwrap()
                .claim_id,
            claim.claim_id
        );
        assert_eq!(
            g.runtime_db
                .inspect_thread_history_discard()
                .unwrap()
                .thread_runtime,
            1
        );
    }

    #[test]
    fn selected_first_remove_and_head_absent_replay_preserve_exact_journal_and_claim() {
        let store = test_store();
        let thread = "T-selected-terminal";
        let mut record = thread_record(thread, thread);
        record.captured_history_policy.as_mut().unwrap().retention =
            ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 };
        store.create_thread_for_test(&record).unwrap();
        let claim = store
            .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
            .unwrap()
            .unwrap();
        finish_workspace_test_thread(&store, thread);
        store.fixture_selected_restored_metadata(thread).unwrap();
        {
            let g = store.lock().unwrap();
            let lock = g.state_db.acquire_chain_lock(thread).unwrap();
            let chain = g
                .state_db
                .authoritative_terminal_chain_under_lock(thread, &lock)
                .unwrap()
                .unwrap();
            let mut result = TerminalChainRetirement::default();
            assert!(
                store
                    .finish_terminal_chain_removal(&g, &chain, &lock, &[], false, &mut result)
                    .is_err()
            );
            assert!(
                g.state_db
                    .pending_chain_transition(thread)
                    .unwrap()
                    .is_none()
            );
            assert!(
                g.state_db
                    .authoritative_terminal_chain_under_lock(thread, &lock)
                    .unwrap()
                    .is_some()
            );
            // Deliberately construct an actual crash-boundary contradiction in
            // private unit code; no production issuer or cleanup permission.
            assert!(g.state_db.remove_chain_head_ref(thread, &lock).unwrap());
        }
        let before = store
            .lock()
            .unwrap()
            .state_db
            .pending_chain_transition(thread)
            .unwrap()
            .unwrap()
            .transition_id;
        assert!(
            store
                .recover_pending_terminal_chain_removals("2099-01-01T00:00:00Z", false, |_| Ok(0))
                .is_err()
        );
        let g = store.lock().unwrap();
        assert_eq!(
            g.state_db
                .pending_chain_transition(thread)
                .unwrap()
                .unwrap()
                .transition_id,
            before
        );
        assert_eq!(
            g.runtime_db
                .get_launch_claim(thread)
                .unwrap()
                .unwrap()
                .claim_id,
            claim.claim_id
        );
        assert_eq!(
            g.runtime_db
                .inspect_thread_history_discard()
                .unwrap()
                .thread_runtime,
            1
        );
    }

    #[test]
    fn selected_expected_and_advanced_replay_keep_actual_signed_heads_and_remove_cursor() {
        for advanced in [false, true] {
            let store = test_store();
            let thread = "T-selected-replay";
            store
                .create_thread_for_test(&thread_record(thread, thread))
                .unwrap();
            let claim = store
                .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
                .unwrap()
                .unwrap();
            let head_path = store
                .lock()
                .unwrap()
                .state_db
                .refs_root()
                .join("generic/chains")
                .join(thread)
                .join("head");
            let expected = std::fs::read(&head_path).unwrap();
            finish_workspace_test_thread(&store, thread);
            let newer = std::fs::read(&head_path).unwrap();
            assert_ne!(expected, newer);
            store.fixture_selected_restored_metadata(thread).unwrap();
            let transition = {
                let g = store.lock().unwrap();
                let lock = g.state_db.acquire_chain_lock(thread).unwrap();
                // Private crash fixture restores a genuinely signed earlier
                // head, then records an actual Remove and a visible signed head.
                std::fs::write(&head_path, &expected).unwrap();
                g.state_db.remove_chain_head_ref(thread, &lock).unwrap();
                std::fs::write(&head_path, if advanced { &newer } else { &expected }).unwrap();
                let transition = g
                    .state_db
                    .pending_chain_transition(thread)
                    .unwrap()
                    .unwrap();
                let actual = g
                    .state_db
                    .pending_chain_removal_head_state(thread, &transition.transition_id, &lock)
                    .unwrap();
                if advanced {
                    assert!(matches!(
                        actual,
                        ryeos_state::PendingRemoveHeadState::AdvancedHeadVisible { .. }
                    ));
                } else {
                    assert!(matches!(
                        actual,
                        ryeos_state::PendingRemoveHeadState::ExpectedHeadVisible
                    ));
                }
                transition.transition_id
            };
            let head_before = std::fs::read(&head_path).unwrap();
            assert!(
                store
                    .recover_pending_terminal_chain_removals("2099-01-01T00:00:00Z", false, |_| Ok(
                        0
                    ))
                    .is_err()
            );
            let g = store.lock().unwrap();
            assert_eq!(
                g.state_db
                    .pending_chain_transition(thread)
                    .unwrap()
                    .unwrap()
                    .transition_id,
                transition
            );
            assert_eq!(std::fs::read(&head_path).unwrap(), head_before);
            assert_eq!(
                g.runtime_db
                    .get_launch_claim(thread)
                    .unwrap()
                    .unwrap()
                    .claim_id,
                claim.claim_id
            );
            assert_eq!(
                g.runtime_db
                    .inspect_thread_history_discard()
                    .unwrap()
                    .thread_runtime,
                1
            );
        }
    }

    #[test]
    fn selected_bulk_retirement_veto_preserves_unrelated_due_chain() {
        use crate::selected_invocation_bookkeeping::SelectedFixturePhase::Occupied;
        let (store, _) = selected_fence_fixture(Occupied, false);
        let due = "T-unrelated-due";
        let mut record = thread_record(due, due);
        record.captured_history_policy.as_mut().unwrap().retention =
            ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 };
        store.create_thread_for_test(&record).unwrap();
        finish_workspace_test_thread(&store, due);
        assert!(
            store
                .retire_due_terminal_chains("2099-01-01T00:00:00Z", false, |_| Ok(0))
                .is_err()
        );
        let g = store.lock().unwrap();
        assert!(g.state_db.pending_chain_transition(due).unwrap().is_none());
        let lock = g.state_db.acquire_chain_lock(due).unwrap();
        assert!(
            g.state_db
                .authoritative_terminal_chain_under_lock(due, &lock)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn empty_selected_domain_preserves_ordinary_head_absent_cleanup() {
        let store = test_store();
        let thread = "T-ordinary-replay";
        let mut record = thread_record(thread, thread);
        record.captured_history_policy.as_mut().unwrap().retention =
            ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 };
        store.create_thread_for_test(&record).unwrap();
        finish_workspace_test_thread(&store, thread);
        {
            let g = store.lock().unwrap();
            let lock = g.state_db.acquire_chain_lock(thread).unwrap();
            assert!(g.state_db.remove_chain_head_ref(thread, &lock).unwrap());
        }
        store
            .recover_pending_terminal_chain_removals("2099-01-01T00:00:00Z", false, |_| Ok(0))
            .unwrap();
        let g = store.lock().unwrap();
        assert!(
            g.state_db
                .pending_chain_transition(thread)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            g.runtime_db
                .inspect_thread_history_discard()
                .unwrap()
                .thread_runtime,
            0
        );
    }

    fn fenced_kind_chain(store: &StateStore, thread: &str) {
        let mut record = thread_record(thread, thread);
        record.kind = "fenced_test_kind".to_string();
        record.captured_history_policy.as_mut().unwrap().retention =
            ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 };
        store.create_thread_for_test(&record).unwrap();
        finish_workspace_test_thread(store, thread);
    }

    fn fenced_test_kinds() -> ryeos_state::FencedChainKinds {
        ryeos_state::FencedChainKinds::new(["fenced_test_kind".to_string()]).unwrap()
    }

    #[test]
    fn ordinary_retention_leaves_fenced_kind_chains_for_the_fenced_path() {
        let store = test_store();
        crate::object_contracts::install_retained_evidence_validator(
            crate::object_contracts::accept_evidence,
        )
        .unwrap();
        let thread = "T-fenced-kind";
        fenced_kind_chain(&store, thread);
        store
            .install_fenced_chain_kinds(&fenced_test_kinds())
            .unwrap();
        let now = "2099-01-01T00:00:00Z";

        let ordinary = store
            .retire_due_terminal_chains(now, false, |_| Ok(0))
            .unwrap();
        assert_eq!(ordinary.retired_chains, 0);
        {
            let g = store.lock().unwrap();
            let lock = g.state_db.acquire_chain_lock(thread).unwrap();
            assert!(
                g.state_db
                    .authoritative_terminal_chain_under_lock(thread, &lock)
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(
            store
                .due_fenced_chain_retirement_slice(now, None, 8)
                .unwrap(),
            vec![thread.to_string()]
        );

        // A pinned chain is refused, not retired around its pin.
        let plan = FencedChainRetirementPlan::default();
        assert!(
            store
                .retire_fenced_chain(thread, now, &plan, |_| Ok(1))
                .is_err()
        );
        let retired = store
            .retire_fenced_chain(thread, now, &plan, |_| Ok(0))
            .unwrap();
        assert_eq!(retired.retired_chains, 1);
        let g = store.lock().unwrap();
        let lock = g.state_db.acquire_chain_lock(thread).unwrap();
        assert!(
            g.state_db
                .authoritative_terminal_chain_under_lock(thread, &lock)
                .unwrap()
                .is_none()
        );
        assert!(
            g.state_db
                .pending_chain_transition(thread)
                .unwrap()
                .is_none()
        );
        // The retirement is complete and not repeatable as a fresh one.
        drop(lock);
        drop(g);
        assert!(
            store
                .retire_fenced_chain(thread, now, &plan, |_| Ok(0))
                .is_err()
        );
    }

    #[test]
    fn fenced_retirement_refuses_an_unfenced_kind_and_a_different_kind_set() {
        let store = test_store();
        crate::object_contracts::install_retained_evidence_validator(
            crate::object_contracts::accept_evidence,
        )
        .unwrap();
        let now = "2099-01-01T00:00:00Z";
        let plan = FencedChainRetirementPlan::default();
        // Nothing installed: the fenced path refuses rather than assuming.
        assert!(
            store
                .due_fenced_chain_retirement_slice(now, None, 8)
                .is_err()
        );
        assert!(
            store
                .retire_fenced_chain("T-any", now, &plan, |_| Ok(0))
                .is_err()
        );

        store
            .install_fenced_chain_kinds(&fenced_test_kinds())
            .unwrap();
        store
            .install_fenced_chain_kinds(&fenced_test_kinds())
            .unwrap();
        let other = ryeos_state::FencedChainKinds::new(["other_kind".to_string()]).unwrap();
        assert!(store.install_fenced_chain_kinds(&other).is_err());

        let thread = "T-ordinary-kind";
        let mut record = thread_record(thread, thread);
        record.captured_history_policy.as_mut().unwrap().retention =
            ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 };
        store.create_thread_for_test(&record).unwrap();
        finish_workspace_test_thread(&store, thread);
        assert!(
            store
                .retire_fenced_chain(thread, now, &plan, |_| Ok(0))
                .is_err()
        );
        assert!(
            store
                .due_fenced_chain_retirement_slice(now, None, 8)
                .unwrap()
                .is_empty()
        );
        // The ordinary kind still retires through the ordinary path.
        assert_eq!(
            store
                .retire_due_terminal_chains(now, false, |_| Ok(0))
                .unwrap()
                .retired_chains,
            1
        );
    }

    #[test]
    fn selected_metadata_is_shared_and_ack_requires_actual_claim_and_active_owner() {
        use crate::selected_invocation_bookkeeping::SelectedBorrowReturn;
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let same_store = Arc::clone(&store);
        let generation = store.register_selected_bookkeeping_generation().unwrap();
        let thread = "T-selected-ack";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
            .unwrap()
            .unwrap();
        let borrower = same_store
            .selected_invocations
            .lock()
            .unwrap()
            .fixture_restored_borrow(&generation, thread, &claim.claim_id, &claim.claimed_by);
        store
            .lock()
            .unwrap()
            .active_launch_owners
            .remove(&claim.claimed_by);
        assert!(same_store.ack_selected_borrow_return(&borrower).is_err());
        assert!(
            store
                .selected_invocations
                .lock()
                .unwrap()
                .fixture_awaiting_ack(&borrower)
        );
        store
            .lock()
            .unwrap()
            .active_launch_owners
            .insert(claim.claimed_by.clone());
        assert_eq!(
            same_store.ack_selected_borrow_return(&borrower).unwrap(),
            SelectedBorrowReturn::Returned
        );
        assert_eq!(
            store.ack_selected_borrow_return(&borrower).unwrap(),
            SelectedBorrowReturn::AlreadyReturned
        );
        // One Store is live per process: the first closes before the other
        // opens, and the borrower is plain data that outlives its Store.
        drop((store, same_store));
        store_owner.close().unwrap();
        let other = test_store();
        other.register_selected_bookkeeping_generation().unwrap();
        assert!(other.ack_selected_borrow_return(&borrower).is_err());
    }

    #[test]
    fn selected_return_retries_after_close_but_new_metadata_borrow_refuses() {
        let store = test_store();
        let generation = store.register_selected_bookkeeping_generation().unwrap();
        let thread = "T-selected-return-close";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
            .unwrap()
            .unwrap();
        let first = store
            .selected_invocations
            .lock()
            .unwrap()
            .fixture_restored_borrow(&generation, thread, &claim.claim_id, &claim.claimed_by);
        let ticket = first.invocation().clone();
        store.mark_selected_metadata_restored(&first).unwrap();
        store.ack_selected_borrow_return(&first).unwrap();
        let second = store.begin_selected_metadata_borrow(&ticket).unwrap();
        assert!(store.mark_selected_metadata_restored(&first).is_err());
        store.close_process_attachment_admission().unwrap();
        store.mark_selected_metadata_restored(&second).unwrap();
        store.mark_selected_metadata_restored(&second).unwrap();
        assert_eq!(
            store.ack_selected_borrow_return(&second).unwrap(),
            crate::SelectedBorrowReturn::Returned
        );
        assert_eq!(
            store.ack_selected_borrow_return(&second).unwrap(),
            crate::SelectedBorrowReturn::AlreadyReturned
        );
        assert!(store.begin_selected_metadata_borrow(&ticket).is_err());
        assert!(store.ack_selected_borrow_return(&first).is_err());
    }

    #[test]
    fn selected_missing_durable_claim_keeps_restored_metadata_pending() {
        let store = test_store();
        let generation = store.register_selected_bookkeeping_generation().unwrap();
        let thread = "T-selected-missing-claim";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
            .unwrap()
            .unwrap();
        let borrower = store
            .selected_invocations
            .lock()
            .unwrap()
            .fixture_restored_borrow(&generation, thread, &claim.claim_id, &claim.claimed_by);
        // Deliberate private unit-fixture corruption, not a Store bypass API.
        store
            .lock()
            .unwrap()
            .runtime_db
            .fixture_release_thread_launch_claim(thread, &claim.claim_id)
            .unwrap();
        assert!(store.ack_selected_borrow_return(&borrower).is_err());
        assert!(
            store
                .selected_invocations
                .lock()
                .unwrap()
                .fixture_awaiting_ack(&borrower)
        );
    }

    #[test]
    fn active_claim_release_refuses_stale_claim_or_wrong_owner_without_mutation() {
        let store = test_store();
        let thread = "T-release-order";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread, "claim-current", "daemon:test")
            .unwrap()
            .unwrap();
        assert!(
            !store
                .release_active_thread_launch_claim(thread, "claim-stale", &claim.claimed_by)
                .unwrap()
        );
        assert!(store.is_launch_owner_active(&claim.claimed_by));
        let retained = store.get_launch_claim(thread).unwrap().unwrap();
        assert_eq!(retained.claim_id, claim.claim_id);
        assert_eq!(retained.claimed_by, claim.claimed_by);
        assert!(
            !store
                .release_active_thread_launch_claim(thread, &claim.claim_id, "wrong-owner")
                .unwrap()
        );
        assert!(store.is_launch_owner_active(&claim.claimed_by));
        assert_eq!(
            store.get_launch_claim(thread).unwrap().unwrap().claimed_by,
            claim.claimed_by
        );
        // Model an already-absent durable row while the actual active registry
        // still retains this owner. A missing-row refusal must not erase it.
        assert!(
            store
                .release_thread_launch_claim(thread, &claim.claim_id)
                .unwrap()
        );
        assert!(
            !store
                .release_active_thread_launch_claim(thread, &claim.claim_id, &claim.claimed_by)
                .unwrap()
        );
        assert!(store.is_launch_owner_active(&claim.claimed_by));
    }

    #[test]
    fn exact_active_claim_release_acknowledges_both_domains_and_preserves_replacement() {
        let store = test_store();
        let thread = "T-release-replaced";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let first = store
            .claim_thread_launch_active(thread, "claim-first", "daemon:test")
            .unwrap()
            .unwrap();
        assert!(
            store
                .release_active_thread_launch_claim(thread, &first.claim_id, &first.claimed_by)
                .unwrap()
        );
        assert!(store.get_launch_claim(thread).unwrap().is_none());
        assert!(!store.is_launch_owner_active(&first.claimed_by));
        let replacement = store
            .claim_thread_launch_active(thread, "claim-replacement", "daemon:test")
            .unwrap()
            .unwrap();
        assert!(
            !store
                .release_active_thread_launch_claim(thread, &first.claim_id, &first.claimed_by)
                .unwrap()
        );
        assert!(store.is_launch_owner_active(&replacement.claimed_by));
        let retained = store.get_launch_claim(thread).unwrap().unwrap();
        assert_eq!(retained.claim_id, replacement.claim_id);
        assert_eq!(retained.claimed_by, replacement.claimed_by);
    }

    // Signed persistence fixture only: no RootAdmission, compiler token, or
    // claim that the resolver/launcher admitted these storage inputs. Birth,
    // substrate attestation, capsule verification and finalization are real.
    fn signed_direct_storage_root(store: &StateStore, thread_id: &str, external: bool) {
        use ryeos_state::objects::*;
        let cas = store.state_authority.cas_store().unwrap();
        let command_hash = cas.store_blob(b"storage-only executable bytes").unwrap();
        let project = ExecutionProjectAuthority::pinned(
            "project:direct-storage".into(),
            None,
            "b".repeat(64),
            PinnedProjectRealization::ReadOnly,
            EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        let mut metadata = external_fixture_launch_metadata(
            project.clone(),
            json!({"binding_records":{}}),
            json!({}),
        );
        let root_history = metadata
            .sealed_root_request
            .as_ref()
            .unwrap()
            .captured_history_policy()
            .clone();
        let AdmittedLaunchArtifactIdentity::ManagedRuntime {
            runtime_ref,
            runtime_content_hash,
            runtime_signer_fingerprint,
            protocol_ref,
            protocol_content_hash,
            protocol_signer_fingerprint,
            executor_ref,
            ..
        } = metadata.admitted_artifact_identity.take().unwrap()
        else {
            unreachable!()
        };
        let AdmittedExecutionClosure::ManagedRuntime {
            protocol_descriptor_document,
            ..
        } = metadata.admitted_execution_closure.take().unwrap()
        else {
            unreachable!()
        };
        let runtime = DirectRuntimeIdentity {
            runtime_ref,
            runtime_content_hash,
            runtime_signer_fingerprint,
            runtime_source_space: DirectRuntimeSourceSpace::Project,
            runtime_bundle_manifest_hash: None,
            runtime_bundle_signer_fingerprint: None,
        };
        let command_path = admitted_direct_command_execution_path(
            &command_hash,
            std::path::Path::new("/storage-command"),
        )
        .unwrap();
        let plan: ryeos_engine::contracts::ExecutionPlan = serde_json::from_value(json!({
            "plan_id":"signed-storage-direct", "root_executor_id":executor_ref,
            "root_ref":"graph:test/storage-fixture", "item_kind":"graph",
            "nodes":[{"node_type":"dispatch_subprocess","id":"spawn","spec":{
                "cmd":command_path,"verified_command":if external {json!({"authority":"captured_content",
                    "code":{"source_path":command_path,"content_hash":command_hash}})} else {Value::Null},
                "args":[],"cwd":"/workspace","env":{},"stdin":null,"timeout_secs":30
            },"executor_chain":["graph:test/storage-fixture",executor_ref]}],
            "entrypoint":"spawn", "capabilities":{"requires_model":false,
                "requires_subprocess":true,"requires_network":false,"custom":[]},
            "materialization_requirements":[],"network_authority_ceiling":"node_policy",
            "filesystem_authority_ceiling":"node_policy","target_requirement":null,
            "endpoint_requirement":if external {json!({"kind":"external",
                "binding_id":"storage-direct","stdout_max_bytes":4096,"stderr_max_bytes":4096})}
                else {json!({"kind":"local"})},
            "external_endpoint_binding":if external {json!({"binding_id":"storage-direct",
                "binding_digest":"c".repeat(64)})} else {Value::Null},
            "subprocess_limits":null, "resource_authority_ceiling":"node_policy","cache_key":"storage",
            "executor_chain":["graph:test/storage-fixture",executor_ref],
            "executor_authorities":[],"runtime_identity":runtime
        }))
        .unwrap();
        plan.validate_endpoint_for_sealing().unwrap();
        let execution_plan = serde_json::to_value(plan).unwrap();
        metadata.launch_driver = Some(ExecutionLaunchDriver::DirectItemExecutor);
        if !external {
            metadata
                .resume_context
                .as_mut()
                .unwrap()
                .lifecycle_authority = ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE;
        }
        metadata.admitted_artifact_identity =
            Some(AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                executor_ref: executor_ref.clone(),
                root_subject_source_content_digest: root_history.item_content_hash.clone(),
                root_subject_signer_fingerprint: root_history.item_signer_fingerprint.clone(),
                root_subject_source_identity: DirectRootSourceIdentity::Project,
                protocol_ref,
                protocol_content_hash,
                protocol_signer_fingerprint,
                execution_plan_hash: lillux::sha256_hex(
                    lillux::canonical_json(&execution_plan).unwrap().as_bytes(),
                ),
                executable_identity: if external {
                    DirectExecutableIdentity::CapturedContent {
                        content_hash: command_hash.clone(),
                    }
                } else {
                    DirectExecutableIdentity::NodePolicy
                },
                runtime_identity: runtime.clone(),
            });
        metadata.admitted_execution_closure = Some(AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan,
            protocol_descriptor_document,
            admitted_project_root: None,
            command: if external {
                AdmittedDirectCommandClosure::ContentAddressed {
                    executable_blob_hash: command_hash,
                    execution_path: command_path,
                }
            } else {
                AdmittedDirectCommandClosure::NodePolicy
            },
        });
        let provisional = metadata.admitted_launch_capsule().unwrap().unwrap();
        let authority = provisional.launch_authority();
        let cas_root = store.cas_root().unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::load(
            &cas_root.ancestors().nth(3).unwrap().join("node-key.pem"),
        )
        .unwrap();
        let substrate = crate::execution_identity_probe::boot_node_execution_identity(
            store,
            &identity,
            &crate::build_info::get(),
        )
        .unwrap();
        let realization = AdmittedExecutionRealization {
            schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ADMITTED_EXECUTION_REALIZATION_KIND.into(),
            substrate_identity_hash: substrate.identity_hash.clone(),
            substrate_attestation_hash: substrate.attestation_hash.clone(),
            launch_authority_digest: authority.digest().unwrap(),
            effective_definition_digest: provisional.exact_program["effective_definition_digest"]
                .as_str()
                .unwrap()
                .into(),
            artifact_identity_digest: authority.artifact_identity_digest().unwrap(),
            execution_closure_digest: authority.execution_closure_digest().unwrap(),
            contract_ref: runtime.runtime_ref,
            contract_digest: runtime.runtime_content_hash,
            components: Vec::new(),
            properties: BTreeMap::new(),
        };
        metadata.execution_realization_hash =
            Some(cas.store_object(&realization.to_value().unwrap()).unwrap());
        let mut thread = thread_record(thread_id, thread_id);
        let resume = metadata.resume_context.as_ref().unwrap();
        thread.kind = resume.kind.clone();
        thread.item_ref = resume.item_ref.clone();
        thread.captured_history_policy = Some(root_history);
        thread.launch_mode = resume.launch_mode.clone();
        thread.executor_ref = executor_ref;
        thread.project_authority = project;
        thread.base_project_snapshot_hash = Some("b".repeat(64));
        store
            .create_root_thread_for_test_with_launch_metadata(&thread, &metadata)
            .unwrap();
        let retained = store.admitted_launch_capsule(thread_id).unwrap().unwrap();
        assert_eq!(
            crate::thread_lifecycle::capsule_requires_external_direct(&retained).unwrap(),
            external,
        );
    }

    fn storage_direct_success() -> FinalizeThreadRecord {
        FinalizeThreadRecord {
            status: "completed".into(),
            outcome_code: Some("exit:0".into()),
            result_json: Some(json!({"answer":42})),
            error_json: None,
            artifacts: Vec::new(),
            final_cost: None,
            managed_envelope: None,
            result_project_snapshot_hash: None,
            result_workspace_output_capture_hash: None,
        }
    }

    #[test]
    fn signed_storage_external_direct_cannot_complete_without_allocation() {
        let store = test_store();
        let thread = "T-storage-external-direct";
        signed_direct_storage_root(&store, thread, true);
        assert!(store.external_allocation(thread).unwrap().is_none());
        let head = || {
            store
                .lock()
                .unwrap()
                .state_db
                .read_generic_head_ref("chains", thread)
                .unwrap()
                .unwrap()
                .target_hash
        };
        let before = head();
        let events = replayed_event_types(&store, thread);
        let error = store
            .finalize_thread(thread, &storage_direct_success())
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("external direct result has no allocation evidence"),
            "{error:#}"
        );
        assert_eq!(head(), before);
        assert_eq!(replayed_event_types(&store, thread), events);
        assert_eq!(store.get_thread(thread).unwrap().unwrap().status, "created");
        assert!(store.external_allocation(thread).unwrap().is_none());
    }

    #[test]
    fn external_finalization_preserves_signed_storage_local_node_policy() {
        let store = test_store();
        let thread = "T-storage-local-node-policy";
        signed_direct_storage_root(&store, thread, false);
        store
            .finalize_thread(thread, &storage_direct_success())
            .unwrap();
        let inner = store.lock().unwrap();
        let snapshot = authoritative_snapshot_for_transition(&inner, thread, thread).unwrap();
        assert_eq!(snapshot.status, ThreadStatus::Completed);
        assert_eq!(snapshot.result, Some(json!({"answer":42})));
        drop(inner);
        assert!(replayed_event_types(&store, thread).contains(&"thread_completed".into()));
        assert!(store.external_allocation(thread).unwrap().is_none());
    }

    fn direct_owner_test_reservation(
        store: &StateStore,
    ) -> runtime_db::external_execution::ExternalAllocationReservation {
        use runtime_db::external_execution::{
            ExternalAllocationOwner, ExternalAllocationReservation,
        };
        let thread = "T-direct-owner-proof";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let owner = store
            .claim_thread_launch_active(thread, "direct-proof-claim", "daemon:direct-proof")
            .unwrap()
            .unwrap()
            .owner;
        let binding=crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        let now = lillux::time::timestamp_millis();
        ExternalAllocationReservation {
            schema: runtime_db::external_execution::EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: thread.into(),
            admitted_capsule_hash: "a".repeat(64),
            owner: ExternalAllocationOwner::DirectThread {
                chain_root_id: thread.into(),
                launch_owner: owner,
                program: crate::thread_lifecycle::external_direct_program_test_fixture(),
            },
            base_snapshot_hash: "b".repeat(64),
            binding_hash: binding.digest().into(),
            capacity_owner: binding.capacity_owner().into(),
            channel_authority_generation: "3".repeat(64),
            channel_owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
                &lillux::crypto::SigningKey::from_bytes(&[19; 32]).verifying_key(),
            )
            .unwrap(),
            channel_bootstrap_capability_hash: "4".repeat(64),
            admitted_adapter_request_base64: None,
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            contact_deadline_ms: now + 30_000,
            startup_started_at_ms: now,
            startup_deadline_ms: now + 60_000,
        }
    }

    #[test]
    fn external_direct_finalization_binds_exact_ordinary_result_meaning() {
        // Content-comparison fixture, not proof of a born launch, signed
        // transcript, or cleanup. The production caller checks those under its
        // existing writer guard before comparing this projection.
        use ryeos_state::external_execution::{
            ExternalCommandOutputCommitment, ExternalCommandTermination,
            ExternalCommandTerminationReason, ExternalTargetExit,
        };
        let stdout = br#"{"answer":42}"#.to_vec();
        let commitment = |bytes: &[u8]| ExternalCommandOutputCommitment {
            bytes: bytes.len() as u64,
            sha256: lillux::sha256_hex(bytes),
            truncated: false,
        };
        let output = runtime_db::external_execution::ExternalDirectOutput {
            binding_digest: "a".repeat(64),
            terminal_sequence: 3,
            terminal_digest: "b".repeat(64),
            termination: ExternalCommandTermination {
                target_exit: ExternalTargetExit::Code(0),
                reason: ExternalCommandTerminationReason::TargetExited,
                stdout: commitment(&stdout),
                stderr: commitment(b""),
            },
            stdout,
            stderr: Vec::new(),
        };
        let expected = ryeos_engine::dispatch::interpret_external_terminal(
            &output.termination,
            &output.stdout,
            &output.stderr,
        );
        let exact = FinalizeThreadRecord {
            status: expected.status.as_str().into(),
            outcome_code: expected.outcome_code,
            result_json: expected.result,
            error_json: expected.error,
            artifacts: Vec::new(),
            final_cost: None,
            managed_envelope: None,
            result_project_snapshot_hash: None,
            result_workspace_output_capture_hash: None,
        };
        validate_external_direct_result(&exact, &output).unwrap();
        for part in [
            "status", "outcome", "result", "error", "managed", "snapshot", "capture",
        ] {
            let mut changed = exact.clone();
            match part {
                "status" => changed.status = "failed".into(),
                "outcome" => changed.outcome_code = Some("invented".into()),
                "result" => changed.result_json = Some(json!({"answer":99})),
                "error" => changed.error_json = Some(json!({"error":"invented"})),
                "managed" => changed.managed_envelope = Some(json!({})),
                "snapshot" => changed.result_project_snapshot_hash = Some("c".repeat(64)),
                "capture" => changed.result_workspace_output_capture_hash = Some("d".repeat(64)),
                _ => unreachable!(),
            }
            assert!(
                validate_external_direct_result(&changed, &output).is_err(),
                "accepted {part}"
            );
        }
        let mut changed = output.clone();
        changed.termination.reason = ExternalCommandTerminationReason::Cancelled;
        assert!(validate_external_direct_result(&exact, &changed).is_err());
        changed = output.clone();
        changed.termination.target_exit = ExternalTargetExit::Code(7);
        assert!(validate_external_direct_result(&exact, &changed).is_err());
    }

    #[test]
    fn direct_allocation_requires_authoritative_born_capsule_not_runtime_metadata() {
        let store = test_store();
        let reservation = direct_owner_test_reservation(&store);
        let binding=crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        store
            .seed_launch_metadata(
                &reservation.placement_thread_id,
                &crate::launch_metadata::RuntimeLaunchMetadata::default().with_launch_driver(
                    ryeos_state::objects::ExecutionLaunchDriver::DirectItemExecutor,
                ),
            )
            .unwrap();
        let error = store
            .reserve_external_allocation(&reservation, &binding)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("born thread has no admitted capsule")
        );
        assert!(
            store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .is_none()
        );
        let mut absent = reservation.clone();
        absent.placement_thread_id = "T-no-authoritative-birth".into();
        let runtime_db::external_execution::ExternalAllocationOwner::DirectThread {
            chain_root_id,
            launch_owner,
            ..
        } = &mut absent.owner
        else {
            unreachable!()
        };
        *chain_root_id = absent.placement_thread_id.clone();
        launch_owner.thread_id = absent.placement_thread_id.clone();
        store
            .lock()
            .unwrap()
            .runtime_db
            .insert_thread_runtime(&absent.placement_thread_id, &absent.placement_thread_id)
            .unwrap();
        assert!(
            store
                .reserve_external_allocation(&absent, &binding)
                .unwrap_err()
                .to_string()
                .contains("authoritative snapshot missing")
        );
        assert!(
            store
                .external_allocation(&absent.placement_thread_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn direct_owner_handoff_matches_only_its_exact_tuple() {
        // Exercise tuple comparison only. This hand-built token cannot serve
        // as positive admission evidence; real birth and compiler consumption
        // belong to the public ordinary-execution fixture.
        let store = test_store();
        let reservation = direct_owner_test_reservation(&store);
        let runtime_db::external_execution::ExternalAllocationOwner::DirectThread {
            chain_root_id,
            launch_owner,
            program,
        } = &reservation.owner
        else {
            unreachable!()
        };
        let proof = VerifiedExternalDirectOwner {
            thread_id: reservation.placement_thread_id.clone(),
            chain_root_id: chain_root_id.clone(),
            capsule_hash: reservation.admitted_capsule_hash.clone(),
            snapshot_hash: reservation.base_snapshot_hash.clone(),
            endpoint_binding_hash: reservation.binding_hash.clone(),
            program_digest: program.digest().unwrap(),
            launch_owner: launch_owner.clone(),
        };
        assert!(proof.matches(&reservation));
        for part in ["capsule", "snapshot", "binding", "thread", "chain", "owner"] {
            let mut changed = reservation.clone();
            let runtime_db::external_execution::ExternalAllocationOwner::DirectThread {
                chain_root_id,
                launch_owner,
                ..
            } = &mut changed.owner
            else {
                unreachable!()
            };
            match part {
                "capsule" => changed.admitted_capsule_hash = "f".repeat(64),
                "snapshot" => changed.base_snapshot_hash = "f".repeat(64),
                "binding" => changed.binding_hash = "f".repeat(64),
                "thread" => changed.placement_thread_id = "T-other".into(),
                "chain" => *chain_root_id = "T-other".into(),
                "owner" => launch_owner.monotonic_launch_epoch += 1,
                _ => unreachable!(),
            }
            assert!(!proof.matches(&changed));
        }
    }

    #[test]
    fn direct_reservation_replay_preserves_original_owner_without_fresh_admission() {
        use runtime_db::external_execution::{
            ExternalAllocationContactClaim, ExternalAllocationPhase,
        };
        // Storage-only retained occurrence: a positive direct capsule launch
        // remains a joined-test gate, not a claim made by this regression.
        let store = test_store();
        let reservation = direct_owner_test_reservation(&store);
        let binding=crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        {
            let inner = store.lock().unwrap();
            runtime_db::external_execution::tests::retain_direct_owner_fixture(
                &inner.runtime_db,
                &reservation,
                "contact_pending",
            );
        }
        store
            .release_thread_launch_claim(&reservation.placement_thread_id, "direct-proof-claim")
            .unwrap();
        store
            .claim_thread_launch_active(
                &reservation.placement_thread_id,
                "replacement-claim",
                "daemon:replacement",
            )
            .unwrap()
            .unwrap();
        let replay = store
            .reserve_external_allocation(&reservation, &binding)
            .unwrap();
        assert_eq!(replay.reservation, reservation);
        assert_eq!(replay.phase, ExternalAllocationPhase::ContactPending);
        assert!(matches!(
            store
                .claim_external_allocation_contact(
                    &reservation.placement_thread_id,
                    &reservation.request_digest
                )
                .unwrap(),
            ExternalAllocationContactClaim::Reconcile(_)
        ));
        let mut changed = reservation.clone();
        changed.base_snapshot_hash = "f".repeat(64);
        assert!(
            store
                .reserve_external_allocation(&changed, &binding)
                .unwrap_err()
                .to_string()
                .contains("changed its exact reservation")
        );
        assert_eq!(
            store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .reservation,
            reservation
        );

        // One Store is live per process: close the first before the second.
        store.close().unwrap();
        let reserved_store = test_store();
        let reserved = direct_owner_test_reservation(&reserved_store);
        {
            let inner = reserved_store.lock().unwrap();
            runtime_db::external_execution::tests::retain_direct_owner_fixture(
                &inner.runtime_db,
                &reserved,
                "reserved",
            );
        }
        assert!(
            reserved_store
                .reserve_external_allocation(&reserved, &binding)
                .unwrap_err()
                .to_string()
                .contains("born thread has no admitted capsule")
        );
        assert_eq!(
            reserved_store
                .external_allocation(&reserved.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::Reserved
        );
    }

    fn workspace_binding_fixture() -> (TestStateStore, runtime_db::RuntimeWorkspaceBinding) {
        let store = test_store();
        let root = "T-workspace-root";
        store
            .create_thread_for_test(&thread_record(root, root))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(root, "claim-root", "daemon:workspace-test")
            .unwrap()
            .unwrap();
        let binding = runtime_db::RuntimeWorkspaceBinding {
            workspace_id: "workspace-parent".to_owned(),
            view_identity: "exact-created-view-1".to_owned(),
            borrower_launch_owner: claim.owner,
        };
        {
            let g = store.lock().unwrap();
            g.runtime_db
                .reserve_workspace(&binding.workspace_id, &"a".repeat(64), "/workspace-fixture")
                .unwrap();
            g.runtime_db
                .transition_workspace(
                    &binding.workspace_id,
                    &[runtime_db::WorkspaceState::Reserved],
                    runtime_db::WorkspaceState::Constructing,
                    None,
                )
                .unwrap();
            g.runtime_db
                .claim_workspace_construction(&binding.workspace_id, root, &claim.claimed_by)
                .unwrap();
            g.runtime_db
                .bind_workspace(runtime_db::WorkspaceBinding {
                    workspace_id: &binding.workspace_id,
                    thread_id: root,
                    launch_owner: Some(&claim.claimed_by),
                    backend_id: Some("native"),
                    backend_version: Some("fixture-build"),
                    pinned_root_identities: Some("fixture-pinned-roots"),
                    mount_identity: Some(&binding.view_identity),
                    workspace_output_partition_identity: None,
                    base_output_capture_hash: None,
                })
                .unwrap();
        }
        store.bind_thread_workspace(root, &binding).unwrap();
        // A replay while still Ready is exact, not another membership.
        store.bind_thread_workspace(root, &binding).unwrap();
        store
            .transition_execution_workspace(
                &binding.workspace_id,
                &[runtime_db::WorkspaceState::Ready],
                runtime_db::WorkspaceState::Active,
                None,
            )
            .unwrap();
        (store, binding)
    }

    fn workspace_child_fixture(
        store: &StateStore,
        parent: &str,
        child: &str,
        root_binding: &runtime_db::RuntimeWorkspaceBinding,
    ) -> runtime_db::RuntimeWorkspaceBinding {
        // Deliberately separate chain roots: membership is not chain equality.
        store
            .create_thread_for_test(&thread_record(child, child))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(child, &format!("claim-{child}"), "daemon:workspace-test")
            .unwrap()
            .unwrap();
        assert!(
            store
                .record_child_link(parent, child, "dispatch")
                .unwrap()
                .is_none()
        );
        runtime_db::RuntimeWorkspaceBinding {
            borrower_launch_owner: claim.owner,
            ..root_binding.clone()
        }
    }

    fn finish_workspace_test_thread(store: &StateStore, thread_id: &str) {
        store
            .finalize_thread(
                thread_id,
                &FinalizeThreadRecord {
                    status: ThreadStatus::Completed.as_str().to_owned(),
                    outcome_code: Some("completed".to_owned()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .unwrap();
    }

    fn attach_workspace_test_process(
        store: &StateStore,
        binding: &runtime_db::RuntimeWorkspaceBinding,
    ) -> crate::process::ExecutionProcessIdentity {
        let identity = crate::process::ExecutionProcessIdentity {
            schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            process_scope: None,
            boot_id: "test-boot".to_owned(),
            target_pid: 12345,
            target_start_time_ticks: 10,
            group_leader_pid: 12345,
            group_leader_start_time_ticks: 10,
            resource_selections: Vec::new(),
            resource_settlement_authority: None,
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        };
        let thread_id = &binding.borrower_launch_owner.thread_id;
        let owner = store
            .get_launch_claim(thread_id)
            .unwrap()
            .unwrap()
            .claimed_by;
        store
            .attach_new_thread_process(
                thread_id,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                Some(&owner),
            )
            .unwrap();
        identity
    }

    #[test]
    fn owned_completed_terminal_signs_only_its_exact_cleared_process_attempt() {
        let store = test_store();
        let thread_id = "T-signed-settlement";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread_id, "claim-settlement", "daemon:test")
            .unwrap()
            .unwrap();
        let identity = crate::process::ExecutionProcessIdentity {
            schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            process_scope: None,
            boot_id: "test-boot".into(),
            target_pid: 12347,
            target_start_time_ticks: 11,
            group_leader_pid: 12347,
            group_leader_start_time_ticks: 11,
            resource_selections: Vec::new(),
            resource_settlement_authority: None,
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        };
        store
            .attach_new_thread_process(
                thread_id,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                Some(&claim.claimed_by),
            )
            .unwrap();
        assert!(
            store
                .clear_thread_process_if_matches_owned(thread_id, &identity, &claim.claimed_by)
                .unwrap()
        );
        let settlement = store
            .latest_thread_process_settlement(thread_id)
            .unwrap()
            .unwrap();
        assert_eq!(settlement.launch_owner, claim.owner);
        assert_eq!(settlement.process_identity, identity);
        let (events, _) = store
            .finalize_thread_effective_owned(
                thread_id,
                &claim.claimed_by,
                &FinalizeThreadRecord {
                    status: ThreadStatus::Completed.as_str().to_owned(),
                    outcome_code: Some("completed".into()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .unwrap();
        let terminal = events
            .iter()
            .find(|event| event.event_type == ryeos_state::event_types::THREAD_COMPLETED)
            .unwrap();
        assert_eq!(
            terminal.payload["process_settlement_witness_digest"],
            settlement.digest().unwrap()
        );
        assert!(terminal.event_hash.is_some());

        for (thread_id, owned, status) in [
            ("T-unowned-settlement", false, ThreadStatus::Completed),
            ("T-failed-settlement", true, ThreadStatus::Failed),
        ] {
            store
                .create_thread_for_test(&thread_record(thread_id, thread_id))
                .unwrap();
            let claim = store
                .claim_thread_launch_active(thread_id, "claim-settlement", "daemon:test")
                .unwrap()
                .unwrap();
            store
                .attach_new_thread_process(
                    thread_id,
                    identity.target_pid,
                    identity.group_leader_pid,
                    &identity,
                    &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                    Some(&claim.claimed_by),
                )
                .unwrap();
            assert!(
                store
                    .clear_thread_process_if_matches_owned(thread_id, &identity, &claim.claimed_by)
                    .unwrap()
            );
            let update = FinalizeThreadRecord {
                status: status.as_str().to_owned(),
                outcome_code: Some(status.as_str().to_owned()),
                result_json: None,
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            };
            let (events, _) = if owned {
                store
                    .finalize_thread_effective_owned(thread_id, &claim.claimed_by, &update)
                    .unwrap()
            } else {
                store.finalize_thread_effective(thread_id, &update).unwrap()
            };
            let terminal = events
                .iter()
                .find(|event| event.event_type == terminal_event_type(status.as_str()).unwrap())
                .unwrap();
            assert!(
                terminal
                    .payload
                    .get("process_settlement_witness_digest")
                    .is_none(),
                "{thread_id} must not inherit a completion settlement witness"
            );
        }

        let thread_id = "T-replaced-settlement";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .unwrap();
        let first_claim = store
            .claim_thread_launch_active(thread_id, "claim-first", "daemon:test")
            .unwrap()
            .unwrap();
        store
            .attach_new_thread_process(
                thread_id,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                Some(&first_claim.claimed_by),
            )
            .unwrap();
        assert!(
            store
                .clear_thread_process_if_matches_owned(
                    thread_id,
                    &identity,
                    &first_claim.claimed_by,
                )
                .unwrap()
        );
        assert!(
            store
                .release_active_thread_launch_claim(
                    thread_id,
                    "claim-first",
                    &first_claim.claimed_by,
                )
                .unwrap()
        );
        let replacement = store
            .claim_thread_launch_active(thread_id, "claim-second", "daemon:test")
            .unwrap()
            .unwrap();
        assert_ne!(first_claim.owner, replacement.owner);
        let (events, _) = store
            .finalize_thread_effective_owned(
                thread_id,
                &replacement.claimed_by,
                &FinalizeThreadRecord {
                    status: ThreadStatus::Completed.as_str().to_owned(),
                    outcome_code: Some("completed".into()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .unwrap();
        let terminal = events
            .iter()
            .find(|event| event.event_type == ryeos_state::event_types::THREAD_COMPLETED)
            .unwrap();
        assert!(
            terminal
                .payload
                .get("process_settlement_witness_digest")
                .is_none()
        );
    }

    #[test]
    fn workspace_binding_quarantine_requires_retained_authority_not_missing_pid() {
        let (store, binding) = workspace_binding_fixture();
        let root = "T-workspace-root";
        assert!(!store.has_retained_workspace_quarantine("T-absent").unwrap());
        assert!(!store.has_retained_workspace_quarantine(root).unwrap());
        let claim = store.get_launch_claim(root).unwrap().unwrap();
        store
            .release_active_thread_launch_claim(root, &claim.claim_id, &claim.claimed_by)
            .unwrap();
        assert!(store.has_retained_workspace_quarantine(root).unwrap());
        assert_eq!(store.thread_workspace_binding(root).unwrap(), Some(binding));

        // One Store is live per process: close the first before the second.
        store.close().unwrap();
        let (settled, binding) = workspace_binding_fixture();
        let identity = attach_workspace_test_process(&settled, &binding);
        assert!(
            settled
                .settle_reaped_thread_workspace_owned(root, &binding, &identity)
                .unwrap()
        );
        let claim = settled.get_launch_claim(root).unwrap().unwrap();
        settled
            .release_active_thread_launch_claim(root, &claim.claim_id, &claim.claimed_by)
            .unwrap();
        // Missing process identity plus an ordinary journal is insufficient.
        assert!(!settled.has_retained_workspace_quarantine(root).unwrap());
        settled
            .transition_execution_workspace(
                &binding.workspace_id,
                &[runtime_db::WorkspaceState::Active],
                runtime_db::WorkspaceState::Freezing,
                None,
            )
            .unwrap();
        assert!(settled.has_retained_workspace_quarantine(root).unwrap());
    }

    #[test]
    fn workspace_binding_refuses_process_only_clear_until_atomic_settlement() {
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-process-only-clear";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        let identity = attach_workspace_test_process(&store, &binding);
        let claim = store.get_launch_claim(child).unwrap().unwrap();
        assert!(
            !store
                .clear_thread_process_if_matches(child, &identity)
                .unwrap()
        );
        assert!(
            !store
                .clear_thread_process_if_matches_owned(child, &identity, &claim.claimed_by)
                .unwrap()
        );
        assert_eq!(
            store
                .get_thread(child)
                .unwrap()
                .unwrap()
                .runtime
                .process_identity,
            Some(identity.clone())
        );
        assert_eq!(
            store.thread_workspace_binding(child).unwrap(),
            Some(binding.clone())
        );
        assert!(
            store
                .list_attached_thread_ids()
                .unwrap()
                .iter()
                .any(|id| id == child)
        );
        // Shutdown's abandoned-owner path retains the same coordinates after
        // the waiting task drops its claim; only joint settlement releases them.
        store
            .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
            .unwrap();
        assert!(
            store
                .settle_dead_thread_workspace_if_matches(child, &binding, &identity)
                .unwrap()
        );
        assert!(store.thread_workspace_binding(child).unwrap().is_none());
        assert!(
            store
                .get_thread(child)
                .unwrap()
                .unwrap()
                .runtime
                .process_identity
                .is_none()
        );
    }

    #[test]
    fn workspace_binding_reaped_attempt_clears_exact_attachment_atomically_before_retry() {
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-reaped-attempt";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        let identity = attach_workspace_test_process(&store, &binding);
        let mut wrong = identity.clone();
        wrong.target_start_time_ticks += 1;
        assert!(
            !store
                .settle_reaped_thread_workspace_owned(child, &binding, &wrong)
                .unwrap()
        );
        assert_eq!(
            store
                .get_thread(child)
                .unwrap()
                .unwrap()
                .runtime
                .process_identity,
            Some(identity.clone())
        );
        assert_eq!(
            store.thread_workspace_binding(child).unwrap(),
            Some(binding.clone())
        );
        // The test supplies the owner's completed-wait proof at this boundary;
        // a nonterminal attempt may settle without changing thread lifecycle.
        assert!(!is_terminal_status(
            &store.get_thread(child).unwrap().unwrap().status
        ));
        assert!(
            store
                .settle_reaped_thread_workspace_owned(child, &binding, &identity)
                .unwrap()
        );
        assert!(
            store
                .get_thread(child)
                .unwrap()
                .unwrap()
                .runtime
                .process_identity
                .is_none()
        );
        assert!(store.thread_workspace_binding(child).unwrap().is_none());
        assert!(
            !store
                .list_attached_thread_ids()
                .unwrap()
                .iter()
                .any(|id| id == child)
        );
        assert!(
            !store
                .settle_reaped_thread_workspace_owned(child, &binding, &identity)
                .unwrap()
        );
        let claim = store.get_launch_claim(child).unwrap().unwrap();
        assert!(matches!(
            store
                .rotate_active_thread_launch_claim_after_runtime_recovery(
                    child,
                    &claim.claimed_by,
                    "next-attempt",
                    "daemon:workspace-test",
                    2,
                )
                .unwrap(),
            runtime_db::RuntimeRecoveryClaimRotation::Rotated { .. }
        ));
    }

    #[test]
    fn workspace_binding_dead_settlement_requires_abandoned_exact_attachment() {
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-dead-attempt";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        let identity = attach_workspace_test_process(&store, &binding);
        assert!(
            !store
                .settle_dead_thread_workspace_if_matches(child, &binding, &identity)
                .unwrap()
        );
        let claim = store.get_launch_claim(child).unwrap().unwrap();
        store
            .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
            .unwrap();
        let mut wrong = identity.clone();
        wrong.target_start_time_ticks += 1;
        assert!(
            !store
                .settle_dead_thread_workspace_if_matches(child, &binding, &wrong)
                .unwrap()
        );
        let mut wrong_binding = binding.clone();
        wrong_binding.view_identity = "replacement-view".to_owned();
        assert!(
            !store
                .settle_dead_thread_workspace_if_matches(child, &wrong_binding, &identity)
                .unwrap()
        );
        assert_eq!(
            store.thread_workspace_binding(child).unwrap(),
            Some(binding.clone())
        );
        assert!(
            store
                .settle_dead_thread_workspace_if_matches(child, &binding, &identity)
                .unwrap()
        );
        assert!(
            store
                .get_thread(child)
                .unwrap()
                .unwrap()
                .runtime
                .process_identity
                .is_none()
        );
        assert!(store.thread_workspace_binding(child).unwrap().is_none());
    }

    #[test]
    fn workspace_binding_reaped_settlement_refuses_replacement_claim_and_unattached_contact() {
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-replaced-attempt";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        let identity = attach_workspace_test_process(&store, &binding);
        let claim = store.get_launch_claim(child).unwrap().unwrap();
        store
            .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
            .unwrap();
        store
            .claim_thread_launch(child, "replacement", "daemon:replacement")
            .unwrap();
        assert!(
            !store
                .settle_reaped_thread_workspace_owned(child, &binding, &identity)
                .unwrap()
        );
        assert!(
            !store
                .settle_dead_thread_workspace_if_matches(child, &binding, &identity)
                .unwrap()
        );
        assert_eq!(
            store.thread_workspace_binding(child).unwrap(),
            Some(binding.clone())
        );
        assert_eq!(
            store
                .get_thread(child)
                .unwrap()
                .unwrap()
                .runtime
                .process_identity,
            Some(identity.clone())
        );

        let unknown = "T-unattached-contact";
        let unknown_binding =
            workspace_child_fixture(&store, "T-workspace-root", unknown, &root_binding);
        store
            .bind_thread_workspace(unknown, &unknown_binding)
            .unwrap();
        assert!(
            !store
                .settle_reaped_thread_workspace_owned(unknown, &unknown_binding, &identity)
                .unwrap()
        );
        let unknown_claim = store.get_launch_claim(unknown).unwrap().unwrap();
        store
            .release_active_thread_launch_claim(
                unknown,
                &unknown_claim.claim_id,
                &unknown_claim.claimed_by,
            )
            .unwrap();
        assert!(
            !store
                .settle_dead_thread_workspace_if_matches(unknown, &unknown_binding, &identity)
                .unwrap()
        );
        assert_eq!(
            store.thread_workspace_binding(unknown).unwrap(),
            Some(unknown_binding)
        );
    }

    #[test]
    fn workspace_binding_reaped_parent_cannot_hide_unsettled_descendant_contact() {
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-reaped-parent";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        let identity = attach_workspace_test_process(&store, &binding);
        let nested = workspace_child_fixture(&store, child, "T-live-descendant", &root_binding);
        store
            .bind_thread_workspace("T-live-descendant", &nested)
            .unwrap();
        assert!(
            !store
                .settle_reaped_thread_workspace_owned(child, &binding, &identity)
                .unwrap()
        );
        assert_eq!(
            store
                .get_thread(child)
                .unwrap()
                .unwrap()
                .runtime
                .process_identity,
            Some(identity.clone())
        );
        finish_workspace_test_thread(&store, "T-live-descendant");
        assert!(
            store
                .settle_thread_workspace_owned("T-live-descendant", &nested)
                .unwrap()
        );
        assert!(
            store
                .settle_reaped_thread_workspace_owned(child, &binding, &identity)
                .unwrap()
        );
    }

    fn reserve_workspace_test_barrier(
        store: &StateStore,
        access: ryeos_engine::kind_registry::WorkspaceAccess,
        child: &str,
    ) -> String {
        let invocation = ryeos_runtime::workload_client::WorkloadInvocationSource::Cli {
            external_request_id: "fixture-request".to_owned(),
        };
        let operation_id = invocation.runtime_operation_id(&"c".repeat(64)).unwrap();
        let g = store.lock().unwrap();
        g.runtime_db
            .reserve_runtime_action_intent_with_workspace(
                &operation_id,
                "T-workspace-root",
                "T-workspace-root",
                runtime_db::RuntimeActionMode::Inline,
                &"e".repeat(64),
                child,
                None,
                Some(&runtime_db::NewRuntimeWorkspaceOperation {
                    workspace_id: "workspace-parent",
                    access,
                    worker_instance_id: "test-worker",
                    worker_boot_epoch: 1,
                    worker_boot_identity_hash: &"a".repeat(64),
                    project_authority_digest: &"b".repeat(64),
                    workload_client_grant_digest: &"c".repeat(64),
                    invocation,
                }),
            )
            .unwrap();
        operation_id
    }

    #[test]
    fn workspace_binding_replays_exactly_and_refuses_conflicts_and_stale_owners() {
        let (store, root_binding) = workspace_binding_fixture();
        // Contact can use the existing Active root view; it does not repeat
        // the Ready-only root birth mutation.
        store
            .authorize_thread_workspace_contact("T-workspace-root", &root_binding)
            .unwrap();
        let mut wrong_root = root_binding.clone();
        wrong_root.view_identity = "other-created-view".to_owned();
        assert!(
            store
                .authorize_thread_workspace_contact("T-workspace-root", &wrong_root)
                .is_err()
        );
        let child = "T-binding-child";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        store.bind_thread_workspace(child, &binding).unwrap();
        let mut conflicting = binding.clone();
        conflicting.view_identity = "other-created-view".to_owned();
        assert!(store.bind_thread_workspace(child, &conflicting).is_err());
        let claim = store.get_launch_claim(child).unwrap().unwrap();
        store
            .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
            .unwrap();
        store
            .claim_thread_launch_active(child, "replacement-claim", "daemon:replacement")
            .unwrap()
            .unwrap();
        assert!(store.bind_thread_workspace(child, &binding).is_err());
        assert!(
            store
                .authorize_thread_workspace_contact(child, &binding)
                .is_err()
        );
        assert_eq!(
            store.thread_workspace_binding(child).unwrap(),
            Some(binding)
        );
    }

    #[test]
    fn workspace_binding_freeze_fences_admission_attachment_and_late_release() {
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-freeze-child";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        let identity = crate::process::ExecutionProcessIdentity {
            schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            process_scope: None,
            boot_id: "test-boot".to_owned(),
            target_pid: 12345,
            target_start_time_ticks: 10,
            group_leader_pid: 12345,
            group_leader_start_time_ticks: 10,
            resource_selections: Vec::new(),
            resource_settlement_authority: None,
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        };
        let owner = store.get_launch_claim(child).unwrap().unwrap().claimed_by;
        store
            .attach_new_thread_process(
                child,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                Some(&owner),
            )
            .unwrap();
        store
            .authorize_attached_process_release(child, &identity, Some(&owner))
            .unwrap();
        store
            .transition_execution_workspace(
                &binding.workspace_id,
                &[runtime_db::WorkspaceState::Active],
                runtime_db::WorkspaceState::Freezing,
                None,
            )
            .unwrap();
        assert!(store.bind_thread_workspace(child, &binding).is_err());
        assert!(
            store
                .authorize_thread_workspace_contact(child, &binding)
                .is_err()
        );
        assert!(
            store
                .authorize_attached_process_release(child, &identity, Some(&owner))
                .is_err()
        );
        assert!(
            store
                .attach_thread_process(
                    child,
                    identity.target_pid,
                    identity.group_leader_pid,
                    &identity,
                    &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                    Some(&owner)
                )
                .is_err()
        );
        assert_eq!(
            store.thread_workspace_binding(child).unwrap(),
            Some(binding)
        );
    }

    #[test]
    fn dedicated_workspace_contact_requires_exact_active_root_binding() {
        let (store, binding) = workspace_binding_fixture();
        {
            let g = store.lock().unwrap();
            store
                .authorize_dedicated_workspace_contact_locked(
                    &g,
                    "T-workspace-root",
                    &binding.workspace_id,
                )
                .unwrap();
            assert!(
                store
                    .authorize_dedicated_workspace_contact_locked(
                        &g,
                        "T-workspace-root",
                        "obsolete-workspace"
                    )
                    .is_err()
            );
        }
        let child = "T-not-dedicated-root";
        let child_binding = workspace_child_fixture(&store, "T-workspace-root", child, &binding);
        store.bind_thread_workspace(child, &child_binding).unwrap();
        assert!(
            store
                .authorize_dedicated_workspace_contact_locked(
                    &store.lock().unwrap(),
                    child,
                    &binding.workspace_id
                )
                .is_err()
        );
        let claim = store.get_launch_claim("T-workspace-root").unwrap().unwrap();
        store
            .release_active_thread_launch_claim(
                "T-workspace-root",
                &claim.claim_id,
                &claim.claimed_by,
            )
            .unwrap();
        assert!(
            store
                .authorize_dedicated_workspace_contact_locked(
                    &store.lock().unwrap(),
                    "T-workspace-root",
                    &binding.workspace_id
                )
                .is_err()
        );
    }

    #[test]
    fn workspace_capture_refuses_pending_and_identity_unknown_worker_start() {
        for (cleanup_proved, recovered_boot, contacted_command) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (true, true, true),
        ] {
            let (store, binding) = workspace_binding_fixture();
            let root = "T-workspace-root";
            let workspace = store
                .execution_workspace(&binding.workspace_id)
                .unwrap()
                .unwrap();
            assert!(
                store
                    .workspace_worker_capture_record(&workspace)
                    .unwrap()
                    .is_none()
            );
            store
                .create_credential_profile(NewCredentialProfile {
                    profile_id: "P-capture",
                    owner_principal: "fp:operator",
                    home_id: "home-capture",
                })
                .unwrap();
            store
                .acquire_credential_profile("P-capture", "fp:operator", "worker-capture")
                .unwrap();
            store
                .admit_dedicated_session(NewDedicatedSession {
                    placement_thread_id: root,
                    chain_root_id: root,
                    owner_principal: "fp:operator",
                    admitted_capsule_hash: &"a".repeat(64),
                    workspace_id: &binding.workspace_id,
                    candidate_required: false,
                    candidate_disposition: runtime_db::DedicatedCandidateDisposition::OwnerDecision,
                    credential_profile_id: "P-capture",
                    credential_generation: 1,
                    credential_lock_owner: "worker-capture",
                })
                .unwrap();
            assert!(store.workspace_worker_capture_record(&workspace).is_err());
            let (worker_id, boot_epoch) = if recovered_boot {
                // An exact old retirement permits recovery before, but never
                // after, a new admitted attempt crosses the durable boundary.
                let identity = attach_workspace_test_process(&store, &binding);
                store
                    .transition_execution_workspace_owned(
                        &binding.workspace_id,
                        root,
                        &store.get_launch_claim(root).unwrap().unwrap().claimed_by,
                        &[runtime_db::WorkspaceState::Active],
                        runtime_db::WorkspaceState::Active,
                        Some(&serde_json::to_string(&identity).unwrap()),
                    )
                    .unwrap();
                store
                    .attach_worker_process(&WorkerProcessRecord {
                        worker_instance_id: "worker-capture".to_owned(),
                        boot_identity_hash: "b".repeat(64),
                        session_capsule_hash: "a".repeat(64),
                        boot_epoch: 1,
                        lifecycle_generation: 1,
                        process_identity: identity,
                        control_channel_identity: "fixture-channel".to_owned(),
                        state: runtime_db::WorkerProcessState::Attached,
                        daemon_generation_id: "daemon:workspace-test".to_owned(),
                        placement_thread_id: root.to_owned(),
                        cleanup_state: "owned".to_owned(),
                        created_at_ms: 1,
                        updated_at_ms: 1,
                    })
                    .unwrap();
                let contacted = if contacted_command {
                    store
                        .complete_worker_binding("worker-capture", root, 1)
                        .unwrap();
                    let command = store
                        .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                            placement_thread_id: root,
                            idempotency_key: "capture-contacted-command",
                            worker_boot_epoch: 1,
                            command_kind: "route",
                            request_digest: &"c".repeat(64),
                            payload: &serde_json::json!({"route": "fixture.read"}),
                        })
                        .unwrap();
                    store
                        .mark_dedicated_command_contacted(root, command.command_sequence, 1)
                        .unwrap();
                    Some(command.command_sequence)
                } else {
                    None
                };
                store
                    .fence_abandoned_worker_process("worker-capture", root, 1, "reaped")
                    .unwrap();
                let retired = store
                    .execution_workspace(&binding.workspace_id)
                    .unwrap()
                    .unwrap();
                assert!(
                    store
                        .workspace_worker_capture_record(&retired)
                        .unwrap()
                        .is_none()
                );
                if let Some(sequence) = contacted {
                    let session = store.dedicated_session(root).unwrap().unwrap();
                    assert_eq!(session.state, "outcome_unknown");
                    assert_eq!(session.send_boundary, "outcome_unknown");
                    assert!(session.worker_instance_id.is_none());
                    assert!(session.worker_boot_epoch.is_none());
                    let command = store
                        .dedicated_session_command(root, sequence)
                        .unwrap()
                        .unwrap();
                    assert_eq!(command.state, "outcome_unknown");
                    assert_eq!(command.worker_boot_epoch, 1);
                    assert!(command.result.is_none());
                    assert!(
                        store
                            .mark_dedicated_command_contacted(root, sequence, 1)
                            .is_err()
                    );
                }
                store
                    .acquire_credential_profile("P-capture", "fp:operator", "worker-next")
                    .unwrap();
                if contacted.is_some() {
                    // Proven process retirement permits capture, not automatic
                    // recovery of an unknown command. The uncontacted cases
                    // below separately prove that a valid newer pending boot
                    // cannot hide behind this historical cleanup testimony.
                    assert!(
                        store
                            .prepare_dedicated_session_recovery(
                                root,
                                1,
                                "worker-next",
                                &binding.workspace_id
                            )
                            .is_err()
                    );
                    let session = store.dedicated_session(root).unwrap().unwrap();
                    assert_eq!(session.state, "outcome_unknown");
                    assert_eq!(session.send_boundary, "outcome_unknown");
                    assert!(session.worker_instance_id.is_none());
                    assert!(session.worker_boot_epoch.is_none());
                    assert!(
                        store
                            .workspace_worker_capture_record(&retired)
                            .unwrap()
                            .is_none()
                    );
                    continue;
                }
                assert_eq!(
                    store
                        .prepare_dedicated_session_recovery(
                            root,
                            1,
                            "worker-next",
                            &binding.workspace_id
                        )
                        .unwrap(),
                    2
                );
                assert!(store.workspace_worker_capture_record(&retired).is_err());
                assert!(
                    store
                        .fail_dedicated_session_start(
                            root,
                            "worker-capture",
                            1,
                            "stale cleanup",
                            true
                        )
                        .is_err()
                );
                assert!(
                    store
                        .fail_dedicated_session_start(root, "worker-next", 1, "wrong epoch", true)
                        .is_err()
                );
                assert_eq!(
                    store
                        .reconcile_unattached_credential_profile_locks()
                        .unwrap(),
                    (1, 0)
                );
                assert!(store.workspace_worker_capture_record(&retired).is_err());
                assert_eq!(
                    store
                        .credential_profile("P-capture")
                        .unwrap()
                        .unwrap()
                        .lock_owner
                        .as_deref(),
                    Some("worker-next")
                );
                ("worker-next", 2)
            } else {
                ("worker-capture", 1)
            };
            store
                .fail_dedicated_session_start(
                    root,
                    worker_id,
                    boot_epoch,
                    "fixture start failure",
                    cleanup_proved,
                )
                .unwrap();
            let current = store
                .execution_workspace(&binding.workspace_id)
                .unwrap()
                .unwrap();
            let observed = store.workspace_worker_capture_record(&current);
            if cleanup_proved {
                assert!(observed.unwrap().is_none());
            } else {
                assert!(
                    observed.is_err(),
                    "missing identity must not hide outcome-unknown contact"
                );
            }
        }
    }

    #[test]
    fn workspace_binding_unknown_contact_survives_terminalization_and_claim_removal() {
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-unknown-child";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        assert!(
            store
                .get_thread(child)
                .unwrap()
                .unwrap()
                .runtime
                .process_identity
                .is_none()
        );
        assert!(
            !store
                .settle_thread_workspace_owned(child, &binding)
                .unwrap()
        );
        finish_workspace_test_thread(&store, child);
        assert_eq!(
            store.thread_workspace_binding(child).unwrap(),
            Some(binding.clone())
        );
        let claim = store.get_launch_claim(child).unwrap().unwrap();
        store
            .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
            .unwrap();
        assert!(
            store
                .settle_thread_workspace_owned(child, &binding)
                .is_err()
        );
        let members = store
            .workspace_members_after(&binding.workspace_id, &binding.view_identity, None, 10)
            .unwrap();
        assert!(
            members
                .iter()
                .any(|member| member.thread_id == child && member.binding == binding)
        );
    }

    #[test]
    fn workspace_binding_live_owner_must_explicitly_settle_after_lifecycle_completion() {
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-settled-child";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        finish_workspace_test_thread(&store, child);
        assert!(store.thread_workspace_binding(child).unwrap().is_some());
        assert!(
            store
                .settle_thread_workspace_owned(child, &binding)
                .unwrap()
        );
        assert!(
            store
                .settle_thread_workspace_owned(child, &binding)
                .unwrap()
        );
        assert!(store.thread_workspace_binding(child).unwrap().is_none());
    }

    #[test]
    fn workspace_binding_immutable_input_is_not_membership_or_mutable_descendant_authority() {
        let (store, root_binding) = workspace_binding_fixture();
        let input = "T-independent-input";
        workspace_child_fixture(&store, "T-workspace-root", input, &root_binding);
        // The launch owner selects a separate immutable input and never binds
        // the subject's mutable view. Its lineage alone cannot add membership.
        assert!(store.thread_workspace_binding(input).unwrap().is_none());
        StateStore::authorize_thread_workspace_contact_locked(&store.lock().unwrap(), input)
            .unwrap();
        let nested = workspace_child_fixture(&store, input, "T-input-nested", &root_binding);
        assert!(
            store
                .bind_thread_workspace("T-input-nested", &nested)
                .is_err()
        );
        assert_eq!(
            store
                .workspace_members_after(
                    &root_binding.workspace_id,
                    &root_binding.view_identity,
                    None,
                    10
                )
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn workspace_binding_immutable_capture_blocks_even_previously_bound_writers() {
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-capture-child";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        reserve_workspace_test_barrier(
            &store,
            ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
            "T-operation-child",
        );
        assert!(store.bind_thread_workspace(child, &binding).is_err());
        assert!(
            StateStore::authorize_thread_workspace_contact_locked(&store.lock().unwrap(), child)
                .is_err()
        );
    }

    #[test]
    fn workspace_binding_exclusive_operation_admits_only_its_same_view_subtree() {
        use runtime_db::RuntimeWorkspaceOperationPhase as Phase;
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-exclusive-child";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        let op = reserve_workspace_test_barrier(
            &store,
            ryeos_engine::kind_registry::WorkspaceAccess::SharedExclusive,
            child,
        );
        assert!(store.bind_thread_workspace(child, &binding).is_err());
        store
            .transition_runtime_workspace_operation(&op, &[Phase::Reserved], Phase::Quiescing)
            .unwrap();
        store
            .transition_runtime_workspace_operation(&op, &[Phase::Quiescing], Phase::Quiesced)
            .unwrap();
        store.bind_thread_workspace(child, &binding).unwrap();
        let nested = workspace_child_fixture(&store, child, "T-exclusive-nested", &root_binding);
        store
            .bind_thread_workspace("T-exclusive-nested", &nested)
            .unwrap();
        let sibling = workspace_child_fixture(
            &store,
            "T-workspace-root",
            "T-exclusive-sibling",
            &root_binding,
        );
        assert!(
            store
                .bind_thread_workspace("T-exclusive-sibling", &sibling)
                .is_err()
        );
        store
            .transition_runtime_workspace_operation(&op, &[Phase::Quiesced], Phase::ChildRunning)
            .unwrap();
        finish_workspace_test_thread(&store, child);
        assert!(
            !store
                .settle_thread_workspace_owned(child, &binding)
                .unwrap()
        );
        let intent = store.get_runtime_action_intent(&op).unwrap().unwrap();
        assert!(
            !StateStore::runtime_workspace_subtree_is_settled_locked(
                &store.lock().unwrap(),
                &intent
            )
            .unwrap()
        );
        finish_workspace_test_thread(&store, "T-exclusive-nested");
        assert!(
            store
                .settle_thread_workspace_owned("T-exclusive-nested", &nested)
                .unwrap()
        );
        assert!(
            store
                .settle_thread_workspace_owned(child, &binding)
                .unwrap()
        );
        assert!(
            StateStore::runtime_workspace_subtree_is_settled_locked(
                &store.lock().unwrap(),
                &intent
            )
            .unwrap()
        );
        store
            .transition_runtime_workspace_operation(&op, &[Phase::ChildRunning], Phase::Settling)
            .unwrap();
        assert!(
            StateStore::authorize_thread_workspace_contact_locked(
                &store.lock().unwrap(),
                "T-workspace-root"
            )
            .is_err()
        );
    }

    #[test]
    fn workspace_binding_member_pages_are_bounded_and_exact() {
        let (store, root_binding) = workspace_binding_fixture();
        let child = "T-page-child";
        let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
        store.bind_thread_workspace(child, &binding).unwrap();
        let first = store
            .workspace_members_after(&binding.workspace_id, &binding.view_identity, None, 1)
            .unwrap();
        let next = store
            .workspace_members_after(
                &binding.workspace_id,
                &binding.view_identity,
                Some(&first[0].thread_id),
                1,
            )
            .unwrap();
        assert_ne!(first[0].thread_id, next[0].thread_id);
        assert!(
            store
                .workspace_members_after(&binding.workspace_id, "another-view", None, 1)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .workspace_members_after(&binding.workspace_id, &binding.view_identity, None, 0)
                .is_err()
        );
        assert!(
            store
                .workspace_members_after(
                    &binding.workspace_id,
                    &binding.view_identity,
                    None,
                    runtime_db::WORKSPACE_MEMBER_PAGE_SIZE + 1
                )
                .is_err()
        );
    }

    #[test]
    fn handoff_reservation_fences_the_exact_project_head_until_adoption() {
        let tmp = tempdir().expect("tempdir");
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&tmp.path().join("node-key.pem"))
                .expect("test node identity");
        let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let store = TestStateStore::open(tmp.path(), signer.clone(), Arc::new(head_trust))
            .expect("state store");
        let authority = store.pinned_state_authority().expect("state authority");
        let guard = authority.acquire_shared_guard().expect("CAS guard");
        let principal_key = "1".repeat(64);
        let project_hash = "2".repeat(64);
        let initial_head = "3".repeat(64);
        let next_head = "4".repeat(64);
        store
            .write_project_head_ref(
                &principal_key,
                &project_hash,
                &initial_head,
                signer.as_ref(),
                &guard,
            )
            .expect("initial project HEAD");

        store
            .create_credential_profile(NewCredentialProfile {
                profile_id: "P-project-fence",
                owner_principal: "fp:operator",
                home_id: "home-project-fence",
            })
            .expect("credential profile");
        store
            .acquire_credential_profile(
                "P-project-fence",
                "fp:operator",
                "credential-enrollment:project-fence",
            )
            .expect("credential lock");
        let login_epoch = store
            .begin_credential_enrollment(
                "P-project-fence",
                "credential-enrollment:project-fence",
                "credential-login:project-fence",
                lillux::time::timestamp_millis() + 60_000,
            )
            .expect("begin credential enrollment");
        let credential_generation = store
            .complete_credential_enrollment(
                "P-project-fence",
                "credential-enrollment:project-fence",
                "credential-login:project-fence",
                login_epoch,
                &json!({"account":"project-fence"}),
            )
            .expect("complete credential enrollment");
        store
            .release_credential_profile("P-project-fence", "credential-enrollment:project-fence")
            .expect("release enrollment lock");
        store
            .reserve_credential_profile_generation(NewCredentialProfileReservation {
                reservation_id: "reservation-project-fence",
                operation_id: "operation-project-fence",
                successor_thread_id: "T-project-fence-target",
                profile_id: "P-project-fence",
                owner_principal: "fp:operator",
                credential_generation,
                subject_contract_digest: &"5".repeat(64),
                subject_digest: &"6".repeat(64),
                checkpoint_manifest_hash: &"7".repeat(64),
                upstream_session_id: "upstream-project-fence",
                project_principal_key: &principal_key,
                project_hash: &project_hash,
                target_project_head_hash: &initial_head,
            })
            .expect("handoff reservation");

        let error = store
            .advance_project_head_ref(
                &principal_key,
                &project_hash,
                &next_head,
                &initial_head,
                signer.as_ref(),
                &guard,
            )
            .expect_err("active handoff fence must reject project mutation");
        assert!(format!("{error:#}").contains("fenced by worker handoff reservation"));
        assert_eq!(
            store
                .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))
                .unwrap()
                .as_deref(),
            Some(initial_head.as_str())
        );

        let compact_called = std::sync::atomic::AtomicBool::new(false);
        let error = store
            .with_project_head_compaction_authority(|| {
                compact_called.store(true, Ordering::SeqCst);
                Ok(())
            })
            .expect_err("online project compaction must share the handoff fence");
        assert!(format!("{error:#}").contains("active worker handoff"));
        assert!(!compact_called.load(Ordering::SeqCst));

        store
            .release_project_head_fence("reservation-project-fence")
            .expect("authoritative adoption releases project fence");
        store
            .with_project_head_compaction_authority(|| {
                compact_called.store(true, Ordering::SeqCst);
                Ok(())
            })
            .expect("released fence permits project compaction");
        assert!(compact_called.load(Ordering::SeqCst));
        store
            .advance_project_head_ref(
                &principal_key,
                &project_hash,
                &next_head,
                &initial_head,
                signer.as_ref(),
                &guard,
            )
            .expect("released fence permits exact project CAS");
    }

    fn handoff_test_operation(
        role: crate::worker_handoff::WorkerHandoffJobRole,
        operation_label: &str,
        placement_thread_id: &str,
    ) -> crate::worker_handoff::WorkerSessionHandoffJobOperation {
        let hash = |label: &str| lillux::sha256_hex(label.as_bytes());
        crate::worker_handoff::WorkerSessionHandoffJobOperation::new(
            role,
            hash(operation_label),
            hash(&format!("{operation_label}:preflight")),
            hash(&format!("{operation_label}:preflight-attestation")),
            "fp:handoff-owner".to_owned(),
            format!("T-chain-{operation_label}"),
            "site:origin".to_owned(),
            "site:source".to_owned(),
            "site:target".to_owned(),
            placement_thread_id.to_owned(),
            format!("T-successor-{operation_label}"),
            hash(&format!("{operation_label}:source-head")),
            hash(&format!("{operation_label}:source-event")),
            hash(&format!("{operation_label}:checkpoint")),
            hash(&format!("{operation_label}:transfer")),
            "source".to_owned(),
            "/source/project".to_owned(),
            "/target/project".to_owned(),
            hash(&format!("{operation_label}:route")),
            "credential:target".to_owned(),
            None,
        )
        .unwrap()
    }

    fn target_handoff_progress(
        operation_id: &str,
        phase: crate::worker_handoff::WorkerHandoffPhase,
    ) -> crate::worker_handoff::WorkerSessionHandoffProgress {
        let mut progress =
            crate::worker_handoff::WorkerSessionHandoffProgress::planned(operation_id.to_owned())
                .unwrap();
        progress.phase = phase;
        if !matches!(
            phase,
            crate::worker_handoff::WorkerHandoffPhase::Planned
                | crate::worker_handoff::WorkerHandoffPhase::SourceExported
                | crate::worker_handoff::WorkerHandoffPhase::AbortAuthorized
        ) {
            progress.placement_attestation_hash = Some("a".repeat(64));
            progress.target_runtime_seed_hash = Some("b".repeat(64));
            progress.credential_reservation_id = Some("reservation:target".to_owned());
        }
        if phase.successor_is_only_authorized_writer() {
            progress.writer_grant_hash = Some("c".repeat(64));
            progress.target_chain_head_hash = Some("d".repeat(64));
        }
        progress.validate().unwrap();
        progress
    }

    fn target_adoption_coordinates(
        operation: &crate::worker_handoff::WorkerSessionHandoffJobOperation,
    ) -> (
        crate::worker_handoff::WorkerPlacementAdoptRequest,
        crate::worker_handoff::WorkerPlacementAdoptResponse,
    ) {
        let request = crate::worker_handoff::WorkerPlacementAdoptRequest {
            operation_id: operation.operation_id.clone(),
            chain_root_id: operation.chain_root_id.clone(),
            target_chain_head_hash: "d".repeat(64),
            placement_attestation_hash: "a".repeat(64),
            writer_grant_hash: "c".repeat(64),
        };
        let response = crate::worker_handoff::WorkerPlacementAdoptResponse {
            operation_id: operation.operation_id.clone(),
            chain_root_id: operation.chain_root_id.clone(),
            placement_thread_id: operation.successor_placement_thread_id.clone(),
            target_chain_head_hash: request.target_chain_head_hash.clone(),
            delivery: "attached".to_owned(),
        };
        (request, response)
    }

    fn seed_projected_target_adoption_job(
        store: &StateStore,
        operation: &crate::worker_handoff::WorkerSessionHandoffJobOperation,
    ) -> String {
        let progress = target_handoff_progress(
            &operation.operation_id,
            crate::worker_handoff::WorkerHandoffPhase::ProcessAttached,
        );
        let job_id = format!("worker-handoff-target:{}", operation.operation_id);
        store
            .with_state_db(|db| {
                db.create_sync_job_with_initial_progress(
                    &ryeos_state::NewSyncJob {
                        job_id: job_id.clone(),
                        operation_type: crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
                            .to_owned(),
                        operation: operation.to_value()?,
                        peer: Some("source".to_owned()),
                        roots: vec![
                            operation.transfer_manifest_hash.clone(),
                            "a".repeat(64),
                            "b".repeat(64),
                            "c".repeat(64),
                            "d".repeat(64),
                        ],
                        // The target sync head remains the source head until
                        // the permanent adoption receipt is folded terminally.
                        heads: vec![operation.source_chain_head_hash.clone()],
                        max_attempts: crate::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
                    },
                    ryeos_state::SyncJobState::Running,
                    progress.phase.as_str(),
                    Some(&progress.to_value()?),
                )
            })
            .unwrap();
        job_id
    }

    #[test]
    fn target_terminal_outcomes_share_one_irreversible_branch_head() {
        let adoption_store = test_store();
        let adoption_operation = handoff_test_operation(
            crate::worker_handoff::WorkerHandoffJobRole::Target,
            "adoption-wins",
            "T-source-adoption-wins",
        );
        let adoption_job_id =
            seed_projected_target_adoption_job(&adoption_store, &adoption_operation);
        let (request, response) = target_adoption_coordinates(&adoption_operation);
        adoption_store
            .publish_worker_handoff_adoption_receipt(
                &adoption_job_id,
                &adoption_operation,
                &request,
                &response,
            )
            .unwrap();
        assert!(
            adoption_store
                .publish_worker_handoff_abort_fence(&adoption_operation, &"e".repeat(64))
                .unwrap_err()
                .to_string()
                .contains("competing target branch")
        );
        assert_eq!(
            adoption_store
                .worker_handoff_adoption_receipt(&adoption_operation.operation_id)
                .unwrap()
                .unwrap()
                .response,
            response
        );

        // One Store is live per process: each branch closes its Store before
        // the next opens.
        adoption_store.close().unwrap();
        let abort_store = test_store();
        let abort_operation = handoff_test_operation(
            crate::worker_handoff::WorkerHandoffJobRole::Target,
            "abort-wins",
            "T-source-abort-wins",
        );
        let abort_job_id = seed_projected_target_adoption_job(&abort_store, &abort_operation);
        let (request, response) = target_adoption_coordinates(&abort_operation);
        abort_store
            .publish_worker_handoff_abort_fence(&abort_operation, &"e".repeat(64))
            .unwrap();
        assert!(
            abort_store
                .publish_worker_handoff_adoption_receipt(
                    &abort_job_id,
                    &abort_operation,
                    &request,
                    &response,
                )
                .is_err()
        );
        assert!(
            abort_store
                .worker_handoff_abort_fence(&abort_operation.operation_id)
                .unwrap()
                .is_some()
        );

        abort_store.close().unwrap();
        let completion_store = test_store();
        let completion_operation = handoff_test_operation(
            crate::worker_handoff::WorkerHandoffJobRole::Target,
            "completion-wins",
            "T-source-completion-wins",
        );
        let completion_job_id =
            seed_projected_target_adoption_job(&completion_store, &completion_operation);
        let (completion_request, adoption_response) =
            target_adoption_coordinates(&completion_operation);
        let completion = crate::worker_handoff::WorkerPlacementCompletionResponse {
            operation_id: completion_operation.operation_id.clone(),
            chain_root_id: completion_operation.chain_root_id.clone(),
            placement_thread_id: completion_operation.successor_placement_thread_id.clone(),
            target_chain_head_hash: "f".repeat(64),
            terminal_status: "completed".to_owned(),
            credential_disposition: "completed_session_released".to_owned(),
        };
        let completion_hash = completion_store
            .publish_worker_handoff_terminal_completion(
                &completion_operation,
                &completion_request,
                &completion,
            )
            .unwrap();
        assert_eq!(
            completion_store
                .publish_worker_handoff_terminal_completion(
                    &completion_operation,
                    &completion_request,
                    &completion,
                )
                .unwrap(),
            completion_hash
        );
        assert_eq!(
            completion_store
                .worker_handoff_terminal_completion(&completion_operation.operation_id)
                .unwrap()
                .unwrap()
                .completion,
            completion
        );
        assert!(
            completion_store
                .publish_worker_handoff_adoption_receipt(
                    &completion_job_id,
                    &completion_operation,
                    &completion_request,
                    &adoption_response,
                )
                .is_err()
        );
        assert!(
            completion_store
                .publish_worker_handoff_abort_fence(&completion_operation, &"e".repeat(64))
                .is_err()
        );
        assert!(
            completion_store
                .publish_worker_handoff_terminal_failure(
                    &completion_operation,
                    &completion_request,
                    &crate::worker_handoff::WorkerPlacementFailureResponse {
                        operation_id: completion_operation.operation_id.clone(),
                        chain_root_id: completion_operation.chain_root_id.clone(),
                        placement_thread_id: completion_operation
                            .successor_placement_thread_id
                            .clone(),
                        target_chain_head_hash: "d".repeat(64),
                        terminal_status: "failed".to_owned(),
                        credential_disposition: "consumed_session_terminal".to_owned(),
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn rejected_verified_sync_payload_cannot_poison_a_same_process_retry() {
        let store = test_store();
        let job_id = "job:verified-sync-retry";
        store
            .with_state_db(|db| {
                db.create_sync_job(&ryeos_state::NewSyncJob {
                    job_id: job_id.to_owned(),
                    operation_type: "verified_sync_retry".to_owned(),
                    operation: serde_json::json!({
                        "schema": 1,
                        "operation_type": "verified_sync_retry",
                    }),
                    peer: Some("remote".to_owned()),
                    roots: Vec::new(),
                    heads: Vec::new(),
                    max_attempts: 1,
                })
            })
            .unwrap();
        let value = serde_json::json!({
            "schema": "ryeos.test.verified_sync_retry.v1",
            "coordinate": "exact",
        });
        let bytes = lillux::canonical_json(&value).unwrap().into_bytes();
        let hash = lillux::sha256_hex(&bytes);
        let payload = ryeos_state::sync::ExportPayload {
            chain_root_id: "T-verified-sync-retry".to_owned(),
            chain_head_hash: hash.clone(),
            entries: vec![ryeos_state::sync::SyncEntry {
                hash: hash.clone(),
                is_blob: false,
                data: bytes,
            }],
            total_bytes: lillux::canonical_json(&value).unwrap().len(),
        };
        let before = store
            .with_state_db(|db| db.get_sync_job(job_id))
            .unwrap()
            .unwrap();

        let rejected = store.stage_verified_sync_payload_for_existing_job(
            &payload,
            &ryeos_state::sync::ImportAttribution::default(),
            job_id,
            "remote_proof",
            std::slice::from_ref(&hash),
            Some(serde_json::json!({"proof":"rejected"})),
            |_| anyhow::bail!("synthetic semantic proof failure"),
        );
        assert!(
            rejected
                .unwrap_err()
                .to_string()
                .contains("synthetic semantic proof failure")
        );
        assert_eq!(
            store
                .with_state_db(|db| db.get_sync_job(job_id))
                .unwrap()
                .unwrap(),
            before
        );

        store
            .stage_verified_sync_payload_for_existing_job(
                &payload,
                &ryeos_state::sync::ImportAttribution::default(),
                job_id,
                "remote_proof",
                std::slice::from_ref(&hash),
                Some(serde_json::json!({"proof":"accepted"})),
                |cas| {
                    anyhow::ensure!(cas.get_object(&hash)?.as_ref() == Some(&value));
                    Ok(())
                },
            )
            .unwrap();
        let accepted = store
            .with_state_db(|db| db.get_sync_job(job_id))
            .unwrap()
            .unwrap();
        assert_eq!(accepted.roots, vec![hash]);
        assert_eq!(
            accepted.result,
            Some(serde_json::json!({"proof":"accepted"}))
        );
    }

    #[test]
    fn concurrent_target_branch_publishers_cannot_overwrite_each_other() {
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let operation = handoff_test_operation(
            crate::worker_handoff::WorkerHandoffJobRole::Target,
            "concurrent-branch",
            "T-source-concurrent-branch",
        );
        let job_id = seed_projected_target_adoption_job(&store, &operation);
        let (request, response) = target_adoption_coordinates(&operation);
        let ready = Arc::new(std::sync::Barrier::new(3));
        let (adoption, abort) = std::thread::scope(|scope| {
            let adoption_store = Arc::clone(&store);
            let adoption_ready = Arc::clone(&ready);
            let adoption_operation = operation.clone();
            let adoption_job_id = job_id.clone();
            let adoption = scope.spawn(move || {
                adoption_ready.wait();
                adoption_store.publish_worker_handoff_adoption_receipt(
                    &adoption_job_id,
                    &adoption_operation,
                    &request,
                    &response,
                )
            });
            let abort_store = Arc::clone(&store);
            let abort_ready = Arc::clone(&ready);
            let abort_operation = operation.clone();
            let abort = scope.spawn(move || {
                abort_ready.wait();
                abort_store.publish_worker_handoff_abort_fence(&abort_operation, &"e".repeat(64))
            });
            ready.wait();
            (adoption.join().unwrap(), abort.join().unwrap())
        });
        assert_ne!(
            adoption.is_ok(),
            abort.is_ok(),
            "exactly one competing target branch must win"
        );
        let head = store
            .with_state_db(|db| {
                db.read_generic_head_ref(
                    crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                    &operation.operation_id,
                )
            })
            .unwrap()
            .expect("winning target branch head");
        if adoption.is_ok() {
            assert_eq!(
                store
                    .worker_handoff_adoption_receipt(&operation.operation_id)
                    .unwrap()
                    .unwrap()
                    .response
                    .operation_id,
                operation.operation_id
            );
        } else {
            assert_eq!(
                store
                    .worker_handoff_abort_fence(&operation.operation_id)
                    .unwrap()
                    .unwrap()
                    .target_operation
                    .operation_id,
                operation.operation_id
            );
        }
        assert!(!head.target_hash.is_empty());
    }

    #[test]
    fn contradictory_projected_attachment_cannot_publish_a_branch_head() {
        let store = test_store();
        let operation = handoff_test_operation(
            crate::worker_handoff::WorkerHandoffJobRole::Target,
            "contradictory-adoption",
            "T-source-contradictory-adoption",
        );
        let job_id = seed_projected_target_adoption_job(&store, &operation);
        let (mut request, mut response) = target_adoption_coordinates(&operation);
        request.writer_grant_hash = "f".repeat(64);
        response.target_chain_head_hash = request.target_chain_head_hash.clone();

        assert!(
            store
                .publish_worker_handoff_adoption_receipt(&job_id, &operation, &request, &response,)
                .unwrap_err()
                .to_string()
                .contains("projected attachment authority")
        );
        assert!(
            store
                .with_state_db(|db| {
                    db.read_generic_head_ref(
                        crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                        &operation.operation_id,
                    )
                })
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn target_abort_claim_is_durable_for_existing_prepared_job_and_hash_exact() {
        let store = test_store();
        let operation = handoff_test_operation(
            crate::worker_handoff::WorkerHandoffJobRole::Target,
            "abort-claim",
            "T-source-abort-claim",
        );
        let progress = target_handoff_progress(
            &operation.operation_id,
            crate::worker_handoff::WorkerHandoffPhase::TargetPrepared,
        );
        let job_id = format!("worker-handoff-target:{}", operation.operation_id);
        let source_head = operation.source_chain_head_hash.clone();
        store
            .with_state_db(|db| {
                db.create_sync_job_with_initial_progress(
                    &ryeos_state::NewSyncJob {
                        job_id: job_id.clone(),
                        operation_type: crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
                            .to_owned(),
                        operation: operation.to_value()?,
                        peer: Some("source".to_owned()),
                        roots: vec![source_head.clone()],
                        heads: vec![source_head.clone()],
                        max_attempts: crate::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
                    },
                    ryeos_state::SyncJobState::Running,
                    progress.phase.as_str(),
                    Some(&progress.to_value()?),
                )
            })
            .unwrap();
        let request = ryeos_state::NewSyncJob {
            job_id: job_id.clone(),
            operation_type: crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
            operation: operation.to_value().unwrap(),
            peer: Some("source".to_owned()),
            roots: Vec::new(),
            heads: Vec::new(),
            max_attempts: crate::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
        };
        let abort_head = "e".repeat(64);
        let claimed = store
            .claim_worker_handoff_target_abort(&request, &abort_head)
            .unwrap();
        assert_eq!(
            claimed.phase,
            crate::worker_handoff::WORKER_HANDOFF_TARGET_ABORT_CLAIM_PHASE
        );
        assert_eq!(claimed.heads, vec![abort_head.clone()]);
        assert_eq!(claimed.roots, vec![source_head]);
        assert_eq!(
            crate::worker_handoff::WorkerSessionHandoffProgress::from_value(
                claimed.result.clone().unwrap()
            )
            .unwrap(),
            progress
        );
        assert!(
            store
                .claim_worker_handoff_target_abort(&request, &"f".repeat(64))
                .is_err()
        );
        assert_eq!(
            store
                .with_state_db(|db| db.get_sync_job(&job_id))
                .unwrap()
                .unwrap(),
            claimed
        );
    }

    #[test]
    fn duplicate_preparation_cannot_regress_post_cut_job_or_roots() {
        for phase in [
            crate::worker_handoff::WorkerHandoffPhase::SourceCommitted,
            crate::worker_handoff::WorkerHandoffPhase::TargetAdopted,
        ] {
            let operation = handoff_test_operation(
                crate::worker_handoff::WorkerHandoffJobRole::Target,
                phase.as_str(),
                "T-source-post-cut",
            );
            let current = target_handoff_progress(&operation.operation_id, phase);
            let requested = target_handoff_progress(
                &operation.operation_id,
                crate::worker_handoff::WorkerHandoffPhase::TargetPrepared,
            );
            let job = ryeos_state::SyncJobRecord {
                job_id: format!("worker-handoff-target:{}", operation.operation_id),
                operation_type: crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
                operation: operation.to_value().unwrap(),
                peer: Some("source".to_owned()),
                state: ryeos_state::SyncJobState::Running,
                phase: phase.as_str().to_owned(),
                roots: vec![
                    operation.transfer_manifest_hash.clone(),
                    "a".repeat(64),
                    "b".repeat(64),
                    "c".repeat(64),
                    "d".repeat(64),
                ],
                heads: vec!["d".repeat(64)],
                uploaded_hashes: vec!["d".repeat(64)],
                fetched_hashes: Vec::new(),
                attempt_count: 1,
                max_attempts: crate::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
                last_error: None,
                result: Some(current.to_value().unwrap()),
                created_at: "2026-01-01T00:00:00Z".to_owned(),
                updated_at: "2026-01-01T00:00:00Z".to_owned(),
                finished_at: None,
            };
            let before = job.clone();
            assert!(
                validate_target_preparation_job_transition(
                    &job,
                    &operation.transfer_manifest_hash,
                    &requested,
                )
                .is_err()
            );
            assert_eq!(job, before);
        }
    }

    #[test]
    fn active_source_handoff_query_fails_closed_on_duplicate_placement_owner() {
        let store = test_store();
        for label in ["source-owner-a", "source-owner-b"] {
            let operation = handoff_test_operation(
                crate::worker_handoff::WorkerHandoffJobRole::Source,
                label,
                "T-one-source-placement",
            );
            let progress = crate::worker_handoff::WorkerSessionHandoffProgress::source_exported(
                operation.operation_id.clone(),
            )
            .unwrap();
            store
                .with_state_db(|db| {
                    db.create_sync_job_with_initial_progress(
                        &ryeos_state::NewSyncJob {
                            job_id: format!("worker-handoff-source:{}", operation.operation_id),
                            operation_type: crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
                                .to_owned(),
                            operation: operation.to_value()?,
                            peer: Some("target".to_owned()),
                            roots: vec![operation.transfer_manifest_hash.clone()],
                            heads: vec![operation.source_chain_head_hash.clone()],
                            max_attempts:
                                crate::worker_handoff::WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
                        },
                        ryeos_state::SyncJobState::Running,
                        progress.phase.as_str(),
                        Some(&progress.to_value()?),
                    )
                })
                .unwrap();
            if label == "source-owner-a" {
                assert!(
                    store
                        .active_source_worker_handoff_for_placement("T-one-source-placement")
                        .unwrap()
                        .is_some()
                );
            }
        }
        assert!(
            store
                .active_source_worker_handoff_for_placement("T-one-source-placement")
                .is_err()
        );
    }

    #[test]
    fn credential_authority_reconciles_a_runtime_commit_gap_on_reopen() {
        let tmp = tempdir().expect("tempdir").keep();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
                .expect("test node identity");
        let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let trust = Arc::new(trust);
        let open = || {
            TestStateStore::open(&tmp, Arc::clone(&signer), Arc::clone(&trust))
                .expect("state store")
        };

        let store = open();
        store
            .create_credential_profile(NewCredentialProfile {
                profile_id: "profile-gap",
                owner_principal: "fp:operator",
                home_id: "credential-gap",
            })
            .unwrap();
        store
            .acquire_credential_profile("profile-gap", "fp:operator", "login-gap")
            .unwrap();
        {
            // Deliberately stop after RuntimeDb's durable transition, before
            // StateStore folds the higher revision into OperationalDb.
            let guard = store.lock().unwrap();
            guard
                .runtime_db
                .begin_credential_enrollment(
                    "profile-gap",
                    "login-gap",
                    "ceremony-gap",
                    lillux::time::timestamp_millis() as i64 + 60_000,
                )
                .unwrap();
        }
        drop(store);

        let reopened = open();
        let profile = reopened.credential_profile("profile-gap").unwrap().unwrap();
        assert_eq!(profile.state, "enrolling");
        assert_eq!(profile.authority_revision, 2);
        let stable = reopened
            .with_state_db(|db| db.operational_credential_profiles())
            .unwrap();
        assert_eq!(stable.len(), 1);
        assert_eq!(stable[0].authority_revision, 2);
        assert_eq!(stable[0].state, "enrolling");
    }

    #[test]
    fn recreated_credential_profile_persists_its_advanced_generation() {
        let tmp = tempdir().expect("tempdir").keep();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
                .expect("test node identity");
        let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let trust = Arc::new(trust);
        let open = || {
            TestStateStore::open(&tmp, Arc::clone(&signer), Arc::clone(&trust))
                .expect("state store")
        };

        let store = open();
        let profile = NewCredentialProfile {
            profile_id: "profile-recreated",
            owner_principal: "fp:operator",
            home_id: "credential-recreated",
        };
        store.create_credential_profile(profile.clone()).unwrap();
        let revoked_generation = store
            .revoke_credential_profile("profile-recreated", "fp:operator", 1)
            .unwrap();
        store
            .finish_credential_profile_revocation(
                "profile-recreated",
                "fp:operator",
                revoked_generation,
            )
            .unwrap();
        let deleted_generation = store
            .begin_credential_profile_deletion(
                "profile-recreated",
                "fp:operator",
                revoked_generation,
            )
            .unwrap();
        store
            .finish_credential_profile_deletion(
                "profile-recreated",
                "fp:operator",
                deleted_generation,
            )
            .unwrap();
        store.create_credential_profile(profile).unwrap();
        drop(store);

        let reopened = open();
        let recreated = reopened
            .credential_profile("profile-recreated")
            .unwrap()
            .expect("recreated profile after reopen");
        assert_eq!(recreated.credential_generation, deleted_generation + 1);
        assert_eq!(recreated.state, "unauthenticated");
        let stable = reopened
            .with_state_db(|db| db.operational_credential_profiles())
            .unwrap();
        assert_eq!(stable.len(), 1);
        assert_eq!(stable[0].credential_generation, deleted_generation + 1);
        assert_eq!(stable[0].state, "unauthenticated");
    }

    #[test]
    fn terminal_credential_cancellation_folds_stable_authority_and_reopens() {
        let tmp = tempdir().expect("tempdir").keep();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
                .expect("test node identity");
        let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let trust = Arc::new(trust);
        let open = || {
            TestStateStore::open(&tmp, Arc::clone(&signer), Arc::clone(&trust))
                .expect("state store")
        };

        let store = open();
        {
            let guard = store.lock().unwrap();
            guard
                .runtime_db
                .reserve_workspace("W-terminal-profile", &"a".repeat(64), "/workspace")
                .unwrap();
            guard
                .runtime_db
                .transition_workspace(
                    "W-terminal-profile",
                    &[runtime_db::WorkspaceState::Reserved],
                    runtime_db::WorkspaceState::Constructing,
                    None,
                )
                .unwrap();
            guard
                .runtime_db
                .claim_workspace_construction(
                    "W-terminal-profile",
                    "T-terminal-profile",
                    "launch-owner",
                )
                .unwrap();
            guard
                .runtime_db
                .bind_workspace(runtime_db::WorkspaceBinding {
                    workspace_id: "W-terminal-profile",
                    thread_id: "T-terminal-profile",
                    launch_owner: Some("launch-owner"),
                    backend_id: Some("native"),
                    backend_version: Some("fixture"),
                    pinned_root_identities: Some("fixture-roots"),
                    mount_identity: Some("fixture-view"),
                    workspace_output_partition_identity: None,
                    base_output_capture_hash: None,
                })
                .unwrap();
        }
        store
            .create_credential_profile(NewCredentialProfile {
                profile_id: "P-terminal-profile",
                owner_principal: "fp:operator",
                home_id: "credential-terminal-profile",
            })
            .unwrap();
        store
            .acquire_credential_profile(
                "P-terminal-profile",
                "fp:operator",
                "worker-terminal-profile",
            )
            .unwrap();
        store
            .admit_dedicated_session(NewDedicatedSession {
                placement_thread_id: "T-terminal-profile",
                chain_root_id: "T-terminal-profile",
                owner_principal: "fp:operator",
                admitted_capsule_hash: &"b".repeat(64),
                workspace_id: "W-terminal-profile",
                candidate_required: false,
                candidate_disposition: runtime_db::DedicatedCandidateDisposition::OwnerDecision,
                credential_profile_id: "P-terminal-profile",
                credential_generation: 1,
                credential_lock_owner: "worker-terminal-profile",
            })
            .unwrap();
        {
            let guard = store.lock().unwrap();
            guard
                .runtime_db
                .attach_worker_process(&WorkerProcessRecord {
                    worker_instance_id: "worker-terminal-profile".to_owned(),
                    boot_identity_hash: "c".repeat(64),
                    session_capsule_hash: "b".repeat(64),
                    boot_epoch: 1,
                    lifecycle_generation: 1,
                    process_identity: crate::process::ExecutionProcessIdentity {
                        schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                        boot_id: "fixture-boot".to_owned(),
                        target_pid: 101,
                        target_start_time_ticks: 201,
                        group_leader_pid: 101,
                        group_leader_start_time_ticks: 201,
                        process_scope: None,
                        resource_selections: Vec::new(),
                        resource_settlement_authority: None,
                        resource_operations: Vec::new(),
                        resource_allocation_limit: None,
                        resource_occupancy_start: None,
                        resource_occupancy_limit: None,
                        resource_cleanup_allowance_ms: None,
                    },
                    control_channel_identity: "fixture-control".to_owned(),
                    state: runtime_db::WorkerProcessState::Attached,
                    daemon_generation_id: "daemon:fixture".to_owned(),
                    placement_thread_id: "T-terminal-profile".to_owned(),
                    cleanup_state: "owned".to_owned(),
                    created_at_ms: 1,
                    updated_at_ms: 1,
                })
                .unwrap();
        }
        store
            .complete_worker_binding("worker-terminal-profile", "T-terminal-profile", 1)
            .unwrap();
        store
            .begin_credential_enrollment(
                "P-terminal-profile",
                "worker-terminal-profile",
                "login-terminal-profile",
                lillux::time::timestamp_millis() as i64 + 60_000,
            )
            .unwrap();
        store
            .settle_worker_process(
                "worker-terminal-profile",
                "T-terminal-profile",
                1,
                "reaped",
                "cancelled",
            )
            .unwrap();
        store
            .terminalize_dedicated_session(
                "T-terminal-profile",
                "worker-terminal-profile",
                1,
                "cancelled",
            )
            .unwrap();
        let stable = store
            .with_state_db(|db| db.operational_credential_profiles())
            .unwrap();
        let stable = stable
            .iter()
            .find(|profile| profile.profile_id == "P-terminal-profile")
            .unwrap();
        assert_eq!(stable.state, "unauthenticated");
        assert_eq!(stable.authority_revision, 3);
        drop(store);

        let reopened = open();
        let runtime = reopened
            .credential_profile("P-terminal-profile")
            .unwrap()
            .unwrap();
        assert_eq!(runtime.state, "unauthenticated");
        assert_eq!(runtime.authority_revision, 3);
        let stable = reopened
            .with_state_db(|db| db.operational_credential_profiles())
            .unwrap();
        let stable = stable
            .iter()
            .find(|profile| profile.profile_id == "P-terminal-profile")
            .unwrap();
        assert_eq!(stable.state, "unauthenticated");
        assert_eq!(stable.authority_revision, 3);
    }

    #[test]
    fn replay_object_verification_runs_outside_the_state_mutex() {
        let store = test_store();
        let namespace = ryeos_state::ReplayIndexNamespace::new("dispatch.effect").unwrap();
        let indexed = ryeos_state::ReplayIndexRecord {
            cache_key: "a".repeat(64),
            answer_digest: "b".repeat(64),
            record_hash: "c".repeat(64),
        };
        store
            .with_state_db(|db| {
                db.publish_replay_record(&namespace, &indexed, |_| {
                    ryeos_state::ReplayRecordVerification::Verified
                })
            })
            .expect("publish replay row");

        let outcome = store
            .lookup_replay_record(&namespace, &indexed.cache_key, |_| {
                assert!(
                    store.inner.try_lock().is_ok(),
                    "CAS verification must not hold the node-wide state mutex"
                );
                ryeos_state::ReplayRecordVerification::Verified
            })
            .expect("lookup replay row");
        assert_eq!(outcome, ryeos_state::ReplayLookupOutcome::Present(indexed));
    }

    #[test]
    fn predecessor_released_binding_head_requires_explicit_epoch_reset() {
        let tmp = tempdir().unwrap();
        let runtime_state_dir = tmp.path().join(".ai/state");
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&tmp.path().join("node-key.pem"))
                .expect("test node identity");
        let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let trust = Arc::new(trust);
        let state =
            ryeos_state::test_support::TestState::open(&runtime_state_dir, Arc::clone(&trust))
                .unwrap();
        let authority = state.pinned_authority().unwrap();
        let guard = authority.acquire_exclusive_guard(true).unwrap();
        let active = ryeos_state::objects::ExternalContentBinding::active(
            "a".repeat(64),
            ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            ryeos_state::objects::ExternalContentConsumerAuthority::installed_bundle(
                "worker:tests/old".to_owned(),
                "b".repeat(64),
            )
            .unwrap(),
            identity.fingerprint().to_owned(),
            "c".repeat(64),
            "b".repeat(64),
        )
        .unwrap();
        let released = ryeos_state::objects::ExternalContentBinding::released_from(
            &active,
            "c".repeat(64),
            "b".repeat(64),
        )
        .unwrap();
        let binding_hash = authority
            .cas_store()
            .unwrap()
            .store_object(&released.to_value().unwrap())
            .unwrap();
        state
            .write_generic_head_ref(
                ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                &released.binding_subject_id,
                &binding_hash,
                signer.as_ref(),
                &guard,
            )
            .unwrap();
        drop(guard);
        drop(authority);
        drop(state);

        let refusal = TestStateStore::open_refusal(tmp.path(), signer, trust);
        assert!(
            refusal.contains("ryeos node reset external-content-bindings"),
            "{refusal}"
        );
    }

    #[test]
    fn daemon_bundle_event_append_serializes_concurrent_writers_at_current_head() {
        const WRITERS: usize = 12;
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let start = Arc::new(std::sync::Barrier::new(WRITERS));
        let writers = (0..WRITERS)
            .map(|index| {
                let store = Arc::clone(&store);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    store
                        .append_daemon_bundle_event_at_current_head(
                            ryeos_state::BundleEventAppendRequest {
                                effective_bundle_id: "ryeos-node".to_string(),
                                bundle_id: None,
                                event_kind: "admission".to_string(),
                                chain_id: "project-test".to_string(),
                                event_type: "admission_recorded".to_string(),
                                schema_version: 1,
                                payload: json!({"writer": index}),
                                expected_chain_head_hash: None,
                                idempotency_key: None,
                                correlation_id: None,
                                causation_id: None,
                                attribution: ryeos_state::objects::BundleEventAttribution::default(
                                ),
                                attachments: vec![],
                            },
                        )
                        .expect("daemon-owned append")
                })
            })
            .collect::<Vec<_>>();

        for writer in writers {
            writer.join().expect("writer thread");
        }

        let page = store
            .read_bundle_event_chain_page(
                "ryeos-node",
                "admission",
                "project-test",
                None,
                WRITERS,
                ryeos_state::objects::MAX_BUNDLE_EVENT_SERIALIZED_BYTES,
            )
            .expect("read daemon-owned event chain");
        assert_eq!(page.records.len(), WRITERS);
        let mut sequences = page
            .records
            .iter()
            .map(|record| record.event.chain_seq)
            .collect::<Vec<_>>();
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=WRITERS as u64).collect::<Vec<_>>());
    }

    fn positioned_test_event(
        thread_id: &str,
        chain_seq: u64,
        thread_seq: u64,
        prev_chain_event_hash: Option<String>,
        prev_thread_event_hash: Option<String>,
    ) -> ryeos_state::objects::ThreadEvent {
        ryeos_state::objects::ThreadEvent {
            schema: ryeos_state::objects::SCHEMA_VERSION,
            kind: "thread_event".to_string(),
            chain_root_id: "T-root".to_string(),
            chain_seq,
            thread_id: thread_id.to_string(),
            thread_seq,
            event_type: "test_event".to_string(),
            durability: ryeos_state::objects::EventDurability::Durable,
            ts: "2026-07-24T00:00:00.000Z".to_string(),
            prev_chain_event_hash,
            prev_thread_event_hash,
            payload: json!({}),
        }
    }

    #[test]
    fn event_append_readback_proves_exact_batch_from_both_predecessor_anchors() {
        let predecessor_chain_hash = "a".repeat(64);
        let predecessor_thread_hash = "b".repeat(64);
        let first_hash = "c".repeat(64);
        let second_hash = "d".repeat(64);
        let predecessor = ryeos_state::AuthoritativeThreadAppendAnchorReadback {
            chain_last_event_hash: Some(predecessor_chain_hash.clone()),
            chain_last_seq: 40,
            thread_last_event_hash: Some(predecessor_thread_hash.clone()),
            thread_last_seq: 7,
        };
        let first = positioned_test_event(
            "T-root",
            41,
            8,
            Some(predecessor_chain_hash),
            Some(predecessor_thread_hash),
        );
        let second = positioned_test_event(
            "T-root",
            42,
            9,
            Some(first_hash.clone()),
            Some(first_hash.clone()),
        );
        let readback = ryeos_state::AuthoritativeThreadAppendHistoryReadback {
            anchor: ryeos_state::AuthoritativeThreadAppendAnchorReadback {
                chain_last_event_hash: Some(second_hash.clone()),
                chain_last_seq: 42,
                thread_last_event_hash: Some(second_hash),
                thread_last_seq: 9,
            },
            events: vec![
                (first_hash, first.clone()),
                ("d".repeat(64), second.clone()),
            ],
        };

        assert!(matches!(
            classify_event_append_readback(
                Some(&readback),
                &predecessor,
                &[first, second]
            )
            .unwrap(),
            EventAppendCommitReadback::Exact(events) if events.len() == 2
        ));
    }

    #[test]
    fn event_append_readback_only_retries_a_proven_unchanged_head() {
        let predecessor = ryeos_state::AuthoritativeThreadAppendAnchorReadback {
            chain_last_event_hash: Some("a".repeat(64)),
            chain_last_seq: 10,
            thread_last_event_hash: Some("b".repeat(64)),
            thread_last_seq: 4,
        };
        let unchanged = ryeos_state::AuthoritativeThreadAppendHistoryReadback {
            anchor: predecessor.clone(),
            events: Vec::new(),
        };
        let expected = positioned_test_event("T-root", 0, 1, None, None);
        assert!(matches!(
            classify_event_append_readback(
                Some(&unchanged),
                &predecessor,
                std::slice::from_ref(&expected)
            )
            .unwrap(),
            EventAppendCommitReadback::ProvenAbsent
        ));

        let advanced = ryeos_state::AuthoritativeThreadAppendHistoryReadback {
            anchor: ryeos_state::AuthoritativeThreadAppendAnchorReadback {
                chain_last_event_hash: Some("c".repeat(64)),
                chain_last_seq: 11,
                thread_last_event_hash: Some("d".repeat(64)),
                thread_last_seq: 5,
            },
            events: vec![(
                "d".repeat(64),
                ryeos_state::objects::ThreadEvent {
                    payload: json!({"different": true}),
                    ..expected
                },
            )],
        };
        assert!(matches!(
            classify_event_append_readback(
                Some(&advanced),
                &predecessor,
                &[positioned_test_event("T-root", 0, 1, None, None)]
            )
            .unwrap(),
            EventAppendCommitReadback::Ambiguous(_)
        ));
    }

    #[test]
    fn atomic_continuation_positions_reject_injected_source_event() {
        let predecessor_chain_hash = "a".repeat(64);
        let predecessor_source_hash = "b".repeat(64);
        let successor_hash = "c".repeat(64);
        let predecessor = ryeos_state::AuthoritativeContinuationPredecessorReadback {
            updated_at: "2026-07-24T00:00:00.000Z".to_string(),
            last_event_hash: Some(predecessor_chain_hash.clone()),
            last_chain_seq: 10,
            source_was_present: true,
            source_last_event_hash: Some(predecessor_source_hash.clone()),
            source_last_thread_seq: 7,
            successor_was_absent: true,
        };
        let successor_events = vec![(
            successor_hash.clone(),
            positioned_test_event("T-successor", 11, 1, Some(predecessor_chain_hash), None),
        )];
        let exact_source = positioned_test_event(
            "T-source",
            12,
            8,
            Some(successor_hash),
            Some(predecessor_source_hash),
        );
        assert!(has_exact_atomic_continuation_positions(
            &predecessor,
            &successor_events,
            &exact_source,
        ));

        let injected_hash = "d".repeat(64);
        let source_after_injected_event = positioned_test_event(
            "T-source",
            13,
            9,
            Some(injected_hash.clone()),
            Some(injected_hash),
        );
        assert!(!has_exact_atomic_continuation_positions(
            &predecessor,
            &successor_events,
            &source_after_injected_event,
        ));
    }

    #[test]
    fn exact_root_event_proof_rejects_extra_or_divergent_events() {
        let first_hash = "e".repeat(64);
        let second_hash = "f".repeat(64);
        let third_hash = "1".repeat(64);
        let first = positioned_test_event("T-root", 1, 1, None, None);
        let second = positioned_test_event(
            "T-root",
            2,
            2,
            Some(first_hash.clone()),
            Some(first_hash.clone()),
        );
        let expected = vec![first.clone(), second.clone()];
        let mut with_extra = vec![(first_hash.clone(), first), (second_hash.clone(), second)];
        with_extra.push((
            third_hash,
            positioned_test_event("T-root", 3, 3, Some(second_hash.clone()), Some(second_hash)),
        ));
        assert!(has_exact_root_event_positions(&with_extra));
        assert!(!same_successor_event_batch(&with_extra, &expected));

        let mut divergent = with_extra[..2].to_vec();
        divergent[1].1.prev_thread_event_hash = Some("2".repeat(64));
        assert!(!has_exact_root_event_positions(&divergent));

        let expected_snapshot = build_snapshot(&thread_record("T-root", "T-root"));
        let mut divergent_snapshot = expected_snapshot.clone();
        divergent_snapshot.result = Some(json!({"unexpected": true}));
        assert!(!same_exact_snapshot(
            &divergent_snapshot,
            &expected_snapshot,
        ));
    }

    #[tokio::test]
    async fn unbound_launch_cancel_commits_then_aborts_registered_task() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-planning", "fp:owner")
            .expect("reserve planning");
        assert!(
            store
                .get_thread("T-planning")
                .expect("read absent authoritative row")
                .is_none()
        );
        let task = tokio::spawn(std::future::pending::<()>());
        store
            .register_launch_task_abort("T-planning", task.abort_handle())
            .expect("register abort signal");

        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("cancel planning"),
            Some(LaunchCancellationResolution::Cancelled)
        );
        assert!(task.await.expect_err("task must be aborted").is_cancelled());
        store
            .settle_launch_planning_task_exit("T-planning")
            .expect("cancelled task exit must preserve cancelled outcome");
        assert!(
            store
                .get_thread("T-planning")
                .expect("read absent authoritative row after cancel")
                .is_none()
        );
        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("read terminal planning outcome"),
            Some(LaunchCancellationResolution::Terminal {
                state: "cancelled".to_string(),
                outcome_code: Some("cancelled_by_requester".to_string()),
            })
        );
    }

    #[test]
    fn caller_retained_launch_status_is_exact_owner_bound_and_terminal_on_admission_exit() {
        let store = test_store();
        let launch_id = "L-0123456789abcdef0123456789abcdef";
        assert!(is_canonical_launch_id(launch_id));
        assert!(!is_canonical_launch_id("L-not-a-canonical-coordinate"));
        store
            .reserve_launch_planning_with_id(launch_id, "T-reserved", "fp:owner")
            .expect("reserve caller coordinate");

        assert_eq!(
            store
                .launch_planning_status(launch_id, "fp:owner")
                .expect("read owner status"),
            Some(LaunchPlanningStatus {
                launch_id: launch_id.to_string(),
                thread_id: None,
                status: "planning".to_string(),
                outcome_code: None,
            })
        );
        assert_eq!(
            store
                .launch_planning_status(launch_id, "fp:other")
                .expect("foreign read is indistinguishable from absence"),
            None
        );

        store
            .settle_launch_planning_admission_exit("T-reserved")
            .expect("settle pre-handoff refusal");
        assert_eq!(
            store
                .launch_planning_status(launch_id, "fp:owner")
                .expect("read terminal owner status"),
            Some(LaunchPlanningStatus {
                launch_id: launch_id.to_string(),
                thread_id: None,
                status: "failed".to_string(),
                outcome_code: Some("launch_admission_failed".to_string()),
            })
        );
    }

    #[test]
    fn owner_status_repairs_committed_root_and_refuses_owner_disagreement() {
        let store = test_store();
        let thread = thread_record("T-status-repair", "T-status-repair");
        store
            .create_root_thread_with_events(&thread, Vec::new())
            .expect("publish authoritative root before auxiliary planning row");
        let launch_id = "L-11111111111111111111111111111111";
        store
            .reserve_launch_planning_with_id(launch_id, &thread.thread_id, "fp:test")
            .expect("reserve interrupted auxiliary planning row");
        assert_eq!(
            store
                .launch_planning_status(launch_id, "fp:test")
                .expect("status repairs exact committed root"),
            Some(LaunchPlanningStatus {
                launch_id: launch_id.to_string(),
                thread_id: Some(thread.thread_id.clone()),
                status: "bound".to_string(),
                outcome_code: Some("thread_bound".to_string()),
            })
        );

        let mismatched_thread = thread_record("T-status-mismatch", "T-status-mismatch");
        store
            .create_root_thread_with_events(&mismatched_thread, Vec::new())
            .expect("publish second authoritative root");
        let mismatched_id = "L-22222222222222222222222222222222";
        store
            .reserve_launch_planning_with_id(
                mismatched_id,
                &mismatched_thread.thread_id,
                "fp:other",
            )
            .expect("reserve deliberately mismatched planning owner");
        let error = store
            .launch_planning_status(mismatched_id, "fp:other")
            .expect_err("authoritative owner disagreement must fail closed");
        assert!(error.to_string().contains("owner disagrees"), "{error:#}");
    }

    #[tokio::test]
    async fn cancel_before_task_registration_aborts_at_registration() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-late-registration", "fp:owner")
            .expect("reserve planning");
        store
            .cancel_launch_planning(&launch_id, "fp:owner")
            .expect("cancel planning");
        let task = tokio::spawn(std::future::pending::<()>());
        store
            .register_launch_task_abort("T-late-registration", task.abort_handle())
            .expect("late registration");
        assert!(task.await.expect_err("task must be aborted").is_cancelled());
    }

    #[tokio::test]
    async fn launch_abort_registry_capacity_refusal_aborts_and_can_be_settled() {
        let store = test_store();
        store
            .reserve_launch_planning("T-capacity-one", "fp:owner")
            .expect("reserve first planning");
        let refused_launch_id = store
            .reserve_launch_planning("T-capacity-two", "fp:owner")
            .expect("reserve second planning");
        let first = tokio::spawn(std::future::pending::<()>());
        store
            .register_launch_task_abort_bounded("T-capacity-one", first.abort_handle(), 1)
            .expect("register first signal");
        let refused = tokio::spawn(std::future::pending::<()>());
        assert!(matches!(
            store.register_launch_task_abort_bounded("T-capacity-two", refused.abort_handle(), 1,),
            Err(LaunchTaskAbortRegistrationError::CapacityExceeded)
        ));
        assert!(
            refused
                .await
                .expect_err("capacity-refused task must be aborted")
                .is_cancelled()
        );

        store
            .settle_launch_planning_task_exit("T-capacity-two")
            .expect("settle refused planning");
        assert_eq!(
            store
                .cancel_launch_planning(&refused_launch_id, "fp:owner")
                .expect("read settled planning"),
            Some(LaunchCancellationResolution::Terminal {
                state: "failed".to_string(),
                outcome_code: Some("thread_creation_failed".to_string()),
            })
        );
        first.abort();
        assert!(
            first
                .await
                .expect_err("first task must be aborted during cleanup")
                .is_cancelled()
        );
    }

    #[test]
    fn planning_reservation_maps_only_typed_capacity_and_preserves_internal_errors() {
        let capacity = map_launch_planning_reservation_error(
            anyhow::Error::new(LaunchPlanningCapacityExceeded)
                .context("reserve durable planning row"),
        );
        assert!(matches!(
            capacity,
            LaunchPlanningReservationError::CapacityExceeded(_)
        ));

        let internal =
            map_launch_planning_reservation_error(anyhow::anyhow!("runtime database unavailable"));
        assert!(matches!(
            internal,
            LaunchPlanningReservationError::Internal(_)
        ));
    }

    #[test]
    fn foreign_launch_id_is_indistinguishable_from_nonexistent_and_does_not_cancel() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-owned-planning", "fp:owner")
            .expect("reserve planning");
        let foreign = store
            .cancel_launch_planning(&launch_id, "fp:other")
            .expect("hide non-owner");
        let nonexistent = store
            .cancel_launch_planning("L-does-not-exist", "fp:other")
            .expect("hide nonexistent");
        assert_eq!(foreign, nonexistent);
        assert_eq!(foreign, None);
        store
            .ensure_launch_planning_active("T-owned-planning")
            .expect("non-owner must not mutate planning");
    }

    #[tokio::test]
    async fn row_commit_before_handoff_routes_cancel_to_thread_without_aborting_launch_task() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-row-before-handoff", "fp:owner")
            .expect("reserve planning");
        let task = tokio::spawn(std::future::pending::<()>());
        store
            .register_launch_task_abort("T-row-before-handoff", task.abort_handle())
            .expect("register launch task");
        let mut root = thread_record("T-row-before-handoff", "T-row-before-handoff");
        root.requested_by = Some("fp:owner".to_string());
        store
            .create_thread_for_test(&root)
            .expect("commit and bind authoritative root");

        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("resolve post-row cancel"),
            Some(LaunchCancellationResolution::Bound {
                thread_id: "T-row-before-handoff".to_string(),
            })
        );
        assert!(
            !task.is_finished(),
            "binding must transfer cancellation to the durable thread rather than aborting its launch task"
        );
        task.abort();
        assert!(
            task.await
                .expect_err("test cleanup must abort launch task")
                .is_cancelled()
        );
        store
            .settle_launch_planning_task_exit("T-row-before-handoff")
            .expect("post-bind task exit must preserve thread binding");
        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("read binding after task exit"),
            Some(LaunchCancellationResolution::Bound {
                thread_id: "T-row-before-handoff".to_string(),
            })
        );
    }

    #[test]
    fn cancel_after_handoff_resolves_to_canonical_durable_thread_stop() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-after-handoff", "fp:owner")
            .expect("reserve planning");
        let mut root = thread_record("T-after-handoff", "T-after-handoff");
        root.requested_by = Some("fp:owner".to_string());
        store
            .create_thread_for_test(&root)
            .expect("commit and bind authoritative root");

        let foreign = store
            .cancel_launch_planning(&launch_id, "fp:other")
            .expect("hide foreign handed-off launch");
        let nonexistent = store
            .cancel_launch_planning("L-missing-after-handoff", "fp:other")
            .expect("hide nonexistent launch");
        assert_eq!(foreign, nonexistent);
        assert_eq!(foreign, None);

        let resolution = store
            .cancel_launch_planning(&launch_id, "fp:owner")
            .expect("resolve handed-off launch")
            .expect("owned launch");
        let LaunchCancellationResolution::Bound { thread_id } = resolution else {
            panic!("handed-off launch must resolve to its durable thread");
        };
        let runtime = store
            .request_thread_stop(&thread_id, StopIntent::Cancel)
            .expect("request canonical durable stop");
        assert_eq!(runtime.stop_intent, Some(StopIntent::Cancel));
        assert_eq!(thread_id, "T-after-handoff");
    }

    #[test]
    fn pre_bind_augmentation_or_launch_task_exit_settles_failed() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-pre-bind-task-exit", "fp:owner")
            .expect("reserve planning");

        store
            .settle_launch_planning_task_exit("T-pre-bind-task-exit")
            .expect("settle pre-bind task exit");

        assert!(
            store
                .get_thread("T-pre-bind-task-exit")
                .expect("read absent authoritative row")
                .is_none()
        );
        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("read settled launch outcome"),
            Some(LaunchCancellationResolution::Terminal {
                state: "failed".to_string(),
                outcome_code: Some("thread_creation_failed".to_string()),
            })
        );
    }

    #[test]
    fn root_publication_binds_planning_before_return() {
        let store = test_store();
        let thread_id = "T-root-direct-bind";
        let launch_id = store
            .reserve_launch_planning(thread_id, "fp:owner")
            .expect("reserve root planning");
        let mut root = thread_record(thread_id, thread_id);
        root.requested_by = Some("fp:owner".to_string());

        let publication = store
            .create_root_thread_with_events_and_launch_metadata(&root, Vec::new(), None)
            .expect("publish root and bind planning");
        assert_eq!(publication.successor.thread_id, thread_id);
        assert_eq!(publication.successor.chain_root_id, thread_id);
        assert!(publication.successor.upstream_thread_id.is_none());
        assert_eq!(publication.successor.status, ThreadStatus::Created.as_str());
        assert!(publication.successor.runtime.pid.is_none());
        assert!(publication.successor.runtime.launch_metadata.is_none());

        let g = store.lock().expect("inspect root publication");
        let planning = g
            .runtime_db
            .launch_planning_by_thread(thread_id)
            .expect("read planning directly")
            .expect("root planning record");
        assert_eq!(planning.launch_id, launch_id);
        assert_eq!(planning.state, "bound");
        assert_eq!(planning.bound_thread_id.as_deref(), Some(thread_id));
        assert!(
            g.state_db
                .get_thread(thread_id)
                .expect("read authoritative root directly")
                .is_some()
        );
    }

    #[test]
    fn launch_cancel_and_authoritative_root_bind_have_one_atomic_winner() {
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let launch_id = store
            .reserve_launch_planning("T-bind-race", "fp:owner")
            .expect("reserve planning");
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let creator_store = store.clone();
        let creator_barrier = barrier.clone();
        let creator = std::thread::spawn(move || {
            creator_barrier.wait();
            let mut root = thread_record("T-bind-race", "T-bind-race");
            root.requested_by = Some("fp:owner".to_string());
            creator_store.create_thread_for_test(&root)
        });

        let canceller_store = store.clone();
        let canceller_barrier = barrier.clone();
        let canceller = std::thread::spawn(move || {
            canceller_barrier.wait();
            canceller_store.cancel_launch_planning(&launch_id, "fp:owner")
        });

        barrier.wait();
        let created = creator.join().expect("creator thread");
        let cancelled = canceller
            .join()
            .expect("canceller thread")
            .expect("cancel resolution")
            .expect("owned planning record");
        let outcomes = (created, cancelled);
        match outcomes {
            (Ok(_), LaunchCancellationResolution::Bound { thread_id }) => {
                assert_eq!(thread_id, "T-bind-race")
            }
            (Err(error), LaunchCancellationResolution::Cancelled) => {
                assert!(
                    error
                        .chain()
                        .any(|cause| cause.is::<LaunchPlanningInactive>()),
                    "cancel-won publication must retain typed planning-inactive authority: {error:#}"
                );
            }
            (created, cancelled) => {
                panic!(
                    "invalid cancel-vs-bind race outcome: created={created:?}, cancelled={cancelled:?}"
                )
            }
        }
    }

    #[test]
    fn continuation_publication_binds_planning_before_return() {
        let store = test_store();
        let source_thread_id = "T-continuation-direct-bind-source";
        let successor_thread_id = "T-continuation-direct-bind-successor";
        let mut source = thread_record(source_thread_id, source_thread_id);
        source.requested_by = Some("fp:owner".to_string());
        store
            .create_thread_for_test(&source)
            .expect("create continuation source");
        let launch_id = store
            .reserve_launch_planning(successor_thread_id, "fp:owner")
            .expect("reserve continuation planning");
        let mut successor = thread_record(successor_thread_id, source_thread_id);
        successor.requested_by = Some("fp:owner".to_string());

        let publication = store
            .create_continuation_admitted(
                &successor,
                source_thread_id,
                source_thread_id,
                Some("chained_resume"),
                Vec::new(),
                None,
            )
            .expect("publish continuation and bind planning");
        assert_eq!(publication.successor.thread_id, successor_thread_id);
        assert_eq!(publication.successor.chain_root_id, source_thread_id);
        assert_eq!(
            publication.successor.upstream_thread_id.as_deref(),
            Some(source_thread_id)
        );
        assert_eq!(publication.successor.status, ThreadStatus::Created.as_str());
        assert!(publication.successor.runtime.pid.is_none());
        assert!(publication.successor.runtime.launch_metadata.is_none());
        assert_eq!(
            store
                .current_chain_placement_thread_id(source_thread_id)
                .expect("resolve authoritative placement"),
            Some(successor_thread_id.to_string())
        );
        assert_eq!(
            store
                .current_chain_placement_thread_id("T-absent-chain")
                .expect("resolve absent chain"),
            None
        );

        let g = store.lock().expect("inspect continuation publication");
        let planning = g
            .runtime_db
            .launch_planning_by_thread(successor_thread_id)
            .expect("read planning directly")
            .expect("continuation planning record");
        assert_eq!(planning.launch_id, launch_id);
        assert_eq!(planning.state, "bound");
        assert_eq!(
            planning.bound_thread_id.as_deref(),
            Some(successor_thread_id)
        );
        assert!(
            g.state_db
                .get_thread(successor_thread_id)
                .expect("read authoritative successor directly")
                .is_some()
        );
        assert_eq!(
            queries::continuation_successor(g.state_db.projection(), source_thread_id)
                .expect("read continuation edge directly")
                .as_deref(),
            Some(successor_thread_id)
        );
    }

    #[test]
    fn cancelled_planning_refuses_continuation_publication_with_typed_inactivity() {
        let store = test_store();
        let source_thread_id = "T-continuation-cancelled-source";
        let successor_thread_id = "T-continuation-cancelled-successor";
        let mut source = thread_record(source_thread_id, source_thread_id);
        source.requested_by = Some("fp:owner".to_string());
        store
            .create_thread_for_test(&source)
            .expect("create continuation source");
        let launch_id = store
            .reserve_launch_planning(successor_thread_id, "fp:owner")
            .expect("reserve continuation planning");
        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("cancel continuation planning"),
            Some(LaunchCancellationResolution::Cancelled)
        );

        let mut successor = thread_record(successor_thread_id, source_thread_id);
        successor.requested_by = Some("fp:owner".to_string());
        let error = store
            .create_continuation_for_test(
                &successor,
                source_thread_id,
                source_thread_id,
                Some("chained_resume"),
            )
            .expect_err("cancelled planning must fence continuation publication");
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<LaunchPlanningInactive>()),
            "continuation publication must retain typed planning inactivity: {error:#}"
        );
        assert!(
            store
                .get_thread(successor_thread_id)
                .expect("read absent continuation successor")
                .is_none()
        );
        let source = store
            .get_thread(source_thread_id)
            .expect("read unchanged continuation source")
            .expect("continuation source");
        assert_eq!(source.status, ThreadStatus::Created.as_str());
        assert!(source.successor_thread_id.is_none());
    }

    #[test]
    fn launch_cancel_and_authoritative_continuation_bind_have_one_atomic_winner() {
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let source_thread_id = "T-continuation-bind-race-source";
        let successor_thread_id = "T-continuation-bind-race-successor";
        let mut source = thread_record(source_thread_id, source_thread_id);
        source.requested_by = Some("fp:owner".to_string());
        store
            .create_thread_for_test(&source)
            .expect("create continuation source");
        let launch_id = store
            .reserve_launch_planning(successor_thread_id, "fp:owner")
            .expect("reserve continuation planning");
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let creator_store = store.clone();
        let creator_barrier = barrier.clone();
        let creator = std::thread::spawn(move || {
            creator_barrier.wait();
            let mut successor = thread_record(successor_thread_id, source_thread_id);
            successor.requested_by = Some("fp:owner".to_string());
            creator_store.create_continuation_for_test(
                &successor,
                source_thread_id,
                source_thread_id,
                Some("chained_resume"),
            )
        });

        let canceller_store = store.clone();
        let canceller_barrier = barrier.clone();
        let canceller = std::thread::spawn(move || {
            canceller_barrier.wait();
            canceller_store.cancel_launch_planning(&launch_id, "fp:owner")
        });

        barrier.wait();
        let created = creator.join().expect("creator thread");
        let cancelled = canceller
            .join()
            .expect("canceller thread")
            .expect("cancel resolution")
            .expect("owned planning record");
        let outcomes = (created, cancelled);
        match outcomes {
            (Ok(_), LaunchCancellationResolution::Bound { thread_id }) => {
                assert_eq!(thread_id, successor_thread_id);
                let source = store
                    .get_thread(source_thread_id)
                    .expect("read continued source")
                    .expect("continuation source");
                assert_eq!(source.status, ThreadStatus::Continued.as_str());
                assert_eq!(
                    source.successor_thread_id.as_deref(),
                    Some(successor_thread_id)
                );
                assert!(
                    store
                        .get_thread(successor_thread_id)
                        .expect("read published successor")
                        .is_some()
                );
            }
            (Err(error), LaunchCancellationResolution::Cancelled) => {
                assert!(
                    error
                        .chain()
                        .any(|cause| cause.is::<LaunchPlanningInactive>()),
                    "cancel-won continuation publication must retain typed planning inactivity: {error:#}"
                );
                let source = store
                    .get_thread(source_thread_id)
                    .expect("read unchanged source")
                    .expect("continuation source");
                assert_eq!(source.status, ThreadStatus::Created.as_str());
                assert!(source.successor_thread_id.is_none());
                assert!(
                    store
                        .get_thread(successor_thread_id)
                        .expect("read absent successor")
                        .is_none()
                );
            }
            (created, cancelled) => {
                panic!(
                    "invalid continuation cancel-vs-bind race outcome: \
                     created={created:?}, cancelled={cancelled:?}"
                )
            }
        }
    }

    #[test]
    fn direct_attachment_fork_proceeds_while_the_state_store_is_held() {
        // A held StateStore closes no fork window: the locks taken beneath
        // it are closed in every fork child, so a long hold must not stall
        // process creation on other threads.
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let guard = store.lock().expect("hold state store");
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let pending = lillux::spawn_awaiting_attachment(lillux::SubprocessRequest {
                cmd: "/bin/sh".to_string(),
                argv0: None,
                args: vec!["-c".to_string(), "exit 0".to_string()],
                cwd: None,
                envs: Vec::new(),
                stdin_data: None,
                timeout: 5.0,
                limits: None,
                inherited_fds: Vec::new(),
                inherited_fd_mappings: Vec::new(),
                supervised_status: None,
            })
            .expect("spawn while the state store is held");
            sender.send(pending).expect("publish pending process");
        });

        let pending = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("direct fork proceeds while the StateStore is held");
        drop(guard);
        pending.abort_and_reap().expect("abort held test target");
        worker.join().expect("spawn worker");
    }

    #[test]
    fn direct_projection_access_fails_closed_while_repair_is_pending() {
        let store = test_store();
        ryeos_state::ProjectionRepairSink::request_repair(
            &*store.projection_health(),
            ryeos_state::ProjectionRepairRequest {
                chain_root_id: "T-root".into(),
                committed_head_hash: "head".into(),
                operation: "append_events",
                error: "projection failed".into(),
            },
        );
        let error = store
            .with_projection(|_| Ok(()))
            .expect_err("stale projection read must fail");
        assert!(error.to_string().contains("not current"));
    }

    #[test]
    fn settlement_cost_boundary_rejects_invalid_values() {
        let valid = ryeos_engine::contracts::FinalCost {
            turns: 0,
            input_tokens: 1,
            output_tokens: 2,
            spend: ryeos_engine::launch_envelope_types::UsdNanos::parse_canonical("0.01").unwrap(),
            provider: None,
            basis: None,
            metadata: None,
        };
        assert!(validate_final_cost_for_settlement(&valid).is_ok());

        // Negative and non-finite spend are unrepresentable in `UsdNanos`;
        // the numeric-wire rejection is proven by the FinalCost contract test.
        for invalid in [
            ryeos_engine::contracts::FinalCost {
                input_tokens: i64::MAX as u64 + 1,
                ..valid.clone()
            },
            ryeos_engine::contracts::FinalCost {
                output_tokens: i64::MAX as u64 + 1,
                ..valid.clone()
            },
            ryeos_engine::contracts::FinalCost {
                basis: Some("estimated".to_string()),
                ..valid.clone()
            },
        ] {
            assert!(validate_final_cost_for_settlement(&invalid).is_err());
        }
    }

    fn follow_waiter_for_admission() -> runtime_db::FollowWaiter {
        runtime_db::FollowWaiter {
            follow_key: "follow-key".to_string(),
            parent_thread_id: "T-parent".to_string(),
            parent_chain_root_id: "T-parent".to_string(),
            parent_successor_thread_id: Some("T-successor".to_string()),
            follow_node: "fanout".to_string(),
            graph_run_id: "run-1".to_string(),
            step_count: 1,
            frontier_id: None,
            fanout: true,
            expected_children: 2,
            child_project_authority: None,
            children: vec![runtime_db::FollowWaiterChild {
                item_index: 0,
                item_ref: "tool:test/one".to_string(),
                spec_hash: "spec".to_string(),
                child_thread_id: "T-child".to_string(),
                child_chain_root_id: "T-child".to_string(),
                sealed_root_request:
                    crate::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture(),
                terminal_thread_id: None,
                terminal_status: None,
                terminal_envelope: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            }],
            phase: runtime_db::follow_phase::RESERVED.to_string(),
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn follow_terminal_admission_reserves_space_for_pending_children() {
        let waiter = follow_waiter_for_admission();
        let candidate = ryeos_runtime::envelope::encode_follow_terminal_envelope(
            "T-child",
            ryeos_runtime::envelope::RuntimeResultStatus::Completed,
            json!(1),
            None,
        )
        .unwrap();

        validate_prospective_follow_resume_payload(&waiter, "T-child", &candidate).unwrap();
    }

    #[test]
    fn oversized_follow_terminal_is_replaced_before_persistence() {
        let waiter = follow_waiter_for_admission();
        let candidate = json!({
            "success": true,
            "child_thread_id": "T-terminal",
            "status": "completed",
            "result": "x".repeat(checkpoint_shape_limits().max_result_bytes),
            "outputs": null,
            "warnings": [],
            "cost": {
                "input_tokens": 11,
                "output_tokens": 7,
                "total_usd": "0.03",
            },
        });

        let (admitted, degraded) =
            admit_follow_terminal_envelope(&waiter, "T-child", "T-terminal", &candidate).unwrap();
        assert!(degraded);
        assert_eq!(
            admitted["projection"],
            ryeos_runtime::envelope::FOLLOW_ACTION_RESULT_PROJECTION
        );
        assert_eq!(admitted["success"], false);
        assert_eq!(admitted["child_thread_id"], "T-terminal");
        assert_eq!(admitted["result"]["code"], FOLLOW_ENVELOPE_LIMIT_CODE);
        assert_eq!(admitted["cost"]["input_tokens"], 11);
        assert_eq!(admitted["cost"]["output_tokens"], 7);
    }

    #[test]
    fn graph_follow_cohort_retains_returns_without_private_child_state() {
        let mut waiter = follow_waiter_for_admission();
        waiter.expected_children = 12;
        waiter.children = (0..12)
            .map(|index| runtime_db::FollowWaiterChild {
                item_index: index,
                item_ref: "graph:farm/match".to_string(),
                spec_hash: format!("spec-{index}"),
                child_thread_id: format!("T-child-{index}"),
                child_chain_root_id: format!("T-child-{index}"),
                sealed_root_request:
                    crate::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture(),
                terminal_thread_id: None,
                terminal_status: None,
                terminal_envelope: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .collect();

        for index in 0..12 {
            let child_thread_id = format!("T-child-{index}");
            let candidate = json!({
                "success": true,
                "child_thread_id": child_thread_id,
                "status": "completed",
                "result": {
                    "success": true,
                    "graph_id": "farm/match",
                    "definition_ref": "graph:farm/match",
                    "effective_definition_digest": "a".repeat(64),
                    "graph_run_id": format!("gr-child-{index}"),
                    "status": "completed",
                    "steps": 31,
                    "state": {"private_child_state": "x".repeat(512 * 1024)},
                    "result": {"seed": index, "winner": "melon"},
                    "node_costs": [],
                    "hook_costs": [],
                },
                "outputs": null,
                "warnings": [],
                "cost": null,
            });
            validate_checkpoint_shape(&candidate, "large managed child terminal").unwrap();
            let (admitted, degraded) = admit_follow_terminal_envelope(
                &waiter,
                &child_thread_id,
                &child_thread_id,
                &candidate,
            )
            .unwrap();
            assert!(!degraded, "child {index} was incorrectly degraded");
            assert_eq!(
                admitted["result"],
                json!({"seed": index, "winner": "melon"})
            );
            assert!(serde_json::to_vec(&admitted).unwrap().len() < 1024);
            waiter.children[index as usize].terminal_envelope = Some(admitted);
        }

        for child in &waiter.children {
            validate_prospective_follow_resume_payload(
                &waiter,
                &child.child_chain_root_id,
                child.terminal_envelope.as_ref().unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn follow_terminal_admission_rejects_mismatched_child_identity() {
        let waiter = follow_waiter_for_admission();
        let candidate = json!({
            "success": true,
            "child_thread_id": "T-other",
            "status": "completed",
            "result": 1,
            "outputs": null,
            "warnings": [],
            "cost": null,
        });
        let error = admit_follow_terminal_envelope(&waiter, "T-child", "T-terminal", &candidate)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not match terminal child"), "{error}");
    }

    #[test]
    fn impossible_follow_cohort_is_rejected_before_reservation() {
        let seed = runtime_db::NewFollowWaiter {
            follow_key: "too-wide".to_string(),
            parent_thread_id: "T-parent".to_string(),
            parent_chain_root_id: "T-parent".to_string(),
            follow_node: "fanout".to_string(),
            graph_run_id: "run-1".to_string(),
            step_count: 1,
            frontier_id: None,
            fanout: true,
            expected_children: u32::MAX,
            child_project_authority: None,
        };

        let error = validate_follow_reservation_shape(&seed).unwrap_err();
        assert!(error.to_string().contains("maximum"));
    }

    fn thread_record(thread_id: &str, chain_root_id: &str) -> NewThreadRecord {
        let captured_history_policy = (thread_id == chain_root_id).then(|| {
            let hash = "a".repeat(64);
            ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
                canonical_item_ref: "directive:test".to_string(),
                item_content_hash: hash.clone(),
                item_signer_fingerprint: Some(hash.clone()),
                item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: hash,
                resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:
                        ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
                },
            }
        });
        NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: chain_root_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test".to_string(),
            executor_ref: "native:test".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("fp:test".to_string()),
            project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy,
        }
    }

    fn launch_attempt_audit_events() -> Vec<NewEventRecord> {
        LAUNCH_ATTEMPT_AUDIT_TYPES
            .into_iter()
            .enumerate()
            .map(|(index, event_type)| NewEventRecord {
                event_type: event_type.as_str().to_string(),
                storage_class: event_type.storage_class().as_str().to_string(),
                payload: json!({"fixture": index}),
            })
            .collect()
    }

    fn replayed_event_types(store: &StateStore, thread_id: &str) -> Vec<String> {
        store
            .replay_events(thread_id, Some(thread_id), None, 32, 1024 * 1024)
            .expect("replay thread events")
            .events
            .into_iter()
            .map(|event| event.event_type)
            .collect()
    }

    #[test]
    fn terminal_result_larger_than_event_is_retained_once_and_event_is_bounded() {
        let store = test_store();
        let thread_id = "T-large-terminal-result";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create thread");
        let result = json!({
            "state": "x".repeat(ryeos_state::objects::MAX_THREAD_EVENT_SERIALIZED_BYTES)
        });
        let managed_envelope = crate::thread_lifecycle::managed_runtime_envelope(
            thread_id,
            ThreadStatus::Completed.as_str(),
            Some(&result),
            None,
            None,
            &json!({"answer": 42}),
            &["retained warning".to_string()],
        );

        store
            .finalize_thread(
                thread_id,
                &FinalizeThreadRecord {
                    status: ThreadStatus::Completed.as_str().to_string(),
                    outcome_code: Some("success".to_string()),
                    result_json: Some(result.clone()),
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: Some(managed_envelope),
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .expect("large terminal result must not inflate the lifecycle event");

        let projected = store
            .get_thread_result(thread_id)
            .expect("read projected result")
            .expect("projected result exists");
        assert_eq!(projected.result.as_ref(), Some(&result));

        let events = store
            .replay_events(thread_id, Some(thread_id), None, 8, 1024 * 1024)
            .expect("replay terminal events")
            .events;
        let terminal = events
            .iter()
            .find(|event| event.event_type == ryeos_state::event_types::THREAD_COMPLETED)
            .expect("thread_completed event");
        assert!(terminal.payload.get("result").is_none());
        assert_eq!(terminal.payload["result_present"], true);
        assert!(terminal.payload["result_size_bytes"].as_u64().unwrap() > 512 * 1024);
        assert!(
            serde_json::to_vec(&terminal.payload).unwrap().len() < 1024,
            "terminal lifecycle payload must remain a summary"
        );

        let authority = store
            .get_thread_terminal_authority(thread_id)
            .expect("read terminal authority")
            .expect("terminal authority exists");
        let envelope = authority
            .managed_envelope
            .expect("managed envelope is reconstructed from its supplement");
        assert_eq!(envelope["result"], result);
        assert_eq!(envelope["outputs"], json!({"answer": 42}));
        assert_eq!(envelope["warnings"], json!(["retained warning"]));

        let inner = store.lock().expect("lock state store");
        let snapshot = authoritative_snapshot_for_transition(&inner, thread_id, thread_id)
            .expect("authoritative terminal snapshot");
        let supplement = snapshot
            .managed_runtime_terminal
            .expect("authoritative managed runtime supplement");
        assert_eq!(supplement.outputs, json!({"answer": 42}));
        assert_eq!(supplement.warnings, vec!["retained warning".to_string()]);
    }

    #[test]
    fn large_terminal_error_is_retained_but_not_duplicated_into_the_event() {
        let store = test_store();
        let thread_id = "T-large-terminal-error";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create thread");
        let error = json!({"error": "x".repeat(MAX_TERMINAL_EVENT_INLINE_ERROR_BYTES)});

        store
            .finalize_thread(
                thread_id,
                &FinalizeThreadRecord {
                    status: ThreadStatus::Failed.as_str().to_string(),
                    outcome_code: Some("large_failure".to_string()),
                    result_json: None,
                    error_json: Some(error.clone()),
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .expect("large terminal error remains durable outside the event");

        let projected = store
            .get_thread_result(thread_id)
            .expect("read projected error")
            .expect("projected error exists");
        assert_eq!(projected.error.as_ref(), Some(&error));

        let events = store
            .replay_events(thread_id, Some(thread_id), None, 8, 1024 * 1024)
            .expect("replay terminal events")
            .events;
        let terminal = events
            .iter()
            .find(|event| event.event_type == ryeos_state::event_types::THREAD_FAILED)
            .expect("thread_failed event");
        assert!(terminal.payload.get("error").is_none());
        assert_eq!(terminal.payload["error_omitted"], true);
        assert!(terminal.payload["error_digest"].as_str().is_some());
        assert!(serde_json::to_vec(&terminal.payload).unwrap().len() < 1024);
    }

    #[test]
    fn oversized_terminal_content_refuses_before_mutating_the_chain() {
        let store = test_store();
        let thread_id = "T-oversized-terminal-result";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create thread");
        let result = json!("x".repeat(ryeos_state::objects::MAX_THREAD_RESULT_CONTENT_BYTES));

        let error = store
            .finalize_thread(
                thread_id,
                &FinalizeThreadRecord {
                    status: ThreadStatus::Completed.as_str().to_string(),
                    outcome_code: Some("success".to_string()),
                    result_json: Some(result),
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .expect_err("oversized terminal content must fail closed");
        assert!(error.to_string().contains("thread result"));

        let thread = store
            .get_thread(thread_id)
            .expect("read unchanged thread")
            .expect("thread remains present");
        assert_eq!(thread.status, ThreadStatus::Created.as_str());
        assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);
        assert!(
            store
                .get_thread_result(thread_id)
                .expect("read absent result")
                .is_none()
        );
    }

    fn running_in_process_test_root(store: &StateStore, thread_id: &str) {
        let metadata = in_process_launch_metadata();
        let owner = store
            .register_in_process_handler(thread_id)
            .expect("register hook evidence owner");
        store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(thread_id, thread_id),
                vec![NewEventRecord {
                    event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                    storage_class: "indexed".to_string(),
                    payload: json!({}),
                }],
                &metadata,
                &owner,
            )
            .expect("create running hook evidence root");
    }

    #[test]
    fn project_observation_exact_retry_returns_one_durable_event_and_divergence_refuses() {
        let store = test_store();
        let thread_id = "T-project-observation";
        running_in_process_test_root(&store, thread_id);
        let request = ryeos_runtime::ProjectObservationPublishParams {
            thread_id: thread_id.to_string(),
            graph_run_id: "G-campaign".to_string(),
            node: "classify".to_string(),
            step: 7,
            observation: ryeos_state::ProjectObservationRequest {
                namespace: "example.classification".to_string(),
                stable_id: "classification:game-1".to_string(),
                payload: json!({"status": "accepted", "score": 1}),
            },
        };
        let source_ref = "graph:example/campaign";
        let source_digest = "d".repeat(64);

        let first = store
            .publish_project_observation(&request, source_ref, &source_digest)
            .expect("publish project observation");
        assert!(first.inserted);
        let replay = store
            .publish_project_observation(&request, source_ref, &source_digest)
            .expect("fold exact project observation retry");
        assert!(!replay.inserted);
        assert_eq!(replay.event.event_hash, first.event.event_hash);
        assert_eq!(replay.event.chain_seq, first.event.chain_seq);

        let events = store
            .replay_events(thread_id, Some(thread_id), None, 16, 1024 * 1024)
            .expect("replay project observation evidence")
            .events
            .into_iter()
            .filter(|event| {
                event.event_type == ryeos_state::event_types::PROJECT_OBSERVATION_RECORDED
            })
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1, "exact retry must not append a duplicate");

        let mut changed_payload = request.clone();
        changed_payload.observation.payload = json!({"status": "rejected", "score": 0});
        assert!(
            store
                .publish_project_observation(&changed_payload, source_ref, &source_digest)
                .unwrap_err()
                .to_string()
                .contains("contradicts its durable identity")
        );

        let mut changed_occurrence = request;
        changed_occurrence.step = 8;
        assert!(
            store
                .publish_project_observation(&changed_occurrence, source_ref, &source_digest)
                .unwrap_err()
                .to_string()
                .contains("divergent durable event content")
        );
    }

    #[test]
    fn provider_call_observation_exact_retry_folds_and_replay_stays_distinct() {
        let store = test_store();
        let thread_id = "T-provider-observation";
        running_in_process_test_root(&store, thread_id);
        let draft = ryeos_state::ProviderCallObservationDraft {
            turn: 2,
            attempt_number: 1,
            effect_coordinate_digest: "a".repeat(64),
            source: ryeos_state::ProviderCallObservationSource::Executed,
            answer_digest: "b".repeat(64),
            record_hash: "c".repeat(64),
            publication: ryeos_state::ProviderCallObservationPublication::Inserted,
            replayed_from: None,
        };

        let first = store
            .publish_provider_call_observation(thread_id, &draft)
            .expect("publish provider-call observation");
        assert!(first.inserted);
        let replay = store
            .publish_provider_call_observation(thread_id, &draft)
            .expect("fold provider-call observation retry");
        assert!(!replay.inserted);
        assert_eq!(replay.event.event_hash, first.event.event_hash);

        let events = store
            .replay_events(thread_id, Some(thread_id), None, 16, 1024 * 1024)
            .expect("replay provider-call observation")
            .events
            .into_iter()
            .filter(|event| {
                event.event_type == ryeos_state::event_types::PROVIDER_CALL_OBSERVATION_RECORDED
            })
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);

        let replay = ryeos_state::ProviderCallObservationDraft {
            source: ryeos_state::ProviderCallObservationSource::Replay,
            publication: ryeos_state::ProviderCallObservationPublication::NotApplicable,
            replayed_from: Some(ryeos_state::ProviderCallReplaySource {
                produced_by_thread: thread_id.to_string(),
                attempt_id: "attempt-origin".to_string(),
            }),
            ..draft
        };
        let replay = store
            .publish_provider_call_observation(thread_id, &replay)
            .expect("publish same-coordinate replay observation");
        assert!(replay.inserted);
        assert_ne!(replay.event.event_hash, first.event.event_hash);

        let events = store
            .replay_events(thread_id, Some(thread_id), None, 16, 1024 * 1024)
            .expect("replay provider-call observations")
            .events
            .into_iter()
            .filter(|event| {
                event.event_type == ryeos_state::event_types::PROVIDER_CALL_OBSERVATION_RECORDED
            })
            .count();
        assert_eq!(events, 2);
    }

    fn hook_observation_identity(hook_id: &str) -> ryeos_runtime::callback::HookDispatchIdentity {
        ryeos_runtime::callback::HookDispatchIdentity {
            occurrence: ryeos_runtime::callback::HookDispatchOccurrence::new(
                "graph",
                "graph_step_completed",
                "graph:test/hook-evidence",
                "r".repeat(64),
                "d".repeat(64),
            )
            .with_text_coordinate("graph_run_id", "G-hook-evidence")
            .with_counter_coordinate("step", 3)
            .with_text_coordinate("node", "observe"),
            hook_id: hook_id.to_string(),
            layer: ryeos_runtime::hooks_loader::HookLayer::Infrastructure,
            result_mode: ryeos_runtime::hooks_loader::HookResultMode::Observation,
            context_contract: ryeos_engine::hooks::HookContextContract {
                schema: ryeos_engine::hooks::HOOK_CONTEXT_SCHEMA.to_string(),
                allowed_roots: std::collections::BTreeSet::from(["state".to_string()]),
            },
            context_hash: "c".repeat(64),
        }
    }

    fn hook_test_dispatch_evidence() -> ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
        ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
            source: ryeos_runtime::callback_contract::RuntimeDispatchSource::Executed,
            effect_class: ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Live,
            action_digest: "7".repeat(64),
            effect_identity: None,
            publication:
                ryeos_runtime::callback_contract::RuntimeDispatchPublication::NotApplicable,
            record_hash: None,
            replayed_from: None,
            result_projection: ryeos_effect_contract::DispatchResultProjection::DispatchedSubject,
        }
    }

    #[test]
    fn completed_observation_hook_appends_durable_daemon_evidence_on_replay() {
        let store = test_store();
        let thread_id = "T-hook-evidence";
        running_in_process_test_root(&store, thread_id);
        let identity = hook_observation_identity("hook:system/evidence");
        let seed = runtime_db::NewHookDispatch {
            seed_version: runtime_db::HOOK_DISPATCH_SEED_VERSION,
            dispatch_key: "a".repeat(64),
            chain_root_id: thread_id.to_string(),
            caller_thread_id: thread_id.to_string(),
            event: identity.occurrence.event().to_string(),
            hook_id: identity.hook_id.clone(),
            request_hash: "b".repeat(64),
        };
        assert!(matches!(
            store
                .reserve_hook_dispatch(&seed)
                .expect("reserve evidence hook"),
            runtime_db::HookDispatchReservation::Execute
        ));
        let response = json!({
            "thread": {"id": "T-hook-child", "status": "completed"},
            "result": {
                "kind": "build.step_completed",
                "payload": {"step": 3, "ok": true}
            },
            "dispatch": hook_test_dispatch_evidence()
        });
        store
            .complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &response)
            .expect("complete evidence hook");
        let first = store
            .append_completed_hook_outcome(
                thread_id,
                &identity,
                &seed.dispatch_key,
                &seed.request_hash,
            )
            .expect("append completed observation");
        assert_eq!(
            first.event_type,
            ryeos_state::event_types::HOOK_OBSERVATION_RECORDED
        );

        let runtime_db::HookDispatchReservation::Replay(replayed) = store
            .reserve_hook_dispatch(&seed)
            .expect("replay completed evidence hook")
        else {
            panic!("completed hook must replay");
        };
        let duplicate = store
            .append_completed_hook_outcome(
                thread_id,
                &identity,
                &replayed.dispatch_key,
                &replayed.request_hash,
            )
            .expect("append replayed observation");

        let events = store
            .replay_events(thread_id, Some(thread_id), None, 16, 1024 * 1024)
            .expect("replay durable hook evidence")
            .events
            .into_iter()
            .filter(|event| event.event_type == ryeos_state::event_types::HOOK_OBSERVATION_RECORDED)
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2, "retry appends a foldable duplicate");
        let first_payload: ryeos_runtime::HookObservationRecordedPayload =
            serde_json::from_value(events[0].payload.clone()).expect("decode first observation");
        let duplicate_payload: ryeos_runtime::HookObservationRecordedPayload =
            serde_json::from_value(events[1].payload.clone())
                .expect("decode duplicate observation");
        assert_eq!(first_payload, duplicate_payload);
        assert_eq!(
            first_payload.observation_id,
            format!("hook-observation:{}", seed.dispatch_key)
        );
        assert_eq!(first_payload.occurrence_thread_id, thread_id);
        assert_eq!(first.payload, duplicate.payload);
    }

    #[test]
    fn known_post_reservation_dispatch_failure_completes_and_replays_durable_integrity_evidence() {
        let store = test_store();
        let thread_id = "T-hook-known-failure";
        running_in_process_test_root(&store, thread_id);
        let identity = hook_observation_identity("hook:system/known-failure");
        let seed = runtime_db::NewHookDispatch {
            seed_version: runtime_db::HOOK_DISPATCH_SEED_VERSION,
            dispatch_key: "9".repeat(64),
            chain_root_id: thread_id.to_string(),
            caller_thread_id: thread_id.to_string(),
            event: identity.occurrence.event().to_string(),
            hook_id: identity.hook_id.clone(),
            request_hash: "8".repeat(64),
        };
        assert!(matches!(
            store.reserve_hook_dispatch(&seed).unwrap(),
            runtime_db::HookDispatchReservation::Execute
        ));
        let response =
            serde_json::to_value(ryeos_runtime::callback_contract::CallbackDispatchResponse {
                thread: Value::Null,
                result: ryeos_runtime::envelope::hook_dispatch_integrity_failure(
                    "child dispatcher refused the admitted request",
                ),
                dispatch: ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
                    source: ryeos_runtime::callback_contract::RuntimeDispatchSource::Executed,
                    effect_class:
                        ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Live,
                    action_digest: "7".repeat(64),
                    effect_identity: None,
                    publication:
                        ryeos_runtime::callback_contract::RuntimeDispatchPublication::NotApplicable,
                    record_hash: None,
                    replayed_from: None,
                    result_projection:
                        ryeos_effect_contract::DispatchResultProjection::DispatchedSubject,
                },
            })
            .unwrap();
        store
            .complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &response)
            .unwrap();
        let persisted = store
            .append_completed_hook_outcome(
                thread_id,
                &identity,
                &seed.dispatch_key,
                &seed.request_hash,
            )
            .unwrap();
        assert_eq!(persisted.event_type, ryeos_state::event_types::HOOK_FAILED);
        let failure: ryeos_runtime::HookFailedPayload =
            serde_json::from_value(persisted.payload).unwrap();
        assert_eq!(
            failure.failure_class,
            ryeos_runtime::HookFailureClass::DispatchEnvelopeIntegrity
        );

        let runtime_db::HookDispatchReservation::Replay(replayed) =
            store.reserve_hook_dispatch(&seed).unwrap()
        else {
            panic!("known failure must complete rather than remain pending");
        };
        assert_eq!(replayed.response, response);
    }

    #[test]
    fn terminal_occurrence_evidence_appends_to_the_live_successor() {
        let store = test_store();
        let predecessor = "T-hook-predecessor";
        let successor = "T-hook-successor";
        store
            .create_thread_for_test(&thread_record(predecessor, predecessor))
            .expect("create predecessor");
        store
            .mark_thread_running(predecessor, None)
            .expect("start predecessor");
        let mut successor_record = thread_record(successor, predecessor);
        successor_record.upstream_thread_id = Some(predecessor.to_string());
        store
            .create_thread_for_test(&successor_record)
            .expect("create successor");
        store
            .mark_thread_running(successor, None)
            .expect("start successor");
        store
            .finalize_thread(
                predecessor,
                &FinalizeThreadRecord {
                    status: ThreadStatus::Continued.as_str().to_string(),
                    outcome_code: Some(ThreadStatus::Continued.as_str().to_string()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .expect("settle predecessor as continued");

        let identity = ryeos_runtime::callback::HookDispatchIdentity {
            occurrence: ryeos_runtime::callback::HookDispatchOccurrence::new(
                "graph",
                "graph_completed",
                "graph:test/terminal-evidence",
                "r".repeat(64),
                "d".repeat(64),
            )
            .with_text_coordinate("graph_run_id", "G-terminal-evidence")
            .with_counter_coordinate("steps", 4),
            hook_id: "hook:system/terminal-evidence".to_string(),
            layer: ryeos_runtime::HookLayer::Infrastructure,
            result_mode: ryeos_runtime::HookResultMode::Observation,
            context_contract: ryeos_engine::hooks::HookContextContract {
                schema: ryeos_engine::hooks::HOOK_CONTEXT_SCHEMA.to_string(),
                allowed_roots: std::collections::BTreeSet::from(["state".to_string()]),
            },
            context_hash: "c".repeat(64),
        };
        let seed = runtime_db::NewHookDispatch {
            seed_version: runtime_db::HOOK_DISPATCH_SEED_VERSION,
            dispatch_key: "e".repeat(64),
            chain_root_id: predecessor.to_string(),
            caller_thread_id: predecessor.to_string(),
            event: identity.occurrence.event().to_string(),
            hook_id: identity.hook_id.clone(),
            request_hash: "f".repeat(64),
        };
        assert!(matches!(
            store.reserve_hook_dispatch(&seed).unwrap(),
            runtime_db::HookDispatchReservation::Execute
        ));
        store
            .complete_hook_dispatch(
                &seed.dispatch_key,
                &seed.request_hash,
                &json!({
                    "thread": {"id": "T-hook-child", "status": "completed"},
                    "result": {"kind": "solve.completed", "payload": {"ok": true}},
                    "dispatch": hook_test_dispatch_evidence()
                }),
            )
            .expect("complete terminal observation");

        let persisted = store
            .append_completed_hook_outcome(
                successor,
                &identity,
                &seed.dispatch_key,
                &seed.request_hash,
            )
            .expect("append terminal evidence to successor");
        assert_eq!(persisted.thread_id, successor);
        let payload: ryeos_runtime::HookObservationRecordedPayload =
            serde_json::from_value(persisted.payload).expect("decode terminal evidence");
        assert_eq!(payload.occurrence_thread_id, predecessor);
        assert!(
            store
                .append_completed_hook_outcome(
                    predecessor,
                    &identity,
                    &seed.dispatch_key,
                    &seed.request_hash,
                )
                .unwrap_err()
                .to_string()
                .contains("requires a running caller")
        );
    }

    fn in_process_launch_metadata() -> crate::launch_metadata::RuntimeLaunchMetadata {
        crate::launch_metadata::RuntimeLaunchMetadata::default()
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
            .with_in_process_lifecycle_authority(
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE,
            )
    }

    #[test]
    fn in_process_root_birth_atomically_publishes_created_genesis_and_running_head() {
        let store = test_store();
        let thread_id = "T-in-process-root-birth";
        let metadata = in_process_launch_metadata();
        let owner = store
            .register_in_process_handler(thread_id)
            .expect("register in-process owner");

        let persisted = store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(thread_id, thread_id),
                vec![NewEventRecord {
                    event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                    storage_class: "indexed".to_string(),
                    payload: json!({}),
                }],
                &metadata,
                &owner,
            )
            .expect("atomically publish in-process root birth");

        assert_eq!(
            persisted
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            [
                ryeos_state::event_types::THREAD_CREATED,
                ryeos_state::event_types::THREAD_STARTED,
            ]
        );
        let projected = store
            .get_thread(thread_id)
            .expect("read projected in-process root")
            .expect("projected in-process root");
        assert_eq!(projected.status, ThreadStatus::Running.as_str());
        assert!(projected.started_at.is_some());
        let projected_metadata = projected
            .runtime
            .launch_metadata
            .expect("in-process launch metadata");
        assert_eq!(
            projected_metadata.launch_driver,
            Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        );
        assert_eq!(
            projected_metadata.in_process_lifecycle_authority,
            Some(ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE)
        );
        assert_eq!(
            replayed_event_types(&store, thread_id),
            ["thread_created", "thread_started"]
        );
        assert_eq!(
            store
                .in_process_handler_reservations_after(
                    None,
                    runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
                )
                .expect("list live reservations"),
            [runtime_db::InProcessHandlerReservation {
                thread_id: thread_id.to_string(),
                phase: runtime_db::InProcessHandlerReservationPhase::Running,
            }]
        );

        let inner = store.lock().expect("lock state store");
        let head_ref = inner
            .state_db
            .read_generic_head_ref("chains", thread_id)
            .expect("read signed chain head")
            .expect("signed chain head");
        assert!(
            !head_ref.target_hash.is_empty(),
            "atomic birth must publish a signed CAS head"
        );
        let cas = store.state_authority.cas_store().expect("open pinned CAS");
        let head_state: ryeos_state::ChainState = serde_json::from_value(
            cas.get_object(&head_ref.target_hash)
                .expect("load current ChainState")
                .expect("current ChainState object"),
        )
        .expect("decode current ChainState");
        head_state.validate().expect("validate current ChainState");
        assert_eq!(head_state.last_chain_seq, 2);
        let running_entry = head_state
            .threads
            .get(thread_id)
            .expect("running ChainState entry");
        assert_eq!(running_entry.status, ThreadStatus::Running);
        assert_eq!(running_entry.last_thread_seq, 2);
        let started_event_hash = persisted[1]
            .event_hash
            .as_deref()
            .expect("durable thread_started event hash");
        assert_eq!(
            head_state.last_event_hash.as_deref(),
            Some(started_event_hash)
        );
        assert_eq!(
            running_entry.last_event_hash.as_deref(),
            Some(started_event_hash)
        );

        let authoritative = authoritative_snapshot_for_transition(&inner, thread_id, thread_id)
            .expect("read authoritative running snapshot");
        assert_eq!(authoritative.status, ThreadStatus::Running);
        assert!(authoritative.started_at.is_some());
        assert_eq!(authoritative.last_chain_seq, 2);
        assert_eq!(authoritative.last_thread_seq, 2);
        assert_eq!(
            authoritative.last_event_hash.as_deref(),
            Some(started_event_hash)
        );

        let genesis_hash = head_state
            .prev_chain_state_hash
            .as_deref()
            .expect("event successor must retain the genesis ChainState");
        let genesis_state: ryeos_state::ChainState = serde_json::from_value(
            cas.get_object(genesis_hash)
                .expect("load genesis ChainState")
                .expect("genesis ChainState object"),
        )
        .expect("decode genesis ChainState");
        genesis_state
            .validate()
            .expect("validate genesis ChainState");
        assert_eq!(genesis_state.last_chain_seq, 0);
        assert!(genesis_state.last_event_hash.is_none());
        assert!(genesis_state.prev_chain_state_hash.is_none());
        let created_entry = genesis_state
            .threads
            .get(thread_id)
            .expect("created genesis entry");
        assert_eq!(created_entry.status, ThreadStatus::Created);
        assert_eq!(created_entry.last_thread_seq, 0);
        assert!(created_entry.last_event_hash.is_none());

        let genesis_snapshot = ThreadSnapshot::from_current_value(
            cas.get_object(&created_entry.snapshot_hash)
                .expect("load genesis snapshot")
                .expect("genesis snapshot object"),
        )
        .expect("decode current genesis snapshot");
        assert_eq!(genesis_snapshot.status, ThreadStatus::Created);
        assert!(genesis_snapshot.started_at.is_none());
        assert_eq!(genesis_snapshot.last_chain_seq, 0);
        assert_eq!(genesis_snapshot.last_thread_seq, 0);
        assert!(genesis_snapshot.last_event_hash.is_none());
    }

    #[test]
    fn shutdown_gate_rejects_new_in_process_owner_registration() {
        let store = test_store();
        store
            .close_process_attachment_admission()
            .expect("close shutdown admission");
        let error = store
            .register_in_process_handler("T-after-shutdown-gate")
            .err()
            .expect("shutdown gate must reject a late in-process owner");
        assert!(error.to_string().contains("closed for daemon shutdown"));
    }

    fn process_resource_reservation_fixture(
        coordinate: &str,
    ) -> (
        runtime_db::ProcessResourceReservationRecord,
        lillux::ProcessScopeRecovery,
    ) {
        let host =
            serde_json::to_value(lillux::ProcessHostLifetime::capture_current().unwrap()).unwrap();
        let planned = serde_json::json!({
            "version": 3,
            "control_timeout": {"secs": 1, "nanos": 0},
            "configuration": {"version": 3, "backend": {
                "implementation": "linux_cgroup_v2", "parent": "/fixture/delegation"
            }},
            "backend": {"implementation": "linux_cgroup_v2",
                "boot_id": host["backend"]["boot_id"],
                "cgroup_enclosure": host["backend"]["cgroup_enclosure"],
                "parent": {"filesystem": {"device": 1}, "inode": 2},
                "name": format!("resource-{coordinate}")}
        });
        let allocation: lillux::ProcessScopeAllocation =
            serde_json::from_value(planned.clone()).unwrap();
        let mut bound = planned;
        bound["version"] = 5.into();
        bound["backend"]["directory"] =
            serde_json::json!({"filesystem": {"device": 1}, "inode": 3});
        let recovery: lillux::ProcessScopeRecovery = serde_json::from_value(bound).unwrap();
        (
            runtime_db::ProcessResourceReservationRecord {
                owner_kind: "thread".to_owned(),
                owner_coordinate: coordinate.to_owned(),
                daemon_generation_id: runtime_db::daemon_generation_id().to_owned(),
                selections: vec![ryeos_engine::contracts::ExecutionResourceSelection {
                    stable_id: format!("accelerator-{coordinate}"),
                    class: "accelerator".to_owned(),
                    matched_facts: BTreeMap::new(),
                    observation_contract_digest: "1".repeat(64),
                    device_binding_digest: "2".repeat(64),
                    access: ryeos_engine::contracts::ExecutionResourceAccess::DeploymentVisible,
                    enforcement:
                        ryeos_engine::contracts::ExecutionResourceEnforcement::DeploymentVisible,
                    character_devices: Vec::new(),
                }],
                allocation_limit: 1,
                runtime_custody: None,
                launch_authority:
                    crate::runtime_db::ProcessResourceLaunchAuthority::LocalProcessScope {
                        allocation,
                        recovery: None,
                    },
            },
            recovery,
        )
    }

    /// Storage-only trusted coordinates; no admitted capsule, process or GPU.
    #[cfg(target_os = "linux")]
    fn scratch_reservation_fixture(
        store: &StateStore,
        coordinate: &str,
    ) -> (
        Arc<crate::temp_dir_guard::TempDirGuard>,
        runtime_db::ProcessResourceReservationRecord,
    ) {
        use runtime_db::process_resource_custody::*;
        let runtime = store.state_authority.runtime_directory();
        let (_, original) = crate::temp_dir_guard::create_projectless_workspace(
            &runtime.path().join("cache"),
            coordinate,
        )
        .unwrap();
        std::fs::write(
            original.path().unwrap().join("sentinel"),
            b"retained original scratch",
        )
        .unwrap();
        let (parent, name) = original.owned_scratch_parent().unwrap();
        let (mut reservation, _) = process_resource_reservation_fixture(coordinate);
        reservation.owner_kind = "pooled_session".to_owned();
        reservation.launch_authority =
            runtime_db::ProcessResourceLaunchAuthority::TrustedProcessGroup {
                cleanup_contract_digest: "7".repeat(64),
                host_lifetime: lillux::ProcessHostLifetime::capture_current().unwrap(),
                phase: runtime_db::TrustedResourceLaunchPhase::Reserved,
            };
        reservation.runtime_custody = Some(ProcessResourceRuntimeCustody {
            version: ProcessResourceRuntimeCustody::VERSION,
            session_capsule_hash: "1".repeat(64),
            execution_realization_hash: "2".repeat(64),
            source_binding_hash: Some("3".repeat(64)),
            materializations: vec![ProcessMaterializationCustody {
                cache: ProcessMaterializationCache::ExternalContent,
                manifest_hash: "4".repeat(64),
            }],
            workspace: ProcessWorkspaceCustody::ResourceOwnedScratch {
                runtime_directory_identity: runtime.identity().unwrap(),
                parent_directory_identity: parent.identity().unwrap(),
                scratch_name: name.to_str().unwrap().to_owned(),
                workspace_identity: original.owned_scratch_root().unwrap().identity().unwrap(),
            },
            physical_state: ProcessCustodyPhysicalState::Retained,
        });
        (original, reservation)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scratch_preservation_precedes_reservation_commit_and_survives_lost_ack() {
        let store = test_store();
        let (original, reservation) = scratch_reservation_fixture(&store, "scratch-precommit");
        let path = original.path().unwrap();
        assert!(store.reserve_process_resource_launch(&reservation).is_err());
        store.close_process_attachment_admission().unwrap();
        assert!(
            store
                .reserve_process_resource_launch_with_scratch(&reservation, &original)
                .is_err()
        );
        assert!(store.process_resource_reservations().unwrap().is_empty());
        drop(original);
        assert!(
            path.join("sentinel").exists(),
            "first possible commit must never be followed by automatic rollback"
        );

        // One Store is live per process: close the first before the second.
        store.close().unwrap();
        let store = test_store();
        let (original, reservation) =
            scratch_reservation_fixture(&store, "scratch-lost-reserve-ack");
        let path = original.path().unwrap();
        // Discard the successful acknowledgement and original process-local
        // guard. The authoritative stored birth and bytes must remain intact.
        store
            .reserve_process_resource_launch_with_scratch(&reservation, &original)
            .unwrap();
        assert!(
            store
                .reserve_process_resource_launch_with_scratch(&reservation, &original)
                .is_err()
        );
        drop(original);
        assert!(path.join("sentinel").exists());
        assert_eq!(
            store
                .process_resource_reservation(
                    &reservation.owner_kind,
                    &reservation.owner_coordinate
                )
                .unwrap(),
            Some(reservation.clone())
        );
        assert!(
            store
                .clear_process_resource_reservation(&reservation)
                .is_err()
        );
        assert_eq!(
            store
                .lock()
                .unwrap()
                .runtime_db
                .process_resource_cas_roots()
                .unwrap(),
            reservation
                .runtime_custody
                .as_ref()
                .unwrap()
                .cas_roots()
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scratch_physical_receipt_retains_roots_and_refuses_replacement_after_removal() {
        use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
        let store = test_store();
        let (original, reservation) =
            scratch_reservation_fixture(&store, "scratch-physical-receipt");
        let path = original.path().unwrap();
        store
            .reserve_process_resource_launch_with_scratch(&reservation, &original)
            .unwrap();
        let roots = store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_cas_roots()
            .unwrap();
        let pins = store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_materialization_pins()
            .unwrap();
        let removed = store
            .remove_never_issued_resource_scratch(&reservation, &original)
            .unwrap();
        assert!(!path.exists());
        assert!(original.path().is_none());
        assert_eq!(
            removed.runtime_custody.as_ref().unwrap().physical_state,
            ProcessCustodyPhysicalState::PhysicallyRemoved
        );
        assert_eq!(
            store
                .lock()
                .unwrap()
                .runtime_db
                .process_resource_cas_roots()
                .unwrap(),
            roots
        );
        assert_eq!(
            store
                .lock()
                .unwrap()
                .runtime_db
                .process_resource_materialization_pins()
                .unwrap(),
            pins
        );
        assert!(
            store.clear_process_resource_reservation(&removed).is_err(),
            "physical removal cannot release resource/financial custody"
        );
        assert_eq!(
            store
                .recover_never_issued_resource_scratch(&removed)
                .unwrap(),
            removed
        );
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("replacement"), b"must remain").unwrap();
        assert!(
            store
                .recover_never_issued_resource_scratch(&removed)
                .is_err()
        );
        assert_eq!(
            std::fs::read(path.join("replacement")).unwrap(),
            b"must remain"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scratch_disarmed_guard_with_live_root_cannot_mint_physical_receipt() {
        use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
        let store = test_store();
        let (original, reservation) = scratch_reservation_fixture(&store, "scratch-disarmed");
        let path = original.path().unwrap();
        store
            .reserve_process_resource_launch_with_scratch(&reservation, &original)
            .unwrap();
        original.disarm().unwrap();
        original.remove_now().unwrap(); // Existing Ok no-op is NOT proof.
        assert!(path.join("sentinel").exists());
        assert!(
            store
                .remove_never_issued_resource_scratch(&reservation, &original)
                .is_err()
        );
        assert_eq!(
            store.process_resource_reservations().unwrap()[0]
                .runtime_custody
                .as_ref()
                .unwrap()
                .physical_state,
            ProcessCustodyPhysicalState::Retained
        );
        // One Store is live per process: close the first before the second.
        drop(original);
        store.close().unwrap();

        // The SAME actual-scratch owner also refuses rollback once the real
        // one-shot stored spawn-intent transition has committed. No subprocess
        // is launched; this is the issued storage boundary, not GPU evidence.
        let store = test_store();
        let (original, reserved) =
            scratch_reservation_fixture(&store, "scratch-issued-no-rollback");
        let path = original.path().unwrap();
        store
            .reserve_process_resource_launch_with_scratch(&reserved, &original)
            .unwrap();
        let intent = store
            .begin_trusted_process_resource_spawn(&reserved)
            .unwrap()
            .into_reservation();
        let roots = store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_cas_roots()
            .unwrap();
        let pins = store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_materialization_pins()
            .unwrap();
        assert!(
            store
                .begin_trusted_process_resource_spawn(&reserved)
                .is_err()
        );
        assert!(
            store
                .remove_never_issued_resource_scratch(&intent, &original)
                .is_err()
        );
        assert!(
            store
                .recover_never_issued_resource_scratch(&intent)
                .is_err()
        );
        assert_eq!(
            std::fs::read(path.join("sentinel")).unwrap(),
            b"retained original scratch"
        );
        assert_eq!(original.path(), Some(path));
        assert_eq!(store.process_resource_reservations().unwrap(), vec![intent]);
        assert_eq!(
            store
                .lock()
                .unwrap()
                .runtime_db
                .process_resource_cas_roots()
                .unwrap(),
            roots
        );
        assert_eq!(
            store
                .lock()
                .unwrap()
                .runtime_db
                .process_resource_materialization_pins()
                .unwrap(),
            pins
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scratch_replaced_parent_or_root_keeps_original_custody_and_replacement_bytes() {
        use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
        for replace_parent in [false, true] {
            let store = test_store();
            let (original, reservation) =
                scratch_reservation_fixture(&store, "scratch-replacement");
            let path = original.path().unwrap();
            store
                .reserve_process_resource_launch_with_scratch(&reservation, &original)
                .unwrap();
            let replaced = if replace_parent {
                path.parent().unwrap().to_owned()
            } else {
                path.clone()
            };
            let saved = replaced.with_extension("original");
            std::fs::rename(&replaced, &saved).unwrap();
            std::fs::create_dir(&replaced).unwrap();
            let replacement = if replace_parent {
                std::fs::create_dir(&path).unwrap();
                path.clone()
            } else {
                replaced
            };
            std::fs::write(replacement.join("replacement"), b"do not delete").unwrap();
            assert!(
                store
                    .recover_never_issued_resource_scratch(&reservation)
                    .is_err()
            );
            assert_eq!(
                std::fs::read(replacement.join("replacement")).unwrap(),
                b"do not delete"
            );
            let retained = store.process_resource_reservations().unwrap().remove(0);
            assert_eq!(
                retained.runtime_custody.as_ref().unwrap().physical_state,
                ProcessCustodyPhysicalState::Retained
            );
            assert_eq!(
                store
                    .lock()
                    .unwrap()
                    .runtime_db
                    .process_resource_cas_roots()
                    .unwrap(),
                reservation
                    .runtime_custody
                    .as_ref()
                    .unwrap()
                    .cas_roots()
                    .unwrap()
                    .into_iter()
                    .collect::<Vec<_>>()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scratch_failed_physical_removal_keeps_original_guard_intent_and_pins() {
        use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
        use std::os::unix::fs::PermissionsExt as _;
        let store = test_store();
        let (original, reservation) = scratch_reservation_fixture(&store, "scratch-removal-denied");
        let path = original.path().unwrap();
        store
            .reserve_process_resource_launch_with_scratch(&reservation, &original)
            .unwrap();
        let pins = store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_materialization_pins()
            .unwrap();
        // Actual unprivileged filesystem unlink refusal, not worker evidence.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
        let failed = store.remove_never_issued_resource_scratch(&reservation, &original);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            failed.is_err(),
            "control requires the unprivileged filesystem denial"
        );
        assert_eq!(original.path(), Some(path.clone()));
        assert!(path.join("sentinel").exists());
        let intent = store.process_resource_reservations().unwrap().remove(0);
        assert_eq!(
            intent.runtime_custody.as_ref().unwrap().physical_state,
            ProcessCustodyPhysicalState::CleanupIntent
        );
        assert_eq!(
            store
                .lock()
                .unwrap()
                .runtime_db
                .process_resource_materialization_pins()
                .unwrap(),
            pins
        );
        store
            .remove_never_issued_resource_scratch(&intent, &original)
            .unwrap();
        assert!(!path.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scratch_missing_root_before_cleanup_intent_cannot_mint_receipt() {
        use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
        let store = test_store();
        let (original, reservation) =
            scratch_reservation_fixture(&store, "scratch-missing-no-intent");
        store
            .reserve_process_resource_launch_with_scratch(&reservation, &original)
            .unwrap();
        let roots = store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_cas_roots()
            .unwrap();
        original.remove_now().unwrap(); // Host fixture removal WITHOUT intent.
        assert!(
            store
                .recover_never_issued_resource_scratch(&reservation)
                .is_err()
        );
        assert_eq!(
            store.process_resource_reservations().unwrap()[0]
                .runtime_custody
                .as_ref()
                .unwrap()
                .physical_state,
            ProcessCustodyPhysicalState::Retained
        );
        assert_eq!(
            store
                .lock()
                .unwrap()
                .runtime_db
                .process_resource_cas_roots()
                .unwrap(),
            roots
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scratch_recovery_preserves_birth_authority_after_physical_removal_lost_ack() {
        use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("node-key.pem");
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&key).unwrap();
        let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let trust = Arc::new(trust);
        let open = || TestStateStore::open(root.path(), signer.clone(), trust.clone()).unwrap();
        let store = open();
        let (original, reservation) =
            scratch_reservation_fixture(&store, "scratch-restart-lost-ack");
        let path = original.path().unwrap();
        store
            .reserve_process_resource_launch_with_scratch(&reservation, &original)
            .unwrap();
        let birth = store
            .lock()
            .unwrap()
            .runtime_db
            .fixture_previous_scratch_daemon_birth(
                &reservation,
                "fixture-original-daemon-incarnation",
            )
            .unwrap();
        let intent = store
            .lock()
            .unwrap()
            .runtime_db
            .claim_never_issued_scratch_cleanup(&birth)
            .unwrap();
        original.remove_now().unwrap(); // Simulate crash BEFORE receipt ACK.
        assert!(!path.exists());
        let roots = store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_cas_roots()
            .unwrap();
        drop(original);
        drop(store);
        let reopened = open();
        assert_ne!(
            intent.daemon_generation_id,
            runtime_db::daemon_generation_id()
        );
        assert_eq!(
            reopened.process_resource_reservations().unwrap(),
            vec![intent.clone()]
        );
        assert_eq!(
            reopened
                .lock()
                .unwrap()
                .runtime_db
                .process_resource_cas_roots()
                .unwrap(),
            roots
        );
        let removed = reopened
            .recover_never_issued_resource_scratch(&intent)
            .unwrap();
        assert_eq!(removed.daemon_generation_id, birth.daemon_generation_id);
        assert_eq!(
            removed.runtime_custody.as_ref().unwrap().physical_state,
            ProcessCustodyPhysicalState::PhysicallyRemoved
        );
        assert_eq!(
            reopened
                .lock()
                .unwrap()
                .runtime_db
                .process_resource_cas_roots()
                .unwrap(),
            roots
        );
    }

    #[test]
    fn shutdown_closure_serializes_resource_reservation_and_scope_binding() {
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let (reservation, _) = process_resource_reservation_fixture("reserve-race");
        let guard = store.lock().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reserve_store = Arc::clone(&store);
        let reserve_record = reservation.clone();
        let reserve = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            reserve_store.reserve_process_resource_launch(&reserve_record)
        });
        ready_rx.recv().unwrap();
        // This is the shutdown linearization point while the reservation is
        // queued behind the same StateStore lock. It must observe closure
        // after acquiring the lock, not rely on a stale pre-lock read.
        store
            .process_attachment_admission_open
            .store(false, Ordering::Release);
        drop(guard);
        assert!(
            reserve
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("closed for daemon shutdown")
        );
        assert!(store.process_resource_reservations().unwrap().is_empty());

        // One Store is live per process: close the first before the second.
        drop(store);
        store_owner.close().unwrap();
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let (reservation, recovery) = process_resource_reservation_fixture("bind-race");
        store.reserve_process_resource_launch(&reservation).unwrap();
        let guard = store.lock().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let bind_store = Arc::clone(&store);
        let bind = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            bind_store.bind_process_resource_scope("thread", "bind-race", &recovery)
        });
        ready_rx.recv().unwrap();
        store
            .process_attachment_admission_open
            .store(false, Ordering::Release);
        drop(guard);
        assert!(
            bind.join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("closed for daemon shutdown")
        );
        assert_eq!(
            store
                .process_resource_reservation("thread", "bind-race")
                .unwrap()
                .unwrap()
                .launch_authority
                .local_scope_recovery()
                .cloned(),
            None
        );
    }

    #[test]
    fn shutdown_gate_rejects_late_dedicated_worker_attachment() {
        let store = test_store();
        store
            .close_process_attachment_admission()
            .expect("close shutdown admission");
        let record = WorkerProcessRecord {
            worker_instance_id: "worker-after-shutdown".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: "c".repeat(64),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: crate::process::ExecutionProcessIdentity {
                schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                process_scope: None,
                boot_id: "test-boot".to_owned(),
                target_pid: 12345,
                target_start_time_ticks: 10,
                group_leader_pid: 12345,
                group_leader_start_time_ticks: 10,
                resource_selections: Vec::new(),
                resource_settlement_authority: None,
                resource_operations: Vec::new(),
                resource_allocation_limit: None,
                resource_occupancy_start: None,
                resource_occupancy_limit: None,
                resource_cleanup_allowance_ms: None,
            },
            control_channel_identity: "fd:test".to_owned(),
            state: runtime_db::WorkerProcessState::Attached,
            daemon_generation_id: "daemon:test".to_owned(),
            placement_thread_id: "T-after-shutdown".to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let error = store
            .attach_worker_process(&record)
            .expect_err("shutdown gate must reject a late dedicated worker attach");
        assert!(
            error
                .to_string()
                .contains("attachment admission is closed for daemon shutdown")
        );
    }

    #[test]
    fn in_process_root_birth_rejects_noncanonical_event_shape_without_partial_rows() {
        let store = test_store();
        let thread_id = "T-in-process-root-birth-rejected";
        let owner = store
            .register_in_process_handler(thread_id)
            .expect("register in-process owner");
        let error = store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(thread_id, thread_id),
                Vec::new(),
                &in_process_launch_metadata(),
                &owner,
            )
            .expect_err("missing thread_started event must fail closed");
        assert!(
            error
                .to_string()
                .contains("requires exactly one thread_started successor event")
        );
        assert!(
            store
                .get_thread(thread_id)
                .expect("inspect rejected root")
                .is_none()
        );
        let inner = store.lock().expect("lock state store");
        assert!(
            inner
                .runtime_db
                .get_runtime_info(thread_id)
                .expect("inspect rejected runtime row")
                .is_none()
        );
        assert!(
            inner
                .runtime_db
                .in_process_handler_reservations_after(
                    None,
                    runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
                )
                .expect("inspect rejected reservation")
                .is_empty()
        );
        assert!(
            inner
                .state_db
                .read_generic_head_ref("chains", thread_id)
                .expect("inspect rejected chain head")
                .is_none()
        );
    }

    #[test]
    fn in_process_root_birth_requires_the_exact_registered_owner() {
        let store = test_store();
        let thread_id = "T-in-process-root-owner-qualified";
        let metadata = in_process_launch_metadata();
        let stale = InProcessHandlerControl::new();
        let events = || {
            vec![NewEventRecord {
                event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({}),
            }]
        };

        let error = store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(thread_id, thread_id),
                events(),
                &metadata,
                &stale,
            )
            .expect_err("unregistered owner must not publish an in-process root");
        assert!(
            error
                .to_string()
                .contains("recorded in-process handler has no active daemon owner")
        );

        let owner = store
            .register_in_process_handler(thread_id)
            .expect("register exact in-process owner");
        let error = store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(thread_id, thread_id),
                events(),
                &metadata,
                &stale,
            )
            .expect_err("stale owner must not publish an in-process root");
        assert!(error.to_string().contains("stale in-process handler owner"));
        assert!(
            store
                .get_thread(thread_id)
                .expect("inspect owner-rejected root")
                .is_none()
        );

        store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(thread_id, thread_id),
                events(),
                &metadata,
                &owner,
            )
            .expect("exact registered owner publishes the root");
    }

    #[test]
    fn failed_duplicate_birth_owner_cannot_adopt_or_terminalize_existing_root() {
        let store = test_store();
        let thread_id = "T-in-process-duplicate-birth";
        let metadata = in_process_launch_metadata();
        let started = || {
            vec![NewEventRecord {
                event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({}),
            }]
        };
        let original_owner = store
            .register_in_process_handler(thread_id)
            .expect("register original owner");
        store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(thread_id, thread_id),
                started(),
                &metadata,
                &original_owner,
            )
            .expect("publish original root");
        assert!(original_owner.has_committed_birth());
        assert!(
            store
                .unregister_in_process_handler(thread_id, &original_owner)
                .expect("simulate loss of original volatile owner")
        );
        assert_eq!(
            original_owner.completion(),
            Some(InProcessHandlerCompletion::OwnerLostUnsettled)
        );

        let colliding_owner = store
            .register_in_process_handler(thread_id)
            .expect("register colliding owner");
        let error = store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(thread_id, thread_id),
                started(),
                &metadata,
                &colliding_owner,
            )
            .expect_err("duplicate birth must fail");
        assert!(!error.to_string().is_empty());
        assert!(!colliding_owner.has_committed_birth());

        let terminal = FinalizeThreadRecord {
            status: ThreadStatus::Failed.as_str().to_string(),
            outcome_code: Some("service_interrupted".to_string()),
            result_json: None,
            error_json: Some(json!({"error": "must not be adopted"})),
            artifacts: Vec::new(),
            final_cost: None,
            managed_envelope: None,
            result_project_snapshot_hash: None,
            result_workspace_output_capture_hash: None,
        };
        let error = store
            .finalize_in_process_handler_owned(thread_id, &colliding_owner, &terminal)
            .expect_err("pre-commit owner cannot terminalize an existing root");
        assert!(error.to_string().contains("before its birth commits"));
        let existing = store
            .get_thread(thread_id)
            .expect("read existing root")
            .expect("existing root remains");
        assert_eq!(existing.status, ThreadStatus::Running.as_str());
        let authoritative = store
            .get_authoritative_root_thread_snapshot(thread_id)
            .expect("read authoritative existing root")
            .expect("authoritative existing root remains");
        assert_eq!(authoritative.outcome_code, None);
        assert_eq!(
            replayed_event_types(&store, thread_id),
            ["thread_created", "thread_started"]
        );
        assert!(
            store
                .unregister_in_process_handler(thread_id, &colliding_owner)
                .expect("retire rejected colliding owner")
        );
        assert_eq!(
            colliding_owner.completion(),
            Some(InProcessHandlerCompletion::BirthAborted)
        );
    }

    #[test]
    fn in_process_terminal_settlement_requires_the_exact_registered_owner() {
        let store = test_store();
        let thread_id = "T-in-process-exact-owner";
        let owner = store
            .register_in_process_handler(thread_id)
            .expect("register in-process owner");
        let duplicate = store
            .register_in_process_handler(thread_id)
            .err()
            .expect("duplicate in-process owner must fail closed");
        assert!(duplicate.to_string().contains("already registered"));
        store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(thread_id, thread_id),
                vec![NewEventRecord {
                    event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                    storage_class: "indexed".to_string(),
                    payload: json!({}),
                }],
                &in_process_launch_metadata(),
                &owner,
            )
            .expect("publish in-process root");

        let terminal = FinalizeThreadRecord {
            status: ThreadStatus::Completed.as_str().to_string(),
            outcome_code: Some(ThreadStatus::Completed.as_str().to_string()),
            result_json: Some(json!({"ok": true})),
            error_json: None,
            artifacts: Vec::new(),
            final_cost: None,
            managed_envelope: None,
            result_project_snapshot_hash: None,
            result_workspace_output_capture_hash: None,
        };
        let stale = InProcessHandlerControl::new();
        let error = store
            .finalize_in_process_handler_owned(thread_id, &stale, &terminal)
            .expect_err("stale in-process owner must not settle the root");
        assert!(error.to_string().contains("stale in-process handler owner"));
        assert_eq!(
            store
                .get_thread(thread_id)
                .expect("read root after rejected settlement")
                .expect("root after rejected settlement")
                .status,
            ThreadStatus::Running.as_str()
        );

        assert!(matches!(
            store
                .finalize_in_process_handler_owned(thread_id, &owner, &terminal)
                .expect("exact owner settles root"),
            FinalizeIfNonterminalOutcome::Finalized { .. }
        ));
        assert_eq!(
            store
                .get_thread(thread_id)
                .expect("read settled root")
                .expect("settled root")
                .status,
            ThreadStatus::Completed.as_str()
        );
        let error = store
            .settle_ownerless_in_process_handler_reservation(thread_id)
            .expect_err("shutdown settlement must not steal from an active owner");
        assert!(error.to_string().contains("while its owner is active"));
        assert!(
            store
                .settle_in_process_handler_reservation_owned(thread_id, &owner)
                .expect("exact owner settles terminal reservation")
        );
        let error = store
            .delete_ownerless_terminal_in_process_handler_reservation(thread_id)
            .expect_err("shutdown cleanup must not delete an active owner's reservation");
        assert!(error.to_string().contains("while its owner is active"));
        owner.mark_terminal_confirmed();
        assert!(
            store
                .unregister_in_process_handler(thread_id, &owner)
                .expect("unregister exact owner")
        );
        assert!(
            !store
                .is_in_process_handler_active(thread_id)
                .expect("inspect active owner")
        );
        assert_eq!(
            owner.completion(),
            Some(InProcessHandlerCompletion::TerminalConfirmed)
        );
        assert!(
            store
                .delete_ownerless_terminal_in_process_handler_reservation(thread_id)
                .expect("ownerless shutdown cleanup deletes terminal reservation residue")
        );
        assert!(
            store
                .in_process_handler_reservations_after(
                    None,
                    runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
                )
                .expect("list settled reservations")
                .is_empty()
        );
    }

    #[test]
    fn primary_stop_boundary_rejects_cancel_and_kill_for_active_in_process_owner() {
        let store = test_store();
        let thread_id = "T-in-process-stop-fenced";
        let owner = store
            .register_in_process_handler(thread_id)
            .expect("register in-process owner");
        store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(thread_id, thread_id),
                vec![NewEventRecord {
                    event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                    storage_class: "indexed".to_string(),
                    payload: json!({}),
                }],
                &in_process_launch_metadata(),
                &owner,
            )
            .expect("publish in-process root");

        assert_eq!(
            store
                .in_process_handler_stop_refusal(thread_id)
                .expect("read shared API/command stop fence"),
            Some(InProcessHandlerStopRefusal::MissingCancellationContract)
        );
        for intent in [StopIntent::Cancel, StopIntent::Kill] {
            let error = store
                .request_thread_stop(thread_id, intent)
                .expect_err("active in-process owner has no admitted stop contract");
            assert!(
                error
                    .to_string()
                    .contains("has no declared cancellation/drain contract")
            );
            let detail = store
                .get_thread(thread_id)
                .expect("read stop-fenced root")
                .expect("stop-fenced root");
            assert_eq!(detail.status, ThreadStatus::Running.as_str());
            assert_eq!(detail.runtime.stop_intent, None);
        }

        assert_eq!(
            replayed_event_types(&store, thread_id),
            ["thread_created", "thread_started"],
            "rejected stop requests must not append terminal or tombstone events"
        );
        assert!(
            store
                .unregister_in_process_handler(thread_id, &owner)
                .expect("unregister in-process owner")
        );
    }

    #[test]
    fn fresh_launch_reservation_precedes_thread_publication() {
        let store = test_store();
        let thread_id = "T-fresh-launch-reservation";

        assert_eq!(
            store
                .reserve_fresh_thread_launch(thread_id, "claim-fresh", "daemon:test")
                .expect("reserve fresh launch"),
            runtime_db::LaunchClaimOutcome::Claimed
        );
        assert!(
            store
                .get_thread(thread_id)
                .expect("inspect unpublished thread")
                .is_none()
        );

        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("publish reserved thread");
        let claim = store
            .get_launch_claim(thread_id)
            .expect("read launch claim")
            .expect("claim remains attached after publication");
        assert_eq!(claim.claim_id, "claim-fresh");
        assert!(
            store
                .release_thread_launch_claim(thread_id, "claim-fresh")
                .expect("release launch reservation")
        );
    }

    #[test]
    fn running_output_observations_are_fenced_by_exact_launch_owner() {
        let store = test_store();
        let thread_id = "T-output-owner";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .unwrap();
        store
            .claim_thread_launch(thread_id, "claim-1", "daemon:test")
            .unwrap();
        let first = store
            .get_launch_claim(thread_id)
            .unwrap()
            .unwrap()
            .claimed_by;
        store.mark_thread_running(thread_id, None).unwrap();
        let events = [NewEventRecord {
            event_type: ryeos_state::event_types::SUBPROCESS_OUTPUT_OBSERVED.into(),
            storage_class: "indexed".into(),
            payload: json!({"data": "first"}),
        }];
        assert!(
            store
                .append_events_if_thread_running_owned(thread_id, thread_id, &events, &first)
                .unwrap()
                .is_some()
        );
        store
            .release_thread_launch_claim(thread_id, "claim-1")
            .unwrap();
        store
            .claim_thread_launch(thread_id, "claim-2", "daemon:test")
            .unwrap();
        let second = store
            .get_launch_claim(thread_id)
            .unwrap()
            .unwrap()
            .claimed_by;
        assert_ne!(first, second);
        let before = replayed_event_types(&store, thread_id);
        assert!(
            store
                .append_events_if_thread_running_owned(thread_id, thread_id, &events, &first)
                .unwrap()
                .is_none()
        );
        assert_eq!(replayed_event_types(&store, thread_id), before);
        assert!(
            store
                .append_events_if_thread_running_owned(thread_id, thread_id, &events, &second)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn claimed_created_thread_accepts_exact_launch_attempt_audit() {
        let store = test_store();
        let thread_id = "T-launch-audit";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create thread");
        let audit = launch_attempt_audit_events();

        // The runtime-authored boundary remains running-only.
        assert!(
            store
                .append_events_if_thread_running(thread_id, thread_id, &audit)
                .expect("runtime append guard")
                .is_none()
        );
        assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);

        assert_eq!(
            store
                .claim_thread_launch(thread_id, "claim-audit", "daemon:test")
                .expect("claim launch"),
            runtime_db::LaunchClaimOutcome::Claimed
        );
        let persisted = store
            .append_launch_attempt_audit(thread_id, thread_id, &audit)
            .expect("append claimed launch audit");
        assert_eq!(persisted.len(), 3);
        assert_eq!(
            persisted
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            LAUNCH_ATTEMPT_AUDIT_TYPES
                .iter()
                .map(|event_type| event_type.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            replayed_event_types(&store, thread_id),
            [
                "thread_created",
                ryeos_runtime::RuntimeEventType::AsLaunchedResolution.as_str(),
                ryeos_runtime::RuntimeEventType::AsLaunchedRefBindings.as_str(),
                ryeos_runtime::RuntimeEventType::RuntimeLaunchFacts.as_str(),
            ]
        );
    }

    #[test]
    fn launch_attempt_audit_rejections_do_not_mutate_the_chain() {
        let store = test_store();
        let thread_id = "T-launch-audit-reject";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create thread");
        let audit = launch_attempt_audit_events();

        let unclaimed = store
            .append_launch_attempt_audit(thread_id, thread_id, &audit)
            .expect_err("unclaimed launch audit must fail");
        assert!(unclaimed.to_string().contains("no active launch claim"));
        assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);

        assert_eq!(
            store
                .claim_thread_launch(thread_id, "claim-reject", "daemon:test")
                .expect("claim launch"),
            runtime_db::LaunchClaimOutcome::Claimed
        );
        let mut wrong_event = audit.clone();
        wrong_event.swap(0, 1);
        let wrong_event_error = store
            .append_launch_attempt_audit(thread_id, thread_id, &wrong_event)
            .expect_err("out-of-order audit must fail");
        assert!(wrong_event_error.to_string().contains("event 0"));
        assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);

        let mut wrong_storage = audit;
        wrong_storage[0].storage_class = "journal".to_string();
        let wrong_storage_error = store
            .append_launch_attempt_audit(thread_id, thread_id, &wrong_storage)
            .expect_err("non-canonical audit storage must fail");
        assert!(
            wrong_storage_error
                .to_string()
                .contains("canonical storage class")
        );
        assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);
    }

    #[test]
    fn launch_attempt_audit_rejects_terminal_and_process_attached_rows_without_mutation() {
        let audit = launch_attempt_audit_events();

        let terminal_store = test_store();
        let terminal_id = "T-launch-audit-terminal";
        terminal_store
            .create_thread_for_test(&thread_record(terminal_id, terminal_id))
            .expect("create terminal fixture");
        terminal_store
            .claim_thread_launch(terminal_id, "claim-terminal", "daemon:test")
            .expect("claim terminal fixture");
        terminal_store
            .finalize_thread(
                terminal_id,
                &FinalizeThreadRecord {
                    status: ThreadStatus::Completed.as_str().to_string(),
                    outcome_code: Some(ThreadStatus::Completed.as_str().to_string()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .expect("finalize fixture");
        let terminal_before = replayed_event_types(&terminal_store, terminal_id);
        let terminal_error = terminal_store
            .append_launch_attempt_audit(terminal_id, terminal_id, &audit)
            .expect_err("terminal launch audit must fail");
        assert!(
            terminal_error
                .to_string()
                .contains("requires a created or running thread")
        );
        assert_eq!(
            replayed_event_types(&terminal_store, terminal_id),
            terminal_before
        );

        // One Store is live per process: close the first before the second.
        terminal_store.close().unwrap();
        let attached_store = test_store();
        let attached_id = "T-launch-audit-attached";
        attached_store
            .create_thread_for_test(&thread_record(attached_id, attached_id))
            .expect("create attached fixture");
        attached_store
            .claim_thread_launch(attached_id, "claim-attached", "daemon:test")
            .expect("claim attached fixture");
        attached_store
            .attach_thread_process(
                attached_id,
                12345,
                67890,
                &crate::process::ExecutionProcessIdentity {
                    schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                    process_scope: None,
                    boot_id: "test-boot".to_string(),
                    target_pid: 12345,
                    target_start_time_ticks: 10,
                    group_leader_pid: 67890,
                    group_leader_start_time_ticks: 20,
                    resource_selections: Vec::new(),
                    resource_settlement_authority: None,
                    resource_operations: Vec::new(),
                    resource_allocation_limit: None,
                    resource_occupancy_start: None,
                    resource_occupancy_limit: None,
                    resource_cleanup_allowance_ms: None,
                },
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect("attach fixture process");
        let attached_before = replayed_event_types(&attached_store, attached_id);
        let attached_error = attached_store
            .append_launch_attempt_audit(attached_id, attached_id, &audit)
            .expect_err("process-attached launch audit must fail");
        assert!(attached_error.to_string().contains("process attachment"));
        assert_eq!(
            replayed_event_types(&attached_store, attached_id),
            attached_before
        );
    }

    #[test]
    fn pre_release_attachment_is_strict_and_release_observes_stop_tombstone() {
        let store = test_store();
        let thread_id = "T-pre-release-attachment";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create attachment fixture");
        let identity = crate::process::ExecutionProcessIdentity {
            schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            process_scope: None,
            boot_id: "test-boot".to_string(),
            target_pid: 12345,
            target_start_time_ticks: 10,
            group_leader_pid: 12345,
            group_leader_start_time_ticks: 10,
            resource_selections: Vec::new(),
            resource_settlement_authority: None,
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        };
        store
            .attach_new_thread_process(
                thread_id,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect("attach exact held identity");
        let repeat = store
            .attach_new_thread_process(
                thread_id,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect_err("pre-release exact repeat must be rejected");
        assert!(repeat.to_string().contains("already attached"));

        store
            .authorize_attached_process_release(thread_id, &identity, None)
            .expect("release exact attached identity");
        store
            .request_thread_stop(thread_id, StopIntent::Cancel)
            .expect("persist stop tombstone");
        let stopped = store
            .authorize_attached_process_release(thread_id, &identity, None)
            .expect_err("stop tombstone must fence release");
        assert!(stopped.to_string().contains("cancel request"));
    }

    #[test]
    fn attached_process_listing_does_not_wait_for_the_global_store_lock() {
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let thread_id = "T-attached-process-registry";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create attachment fixture");
        let identity = crate::process::ExecutionProcessIdentity {
            schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            process_scope: None,
            boot_id: "test-boot".to_string(),
            target_pid: 12346,
            target_start_time_ticks: 11,
            group_leader_pid: 12346,
            group_leader_start_time_ticks: 11,
            resource_selections: Vec::new(),
            resource_settlement_authority: None,
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        };
        store
            .attach_new_thread_process(
                thread_id,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect("attach exact held identity");

        let global_guard = store.lock().expect("hold global state lock");
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let listing_store = Arc::clone(&store);
        let listing = std::thread::spawn(move || {
            sender
                .send(listing_store.list_attached_thread_ids())
                .expect("send listing result");
        });
        let ids = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("attached listing must not wait for the global state lock")
            .expect("list attached threads");
        assert_eq!(ids, vec![thread_id.to_string()]);
        drop(global_guard);
        listing.join().expect("join listing thread");

        let wrong_identity = crate::process::ExecutionProcessIdentity {
            target_start_time_ticks: 12,
            ..identity.clone()
        };
        assert!(
            !store
                .clear_thread_process_if_matches(thread_id, &wrong_identity)
                .expect("mismatched clear is a proven no-op")
        );
        assert_eq!(
            store.list_attached_thread_ids().expect("list after no-op"),
            vec![thread_id.to_string()]
        );
        assert!(
            store
                .clear_thread_process_if_matches(thread_id, &identity)
                .expect("clear exact attachment")
        );
        assert!(
            store
                .list_attached_thread_ids()
                .expect("list after clear")
                .is_empty()
        );

        store
            .attach_new_thread_process(
                thread_id,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect("reattach fixture process");
        {
            let global_guard = store.lock().expect("lock for runtime-row rollback");
            assert_eq!(
                store
                    .delete_thread_runtime_locked(&global_guard, thread_id)
                    .expect("delete attached runtime row"),
                1
            );
        }
        assert!(
            store
                .list_attached_thread_ids()
                .expect("list after runtime-row deletion")
                .is_empty()
        );
    }

    #[test]
    fn attached_process_registry_is_seeded_from_durable_runtime_state() {
        let tmp = tempdir().expect("tempdir").keep();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
                .expect("test node identity");
        let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let head_trust = Arc::new(head_trust);
        let open_store = || {
            TestStateStore::open(&tmp, Arc::clone(&signer), Arc::clone(&head_trust))
                .expect("state store")
        };

        let thread_id = "T-attached-process-reopen";
        let process_identity = crate::process::ExecutionProcessIdentity {
            schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            process_scope: None,
            boot_id: "test-boot".to_string(),
            target_pid: 12347,
            target_start_time_ticks: 12,
            group_leader_pid: 12347,
            group_leader_start_time_ticks: 12,
            resource_selections: Vec::new(),
            resource_settlement_authority: None,
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        };
        let store = open_store();
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create attachment fixture");
        store
            .attach_new_thread_process(
                thread_id,
                process_identity.target_pid,
                process_identity.group_leader_pid,
                &process_identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect("attach fixture process");
        drop(store);

        let reopened = open_store();
        assert_eq!(
            reopened
                .list_attached_thread_ids()
                .expect("list seeded attachments"),
            vec![thread_id.to_string()]
        );
        assert!(
            reopened
                .clear_thread_process_if_matches(thread_id, &process_identity)
                .expect("clear seeded attachment")
        );
    }

    fn continuation_resume_context(
        project_context: ProjectContext,
    ) -> crate::launch_metadata::ResumeContext {
        let stable_project_identity = match &project_context {
            ProjectContext::LocalPath { path } => Some(
                crate::launch_metadata::StableProjectIdentity::from_path(path, "site:test")
                    .unwrap(),
            ),
            _ => None,
        };
        let local_overlay_root = Some(PathBuf::from("/work/project"));
        let original_snapshot_hash = Some("a".repeat(64));
        let project_root = PathBuf::from("/work/project");
        let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            format!("local:{}", project_root.display()),
            Some(project_root),
            original_snapshot_hash.clone().unwrap(),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
            },
            ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
                project_authority_id: lillux::sha256_hex(
                    b"live-project\0local:/work/project\0/work/project",
                ),
                source_identity: "dotenv:/work/project/.env".to_string(),
                include_operator_vault: true,
                name_authority: ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
            },
            Vec::new(),
        )
        .unwrap();
        crate::launch_metadata::ResumeContext {
            kind: "directive".to_string(),
            item_ref: "directive:test".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            product_selections: Vec::new(),
            launch_mode: "wait".to_string(),
            parameters: json!({}),
            project_context,
            project_authority,
            lifecycle_authority:
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            stable_project_identity,
            local_overlay_root,
            original_snapshot_hash,
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".to_string(),
                scopes: vec!["execute".to_string()],
            }),
            execution_hints: ExecutionHints::default(),
            scheduled_fire: None,
            effective_caps: Vec::new(),
            parent_delegation_caps: None,
            executor_ref: Some("native:test".to_string()),
            runtime_ref: None,
        }
    }

    #[test]
    fn continuation_snapshot_binds_project_root_and_pin() {
        let source = "T-source";
        let mut record = thread_record("T-successor", "T-root");
        record.upstream_thread_id = Some(source.to_string());
        record.base_project_snapshot_hash = Some("a".repeat(64));
        let resume = continuation_resume_context(ProjectContext::LocalPath {
            path: PathBuf::from("/work/project"),
        });
        record.project_authority = resume.project_authority.clone();
        record.project_root = Some(PathBuf::from("/wrong/caller/path"));
        assert!(build_continuation_snapshot(&record, &resume).is_err());
        record.project_root = Some(PathBuf::from("/work/project"));
        let snapshot = build_continuation_snapshot(&record, &resume).unwrap();
        assert_eq!(snapshot.project_root, Some(PathBuf::from("/work/project")));
        assert_eq!(snapshot.base_project_snapshot_hash, Some("a".repeat(64)));
    }

    #[test]
    fn continuation_handoff_persists_created_successor_with_inherited_pin() {
        let store = test_store();
        let resume = continuation_resume_context(ProjectContext::LocalPath {
            path: PathBuf::from("/work/project"),
        });
        let launch_metadata = crate::launch_metadata::RuntimeLaunchMetadata::default()
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime)
            .with_resume_context(resume.clone());
        store
            .create_root_thread_with_events_and_launch_metadata(
                &thread_record("T-root", "T-root"),
                Vec::new(),
                Some(&launch_metadata),
            )
            .expect("root thread");

        let mut successor = thread_record("T-successor", "T-root");
        successor.project_root = Some(PathBuf::from("/work/project"));
        successor.base_project_snapshot_hash = Some("a".repeat(64));
        successor.project_authority = resume.project_authority.clone();
        let outcome = store
            .create_or_get_continuation_for_test(
                &successor,
                "T-root",
                "T-root",
                Some("follow"),
                "request-fingerprint",
                Some(&resume),
            )
            .expect("create continuation successor");
        assert!(matches!(outcome, ContinuationOutcome::Created { .. }));

        let projected = store
            .get_thread("T-successor")
            .expect("read successor")
            .expect("successor row");
        assert_eq!(projected.status, ThreadStatus::Created.as_str());
        assert_eq!(projected.upstream_thread_id.as_deref(), Some("T-root"));

        let inner = store.lock().expect("lock state store");
        let persisted = authoritative_snapshot_for_transition(&inner, "T-root", "T-successor")
            .expect("read authoritative successor snapshot");
        assert_eq!(persisted.base_project_snapshot_hash, Some("a".repeat(64)));
    }

    #[test]
    fn exact_child_link_replay_after_parent_continues_does_not_cancel_child() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-parent", "T-parent"))
            .expect("parent thread");
        store
            .create_thread_for_test(&thread_record("T-child", "T-child"))
            .expect("child thread");

        assert_eq!(
            store
                .record_child_link("T-parent", "T-child", "dispatch")
                .expect("initial child link"),
            None
        );
        store
            .finalize_thread(
                "T-parent",
                &FinalizeThreadRecord {
                    status: ThreadStatus::Continued.as_str().to_string(),
                    outcome_code: Some(ThreadStatus::Continued.as_str().to_string()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .expect("continue parent");

        assert_eq!(
            store
                .record_child_link("T-parent", "T-child", "dispatch")
                .expect("exact link replay"),
            None
        );
        let child = store
            .get_thread("T-child")
            .expect("read child")
            .expect("child row");
        assert_eq!(child.runtime.stop_intent, None);
    }

    #[test]
    fn fresh_dispatch_link_after_parent_terminal_atomically_cancels_child() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-parent", "T-parent"))
            .expect("parent thread");
        store
            .create_thread_for_test(&thread_record("T-child", "T-child"))
            .expect("child thread");
        store
            .finalize_thread(
                "T-parent",
                &FinalizeThreadRecord {
                    status: ThreadStatus::Completed.as_str().to_string(),
                    outcome_code: Some(ThreadStatus::Completed.as_str().to_string()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .expect("complete parent");

        assert_eq!(
            store
                .record_child_link("T-parent", "T-child", "dispatch")
                .expect("late child link"),
            Some(StopIntent::Cancel)
        );
        let child = store
            .get_thread("T-child")
            .expect("read child")
            .expect("child row");
        assert_eq!(child.runtime.stop_intent, Some(StopIntent::Cancel));
    }

    #[test]
    fn exact_child_link_replay_propagates_durable_parent_kill() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-parent", "T-parent"))
            .expect("parent thread");
        store
            .create_thread_for_test(&thread_record("T-child", "T-child"))
            .expect("child thread");
        store
            .record_child_link("T-parent", "T-child", "dispatch")
            .expect("initial child link");
        store
            .request_thread_stop("T-parent", StopIntent::Kill)
            .expect("kill parent");

        assert_eq!(
            store
                .record_child_link("T-parent", "T-child", "dispatch")
                .expect("exact link replay"),
            Some(StopIntent::Kill)
        );
        let child = store
            .get_thread("T-child")
            .expect("read child")
            .expect("child row");
        assert_eq!(child.runtime.stop_intent, Some(StopIntent::Kill));
    }

    #[test]
    fn inherited_child_stop_rejects_active_in_process_owner_without_linking_or_tombstoning() {
        let store = test_store();
        let parent_id = "T-parent-stop-fenced";
        let child_id = "T-in-process-child-stop-fenced";
        store
            .create_thread_for_test(&thread_record(parent_id, parent_id))
            .expect("parent thread");
        let owner = store
            .register_in_process_handler(child_id)
            .expect("register in-process child owner");
        store
            .create_in_process_root_with_events_and_launch_metadata(
                &thread_record(child_id, child_id),
                vec![NewEventRecord {
                    event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                    storage_class: "indexed".to_string(),
                    payload: json!({}),
                }],
                &in_process_launch_metadata(),
                &owner,
            )
            .expect("publish in-process child");
        store
            .request_thread_stop(parent_id, StopIntent::Kill)
            .expect("tombstone parent");

        let error = store
            .record_child_link(parent_id, child_id, "dispatch")
            .expect_err("active in-process child cannot inherit undeclared stop authority");
        assert!(
            error
                .to_string()
                .contains("has no declared cancellation/drain contract")
        );
        let child = store
            .get_thread(child_id)
            .expect("read fenced child")
            .expect("fenced child row");
        assert_eq!(child.status, ThreadStatus::Running.as_str());
        assert_eq!(child.runtime.stop_intent, None);
        assert!(
            store
                .descendant_thread_ids(parent_id)
                .expect("read parent descendants")
                .is_empty()
        );
        assert!(
            store
                .unregister_in_process_handler(child_id, &owner)
                .expect("unregister in-process child owner")
        );
    }

    fn in_process_child_birth_events() -> Vec<NewEventRecord> {
        vec![NewEventRecord {
            event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({}),
        }]
    }

    fn in_process_child_record(child_id: &str, parent_id: &str) -> NewThreadRecord {
        let mut record = thread_record(child_id, parent_id);
        record.upstream_thread_id = Some(parent_id.to_string());
        record
    }

    fn in_process_reservation_phase(
        store: &StateStore,
        thread_id: &str,
    ) -> Option<runtime_db::InProcessHandlerReservationPhase> {
        store
            .in_process_handler_reservation(thread_id)
            .expect("read in-process reservation")
            .map(|reservation| reservation.phase)
    }

    fn requested_failure() -> FinalizeThreadRecord {
        FinalizeThreadRecord {
            status: ThreadStatus::Failed.as_str().to_string(),
            outcome_code: Some(ThreadStatus::Failed.as_str().to_string()),
            result_json: None,
            error_json: Some(json!({"reason": "handler_failed"})),
            artifacts: Vec::new(),
            final_cost: None,
            managed_envelope: None,
            result_project_snapshot_hash: None,
            result_workspace_output_capture_hash: None,
        }
    }

    #[test]
    fn in_process_child_birth_links_the_child_to_its_live_parent() {
        let store = test_store();
        let parent_id = "T-in-process-parent-live";
        let child_id = "T-in-process-child-live";
        store
            .create_thread_for_test(&thread_record(parent_id, parent_id))
            .expect("parent thread");
        let owner = store
            .register_in_process_handler(child_id)
            .expect("register in-process child owner");
        let _birth = store
            .create_in_process_child_with_events_and_launch_metadata(
                &in_process_child_record(child_id, parent_id),
                in_process_child_birth_events(),
                &in_process_launch_metadata(),
                &owner,
            )
            .expect("publish in-process child");

        assert_eq!(
            store
                .descendant_thread_ids(parent_id)
                .expect("read parent descendants"),
            [child_id]
        );
        assert_eq!(
            in_process_reservation_phase(&store, child_id),
            Some(runtime_db::InProcessHandlerReservationPhase::Running)
        );
        assert_eq!(
            store
                .in_process_handler_chain_root_id(child_id)
                .expect("child runtime row names its chain"),
            parent_id
        );
        // The birth's own link is replayed exactly, with no stop to inherit.
        assert_eq!(
            store
                .record_child_link(parent_id, child_id, "dispatch")
                .expect("exact replay of the birth link"),
            None
        );
        let error = store
            .record_child_link("T-some-other-parent", child_id, "dispatch")
            .expect_err("an in-process member is linked only to its upstream");
        assert!(error.to_string().contains("was born under"));
        let child = store
            .get_thread(child_id)
            .expect("read child")
            .expect("child row");
        assert_eq!(child.status, ThreadStatus::Running.as_str());
        assert_eq!(child.runtime.stop_intent, None);
        assert!(
            store
                .unregister_in_process_handler(child_id, &owner)
                .expect("unregister in-process child owner")
        );
    }

    fn birth_in_process_child(
        store: &StateStore,
        parent_id: &str,
        child_id: &str,
    ) -> InProcessHandlerControl {
        birth_unspawned_in_process_child(store, parent_id, child_id).0
    }

    fn birth_unspawned_in_process_child(
        store: &StateStore,
        parent_id: &str,
        child_id: &str,
    ) -> (InProcessHandlerControl, UnspawnedInProcessChild) {
        let owner = store
            .register_in_process_handler(child_id)
            .expect("register in-process child owner");
        let (_, unspawned) = store
            .create_in_process_child_with_events_and_launch_metadata(
                &in_process_child_record(child_id, parent_id),
                in_process_child_birth_events(),
                &in_process_launch_metadata(),
                &owner,
            )
            .expect("publish in-process child");
        (owner, unspawned)
    }

    #[test]
    fn recorded_thread_placement_proves_in_process_lineage() {
        let store = test_store();
        let parent_id = "T-placement-parent";
        let child_id = "T-placement-inline-child";
        let plain_member_id = "T-placement-plain-member";
        store
            .create_thread_for_test(&thread_record(parent_id, parent_id))
            .expect("parent thread");
        let owner = birth_in_process_child(&store, parent_id, child_id);
        store
            .create_thread_for_test(&in_process_child_record(plain_member_id, parent_id))
            .expect("plain chain member");

        assert_eq!(
            store
                .recorded_thread_placement(parent_id)
                .expect("place root"),
            RecordedThreadPlacement::Root
        );
        assert_eq!(
            store
                .recorded_thread_placement(child_id)
                .expect("place inline child"),
            RecordedThreadPlacement::InProcessMember {
                chain_root_id: parent_id.to_owned(),
                upstream_thread_id: parent_id.to_owned(),
            }
        );
        assert_eq!(
            store
                .recorded_thread_placement(plain_member_id)
                .expect("place plain member"),
            RecordedThreadPlacement::OtherMember {
                chain_root_id: parent_id.to_owned(),
            }
        );
        assert!(store.recorded_thread_placement("T-absent").is_err());
        assert!(
            store
                .unregister_in_process_handler(child_id, &owner)
                .expect("unregister in-process child owner")
        );
    }

    #[test]
    fn in_process_child_link_replay_refuses_once_shutdown_closed_admission() {
        let store = test_store();
        let parent_id = "T-in-process-parent-shutdown";
        let child_id = "T-in-process-child-shutdown";
        store
            .create_thread_for_test(&thread_record(parent_id, parent_id))
            .expect("parent thread");
        let (owner, unspawned) = birth_unspawned_in_process_child(&store, parent_id, child_id);
        store
            .close_process_attachment_admission()
            .expect("close shutdown admission");

        let error = store
            .link_unspawned_in_process_child(unspawned)
            .expect_err("a closed gate must refuse the pre-spawn replay");
        assert!(
            error.to_string().contains("closed for daemon shutdown"),
            "unexpected refusal: {error:#}"
        );
        // The owner settles the unstarted child through the closed gate.
        store
            .finalize_in_process_handler_owned(child_id, &owner, &requested_failure())
            .expect("owner settles the unstarted child");
        assert_eq!(
            store
                .get_thread(child_id)
                .expect("read child")
                .expect("child row")
                .status,
            ThreadStatus::Failed.as_str()
        );
        assert!(
            store
                .unregister_in_process_handler(child_id, &owner)
                .expect("unregister in-process child owner")
        );
    }

    #[test]
    fn in_process_child_link_replay_inherits_a_stop_landed_after_birth() {
        let store = test_store();
        let parent_id = "T-in-process-parent-late-stop";
        let child_id = "T-in-process-child-late-stop";
        store
            .create_thread_for_test(&thread_record(parent_id, parent_id))
            .expect("parent thread");
        let (owner, unspawned) = birth_unspawned_in_process_child(&store, parent_id, child_id);
        assert_eq!(
            store
                .get_thread(child_id)
                .expect("read child")
                .expect("child row")
                .runtime
                .stop_intent,
            None
        );
        store
            .request_thread_stop(parent_id, StopIntent::Kill)
            .expect("tombstone parent after the birth");

        // A plain link request is a replay only: without the birth's
        // unspawned witness it never inherits a stop landed after the birth.
        assert_eq!(
            store
                .record_child_link(parent_id, child_id, "dispatch")
                .expect("plain replay after the birth"),
            None
        );
        assert_eq!(
            store
                .link_unspawned_in_process_child(unspawned)
                .expect("witnessed replay before the handler spawns"),
            Some(StopIntent::Kill)
        );
        // Inherited once, the stop is the child's own row and every later
        // replay reports it.
        assert_eq!(
            store
                .record_child_link(parent_id, child_id, "dispatch")
                .expect("replay after inheritance"),
            Some(StopIntent::Kill)
        );
        store
            .finalize_in_process_handler_owned(child_id, &owner, &requested_failure())
            .expect("owner settles the unstarted child");
        assert_eq!(
            store
                .get_thread(child_id)
                .expect("read child")
                .expect("child row")
                .status,
            ThreadStatus::Killed.as_str()
        );
        assert!(
            store
                .unregister_in_process_handler(child_id, &owner)
                .expect("unregister in-process child owner")
        );
    }

    #[test]
    fn in_process_child_born_under_a_stopped_parent_inherits_the_stop() {
        let store = test_store();
        let parent_id = "T-in-process-parent-stopped";
        let child_id = "T-in-process-child-stopped";
        store
            .create_thread_for_test(&thread_record(parent_id, parent_id))
            .expect("parent thread");
        store
            .request_thread_stop(parent_id, StopIntent::Cancel)
            .expect("tombstone parent");
        let owner = store
            .register_in_process_handler(child_id)
            .expect("register in-process child owner");
        let _birth = store
            .create_in_process_child_with_events_and_launch_metadata(
                &in_process_child_record(child_id, parent_id),
                in_process_child_birth_events(),
                &in_process_launch_metadata(),
                &owner,
            )
            .expect("a live parent with a stop still births its child");

        // Link and inherited stop landed together with the birth.
        assert_eq!(
            store
                .descendant_thread_ids(parent_id)
                .expect("read parent descendants"),
            [child_id]
        );
        assert_eq!(
            store
                .get_thread(child_id)
                .expect("read child")
                .expect("child row")
                .runtime
                .stop_intent,
            Some(StopIntent::Cancel)
        );
        // The birth caller's link request reports the inherited stop instead
        // of refusing the in-process child.
        assert_eq!(
            store
                .record_child_link(parent_id, child_id, "dispatch")
                .expect("replay reports the inherited stop"),
            Some(StopIntent::Cancel)
        );
        // Any terminal the owner writes settles to the inherited stop.
        let outcome = store
            .finalize_in_process_handler_owned(child_id, &owner, &requested_failure())
            .expect("owner settles the newborn child");
        assert!(matches!(
            outcome,
            FinalizeIfNonterminalOutcome::Finalized { .. }
        ));
        let child = store
            .get_thread(child_id)
            .expect("read child")
            .expect("child row");
        assert_eq!(child.status, ThreadStatus::Cancelled.as_str());
        assert!(
            store
                .unregister_in_process_handler(child_id, &owner)
                .expect("unregister in-process child owner")
        );
    }

    #[test]
    fn in_process_child_birth_refuses_a_terminal_parent_before_any_write() {
        let store = test_store();
        let parent_id = "T-in-process-parent-terminal";
        let child_id = "T-in-process-child-refused";
        store
            .create_thread_for_test(&thread_record(parent_id, parent_id))
            .expect("parent thread");
        store
            .finalize_thread(
                parent_id,
                &FinalizeThreadRecord {
                    status: ThreadStatus::Completed.as_str().to_string(),
                    outcome_code: Some(ThreadStatus::Completed.as_str().to_string()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .expect("complete parent");
        let owner = store
            .register_in_process_handler(child_id)
            .expect("register in-process child owner");

        let error = store
            .create_in_process_child_with_events_and_launch_metadata(
                &in_process_child_record(child_id, parent_id),
                in_process_child_birth_events(),
                &in_process_launch_metadata(),
                &owner,
            )
            .expect_err("a terminal parent cannot acquire a live child");
        assert!(
            error.to_string().contains("under terminal parent"),
            "unexpected refusal: {error:#}"
        );
        assert!(!owner.has_committed_birth());
        assert_eq!(in_process_reservation_phase(&store, child_id), None);
        assert!(store.in_process_handler_chain_root_id(child_id).is_err());
        assert!(store.get_thread(child_id).expect("read child").is_none());
        assert!(
            store
                .descendant_thread_ids(parent_id)
                .expect("read parent descendants")
                .is_empty()
        );
        assert!(
            store
                .unregister_in_process_handler(child_id, &owner)
                .expect("unregister in-process child owner")
        );
    }

    #[test]
    fn in_process_child_interrupted_before_its_link_is_relinked_by_reconcile() {
        let store = test_store();
        let parent_id = "T-in-process-parent-crash";
        let child_id = "T-in-process-child-crash";
        store
            .create_thread_for_test(&thread_record(parent_id, parent_id))
            .expect("parent thread");
        let owner = store
            .register_in_process_handler(child_id)
            .expect("register in-process child owner");
        store
            .fixture_create_in_process_child_interrupted_before_link(
                &in_process_child_record(child_id, parent_id),
                in_process_child_birth_events(),
                &in_process_launch_metadata(),
                &owner,
            )
            .expect("publish in-process child without its link");
        assert!(
            store
                .descendant_thread_ids(parent_id)
                .expect("read parent descendants")
                .is_empty()
        );
        assert_eq!(
            in_process_reservation_phase(&store, child_id),
            Some(runtime_db::InProcessHandlerReservationPhase::Pending)
        );
        // The daemon dies: the owner is gone and the parent is stopped before
        // boot reconciliation reaches the published pending birth.
        assert!(
            store
                .unregister_in_process_handler(child_id, &owner)
                .expect("drop in-process child owner")
        );
        store
            .request_thread_stop(parent_id, StopIntent::Kill)
            .expect("tombstone parent");

        store
            .reconcile_in_process_handler_birth_running(child_id)
            .expect("reconcile the published pending birth");
        assert_eq!(
            store
                .descendant_thread_ids(parent_id)
                .expect("read parent descendants"),
            [child_id]
        );
        assert_eq!(
            in_process_reservation_phase(&store, child_id),
            Some(runtime_db::InProcessHandlerReservationPhase::Running)
        );
        // No handler ran for the ownerless member, so it inherits the stop.
        assert_eq!(
            store
                .get_thread(child_id)
                .expect("read child")
                .expect("child row")
                .runtime
                .stop_intent,
            Some(StopIntent::Kill)
        );
    }

    #[test]
    fn terminal_in_process_child_without_its_link_is_relinked_at_settlement() {
        let store = test_store();
        let parent_id = "T-in-process-parent-settle";
        let child_id = "T-in-process-child-settle";
        store
            .create_thread_for_test(&thread_record(parent_id, parent_id))
            .expect("parent thread");
        let owner = store
            .register_in_process_handler(child_id)
            .expect("register in-process child owner");
        store
            .fixture_create_in_process_child_interrupted_before_link(
                &in_process_child_record(child_id, parent_id),
                in_process_child_birth_events(),
                &in_process_launch_metadata(),
                &owner,
            )
            .expect("publish in-process child without its link");
        store
            .finalize_in_process_handler_owned(child_id, &owner, &requested_failure())
            .expect("owner settles its child");
        assert!(
            store
                .descendant_thread_ids(parent_id)
                .expect("read parent descendants")
                .is_empty()
        );

        assert!(
            store
                .settle_in_process_handler_reservation_owned(child_id, &owner)
                .expect("settle terminal reservation")
        );
        assert_eq!(
            store
                .descendant_thread_ids(parent_id)
                .expect("read parent descendants"),
            [child_id]
        );
        let child = store
            .get_thread(child_id)
            .expect("read child")
            .expect("child row");
        assert_eq!(child.status, ThreadStatus::Failed.as_str());
        assert_eq!(child.runtime.stop_intent, None);
        assert!(
            store
                .unregister_in_process_handler(child_id, &owner)
                .expect("unregister in-process child owner")
        );
    }

    #[test]
    fn trace_branch_does_not_project_ordinary_upstream_edge() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-root", "T-root"))
            .expect("root thread");

        let branch = thread_record("T-branch", "T-root");
        let persisted = store
            .create_trace_branch(
                &branch,
                json!({
                    "relation": "trace_branch",
                    "child_thread_id": "T-branch",
                    "parent_event_ref": {"chain_root_id": "T-root"},
                    "state_anchor_ref": {"chain_root_id": "T-root"}
                }),
            )
            .expect("trace branch");

        let child = store
            .get_thread("T-branch")
            .expect("get branch")
            .expect("branch thread");
        assert_eq!(child.upstream_thread_id, None);
        assert!(store.list_chain_edges("T-root").expect("edges").is_empty());
        assert_eq!(persisted.len(), 2);
        assert_eq!(
            persisted[1].event_type,
            ryeos_state::event_types::EDGE_RECORDED
        );
        assert_eq!(persisted[1].payload["relation"], json!("trace_branch"));
    }

    #[test]
    fn trace_branch_duplicate_explicit_child_id_does_not_append_events() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-root", "T-root"))
            .expect("root thread");

        let branch = thread_record("T-branch", "T-root");
        store
            .create_trace_branch(
                &branch,
                json!({
                    "relation": "trace_branch",
                    "child_thread_id": "T-branch",
                    "parent_event_ref": {"chain_root_id": "T-root"},
                    "state_anchor_ref": {"chain_root_id": "T-root"}
                }),
            )
            .expect("trace branch");

        let head_after_first = {
            let g = store.lock().expect("lock");
            g.state_db
                .read_generic_head_ref("chains", "T-root")
                .expect("read chain head")
                .expect("chain head")
                .target_hash
        };

        let err = store
            .create_trace_branch(
                &branch,
                json!({
                    "relation": "trace_branch",
                    "child_thread_id": "T-branch",
                    "parent_event_ref": {"chain_root_id": "T-root"},
                    "state_anchor_ref": {"chain_root_id": "T-root"}
                }),
            )
            .expect_err("duplicate child id should fail");

        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err:#}"
        );

        let head_after_duplicate = {
            let g = store.lock().expect("lock");
            g.state_db
                .read_generic_head_ref("chains", "T-root")
                .expect("read chain head")
                .expect("chain head")
                .target_hash
        };
        assert_eq!(head_after_duplicate, head_after_first);

        let events = store
            .replay_events("T-root", Some("T-branch"), None, 10, 1024 * 1024)
            .expect("branch replay");
        assert_eq!(events.events.len(), 2);
        assert_eq!(
            events.events[0].event_type,
            ryeos_state::event_types::THREAD_CREATED
        );
        assert_eq!(
            events.events[1].event_type,
            ryeos_state::event_types::EDGE_RECORDED
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn j4_no_birth_funding_receipt_requires_same_actual_original_scratch_owner() {
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let (scratch, original) = scratch_reservation_fixture(&store, "j4-no-birth-funding");
        store
            .reserve_process_resource_launch_with_scratch(&original, &scratch)
            .unwrap();
        let receipt = store
            .remove_never_born_scratch_for_funding(&original, &scratch)
            .unwrap();
        assert!(
            store
                .begin_trusted_process_resource_spawn(&original)
                .is_err()
        );
        receipt
            .matches_original(&Arc::clone(&store), &original)
            .unwrap();
        assert!(
            store
                .process_resource_reservation(&original.owner_kind, &original.owner_coordinate)
                .unwrap()
                .is_some()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn j4_no_birth_funding_receipt_refuses_other_state_and_changed_original() {
        for foreign_state in [false, true] {
            let store_owner = test_store();
            let store = Arc::clone(store_owner.store());
            let (scratch, original) = scratch_reservation_fixture(&store, "j4-no-birth-foreign");
            store
                .reserve_process_resource_launch_with_scratch(&original, &scratch)
                .unwrap();
            let receipt = store
                .remove_never_born_scratch_for_funding(&original, &scratch)
                .unwrap();
            let mut changed = original.clone();
            if !foreign_state {
                changed.owner_coordinate.push_str("-changed");
            }
            // One Store is live per process, so the other Store is its
            // identity alone: an owner that is not the minting Store.
            let refused = if foreign_state {
                receipt.matches_original_of(&std::sync::Weak::new(), &changed)
            } else {
                receipt.matches_original(&store, &changed)
            };
            assert!(refused.is_err());
            drop(store);
            store_owner.close().unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn j4_spawn_intent_cannot_mint_no_birth_funding_receipt() {
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let (scratch, original) = scratch_reservation_fixture(&store, "j4-contacted-no-refund");
        store
            .reserve_process_resource_launch_with_scratch(&original, &scratch)
            .unwrap();
        let permit = store
            .begin_trusted_process_resource_spawn(&original)
            .unwrap();
        assert!(
            store
                .remove_never_born_scratch_for_funding(permit.reservation(), &scratch)
                .is_err()
        );
        assert!(scratch.path().unwrap().exists());
        assert_eq!(
            store
                .process_resource_reservation(&original.owner_kind, &original.owner_coordinate)
                .unwrap()
                .as_ref(),
            Some(permit.reservation())
        );
    }
}

#[cfg(test)]
#[path = "state_store/qualification_transcript_publication_tests.rs"]
mod qualification_transcript_publication_tests;

#[cfg(test)]
#[path = "state_store/seat_cut_tests.rs"]
mod seat_cut_tests;
