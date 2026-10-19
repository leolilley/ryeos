//! Maintenance mode GC executor.
//!
//! Runs GC with the write barrier quiesced, ensuring no concurrent CAS
//! writes during garbage collection. Used by the daemon for online GC.
//!
//! Flow:
//! 1. Acquire the GC run lock
//! 2. Acquire the cross-process CAS mutation guard on a blocking worker
//! 3. Quiesce the in-process write barrier
//! 4. Run GC on that same guarded worker and resume writes
//!
//! If quiesce times out, GC is aborted without running.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};

use ryeos_state::gc::{self, GcParams, GcResult};
use ryeos_state::object_closure::EventPayloadContractRegistration;

use ryeos_app::object_contracts::RetainedEvidenceValidator;
use ryeos_app::retention_exclusion::RetentionExclusion;
use ryeos_app::runtime_db::SeatLeaseMaintenancePage;
use ryeos_app::scheduler_boundary::SchedulerRetentionGuard;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{
    FencedChainRetirementPlan, NodeIdentitySigner, StateStore, TerminalChainRetirement,
};

/// Default quiesce timeout: 30 seconds.
const DEFAULT_QUIESCE_TIMEOUT: Duration = Duration::from_secs(30);

/// Seat lease rows examined per maintenance slice. Within the store bound.
const SEAT_LEASE_SLICE: usize = 64;

/// A composition's decision for one fenced chain root: `None` leaves the
/// chain for a later pass, a plan retires it.
pub type FencedChainQualifier =
    Arc<dyn Fn(&str) -> Result<Option<FencedChainRetirementPlan>> + Send + Sync>;

/// A composition's settlement of seat leases that are already claimed for
/// reaping. It runs under the exclusive retention exclusion, receives the
/// claimed seat roots, and returns how many it fully settled (the rest stay
/// claimed and are offered again by a later pass).
pub type ClaimedSeatSettler =
    Arc<dyn Fn(&AppState, &RetentionExclusion<'_>, &[String]) -> Result<usize> + Send + Sync>;

/// Everything a domain composition supplies to the retention paths, through
/// the one injection point the daemon root calls
/// ([`install_retention_composition`]). The substrate never interprets any of
/// it: the contracts and validator decide what retained evidence means, the
/// qualifier decides which fenced chains retire, the settler decides when a
/// claimed seat is finished.
pub struct RetentionComposition {
    pub event_contracts: Vec<EventPayloadContractRegistration>,
    pub evidence_validator: RetainedEvidenceValidator,
    pub qualify: FencedChainQualifier,
    pub settle_claimed_seats: ClaimedSeatSettler,
}

/// The installed hooks the retention paths read.
pub(crate) struct InstalledRetention {
    pub(crate) qualify: FencedChainQualifier,
    pub(crate) settle_claimed_seats: ClaimedSeatSettler,
}

/// `Some` when a composition was installed, `None` when the root installed
/// the plain object contracts alone.
static INSTALLED: OnceLock<Option<InstalledRetention>> = OnceLock::new();

/// The composition root's single retention installation, called once before
/// readiness. With a composition it installs the event contracts and the
/// retained-evidence validator together with the hooks maintenance reads;
/// with `None` it installs the plain object contracts only, no validator and
/// no hooks, so retirement of fenced kinds refuses rather than runs on a
/// stand-in. A second installation is refused.
pub fn install_retention_composition(composition: Option<RetentionComposition>) -> Result<()> {
    let installed = match composition {
        None => {
            ryeos_app::object_contracts::install()
                .context("install application object contracts")?;
            None
        }
        Some(composition) => {
            let RetentionComposition {
                event_contracts,
                evidence_validator,
                qualify,
                settle_claimed_seats,
            } = composition;
            anyhow::ensure!(
                !event_contracts.is_empty(),
                "a retention composition must supply its event contracts"
            );
            ryeos_app::object_contracts::install_with_event_contracts(event_contracts)
                .context("install application object and event contracts")?;
            ryeos_app::object_contracts::install_retained_evidence_validator(evidence_validator)
                .context("install retained-evidence validator")?;
            Some(InstalledRetention {
                qualify,
                settle_claimed_seats,
            })
        }
    };
    INSTALLED
        .set(installed)
        .map_err(|_| anyhow::anyhow!("the retention composition was already installed"))
}

fn installed_retention() -> Option<&'static InstalledRetention> {
    INSTALLED.get().and_then(Option::as_ref)
}

/// One mechanical step of the retention phase, recorded in the order it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetentionStep {
    Replay,
    FencedRetirement,
    FencedRetirementRefused,
    OrdinaryRetirement,
    RecoverPending,
    SeatReaping,
}

#[derive(Debug, Default)]
pub(crate) struct RetentionPhaseReport {
    pub(crate) retirement: TerminalChainRetirement,
    pub(crate) reaped_seats: usize,
    /// Due fenced chains left alone because no composition is installed.
    pub(crate) refused_fenced_chains: usize,
    pub(crate) steps: Vec<RetentionStep>,
}

pub(crate) struct RetentionPhaseParams<'a> {
    pub(crate) now: &'a str,
    pub(crate) dry_run: bool,
    /// Retire due chain history (ordinary and fenced kinds).
    pub(crate) retire_history: bool,
    /// Lease-expiry cutoff when seat reaping is requested.
    pub(crate) seat_cutoff_ms: Option<i64>,
}

fn add_retirement(total: &mut TerminalChainRetirement, part: &TerminalChainRetirement) {
    total.candidate_chains += part.candidate_chains;
    total.retired_chains += part.retired_chains;
    total.deleted_projection_rows += part.deleted_projection_rows;
    total.deleted_runtime_rows += part.deleted_runtime_rows;
    total.deleted_runtime_files += part.deleted_runtime_files;
    total.pending_retirements_recovered += part.pending_retirements_recovered;
}

/// The retention work that runs under the exclusive exclusion, in its one
/// order: interrupted fenced retirements replay, due fenced chains retire,
/// ordinary history retires, pending removals recover, expired seats reap.
/// The release pass and the sweep follow under the CAS guard.
pub(crate) fn run_retention_phase(
    state: &AppState,
    exclusion: &RetentionExclusion<'_>,
    hooks: Option<&InstalledRetention>,
    params: &RetentionPhaseParams<'_>,
) -> Result<RetentionPhaseReport> {
    exclusion.require_exclusive()?;
    let pin_count = |thread_ids: &[String]| exclusion.count_thread_pins(thread_ids);
    let mut report = RetentionPhaseReport::default();
    if !params.dry_run {
        state.threads.replay_fenced_chain_retirements(exclusion)?;
        report.steps.push(RetentionStep::Replay);
    }
    if params.retire_history {
        match hooks {
            Some(hooks) => {
                let fenced = state.threads.retire_due_fenced_chains(
                    exclusion,
                    params.now,
                    params.dry_run,
                    |root| (hooks.qualify)(root),
                )?;
                add_retirement(&mut report.retirement, &fenced);
                report.steps.push(RetentionStep::FencedRetirement);
            }
            None => {
                // Counting due chains is a dry pass that qualifies nothing.
                let due = state
                    .threads
                    .retire_due_fenced_chains(exclusion, params.now, true, |_| Ok(None))?
                    .candidate_chains;
                report.refused_fenced_chains = due;
                if due != 0 {
                    tracing::warn!(
                        due,
                        "fenced chain retirement refused: no composition installed a \
                         retained-evidence validator and qualifier"
                    );
                }
                report.steps.push(RetentionStep::FencedRetirementRefused);
            }
        }
        let ordinary =
            state
                .state_store
                .retire_due_terminal_chains(params.now, params.dry_run, pin_count)?;
        add_retirement(&mut report.retirement, &ordinary);
        report.steps.push(RetentionStep::OrdinaryRetirement);
    }
    let recovered = state.state_store.recover_pending_terminal_chain_removals(
        params.now,
        params.dry_run,
        pin_count,
    )?;
    report.retirement.pending_retirements_recovered += recovered.pending_retirements_recovered;
    report.steps.push(RetentionStep::RecoverPending);
    if let Some(cutoff_ms) = params.seat_cutoff_ms
        && !params.dry_run
    {
        let claimed = claim_expired_seats(&*state.state_store, cutoff_ms)?;
        report.reaped_seats = settle_claimed_seats(state, exclusion, hooks, &claimed)?;
        report.steps.push(RetentionStep::SeatReaping);
    }
    Ok(report)
}

/// The seat-lease slices maintenance walks. A trait so the paging is
/// testable without lease rows, which only a composition can create.
trait SeatLeases {
    fn max_id(&self) -> Result<Option<String>>;
    fn expired_slice(
        &self,
        cutoff_ms: i64,
        after_id: Option<&str>,
        cycle_max_id: &str,
    ) -> Result<SeatLeaseMaintenancePage>;
    fn claim(&self, thread_id: &str, cutoff_ms: i64) -> Result<bool>;
    fn claimed_slice(&self, after_id: Option<&str>, cycle_max_id: &str) -> Result<Vec<String>>;
}

impl SeatLeases for StateStore {
    fn max_id(&self) -> Result<Option<String>> {
        self.max_seat_lease_id()
    }

    fn expired_slice(
        &self,
        cutoff_ms: i64,
        after_id: Option<&str>,
        cycle_max_id: &str,
    ) -> Result<SeatLeaseMaintenancePage> {
        self.expired_seat_leases_slice(cutoff_ms, after_id, cycle_max_id, SEAT_LEASE_SLICE)
    }

    fn claim(&self, thread_id: &str, cutoff_ms: i64) -> Result<bool> {
        self.claim_expired_seat_lease(thread_id, cutoff_ms)
    }

    fn claimed_slice(&self, after_id: Option<&str>, cycle_max_id: &str) -> Result<Vec<String>> {
        self.claimed_seat_leases_slice(after_id, cycle_max_id, SEAT_LEASE_SLICE)
    }
}

/// Claim every expired lease in this cycle: rows up to the highest id seen at
/// the start, so rows created meanwhile wait for the next cycle. Claiming
/// marks rather than deletes the lease: a failed settlement stays retryable
/// and a racing heartbeat cannot resurrect it.
fn claim_expired_seats(leases: &impl SeatLeases, cutoff_ms: i64) -> Result<Vec<String>> {
    let Some(cycle_max_id) = leases.max_id()? else {
        return Ok(Vec::new());
    };
    let mut claimed = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = leases.expired_slice(cutoff_ms, after.as_deref(), &cycle_max_id)?;
        for thread_id in page.eligible_ids {
            if leases.claim(&thread_id, cutoff_ms)? {
                claimed.push(thread_id);
            }
        }
        if page.exhausted {
            return Ok(claimed);
        }
        after = page.last_examined_id;
    }
}

/// Every lease already claimed for reaping, as a restart finds them.
fn collect_claimed_seats(leases: &impl SeatLeases) -> Result<Vec<String>> {
    let Some(cycle_max_id) = leases.max_id()? else {
        return Ok(Vec::new());
    };
    let mut claimed = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = leases.claimed_slice(after.as_deref(), &cycle_max_id)?;
        let exhausted = page.len() < SEAT_LEASE_SLICE;
        after = page.last().cloned();
        claimed.extend(page);
        if exhausted {
            return Ok(claimed);
        }
    }
}

/// Settle claimed leases through the composition's settler when one is
/// installed, otherwise by the plain lease contract: a claimed lease whose
/// thread is gone or no longer running is removed, a running one is completed
/// and then removed.
fn settle_claimed_seats(
    state: &AppState,
    exclusion: &RetentionExclusion<'_>,
    hooks: Option<&InstalledRetention>,
    claimed: &[String],
) -> Result<usize> {
    if claimed.is_empty() {
        return Ok(0);
    }
    if let Some(hooks) = hooks {
        return (hooks.settle_claimed_seats)(state, exclusion, claimed);
    }
    let mut reaped = 0usize;
    for thread_id in claimed {
        let Some(detail) = state.state_store.get_thread(thread_id)? else {
            state.state_store.remove_seat_lease(thread_id)?;
            continue;
        };
        // The claimed lease row is the structural authority that this
        // thread participates in lease expiry. Maintenance must not
        // rediscover that relationship from an authored kind string.
        if detail.status != "running" {
            state.state_store.remove_seat_lease(thread_id)?;
            continue;
        }
        state
            .threads
            .finalize_thread(&ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                thread_id: thread_id.clone(),
                status: "completed".to_string(),
                outcome_code: Some("seat_lease_expired".to_string()),
                result: None,
                error: None,
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            })?;
        state.state_store.remove_seat_lease(&detail.thread_id)?;
        reaped += 1;
    }
    Ok(reaped)
}

/// What startup recovery did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StartupRetentionRecovery {
    pub fences_checked: usize,
    pub heads_unlinked: usize,
    pub pending_retirements_recovered: usize,
    pub seats_settled: usize,
}

/// Startup recovery under the exclusive exclusion, after the pin sources are
/// sealed and the lifecycle service is built, before readiness: replay
/// interrupted fenced retirements, finish pending removals, then offer
/// already-claimed seat leases to the installed settler. `pin_count` is the
/// caller's pin count for pending-removal recovery. Claimed seats are left
/// alone when no composition is installed: nothing then owns their meaning.
pub fn recover_retention_at_startup(
    state: &AppState,
    exclusion: &RetentionExclusion<'_>,
    now: &str,
    pin_count: impl Fn(&[String]) -> Result<u64>,
) -> Result<StartupRetentionRecovery> {
    recover_retention(state, exclusion, installed_retention(), now, pin_count)
}

fn recover_retention(
    state: &AppState,
    exclusion: &RetentionExclusion<'_>,
    hooks: Option<&InstalledRetention>,
    now: &str,
    pin_count: impl Fn(&[String]) -> Result<u64>,
) -> Result<StartupRetentionRecovery> {
    exclusion.require_exclusive()?;
    let replay = state.threads.replay_fenced_chain_retirements(exclusion)?;
    let recovered = state
        .state_store
        .recover_pending_terminal_chain_removals(now, false, pin_count)?;
    let seats_settled = if hooks.is_some() {
        let claimed = collect_claimed_seats(&*state.state_store)?;
        settle_claimed_seats(state, exclusion, hooks, &claimed)?
    } else {
        0
    };
    Ok(StartupRetentionRecovery {
        fences_checked: replay.fences_checked,
        heads_unlinked: replay.heads_unlinked,
        pending_retirements_recovered: recovered.pending_retirements_recovered,
        seats_settled,
    })
}

/// The release pass's view of the fenced chains. A trait so the retry-later
/// behaviour is testable without constructing retired closures.
trait ChainReleaser {
    fn unreleased(&self) -> Result<Vec<String>>;
    fn release(&self, owner: &str) -> Result<()>;
}

struct StoreReleaser<'a> {
    state_store: &'a StateStore,
    authority: &'a ryeos_state::PinnedStateAuthority,
    cas_guard: &'a ryeos_state::CasMutationGuard,
}

impl ChainReleaser for StoreReleaser<'_> {
    fn unreleased(&self) -> Result<Vec<String>> {
        self.state_store
            .with_state_db(|db| Ok(db.unreleased_retired_chains()?.into_keys().collect()))
    }

    fn release(&self, owner: &str) -> Result<()> {
        let lock = self.state_store.with_state_db(|db| {
            db.acquire_existing_chain_lock(owner)
                .with_context(|| format!("lock retired chain {owner}"))
        })?;
        let recovery = self
            .authority
            .recovery()
            .context("the pinned state authority has no recovery store")?;
        recovery.release_retired_chain(&self.authority.cas_store()?, self.cas_guard, &lock, owner)
    }
}

/// What the release pass did: chains released, and owners whose release was
/// refused and is left for the next maintenance run.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ReleaseReport {
    pub(crate) released: usize,
    pub(crate) deferred: Vec<String>,
}

/// Whether a release refusal is one of the two documented not-yet cases the
/// State release reports as plain messages: the owner's head or pending
/// removal still exists, or a counterpart is not yet fenced. Any other error
/// is a real failure.
fn release_not_yet_possible(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.starts_with("cannot release ")
        && (message.ends_with("its head or pending transition still exists")
            || message.contains(" is not fenced"))
}

/// Release every fenced chain's closure. A not-yet-possible release (see
/// [`release_not_yet_possible`]) is retry-later: the fence and closure stay
/// rooted and the refusal is logged. Any other error aborts the run before
/// the sweep. A second pass re-reads the unreleased set, so an obstacle that
/// cleared meanwhile releases in the same run; what still refuses is
/// reported as deferred for the next run.
fn run_release_pass(releaser: &impl ChainReleaser) -> Result<ReleaseReport> {
    let mut released = 0usize;
    let mut deferred = Vec::new();
    for pass in 0..2 {
        deferred.clear();
        for owner in releaser.unreleased()? {
            match releaser.release(&owner) {
                Ok(()) => released += 1,
                Err(error) if release_not_yet_possible(&error) => {
                    tracing::warn!(%owner, pass, %error, "retired chain release deferred");
                    deferred.push(owner);
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("release retired chain {owner}"));
                }
            }
        }
        if deferred.is_empty() {
            break;
        }
    }
    Ok(ReleaseReport { released, deferred })
}

enum GcWorkerCommand {
    Run(WriteBarrierResume),
    Abort,
}

/// Cancellation-safe ownership of the fully quiesced state. If the async GC
/// coordinator is dropped at any await, ordinary writes are immediately
/// re-enabled while dropping the command sender releases the CAS worker.
struct WriteBarrierResume {
    barrier: Arc<ryeos_app::write_barrier::WriteBarrier>,
}

struct GcRunInput<'a> {
    state_authority: &'a ryeos_state::PinnedStateAuthority,
    cas_guard: &'a ryeos_state::CasMutationGuard,
    signer: &'a NodeIdentitySigner,
    params: &'a GcParams,
    external_content_limits:
        ryeos_node_runtime::node_policy::sections::external_content::ExternalContentImportLimits,
    state_store: &'a ryeos_app::state_store::StateStore,
    accounting: Option<&'a ryeos_app::accounting_db::AccountingDb>,
    scheduler: Option<&'a mut dyn SchedulerRetentionGuard>,
    fire_retention: Option<gc::retention::FireRetentionPolicy>,
    reaped_seats: usize,
    terminal_chain_candidates: usize,
    retired_terminal_chains: usize,
    deleted_thread_projection_rows: usize,
    deleted_thread_runtime_rows: usize,
    deleted_thread_runtime_files: usize,
    pending_retirements_recovered: usize,
}

impl Drop for WriteBarrierResume {
    fn drop(&mut self) {
        self.barrier.resume();
    }
}

/// Run maintenance mode GC with write barrier quiesced.
///
/// This is the daemon-side coordinator for online GC. It:
/// 1. Acquires the GC flock lock (prevents concurrent GC runs)
/// 2. Quiesces the write barrier (pauses CAS writes)
/// 3. Runs the GC pipeline on a blocking thread (doesn't stall tokio)
/// 4. Resumes writes
///
/// Returns the GC result on success. If quiesce times out, returns an error
/// without running GC. The write barrier is always resumed on error paths.
#[tracing::instrument(
    name = "maintenance:gc",
    skip(state, params),
    fields(
        dry_run = params.dry_run,
        compact = params.compact,
        deep = params.deep,
    )
)]
pub async fn run_maintenance_gc(state: &AppState, params: &GcParams) -> Result<GcResult> {
    params.validate()?;
    // Validate authored units before acquiring locks or performing any partial
    // cleanup. The storage/query boundary uses signed milliseconds.
    let seat_lease_grace_ms = params
        .seat_lease_grace_seconds
        .map(|seconds| {
            seconds
                .checked_mul(1_000)
                .and_then(|millis| i64::try_from(millis).ok())
                .context("seat_lease_grace_seconds is too large")
        })
        .transpose()?;
    let runtime_state_dir = state.config.runtime_state_dir();
    let fire_retention = gc::retention::FireRetentionPolicy::from_bounds(
        params.schedule_fire_max_age_days,
        params.schedule_fire_max_count,
    );

    // The App retention gate is held exclusively for the whole run, so no
    // other retention path (periodic recovery, settlement) deletes or fences
    // chain state meanwhile. The scheduler, when this composition has one, is
    // a pin source and also owns fire retention; its guard covers pin
    // inspection, CAS-root gathering, portable journal pruning, and
    // projection repair under its own write gate.
    let scheduler = state.scheduler();
    if fire_retention.is_some() && scheduler.is_none() {
        anyhow::bail!("schedule fire retention requires a scheduler in this composition");
    }
    let retention_gate = state
        .acquire_retention_gate(ryeos_app::retention_exclusion::ExclusionMode::Exclusive)
        .await?;
    let mut scheduler_guard = match scheduler {
        Some(scheduler) => Some(scheduler.acquire_retention().await?),
        None => None,
    };
    // Step 1: Acquire GC lock (after the App gate and source gates)
    let _gc_lock = if params.dry_run {
        gc::GcLock::acquire_existing(&runtime_state_dir)
    } else {
        gc::GcLock::acquire(&runtime_state_dir, state.identity.fingerprint())
    }
    .context("failed to acquire GC lock (another GC may be running)")?;

    tracing::info!(
        dry_run = params.dry_run,
        compact = params.compact,
        deep = params.deep,
        "maintenance GC: lock acquired"
    );

    // Retention work runs under the exclusive exclusion and before the write
    // barrier quiesces, because settlement and retirement are ordinary
    // writes. The release pass and the sweep follow under the CAS guard.
    let held_pins: Vec<&dyn ryeos_app::retention_exclusion::ThreadPinGuard> = scheduler_guard
        .iter()
        .map(|guard| {
            let guard: &dyn SchedulerRetentionGuard = &**guard;
            guard as &dyn ryeos_app::retention_exclusion::ThreadPinGuard
        })
        .collect();
    let exclusion = retention_gate.with_held_pins(held_pins)?;
    let seat_cutoff_ms = if params.dry_run {
        None
    } else {
        seat_lease_grace_ms
            .map(|grace_ms| lillux::time::timestamp_millis().saturating_sub(grace_ms))
    };
    let phase = run_retention_phase(
        state,
        &exclusion,
        installed_retention(),
        &RetentionPhaseParams {
            now: &lillux::time::iso8601_now(),
            dry_run: params.dry_run,
            retire_history: params.deep || params.prune_runtime_history,
            seat_cutoff_ms,
        },
    )?;
    let retention_hold = exclusion.into_gate_hold();
    let reaped_seats = phase.reaped_seats;
    let refused_fenced_chains = phase.refused_fenced_chains;
    let terminal_chain_candidates = phase.retirement.candidate_chains;
    let retired_terminal_chains = phase.retirement.retired_chains;
    let deleted_thread_projection_rows = phase.retirement.deleted_projection_rows;
    let deleted_thread_runtime_rows = phase.retirement.deleted_runtime_rows;
    let deleted_thread_runtime_files = phase.retirement.deleted_runtime_files;
    let pending_retirements_recovered = phase.retirement.pending_retirements_recovered;

    // The global mutation order is cross-process CAS guard, then in-process
    // write permit/barrier. A dedicated blocking worker can hold the !Send file
    // guard while the async coordinator waits for active permit holders to
    // drain, without ever carrying that guard across an await.
    let signer = NodeIdentitySigner::from_identity(&state.identity);
    let params_clone = params.clone();
    let external_content_limits = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::external_content::ExternalContentImportPolicyRecord>()?
        .limits
        .clone();
    let state_store = state.state_store.clone();
    let accounting = state.accounting.clone();
    let state_authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())
        .context("capture pinned state authority for GC")?;
    let (guard_ready_tx, guard_ready_rx) =
        tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
    let (command_tx, command_rx) = std::sync::mpsc::sync_channel(1);
    let gc_worker = tokio::task::spawn_blocking(move || -> Result<Option<GcResult>> {
        // Held to the end of the worker so the App gate outlives the sweep.
        let _retention_hold = retention_hold;
        let cas_guard = match state_authority.acquire_exclusive_guard(!params_clone.dry_run) {
            Ok(guard) => guard,
            Err(error) => {
                let _ = guard_ready_tx.send(Err(error.to_string()));
                return Err(error).context("acquire exclusive CAS mutation guard for GC");
            }
        };
        if let Err(error) = state_authority
            .ensure_guard(&cas_guard)
            .context("GC guard does not protect the live pinned state authority")
        {
            let _ = guard_ready_tx.send(Err(error.to_string()));
            return Err(error);
        }
        let _ = guard_ready_tx.send(Ok(()));
        let command = command_rx.recv();
        match command {
            Ok(GcWorkerCommand::Run(_write_barrier_resume)) => {
                // Released closures stop being roots, so release precedes the
                // sweep that may collect them.
                let release = if params_clone.dry_run {
                    ReleaseReport::default()
                } else {
                    run_release_pass(&StoreReleaser {
                        state_store: &state_store,
                        authority: &state_authority,
                        cas_guard: &cas_guard,
                    })?
                };
                if release.released != 0 || !release.deferred.is_empty() {
                    tracing::info!(
                        released = release.released,
                        deferred = release.deferred.len(),
                        "retired chain release pass"
                    );
                }
                run_gc_and_log(GcRunInput {
                    state_authority: &state_authority,
                    cas_guard: &cas_guard,
                    signer: &signer,
                    params: &params_clone,
                    external_content_limits,
                    state_store: &state_store,
                    accounting: accounting.as_deref(),
                    scheduler: match scheduler_guard.as_mut() {
                        Some(guard) => Some(guard.as_mut()),
                        None => None,
                    },
                    fire_retention,
                    reaped_seats,
                    terminal_chain_candidates,
                    retired_terminal_chains,
                    deleted_thread_projection_rows,
                    deleted_thread_runtime_rows,
                    deleted_thread_runtime_files,
                    pending_retirements_recovered,
                })
                .map(|mut result| {
                    result.released_retired_chains = release.released;
                    result.deferred_retired_chain_releases = release.deferred.len();
                    Some(result)
                })
            }
            Ok(GcWorkerCommand::Abort) | Err(_) => Ok(None),
        }
    });
    guard_ready_rx
        .await
        .context("GC guard worker stopped before reporting acquisition")?
        .map_err(anyhow::Error::msg)?;

    if let Err(error) = state
        .write_barrier
        .quiesce(DEFAULT_QUIESCE_TIMEOUT)
        .await
        .context("write barrier quiesce timed out (GC aborted)")
    {
        let _ = command_tx.send(GcWorkerCommand::Abort);
        let _ = gc_worker.await;
        return Err(error);
    }
    let write_barrier_resume = WriteBarrierResume {
        barrier: state.write_barrier.clone(),
    };

    if !params.dry_run {
        let active_sync_jobs = state
            .state_store
            .with_state_db(|db| db.count_active_sync_jobs())
            .context("failed to inspect active sync jobs before GC");
        match active_sync_jobs {
            Ok(0) => {}
            Ok(active_sync_jobs) => {
                let _ = command_tx.send(GcWorkerCommand::Abort);
                let _ = gc_worker.await;
                anyhow::bail!(
                    "GC refused: {active_sync_jobs} active sync job(s) may pin staged CAS roots"
                );
            }
            Err(err) => {
                let _ = command_tx.send(GcWorkerCommand::Abort);
                let _ = gc_worker.await;
                return Err(err);
            }
        }
    }

    command_tx
        .send(GcWorkerCommand::Run(write_barrier_resume))
        .context("GC guard worker stopped before run command")?;
    drop(command_tx);
    let worker_result = gc_worker.await.context("GC blocking task panicked")?;
    let mut gc_result = match worker_result {
        Ok(Some(result)) => result,
        Ok(None) => anyhow::bail!("GC guard worker aborted before running"),
        Err(error) => {
            tracing::error!(
                error = %format!("{error:#}"),
                "maintenance GC pipeline failed"
            );
            return Err(error);
        }
    };
    gc_result.reaped_seats = reaped_seats;
    gc_result.refused_fenced_chain_retirements = refused_fenced_chains;
    gc_result.terminal_chain_candidates = terminal_chain_candidates;
    gc_result.retired_terminal_chains = retired_terminal_chains;
    gc_result.deleted_thread_projection_rows = deleted_thread_projection_rows;
    gc_result.deleted_thread_runtime_rows = deleted_thread_runtime_rows;
    gc_result.deleted_thread_runtime_files = deleted_thread_runtime_files;
    gc_result.pending_retirements_recovered = pending_retirements_recovered;

    Ok(gc_result)
}

/// Run GC and append event log. Called inside `spawn_blocking`.
fn run_gc_and_log(input: GcRunInput<'_>) -> Result<GcResult> {
    let GcRunInput {
        state_authority,
        cas_guard,
        signer,
        params,
        external_content_limits,
        state_store,
        accounting,
        mut scheduler,
        fire_retention,
        reaped_seats,
        terminal_chain_candidates,
        retired_terminal_chains,
        deleted_thread_projection_rows,
        deleted_thread_runtime_rows,
        deleted_thread_runtime_files,
        pending_retirements_recovered,
    } = input;
    let runtime_directory = state_authority.runtime_directory();
    // The worker already owns the exclusive cross-process guard and the daemon
    // write barrier is quiesced. Every unresolved Set/Remove closure remains a
    // temporary root until its compare-acknowledged journal record is removed.
    // Effect-record retention runs before root gathering so a pruned row's
    // object is sweepable in this same pass. The cap is a row count: results
    // are bounded per record, and the CAS bytes follow the rows.
    const MAX_EFFECT_RECORDS: usize = 10_000;
    const MAX_PROVIDER_CALL_RECORDS: usize = 10_000;
    let provider_namespace = ryeos_state::ReplayIndexNamespace::new(
        ryeos_provider_contract::PROVIDER_CALL_REPLAY_NAMESPACE,
    )?;
    let effect_namespace =
        ryeos_state::ReplayIndexNamespace::new(ryeos_effect_contract::EFFECT_REPLAY_NAMESPACE)?;
    let pruned_provider_call_records = state_store
        .with_state_db(|db| db.prune_replay_records(&provider_namespace, MAX_PROVIDER_CALL_RECORDS))
        .context("prune provider call record retention")?;
    if pruned_provider_call_records > 0 {
        tracing::info!(
            pruned_provider_call_records,
            "provider-call-record retention pruned rows"
        );
    }
    let pruned_effect_records = state_store
        .with_state_db(|db| db.prune_replay_records(&effect_namespace, MAX_EFFECT_RECORDS))
        .context("prune stale effect records before CAS sweep")?;
    if pruned_effect_records > 0 {
        tracing::info!(pruned_effect_records, "effect-record retention pruned rows");
    }
    let mut operational_object_roots = state_store
        .with_state_db(|db| db.pending_transition_object_roots())
        .context("read pending chain-head transition roots before CAS sweep")?
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    operational_object_roots.extend(
        state_store
            .active_resume_snapshot_roots()
            .context("failed to collect active runtime snapshot roots")?,
    );
    if let Some(scheduler) = scheduler.as_deref() {
        operational_object_roots.extend(
            scheduler
                .operational_snapshot_roots()
                .context("failed to collect scheduler snapshot roots")?,
        );
    }
    let mirrored_entries = state_store
        .with_state_db(|db| db.list_cas_entries_by_state(ryeos_state::CasEntryState::Mirrored))
        .context("read durable mirrored CAS roots before sweep")?;
    let mut operational_blob_roots = std::collections::BTreeSet::new();
    operational_blob_roots.extend(
        state_store
            .external_execution_blob_roots()
            .context("read retained external candidate evidence roots before sweep")?,
    );
    for entry in mirrored_entries {
        match entry.entry_kind {
            ryeos_state::CasEntryKind::Object => {
                operational_object_roots.insert(entry.hash);
            }
            ryeos_state::CasEntryKind::Blob => {
                operational_blob_roots.insert(entry.hash);
            }
        }
    }
    // Indexed effect records are replay authority: they stay reachable
    // exactly as long as their index rows exist, and retention is row
    // deletion followed by an ordinary sweep.
    operational_object_roots.extend(
        state_store
            .with_state_db(|db| db.list_replay_record_hashes())
            .context("read durable replay record roots before sweep")?,
    );
    if let Some(accounting) = accounting {
        operational_object_roots.extend(
            accounting
                .provider_evidence_object_roots()
                .context("read durable provider evidence roots before sweep")?,
        );
    }
    let operational_roots = gc::AdditionalCasRoots {
        object_hashes: operational_object_roots.into_iter().collect(),
        blob_hashes: operational_blob_roots.into_iter().collect(),
    };
    let mut runtime_cleanup = GcResult::default();
    if fire_retention.is_some() {
        scheduler
            .as_deref_mut()
            .context("fire retention requires a scheduler guard")?
            .prepare_fire_retention(runtime_directory, params.dry_run)
            .context("prepare scheduler fire retention")?;
    }
    let retention_result = (|| -> Result<(Vec<gc::retention::FireRetentionTarget>, usize)> {
        let targets = gc::purge_runtime_state_in_directory(
            runtime_directory,
            params,
            fire_retention,
            &mut runtime_cleanup,
        )
        .context("runtime-state purge failed")?;
        let deleted_projection_rows = if fire_retention.is_some() {
            scheduler
                .as_deref_mut()
                .context("fire retention requires a scheduler guard")?
                .finish_fire_retention(&targets)
                .context("publish exact scheduler fire retention projection")?
        } else {
            targets.len()
        };
        Ok((targets, deleted_projection_rows))
    })();
    let (_, deleted_fire_projection_rows) = match retention_result {
        Ok(result) => result,
        Err(error) if fire_retention.is_some() && !params.dry_run => {
            // Do not release the scheduler gate with an invalid projection. The
            // marker makes the crash path safe; this in-process repair gives a
            // failed maintenance call the same convergence before returning.
            let repair = scheduler
                .context("fire retention requires a scheduler guard")?
                .repair_fire_retention();
            match repair {
                Ok(()) => return Err(error),
                Err(repair_error) => {
                    anyhow::bail!(
                        "{error:#}; scheduler retention repair also failed: {repair_error:#}"
                    )
                }
            }
        }
        Err(error) => return Err(error),
    };

    // Materialized project generations are executor-owned cache. Deep GC
    // reclaims them through their exact construction/lease protocol while the
    // daemon write barrier and cross-process CAS guard are held; the generic
    // state-layer cache purge deliberately does not guess at this liveness.
    let materialization_cleanup = if params.deep {
        ryeos_executor::execution::cache::MaterializationCache::from_runtime_state(
            runtime_directory.path(),
        )
        .prune_inactive_generations(params.dry_run)
        .context("lease-aware materialized snapshot cache purge failed")?
    } else {
        ryeos_executor::execution::cache::MaterializationPruneReport::default()
    };

    let run_gc = || {
        gc::run_gc_with_pinned_authority(
            state_authority,
            cas_guard,
            Some(signer),
            params,
            &operational_roots,
        )
        .context("GC pipeline failed")
    };
    let mut result = if params.compact && !params.dry_run {
        state_store.with_project_head_compaction_authority(run_gc)?
    } else {
        run_gc()?
    };
    let limits = external_content_limits;
    let store = if params.dry_run {
        ryeos_state::LargeObjectStore::open_under(state_authority.runtime_directory())?
    } else {
        Some(state_authority.large_object_store()?)
    };
    if let Some(store) = store {
        let capacity = store.filesystem_capacity()?;
        let stored_bytes = store.total_stored_bytes()?;
        let reserve_shortfall = limits
            .minimum_free_bytes
            .saturating_sub(capacity.available_bytes);
        let reserve_budget = stored_bytes.saturating_sub(reserve_shortfall);
        let effective_budget = limits.store_budget_bytes.min(reserve_budget);
        let sweep = store.sweep_to_budget_with_mode(
            effective_budget,
            &result.reachable_large_object_hashes,
            params.dry_run,
        )?;
        let staging = store.sweep_abandoned_staging_with_mode(params.dry_run)?;
        result.inspected_large_objects = sweep.inspected_objects;
        result.deleted_large_objects = sweep.evicted.len();
        result.deleted_large_object_staging_files = staging.files;
        result.preserved_rooted_large_objects = sweep.retained_roots;
        result.preserved_leased_large_objects = sweep.retained_leased;
        result.freed_large_object_bytes = sweep
            .evicted
            .iter()
            .fold(staging.bytes, |total, (_, bytes)| {
                total.saturating_add(*bytes)
            });
        result.freed_bytes = result
            .freed_bytes
            .saturating_add(result.freed_large_object_bytes);
    }
    result.deleted_runtime_files = runtime_cleanup
        .deleted_runtime_files
        .saturating_add(materialization_cleanup.reclaimable_files);
    result.deleted_fire_records = runtime_cleanup.deleted_fire_records;
    result.deleted_fire_projection_rows = deleted_fire_projection_rows;
    result.freed_bytes = result
        .freed_bytes
        .saturating_add(runtime_cleanup.freed_bytes)
        .saturating_add(materialization_cleanup.reclaimable_bytes);
    result.inspected_materialized_snapshots = materialization_cleanup.inspected_generations;
    result.deleted_materialized_snapshots = materialization_cleanup.reclaimable_generations;
    result.preserved_active_materialized_snapshots = materialization_cleanup.active_generations;
    result.reaped_seats = reaped_seats;
    result.terminal_chain_candidates = terminal_chain_candidates;
    result.retired_terminal_chains = retired_terminal_chains;
    result.deleted_thread_projection_rows = deleted_thread_projection_rows;
    result.deleted_thread_runtime_rows = deleted_thread_runtime_rows;
    result.deleted_thread_runtime_files = deleted_thread_runtime_files;
    result.pending_retirements_recovered = pending_retirements_recovered;

    // Retention: retire old terminal sync-job rows only when the invocation
    // explicitly authors a window. Rust supplies no fallback age. The write
    // barrier is quiesced here, so no concurrent sync-job writes race delete.
    if !params.dry_run
        && let Some(retention_days) = params.sync_job_retention_days
    {
        let cutoff = gc::retention::iso8601_days_ago(retention_days);
        let (jobs, attempts) = state_store
            .with_state_db(|db| db.delete_terminal_sync_jobs_before(&cutoff))
            .context("explicit sync-job retention sweep failed")?;
        result.deleted_sync_jobs = jobs;
        result.deleted_sync_job_attempts = attempts;
        if jobs > 0 {
            tracing::info!(
                deleted_sync_jobs = jobs,
                deleted_sync_job_attempts = attempts,
                cutoff = %cutoff,
                "retention: retired terminal sync jobs"
            );
        }
    }

    // Dry-run is mutation-free across authoritative and operational stores;
    // even the ordinary best-effort audit append is therefore suppressed.
    if !params.dry_run
        && let Err(err) = gc::event_log::append_event_in_directory(
            runtime_directory,
            &gc::event_log::GcEvent {
                timestamp: lillux::time::iso8601_now(),
                dry_run: params.dry_run,
                compact: params.compact,
                roots_walked: result.roots_walked,
                reachable_objects: result.reachable_objects,
                reachable_blobs: result.reachable_blobs,
                reachable_large_objects: result.reachable_large_objects,
                deleted_objects: result.deleted_objects,
                deleted_blobs: result.deleted_blobs,
                inspected_large_objects: result.inspected_large_objects,
                deleted_large_objects: result.deleted_large_objects,
                deleted_large_object_staging_files: result.deleted_large_object_staging_files,
                preserved_rooted_large_objects: result.preserved_rooted_large_objects,
                preserved_leased_large_objects: result.preserved_leased_large_objects,
                freed_large_object_bytes: result.freed_large_object_bytes,
                deleted_cas_staging_files: result.deleted_cas_staging_files,
                deleted_runtime_files: result.deleted_runtime_files,
                deleted_fire_records: result.deleted_fire_records,
                deleted_fire_projection_rows: result.deleted_fire_projection_rows,
                deleted_sync_jobs: result.deleted_sync_jobs,
                deleted_sync_job_attempts: result.deleted_sync_job_attempts,
                reaped_seats: result.reaped_seats,
                terminal_chain_candidates: result.terminal_chain_candidates,
                retired_terminal_chains: result.retired_terminal_chains,
                deleted_thread_projection_rows: result.deleted_thread_projection_rows,
                deleted_thread_runtime_rows: result.deleted_thread_runtime_rows,
                deleted_thread_runtime_files: result.deleted_thread_runtime_files,
                pending_retirements_recovered: result.pending_retirements_recovered,
                retired_durable_cas_uploads: result.retired_durable_cas_uploads,
                inspected_materialized_snapshots: result.inspected_materialized_snapshots,
                deleted_materialized_snapshots: result.deleted_materialized_snapshots,
                preserved_active_materialized_snapshots: result
                    .preserved_active_materialized_snapshots,
                freed_bytes: result.freed_bytes,
                snapshots_compacted: result
                    .compaction
                    .as_ref()
                    .map(|c| c.snapshots_removed)
                    .unwrap_or(0),
                duration_ms: result.duration_ms,
            },
        )
    {
        tracing::warn!(error = %err, "failed to append GC event log");
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;

    #[test]
    fn quiesce_timeout_is_reasonable() {
        // Verify the default timeout is reasonable for production use
        assert!(DEFAULT_QUIESCE_TIMEOUT >= Duration::from_secs(10));
        assert!(DEFAULT_QUIESCE_TIMEOUT <= Duration::from_secs(120));
    }

    struct FakeReleaser {
        unreleased: RefCell<BTreeSet<String>>,
        /// Releases each owner refuses before it succeeds (`u32::MAX` never).
        refusals: RefCell<BTreeMap<String, u32>>,
        released: RefCell<Vec<String>>,
        listing_fails: bool,
        /// Owners whose release fails for a reason that is not "not yet".
        hard_failures: BTreeSet<String>,
        /// Refuse with the counterpart wording instead of the head wording.
        counterpart_wording: bool,
    }

    impl FakeReleaser {
        fn new(owners: &[(&str, u32)]) -> Self {
            Self {
                unreleased: RefCell::new(owners.iter().map(|o| o.0.to_string()).collect()),
                refusals: RefCell::new(owners.iter().map(|o| (o.0.to_string(), o.1)).collect()),
                released: RefCell::new(Vec::new()),
                listing_fails: false,
                hard_failures: BTreeSet::new(),
                counterpart_wording: false,
            }
        }
    }

    impl ChainReleaser for FakeReleaser {
        fn unreleased(&self) -> Result<Vec<String>> {
            anyhow::ensure!(!self.listing_fails, "fence listing unavailable");
            Ok(self.unreleased.borrow().iter().cloned().collect())
        }

        fn release(&self, owner: &str) -> Result<()> {
            anyhow::ensure!(
                !self.hard_failures.contains(owner),
                "retired chain manifest is corrupt"
            );
            let mut refusals = self.refusals.borrow_mut();
            let remaining = refusals.get_mut(owner).unwrap();
            if *remaining > 0 {
                if *remaining != u32::MAX {
                    *remaining -= 1;
                }
                if self.counterpart_wording {
                    anyhow::bail!("cannot release {owner:?}: counterpart \"T-peer\" is not fenced");
                }
                anyhow::bail!(
                    "cannot release {owner:?}: its head or pending transition still exists"
                );
            }
            self.unreleased.borrow_mut().remove(owner);
            self.released.borrow_mut().push(owner.to_string());
            Ok(())
        }
    }

    #[test]
    fn release_pass_releases_every_unreleased_chain() {
        let releaser = FakeReleaser::new(&[("T-a", 0), ("T-b", 0)]);
        let report = run_release_pass(&releaser).unwrap();
        assert_eq!(
            report,
            ReleaseReport {
                released: 2,
                deferred: Vec::new()
            }
        );
        assert_eq!(*releaser.released.borrow(), ["T-a", "T-b"]);
    }

    #[test]
    fn release_pass_retries_a_refused_chain_once_in_the_same_run() {
        let releaser = FakeReleaser::new(&[("T-a", 1), ("T-b", 0)]);
        let report = run_release_pass(&releaser).unwrap();
        assert_eq!(report.released, 2);
        assert!(report.deferred.is_empty());
        assert_eq!(*releaser.released.borrow(), ["T-b", "T-a"]);
    }

    #[test]
    fn release_pass_defers_a_chain_that_keeps_refusing_and_releases_the_rest() {
        let releaser = FakeReleaser::new(&[("T-a", u32::MAX), ("T-b", 0)]);
        let report = run_release_pass(&releaser).unwrap();
        assert_eq!(report.released, 1);
        assert_eq!(report.deferred, ["T-a"]);
        assert!(releaser.unreleased.borrow().contains("T-a"));
    }

    #[test]
    fn release_pass_defers_a_counterpart_not_yet_fenced() {
        let mut releaser = FakeReleaser::new(&[("T-a", u32::MAX)]);
        releaser.counterpart_wording = true;
        let report = run_release_pass(&releaser).unwrap();
        assert_eq!(report.deferred, ["T-a"]);
    }

    #[test]
    fn release_pass_propagates_an_error_that_is_not_a_documented_not_yet_case() {
        let mut releaser = FakeReleaser::new(&[("T-a", 0), ("T-b", 0)]);
        releaser.hard_failures.insert("T-a".to_string());
        let error = run_release_pass(&releaser).unwrap_err();
        assert!(format!("{error:#}").contains("manifest is corrupt"));
        assert!(releaser.released.borrow().is_empty());
    }

    #[test]
    fn release_classification_matches_only_the_two_documented_messages() {
        let not_yet = |m: &str| release_not_yet_possible(&anyhow::anyhow!("{m}"));
        assert!(not_yet(
            "cannot release \"T-a\": its head or pending transition still exists"
        ));
        assert!(not_yet(
            "cannot release \"T-a\": counterpart \"T-b\" is not fenced"
        ));
        assert!(!not_yet("chain \"T-a\" has no retirement fence to release"));
        assert!(!not_yet("retired chain manifest is corrupt"));
    }

    #[test]
    fn release_pass_with_nothing_unreleased_does_nothing() {
        let report = run_release_pass(&FakeReleaser::new(&[])).unwrap();
        assert_eq!(report, ReleaseReport::default());
    }

    #[test]
    fn release_pass_fails_when_the_fences_cannot_be_listed() {
        let mut releaser = FakeReleaser::new(&[("T-a", 0)]);
        releaser.listing_fails = true;
        assert!(run_release_pass(&releaser).is_err());
    }

    struct FakeSeats {
        /// (id, last_seen_ms, claimed), sorted by id.
        rows: RefCell<Vec<(String, i64, bool)>>,
    }

    impl FakeSeats {
        fn new(rows: &[(&str, i64, bool)]) -> Self {
            let mut rows: Vec<_> = rows.iter().map(|r| (r.0.to_string(), r.1, r.2)).collect();
            rows.sort();
            Self {
                rows: RefCell::new(rows),
            }
        }

        fn claimed_ids(&self) -> Vec<String> {
            self.rows
                .borrow()
                .iter()
                .filter(|r| r.2)
                .map(|r| r.0.clone())
                .collect()
        }
    }

    impl SeatLeases for FakeSeats {
        fn max_id(&self) -> Result<Option<String>> {
            Ok(self.rows.borrow().last().map(|r| r.0.clone()))
        }

        fn expired_slice(
            &self,
            cutoff_ms: i64,
            after_id: Option<&str>,
            cycle_max_id: &str,
        ) -> Result<SeatLeaseMaintenancePage> {
            let rows: Vec<_> = self
                .rows
                .borrow()
                .iter()
                .filter(|r| {
                    after_id.is_none_or(|a| r.0.as_str() > a) && r.0.as_str() <= cycle_max_id
                })
                .take(SEAT_LEASE_SLICE)
                .cloned()
                .collect();
            Ok(SeatLeaseMaintenancePage {
                exhausted: rows.len() < SEAT_LEASE_SLICE,
                last_examined_id: rows.last().map(|r| r.0.clone()),
                eligible_ids: rows
                    .into_iter()
                    .filter(|r| r.2 || r.1 < cutoff_ms)
                    .map(|r| r.0)
                    .collect(),
            })
        }

        fn claim(&self, thread_id: &str, cutoff_ms: i64) -> Result<bool> {
            let mut rows = self.rows.borrow_mut();
            let Some(row) = rows.iter_mut().find(|r| r.0 == thread_id) else {
                return Ok(false);
            };
            if row.2 || row.1 < cutoff_ms {
                row.2 = true;
                return Ok(true);
            }
            Ok(false)
        }

        fn claimed_slice(&self, after_id: Option<&str>, cycle_max_id: &str) -> Result<Vec<String>> {
            Ok(self
                .rows
                .borrow()
                .iter()
                .filter(|r| {
                    r.2 && after_id.is_none_or(|a| r.0.as_str() > a) && r.0.as_str() <= cycle_max_id
                })
                .take(SEAT_LEASE_SLICE)
                .map(|r| r.0.clone())
                .collect())
        }
    }

    #[test]
    fn seat_claiming_pages_past_one_slice_and_claims_only_expired_rows() {
        let ids: Vec<String> = (0..150).map(|n| format!("T-{n:04}")).collect();
        // Even rows are stale, odd rows are fresh.
        let rows: Vec<(&str, i64, bool)> = ids
            .iter()
            .enumerate()
            .map(|(n, id)| (id.as_str(), if n % 2 == 0 { 10 } else { 1_000 }, false))
            .collect();
        let seats = FakeSeats::new(&rows);
        let claimed = claim_expired_seats(&seats, 500).unwrap();
        assert_eq!(claimed.len(), 75);
        assert_eq!(claimed, seats.claimed_ids());
        assert!(
            claimed
                .iter()
                .all(|id| { id.trim_start_matches("T-").parse::<usize>().unwrap() % 2 == 0 })
        );
    }

    #[test]
    fn seat_claiming_includes_leases_claimed_by_an_earlier_run() {
        let seats = FakeSeats::new(&[("T-a", 1_000, true), ("T-b", 1_000, false)]);
        assert_eq!(claim_expired_seats(&seats, 500).unwrap(), ["T-a"]);
    }

    #[test]
    fn seat_claiming_with_no_leases_claims_nothing() {
        assert!(
            claim_expired_seats(&FakeSeats::new(&[]), 500)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn claimed_collection_pages_past_one_slice_and_skips_unclaimed_rows() {
        let ids: Vec<String> = (0..130).map(|n| format!("T-{n:04}")).collect();
        let rows: Vec<(&str, i64, bool)> = ids
            .iter()
            .enumerate()
            .map(|(n, id)| (id.as_str(), 10, n != 7))
            .collect();
        let claimed = collect_claimed_seats(&FakeSeats::new(&rows)).unwrap();
        assert_eq!(claimed.len(), 129);
        assert!(!claimed.contains(&"T-0007".to_string()));
    }

    fn no_validation_hooks(
        qualified: Arc<Mutex<Vec<String>>>,
        settled: Arc<Mutex<Vec<String>>>,
    ) -> InstalledRetention {
        InstalledRetention {
            qualify: Arc::new(move |root| {
                qualified.lock().unwrap().push(root.to_string());
                Ok(None)
            }),
            settle_claimed_seats: Arc::new(move |_, _, claimed| {
                settled.lock().unwrap().extend(claimed.iter().cloned());
                Ok(claimed.len())
            }),
        }
    }

    async fn exclusion_for(
        state: &AppState,
        mode: ryeos_app::retention_exclusion::ExclusionMode,
    ) -> RetentionExclusion<'static> {
        state.seal_thread_pin_sources().unwrap();
        state
            .acquire_retention_gate(mode)
            .await
            .unwrap()
            .acquire_sealed_pins()
            .await
            .unwrap()
    }

    fn phase_params(
        dry_run: bool,
        retire_history: bool,
        seats: bool,
    ) -> RetentionPhaseParams<'static> {
        RetentionPhaseParams {
            now: "2030-01-01T00:00:00Z",
            dry_run,
            retire_history,
            seat_cutoff_ms: seats.then_some(500),
        }
    }

    #[tokio::test]
    async fn retention_phase_runs_replay_fenced_ordinary_recovery_then_seats_in_order() {
        let root = tempfile::TempDir::new().unwrap();
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).unwrap();
        let hooks = no_validation_hooks(Default::default(), Default::default());
        let exclusion = exclusion_for(
            &state,
            ryeos_app::retention_exclusion::ExclusionMode::Exclusive,
        )
        .await;
        let report = run_retention_phase(
            &state,
            &exclusion,
            Some(&hooks),
            &phase_params(false, true, true),
        )
        .unwrap();
        assert_eq!(
            report.steps,
            [
                RetentionStep::Replay,
                RetentionStep::FencedRetirement,
                RetentionStep::OrdinaryRetirement,
                RetentionStep::RecoverPending,
                RetentionStep::SeatReaping,
            ]
        );
        drop(exclusion);
    }

    #[tokio::test]
    async fn retention_phase_without_a_composition_refuses_fenced_retirement_and_never_qualifies() {
        let root = tempfile::TempDir::new().unwrap();
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).unwrap();
        let exclusion = exclusion_for(
            &state,
            ryeos_app::retention_exclusion::ExclusionMode::Exclusive,
        )
        .await;
        let report =
            run_retention_phase(&state, &exclusion, None, &phase_params(false, true, false))
                .unwrap();
        assert_eq!(report.refused_fenced_chains, 0);
        assert_eq!(
            report.steps,
            [
                RetentionStep::Replay,
                RetentionStep::FencedRetirementRefused,
                RetentionStep::OrdinaryRetirement,
                RetentionStep::RecoverPending,
            ]
        );
        drop(exclusion);
    }

    #[tokio::test]
    async fn retention_phase_dry_run_neither_replays_nor_reaps() {
        let root = tempfile::TempDir::new().unwrap();
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).unwrap();
        let hooks = no_validation_hooks(Default::default(), Default::default());
        let exclusion = exclusion_for(
            &state,
            ryeos_app::retention_exclusion::ExclusionMode::Exclusive,
        )
        .await;
        let report = run_retention_phase(
            &state,
            &exclusion,
            Some(&hooks),
            &phase_params(true, true, true),
        )
        .unwrap();
        assert_eq!(
            report.steps,
            [
                RetentionStep::FencedRetirement,
                RetentionStep::OrdinaryRetirement,
                RetentionStep::RecoverPending,
            ]
        );
        drop(exclusion);
    }

    #[tokio::test]
    async fn retention_phase_without_history_retirement_still_replays_and_recovers() {
        let root = tempfile::TempDir::new().unwrap();
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).unwrap();
        let exclusion = exclusion_for(
            &state,
            ryeos_app::retention_exclusion::ExclusionMode::Exclusive,
        )
        .await;
        let report =
            run_retention_phase(&state, &exclusion, None, &phase_params(false, false, false))
                .unwrap();
        assert_eq!(
            report.steps,
            [RetentionStep::Replay, RetentionStep::RecoverPending]
        );
        drop(exclusion);
    }

    #[tokio::test]
    async fn retention_phase_refuses_a_shared_exclusion() {
        let root = tempfile::TempDir::new().unwrap();
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).unwrap();
        let exclusion = exclusion_for(
            &state,
            ryeos_app::retention_exclusion::ExclusionMode::Shared,
        )
        .await;
        assert!(
            run_retention_phase(&state, &exclusion, None, &phase_params(false, true, true))
                .is_err()
        );
        assert!(
            recover_retention(&state, &exclusion, None, "2030-01-01T00:00:00Z", |_| Ok(0)).is_err()
        );
        drop(exclusion);
    }

    #[tokio::test]
    async fn claimed_seats_go_to_the_installed_settler_and_an_empty_set_does_not() {
        let root = tempfile::TempDir::new().unwrap();
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).unwrap();
        let settled = Arc::new(Mutex::new(Vec::new()));
        let hooks = no_validation_hooks(Default::default(), settled.clone());
        let exclusion = exclusion_for(
            &state,
            ryeos_app::retention_exclusion::ExclusionMode::Exclusive,
        )
        .await;
        let claimed = vec!["T-a".to_string(), "T-b".to_string()];
        assert_eq!(
            settle_claimed_seats(&state, &exclusion, Some(&hooks), &claimed).unwrap(),
            2
        );
        assert_eq!(*settled.lock().unwrap(), claimed);
        assert_eq!(
            settle_claimed_seats(&state, &exclusion, Some(&hooks), &[]).unwrap(),
            0
        );
        assert_eq!(settled.lock().unwrap().len(), 2);
        drop(exclusion);
    }

    #[tokio::test]
    async fn startup_recovery_replays_then_recovers_and_settles_nothing_without_a_composition() {
        let root = tempfile::TempDir::new().unwrap();
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).unwrap();
        let exclusion = exclusion_for(
            &state,
            ryeos_app::retention_exclusion::ExclusionMode::Exclusive,
        )
        .await;
        let report = recover_retention(&state, &exclusion, None, "2030-01-01T00:00:00Z", |ids| {
            exclusion.count_thread_pins(ids)
        })
        .unwrap();
        assert_eq!(report, StartupRetentionRecovery::default());
        drop(exclusion);
    }

    #[test]
    fn a_composition_without_event_contracts_is_refused_before_anything_installs() {
        fn accept(_: &ryeos_state::objects::ThreadEvent) -> Result<()> {
            Ok(())
        }
        let error = install_retention_composition(Some(RetentionComposition {
            event_contracts: Vec::new(),
            evidence_validator: accept,
            qualify: Arc::new(|_| Ok(None)),
            settle_claimed_seats: Arc::new(|_, _, _| Ok(0)),
        }))
        .unwrap_err();
        assert!(error.to_string().contains("event contracts"));
        assert!(
            ryeos_app::object_contracts::retained_evidence_validator().is_err(),
            "a refused composition installs no validator"
        );
        assert!(installed_retention().is_none());
    }

    #[test]
    fn retention_composition_installs_once_and_a_plain_root_installs_no_hooks() {
        install_retention_composition(None).unwrap();
        assert!(installed_retention().is_none());
        assert!(
            ryeos_app::object_contracts::retained_evidence_validator().is_err(),
            "a plain root installs no validator"
        );
        assert!(install_retention_composition(None).is_err());
    }
}
