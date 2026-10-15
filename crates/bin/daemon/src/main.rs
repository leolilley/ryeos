use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::serve;
use clap::Parser;
use tokio::net::{TcpListener, UnixListener};

use ryeos_api::handlers::remote_pull_worker_result::recover_durable_worker_result_pulls;
use ryeos_api::handlers::remote_reconcile_project_head::recover_durable_project_head_reconciliations;
use ryeos_app::callback_token::CallbackCapabilityStore;
use ryeos_app::command_service::CommandService;
use ryeos_app::event_store_service::EventStoreService;
use ryeos_app::event_stream::{DEFAULT_EVENT_STREAM_CAPACITY, ThreadEventHub};
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::ThreadLifecycleService;
use ryeos_app::{command_service, event_store_service, thread_lifecycle};
use ryeos_app::{kind_profiles, process, state, state_store};
use ryeos_node_runtime::{state_lock};
use ryeos_executor::executor as service_executor;
use ryeos_node::lifecycle_marker;
use ryeosd::config::{self, Cli, Config};
#[cfg(feature = "scheduling")]
use ryeosd::scheduler;
#[cfg(feature = "scheduling")]
use ryeosd::scheduler::db::SchedulerDb;
use ryeosd::{bootstrap, reconcile, uds};

mod composition_native;
#[cfg(feature = "scheduling")]
mod maintenance_schedule;
#[cfg(feature = "scheduling")]
mod standalone_scheduler;
mod startup;

const STARTUP_FAILURE_REPORTING_GRACE: Duration = Duration::from_secs(30);
/// Stack of the thread that runs native shutdown and settlement. Fixed by
/// mechanism, not policy: the close and failed-start settlement chains keep
/// several large owners on the stack at once, which can exceed a small
/// process stack limit, especially in unoptimized builds.
const NATIVE_SETTLEMENT_STACK_BYTES: usize = 64 * 1024 * 1024;
/// Bound on waiting for the audit publisher's in-flight blocking work.
const ACCOUNTING_PUBLISHER_STOP_BOUND: Duration = process::ACCOUNTING_PUBLISHER_STOP_BOUND;
const BUNDLE_PUBLISHER_URL_ENV: &str = "RYEOS_BUNDLE_PUBLISHER_URL";
const BUNDLE_PUBLISHER_BEARER_ENV: &str = "RYEOS_BUNDLE_PUBLISHER_BEARER";

/// Quote the one-time StateLock owner conversion retained by the consuming
/// native finalizer. The lock target and registry registration are charged by
/// their own physical-custody owners.

fn configured_bundle_publisher(
    policy: &ryeos_node_runtime::node_policy::sections::bundle_publication::BundlePublicationPolicy,
) -> Result<Option<Arc<dyn ryeos_app::bundle_publication::producer::BundlePublisherAuthority>>> {
    let endpoint = std::env::var_os(BUNDLE_PUBLISHER_URL_ENV);
    let bearer = std::env::var_os(BUNDLE_PUBLISHER_BEARER_ENV);
    let (endpoint, bearer) = match (endpoint, bearer) {
        (None, None) => return Ok(None),
        (Some(endpoint), Some(bearer)) => (endpoint, bearer),
        _ => anyhow::bail!(
            "{BUNDLE_PUBLISHER_URL_ENV} and {BUNDLE_PUBLISHER_BEARER_ENV} must be configured together"
        ),
    };
    let endpoint = endpoint
        .into_string()
        .map_err(|_| anyhow::anyhow!("{BUNDLE_PUBLISHER_URL_ENV} is not valid UTF-8"))?
        .parse::<url::Url>()
        .context("parse explicit bundle publisher endpoint")?;
    let bearer = bearer
        .into_string()
        .map_err(|_| anyhow::anyhow!("{BUNDLE_PUBLISHER_BEARER_ENV} is not valid UTF-8"))?;
    let expected = policy
        .catalogs
        .first()
        .context("bundle publisher configuration requires an authorized catalog")?
        .publisher_fingerprint
        .clone();
    anyhow::ensure!(
        policy
            .catalogs
            .iter()
            .all(|catalog| catalog.publisher_fingerprint == expected),
        "one configured bundle publisher cannot serve catalog policies with different publishers"
    );
    Ok(Some(Arc::new(
        ryeos_app::bundle_publication::producer::AuthenticatedPublisherClient::new(
            endpoint, bearer, expected,
        )?,
    )))
}

struct LifecycleExitGuard {
    state_dir: std::path::PathBuf,
    started_at: String,
    launch_route: lifecycle_marker::LaunchRoute,
    recorded: bool,
    // The recorded marker is the provisional finalizing reason, which only
    // this run's completed finalizer may replace.
    provisional: bool,
}

impl LifecycleExitGuard {
    fn new(
        state_dir: std::path::PathBuf,
        started_at: String,
        launch_route: lifecycle_marker::LaunchRoute,
    ) -> Self {
        Self {
            state_dir,
            started_at,
            launch_route,
            recorded: false,
            provisional: false,
        }
    }

    fn record(&mut self, reason: &str, error: Option<&str>) {
        lifecycle_marker::record_exit(
            &self.state_dir,
            reason,
            &self.started_at,
            error,
            self.launch_route,
        );
        self.recorded = true;
    }

    fn record_provisional(&mut self, staged_reason: &str, staged_detail: Option<&str>) {
        let detail = match staged_detail {
            Some(detail) => format!("staged reason: {staged_reason}; {detail}"),
            None => format!("staged reason: {staged_reason}"),
        };
        self.record(lifecycle_marker::FINALIZING_REASON, Some(&detail));
        self.provisional = true;
    }

    /// Record the provisional finalizing exit carrying a latched drain's
    /// facts. Strict: on failure nothing is marked recorded. On success the
    /// later staged provisional record is skipped, so it cannot replace them.
    fn record_provisional_latched(
        &mut self,
        staged_reason: &str,
        facts: &lifecycle_marker::LatchedDrainFacts,
    ) -> Result<()> {
        let detail = format!("staged reason: {staged_reason}");
        lifecycle_marker::record_exit_with_latched_drain(
            &self.state_dir,
            lifecycle_marker::FINALIZING_REASON,
            &self.started_at,
            Some(&detail),
            self.launch_route,
            facts,
        )?;
        self.recorded = true;
        self.provisional = true;
        Ok(())
    }

    fn complete_provisional(&mut self, reason: &str, error: Option<&str>) {
        if self.provisional
            && lifecycle_marker::complete_finalizing_exit(
                &self.state_dir,
                reason,
                &self.started_at,
                error,
                self.launch_route,
            )
        {
            self.provisional = false;
        }
    }
}

impl Drop for LifecycleExitGuard {
    fn drop(&mut self) {
        if !self.recorded {
            lifecycle_marker::record_exit(
                &self.state_dir,
                "startup_failed",
                &self.started_at,
                None,
                self.launch_route,
            );
        }
    }
}

/// Main owns the original exclusion/lifecycle guards and Render custody outside
/// the run future, retaining them until the selected root and Render owners close.
#[derive(Default)]
struct NativeFinalizerAttempts(u8);

impl NativeFinalizerAttempts {
    fn claim(&mut self) -> Result<()> {
        anyhow::ensure!(self.0 < ryeos_node::native_capacity::MAX_FINALIZATION_ATTEMPTS,
            "native finalizer attempt allowance exhausted; original custody remains retained");
        self.0 = self.0.checked_add(1)
            .context("native finalizer attempt counter overflow")?;
        Ok(())
    }

    fn exhausted(&self) -> bool {
        self.0 >= ryeos_node::native_capacity::MAX_FINALIZATION_ATTEMPTS
    }
}

/// Secret-free snapshot of which composition owners remain when bounded
/// finalization cannot continue. This contains custody-presence facts only;
/// it deliberately excludes paths, identifiers, errors, credentials and data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnresolvedNativeCustody {
    attempts_used: u8,
    services_drained: bool,
    persistent_pool_retained: bool,
    app_state_retained: bool,
    live_store_retained: bool,
    scheduler_retained: bool,
    accounting_retained: bool,
    native_failure_retained: bool,
    inputs_retained: bool,
    reservation_retained: bool,
    factory_retained: bool,
    prepared_registry_retained: bool,
    store_close_retained: bool,
    store_outside_retained: bool,
    settled_account_retained: bool,
    native_terminal_retained: bool,
    startup_recovery_retained: bool,
    selected_root_unresolved: bool,
    operator_lock_retained: bool,
    /// Owners dropped unsettled anywhere in this process and retained until
    /// exit, and fork exclusions held until exit. Neither can be retried.
    abandoned_owners: usize,
    fork_exclusions_held: usize,
}

impl UnresolvedNativeCustody {
    /// Whether an owner was abandoned or a fork exclusion is held until exit.
    fn abandoned(&self) -> bool {
        self.abandoned_owners + self.fork_exclusions_held > 0
    }

    /// The lifecycle and restart-inhibition reason this snapshot ends the
    /// run with. Abandonment is named first: no attempt can resolve an
    /// owner that already lost its supervised terminal.
    fn exit_reason(&self) -> &'static str {
        if self.abandoned() {
            "native_custody_abandoned"
        } else {
            "native_custody_unresolved"
        }
    }
}

/// Owners this process retains because a destructor ran on them unsettled,
/// and fork exclusions held until exit. Read from the process-wide registries
/// in lillux; both only ever grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct AbandonedNativeCustody {
    owners: usize,
    fork_exclusions: usize,
}

impl AbandonedNativeCustody {
    fn observe() -> Self {
        Self {
            owners: lillux::custody::abandoned_custody().total,
            fork_exclusions: lillux::fork_exclusions_held_until_exit(),
        }
    }

    fn total(self) -> usize {
        self.owners + self.fork_exclusions
    }
}

/// Whether any owner in this process was dropped without settlement or any
/// fork exclusion is held until exit. Unlike retained composition fields this
/// cannot be resolved by another finalization attempt.
fn has_abandoned_native_custody() -> bool {
    AbandonedNativeCustody::observe().total() > 0
}

impl std::fmt::Display for UnresolvedNativeCustody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f,
            "attempts_used={} services_drained={} persistent_pool_retained={} app_state_retained={} live_store_retained={} scheduler_retained={} accounting_retained={} native_failure_retained={} inputs_retained={} reservation_retained={} factory_retained={} prepared_registry_retained={} store_close_retained={} store_outside_retained={} settled_account_retained={} native_terminal_retained={} startup_recovery_retained={} selected_root_unresolved={} operator_lock_retained={} abandoned_owners={} fork_exclusions_held={}",
            self.attempts_used,
            self.services_drained,
            self.persistent_pool_retained,
            self.app_state_retained,
            self.live_store_retained,
            self.scheduler_retained,
            self.accounting_retained,
            self.native_failure_retained,
            self.inputs_retained,
            self.reservation_retained,
            self.factory_retained,
            self.prepared_registry_retained,
            self.store_close_retained,
            self.store_outside_retained,
            self.settled_account_retained,
            self.native_terminal_retained,
            self.startup_recovery_retained,
            self.selected_root_unresolved,
            self.operator_lock_retained,
            self.abandoned_owners,
            self.fork_exclusions_held,
        )
    }
}

impl std::error::Error for UnresolvedNativeCustody {}

/// Non-owning native admission fence. The select point can close native
/// tickets and Scheduler metadata admission while the startup future still
/// borrows the composition. It holds only `Weak` handles, so it never extends
/// an owner's lifetime; an install after close is closed immediately.
#[derive(Default)]
struct NativeAdmissionFence {
    closed: std::sync::atomic::AtomicBool,
    factory: Mutex<Option<std::sync::Weak<ryeos_node::native_capacity::LiveNativeFactory>>>,
    stores: Mutex<Vec<std::sync::Weak<state_store::StateStore>>>,
    // Read-only retained shutdown observation, not a native terminal certificate.
    factory_close: Mutex<Option<std::result::Result<(), anyhow::Error>>>,
}

impl NativeAdmissionFence {
    fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn mark_closed(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn close_factory(&self, factory: &ryeos_node::native_capacity::LiveNativeFactory) {
        // A transient Busy refusal must not be frozen as the final close.
        // Retain the latest observation; no successful close grants a
        // native terminal or releases any real owner. The observation lock
        // is never held across an await, so waiting for it cannot skip a close.
        let mut observation = self.factory_close.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(observation.as_ref(), Some(Ok(()))) {
            *observation = Some(factory.close_admission());
        }
    }

    fn register_factory(&self, factory: &Arc<ryeos_node::native_capacity::LiveNativeFactory>) {
        if let Ok(mut slot) = self.factory.lock() {
            *slot = Some(Arc::downgrade(factory));
        }
        if self.is_closed() {
            self.close_factory(factory);
        }
    }

    fn register_store(&self, store: &Arc<state_store::StateStore>) {
        if let Ok(mut stores) = self.stores.lock() {
            let weak = Arc::downgrade(store);
            if !stores.iter().any(|known| std::sync::Weak::ptr_eq(known, &weak)) {
                stores.push(weak);
            }
        }
        if self.is_closed() {
            store.close_live_scheduler_metadata_admission();
        }
    }

    /// Close admission on every registered owner still alive. Each upgrade is
    /// a transient strong handle dropped at once, before any release path.
    fn close(&self) {
        self.mark_closed();
        if let Ok(stores) = self.stores.lock() {
            for store in stores.iter().filter_map(std::sync::Weak::upgrade) {
                store.close_live_scheduler_metadata_admission();
            }
        }
        let factory = self.factory.lock().ok().and_then(|slot| slot.as_ref().and_then(std::sync::Weak::upgrade));
        if let Some(factory) = factory {
            self.close_factory(&factory);
        }
    }
}

struct DaemonSelectedComposition {
    root: ryeos_executor::SelectedInvocationCompositionBarrier,
    launch_route: Option<lifecycle_marker::LaunchRoute>,
    // App root of a placed native host-service controller. Taken once by the
    // failure exits that must not let the supervisor restart this node.
    supervised_app_root: Option<std::path::PathBuf>,
    process_state_lock: Option<state_lock::StateLock>,
    #[cfg(target_os = "linux")]
    native_operator_lock: Option<ryeos_node::native_capacity::OperatorLockCustody>,
    #[cfg(target_os = "linux")]
    native_operator_lock_registered: bool,
    lifecycle_exit: Option<LifecycleExitGuard>,
    // Exit reason chosen before native shutdown. It is recorded as the detail
    // of a provisional finalizing marker while the original exclusion is still
    // held, before operator target retirement.
    staged_exit_record: Option<(&'static str, Option<String>)>,
    operator_target_retirement_entered: bool,
    persistent_pool_custody: Option<Arc<ryeos_app::persistent_session::PersistentSessionPool>>,
    app_state_custody: Option<AppState>,
    live_state_store_custody: Option<Arc<state_store::StateStore>>,
    native_bootstrap_entered: bool,
    native_inputs: Option<Arc<ryeos_node::native_capacity::LiveNativeCapacityInputs>>,
    native_reservation: Option<Arc<ryeos_node::native_capacity::LiveNativeCapacityReservation>>,
    native_factory: Option<Arc<ryeos_node::native_capacity::LiveNativeFactory>>,
    native_failure: Option<anyhow::Error>,
    services_drained: bool,
    native_closed_store: Option<state_store::ClosedLiveStateStore>,
    native_outside_store: Option<state_store::LiveStateStoreOutsideCustody>,
    native_prepared_registry: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry>,
    native_store_close_failure: Option<state_store::LiveStateStoreCloseFailure>,
    native_reservation_close_failure: Option<ryeos_node::native_capacity::LiveNativeCapacityReservationFailure>,
    native_settled_reservation: Option<ryeos_node::native_capacity::SettledLiveNative>,
    native_terminal: Option<Arc<lillux_sqlite::OriginalNativeTerminal>>,
    native_startup_recovery_failure: Option<state_store::LiveStateStoreStartupRecoveryFailure>,
    native_released_startup: Option<state_store::ReleasedLiveStateStoreStartup>,
    // Shared by the outer composition; once the consuming finalizer is wired,
    // every shutdown entry path must claim from this same finite allowance.
    native_finalizer_attempts: NativeFinalizerAttempts,
    native_admission_fence: Arc<NativeAdmissionFence>,
    native_control_observation: std::sync::Mutex<Option<
        std::result::Result<ryeos_node::native_capacity::LiveProducerControlCensus, anyhow::Error>,
    >>,
    // Live accounting ledger native owner, retained from its first open
    // attempt (successful or refused) until its acknowledged outside sources
    // are cached below; then the cached sources join the Store graph.
    accounting_native: Option<Arc<ryeos_app::accounting_db::AccountingNativeLedger>>,
    // Native SQLite source slots of the accounting and scheduler owners, under
    // type-level custody: occupied slots are retained, never dropped.
    native_sources: composition_native::CompositionNativeSources,
    // The audit outbox publisher holds Store and ledger aliases. Its handle
    // is filled from inside the startup future and ended before native
    // shutdown, so the consuming Store close sees no service owner.
    accounting_publisher: Arc<Mutex<Option<(tokio::task::JoinHandle<()>, tokio::sync::watch::Sender<bool>)>>>,
    // The incarnation's service latch, its planned-window wake and the
    // outcome a latched or rollover stop records.
    service_stop: ServiceStopState,
    #[cfg(feature = "scheduling")]
    scheduler_db_custody: Option<Arc<scheduler::db::SchedulerDb>>,
    #[cfg(feature = "scheduling")]
    scheduler_admission_failure: Option<state_store::LiveSchedulerNativeAdmissionFailure>,
    #[cfg(feature = "scheduling")]
    scheduler_admission_sources: Option<state_store::LiveSchedulerNativeAdmissionOutsideSources>,
    #[cfg(feature = "scheduling")]
    scheduler_participant_custody: Option<Arc<dyn ryeos_app::scheduler_boundary::SchedulerParticipant>>,
    // Retain actual receipts alongside their database until the full original
    // factory census settles. Service close alone cannot release this owner.
    #[cfg(feature = "scheduling")]
    scheduler_open_failure: Option<scheduler::db::SchedulerDbOpenFailure>,
    #[cfg(feature = "scheduling")]
    scheduler_schema_failure: Option<ryeos_state::sqlite_schema::NativeSchemaSqlReferenceFailure>,
    #[cfg(feature = "scheduling")]
    scheduler_recovered_schema: Option<ryeos_state::sqlite_schema::ReleasedNativeSchemaSqlReference>,
    #[cfg(feature = "scheduling")]
    scheduler_admission_custody: Option<state_store::LiveSchedulerNativeAdmission>,
}

impl DaemonSelectedComposition {
    fn bootstrap() -> Self {
        let (root, _unpublished_attachment) =
            ryeos_executor::SelectedInvocationCompositionBarrier::bootstrap();
        Self {
            root,
            launch_route: None,
            supervised_app_root: None,
            process_state_lock: None,
            #[cfg(target_os = "linux")]
            native_operator_lock: None,
            #[cfg(target_os = "linux")]
            native_operator_lock_registered: false,
            lifecycle_exit: None,
            staged_exit_record: None,
            operator_target_retirement_entered: false,
            persistent_pool_custody: None,
            app_state_custody: None,
            live_state_store_custody: None,
            native_bootstrap_entered: false,
            native_inputs: None,
            native_reservation: None,
            native_factory: None,
            native_failure: None,
            services_drained: false,
            native_closed_store: None,
            native_outside_store: None,
            native_prepared_registry: None,
            native_store_close_failure: None,
            native_reservation_close_failure: None,
            native_settled_reservation: None,
            native_terminal: None,
                native_startup_recovery_failure: None,
                native_released_startup: None,
            native_finalizer_attempts: NativeFinalizerAttempts::default(),
            native_admission_fence: Arc::default(),
            native_control_observation: std::sync::Mutex::new(None),
            accounting_native: None,
            native_sources: composition_native::CompositionNativeSources::new(),
            accounting_publisher: Arc::default(),
            service_stop: ServiceStopState::default(),
            #[cfg(feature = "scheduling")]
            scheduler_db_custody: None,
            #[cfg(feature = "scheduling")]
            scheduler_admission_failure: None,
            #[cfg(feature = "scheduling")]
            scheduler_admission_sources: None,
            #[cfg(feature = "scheduling")]
            scheduler_participant_custody: None,
            #[cfg(feature = "scheduling")]
            scheduler_open_failure: None,
                #[cfg(feature = "scheduling")]
                scheduler_schema_failure: None,
                #[cfg(feature = "scheduling")]
                scheduler_recovered_schema: None,
                #[cfg(feature = "scheduling")]
                scheduler_admission_custody: None,
        }
    }

    #[cfg(feature = "scheduling")]
    fn retain_scheduler_db(&mut self, db: Arc<scheduler::db::SchedulerDb>) {
        self.scheduler_db_custody = Some(db);
    }

    #[cfg(feature = "scheduling")]
    fn retain_scheduler_open_failure(&mut self, failure: scheduler::db::SchedulerDbOpenFailure) {
        self.scheduler_open_failure = Some(failure);
    }

    #[cfg(feature = "scheduling")]
    fn retain_scheduler_admission_failure(&mut self, error: anyhow::Error) -> anyhow::Error {
        match error.downcast::<state_store::LiveSchedulerNativeAdmissionFailure>() {
            Ok(failure) if self.scheduler_admission_failure.is_none() => {
                self.scheduler_admission_failure = Some(failure);
                anyhow::anyhow!("original Scheduler admission failure retained")
            }
            Ok(failure) => anyhow::Error::new(failure),
            // Non-owning admission refusal created no scheduler source frame.
            // Let ordinary startup unwind retain its original diagnostic.
            Err(error) => error,
        }
    }

    #[cfg(feature = "scheduling")]
    fn retain_scheduler_terminals_after_close(&mut self) -> Result<()> {
        if self.native_sources.scheduler_native_terminals().is_some() {
            return Ok(());
        }
        if let Some(owner) = &self.native_sources.scheduler_native_finalization_owner() {
            let terminals = owner.retained_native_terminals()?;
            *self.native_sources.scheduler_native_terminals_mut() = Some(terminals);
        } else if let Some(db) = &self.scheduler_db_custody {
            // This method validates the original factory and actual terminal
            // state. Any failure leaves the database under composition custody.
            let terminals = db.retained_native_terminals()?;
            *self.native_sources.scheduler_native_terminals_mut() = Some(terminals);
        }
        Ok(())
    }

    /// Retry the post-drain Scheduler close and terminal caching, then the
    /// owner transfer. Each step is idempotent once it has completed.
    #[cfg(feature = "scheduling")]
    fn retry_scheduler_close_and_transfer(&mut self) -> Result<()> {
        if self.native_sources.scheduler_native_finalization_owner().is_none() {
            let service = self.app_state_custody.as_ref()
                .and_then(|app| app.extensions.get::<ryeos_scheduler_service::SchedulerService>());
            if let Some(service) = service {
                service.close_after_shutdown_drain()?;
            }
            self.retain_scheduler_terminals_after_close()?;
        }
        self.transfer_scheduler_native_finalization_owner()
    }

    /// The supervised drain completed but the Scheduler close refused. The
    /// service stays in AppState for the native shutdown retry.
    #[cfg(feature = "scheduling")]
    fn mark_services_drained_with_scheduler_close_pending(&mut self) {
        self.services_drained = true;
    }

    #[cfg(feature = "scheduling")]
    fn transfer_scheduler_native_finalization_owner(&mut self) -> Result<()> {
        if self.native_sources.scheduler_native_finalization_owner().is_some() {
            return Ok(());
        }
        let Some(expected_db) = self.scheduler_db_custody.as_ref() else {
            return Ok(());
        };
        let state = self.app_state_custody.as_mut()
            .context("scheduler finalization transfer has no retained AppState")?;
        let Some(service) = state.extensions.remove::<ryeos_scheduler_service::SchedulerService>() else {
            anyhow::bail!("selected scheduler service is absent from the retained extension owner");
        };
        let service_db = service.scheduler_db();
        if !Arc::ptr_eq(expected_db, &service_db) {
            state.extensions.insert(service);
            anyhow::bail!("scheduler service selected a different DB than composition custody");
        }
        drop(service_db);
        let Some(participant) = self.scheduler_participant_custody.take()
            .or_else(|| state.take_scheduler_participant_for_shutdown()) else {
            state.extensions.insert(service);
            anyhow::bail!("selected scheduler service has no retained lifecycle participant");
        };
        let service_participant: Arc<dyn ryeos_app::scheduler_boundary::SchedulerParticipant> =
            service.clone();
        if !Arc::ptr_eq(&participant, &service_participant) {
            self.scheduler_participant_custody = Some(participant);
            state.extensions.insert(service);
            anyhow::bail!("scheduler lifecycle participant is not the selected service owner");
        }
        drop(service_participant);
        drop(participant);
        // The service is the canonical DB owner after drain. Retire the
        // composition's observation alias only after identity is checked; a
        // refusal below reconstructs it from the returned complete service.
        self.scheduler_db_custody.take();
        match service.into_native_finalization_owner() {
            Ok(owner) => {
                *self.native_sources.scheduler_native_finalization_owner_mut() = Some(owner);
                Ok(())
            }
            Err((service, error)) => {
                self.scheduler_db_custody = Some(service.scheduler_db());
                let participant: Arc<dyn ryeos_app::scheduler_boundary::SchedulerParticipant> =
                    service.clone();
                self.scheduler_participant_custody = Some(participant);
                state.extensions.insert(service);
                Err(error)
            }
        }
    }

    #[cfg(feature = "scheduling")]
    fn prepare_scheduler_outside_source_fields(&mut self) -> Result<()> {
        if let Some(sources) = self.native_sources.scheduler_native_outside_sources().as_ref() {
            sources.handoff_id()?;
            anyhow::ensure!(self.native_sources.scheduler_native_connection_parts().is_none()
                && self.native_sources.scheduler_native_unopened_inputs().is_none()
                && self.native_sources.scheduler_native_schema_parts().is_none()
                && self.native_sources.scheduler_native_schema_transient().is_none(),
                "Scheduler outside packet competes with an already retained source field");
        }
        Ok(())
    }

    #[cfg(feature = "scheduling")]
    fn advance_scheduler_sources_in_store(
        &mut self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        producer: &ryeos_node::native_capacity::LiveNativeFactory,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<()> {
        self.prepare_scheduler_outside_source_fields()?;
        if self.scheduler_recovered_schema.is_some() {
            anyhow::ensure!(self.native_sources.scheduler_native_schema_parts().is_none()
                && self.native_sources.scheduler_native_schema_transient().is_none()
                && self.native_sources.scheduler_native_outside_sources().as_ref()
                    .is_none_or(scheduler::db::SchedulerNativeOutsideSources::is_empty),
                "failed schema reference competes with a normal Scheduler source packet");
            let released = self.scheduler_recovered_schema.take()
                .expect("checked recovered Scheduler schema source");
            match released.into_outside_parts(factory, terminal) {
                Ok((parts, transient, _prior_refusal)) => {
                    if let Some(parts) = parts {
                        *self.native_sources.scheduler_native_schema_parts_mut() = Some(parts);
                    }
                    *self.native_sources.scheduler_native_schema_transient_mut() = transient
                        .map(ryeos_state::sqlite_schema::UnusedSchemaTransientInputs::take_or_abort);
                    tracing::warn!(
                        "scheduler schema reference failed; retaining its original failure evidence during shutdown"
                    );
                }
                Err((released, error)) => {
                    self.scheduler_recovered_schema = Some(released);
                    return Err(error);
                }
            }
        }
        let outside = self.native_outside_store.as_mut()
            .context("selected Scheduler source lacks the retained Store graph")?;
        outside.advance_scheduler_admission_sources(&mut self.scheduler_admission_sources,
            factory, terminal, &|bytes, steps|
                producer.prepay_outside_control(terminal, bytes, steps))?;
        outside.advance_scheduler_native_handoff(self.native_sources.scheduler_native_outside_sources_mut(),
            factory, terminal, &|bytes, steps|
                producer.prepay_outside_control(terminal, bytes, steps))?;
        if self.native_sources.scheduler_native_connection_parts().is_some() {
            outside.advance_scheduler_packet_parts(self.native_sources.scheduler_native_connection_parts_mut(), factory, terminal,
                &|bytes, steps| producer.prepay_outside_control(terminal, bytes, steps))?;
        }
        if self.native_sources.scheduler_native_unopened_inputs().is_some() {
            outside.advance_unopened_scheduler_database(self.native_sources.scheduler_native_unopened_inputs_mut(), factory, terminal,
                &|bytes, steps| producer.prepay_outside_control(terminal, bytes, steps))?;
        }
        if self.native_sources.scheduler_native_schema_parts().is_some() {
            outside.advance_scheduler_packet_parts(self.native_sources.scheduler_native_schema_parts_mut(), factory, terminal,
                &|bytes, steps| producer.prepay_outside_control(terminal, bytes, steps))?;
        }
        if self.native_sources.scheduler_native_schema_transient().is_some() {
            outside.advance_unopened_scheduler_transient(self.native_sources.scheduler_native_schema_transient_mut(),
                factory, terminal, &|bytes, steps|
                    producer.prepay_outside_control(terminal, bytes, steps))?;
        }
        // Resume any packet or unopened-bound transition already cached in the
        // Store after a prior callback/prepayment refusal.
        let mut no_packet_source = None;
        outside.advance_scheduler_packet_parts(&mut no_packet_source, factory, terminal,
            &|bytes, steps| producer.prepay_outside_control(terminal, bytes, steps))?;
        let mut no_unopened_source = None;
        outside.advance_unopened_scheduler_database(&mut no_unopened_source, factory, terminal,
            &|bytes, steps| producer.prepay_outside_control(terminal, bytes, steps))?;
        let mut no_transient_source = None;
        outside.advance_unopened_scheduler_transient(&mut no_transient_source, factory, terminal,
            &|bytes, steps| producer.prepay_outside_control(terminal, bytes, steps))?;
        anyhow::ensure!(self.native_sources.scheduler_native_connection_parts().is_none()
            && self.native_sources.scheduler_native_unopened_inputs().is_none()
            && self.native_sources.scheduler_native_schema_parts().is_none()
            && self.native_sources.scheduler_native_schema_transient().is_none()
            && self.scheduler_admission_sources.is_none(),
            "selected Scheduler sources remain outside the Store inventory");
        if self.native_sources.scheduler_native_finalization_owner().is_some() {
            let sources = self.native_sources.scheduler_native_outside_sources().as_ref()
                .context("selected Scheduler source owner absent before completion")?;
            anyhow::ensure!(sources.is_empty(),
                "selected Scheduler inputs remain outside Store custody");
            let handoff_id = sources.handoff_id()?;
            let receipt = outside.scheduler_native_store_handoff_receipt(handoff_id,
                factory, terminal)?;
            let owner = self.native_sources.scheduler_native_finalization_owner_mut().take()
                .expect("checked Scheduler finalization owner");
            if let Err((owner, error)) = owner.finish_outside_handoff(receipt) {
                *self.native_sources.scheduler_native_finalization_owner_mut() = Some(owner);
                return Err(error);
            }
            *self.native_sources.scheduler_native_outside_sources_mut() = None;
        } else {
            anyhow::ensure!(self.native_sources.scheduler_native_outside_sources().as_ref()
                .is_none_or(scheduler::db::SchedulerNativeOutsideSources::is_empty),
                "Scheduler Store source inputs outlived their finalization owner");
        }
        Ok(())
    }

    #[cfg(feature = "scheduling")]
    fn has_scheduler_custody(&self) -> bool {
        self.scheduler_db_custody.is_some() || self.native_sources.scheduler_native_terminals().is_some()
            || self.native_sources.scheduler_native_finalization_owner().is_some()
            || self.native_sources.scheduler_native_outside_sources().is_some()
            || self.scheduler_admission_failure.is_some()
            || self.scheduler_admission_sources.is_some()
            || self.native_sources.scheduler_native_connection_parts().is_some()
            || self.native_sources.scheduler_native_unopened_inputs().is_some()
            || self.native_sources.scheduler_native_schema_parts().is_some()
            || self.native_sources.scheduler_native_schema_transient().is_some()
            || self.scheduler_participant_custody.is_some()
            || self.scheduler_open_failure.is_some()
            || self.scheduler_schema_failure.is_some()
            || self.scheduler_recovered_schema.is_some()
            || self.scheduler_admission_custody.is_some()
    }

    #[cfg(not(feature = "scheduling"))]
    fn has_scheduler_custody(&self) -> bool {
        false
    }

    /// End the audit outbox publisher before native shutdown. Stop is
    /// cooperative so an in-flight publish finishes and releases its Store
    /// alias; claims are lease-bound and acknowledgements identity-checked, so
    /// a bounded abort after timeout is recovered by the next publisher.
    async fn stop_accounting_publisher(&mut self) {
        let publisher = self.accounting_publisher.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((mut publisher, stop)) = publisher {
            // Cooperative stop: an in-flight blocking claim or publish holds
            // Store and ledger aliases that an abort would leave running, so
            // wait for it to finish, within a bound.
            let _ = stop.send(true);
            match tokio::time::timeout(ACCOUNTING_PUBLISHER_STOP_BOUND, &mut publisher).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(%error, "accounting outbox publisher ended abnormally"),
                Err(_) => {
                    // The Store close that follows refuses while an alias
                    // survives, keeping custody; nothing is forced here.
                    tracing::error!(bound = ?ACCOUNTING_PUBLISHER_STOP_BOUND,
                        "accounting outbox publisher did not stop within its bound");
                    publisher.abort();
                }
            }
        }
    }

    fn has_accounting_custody(&self) -> bool {
        self.accounting_native.is_some() || self.native_sources.accounting_native_connection_parts().is_some()
            || self.native_sources.accounting_native_reference_parts().is_some()
            || self.native_sources.accounting_native_unused_reference().is_some()
    }

    /// After the aggregate ACK, move the ledger's acknowledged terminals into
    /// cached outside sources. A refusal keeps them inside the same owner.
    fn take_accounting_outside_sources(
        &mut self,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<()> {
        let Some(ledger) = self.accounting_native.as_ref() else { return Ok(()) };
        anyhow::ensure!(self.native_sources.accounting_native_connection_parts().is_none()
            && self.native_sources.accounting_native_reference_parts().is_none()
            && self.native_sources.accounting_native_unused_reference().is_none(),
            "accounting outside sources compete with already cached sources");
        let sources = ledger.take_outside_sources(terminal)
            .context("take accounting ledger outside sources")?;
        let (connection, reference, unused_reference) = sources.into_parts();
        *self.native_sources.accounting_native_connection_parts_mut() = connection;
        *self.native_sources.accounting_native_reference_parts_mut() = reference;
        *self.native_sources.accounting_native_unused_reference_mut() = unused_reference;
        // The emptied owner holds no native object; remaining AccountingDb
        // aliases already refuse SQL.
        self.accounting_native = None;
        Ok(())
    }

    /// Join the cached ledger sources to the Store's direct auxiliary packet
    /// collection. Each call also resumes any join already cached in the
    /// Store after an earlier refusal.
    fn advance_accounting_sources_in_store(
        &mut self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        producer: &ryeos_node::native_capacity::LiveNativeFactory,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<()> {
        anyhow::ensure!(self.accounting_native.is_none(),
            "accounting ledger owner has not released its outside sources");
        let outside = self.native_outside_store.as_mut()
            .context("accounting ledger sources lack the retained Store graph")?;
        let prepay = |bytes, steps| producer.prepay_outside_control(terminal, bytes, steps);
        outside.advance_accounting_packet_parts(self.native_sources.accounting_native_connection_parts_mut(),
            factory, terminal, &prepay).context("Store pre-seal accounting ledger packet")?;
        outside.advance_accounting_packet_parts(self.native_sources.accounting_native_reference_parts_mut(),
            factory, terminal, &prepay).context("Store pre-seal accounting reference packet")?;
        outside.advance_unopened_accounting_transient(self.native_sources.accounting_native_unused_reference_mut(),
            factory, terminal, &prepay).context("Store pre-seal unused accounting reference")?;
        anyhow::ensure!(!self.has_accounting_custody(),
            "accounting ledger sources remain outside the Store inventory");
        Ok(())
    }

    #[cfg(feature = "scheduling")]
    fn app_state_holds_scheduler_service(&self) -> bool {
        self.app_state_custody.as_ref().is_some_and(|app|
            app.extensions.get::<ryeos_scheduler_service::SchedulerService>().is_some())
    }

    /// A Scheduler opened before AppState adoption has no service close
    /// owner. Close the composition's own database, cache its terminals and
    /// move it to finalization custody; any refusal keeps the same Arc.
    #[cfg(feature = "scheduling")]
    fn retire_unadopted_scheduler_db(&mut self) -> Result<()> {
        if self.app_state_custody.is_some() || self.native_sources.scheduler_native_finalization_owner().is_some() {
            return Ok(());
        }
        let Some(db) = self.scheduler_db_custody.as_ref() else {
            return Ok(());
        };
        // Close only the sole owner: a surviving alias could still be using
        // the database, and the transfer below would refuse it anyway.
        anyhow::ensure!(Arc::strong_count(db) == 1,
            "unadopted Scheduler database still has active aliases");
        db.close().map_err(anyhow::Error::new)
            .context("close unadopted Scheduler database")?;
        self.retain_scheduler_terminals_after_close()?;
        let db = self.scheduler_db_custody.take().expect("checked unadopted Scheduler database");
        let db = match Arc::try_unwrap(db) {
            Ok(db) => db,
            Err(db) => {
                self.scheduler_db_custody = Some(db);
                anyhow::bail!("unadopted Scheduler database still has active aliases");
            }
        };
        match db.into_native_finalization() {
            Ok(owner) => {
                *self.native_sources.scheduler_native_finalization_owner_mut() = Some(owner);
                Ok(())
            }
            Err((db, error)) => {
                self.scheduler_db_custody = Some(Arc::new(db));
                Err(error)
            }
        }
    }

    fn retain_persistent_pool(
        &mut self,
        pool: Arc<ryeos_app::persistent_session::PersistentSessionPool>,
    ) {
        self.persistent_pool_custody = Some(pool);
    }

    fn install_live_state_store(&mut self, store: Arc<state_store::StateStore>) {
        self.native_admission_fence.register_store(&store);
        self.live_state_store_custody = Some(store);
    }

    fn adopt_app_state(&mut self, app: AppState) -> Result<()> {
        // Retain the destination before checking the actual handoff. On any
        // mismatch both original stores remain under composition custody.
        self.native_admission_fence.register_store(&app.state_store);
        self.app_state_custody = Some(app);
        if let Some(original) = &self.live_state_store_custody {
            anyhow::ensure!(Arc::ptr_eq(original,
                &self.app_state_custody.as_ref().expect("retained AppState").state_store),
                "AppState selects a different live StateStore");
        }
        self.live_state_store_custody.take();
        self.persistent_pool_custody.take();
        Ok(())
    }

    fn app_state(&self) -> Option<&AppState> {
        self.app_state_custody.as_ref()
    }

    fn retain_native_failure(&mut self, error: anyhow::Error) -> anyhow::Error {
        // Retain actual partial owners before allocating a propagation error.
        // Do not format/copy the original failure before composition custody.
        self.native_failure = Some(error);
        anyhow::anyhow!("live native startup refused; original failure custody retained")
    }

    /// Refuse a start whose capability floor is not met on the residency
    /// its reservation established. A fact refusal inhibits the restart at
    /// once, with its reason, so an always-restart supervisor cannot spend
    /// another native incarnation on the same host. The refusal owns no
    /// native owner; it is retained typed so recovery releases exactly the
    /// reservation it followed.
    fn refuse_established_residency(&mut self, error: anyhow::Error) -> anyhow::Error {
        let detail = format!("{error:#}");
        tracing::error!(error = %detail, "capability floor refused after native reservation");
        if lillux_sqlite::native_refusal_is_fact(&error) {
            self.inhibit_supervised_restart_with_detail("capability_floor_refused", Some(&detail));
        }
        self.retain_native_failure(anyhow::Error::new(EstablishedResidencyRefused(error)))
    }

    /// A reservation refused before any account owner existed: the admission
    /// interval, a signed envelope floor, or opening the capacity account.
    /// Nothing was debited, mapped or issued, so there is no cleanup budget
    /// or registry to retire the captured inputs through; they are the
    /// ordinary owners a capture or loaded-startup refusal also closes, and
    /// the sole remaining alias closes them here. A second alias would be an
    /// owner this route does not know, so it is retained for the custody exit
    /// instead of being closed under it.
    fn refuse_before_account(
        &mut self,
        inputs: Arc<ryeos_node::native_capacity::LiveNativeCapacityInputs>,
        error: anyhow::Error,
    ) -> anyhow::Error {
        match Arc::try_unwrap(inputs) {
            Ok(inputs) => {
                drop(inputs);
                error
            }
            Err(inputs) => {
                let aliases = Arc::strong_count(&inputs);
                self.native_inputs = Some(inputs);
                error.context(format!(
                    "native inputs still have {aliases} owners after a refusal before the account; \
                     original custody retained"
                ))
            }
        }
    }

    fn prepare_live_native(
        &mut self,
        config: &Config,
        identity: &NodeIdentity,
        policy: &ryeos_node_runtime::node_policy::NodePolicySnapshot,
    ) -> Result<(
        Arc<lillux_sqlite::OriginalNativeFactory>,
        lillux::time::MonotonicDeadline,
        Arc<state_lock::StateLockLease>,
    )> {
        anyhow::ensure!(!self.native_bootstrap_entered && self.native_inputs.is_none()
            && self.native_reservation.is_none() && self.native_factory.is_none()
            && self.native_failure.is_none() && self.native_outside_store.is_none(),
            "live native startup already entered; retain original attempt");
        // Claim before any fallible preparation. Even a pre-owner refusal may
        // not renew the original interval by entering another capture attempt.
        self.native_bootstrap_entered = true;
        let exclusion = Arc::new(self.process_state_lock.as_ref()
            .context("daemon state lock is absent")?.retain());
        let inputs = ryeos_node::native_capacity::LiveNativeCapacityInputs::capture(
            &config.app_root, Arc::clone(&exclusion),
        )?;
        inputs.require_loaded_startup(identity, policy)?;
        // The composition's alias of the captured inputs is retained only
        // once a typed reservation owner holds the other reference: the
        // settled chain takes it back by pointer identity and finalizes it
        // through that owner's cleanup budget. A refusal before any account
        // owner exists has no such chain, so the alias is settled here.
        let alias = Arc::clone(&inputs);
        let reservation = match ryeos_node::native_capacity::LiveNativeCapacityReservation::reserve(inputs) {
            Ok(reservation) => {
                self.native_inputs = Some(alias);
                reservation
            }
            Err(error) => match error.downcast::<ryeos_node::native_capacity::LiveNativeCapacityReservationFailure>() {
                Ok(failure) => {
                    // Only failures after an actual reservation checkout carry
                    // this affine owner. Quote/input refusals have no such owner.
                    self.native_inputs = Some(alias);
                    self.native_reservation_close_failure = Some(failure);
                    // Describe the refusal only after its owner is retained.
                    let detail = self.native_reservation_close_failure.as_ref()
                        .map(|failure| format!("{failure:?}"))
                        .unwrap_or_default();
                    anyhow::bail!("live native reservation refused; original recovery custody retained: {detail}");
                }
                Err(error) => return Err(self.refuse_before_account(alias, error)),
            }
        };
        self.native_reservation = Some(Arc::clone(&reservation));
        // Name the epoch this start debited once; a summary that cannot be
        // read is logged, not refused, because every later step re-proves
        // the account itself.
        match reservation.capacity_summary() {
            Ok(summary) => tracing::info!(
                epoch = summary.epoch,
                maximum_epochs = summary.maximum_epochs,
                incarnations_remaining = summary.incarnations_remaining,
                recovery_attempts_used = summary.recovery_attempts_used,
                "native capacity debited"
            ),
            Err(error) => tracing::warn!(error = %format!("{error:#}"), "native capacity summary unavailable"),
        }
        // Re-decide the capability floor on the residency this reservation
        // actually established, before anything is installed, armed,
        // initialized or admitted against it: a lock the kernel refused can
        // leave a weaker basis than the plan the floor was admitted on.
        let floor = record_established_residency(
            reservation.residency_proof(),
            ryeos_node::capability_report::SignedNodeCapabilityReport::read_verified(&config.app_root),
            identity,
            &config.app_root,
        );
        // Bind the previous stop's unapplied facts as soon as this start's
        // debit is durable, before any further preparation can fail or the
        // process be killed: once the debit settles the carrier, only this
        // start's binding keeps them acceptable to the next one. Binding
        // admits no work, so it precedes even a floor refusal, which would
        // otherwise drop those facts.
        self.bind_previous_latched_drain(&reservation)?;
        if let Err(error) = floor {
            return Err(self.refuse_established_residency(error));
        }
        let factory = match ryeos_node::native_capacity::LiveNativeFactory::install(reservation) {
            Ok(factory) => factory,
            Err(error) => return Err(self.retain_native_failure(error)),
        };
        self.native_admission_fence.register_factory(&factory);
        self.native_factory = Some(Arc::clone(&factory));
        if let Err(error) = self.arm_service_stop(&factory) {
            return Err(self.retain_native_failure(error));
        }
        if let Err(error) = factory.initialize() {
            return Err(self.retain_native_failure(error));
        }
        let ready = factory.ready_factory().map_err(|error| self.retain_native_failure(error))?;
        let deadline = factory.original_sql_deadline().map_err(|error| self.retain_native_failure(error))?;
        Ok((ready, deadline, exclusion))
    }

    /// Wire this incarnation's service latch and planned window to an orderly
    /// stop. The latch wake and the margin wake only store an atomic and wake
    /// a task, so they are safe from inside SQLite's commit path; the task
    /// requests the ordinary graceful shutdown. When the task sees a fact
    /// bit it records the restart inhibition at once, before the drain, so a
    /// kill during the drain still leaves the next start refused.
    fn arm_service_stop(
        &mut self,
        factory: &Arc<ryeos_node::native_capacity::LiveNativeFactory>,
    ) -> Result<()> {
        let state_dir = self
            .lifecycle_exit
            .as_ref()
            .context("daemon lifecycle owner is absent")?
            .state_dir
            .clone();
        let signal = Arc::clone(&self.service_stop.signal);
        let latched = Arc::clone(&signal);
        factory.register_service_refusal_wake(Box::new(move || {
            ryeosd::refuse_service_sql();
            latched.latched.store(true, std::sync::atomic::Ordering::SeqCst);
            latched.notify.notify_one();
        }))?;
        let margin = Arc::clone(&signal);
        factory.arm_service_window_timers(Box::new(move || {
            margin.margin_reached.store(true, std::sync::atomic::Ordering::SeqCst);
            margin.notify.notify_one();
        }))?;
        let latch = factory.service_refusal_latch();
        self.service_stop.latch = Some(Arc::clone(&latch));
        tokio::spawn(async move {
            let mut stop_requested = false;
            loop {
                signal.notify.notified().await;
                if !stop_requested {
                    tracing::warn!(
                        latched = signal.latched.load(std::sync::atomic::Ordering::SeqCst),
                        margin_reached = signal.margin_reached.load(std::sync::atomic::Ordering::SeqCst),
                        "native service latch or planned window requests an orderly stop"
                    );
                    ryeosd::request_shutdown();
                    stop_requested = true;
                }
                if latch.bits() & ryeos_node::native_capacity::service_refusal::FACT != 0 {
                    record_service_refusal_inhibition(&state_dir);
                    break;
                }
            }
        });
        Ok(())
    }

    /// Once this start's capacity debit is durable, bind the previous stop's
    /// unapplied latched-drain facts to it. A refused marker write fails the
    /// start closed and leaves the marker in place.
    fn bind_previous_latched_drain(
        &mut self,
        reservation: &ryeos_node::native_capacity::LiveNativeCapacityReservation,
    ) -> Result<()> {
        let lifecycle = self
            .lifecycle_exit
            .as_ref()
            .context("daemon lifecycle owner is absent")?;
        let (state_dir, started_at) = (lifecycle.state_dir.clone(), lifecycle.started_at.clone());
        let (own, predecessor) = reservation.incarnation_lineage()?;
        self.service_stop.incarnation_digest = Some(own);
        self.service_stop.pending_latched_drain = lifecycle_marker::bind_pending_latched_drain(
            &state_dir,
            &started_at,
            &own,
            predecessor.as_ref(),
        )
        .context("bind previous latched-drain facts to this start")?;
        Ok(())
    }

    /// Drop the previous stop's latched-drain facts after reconciliation
    /// applied them. The only write that removes them.

    /// Durably record the facts a latched drain deferred to the next start,
    /// as this run's provisional exit, before the cleanup chain. A refused
    /// write holds: the resume re-arm cannot be deferred without it.
    fn record_latched_drain_facts(&mut self, outcome: DrainOutcome) -> DrainOutcome {
        let DrainOutcome::ProcessCleanRowsDeferred(deferred) = outcome else {
            return outcome;
        };
        if deferred.resume_reset.is_empty() {
            return DrainOutcome::ProcessCleanRowsDeferred(deferred);
        }
        let stop = self.service_stop.outcome().unwrap_or(ServiceStopOutcome::ServiceRefusalStop);
        let written = (|| -> Result<()> {
            let digest = self
                .service_stop
                .incarnation_digest
                .context("latched drain has no bound incarnation digest")?;
            let facts = lifecycle_marker::LatchedDrainFacts {
                outcome: stop.marker_outcome(),
                incarnation_digest: lifecycle_marker::incarnation_digest_hex(&digest),
                resume_attempts_reset_deferred: deferred.resume_reset.clone(),
            };
            self.lifecycle_exit
                .as_mut()
                .context("daemon lifecycle owner is absent")?
                .record_provisional_latched(stop.reason(), &facts)
        })();
        match written {
            Ok(()) => DrainOutcome::ProcessCleanRowsDeferred(deferred),
            Err(error) => {
                tracing::error!(
                    error = %format!("{error:#}"),
                    "latched-drain facts were not recorded; resume re-arm is held"
                );
                DrainOutcome::Unproved
            }
        }
    }

    fn app_state_mut(&mut self) -> Option<&mut AppState> {
        self.app_state_custody.as_mut()
    }

    fn release_app_state_after_drain(&mut self) {
        // Called only after the actual supervised thread/service drain and
        // scheduler close. Native owners remain separately retained; perform
        // consuming close after run's listener/service locals leave scope.
        self.services_drained = true;
        if self.native_reservation.is_some() || self.native_factory.is_some() || self.native_failure.is_some()
            || self.native_outside_store.is_some() {
            return;
        }
        #[cfg(feature = "scheduling")]
        if self.app_state_holds_scheduler_service() {
            return;
        }
        match self.root.close() {
            ryeos_executor::SelectedRootClose::Closed => { self.app_state_custody.take(); }
            ryeos_executor::SelectedRootClose::Unresolved => {}
        }
    }

    fn retain_settled_reservation(&mut self, settled: ryeos_node::native_capacity::SettledLiveNative) -> Result<()> {
        self.native_settled_reservation = Some(settled);
        // Account settlement does not discard the still-owned input,
        // exclusion, clock or pre-mapping registry. The consuming finalizer
        // continues from this cached phase within the same attempt; every
        // refusal leaves the settled owner cached for the next attempt.
        self.advance_settled_native()
    }

    /// [`Self::advance_native_shutdown`] on a dedicated thread with an
    /// explicit stack. The settlement chain holds several large owners on the
    /// stack at once, and the calling thread's stack is whatever the process
    /// was given (a container may set a small `ulimit -s`). The composition
    /// is only borrowed, so a refused spawn or a panic leaves every owner in
    /// it, retained for the ordinary retry or the custody exit.
    fn advance_native_shutdown_on_settlement_stack(&mut self) -> Result<()> {
        std::thread::scope(|scope| {
            let settling = std::thread::Builder::new()
                .name("ryeosd-native-settlement".to_owned())
                .stack_size(NATIVE_SETTLEMENT_STACK_BYTES)
                .spawn_scoped(scope, || self.advance_native_shutdown())
                .context("start the native settlement thread; original custody retained")?;
            settling.join().map_err(|_| {
                anyhow::anyhow!("native settlement thread panicked; original custody retained")
            })?
        })
    }

    fn advance_native_shutdown(&mut self) -> Result<()> {
        self.native_finalizer_attempts.claim()?;
        if self.native_settled_reservation.is_some() {
            return self.advance_settled_native();
        }
        self.advance_unsettled_native()
    }

    fn advance_settled_native(&mut self) -> Result<()> {
        {
            let settled = self.native_settled_reservation.as_mut()
                .context("settled native reservation absent")?;
            settled.require_original_process().context("settled finalization: require_original_process")?;
            if !settled.finalization_registry_retired() {
                settled.prepare_account_finalization_sources().context("settled finalization: prepare_account_finalization_sources")?;
                settled.prepare_input_finalization_sources(&mut self.native_inputs).context("settled finalization: prepare_input_finalization_sources")?;
                if let Some(outside) = &mut self.native_outside_store {
                    settled.register_finalization_sources_in_store(outside).context("settled finalization: register_finalization_sources_in_store")?;
                    settled.retire_non_account_finalization_descriptors_in_store(outside).context("settled finalization: retire_non_account_finalization_descriptors_in_store")?;
                } else {
                    settled.register_finalization_sources_in_reservation().context("settled finalization: register_finalization_sources_in_reservation")?;
                    #[cfg(target_os = "linux")]
                    if !self.native_operator_lock_registered {
                        if self.native_operator_lock.is_none() {
                            let primary = self.process_state_lock.take()
                                .context("actual no-Store daemon operator lock absent")?;
                            match primary.into_original_owner() {
                                Ok(owner) => self.native_operator_lock =
                                    Some(ryeos_node::native_capacity::OperatorLockCustody::new(owner)),
                                Err((primary, error)) => {
                                    self.process_state_lock = Some(primary);
                                    return Err(error);
                                }
                            }
                        }
                        settled.register_operator_lock_in_reservation(
                            &mut self.native_operator_lock,
                        ).context("settled finalization: register_operator_lock_in_reservation")?;
                        anyhow::ensure!(self.native_operator_lock.is_none(),
                            "registered no-Store operator lock still has a caller alias");
                        self.native_operator_lock_registered = true;
                    }
                    settled.retire_non_account_finalization_descriptors_in_reservation().context("settled finalization: retire_non_account_finalization_descriptors_in_reservation")?;
                }
                #[cfg(target_os = "linux")]
                record_staged_exit_before_operator_retirement(&mut self.lifecycle_exit,
                    &self.staged_exit_record, &mut self.operator_target_retirement_entered);
                #[cfg(target_os = "linux")]
                if let Some(outside) = &mut self.native_outside_store {
                    settled.retire_operator_finalization_descriptors_in_store(outside).context("settled finalization: retire_operator_finalization_descriptors_in_store")?;
                } else {
                    settled.retire_operator_finalization_descriptors_in_reservation().context("settled finalization: retire_operator_finalization_descriptors_in_reservation")?;
                }
                if let Some(outside) = &mut self.native_outside_store {
                    settled.retire_finalization_registry_in_store(outside).context("settled finalization: retire_finalization_registry_in_store")?;
                } else {
                    settled.retire_finalization_registry_in_reservation().context("settled finalization: retire_finalization_registry_in_reservation")?;
                }
            }
            settled.resume_finalization_registry_phase(self.native_outside_store.is_some())?;
            if let Some(outside) = &mut self.native_outside_store {
                settled.consume_finalization_packet_destinations_in_store(outside).context("settled finalization: consume_finalization_packet_destinations_in_store")?;
                settled.consume_finalization_store_packet_bound_roots(outside).context("settled finalization: consume_finalization_store_packet_bound_roots")?;
                settled.consume_finalization_store_packet_metadata_pins(outside).context("settled finalization: consume_finalization_store_packet_metadata_pins")?;
                // Packet source frames must be exhausted before State and
                // Operational root retirement validates their receipts.
                settled.consume_finalization_store_packet_source_parts(outside).context("settled finalization: consume_finalization_store_packet_source_parts")?;
                settled.consume_finalization_store_metadata_sources(outside).context("settled finalization: consume_finalization_store_metadata_sources")?;
                settled.consume_finalization_store_parent_sources(outside).context("settled finalization: consume_finalization_store_parent_sources")?;
                settled.consume_finalization_store_state_projection_sources(outside).context("settled finalization: consume_finalization_store_state_projection_sources")?;
                settled.consume_finalization_store_state_root_sources(outside).context("settled finalization: consume_finalization_store_state_root_sources")?;
                settled.consume_finalization_store_runtime_shared_roots(outside).context("settled finalization: consume_finalization_store_runtime_shared_roots")?;
                settled.consume_finalization_store_scheduler_sources(outside).context("settled finalization: consume_finalization_store_scheduler_sources")?;
                settled.consume_finalization_store_owner_frames(outside).context("settled finalization: consume_finalization_store_owner_frames")?;
            }
            if self.native_outside_store.is_some() {
                // Cleared only on success; a refusal restores the graph.
                settled.exhaust_finalization_store(&mut self.native_outside_store).context("settled finalization: exhaust_finalization_store")?;
            } else {
                anyhow::ensure!(settled.finalization_registry_retired(),
                    "no-Store finalization destination phase precedes the original registry retirement");
            }
            settled.consume_finalization_control_sources().context("settled finalization: consume_finalization_control_sources")?;
            // The clock outlives every outside-graph consumer that prepays
            // from it; the Store graph was exhausted above.
            anyhow::ensure!(self.native_outside_store.is_none(),
                "settled Store outside graph remains before the late clock retires");
            // Competing owners are refused while the clock can still fund a
            // retry; after it retires only the final consumption remains.
            anyhow::ensure!(self.native_outside_store.is_none() && self.native_inputs.is_none()
                && self.native_reservation.is_none() && self.native_reservation_close_failure.is_none()
                && self.native_factory.is_none() && self.native_terminal.is_none()
                && self.native_prepared_registry.is_none(),
                "settled native finalization still has a competing composition owner");
            #[cfg(target_os = "linux")]
            anyhow::ensure!(self.native_operator_lock.is_none(),
                "settled native finalization retains an unregistered operator lock alias");
            settled.retire_finalization_clock().context("settled finalization: retire_finalization_clock")?;
        }
        anyhow::ensure!(self.native_outside_store.is_none() && self.native_inputs.is_none()
            && self.native_reservation.is_none() && self.native_reservation_close_failure.is_none()
            && self.native_factory.is_none() && self.native_terminal.is_none()
            && self.native_prepared_registry.is_none(),
            "settled native finalization still has a competing composition owner");
        #[cfg(target_os = "linux")]
        anyhow::ensure!(self.native_operator_lock.is_none(),
            "settled native finalization retains an unregistered operator lock alias");
        let settled = self.native_settled_reservation.take()
            .context("settled native reservation absent before final consumption")?;
        let finalized = match settled.into_finalized() {
            Ok(finalized) => finalized,
            Err((settled, error)) => {
                self.native_settled_reservation = Some(settled);
                return Err(error);
            }
        };
        // Only a non-owning release record remains; no account, clock,
        // registry or descriptor survives the final consumption above.
        drop(finalized);
        #[cfg(target_os = "linux")]
        {
            self.native_operator_lock_registered = false;
        }
        Ok(())
    }

    /// Continue a retained Store graph in the same pass: pre-seal stages while
    /// the producer remains, then the reservation release and settlement.
    fn advance_store_outside(&mut self) -> Result<()> {
        if self.native_factory.is_some() {
            self.advance_store_pre_seal()?;
        }
        self.release_store_reservation()
    }

    /// Pre-seal stages of the retained Store graph, run while the producer,
    /// factory and aggregate terminal remain valid. Scope-bound aliases of the
    /// supervisor's own owners end when this returns, before any release.
    fn advance_store_pre_seal(&mut self) -> Result<()> {
        let producer = Arc::clone(self.native_factory.as_ref().context("original live cleanup producer absent")?);
        let producer = producer.as_ref();
        let factory = producer.retained_factory_for_cleanup()?;
        let terminal = Arc::clone(self.native_terminal.as_ref().context("original live aggregate terminal absent")?);
        let terminal = &terminal;
        let outside = self.native_outside_store.as_mut()
            .context("actual Store outside graph disappeared before destination join")?;
        outside.require_original_process()?;
        if self.native_prepared_registry.is_none() && !outside.has_prepared_physical_registry() {
            self.native_prepared_registry = Some(producer.take_prepared_physical_registry(terminal)?);
        }
        // SAFETY: the actual same-factory reservation supplied this cached
        // pre-mapping registry. Every refusal keeps it in one of these two
        // concrete supervisor/source slots; no late replacement is issued.
        unsafe { outside.accept_prepared_physical_registry(
            &mut self.native_prepared_registry, &factory, terminal) }?;
        let outside = self.native_outside_store.as_mut()
            .context("actual Store outside graph disappeared before its destination join")?;
        outside.prepare_destination_join(&factory, terminal, &|bytes, steps|
            producer.prepay_outside_control(terminal, bytes, steps)).context("Store pre-seal destination join")?;
        // Scheduler and accounting packets carry the Store's live authority;
        // their leaf transfer needs the receiver prepared just above.
        #[cfg(feature = "scheduling")]
        self.advance_scheduler_sources_in_store(&factory, producer, terminal)?;
        self.advance_accounting_sources_in_store(&factory, producer, terminal)?;
        let outside = self.native_outside_store.as_mut()
            .context("actual Store outside graph disappeared during accounting handoff")?;
        outside.advance_state_projection_sources(&factory, terminal, &|bytes, steps|
            producer.prepay_outside_control(terminal, bytes, steps)).context("Store pre-seal State projection sources")?;
        outside.advance_runtime_physical_sources(&factory, terminal, &|bytes, steps|
            producer.prepay_outside_control(terminal, bytes, steps)).context("Store pre-seal Runtime physical sources")?;
        #[cfg(target_os = "linux")]
        if !self.native_operator_lock_registered {
            if self.native_operator_lock.is_none() {
                let quote = ryeos_node::native_capacity::quote_operator_lock_handoff();
                producer.prepay_outside_control(terminal, quote.bytes, quote.steps)?;
                // Startup/use has ended and the real Store is already in
                // outside custody. Consume the primary wrapper exactly once;
                // the SAME lock allocation retains target/parent/registration.
                let primary = self.process_state_lock.take()
                    .context("actual daemon operator lock absent")?;
                match primary.into_original_owner() {
                    Ok(owner) => self.native_operator_lock =
                        Some(ryeos_node::native_capacity::OperatorLockCustody::new(owner)),
                    Err((primary, error)) => {
                        self.process_state_lock = Some(primary);
                        return Err(error);
                    }
                }
            }
            let owner = self.native_operator_lock.as_ref()
                .context("actual daemon operator outside owner absent")?;
            // Any refusal leaves the whole primary cached here and the
            // same Store/registry in outside custody. No replacement grant.
            outside.register_operator_lock_alias(owner, &factory, terminal, &|bytes, steps|
                producer.prepay_outside_control(terminal, bytes, steps)).context("Store pre-seal operator lock alias")?;
            self.native_operator_lock_registered = true;
        }
        // The operator-lock and Store-parent joins below are Linux-only, so
        // no other host can reach the reservation release.
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("Store pre-seal operator and parent joins require Linux")
        }
        #[cfg(target_os = "linux")]
        {
            // The Store's original parent, including its StateLockLease
            // and factory/lease aliases, is consumed only after roots and
            // the canonical operator membership agree in this registry.
            // Nested packet continuations remain behind that barrier.
            outside.complete_store_parent_join(&factory, terminal, &|bytes, steps|
                producer.prepay_outside_control(terminal, bytes, steps),
                &|leaf, pins| producer.packet_metadata_pin_retirement_inputs(terminal, leaf, pins))
                .context("Store pre-seal parent join")?;
            // State-generated Bound pins live on State's closed connections,
            // not in packets. Retire them with the same issuer while the
            // factory and terminal remain valid, then end State's strong
            // native owners; only non-owning receipts stay in State.
            outside.retire_state_native_sources(&factory, terminal, &|bytes, steps|
                producer.prepay_outside_control(terminal, bytes, steps),
                &|leaf, pins| producer.packet_metadata_pin_retirement_inputs(terminal, leaf, pins))
                .context("Store pre-seal State native sources")?;
            // The registry now retains the canonical lock owner and the
            // Store frame retains its original lease through settlement.
            if let Some(owner) = self.native_operator_lock.take() {
                match owner.into_taken() {
                    Ok(owner) => drop(owner),
                    Err(owner) => {
                        self.native_operator_lock = Some(owner);
                        anyhow::bail!("operator lock custody is not held by this process");
                    }
                }
            }
            // Retire the actual budget facade only after the complete
            // native ACK and this Store graph's source/alias join. This is
            // repeatable on the same terminal; region release, account
            // settlement and descriptor retirement remain separate later
            // stages owned by the consuming finalizer.
            producer.retire_native_budget_handle(terminal).context("Store pre-seal budget handle")?;
            // Every pre-seal stage must be complete before the producer and
            // reservation may be released; the seal is the zero-handle census.
            self.native_outside_store.as_ref()
                .context("actual Store outside graph disappeared before its pre-seal census")?
                .require_pre_seal_complete()?;
            Ok(())
        }
    }

    /// Release the producer, factory and original reservation of a retained
    /// Store graph whose pre-seal stages completed. `release_and_settle` owns
    /// the authoritative sole-owner and zero-handle seal checks; a refusal
    /// keeps the same reservation in its typed failure slot for a retry.
    fn release_store_reservation(&mut self) -> Result<()> {
        if self.native_factory.is_some() || self.native_terminal.is_some() {
            anyhow::ensure!(self.app_state_custody.is_none() && self.live_state_store_custody.is_none()
                && self.persistent_pool_custody.is_none() && !self.has_scheduler_custody()
                && !self.has_accounting_custody()
                && !self.root.has_unresolved_owners() && self.native_failure.is_none()
                && self.native_closed_store.is_none() && self.native_store_close_failure.is_none()
                && self.native_startup_recovery_failure.is_none()
                && self.native_released_startup.is_none() && self.native_prepared_registry.is_none(),
                "Store reservation release still has a published or untransferred native owner");
            #[cfg(target_os = "linux")]
            anyhow::ensure!(self.native_operator_lock.is_none(),
                "Store reservation release retains an unregistered operator lock alias");
            self.native_outside_store.as_ref()
                .context("actual Store outside graph absent before reservation release")?
                .require_pre_seal_complete()?;
            // The aggregate terminal and then the producer hold the last
            // supervisor edges to the original factory and its reservation.
            drop(self.native_terminal.take());
            drop(self.native_factory.take());
        }
        anyhow::ensure!(self.native_reservation.is_none() || self.native_reservation_close_failure.is_none(),
            "Store reservation release has competing reservation owners");
        let reservation = if let Some(failure) = self.native_reservation_close_failure.take() {
            match failure.into_original() {
                Ok(reservation) => reservation,
                Err(failure) => {
                    self.native_reservation_close_failure = Some(failure);
                    anyhow::bail!("original Store reservation recovery transfer refused");
                }
            }
        } else {
            self.native_reservation.take()
                .context("actual original Store reservation release owner absent")?
        };
        // A served window may end in an orderly rollover; the claim is the
        // stop owner's observation and the account owner re-proves it.
        let claim = self.service_stop.rollover_claim();
        match reservation.release_and_settle(claim) {
            Ok(settled) => self.retain_settled_reservation(settled),
            Err(failure) => {
                self.native_reservation_close_failure = Some(failure);
                anyhow::bail!("original Store reservation release refused: an outside owner or native \
                    handle remains, or the region seal/settlement is pending; custody retained")
            }
        }
    }

    fn advance_unsettled_native(&mut self) -> Result<()> {
        // A refused Scheduler close, terminal caching or transfer left the
        // complete service in the retained AppState; retry them in order
        // before any Store close can proceed.
        #[cfg(feature = "scheduling")]
        if self.services_drained && self.app_state_custody.is_some() {
            self.retry_scheduler_close_and_transfer()?;
        }
        if self.native_outside_store.is_some() {
            return self.advance_store_outside().context("advance retained Store outside custody");
        }
        if self.native_failure.as_ref().is_some_and(|error| error.is::<state_store::LiveStateStoreOpenFailure>())
            || self.native_startup_recovery_failure.is_some() || self.native_released_startup.is_some() {
            anyhow::ensure!(self.app_state_custody.is_none() && self.live_state_store_custody.is_none()
                && self.persistent_pool_custody.is_none() && !self.has_scheduler_custody()
                && !self.root.has_unresolved_owners(),
                "partial Store startup still has published service consumers");
            let factory = Arc::clone(self.native_factory.as_ref().context("actual startup factory absent")?);
            factory.close_admission()?;
            if self.native_released_startup.is_none() {
                let recovered = if let Some(failure) = self.native_startup_recovery_failure.take() {
                    failure.retry()
                } else {
                    let error = self.native_failure.take().context("actual Store startup failure absent")?;
                    let failure = match error.downcast::<state_store::LiveStateStoreOpenFailure>() {
                        Ok(failure) => failure,
                        Err(error) => { self.native_failure = Some(error); anyhow::bail!("Store failure transfer refused"); }
                    };
                    failure.close_retained()
                };
                match recovered {
                    Ok(released) => self.native_released_startup = Some(released),
                    Err(failure) => {
                        self.native_startup_recovery_failure = Some(failure);
                        anyhow::bail!("actual partial Store startup cleanup pending");
                    }
                }
            }
            let native = factory.retained_factory_for_cleanup()?;
            let terminals = self.native_released_startup.as_ref().expect("retained startup release")
                .native_terminals(&native)?;
            // SAFETY: the actual supervisor above excludes published App,
            // Store, pool, scheduler and unresolved execution owners. Failed
            // startup cleanup retained every SQL owner and its true terminals.
            let leases = unsafe { self.native_released_startup.as_ref().expect("retained startup release")
                .generation_lease_terminals(&native) }?;
            self.native_terminal = Some(factory.finish_live_native(&terminals, &leases)?);
            // Retire actual terminal and namespace holders before resident seal.
            drop(leases);
            drop(terminals);
            drop(native);
            let released = self.native_released_startup.take().expect("actual startup cleanup holder");
            match released.into_outside_custody(
                self.native_terminal.as_ref().expect("actual original backend terminal")) {
                Ok(outside) => self.native_outside_store = Some(outside),
                Err(released) => {
                    self.native_released_startup = Some(released);
                    anyhow::bail!("startup cleanup graph transfer refused; original custody retained");
                }
            }
            // The whole graph is cached; continue its disposal join in this
            // same pass. Any refusal leaves it in outside custody.
            drop(factory);
            return self.advance_store_outside().context("advance retained Store outside custody");
        }
        // A factory-install preparation failure can retain a second reference
        // to this same reservation before any lower factory exists. Transfer
        // that typed custody, retiring only the duplicate outside reference.
        if self.native_factory.is_none() && self.native_failure.as_ref()
            .is_some_and(|error| error.is::<ryeos_node::native_capacity::LiveNativeCapacityReservationFailure>()) {
            anyhow::ensure!(self.app_state_custody.is_none()
                && self.live_state_store_custody.is_none()
                && self.persistent_pool_custody.is_none() && !self.has_scheduler_custody()
                && !self.root.has_unresolved_owners()
                && self.native_reservation_close_failure.is_none(),
                "reservation preparation recovery has published owners");
            let failure = self.native_failure.as_ref().expect("typed reservation error")
                .downcast_ref::<ryeos_node::native_capacity::LiveNativeCapacityReservationFailure>()
                .expect("checked reservation failure type");
            if let Some(reservation) = &self.native_reservation {
                anyhow::ensure!(std::ptr::eq(reservation.as_ref(), failure.original()),
                    "factory preparation retained a different reservation");
            }
            let error = self.native_failure.take().expect("retained reservation failure");
            match error.downcast::<ryeos_node::native_capacity::LiveNativeCapacityReservationFailure>() {
                Ok(failure) => {
                    self.native_reservation_close_failure = Some(failure);
                    self.native_reservation.take();
                }
                Err(error) => {
                    self.native_failure = Some(error);
                    anyhow::bail!("original reservation preparation transfer refused");
                }
            }
        }
        // Reservation refusal precedes factory publication and every service
        // construction. Recover that exact attempt without inventing a service
        // drain receipt or retrying bootstrap capture/debit.
        if self.native_reservation_close_failure.is_some()
            && self.native_factory.is_none() && self.native_reservation.is_none()
            && self.native_failure.is_none() && self.native_terminal.is_none()
            && self.native_closed_store.is_none() && self.native_store_close_failure.is_none()
            && self.native_outside_store.is_none() {
            anyhow::ensure!(self.app_state_custody.is_none()
                && self.live_state_store_custody.is_none()
                && self.persistent_pool_custody.is_none() && !self.has_scheduler_custody()
                && !self.root.has_unresolved_owners(),
                "reservation recovery still has published composition owners");
            let failure = self.native_reservation_close_failure.take()
                .expect("retained original reservation failure");
            let reservation = match failure.into_original() {
                Ok(reservation) => reservation,
                Err(failure) => {
                    self.native_reservation_close_failure = Some(failure);
                    anyhow::bail!("original bootstrap reservation transfer refused");
                }
            };
            // A bootstrap refusal served no window: no rollover claim.
            return match reservation.release_and_settle(None) {
                Ok(settled) => self.retain_settled_reservation(settled),
                Err(failure) => {
                    self.native_reservation_close_failure = Some(failure);
                    anyhow::bail!("original bootstrap reservation settlement pending");
                }
            };
        }
        // A floor refusal on the established residency precedes factory
        // install and every service. Its error owns nothing, so release the
        // exact reservation it followed.
        if self.native_factory.is_none() && self.native_failure.as_ref()
            .is_some_and(|error| error.is::<EstablishedResidencyRefused>()) {
            anyhow::ensure!(self.app_state_custody.is_none()
                && self.live_state_store_custody.is_none()
                && self.persistent_pool_custody.is_none() && !self.has_scheduler_custody()
                && !self.root.has_unresolved_owners()
                && self.native_reservation_close_failure.is_none(),
                "refused residency reservation still has published composition owners");
            let reservation = self.native_reservation.take()
                .context("refused residency reservation absent")?;
            // The original startup error remains the run result.
            self.native_failure.take();
            // A floor refusal served no window: no rollover claim.
            return match reservation.release_and_settle(None) {
                Ok(settled) => self.retain_settled_reservation(settled),
                Err(failure) => {
                    self.native_reservation_close_failure = Some(failure);
                    anyhow::bail!("refused residency reservation settlement pending");
                }
            };
        }
        if self.native_factory.is_none() && self.native_failure.as_ref()
            .is_some_and(|error| error.is::<ryeos_node::native_capacity::LiveNativeFactoryFailure>()) {
            anyhow::ensure!(self.app_state_custody.is_none()
                && self.live_state_store_custody.is_none()
                && self.persistent_pool_custody.is_none() && !self.has_scheduler_custody()
                && !self.root.has_unresolved_owners(),
                "uninstalled issuer still has published composition owners");
            let error = self.native_failure.take().expect("actual failed install");
            let failure = match error.downcast::<ryeos_node::native_capacity::LiveNativeFactoryFailure>() {
                Ok(failure) => failure,
                Err(error) => { self.native_failure = Some(error); anyhow::bail!("original failed install transfer refused"); }
            };
            match failure.retire_uninstalled() {
                Ok(()) => {
                    let reservation = self.native_reservation.take().context("uninstalled issuer reservation absent")?;
                    // An uninstalled issuer served no window: no rollover claim.
                    return match reservation.release_and_settle(None) {
                        Ok(settled) => self.retain_settled_reservation(settled),
                        Err(failure) => {
                            self.native_reservation_close_failure = Some(failure);
                            anyhow::bail!("uninstalled issuer resident settlement pending");
                        }
                    };
                }
                Err(failure) => self.native_failure = Some(anyhow::Error::new(failure)),
            }
        }
        if let Some(failure) = self.native_failure.as_ref()
            .and_then(|error| error.downcast_ref::<ryeos_node::native_capacity::LiveNativeFactoryFailure>()) {
            // No service/database consumer can exist on this pre-Ready route.
            // The lower factory additionally refuses Ready or issued tickets.
            anyhow::ensure!(self.app_state_custody.is_none()
                && self.live_state_store_custody.is_none()
                && self.persistent_pool_custody.is_none() && !self.has_scheduler_custody()
                && !self.root.has_unresolved_owners(),
                "failed initialization still has published composition owners");
            let factory = Arc::clone(failure.original());
            if let Some(retained) = &self.native_factory {
                anyhow::ensure!(Arc::ptr_eq(retained, &factory), "foreign failed native factory");
            }
            self.native_terminal = Some(factory.finish_initialization_refusal()?);
            // Only the actual terminal allows retirement of the error's native
            // owner references. The original startup error remains the run result.
            self.native_failure.take();
            self.native_factory.take();
            self.native_terminal.take();
            drop(factory);
            let reservation = self.native_reservation.take()
                .context("failed initialization reservation absent")?;
            // A failed initialization served no window: no rollover claim.
            return match reservation.release_and_settle(None) {
                Ok(settled) => self.retain_settled_reservation(settled),
                Err(failure) => {
                    self.native_reservation_close_failure = Some(failure);
                    anyhow::bail!("failed initialization resident settlement pending");
                }
            };
        }
        // With no AppState adopted, no service was published from this
        // composition and the startup future has already been dropped.
        if !self.services_drained && self.app_state_custody.is_some() { return Ok(()); }
        #[cfg(feature = "scheduling")]
        self.retire_unadopted_scheduler_db()?;
        #[cfg(feature = "scheduling")]
        if let Some(failure) = self.scheduler_schema_failure.take() {
            match failure.close_retained() {
                Ok(released) => self.scheduler_recovered_schema = Some(released),
                Err(failure) => {
                    self.scheduler_schema_failure = Some(failure);
                    anyhow::bail!("original scheduler schema recovery remains pending");
                }
            }
        }
        #[cfg(feature = "scheduling")]
        if let Some(failure) = self.scheduler_admission_failure.take() {
            match failure.into_outside_sources() {
                Ok((sources, _error)) => {
                    self.scheduler_admission_sources = Some(sources);
                    tracing::warn!(
                        "Scheduler admission failed before native open; preserving its unused sources for Store retirement");
                }
                Err((failure, error)) => {
                    self.scheduler_admission_failure = Some(failure);
                    return Err(error);
                }
            }
        }
        #[cfg(feature = "scheduling")]
        if let Some(failure) = self.scheduler_open_failure.take() {
            anyhow::ensure!(self.native_sources.scheduler_native_finalization_owner().is_none(),
                "failed Scheduler open competes with a finalization owner");
            match failure.retry_close() {
                Ok(failure) => match failure.into_native_finalization_owner() {
                    Ok(owner) => *self.native_sources.scheduler_native_finalization_owner_mut() = Some(owner),
                    Err((failure, error)) => {
                        self.scheduler_open_failure = Some(failure);
                        return Err(error);
                    }
                },
                Err(failure) => {
                    self.scheduler_open_failure = Some(failure);
                    anyhow::bail!("failed Scheduler open has not reached a consuming close phase");
                }
            }
        }
        anyhow::ensure!(self.native_failure.is_none(), "native partial startup requires its retained recovery path");
        if self.native_factory.is_none() && self.native_reservation.is_none()
            && self.native_reservation_close_failure.is_none() { return Ok(()); }
        self.close_native_admission();
        if self.native_terminal.is_none() && self.native_reservation_close_failure.is_none() {
            let factory = self.native_factory.as_ref().context("actual live native factory absent")?;
            // Close selected invocation ownership independently; native custody
            // stays here until its own physical/account terminals complete.
            anyhow::ensure!(matches!(self.root.close(), ryeos_executor::SelectedRootClose::Closed),
                "selected invocation owners remain unresolved");
            #[cfg(feature = "scheduling")]
            anyhow::ensure!(!self.app_state_holds_scheduler_service(),
                "retained AppState still holds the selected Scheduler service");
            // The closed, empty selected root still holds the Store generation
            // it registered; release it so the Store has one consuming owner.
            self.root.release_closed_store_generation()?;
            if let Some(app) = self.app_state_custody.take() {
                self.native_admission_fence.register_store(&app.state_store);
                self.live_state_store_custody = Some(Arc::clone(&app.state_store));
                drop(app);
            }
            let native = factory.retained_factory_for_cleanup()?;
            // Close the ledger in place before its Store authority closes;
            // every later ledger alias refuses SQL. Refusal keeps the owner.
            if let Some(ledger) = &self.accounting_native {
                ledger.close().context("close live accounting ledger")?;
            }
            if self.native_closed_store.is_none() {
                let result = if let Some(failure) = self.native_store_close_failure.take() {
                    failure.retry()
                } else {
                    let store = self.live_state_store_custody.take().context("actual live Store close owner absent")?;
                    store.close_live(Arc::clone(&native))
                };
                match result {
                    Ok(closed) => self.native_closed_store = Some(closed),
                    Err(failure) => {
                        let cause = failure.to_string();
                        self.native_store_close_failure = Some(failure);
                        anyhow::bail!("actual live Store close pending; original custody retained: {cause}");
                    }
                }
            }
            let mut terminals = self.native_closed_store.as_ref().context("actual live Store terminal owner absent")?
                .native_terminals(&native)?;
            #[cfg(feature = "scheduling")]
            if let Some(scheduler) = &self.native_sources.scheduler_native_terminals() { terminals.extend(scheduler.iter().cloned()); }
            #[cfg(feature = "scheduling")]
            if let Some(released) = &self.scheduler_recovered_schema {
                if let Some(terminal) = released.terminal(&native)? { terminals.push(terminal.clone()); }
            }
            if let Some(ledger) = &self.accounting_native {
                terminals.extend(ledger.native_terminals(&native)?);
            }
            let leases = self.native_closed_store.as_ref().context("actual live Store terminal owner absent")?
                .generation_lease_terminals(&native)?;
            if let Ok(snapshot) = native.budget_snapshot() {
                tracing::info!(
                    connections_issued = snapshot.connections_issued,
                    allocator_steps = snapshot.allocator_steps,
                    cleanup_steps = snapshot.cleanup_steps,
                    terminals = terminals.len(),
                    "native budget census before aggregate shutdown"
                );
            }
            self.native_terminal = Some(factory.finish_live_native(&terminals, &leases)?);
            // These borrowed census packets have served the genuine aggregate
            // ACK. Release their aliases before consuming Scheduler terminals
            // into the retained outside source graph.
            drop(leases);
            drop(terminals);
            #[cfg(feature = "scheduling")]
            self.native_sources.scheduler_native_terminals_mut().take();
        }
        // Every native terminal is already acknowledged by the real backend.
        // Retire their Rust handles before the zero-loan resident seal.
        if self.accounting_native.is_some() {
            let aggregate = Arc::clone(self.native_terminal.as_ref()
                .context("actual native terminal absent for accounting retirement")?);
            self.take_accounting_outside_sources(&aggregate)?;
        }
        let terminal = self.native_terminal.as_ref().context("actual native terminal absent for Store retirement")?;
        #[cfg(feature = "scheduling")]
        if self.native_sources.scheduler_native_outside_sources().is_none()
            && let Some(owner) = self.native_sources.scheduler_native_finalization_owner_mut()
        {
            owner.prepare_outside_sources(terminal)?;
            // SAFETY: the exact wrapper is retained in this composition and
            // passed to the Store's typed handoff path before owner completion.
            *self.native_sources.scheduler_native_outside_sources_mut() = Some(unsafe { owner.take_outside_sources()? });
        }
        if let Some(closed) = self.native_closed_store.take() {
            match closed.into_outside_custody(terminal) {
                Ok(outside) => self.native_outside_store = Some(outside),
                Err(closed) => {
                    self.native_closed_store = Some(closed);
                    anyhow::bail!("actual closed Store graph transfer refused; original custody retained");
                }
            }
            return self.advance_store_outside().context("advance retained Store outside custody");
        }
        // Without a Store graph there is no receiver for Scheduler owners.
        anyhow::ensure!(!self.has_scheduler_custody(),
            "Scheduler owners remain without a Store outside handoff");
        anyhow::ensure!(!self.has_accounting_custody(),
            "accounting ledger owners remain without a Store outside handoff");
        self.native_terminal.take();
        self.native_factory.take();
        if let Some(failure) = self.native_reservation_close_failure.take() {
            match failure.into_original() {
                Ok(reservation) => self.native_reservation = Some(reservation),
                Err(failure) => {
                    self.native_reservation_close_failure = Some(failure);
                    anyhow::bail!("original reservation recovery transfer refused");
                }
            }
        }
        let reservation = self.native_reservation.take().context("actual original reservation release owner absent")?;
        // This route follows a served window too; the claim is the stop
        // owner's observation and the account owner re-proves it.
        let claim = self.service_stop.rollover_claim();
        match reservation.release_and_settle(claim) {
            Ok(settled) => self.retain_settled_reservation(settled),
            Err(failure) => {
                self.native_reservation_close_failure = Some(failure);
                anyhow::bail!("original resident/account settlement pending; custody retained");
            }
        }
    }

    fn has_unresolved_custody(&self) -> bool {
        self.persistent_pool_custody.is_some()
            || self.app_state_custody.is_some()
            || self.live_state_store_custody.is_some()
            || self.root.has_unresolved_owners()
            || self.has_scheduler_custody()
            || self.has_accounting_custody()
            || self.native_reservation.is_some()
            || self.native_factory.is_some()
            || self.native_failure.is_some()
            || self.native_outside_store.is_some()
            || self.native_prepared_registry.is_some()
            || self.native_closed_store.is_some()
            || self.native_store_close_failure.is_some()
            || self.native_reservation_close_failure.is_some()
            || self.native_settled_reservation.is_some()
            || self.native_terminal.is_some()
            || self.native_startup_recovery_failure.is_some()
            || self.native_released_startup.is_some()
            || self.native_inputs.is_some()
            || self.has_operator_lock_custody()
    }

    /// The consumed operator lock owner awaiting registration with the
    /// settled finalizer.
    fn has_operator_lock_custody(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.native_operator_lock.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn unresolved_native_custody(&self) -> UnresolvedNativeCustody {
        let abandoned = AbandonedNativeCustody::observe();
        UnresolvedNativeCustody {
            attempts_used: self.native_finalizer_attempts.0,
            services_drained: self.services_drained,
            persistent_pool_retained: self.persistent_pool_custody.is_some(),
            app_state_retained: self.app_state_custody.is_some(),
            live_store_retained: self.live_state_store_custody.is_some(),
            scheduler_retained: self.has_scheduler_custody(),
            accounting_retained: self.has_accounting_custody(),
            native_failure_retained: self.native_failure.is_some(),
            inputs_retained: self.native_inputs.is_some(),
            reservation_retained: self.native_reservation.is_some()
                || self.native_reservation_close_failure.is_some(),
            factory_retained: self.native_factory.is_some(),
            prepared_registry_retained: self.native_prepared_registry.is_some(),
            store_close_retained: self.native_closed_store.is_some()
                || self.native_store_close_failure.is_some(),
            store_outside_retained: self.native_outside_store.is_some(),
            settled_account_retained: self.native_settled_reservation.is_some(),
            native_terminal_retained: self.native_terminal.is_some(),
            startup_recovery_retained: self.native_startup_recovery_failure.is_some()
                || self.native_released_startup.is_some(),
            selected_root_unresolved: self.root.has_unresolved_owners(),
            operator_lock_retained: {
                #[cfg(target_os = "linux")]
                {
                    self.native_operator_lock.is_some() || self.native_operator_lock_registered
                }
                #[cfg(not(target_os = "linux"))]
                {
                    false
                }
            },
            abandoned_owners: abandoned.owners,
            fork_exclusions_held: abandoned.fork_exclusions,
        }
    }

    async fn hold_unresolved_custody(
        &mut self,
    ) -> std::result::Result<(), UnresolvedNativeCustody> {
        self.close_native_admission();
        // Retry the actual retained transition, not bootstrap or a replacement
        // service. Every effect still checks its original clock/account. An
        // expired/refused cleanup grant cannot be refreshed by this supervisor.
        while self.has_unresolved_custody() {
            if self.native_finalizer_attempts.exhausted() {
                return Err(self.unresolved_native_custody());
            }
            let retry_authorized = self.native_settled_reservation.as_ref()
                .map(|settled| settled.require_original_cleanup().is_ok()
                    || settled.finalization_clock_retired())
                .or_else(|| self.native_inputs.as_ref()
                    .map(|inputs| inputs.require_original_cleanup().is_ok()))
                .unwrap_or(false);
            if !retry_authorized {
                return Err(self.unresolved_native_custody());
            }
            if self.advance_native_shutdown_on_settlement_stack().is_err() {
                tracing::debug!("original native recovery transition remains pending");
            }
            if self.has_unresolved_custody() {
                if self.native_finalizer_attempts.exhausted() {
                    return Err(self.unresolved_native_custody());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Ok(())
    }

    /// Terminal outcome for custody that bounded finalization cannot
    /// resolve, or that was abandoned by a destructor anywhere in this
    /// process: best-effort kill the retained worker groups, record the
    /// secret-free snapshot best-effort and exit 70. `process::exit` runs no
    /// destructors, so every unresolved or abandoned owner stays resident
    /// until the kernel ends the process; cold recovery must still prove that
    /// end independently. Once operator target retirement began the exclusion
    /// may already be released, so only this run's provisional marker is
    /// replaced. The snapshot names the reason: abandonment when any owner or
    /// fork exclusion is held until exit, unresolved custody otherwise.
    fn exit_unresolved_custody(&mut self, custody: UnresolvedNativeCustody) -> ! {
        self.close_native_admission();
        let reason = custody.exit_reason();
        tracing::error!(%custody, reason,
            "native custody remains held; exiting with status 70 while original owners remain resident");
        let killed = self.kill_retained_process_groups_before_exit();
        tracing::error!(%killed, "best-effort process group kill before exit 70");
        let detail = format!("{custody}; {killed}");
        if let Some(lifecycle) = self.lifecycle_exit.as_mut() {
            if !self.operator_target_retirement_entered && !lifecycle.recorded {
                lifecycle.record(reason, Some(&detail));
            } else {
                lifecycle.complete_provisional(reason, Some(&detail));
            }
        }
        self.inhibit_supervised_restart(reason);
        std::process::exit(70)
    }

    /// A supervisor restarts any exit while its service is wanted up. After
    /// unresolved custody or a startup failure that may already have debited
    /// an incarnation, each restart would burn another incarnation and a
    /// recovery attempt. The node first records a durable restart
    /// inhibition that its own next start refuses on, which holds under any
    /// supervisor or container restart policy. A placed runit controller
    /// also sets its own service down, without signalling itself mid-exit,
    /// so the supervisor stops retrying. Recovery stays an explicit operator
    /// action. Best-effort: the exit proceeds either way and each outcome is
    /// logged. Unresolved custody can also end a graceful stop; its operator
    /// already set the service down, so this repeats that state.
    fn inhibit_supervised_restart(&mut self, reason: &str) {
        self.inhibit_supervised_restart_with_detail(reason, None);
    }

    fn inhibit_supervised_restart_with_detail(&mut self, reason: &str, detail: Option<&str>) {
        match self.lifecycle_exit.as_ref() {
            Some(lifecycle) => match lifecycle_marker::inhibit_restart(&lifecycle.state_dir, reason, detail) {
                Ok(()) => tracing::warn!(reason,
                    "restart inhibited; the next start is refused until `ryeos node allow-restart`"),
                Err(error) => tracing::error!(reason, error = %format!("{error:#}"),
                    "could not record the restart inhibition; a restart policy may restart this node"),
            },
            None => tracing::error!(reason,
                "no lifecycle state directory to record the restart inhibition in"),
        }
        let Some(app_root) = self.supervised_app_root.take() else {
            return;
        };
        let outcome = ryeos_node::supervision::InstalledService::discover_app_root(&app_root)
            .and_then(|service| {
                service
                    .context("host service association disappeared before restart inhibition")?
                    .inhibit_restart()
            });
        match outcome {
            Ok(()) => tracing::warn!(reason,
                "host service set down before exit; restart requires an explicit operator start"),
            Err(error) => tracing::error!(reason, error = %format!("{error:#}"),
                "could not set host service down before exit; the supervisor may restart this node"),
        }
    }

    /// A capability floor refusal inhibits the restart at any stage. Native
    /// bootstrap claims its original attempt before the incarnation
    /// debit, so a startup failure from that point on may have consumed it.
    /// Otherwise only a fact refusal inhibits the restart: a fact bit on the service
    /// latch, or a failure that is not momentary contention, the planned
    /// edge or a transient shortage of host resources. A transient failure
    /// is retried by the next start rather than stopping the node for good.
    fn inhibit_supervised_restart_after_startup_failure(
        &mut self,
        reason: &str,
        failure: Option<&anyhow::Error>,
    ) {
        if reason != "startup_failed" {
            return;
        }
        // A capability floor refusal is a deterministic fact of this host and
        // policy, so it inhibits even before native bootstrap, where a start
        // spends nothing but would otherwise repeat the same refusal forever.
        if let Some(refusal) =
            failure.and_then(ryeos_node::capability_report::CapabilityFloorRefused::find)
        {
            let detail = refusal.to_string();
            self.inhibit_supervised_restart_with_detail("capability_floor_refused", Some(&detail));
            return;
        }
        if !self.native_bootstrap_entered {
            return;
        }
        let latched_fact = self.service_stop.latch.as_ref().is_some_and(|latch| {
            latch.bits() & ryeos_node::native_capacity::service_refusal::FACT != 0
        });
        let refused_fact = failure.is_some_and(lillux_sqlite::native_refusal_is_fact);
        if latched_fact || refused_fact {
            // The record names the refusal, so an operator reading it can
            // tell an exhausted allowance from any other durable fact.
            let detail = failure.map(|failure| format!("{failure:#}"));
            self.inhibit_supervised_restart_with_detail(reason, detail.as_deref());
        } else {
            tracing::warn!(
                reason,
                "startup failed without a fact refusal; the restart is not inhibited"
            );
        }
    }

    /// Some workers clear their parent-death signal, so exit 70 would leave
    /// them running once the exclusion releases. Kill the retained pool groups
    /// and every attached execution and live dedicated worker group through
    /// the exact identities recorded in memory with their durable attach. No
    /// database is read, so a wedged or refused store cannot block the exit.
    /// Nothing here is cleanup proof.
    fn kill_retained_process_groups_before_exit(&self) -> String {
        let pool = self
            .app_state_custody
            .as_ref()
            .map(|app| Arc::clone(&app.persistent_sessions))
            .or_else(|| self.persistent_pool_custody.clone());
        let pool_pids = pool
            .map(|pool| pool.kill_retained_process_groups_before_exit())
            .unwrap_or_default();
        let store = self
            .app_state_custody
            .as_ref()
            .map(|app| Arc::clone(&app.state_store))
            .or_else(|| self.live_state_store_custody.clone());
        let mut execution_groups = Vec::new();
        if let Some(store) = store {
            let attached = store
                .attached_process_snapshot()
                .into_iter()
                .filter_map(|record| record.process_identity);
            let workers = store
                .live_worker_snapshot()
                .into_iter()
                .map(|worker| worker.process_identity);
            // Bounded, so many groups cannot hold the exit open.
            const EXIT_KILL_LIMIT: Duration = Duration::from_secs(2);
            let deadline = Instant::now() + EXIT_KILL_LIMIT;
            for identity in attached.chain(workers) {
                if Instant::now() >= deadline {
                    break;
                }
                let result = process::kill_by_action(&identity, process::ShutdownAction::Hard);
                execution_groups.push(format!("{}:{}", identity.pgid(), result.method));
            }
        }
        let pool_pids = pool_pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "best-effort killed execution groups [{}]; pool group owner pids [{pool_pids}]",
            execution_groups.join(",")
        )
    }

    fn exit_unresolved_custody_sync(&mut self) -> ! {
        let custody = self.unresolved_native_custody();
        self.exit_unresolved_custody(custody)
    }

    fn arm_retained_app_generation(&mut self) -> Result<()> {
        let app = self
            .app_state_custody
            .as_ref()
            .context("actual AppState custody is absent")?;
        self.process_state_lock
            .as_ref()
            .context("daemon state lock is absent")?
            .ensure_protects_app_root(&app.config.app_root)?;
        let lifecycle = self
            .lifecycle_exit
            .as_ref()
            .context("daemon lifecycle owner is absent")?;
        anyhow::ensure!(
            lifecycle.state_dir
                == ryeos_engine::roots::RuntimeRoot::new(app.config.app_root.clone()).state(),
            "daemon lifecycle owner belongs to a different App root"
        );
        self.root.arm_from_app_generation(app)
    }

    fn close_native_admission(&self) {
        // Fence metadata even during partial startup, before the StateStore
        // has transferred into AppState. This grants no native terminal.
        self.native_admission_fence.mark_closed();
        if let Some(store) = &self.live_state_store_custody {
            store.close_live_scheduler_metadata_admission();
        }
        if let Some(app) = &self.app_state_custody {
            app.state_store.close_live_scheduler_metadata_admission();
        }
        if let Some(factory) = &self.native_factory {
            self.native_admission_fence.close_factory(factory);
        }
    }

    fn observe_native_controls(&self) {
        if let Some(factory) = &self.native_factory {
            if let Ok(mut observation) = self.native_control_observation.try_lock() {
                *observation = Some(factory.producer_control_census());
            }
        }
    }

    fn close_after_run(&mut self) {
        if self.has_unresolved_custody() || has_abandoned_native_custody() {
            self.exit_unresolved_custody_sync();
        }
        self.close_native_admission();
        self.observe_native_controls();
        if matches!(
            self.root.close(),
            ryeos_executor::SelectedRootClose::Unresolved
        ) {
            self.exit_unresolved_custody_sync();
        }
    }

    /// Close selected issuance as soon as shutdown starts. Captured immutable
    /// publications remain readable, but their attachment can no longer admit
    /// another selected owner. Existing owners still get the normal drain.
    fn close_root_admission(&self) {
        self.close_native_admission();
        let _ = self.root.close();
    }

    /// `failure` is the error that ended the run, when there is one; it
    /// decides whether a startup failure inhibits the restart.
    fn record_exit(&mut self, reason: &str, error: Option<&str>, failure: Option<&anyhow::Error>) {
        // Root closure and Render drain are distinct: callers reach the normal
        // exit only after complete_shutdown_drain has returned, never on HOLD.
        self.close_after_run();
        // A marker recorded before operator target retirement is final unless
        // it is this run's provisional finalizing marker.
        if let Some(lifecycle) = self.lifecycle_exit.as_mut() {
            if !lifecycle.recorded {
                lifecycle.record(reason, error);
            } else {
                lifecycle.complete_provisional(reason, error);
            }
        }
        self.inhibit_supervised_restart_after_startup_failure(reason, failure);
    }
}

/// The capability floor refused the residency a native reservation
/// established. It carries no native owner.
#[derive(Debug)]
struct EstablishedResidencyRefused(anyhow::Error);

impl std::fmt::Display for EstablishedResidencyRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:#}", self.0)
    }
}

impl std::error::Error for EstablishedResidencyRefused {}

/// Record the residency proof a reservation established in the signed
/// capability report, re-deciding every residency name and the floor from
/// it, and refuse when the floor is not met on it. A reservation with no
/// proof, or a start with no verified report naming its floor, is refused:
/// the floor cannot be decided, so it is not met.
fn record_established_residency(
    proof: Option<lillux::process_control::ResidencyProof>,
    report: Result<Option<ryeos_node::capability_report::SignedNodeCapabilityReport>>,
    identity: &NodeIdentity,
    app_root: &std::path::Path,
) -> Result<()> {
    let proof = proof.context(
        "startup refused after native reservation: the reservation established no residency \
         proof, so the capability floor cannot be decided",
    )?;
    let signed = report
        .context("startup refused after native reservation: read the node capability report")?
        .context(
            "startup refused after native reservation: no verified node capability report records \
             the floor this start was admitted on",
        )?;
    let established = signed.with_established_residency(proof, identity.signing_key())?;
    established.write(app_root)?;
    established.report.require_floor()
}

impl Drop for DaemonSelectedComposition {
    fn drop(&mut self) {
        if self.has_unresolved_custody() || has_abandoned_native_custody() {
            // Drop cannot await or claim another attempt, and an abandoned
            // owner has none to claim. Exit while every field is still
            // resident rather than letting Rust drop them.
            self.exit_unresolved_custody_sync();
        }
        self.close_after_run();
        // Record the fallback while the exact exclusion owner is still held.
        if let Some(lifecycle) = self.lifecycle_exit.as_mut()
            && !lifecycle.recorded
        {
            lifecycle.record("startup_failed", None);
            self.inhibit_supervised_restart_after_startup_failure("startup_failed", None);
        }
        // Fields then drop root -> same exclusion -> lifecycle. No replacement
        // lock, native owner, or successful selected settlement is constructed.
    }
}

/// Record the provisional exit while the original exclusion is still held,
/// just before the settled finalizer retires the operator target.
#[cfg(target_os = "linux")]
fn record_staged_exit_before_operator_retirement(
    lifecycle: &mut Option<LifecycleExitGuard>,
    staged: &Option<(&'static str, Option<String>)>,
    entered: &mut bool,
) {
    if let Some(lifecycle) = lifecycle.as_mut()
        && !lifecycle.recorded
    {
        match staged {
            Some((reason, detail)) => lifecycle.record_provisional(reason, detail.as_deref()),
            None => lifecycle.record_provisional("runtime_error", None),
        }
    }
    *entered = true;
}

/// Lifecycle outcome of a stop the native service latch or the planned
/// service window caused. Each has its own log line, marker reason and exit
/// status, distinct from a crash, a startup failure, an operator stop and
/// each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceStopOutcome {
    /// A durable admission fact was refused. Supervised restart is inhibited,
    /// so no further incarnation is debited without an explicit start.
    ServiceRefusalStop,
    /// The planned service-window edge, or its drain margin, was reached.
    /// Restart is not inhibited; the next incarnation is debited as usual.
    ServiceWindowRollover,
}

impl ServiceStopOutcome {
    fn reason(self) -> &'static str {
        match self {
            Self::ServiceRefusalStop => "service_refusal_stop",
            Self::ServiceWindowRollover => "service_window_rollover",
        }
    }

    fn exit_code(self) -> i32 {
        match self {
            Self::ServiceRefusalStop => 76,
            Self::ServiceWindowRollover => 75,
        }
    }

    fn marker_outcome(self) -> lifecycle_marker::LatchedStopOutcome {
        match self {
            Self::ServiceRefusalStop => lifecycle_marker::LatchedStopOutcome::ServiceRefusalStop,
            Self::ServiceWindowRollover => {
                lifecycle_marker::LatchedStopOutcome::ServiceWindowRollover
            }
        }
    }
}

/// Wake shared by the latch callback, the margin timer and the stop task.
/// Writers only store an atomic and notify.
#[derive(Default)]
struct ServiceStopSignal {
    latched: std::sync::atomic::AtomicBool,
    margin_reached: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

#[derive(Default)]
struct ServiceStopState {
    signal: Arc<ServiceStopSignal>,
    // The latch alone, never the factory: reading it keeps no native owner.
    latch: Option<Arc<ryeos_node::native_capacity::ServiceRefusalLatch>>,
    incarnation_digest: Option<[u8; 32]>,
    // Previous stop's facts bound to this start and not yet applied.
    pending_latched_drain: Option<lifecycle_marker::LatchedDrainFacts>,
    // Recorded only when the process actually exits after its drain.
    outcome: Option<ServiceStopOutcome>,
}

impl ServiceStopState {
    fn drain_latch(&self) -> DrainLatch {
        DrainLatch(self.latch.clone())
    }

    /// A fact bit wins over the planned window and over an operator stop.
    fn outcome(&self) -> Option<ServiceStopOutcome> {
        use ryeos_node::native_capacity::service_refusal;
        let bits = self.latch.as_ref().map_or(0, |latch| latch.observe());
        if bits & service_refusal::FACT != 0 {
            Some(ServiceStopOutcome::ServiceRefusalStop)
        } else if bits & service_refusal::WINDOW_EDGE != 0
            || self.signal.margin_reached.load(std::sync::atomic::Ordering::SeqCst)
        {
            Some(ServiceStopOutcome::ServiceWindowRollover)
        } else {
            None
        }
    }

    /// A claim only when the stop is the planned window and no fact bit
    /// latched. The claim is advisory: the account owner re-proves the window
    /// and the policy from its own retained inputs before it opens an epoch.
    fn rollover_claim(&self) -> Option<ryeos_node::native_capacity::RolloverClaim> {
        use ryeos_node::native_capacity::service_refusal;
        let bits = self.latch.as_ref().map_or(0, |latch| latch.observe());
        (bits & service_refusal::FACT == 0
            && matches!(self.outcome(), Some(ServiceStopOutcome::ServiceWindowRollover)))
            .then_some(ryeos_node::native_capacity::RolloverClaim { latch_bits: bits })
    }
}

/// Record the restart inhibition for a fact refusal the service latch just
/// reported. The first recorded cause is kept, so the stop's later record of
/// the same refusal changes nothing.
fn record_service_refusal_inhibition(state_dir: &std::path::Path) {
    match lifecycle_marker::inhibit_restart(
        state_dir,
        ServiceStopOutcome::ServiceRefusalStop.reason(),
        Some("native service refused a durable admission fact"),
    ) {
        Ok(()) => tracing::warn!(
            "restart inhibited before the drain; the next start is refused until `ryeos node allow-restart`"
        ),
        Err(error) => tracing::error!(
            error = %format!("{error:#}"),
            "could not record the restart inhibition before the drain; the stop records it again after"
        ),
    }
}

/// How a drain step disposes of work: `Normal` writes its durable rows;
/// `Latched` writes no service SQL, proves process death from in-memory
/// identity records and defers only obligations next-start recovery
/// reconciles from durable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainMode {
    Normal,
    Latched,
}

/// The service latch as one stop reads it. Any bit, fact or planned edge,
/// switches the remaining steps to `Latched`; the switch never reverses.
#[derive(Clone, Default)]
struct DrainLatch(Option<Arc<ryeos_node::native_capacity::ServiceRefusalLatch>>);

impl DrainLatch {
    fn mode(&self) -> DrainMode {
        match &self.0 {
            Some(latch) if latch.observe() != 0 => DrainMode::Latched,
            _ => DrainMode::Normal,
        }
    }

    fn refresh(&self, mode: &mut DrainMode) -> DrainMode {
        if *mode == DrainMode::Normal && self.mode() == DrainMode::Latched {
            tracing::warn!("service latch set during the stop; remaining drain steps write no service SQL");
            *mode = DrainMode::Latched;
        }
        *mode
    }
}

/// Obligations a latched drain left to next-start recovery, one list per
/// kind. Every listed process was proved dead in this drain.
#[derive(Debug, Default)]
struct DeferredRows {
    /// Attached-process settlement (rows reconciled by active-thread
    /// recovery), with the identity proved dead.
    attached: std::collections::BTreeMap<String, process::ExecutionProcessIdentity>,
    /// Resume-budget re-arm, with the identity proved dead; carried by the
    /// lifecycle marker to the next start.
    resume_reset: std::collections::BTreeMap<String, process::ExecutionProcessIdentity>,
    /// Dedicated worker fence and resource-owner proof (worker recovery).
    workers: Vec<String>,
    /// Pool owners whose durable settlement was left (resource-owner and
    /// qualification Worker recovery).
    pooled_owners: usize,
    /// The ownerless in-process reservation audit was skipped.
    in_process_audit_skipped: bool,
    /// In-process handlers whose owner ended without a confirmed terminal.
    in_process_owner_lost: Vec<String>,
}

/// Result of the shutdown drain.
#[derive(Debug)]
enum DrainOutcome {
    Clean,
    Unproved,
    ProcessCleanRowsDeferred(DeferredRows),
}

enum ShutdownDrainDisposition {
    Drained,
    ProcessCleanRowsDeferred {
        deferred: DeferredRows,
    },
    HoldUnproved {
        drain_error: String,
        prior_daemon_error: Option<String>,
    },
}

fn map_shutdown_drain_join(
    joined: std::result::Result<DrainOutcome, tokio::task::JoinError>,
) -> std::result::Result<DrainOutcome, String> {
    joined.map_err(|error| format!("{error}"))
}

fn shutdown_drain_disposition(
    drain: std::result::Result<DrainOutcome, String>,
    prior_daemon_result: &Result<()>,
) -> ShutdownDrainDisposition {
    let prior_daemon_error = prior_daemon_result
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"));
    match drain {
        Ok(DrainOutcome::Clean) => ShutdownDrainDisposition::Drained,
        Ok(DrainOutcome::ProcessCleanRowsDeferred(deferred)) => {
            ShutdownDrainDisposition::ProcessCleanRowsDeferred { deferred }
        }
        Ok(DrainOutcome::Unproved) => ShutdownDrainDisposition::HoldUnproved {
            drain_error: "shutdown coordinator reported unresolved process custody".to_owned(),
            prior_daemon_error,
        },
        Err(error) => ShutdownDrainDisposition::HoldUnproved {
            drain_error: format!("shutdown coordinator task failed: {error}"),
            prior_daemon_error,
        },
    }
}

async fn hold_shutdown_owners<T>(owners: T) -> ! {
    // The awaited future owns this tuple forever. This intentionally has no
    // retry, timer, or endpoint contact.
    let _retained_owners = owners;
    std::future::pending::<()>().await;
    unreachable!("pending custody hold cannot complete")
}

async fn complete_shutdown_drain<T>(
    drain: std::result::Result<DrainOutcome, String>,
    prior_daemon_result: &Result<()>,
    custody: &mut Option<T>,
) {
    match shutdown_drain_disposition(drain, prior_daemon_result) {
        ShutdownDrainDisposition::Drained => {
            drop(custody.take());
        }
        ShutdownDrainDisposition::ProcessCleanRowsDeferred { deferred } => {
            // Every process was proved dead; the listed rows are reconciled
            // by the next start's recovery, so the cleanup chain continues.
            let DeferredRows {
                attached,
                resume_reset,
                workers,
                pooled_owners,
                in_process_audit_skipped,
                in_process_owner_lost,
            } = deferred;
            tracing::warn!(
                shutdown_outcome = "process_clean_rows_deferred",
                attached = ?attached.keys().collect::<Vec<_>>(),
                resume_reset = ?resume_reset.keys().collect::<Vec<_>>(),
                workers = ?workers,
                pooled_owners,
                in_process_audit_skipped,
                in_process_owner_lost = ?in_process_owner_lost,
                "latched drain proved every process dead and deferred its durable rows to next-start recovery"
            );
            drop(custody.take());
        }
        ShutdownDrainDisposition::HoldUnproved {
            drain_error,
            prior_daemon_error,
        } => {
            tracing::error!(
                shutdown_outcome = "cleanup_unproved",
                drain_error = %drain_error,
                prior_daemon_error = ?prior_daemon_error,
                "daemon remains alive holding persistent-session cleanup custody"
            );
            hold_shutdown_owners(
                custody
                    .take()
                    .expect("shutdown custody owner remains present until drain disposition"),
            )
            .await;
        }
    }
}

fn service_descriptors() -> &'static [ryeos_app::service_registry::ServiceDescriptor] {
    static DESCRIPTORS: once_cell::sync::Lazy<Vec<ryeos_app::service_registry::ServiceDescriptor>> =
        once_cell::sync::Lazy::new(|| {
            ryeos_api::handlers::ALL
                .iter()
                .chain(ryeos_ui::handlers::ALL.iter())
                .copied()
                .collect()
        });
    &DESCRIPTORS
}

fn build_service_registry() -> Result<ryeos_app::service_registry::ServiceRegistry> {
    let mut refs = BTreeSet::new();
    let mut endpoints = BTreeSet::new();
    for descriptor in service_descriptors() {
        anyhow::ensure!(
            refs.insert(descriptor.service_ref),
            "duplicate compiled service ref `{}`",
            descriptor.service_ref
        );
        anyhow::ensure!(
            endpoints.insert(descriptor.endpoint),
            "duplicate compiled service endpoint `{}`",
            descriptor.endpoint
        );
    }
    Ok(ryeos_api::registry::build_service_registry_from(
        service_descriptors(),
    ))
}

enum ServiceCatalogCheck {
    Healthy,
    Failed(String),
}

fn check_service_catalog_entry(
    desc: &ryeos_app::service_registry::ServiceDescriptor,
    resolved: Result<ryeos_engine::contracts::ResolvedItem, ryeos_engine::error::EngineError>,
    engine: &ryeos_engine::engine::Engine,
    plan_ctx: &ryeos_engine::contracts::PlanContext,
    services: &ryeos_app::service_registry::ServiceRegistry,
) -> ServiceCatalogCheck {
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(error) => {
            let message = error.to_string();
            tracing::error!(
                service = desc.service_ref,
                error = %message,
                "installed operational service resolution FAILED"
            );
            return ServiceCatalogCheck::Failed(message);
        }
    };
    let verified = match engine.verify(plan_ctx, resolved) {
        Ok(verified) => verified,
        Err(error) => {
            let message = error.to_string();
            tracing::error!(
                service = desc.service_ref,
                error = %message,
                "operational service verification FAILED"
            );
            return ServiceCatalogCheck::Failed(message);
        }
    };
    let endpoint = match ryeos_api::registry::extract_endpoint(&verified.resolved.metadata.extra) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            let message = format!("endpoint extraction failed: {error}");
            tracing::error!(service = desc.service_ref, %error, %message);
            return ServiceCatalogCheck::Failed(message);
        }
    };
    if endpoint != desc.endpoint {
        let message = format!(
            "installed item names endpoint `{endpoint}` but compiled handler descriptor names `{}`",
            desc.endpoint
        );
        tracing::error!(
            service = desc.service_ref,
            installed_endpoint = %endpoint,
            compiled_endpoint = desc.endpoint,
            %message
        );
        return ServiceCatalogCheck::Failed(message);
    }
    let mut signed_caps =
        ryeos_app::service_registry::extract_required_caps(&verified.resolved.metadata.extra);
    signed_caps.sort();
    signed_caps.dedup();
    let mut compiled_caps = desc
        .required_caps
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    compiled_caps.sort();
    compiled_caps.dedup();
    if signed_caps != compiled_caps {
        let message = format!(
            "signed capability requirements {signed_caps:?} differ from compiled descriptor assertion {compiled_caps:?}"
        );
        tracing::error!(service = desc.service_ref, %message);
        return ServiceCatalogCheck::Failed(message);
    }
    if !services.has(&endpoint) {
        let message =
            format!("service resolves to endpoint '{endpoint}' but no handler registered");
        tracing::error!(service = desc.service_ref, %endpoint, %message);
        return ServiceCatalogCheck::Failed(message);
    }

    tracing::debug!(
        service = desc.service_ref,
        trust_class = ?verified.trust_class,
        %endpoint,
        "operational service verified"
    );
    ServiceCatalogCheck::Healthy
}

fn installed_service_refs(
    engine: &ryeos_engine::engine::Engine,
) -> Result<Vec<ryeos_engine::canonical_ref::CanonicalRef>> {
    let roots = engine.resolution_roots(None);
    ryeos_engine::item_resolution::enumerate_in_process_registry_refs(
        &roots,
        &engine.kinds,
        ryeos_engine::kind_registry::InProcessRegistryKind::Services,
    )
    .context("enumerate installed Services-registry items")
}

fn build_route_table(
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
    ui: std::sync::Arc<ryeos_ui::UiState>,
    kinds: &ryeos_engine::kind_registry::KindRegistry,
) -> anyhow::Result<ryeos_api::routes::RouteTable> {
    let mut mode_registry =
        ryeos_api::routes::response_modes::ResponseModeRegistry::with_api_builtins_from(
            service_descriptors(),
            Arc::new(kinds.clone()),
        );
    let mut extensions = ryeos_api::routes::RouteExtensionRegistry {
        auth: ryeos_api::routes::invokers::AuthInvokerRegistry::with_api_builtins(),
    };
    extensions.auth.register(
        "external_occurrence",
        Arc::new(
            ryeos_api::routes::invokers::external_occurrence_invocation::ExternalOccurrenceAuthFactory,
        ),
    );
    extensions.auth.register(
        "external_channel",
        Arc::new(
            ryeos_api::routes::invokers::external_occurrence_invocation::ExternalChannelAuthFactory,
        ),
    );

    ryeos_ui::register_extensions(&mut extensions, &mut mode_registry, ui);

    ryeos_api::routes::build_route_table_from_snapshot_with_extensions(
        snapshot,
        &mode_registry,
        &extensions,
    )
    .map_err(|errors| {
        let msgs: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
        anyhow::anyhow!(
            "route table build failed at startup ({} error(s)): {}",
            errors.len(),
            msgs.join("; ")
        )
    })
}

fn prospective_node_config_validator(
    ui: Arc<ryeos_ui::UiState>,
    engine: Arc<ryeos_engine::engine::Engine>,
) -> Arc<ryeos_app::prospective_admission::ProspectiveNodeConfigValidator> {
    Arc::new(
        ryeos_app::prospective_admission::ProspectiveNodeConfigValidator::new(move |snapshot| {
            build_route_table(snapshot, ui.clone(), &engine.kinds)
                .map(|_| ())
                .context("prospective route-table compilation failed")
        }),
    )
}

const INSTALL_TRANSACTION_FD_ENV: &str = "RYEOS_INSTALL_TRANSACTION_FD";
const INSTALL_PREPARED_ENV: &str = "RYEOS_INSTALL_PREPARED";

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(config::DaemonCommand::InitPolicyPreflight {
        app_root,
        schema_cut,
        active_bundles,
    }) = &cli.command
    {
        ryeos_node::preflight_existing_policy_generation(app_root, *schema_cut, active_bundles)?;
        return Ok(());
    }
    if let Some(config::DaemonCommand::HostProvision {
        app_root,
        controller_account_json,
        node_resources_json,
    }) = &cli.command
    {
        lillux::require_administrator()?;
        let account: lillux::ControllerAccount = serde_json::from_str(controller_account_json)
            .context("parse explicit host controller account")?;
        let node_resources: lillux::ProcessScopeResourceLimits =
            serde_json::from_str(node_resources_json)
                .context("parse explicit host node resource envelope")?;
        let desired = ryeos_node::supervision::provision_host_service(app_root, account, node_resources)?;
        println!(
            "host service provisioned {}; use ryeos start/stop for ordinary lifecycle",
            match desired {
                ryeos_node::supervision::DesiredState::Up => "up",
                ryeos_node::supervision::DesiredState::Down => "down",
            }
        );
        return Ok(());
    }
    if let Some(config::DaemonCommand::HostInstall {
        package_root,
        action,
    }) = &cli.command
    {
        return match action {
            config::HostInstallAction::Acquire {
                installer,
                installer_digest,
                prepared,
                args,
            } => match lillux::exec_install_transaction(
                package_root,
                std::path::Path::new("/usr/bin/bash"),
                installer,
                installer_digest,
                args,
                &[(
                    std::ffi::OsString::from(INSTALL_PREPARED_ENV),
                    std::ffi::OsString::from(if *prepared { "1" } else { "0" }),
                )],
                INSTALL_TRANSACTION_FD_ENV,
            ) {
                Ok(never) => match never {},
                Err(error) => Err(error),
            },
            config::HostInstallAction::Validate { transaction_fd } => {
                lillux::validate_install_transaction(package_root, *transaction_fd)
            }
        };
    }
    if let Some(config::DaemonCommand::HostUpgrade {
        app_root,
        expected_daemon_path,
        expected_daemon_sha256,
        inspect,
        action,
    }) = &cli.command
    {
        let service = ryeos_node::supervision::InstalledService::discover_app_root(app_root)?;
        if let Some(service) = &service {
            service.require_package_daemon_path(expected_daemon_path)?;
        }
        if *inspect {
            println!(
                "{}",
                if service.is_some() {
                    "supervised"
                } else {
                    "direct"
                }
            );
            return Ok(());
        }
        let service = service.context("host upgrade requires a configured service")?;
        let digest = expected_daemon_sha256
            .as_deref()
            .context("host upgrade requires an image digest")?;
        let action = action.context("host upgrade action is absent")?;
        if !matches!(action, config::HostUpgradeAction::Observe) {
            lillux::require_administrator()?;
        }
        match action {
            config::HostUpgradeAction::Begin => {
                let intent = service.begin_upgrade(digest)?;
                println!(
                    "{}",
                    match intent.desired {
                        ryeos_node::supervision::DesiredState::Up => "up",
                        ryeos_node::supervision::DesiredState::Down => "down",
                    }
                );
            }
            config::HostUpgradeAction::ReplacementSafe => {
                service.require_upgrade_replacement_safe(digest)?
            }
            config::HostUpgradeAction::RestoreReady => {
                service.mark_upgrade_restore_ready(digest)?;
            }
            config::HostUpgradeAction::Finish => {
                service.finish_upgrade(digest)?;
            }
            config::HostUpgradeAction::Observe => {
                service.binding.runtime.account.require_current_process()?;
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(service.observe_upgrade(digest))?;
            }
        }
        return Ok(());
    }
    if let Some(config::DaemonCommand::HostService { app_root }) = &cli.command {
        // Must precede async threads, configuration loading, tracing and every
        // node-state write. Lillux drops privileges before executing ryeosd.
        lillux::require_administrator()?;
        return match ryeos_node::supervision::exec_host_service(app_root) {
            Ok(never) => match never {},
            Err(error) => Err(error),
        };
    }
    if let Some(config::DaemonCommand::HostRuntime { binding }) = &cli.command {
        // As with native host-service entry, consume the administrator-owned
        // launch document before async runtime creation, tracing, config loads
        // or node-state writes. The child receives only its exact descriptor.
        let executable = std::env::current_exe().context("locate host-runtime daemon image")?;
        return match ryeos_node::host_runtime::exec_external_controller(binding, &executable) {
            Ok(never) => match never {},
            Err(error) => Err(error),
        };
    }
    let external_host_runtime =
        ryeos_node::host_runtime::ExternalHostRuntime::take_from_environment()
            .context("consume inherited external host-runtime authority")?;
    // Declared provider-delivered values must leave the process environment
    // while this process is still single-threaded. A failure is reported by
    // `run`, after the startup refusals that precede node initialization.
    let provider_environment = match &cli.command {
        Some(config::DaemonCommand::BuildInfo { .. }) => None,
        _ => Some(
            Config::selected_app_root(&cli.to_sources()).and_then(|app_root| {
                // SAFETY: no async runtime, application or library thread
                // exists yet; the runtime is built only below.
                unsafe { ryeos_node::provider_env::take_at_startup(&app_root) }
            }),
        ),
    };
    #[cfg(feature = "crash-qualification-test-support")]
    ryeos_app::dedicated_session_service::test_support::initialize_from_env()
        .context("initialize exact command-settlement crash gate")?;
    ryeos_app::object_contracts::install().context("install application object contracts")?;
    // Recovery and launch reconstruction deserialize the complete retained
    // authority envelope on a runtime worker. Unoptimized builds can exceed
    // Tokio's 2 MiB default while doing that work (the real-process test
    // harness has historically compensated with RUST_MIN_STACK). Make the
    // daemon's actual runtime contract explicit so direct debug and local
    // qualification installs have the same safe floor as the harness.
    const DAEMON_WORKER_STACK_BYTES: usize = 4 * 1024 * 1024;
    let mut composition = DaemonSelectedComposition::bootstrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(DAEMON_WORKER_STACK_BYTES)
        .enable_all()
        .build()
        .context("build daemon async runtime")?;
    // Catch only the run boundary so the external envelope closes before
    // runtime shutdown on both return and unwind. The nonempty selected root
    // closes only after external unpublication and the complete shutdown drain.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(run(
            cli,
            &mut composition,
            external_host_runtime,
            provider_environment,
        ))
    }));
    if composition.has_unresolved_custody() {
        // Exit 70 while the composition and runtime still own every
        // unresolved source; neither Drop nor runtime shutdown runs.
        if let Err(custody) = runtime.block_on(composition.hold_unresolved_custody()) {
            composition.exit_unresolved_custody(custody);
        }
    }
    composition.close_after_run();

    // Tokio cannot cancel work already admitted to spawn_blocking. Never let
    // abandoned request work keep a lifecycle-complete daemon alive holding
    // projection/CAS descriptors indefinitely.
    runtime.shutdown_timeout(Duration::from_secs(5));
    // Release the composition root's owner only after bounded runtime drain.
    // Any admitted blocking placement mutator retains a lease on this exact
    // lock and continues excluding replacement controllers until it actually
    // stops; shutdown_timeout is not treated as proof of cancellation.
    let result = match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    // A latched or rollover stop that completed exits with its own status,
    // after every composition owner has been released in order.
    if let (Ok(()), Some(outcome)) = (&result, composition.service_stop.outcome) {
        drop(composition);
        std::process::exit(outcome.exit_code());
    }
    result
}

#[cfg(feature = "handoff-test-support")]
fn build_handoff_phase_gate(
    cli: &Cli,
) -> Result<Option<Arc<ryeos_app::worker_handoff::test_support::HandoffPhaseGate>>> {
    use std::str::FromStr as _;

    use ryeos_app::worker_handoff::test_support::{
        HANDOFF_PHASE_CUT_FD_ENV, HandoffCrashBoundary, HandoffPhaseGate,
    };

    let descriptor_configured = std::env::var_os(HANDOFF_PHASE_CUT_FD_ENV).is_some();
    let boundary = match (
        descriptor_configured,
        cli.handoff_phase_cut_boundary.as_deref(),
    ) {
        (false, None) => return Ok(None),
        (true, Some(boundary)) => boundary,
        _ => anyhow::bail!("handoff phase-cut boundary and channel must be supplied together"),
    };
    // SAFETY: the test parent minted this exact connected channel through
    // Lillux and consumed its child authority into this daemon spawn.
    let writer =
        unsafe { lillux::take_inherited_duplex_channel_from_env(HANDOFF_PHASE_CUT_FD_ENV) }
            .map_err(anyhow::Error::msg)
            .context("adopt inherited handoff phase-cut channel")?;
    Ok(Some(Arc::new(HandoffPhaseGate::new(
        HandoffCrashBoundary::from_str(boundary)?,
        writer,
    ))))
}

#[cfg(feature = "handoff-test-support")]
fn build_scoped_reserved_attempt_gate(
    cli: &Cli,
) -> Result<Option<Arc<ryeos_app::scoped_producer_start::test_support::ReservedAttemptGate>>> {
    use ryeos_app::scoped_producer_start::test_support::{
        RESERVED_ATTEMPT_GATE_FD_ENV, ReservedAttemptGate,
    };

    let descriptor_configured = std::env::var_os(RESERVED_ATTEMPT_GATE_FD_ENV).is_some();
    match (cli.scoped_reserved_attempt_gate, descriptor_configured) {
        (false, false) => return Ok(None),
        (true, true) => {}
        _ => anyhow::bail!("scoped reserved-attempt gate and channel must be supplied together"),
    }
    // SAFETY: the test parent minted and bound this connected channel through
    // Lillux; the daemon consumes its child authority exactly once.
    let channel =
        unsafe { lillux::take_inherited_duplex_channel_from_env(RESERVED_ATTEMPT_GATE_FD_ENV) }
            .map_err(anyhow::Error::msg)
            .context("adopt inherited scoped reserved-attempt gate channel")?;
    Ok(Some(Arc::new(ReservedAttemptGate::new(channel))))
}

async fn run(
    cli: Cli,
    composition: &mut DaemonSelectedComposition,
    external_host_runtime: Option<ryeos_node::host_runtime::ExternalHostRuntime>,
    provider_environment: Option<Result<ryeos_node::provider_env::StartupProviderEnvironment>>,
) -> Result<()> {
    // Capture process start before any configuration, verification, or state
    // opening so every lifecycle surface reports the same wall/monotonic origin.
    let process_started = Instant::now();
    let process_started_at = lillux::time::iso8601_now();

    #[cfg(feature = "handoff-test-support")]
    let handoff_phase_gate = build_handoff_phase_gate(&cli)?;
    #[cfg(feature = "handoff-test-support")]
    let scoped_reserved_attempt_gate = build_scoped_reserved_attempt_gate(&cli)?;

    if let Some(config::DaemonCommand::BuildInfo {
        revision,
        profile,
        json,
    }) = &cli.command
    {
        let build = ryeos_app::build_info::get_for_version(env!("CARGO_PKG_VERSION"));
        if *revision {
            println!("{}", build.revision);
        } else if *profile {
            println!("{}", build.profile);
        } else if *json {
            println!("{}", serde_json::to_string(&build)?);
        } else {
            println!("version: {}", build.version);
            println!("revision: {}", build.revision);
            println!("build_date: {}", build.build_date);
            println!("profile: {}", build.profile);
        }
        return Ok(());
    }

    let config_sources = cli.to_sources();
    let selected_app_root = Config::selected_app_root(&config_sources)?;
    // This runs as the node account. Host association is not permission to
    // interpret user-owned node configuration while privileged, nor a reason
    // to require administrator-owned ancestors above an ordinary app root.
    let host_service =
        ryeos_node::supervision::InstalledService::discover_app_root(&selected_app_root)?;
    if host_service.is_some() && external_host_runtime.is_some() {
        anyhow::bail!("native and external host-runtime authorities are both present");
    }
    if let Some(service) = &host_service {
        // Refuse a misconfigured root/wrong-account daemon before init checks,
        // state-lock creation, tracing or lifecycle metadata writes.
        service.binding.runtime.account.require_current_process()?;
    }
    if let Some(runtime) = &external_host_runtime {
        runtime.binding().account.require_current_process()?;
        runtime.binding().validate(&selected_app_root)?;
    }
    ryeosd::init_shutdown_channel();

    // Verify operator-owned node initialization before any local repairs
    // or runtime-state writes. `ryeos init` is authoritative for bundle
    // registrations and operator identity/trust artifacts.
    ryeos_node::require_initialized(&selected_app_root)?;
    let provider_environment = provider_environment
        .context("the provider environment was not taken at startup")?
        .context("take the declared provider environment at startup")?;

    // Handle subcommands BEFORE acquiring the daemon state lock or
    // initializing tracing. Subcommands (e.g. `run-service`) manage
    // their own state lock and must not conflict with the daemon's.
    if let Some(ref cmd) = cli.command {
        match cmd {
            config::DaemonCommand::HostProvision { .. } => {
                unreachable!("handled before node startup")
            }
            config::DaemonCommand::InitPolicyPreflight { .. }
            | config::DaemonCommand::HostInstall { .. } => {
                unreachable!("handled before node startup")
            }
            config::DaemonCommand::HostUpgrade { .. } => {
                unreachable!("handled before node startup")
            }
            config::DaemonCommand::HostService { .. } => {
                unreachable!("handled before runtime startup")
            }
            config::DaemonCommand::HostRuntime { .. } => {
                unreachable!("handled before runtime startup")
            }
            config::DaemonCommand::BuildInfo { .. } => unreachable!("handled before config load"),
            config::DaemonCommand::RunService {
                service_ref,
                params,
            } => {
                let config = Config::load(&config_sources)?;
                return run_service_standalone(
                    &config,
                    service_ref,
                    params.as_deref(),
                    &provider_environment,
                )
                .await;
            }
        }
    }

    // Every node route leads its own session and process group: a direct
    // start would otherwise remain in the operator's group. The supervised
    // controller already established this before exec, so this is a no-op
    // there. It precedes every exact current-process identity capture.
    lillux::process_control::establish_own_process_session().map_err(anyhow::Error::msg)?;
    if let Some(service) = &host_service {
        // An association is not placement: only a process the host supervisor
        // put in the controller leaf may take the supervised node route. The
        // standalone subcommands above run from an operator session on a
        // stopped node, so they are not refused here. This precedes the state
        // lock, the running marker and every native debit or allowance use.
        service.binding.runtime.require_current_process_placed()?;
        composition.supervised_app_root = Some(selected_app_root.clone());
    }
    if let Some(runtime) = &external_host_runtime {
        // An inherited launch document is association, not placement either:
        // the external supervisor must have put this process in the bound
        // controller leaf before the node may claim the supervised route.
        runtime.binding().require_current_process_placed()?;
    }
    // Recorded only after placement was proved above, never from a binding.
    let launch_route = if host_service.is_some() || external_host_runtime.is_some() {
        lifecycle_marker::LaunchRoute::Supervised
    } else {
        lifecycle_marker::LaunchRoute::Direct
    };
    composition.launch_route = Some(launch_route);

    // Acquire the daemon state lock BEFORE unlinking any sockets or
    // writing runtime state. This prevents a second `ryeosd` (or
    // standalone service) from racing in and removing the first
    // daemon's live socket. The lock is automatically released when
    // the process exits (Drop on the file descriptor).
    let state_lock_path = state_lock::default_lock_path(&selected_app_root);
    let state_lock = state_lock::StateLock::acquire_with_timeout(
        &state_lock_path,
        Duration::from_secs(5),
    )
    .context(
        "failed to acquire state lock — is another ryeosd instance or standalone service running?",
    )?;
    composition.process_state_lock = Some(state_lock);
    composition
        .process_state_lock
        .as_ref()
        .context("daemon state lock is absent")?
        .ensure_protects_app_root(&selected_app_root)?;

    // Mark the attempt before complete config decoding. The explicit app root
    // is already fenced by StateLock, so a malformed bootstrap document now
    // becomes bounded node-owned failure testimony instead of an invisible
    // supervisor restart loop.
    let state_dir = ryeos_engine::roots::RuntimeRoot::new(selected_app_root.clone()).state();
    // A node that stopped itself to avoid a restart loop refuses here, before
    // the running marker, any recovery or any native debit, until an operator
    // clears the inhibition. This holds whatever restarted the process.
    lifecycle_marker::require_restart_allowed(&state_dir)?;
    // An unreadable marker may carry latched-drain facts that recovery must
    // apply, so the start is refused and the marker left for the operator.
    // What the marker says is narrated after the tracing subscriber exists.
    let previous_exit = lifecycle_marker::take_previous_exit(&state_dir)
        .context("previous lifecycle marker is unreadable; start refused and marker retained")?;
    // Fatal before any native debit: a start without its own running record
    // could not bind the previous stop's unapplied facts.
    let lifecycle_started_at = lifecycle_marker::record_running(&state_dir, launch_route)?;
    composition.lifecycle_exit = Some(LifecycleExitGuard::new(
        state_dir,
        lifecycle_started_at,
        launch_route,
    ));

    let mut config = match Config::load(&config_sources) {
        Ok(config) => config,
        Err(error) => {
            let detail = format!("load node bootstrap configuration: {error:#}");
            composition.record_exit("startup_failed", Some(&detail), None);
            return Err(error).context("load node bootstrap configuration");
        }
    };
    if config.app_root != selected_app_root {
        let detail = format!(
            "decoded node bootstrap app root {} differs from locked app root {}",
            config.app_root.display(),
            selected_app_root.display()
        );
        composition.record_exit("startup_failed", Some(&detail), None);
        anyhow::bail!(detail);
    }
    composition
        .process_state_lock
        .as_ref()
        .context("daemon state lock is absent")?
        .ensure_protects_app_root(&config.app_root)?;

    // Bundle-set recovery must observe the old/new completion fence before
    // ordinary completion verification. The present v1 journal can safely
    // discard pre-activation work and finalize an already signed exact-new
    // completion; activated old-state restoration remains fail-closed until
    // the journal owns exact backup coordinates.
    let bundle_set_recovery = {
        let registry_lock =
            ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire(&config.app_root)?;
        ryeos_app::bundle_set_transaction::recover_for_bootstrap(
            &config.app_root,
            &registry_lock,
            &ryeos_app::bundle_set_transaction::BootstrapBundleSetRecoveryExecutor::new(
                &config.app_root,
            ),
        )?
    };

    // Recheck the signed whole-init fence after recovery while holding the
    // same node state lock as initialization. The fence refuses a policy
    // generation whose isolation policy is for a host class other than the
    // one the signed substrate identity binds, so the daemon never starts
    // under another class's floor and lane.
    bootstrap::verify_initialized(&config)?;
    if bundle_set_recovery
        == Some(ryeos_app::bundle_set_transaction::RecoveryDecision::CompleteExactNew)
    {
        // Preserve the journal until pinned-key verification has authenticated
        // the exact-new whole-init completion and its registrations/policy.
        ryeos_app::bundle_set_transaction::consume_journal(&config.app_root)?;
    }

    // Initialize tracing with file sink only after init-state passes so direct
    // `ryeosd` startup on a fresh system cannot create runtime state.
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_daemon_with_file_sink(
        &config.app_root,
    ));

    // Repair only daemon-local artifacts. Missing operator artifacts
    // (user signing key, trust docs) fail with guidance to run
    // `ryeos init` — daemon never substitutes for operator init.
    bootstrap::repair_daemon_local(&config)?;

    // Startup auto-signing is intentionally NOT called here.
    // The daemon start path is fail-closed on unsigned trust-sensitive items.
    // Use `ryeos init` to install bundle registrations before daemon startup.

    tracing::info!("State lock acquired");
    lifecycle_marker::report_previous_exit(previous_exit.as_ref());
    lifecycle_marker::check_disk_space(
        &ryeos_engine::roots::RuntimeRoot::new(config.app_root.clone()).state(),
    );

    let early_startup = async {
        process::remove_stale_socket(&config.uds_path)?;
        ensure_runtime_paths(&config)?;
        let daemon_json_path = config.app_root.join("daemon.json");
        let _ = std::fs::remove_file(&daemon_json_path);

        // Bind both stable outer transports before projection work. Catch this
        // whole pre-control phase so supervised callers receive the actual
        // terminal failure rather than observing an endless restart spinner.
        let tcp_listener = TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("failed to bind {}", config.bind))?;
        let actual_bind = tcp_listener
            .local_addr()
            .with_context(|| format!("failed to read local_addr after binding {}", config.bind))?;
        let uds_listener = UnixListener::bind(&config.uds_path)
            .with_context(|| format!("failed to bind {}", config.uds_path.display()))?;
        Ok::<_, anyhow::Error>((tcp_listener, uds_listener, daemon_json_path, actual_bind))
    }
    .await;
    let (tcp_listener, uds_listener, daemon_json_path, actual_bind) = match early_startup {
        Ok(value) => value,
        Err(error) => {
            let detail = format!("{error:#}");
            composition.record_exit("startup_failed", Some(&detail), None);
            return Err(error);
        }
    };
    config.bind = actual_bind;
    let _discovery_cleanup =
        startup::DiscoveryCleanup::new(config.uds_path.clone(), daemon_json_path.clone())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.uds_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to set socket permissions on {}",
                    config.uds_path.display()
                )
            })?;
    }

    // daemon.json is an early discovery hint.  Live lifecycle truth always
    // comes from the UDS, and started_at is process start (never ready time).
    let build = ryeos_app::build_info::get_for_version(env!("CARGO_PKG_VERSION"));
    let daemon_info = ryeos_node::DaemonMetadata {
        pid: Some(std::process::id()),
        uds_path: Some(config.uds_path.clone()),
        bind: Some(actual_bind.to_string()),
        started_at: Some(process_started_at.clone()),
        version: Some(build.version.to_string()),
        revision: Some(build.revision.to_string()),
        build_date: Some(build.build_date.to_string()),
        app_root: config.app_root.clone(),
    };
    daemon_info.write(&config.app_root).with_context(|| {
        format!(
            "failed to write daemon.json at {} — tools cannot discover the daemon without it",
            daemon_json_path.display()
        )
    })?;

    let lifecycle_identity = ryeos_node::LifecycleIdentity {
        pid: std::process::id(),
        bind: actual_bind.to_string(),
        uds_path: config.uds_path.clone(),
        app_root: config.app_root.clone(),
        started_at: process_started_at.clone(),
        version: build.version.to_string(),
        revision: Some(build.revision.to_string()),
        build_date: Some(build.build_date.to_string()),
    };
    let startup = startup::StartupCoordinator::bootstrap(lifecycle_identity, process_started)?;
    let recovery_execution_release = ryeos_app::recovery_execution_gate::arm();
    let http_state = startup.http_state();
    let mut uds_task = tokio::spawn(uds::server::serve_dynamic(
        uds_listener,
        startup.uds_state(),
    ));
    let shutdown = shutdown_signal();
    let mut http_task = tokio::spawn(async move {
        serve(tcp_listener, startup::build_outer_router(http_state))
            .with_graceful_shutdown(shutdown)
            .await
    });
    let progress_task = tokio::spawn(startup::progress_ticker(startup.clone()));
    // Give both spawned servers a scheduling turn before any synchronous
    // bootstrap work resumes; listener bind alone is not the accept-loop
    // publication boundary.
    tokio::task::yield_now().await;

    // Taken before the startup future borrows the composition so listener
    // loss can close new selected attachments before awaiting that future.
    let root_admission_fence = composition.root.admission_fence();
    let native_admission_fence = Arc::clone(&composition.native_admission_fence);
    let daemon_result: (Result<()>, bool, bool) = {
        let daemon_work = async {
            startup.phase(
                ryeos_node::StartupPhase::Bootstrapping,
                "loading verified node configuration",
            )?;

            // Resolve every interrupted bundle tree/registration transaction before
            // the bootstrap loader consumes installed bundle registrations.
            let identity = NodeIdentity::load(&config.node_signing_key_path)?;
            composition
                .process_state_lock
                .as_ref()
                .context("daemon state lock is absent")?
                .ensure_protects_app_root(&config.app_root)?;
            if let Some(service) = &host_service {
                service.verify_loaded_node_identity(&identity)?;
            }
            if let Some(runtime) = &external_host_runtime {
                runtime.binding().verify_loaded_node_identity(&identity)?;
            }
            ryeos_api::auth::validate_authorized_key_directory(
                &config.authorized_keys_dir,
                &identity,
            )
            .context(
                "authorized-key cutover failed; stop the daemon and run `ryeos node reset authorization --confirm` to discard every client/remote grant and recreate only the local operator grant",
            )?;
            let repaired_bundles =
                ryeos_app::bundle_transaction::reconcile_all_bundle_transactions(
                    &config.app_root,
                    identity.signing_key(),
                )?;
            if !repaired_bundles.is_empty() {
                tracing::warn!(
                    bundles = ?repaired_bundles,
                    "reconciled interrupted bundle transactions before registry loading"
                );
            }
            // Reconciliation may complete a prepared per-bundle mutation.
            // The whole-node completion fence must still describe the exact
            // resulting bundle and policy generation before any registry is
            // loaded or executable authority is constructed.
            if !repaired_bundles.is_empty() {
                bootstrap::verify_initialized(&config)?;
            }

            // ── Two-phase node-config bootstrap ──
            // The transport that delivered the binding is the provider the
            // capability report attributes every per-scope proof to.
            let host_runtime = external_host_runtime
                .as_ref()
                .map(|runtime| ryeos_node::host_runtime::SelectedHostRuntime {
                    binding: runtime.binding(),
                    provider: runtime.binding().provider_id(),
                })
                .or_else(|| {
                    host_service
                        .as_ref()
                        .map(|service| ryeos_node::host_runtime::SelectedHostRuntime {
                            binding: &service.binding.runtime,
                            provider: ryeos_node::host_runtime::HOST_SERVICE_PROVIDER,
                        })
                });
            let (engine, node_config_snapshot, node_policy_snapshot, isolation) =
                bootstrap::load_node_config_two_phase_with_host_runtime(
                    &config,
                    host_runtime,
                    &provider_environment,
                )?;
            let external_artifacts = engine.with_checked_bundle_generation(|_| {
                ryeos_app::external_artifacts::resolve_external_execution_artifacts(
                    &engine.bundle_roots,
                    &engine.node_trust_store,
                    &ryeos_app::private_artifact_home::root(&config.runtime_state_dir()),
                )
            })?;

            // Build the service registry early — self-check needs it.
            let services = Arc::new(build_service_registry()?);

            // Self-check: verify every registered service resolves and is trusted.
            // Every service must resolve, verify, extract an endpoint, AND have a
            // registered handler. Any failure prevents daemon start (fail-closed).
            let catalog_health = engine.with_checked_bundle_generation(
                |generation| -> anyhow::Result<ryeos_app::state::CatalogHealth> {
                    // Signed installed service items define the operational
                    // catalog. The all-feature daemon may compile handlers for
                    // services belonging to bundles that this node deliberately
                    // does not install; those handlers are capability, not a
                    // second mandatory-service authority.
                    let node_principal = identity.principal_id();

                    let plan_ctx = ryeos_engine::contracts::PlanContext {
                        requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                            ryeos_engine::contracts::Principal {
                                fingerprint: node_principal,
                                scopes: vec![],
                            },
                        ),
                        project_context: ryeos_engine::contracts::ProjectContext::None,
                        subject_resolution_authority:
                            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
                        current_site_id: "site:local".into(),
                        origin_site_id: "site:local".into(),
                        execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
                        scheduled_fire: None,
                        validate_only: true,
                    };

                    let mut failed = Vec::new();
                    let installed_refs = installed_service_refs(&engine)?;
                    let effective_requests = installed_refs
                        .iter()
                        .map(|canonical| ryeos_engine::engine::EffectiveItemRequest {
                            item_ref: canonical.clone(),
                            expected_kind: Some(canonical.kind.clone()),
                            project_root: None,
                            subject_resolution_authority:
                                ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
                        })
                        .collect::<Vec<_>>();
                    let mut operational_services = Vec::new();
                    for (canonical, effective) in installed_refs
                        .into_iter()
                        .zip(generation.effective_items(&effective_requests))
                    {
                        let service_ref = canonical.to_string();
                        let effective = match effective {
                            Ok(effective) => effective,
                            Err(error) => {
                                failed.push((service_ref, error.to_string()));
                                continue;
                            }
                        };
                        if !effective.trusted {
                            failed.push((
                                service_ref,
                                "installed Services-registry item is not trusted".into(),
                            ));
                            continue;
                        }
                        match ryeos_app::service_registry::requires_compiled_service_handler(
                            &effective.composed_value,
                        ) {
                            Ok(false) => continue,
                            Err(error) => {
                                failed.push((service_ref, error.to_string()));
                                continue;
                            }
                            Ok(true) => {}
                        }
                        match service_descriptors()
                            .iter()
                            .find(|candidate| candidate.service_ref == service_ref)
                        {
                            Some(descriptor) => operational_services.push((descriptor, canonical)),
                            None => failed.push((
                                service_ref,
                                "installed operational service has no compiled handler".into(),
                            )),
                        }
                    }
                    let canonical_refs = operational_services
                        .iter()
                        .map(|(_, canonical)| canonical.clone())
                        .collect::<Vec<_>>();
                    let resolved = generation.resolve_many(&plan_ctx, &canonical_refs);
                    for ((desc, _), resolved) in operational_services.into_iter().zip(resolved) {
                        let service_ref = desc.service_ref;
                        let check = check_service_catalog_entry(
                            desc, resolved, &engine, &plan_ctx, &services,
                        );
                        match check {
                            ServiceCatalogCheck::Healthy => {}
                            ServiceCatalogCheck::Failed(error) => {
                                failed.push((service_ref.to_owned(), error));
                            }
                        }
                    }

                    if !failed.is_empty() {
                        for (svc, error) in &failed {
                            tracing::error!(
                                svc,
                                error,
                                "refusing to start: operational service failed verification"
                            );
                        }
                        anyhow::bail!(
                "operational service catalog self-check failed: {} service(s) failed verification",
                failed.len()
            );
                    }

                    // An absent optional service has no installed item and is
                    // therefore outside this node's catalog. Dispatching it
                    // still returns `service_not_installed`; it does not make a
                    // deliberately lean bundle set unhealthy.
                    Ok(ryeos_app::state::CatalogHealth {
                        status: "ok".into(),
                        missing_services: vec![],
                    })
                },
            )?;

            // The installed composition, not the build, decides whether this
            // node runs the scheduler. Without it no scheduler state is opened
            // or created and every consumer observes an absent scheduler.
            // A build that cannot run a selected scheduler refuses.
            let scheduler_selected = ryeos_api::scheduler_selection::resolve_scheduler_selection(
                &engine,
                cfg!(feature = "scheduling"),
            )
            .context("derive scheduler selection from the installed composition")?;
            startup.select_scheduler(scheduler_selected)?;
            tracing::info!(scheduler_selected, "scheduler selection from installed composition");
            // Enabled maintenance schedules need the scheduler. Refuse such a
            // node here, before startup recovery mutates any state.
            if !scheduler_selected
                && node_policy_snapshot
                    .require::<ryeos_scheduler_contract::NodeMaintenancePolicy>()?
                    .schedules
                    .iter()
                    .any(|schedule| schedule.initial_enabled)
            {
                anyhow::bail!(
                    "this node policy requires scheduled maintenance, but the selected daemon composition has no scheduler service"
                );
            }

            // Build UI state (browser sessions + session bus).
            let ui_state = std::sync::Arc::new(ryeos_ui::UiState::new());
            let ui_state_for_hints = ui_state.clone();
            let prospective_node_config_validator =
                prospective_node_config_validator(ui_state.clone(), engine.clone());

            // Build the route table from the node-config snapshot.
            let route_table = {
                let table =
                    build_route_table(&node_config_snapshot, ui_state.clone(), &engine.kinds)
                        .context("route table build failed at startup — check route YAML files")?;
                Arc::new(arc_swap::ArcSwap::from_pointee(table))
            };
            tracing::info!(routes = route_table.load().all.len(), "route table built");

            // Publish the route diagnostics snapshot consumed by
            // `service:system/routes`.
            let route_diagnostics = Arc::new(ryeos_app::route_diagnostics::RouteDiagnostics::new());
            {
                let table = route_table.load();
                route_diagnostics.publish(
                    table.fingerprint.clone(),
                    ryeos_api::routes::route_diagnostic_entries(&table),
                );
            }

            // Build thread-kind profiles from the loaded kind schemas.
            let kind_profiles = Arc::new(kind_profiles::KindProfileRegistry::build(Some(
                &engine.kinds,
            )));
            let runtime_state_dir = config.runtime_state_dir();
            let runtime_db_path = config.db_path.clone();
            let state_app_root = config.app_root.clone();
            let signer = Arc::new(state_store::NodeIdentitySigner::from_identity(&identity));
            let mut head_trust = ryeos_state::refs::TrustStore::new();
            head_trust.insert(
                identity.fingerprint().to_string(),
                *identity.verifying_key(),
            );
            let head_trust = Arc::new(head_trust);

            let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();

            // Execution admission limits must be armed before projection recovery
            // or any later runtime recovery action is classified or enqueued.
            let execution_limits = node_policy_snapshot.require::<
                ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy,
            >()?;
            ryeos_executor::execution::launch::arm_global_live_fanout_limit(Some(
                execution_limits.max_live_fanout,
            ));
            let private_copy_limit = execution_limits.max_private_materialization_copy_bytes;
            ryeos_executor::execution::arm_private_materialization_copy_limit(private_copy_limit)?;
            tracing::info!(
                max_live_fanout = execution_limits.max_live_fanout,
                "node execution limits armed"
            );
            tracing::info!(
                max_private_materialization_copy_bytes = private_copy_limit,
                "node private materialization limit armed"
            );

            startup.phase(
                ryeos_node::StartupPhase::OpeningProjection,
                "verifying runtime state and opening thread projection",
            )?;
            let state_open_observer: Arc<dyn ryeos_state::ProjectionRecoveryObserver> =
                Arc::new(startup.clone());
            let (native_factory, native_deadline, native_exclusion) =
                composition.prepare_live_native(&config, &identity, &node_policy_snapshot)?;
            let native_lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority> =
                composition.native_factory.as_ref().context("actual native issuer absent before State startup")?.clone();
            // Retained Bundle executors are revalidated against the same node
            // trust snapshot this engine admits Bundles with at launch.
            let bundle_admission_trust =
                state_store::BundleAdmissionTrust::bound(engine.node_trust_store.clone());
            let state_store = tokio::task::spawn_blocking(move || {
                // SAFETY: prepare_live_native authenticated/reserved this
                // incarnation; the selected composition retains its original
                // issuer and account/namespace outside the native graph. This
                // authority is a clone of that same issuer, not a new grant.
                unsafe { state_store::StateStore::new_live_with_head_trust_and_recovery_observer(
                    state_app_root,
                    runtime_state_dir,
                    runtime_db_path,
                    signer,
                    write_barrier.clone(),
                    head_trust,
                    bundle_admission_trust,
                    state_open_observer,
                    native_exclusion,
                    native_factory,
                    native_lease_authority,
                    native_deadline,
                ) }
                .map(|store| (store, write_barrier))
            })
            .await
            .context("StateStore initialization task failed")?
            .map_err(|error| composition.retain_native_failure(error.context("StateStore initialization failed")))?;
            let (state_store, write_barrier) = state_store;
            let state_store = Arc::new(state_store);
            composition.install_live_state_store(Arc::clone(&state_store));
            tracing::info!("StateStore initialized successfully");

            // StateDb::open owns the one deterministic pending-transition replay
            // and reports its ReplayingHeadChanges progress through the observer.
            // A Remove whose head is already absent deliberately remains pending:
            // its runtime cleanup needs the scheduler pin index opened below.

            if ryeosd::shutdown_requested() {
                tracing::info!("startup shutdown requested after projection open");
                return Ok(());
            }

            // Scheduler fire projection recovery is a separate, potentially
            // long startup domain. It must complete before terminal-history
            // removal consults scheduler pins and before active-thread recovery
            // can observe scheduler rows.
            #[cfg(feature = "scheduling")]
            let scheduler_runtime = if scheduler_selected {
                startup.phase(
                    ryeos_node::StartupPhase::RecoveringSchedulerProjection,
                    "recovering scheduler fire projection",
                )?;
                let scheduler_db_path = config
                    .app_root
                    .join(ryeos_engine::AI_DIR)
                    .join("state")
                    .join("scheduler.sqlite3");
                let scheduler_runtime_path =
                    config.app_root.join(ryeos_engine::AI_DIR).join("state");
                let scheduler_runtime_directory = Arc::new(
                    lillux::PinnedDirectory::open(&scheduler_runtime_path)?
                        .context("scheduler runtime directory is absent before DB open")?,
                );
                let original_factory = composition.native_factory.as_ref()
                    .context("scheduler requires retained live native factory")?.ready_factory()?;
                ryeos_scheduler_service::validate_retirement_startup(
                    &original_factory,
                    &scheduler_runtime_directory,
                    &identity,
                )
                .context("validate scheduler retirement journal before DB open")?;
                let scheduler_runtime_gate = Arc::new(tokio::sync::RwLock::new(()));
                let scheduler_startup_guard = scheduler_runtime_gate.clone().write_owned().await;
                let admission = state_store.live_scheduler_native_admission()
                    .map_err(|error| composition.retain_scheduler_admission_failure(error))?;
                composition.scheduler_admission_custody = Some(admission);
                composition.scheduler_admission_custody.as_ref().expect("retained scheduler admission")
                    .require_factory(&original_factory)?;
                anyhow::ensure!(composition.native_sources.scheduler_native_unopened_inputs().is_none()
                    && composition.scheduler_schema_failure.is_none(),
                    "scheduler admission competes with retained unopened/schema-failure custody");
                // Consume only after every fallible handoff check; mismatch
                // keeps the complete affine metadata under composition custody.
                let (scheduler_factory, scheduler_inputs, reference_inputs) = composition
                    .scheduler_admission_custody.take().expect("validated scheduler admission").into_parts();
                let reference = match scheduler::db::SchedulerDb::prepare_scheduler_schema_reference(
                    Arc::clone(&scheduler_factory), reference_inputs,
                ) {
                    Ok(reference) => reference,
                    Err(failure) => {
                        composition.scheduler_schema_failure = Some(failure);
                        *composition.native_sources.scheduler_native_unopened_inputs_mut() = Some(scheduler_inputs);
                        anyhow::bail!("scheduler schema reference refused; original custody retained");
                    }
                };
                let scheduler_db = match scheduler::db::SchedulerDb::open_original_scheduler(
                    scheduler_factory, scheduler_inputs, reference,
                ) {
                    Ok(db) => Arc::new(db),
                    Err(failure) => {
                        composition.retain_scheduler_open_failure(failure);
                        anyhow::bail!("SchedulerDb initialization failed; original failure custody retained");
                    }
                };
                composition.retain_scheduler_db(Arc::clone(&scheduler_db));
                let scheduler_service = Arc::new(ryeos_scheduler_service::SchedulerService::new(
                    config.app_root.clone(),
                    scheduler_runtime_directory,
                    scheduler_db.clone(),
                    scheduler_runtime_gate.clone(),
                    Arc::new(identity.clone()),
                    Arc::new(engine.node_trust_store.clone()),
                )?);
                {
                    // Scheduler fire rows are consulted by terminal-chain recovery
                    // before the timer's later reconciliation phase. Recover the
                    // durable outbox and any invalid fire projection now, under the
                    // same exclusive gate used by every later journal mutation, so
                    // pin inspection can never observe a partial full replay.
                    let runtime_state_dir =
                        config.app_root.join(ryeos_engine::AI_DIR).join("state");
                    scheduler_db
                        .drain_fire_outbox(&runtime_state_dir)
                        .context("recover scheduler fire outbox before runtime cleanup")?;
                    if !scheduler_db.fire_projection_is_current()? {
                        scheduler::projection::rebuild_fires_from_dir(
                            &runtime_state_dir.join("schedules"),
                            &scheduler_db,
                        )
                        .context("rebuild scheduler fire projection before runtime cleanup")?;
                    }
                }
                drop(scheduler_startup_guard);
                tracing::info!(path = %scheduler_db_path.display(), "SchedulerDb initialized");
                Some((scheduler_db, scheduler_runtime_gate, scheduler_service))
            } else {
                None
            };

            let events = Arc::new(EventStoreService::new(state_store.clone()));
            // The hub is shared with the lifecycle service so its create/start/
            // finalize/continuation writes publish live (persist-then-publish),
            // and stored as `event_streams` for the SSE endpoints + events.append.
            let event_streams = Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY));
            let threads = Arc::new(ThreadLifecycleService::new(
                state_store.clone(),
                engine.clone(),
                kind_profiles.clone(),
                events.clone(),
                event_streams.clone(),
                &identity.site_id(),
            )?);
            // Operator live-input queue, shared between the `threads.input` enqueue
            // path (via AppState) and lifecycle finalization (closes a thread's entry).
            let live_input = Arc::new(ryeos_app::live_input_queue::LiveInputQueue::new());
            threads.set_live_input_queue(live_input.clone());
            let commands = Arc::new(CommandService::new(
                state_store.clone(),
                kind_profiles.clone(),
                events.clone(),
            ));
            let callback_tokens = Arc::new(CallbackCapabilityStore::new());
            let thread_auth = Arc::new(ryeos_app::callback_token::ThreadAuthStore::new());
            // `services` already built above for self-check

            // Operator-secret store. Sealed-envelope (X25519 + XChaCha20-Poly1305)
            // store at `<state>/.ai/state/secrets/store.enc`, decrypted with
            // the vault X25519 secret key generated by bootstrap. Subprocesses
            // receive only the secrets they declare via `required_secrets`,
            // read after their exact root admission is verified and delivered
            // as named environment entries. Daemon stays vendor-agnostic —
            // `vault.rs` only moves opaque `String -> String` pairs.
            let vault: Arc<dyn ryeos_app::vault::NodeVault> = Arc::new(
                ryeos_app::vault::SealedEnvelopeVault::load(&config.app_root).context(
                    "load sealed-envelope vault — did `ryeos init` (or daemon bootstrap) run?",
                )?,
            );

            let command_registry = Arc::new(
                ryeos_runtime::CommandRegistry::from_records(
                    &node_config_snapshot.commands,
                    &node_policy_snapshot
                        .require::<ryeos_node_runtime::node_policy::sections::command_registration::CommandRegistrationAuthority>()?
                        .runtime_policy(),
                )
                .context("failed to build command registry from node-config records")?,
            );

            let authorizer = Arc::new(ryeos_runtime::authorizer::Authorizer::new());

            // Financial accounting ledger, on the same live native factory as
            // State/Runtime. Failure to open or verify does not stop the node:
            // it disables every hard-budget admission (fail-closed) while
            // unrelated non-hard work continues. The native owner is retained
            // in composition custody whether or not the open succeeds.
            let accounting = match state_store.live_accounting_native_admission() {
                Err(error) => {
                    tracing::error!(
                        %error,
                        "accounting ledger admission refused; hard-budget admission is disabled"
                    );
                    None
                }
                Ok(admission) => {
                    let (ledger, opened) = tokio::task::spawn_blocking(move || {
                        ryeos_app::accounting_db::AccountingDb::open_live_native(admission)
                    })
                    .await
                    .context("accounting ledger open task")?;
                    composition.accounting_native = Some(ledger);
                    if let Some(factory) = composition.native_factory.as_ref()
                        .and_then(|producer| producer.ready_factory().ok())
                        && let Ok(snapshot) = factory.budget_snapshot()
                    {
                        tracing::info!(
                            connections_issued = snapshot.connections_issued,
                            allocator_steps = snapshot.allocator_steps,
                            "native budget census after accounting ledger open"
                        );
                    }
                    match opened {
                        Ok(ledger) => match ledger.startup_verify() {
                            Ok(report) => {
                                tracing::info!(?report, "accounting ledger verified");
                                Some(Arc::new(ledger))
                            }
                            Err(error) => {
                                tracing::error!(
                                    %error,
                                    "accounting ledger failed startup verification; hard-budget \
                                     admission is disabled"
                                );
                                None
                            }
                        },
                        Err(error) => {
                            tracing::error!(
                                error = format!("{error:#}"),
                                "accounting ledger unavailable; hard-budget admission is disabled"
                            );
                            None
                        }
                    }
                }
            };

            let node_execution_identity =
                ryeos_app::execution_identity_probe::boot_node_execution_identity(
                    &state_store,
                    &identity,
                    &build,
                )?;
            let persistent_session_policy = node_policy_snapshot.require::<
                ryeos_node_runtime::node_policy::sections::persistent_sessions::PersistentSessionPolicy,
            >()?;
            let persistent_sessions = Arc::new(if persistent_session_policy.enabled {
                ryeos_app::persistent_session::PersistentSessionPool::with_limits(
                    persistent_session_policy
                        .limits
                        .clone()
                        .context("enabled persistent-session policy has no limits")?,
                )?
            } else {
                ryeos_app::persistent_session::PersistentSessionPool::disabled()
            });
            // Retain this exact pool Arc before any later fallible preparation
            // or AppState field construction can return or unwind.
            composition.retain_persistent_pool(Arc::clone(&persistent_sessions));
            let execution_policy = node_policy_snapshot.require::<
                ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy,
            >()?;
            let execution_resources =
                ryeos_app::execution_resources::ExecutionResourcePool::observe(
                    execution_policy.resource_authority.as_ref(),
                )
                .context("observe node execution resources")?;
            let ignore_matcher = Arc::new(
                node_policy_snapshot
                    .require::<ryeos_node_runtime::node_policy::sections::ingest_ignore::CompiledIngestIgnorePolicy>()?
                    .matcher
                    .clone(),
            );
            let operator_state_lease = Arc::new(
                composition
                    .process_state_lock
                    .as_ref()
                    .context("daemon state lock is absent")?
                    .retain(),
            );

            let app_state = AppState {
                config: Arc::new(config.clone()),
                daemon_build: build.clone(),
                isolation,
                state_store: Arc::clone(&state_store),
                engine: engine.clone(),
                resolution_cache: std::sync::Arc::new(
                    ryeos_app::resolution_cache::ResolutionCache::new(128),
                ),
                engine_cache: ryeos_app::engine_cache::EngineCache::new(
                    ryeos_app::engine_cache::EngineCacheConfig::default(),
                ),
                identity: Arc::new(identity),
                threads,
                live_input,
                events,
                event_streams,
                commands,
                callback_tokens,
                thread_auth,
                controller_lifetime: Arc::clone(&operator_state_lease),
                extensions: {
                    let mut ext = ryeos_app::extension_state::ExtensionState::new();
                    #[cfg(feature = "scheduling")]
                    if let Some((_, _, scheduler_service)) = &scheduler_runtime {
                        ext.insert(scheduler_service.clone());
                    }
                    ext.insert(Arc::new(
                        ryeos_api::project_deploy::ProjectScheduleRecoveryOwners::default(),
                    ));
                    ext.insert(ui_state);
                    ext.insert(route_diagnostics);
                    ext.insert(prospective_node_config_validator);
                    ext.insert(node_execution_identity);
                    // Launch admission judges declared node guarantees
                    // against the signed report this daemon recorded and
                    // was admitted on, re-observing what is cheap to.
                    ext.insert(Arc::new(node_capability_report_reader(
                        config.app_root.clone(),
                        format!("sha256:{}", node_policy_snapshot.generation_digest()),
                    )));
                    let publication_policy = node_policy_snapshot
                        .require::<ryeos_node_runtime::node_policy::sections::bundle_publication::BundlePublicationPolicy>()?
                        .clone();
                    let publisher = configured_bundle_publisher(&publication_policy)?;
                    let publication_authorities =
                        ryeos_app::bundle_publication::producer::BundleReleaseAuthorityRouter {
                            publisher,
                            cas_composition: Some(Arc::new(
                                ryeos_app::bundle_publication::producer::VerifiedLocalCasCompositionAuthority::new(
                                    Arc::clone(&state_store),
                                ),
                            )),
                            ..Default::default()
                        };
                    let publication_adapter = Arc::new(
                        ryeos_app::bundle_publication::producer::PersistentBundleReleaseAdapter::new(
                            config.app_root.clone(), publication_policy,
                            publication_authorities,
                        ),
                    );
                    ext.insert(Arc::new(
                        ryeos_app::bundle_publication::producer::BundleReleaseAuthorities::new(
                            publication_adapter,
                        ),
                    ));
                    #[cfg(feature = "handoff-test-support")]
                    if let Some(gate) = handoff_phase_gate.clone() {
                        ext.insert(gate);
                    }
                    #[cfg(feature = "handoff-test-support")]
                    if let Some(gate) = scoped_reserved_attempt_gate.clone() {
                        ext.insert(gate);
                    }
                    Arc::new(ext)
                },
                write_barrier: Arc::new(write_barrier),
                started_at: process_started,
                started_at_iso: process_started_at.clone(),
                catalog_health,
                services,
                service_descriptors: service_descriptors(),
                node_config: node_config_snapshot,
                external_placement_backends: Arc::new(external_artifacts.placement_backends),
                // External connector generations must be supplied by signed
                // bundle composition. No ambient sibling-binary discovery is
                // permitted; absent composition keeps external admission
                // fail-closed.
                external_candidate_connectors: Arc::new(external_artifacts.connectors),
                external_provider_configurations: Arc::new(
                    external_artifacts.provider_configurations,
                ),
                external_candidate_imports: Arc::new(Default::default()),
                node_policy: node_policy_snapshot,
                vault,
                provider_env: Arc::clone(provider_environment.environment()),
                command_registry,
                authorizer,
                ignore_matcher,
                vault_fingerprint: {
                    let vault_pk_path = config
                        .app_root
                        .join(ryeos_engine::AI_DIR)
                        .join("node/vault/public_key.pem");
                    if vault_pk_path.exists() {
                        lillux::vault::read_public_key(&vault_pk_path)
                            .ok()
                            .map(|pk| pk.fingerprint())
                    } else {
                        None
                    }
                },
                accounting,
                persistent_sessions,
                execution_resources: Arc::new(execution_resources),
                scoped_producer_authorities: Arc::new(Default::default()),
                scoped_producer_processes: Arc::new(Default::default()),
                qualification_workspace_custody: Arc::new(Default::default()),
            };
            #[cfg(feature = "scheduling")]
            if let Some((_, _, scheduler_service)) = &scheduler_runtime {
                app_state.install_scheduler_participant(scheduler_service.clone()
                    as Arc<dyn ryeos_app::scheduler_boundary::SchedulerParticipant>)?;
            }
            // Move the completed AppState under outer custody before the
            // fallible generation arm and recovery sequence. This preserves
            // the exact pool allocation installed above.
            composition.adopt_app_state(app_state)?;
            composition
                .arm_retained_app_generation()
                .context("register actual App generation in empty selected composition")?;
            let accounting_publisher_slot = Arc::clone(&composition.accounting_publisher);
            // Applied latched-drain facts are read once here. The durable
            // marker stays the record until reconcile succeeds, so a failed
            // start rebinds them on the next one.
            let latched_drain = composition.service_stop.pending_latched_drain.take();
            let latched_state_dir = composition
                .lifecycle_exit
                .as_ref()
                .map(|exit| (exit.state_dir.clone(), exit.started_at.clone()));
            let app_state = composition
                .app_state_mut()
                .context("actual AppState custody disappeared after generation arm")?;
            // No selected attachment is published: generation bookkeeping is
            // not native owner installation or permission to contact.
            // This is a generation cut, not a periodic repair. Settle every
            // attempt owned by the dead predecessor before any startup
            // recovery can terminalize its job and before application
            // admission can create an attempt owned by this process.
            let interrupted_sync_attempts = app_state
                .state_store
                .reconcile_interrupted_sync_job_attempts()
                .context("reconcile predecessor sync-job attempts")?;
            if interrupted_sync_attempts != 0 {
                tracing::warn!(
                    interrupted_sync_attempts,
                    "settled sync-job attempts interrupted by the previous daemon process"
                );
            }
            let recovered_project_applies =
                ryeos_api::handlers::project_apply_snapshot::recover_durable_project_snapshot_applies(
                    &app_state,
                )
                .await
                .context("recover interrupted project snapshot applies")?;
            if recovered_project_applies != 0 {
                tracing::warn!(
                    recovered_project_applies,
                    "recovered project snapshot applies before enabling dispatch"
                );
            }
            let admission_store = app_state.state_store.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                if let Err(error) = admission_store.close_process_attachment_admission() {
                    tracing::error!(
                        error = %error,
                        "failed to close process attachment admission from shutdown signal"
                    );
                }
            });

            // Accounting audit outbox publisher: drains committed ledger
            // transitions to the thread chain exactly once, in per-attempt
            // order. Admission authority stays with the ledger; audit lag only
            // degrades accounting health, never erases a committed
            // reservation.
            if let Some(ledger) = app_state.accounting.clone() {
                let outbox_store = app_state.state_store.clone();
                let (stop, stopped) = tokio::sync::watch::channel(false);
                let publisher = tokio::spawn(async move {
                    ryeosd::accounting_outbox::run_publisher(ledger, outbox_store, stopped).await;
                });
                *accounting_publisher_slot.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((publisher, stop));
            }
            // A durable Remove journal record may survive after its signed head was
            // already unlinked. Finish its runtime/projection cleanup before any
            // execution reconciliation can recreate operational pins, and never
            // publish Ready while a head transition remains unresolved.
            startup.phase(
                ryeos_node::StartupPhase::ReplayingHeadChanges,
                "finishing interrupted terminal-history removals",
            )?;
            // A composition without the scheduler may clean terminal history
            // only while its namespaces keep proving no scheduler history.
            let scheduler_absence = if scheduler_selected {
                None
            } else {
                Some(require_absent_scheduler_history(&config.app_root)?)
            };
            #[cfg(feature = "scheduling")]
            let _startup_retention_guard = match &scheduler_runtime {
                Some((_, scheduler_runtime_gate, _)) => {
                    Some(scheduler_runtime_gate.clone().write_owned().await)
                }
                None => None,
            };
            let cleanup_store = app_state.state_store.clone();
            #[cfg(feature = "scheduling")]
            let cleanup_scheduler = scheduler_runtime
                .as_ref()
                .map(|(scheduler_db, _, _)| scheduler_db.clone());
            let recovered_removals = tokio::task::spawn_blocking(move || {
                let recovered = cleanup_store.recover_pending_terminal_chain_removals(
                    &lillux::time::iso8601_now(),
                    false,
                    |thread_ids| {
                        #[cfg(feature = "scheduling")]
                        if let Some(cleanup_scheduler) = &cleanup_scheduler {
                            let mut pins = 0_u64;
                            for thread_id in thread_ids {
                                if cleanup_scheduler.find_fire_by_thread(thread_id)?.is_some() {
                                    pins = pins.checked_add(1).ok_or_else(|| {
                                        anyhow::anyhow!("scheduler recovery pin count overflow")
                                    })?;
                                }
                            }
                            return Ok(pins);
                        }
                        let _ = thread_ids;
                        scheduler_absence
                            .as_ref()
                            .context("scheduler-free cleanup lost its absence proof")?
                            .verify_absent()?;
                        Ok(0)
                    },
                )?;
                if let Some(scheduler_absence) = &scheduler_absence {
                    scheduler_absence.verify_absent()?;
                }
                Ok::<_, anyhow::Error>(recovered)
            })
            .await
            .context("interrupted terminal-history cleanup task failed")??;
            tracing::info!(
                recovered = recovered_removals.pending_retirements_recovered,
                "finished interrupted terminal-history removals"
            );
            let pending_heads = app_state.state_store.pending_head_transition_status()?;
            if pending_heads.pending != 0 {
                anyhow::bail!(
                    "startup recovery left {} unresolved chain-head transition(s)",
                    pending_heads.pending
                );
            }
            #[cfg(feature = "scheduling")]
            drop(_startup_retention_guard);
            let webhook_dedupe =
                Arc::new(ryeos_api::routes::webhook_dedupe::WebhookDedupeStore::new());

            // Reconcile active execution state while the stable listeners continue to
            // serve lifecycle status. Recovery work is collected before application
            // publication and dispatched only after callback state is available.
            // Node-wide admission limits were compiled from the mandatory
            // node-signed execution policy before startup recovery. Per-launch
            // project execution configuration remains a separate item-resolution
            // contract and cannot alter this node-wide ceiling.
            startup.phase(
                ryeos_node::StartupPhase::ReconcilingThreads,
                "recovering predecessor external candidate executions",
            )?;
            recover_external_candidates_before_ready(&app_state)
                .await
                .context("recover predecessor external candidate executions")?;
            startup.phase(
                ryeos_node::StartupPhase::ReconcilingThreads,
                "reconciling active thread execution state",
            )?;
            let launch_planning_repaired = app_state
                .state_store
                .reconcile_launch_planning()
                .context("reconcile durable launch planning admissions")?;
            if launch_planning_repaired != 0 {
                tracing::info!(
                    repaired = launch_planning_repaired,
                    "reconciled durable launch planning admissions"
                );
            }
            ryeos_api::handlers::dedicated_sessions::reconcile_candidate_publications(Arc::new(
                app_state.clone(),
            ))
            .await
            .context("reconcile hosted candidate publications")?;
            let active_reconcile = reconcile::reconcile_active_threads_after_latched_stop(
                &app_state,
                latched_drain.as_ref(),
            )
            .await?;
            if latched_drain.is_some() {
                let (state_dir, started_at) = latched_state_dir
                    .as_ref()
                    .context("daemon lifecycle owner is absent")?;
                lifecycle_marker::clear_pending_latched_drain(state_dir, started_at)
                    .context("clear applied latched-drain facts")?;
            }
            startup.progress(|snapshot| {
                snapshot.recovery_threads = Some(active_reconcile.active_thread_ids.len() as u64);
            })?;
            let recovery_thread_targets = active_reconcile.active_thread_ids;
            let resume_intents = active_reconcile.resume_intents;
            if ryeosd::shutdown_requested() {
                tracing::info!("startup shutdown requested after thread reconciliation");
                return Ok(());
            }
            // Follow reconcile actions collected here, dispatched post-listener too: a
            // resumed parent's (or relaunched child's) first callback must not precede a
            // bound listener.
            // LOAD-BEARING ORDER: settle cancellation tombstones before follow
            // reconciliation can classify an admitted-but-unlaunched child for relaunch.
            ryeos_app::cascade::repair_cancelled_window_members(&app_state)?;
            startup.phase(
                ryeos_node::StartupPhase::ReconcilingFollow,
                "reconciling suspended follow execution state",
            )?;
            let follow_actions = reconcile::reconcile_follow(&app_state)?;
            if ryeosd::shutdown_requested() {
                tracing::info!("startup shutdown requested after follow reconciliation");
                return Ok(());
            }

            // Reconciliation writes are authoritative-first. If any projection apply
            // requested repair, drain that bounded pending journal before publishing
            // application readiness; Ready never fronts a known-stale projection.
            let projection_health = app_state.state_store.projection_health();
            if !projection_health.is_current() {
                startup.phase(
                    ryeos_node::StartupPhase::ReplayingHeadChanges,
                    "repairing projection changes produced during reconciliation",
                )?;
                if let Some(generation) = projection_health.begin_repair() {
                    let repair_store = app_state.state_store.clone();
                    let repaired = tokio::task::spawn_blocking(move || {
                        repair_store.repair_thread_projection()
                    })
                    .await
                    .context("startup projection repair task failed")?;
                    projection_health.finish_repair(generation, &repaired);
                    repaired.context("startup projection repair failed")?;
                }
                if !projection_health.is_current() {
                    anyhow::bail!("thread projection remained stale after startup repair");
                }
            }
            if ryeosd::shutdown_requested() {
                tracing::info!("startup shutdown requested after projection repair");
                return Ok(());
            }

            // Scheduler reload channel — must be created BEFORE the router is built
            // so that HTTP handler clones of AppState carry the sender.
            #[cfg(feature = "scheduling")]
            let scheduler_reload_rx = match &scheduler_runtime {
                Some((_, _, scheduler_service)) => {
                    let (scheduler_reload_tx, scheduler_reload_rx) =
                        tokio::sync::mpsc::channel::<scheduler::ReloadSignal>(16);
                    scheduler_service.install_reload_sender(scheduler_reload_tx)?;
                    Some(scheduler_reload_rx)
                }
                None => None,
            };

            // Auth is per-route via the dispatcher's auth_invoker chain.
            // No global middleware layer — each route declares its own auth
            // policy (auth: "none" for public, auth: "ryeos_signed" for
            // authenticated). The fallback dispatcher handles everything.
            // Shared handle clone for the remaining startup body; ends the
            // custody slot's mutable borrow before the root is borrowed. It
            // lives no longer than this startup scope, like the published copy.
            let app_state = app_state.clone();
            let published_state = Arc::new(app_state.clone());
            let publication = composition
                .root
                .publication(published_state)
                .context("pair HTTP/UDS AppState with selected Executor generation")?;
            let api_state = ryeos_api::ApiState {
                app: publication.app().clone(),
                publication: Some(publication.clone()),
                route_table,
                webhook_dedupe,
            };

            // Reconcile scheduler truth while holding the runtime gate through
            // application publication and recovery enqueue. Timer/recovery tasks may be
            // spawned below, but cannot claim a fire until Ready releases this guard.
            #[cfg(feature = "scheduling")]
            let scheduler_reconciled = if let Some((_, scheduler_runtime_gate, scheduler_service)) =
                &scheduler_runtime
            {
                startup.phase(
                    ryeos_node::StartupPhase::ReconcilingScheduler,
                    "reconciling scheduler state",
                )?;
                maintenance_schedule::ensure_maintenance_schedule(&app_state)
                    .context("materialize node-policy maintenance schedules")?;
                let scheduler_ctx = Arc::new(ryeosd::scheduler_impl::AppSchedulerContext(
                    Arc::new(app_state.clone()),
                    scheduler_service.clone(),
                ));
                let scheduler_reconcile_guard = scheduler_runtime_gate.clone().write_owned().await;
                let scheduler_intents = scheduler::reconcile::reconcile(&scheduler_ctx).await?;
                if ryeosd::shutdown_requested() {
                    tracing::info!("startup shutdown requested after scheduler reconciliation");
                    return Ok(());
                }
                Some((scheduler_ctx, scheduler_reconcile_guard, scheduler_intents))
            } else {
                None
            };
            #[cfg(feature = "scheduling")]
            let (scheduler_ctx, scheduler_reconcile_guard, scheduler_intents) = match scheduler_reconciled {
                Some((ctx, guard, intents)) => (Some(ctx), Some(guard), intents),
                None => (None, None, Vec::new()),
            };

            // Publish callback-capable application state while external admission is
            // still closed. UDS runtime callbacks from recovered work can now succeed;
            // ordinary HTTP/content requests continue to receive node_starting.
            startup.publish_application(publication, api_state)?;

            // Claim and enqueue every classified thread recovery action. The executor
            // preparation APIs persist SQLite launch ownership before detaching the
            // terminal runtime future; this loop therefore waits for durable admission,
            // never for recovered work to finish.
            let mut recovery_thread_targets = recovery_thread_targets;
            for action in &follow_actions {
                match action {
                    reconcile::FollowReconcileAction::Resume { follow_key } => {
                        let waiter = app_state
                            .state_store
                            .get_follow_waiter_by_key(follow_key)?
                            .ok_or_else(|| {
                                anyhow::anyhow!("follow recovery waiter disappeared: {follow_key}")
                            })?;
                        let successor_id = waiter.parent_successor_thread_id.ok_or_else(|| {
                            anyhow::anyhow!(
                                "follow recovery waiter {follow_key} has no parent successor"
                            )
                        })?;
                        recovery_thread_targets.insert(successor_id);
                    }
                    reconcile::FollowReconcileAction::RelaunchChild { child_thread_id } => {
                        recovery_thread_targets.insert(child_thread_id.clone());
                    }
                }
            }
            dispatch_resume_intents(&app_state, resume_intents).await?;

            // Drive the follow reconcile actions collected above. Each launch is claim-
            // guarded, so a duplicate with a live path is a benign `Skipped`. `Resume`
            // wakes a suspended parent; `RelaunchChild` re-fires a child stranded in the
            // pre-launch window.
            // Launch-window recovery: release slots whose member chain settled while
            // no kick could land (crash window), then admit and launch queued
            // members. Post-listener for the same reason as the intents above —
            // launched runtimes call back immediately.
            for (child_thread_id, outcome) in
                ryeos_executor::execution::launch::prepare_launch_window_recovery(&app_state)
                    .context("durably classify launch-window recovery")?
            {
                recovery_thread_targets.insert(child_thread_id.clone());
                tracing::info!(
                    child_thread_id,
                    ?outcome,
                    "launch-window recovery classified"
                );
            }

            prepare_follow_recovery_actions(&app_state, follow_actions)?;

            if !projection_health.is_current()
                && let Some(generation) = projection_health.begin_repair()
            {
                let repair_store = app_state.state_store.clone();
                let repaired =
                    tokio::task::spawn_blocking(move || repair_store.repair_thread_projection())
                        .await
                        .context("recovery projection repair task failed")?;
                projection_health.finish_repair(generation, &repaired);
                repaired.context("recovery projection repair failed")?;
            }
            let pre_scheduler_projection = projection_health.snapshot();
            if pre_scheduler_projection.status
                != ryeos_app::projection_health::ThreadProjectionState::Current
            {
                anyhow::bail!(
                    "thread projection became {:?} while startup recovery was enqueued",
                    pre_scheduler_projection.status
                );
            }

            // Persist/claim every recovered fire while the startup gate is still held.
            // `dispatch_fire` detaches execution after its durable scheduler record, so
            // this waits for enqueue—not for the scheduled work to finish.
            #[cfg(feature = "scheduling")]
            for intent in scheduler_intents {
                let fire_id = intent.fire_id.clone();
                let (Some(scheduler_ctx), Some(scheduler_reconcile_guard)) =
                    (&scheduler_ctx, &scheduler_reconcile_guard)
                else {
                    anyhow::bail!("scheduler recovery fire {fire_id} has no selected scheduler");
                };
                let outcome = scheduler::timer::dispatch_recovery_fire_under_startup_guard(
                    scheduler_ctx.clone(),
                    intent,
                    scheduler_reconcile_guard,
                )
                .await
                .with_context(|| format!("durably enqueue recovered scheduler fire {fire_id}"))?;
                tracing::info!(fire_id, ?outcome, "scheduler recovery fire classified");
            }
            // Detached recovery work may finish or fail quickly after preparation. Do
            // not publish Ready unless each original target is still owned, has attached
            // a live process, reached terminal, or remains in a durable follow/window
            // retry state. This turns task-side logging into an observable startup
            // classification boundary.
            ensure_recovery_targets_classified(&app_state, &recovery_thread_targets)?;

            // Scheduler recovery can itself create/project thread state. Take the
            // readiness snapshot only after every recovered fire has crossed its durable
            // dispatch boundary, repairing any journal work produced along the way.
            if !projection_health.is_current()
                && let Some(generation) = projection_health.begin_repair()
            {
                let repair_store = app_state.state_store.clone();
                let repaired =
                    tokio::task::spawn_blocking(move || repair_store.repair_thread_projection())
                        .await
                        .context("final recovery projection repair task failed")?;
                projection_health.finish_repair(generation, &repaired);
                repaired.context("final recovery projection repair failed")?;
            }
            let projection_snapshot = projection_health.snapshot();
            if projection_snapshot.status
                != ryeos_app::projection_health::ThreadProjectionState::Current
            {
                anyhow::bail!(
                    "thread projection became {:?} after scheduler recovery enqueue",
                    projection_snapshot.status
                );
            }
            let projection = serde_json::to_value(projection_snapshot)
                .context("serialize final projection readiness snapshot")?;

            let recovery_release_for_ready = recovery_execution_release.clone();
            startup.ready(projection, move || {
                recovery_release_for_ready.open();
                #[cfg(feature = "scheduling")]
                drop(scheduler_reconcile_guard);
            })?;

            // The ordinary timer does not exist until Ready is published, so
            // no timer fire can race the final internal-gate release boundary.
            #[cfg(feature = "scheduling")]
            let scheduler_task = match (scheduler_ctx, scheduler_reload_rx) {
                (Some(scheduler_ctx), Some(scheduler_reload_rx)) => {
                    let task = tokio::spawn(scheduler::timer::run(
                        scheduler_ctx,
                        scheduler_reload_rx,
                        shutdown_signal(),
                    ));
                    tracing::info!("scheduler: timer loop started after readiness publication");
                    Some(task)
                }
                (None, None) => None,
                _ => anyhow::bail!("scheduler context and reload channel disagree on selection"),
            };
            #[cfg(not(feature = "scheduling"))]
            let scheduler_task = None;

            supervise_background_tasks(
                app_state.clone(),
                ui_state_for_hints,
                scheduler_task,
            )
            .await
        };
        let mut daemon_work = Box::pin(daemon_work);
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum FirstExit {
            DaemonWork,
            HttpListener,
            UdsListener,
        }

        let (first_exit, mut result) = tokio::select! {
            result = &mut daemon_work => (FirstExit::DaemonWork, result),
            result = &mut http_task => (FirstExit::HttpListener, classify_http_listener_exit(result)),
            result = &mut uds_task => (FirstExit::UdsListener, classify_uds_listener_exit(result)),
        };
        // A request may already hold the immutable application publication.
        // Close its selected attachment before awaiting daemon work, listeners,
        // or cleanup; the returned Unresolved state is held only after drain.
        // Native tickets and Scheduler metadata close through the non-owning
        // fence; the full composition close below follows once the startup
        // future has released its borrow.
        root_admission_fence.close();
        native_admission_fence.close();
        let shutdown_was_requested = ryeosd::shutdown_requested();
        let first_exit_was_clean = result.is_ok();

        // A listener is part of the daemon, not a detached convenience. Losing
        // either one closes admission and signals every producer before the
        // still-owned startup/runtime future is awaited to its boundary.
        if first_exit != FirstExit::DaemonWork {
            startup.begin_shutdown();
            ryeosd::request_shutdown();
            let work_result = (&mut daemon_work).await;
            if result.is_ok() {
                result = work_result;
            } else if let Err(error) = work_result {
                tracing::error!(%error, "daemon work also failed while handling listener loss");
            }
        }

        // Both select paths have awaited daemon_work to its boundary. Drop
        // the owning future now, ending its mutable envelope borrow before
        // shutdown custody borrows the same original composition owners.
        drop(daemon_work);
        composition.close_root_admission();

        // No startup-owned task may remain parked behind a gate that can no
        // longer reach Ready. Waking it with cancellation drops transient
        // claims/tokens while preserving durable recovery truth for next boot.
        if startup.is_starting() {
            recovery_execution_release.cancel();
        }

        let startup_cancelled = result.is_err()
            && startup.is_starting()
            && shutdown_was_requested
            && (first_exit == FirstExit::DaemonWork || first_exit_was_clean);
        let startup_failed = result.is_err() && startup.is_starting() && !startup_cancelled;
        let uds_reporting_alive = first_exit == FirstExit::DaemonWork && !uds_task.is_finished();

        if startup_failed && uds_reporting_alive {
            let error = result.as_ref().expect_err("startup_failed implies error");
            if let Err(publish_error) = startup.failed(error) {
                tracing::error!(
                    error = %publish_error,
                    "failed to publish terminal startup failure"
                );
            }
            // The daemon-work future and all producers have already stopped.
            // Retain only the stable local diagnostics surface for the bounded
            // failure grace; never attempt this when UDS itself was lost.
            tokio::select! {
                _ = tokio::time::sleep(STARTUP_FAILURE_REPORTING_GRACE) => {}
                _ = shutdown_signal() => {}
            }
        }

        startup.begin_shutdown();
        ryeosd::request_shutdown();

        progress_task.abort();
        let _ = progress_task.await;

        if first_exit != FirstExit::HttpListener
            && let Err(error) = settle_http_listener(&mut http_task).await
        {
            if result.is_ok() {
                result = Err(error);
            } else {
                tracing::error!(%error, "HTTP listener also failed during daemon shutdown");
            }
        }
        if first_exit != FirstExit::UdsListener
            && let Err(error) = settle_uds_listener(&mut uds_task).await
        {
            if result.is_ok() {
                result = Err(error);
            } else {
                tracing::error!(%error, "UDS listener also failed during daemon shutdown");
            }
        }

        if let Some(state) = composition.app_state().cloned() {
            // The outer composition retains the original AppState across this
            // awaited task. A panic/cancellation drops only task clones.
            let drain_state = state.clone();
            let held_state = drain_state.clone();
            // The drain mode is chosen from the latch, after any planned
            // window point a suspended host crossed has been observed.
            let drain_latch = composition.service_stop.drain_latch();
            let drain_task = tokio::spawn(async move {
                drain_running_threads(&drain_state, &drain_latch).await
            });
            let drain_result = map_shutdown_drain_join(drain_task.await)
                .map(|outcome| composition.record_latched_drain_facts(outcome));
            let mut custody = Some((held_state, &*composition));
            complete_shutdown_drain(drain_result, &result, &mut custody).await;
            drop(custody);
            #[cfg(feature = "scheduling")]
            {
                let scheduler_service = state
                    .extensions
                    .get::<ryeos_scheduler_service::SchedulerService>();
                let close_result = match &scheduler_service {
                    Some(service) => service.close_after_shutdown_drain(),
                    None if composition.has_scheduler_custody() => Err(anyhow::anyhow!(
                        "selected scheduler database has no retained service close owner"
                    )),
                    None => Ok(()),
                };
                let close_result = close_result
                    .and_then(|()| composition.retain_scheduler_terminals_after_close());
                match close_result {
                    Ok(()) => {
                        drop(scheduler_service);
                        drop(state);
                        if composition.transfer_scheduler_native_finalization_owner().is_err() {
                            if result.is_ok() {
                                result = Err(anyhow::anyhow!(
                                    "scheduler finalization custody transfer did not complete"
                                ));
                            } else {
                                tracing::error!(
                                    "closed scheduler owner could not transfer into native finalization custody"
                                );
                            }
                        }
                        composition.release_app_state_after_drain();
                    }
                    Err(_error) => {
                        drop(scheduler_service);
                        drop(state);
                        composition.mark_services_drained_with_scheduler_close_pending();
                        if result.is_ok() {
                            result = Err(anyhow::anyhow!(
                                "scheduler database close did not complete"
                            ));
                        } else {
                            tracing::error!(
                                "scheduler database close failed while preserving its owner"
                            );
                        }
                    }
                }
            }
            #[cfg(not(feature = "scheduling"))]
            composition.release_app_state_after_drain();
        } else if let Some(pool) = composition.persistent_pool_custody.clone() {
            // Startup ended before AppState adoption. Reap the retained pool
            // directly; on any refusal the composition keeps its original Arc.
            let reaped = tokio::task::spawn_blocking(move || {
                pool.shutdown_and_reap_all(Duration::from_secs(
                    process::MAX_GRACEFUL_SHUTDOWN_GRACE_SECS,
                ))
            })
            .await;
            if matches!(reaped, Ok(Ok(_))) {
                composition.persistent_pool_custody.take();
            } else if result.is_ok() {
                result = Err(anyhow::anyhow!("unadopted persistent-session pool cleanup did not complete"));
            } else {
                tracing::error!("unadopted persistent-session pool cleanup remains unproved");
            }
        }

        (result, startup_cancelled, startup_failed)
    };
    let (mut daemon_result, startup_cancelled, startup_failed) = daemon_result;
    let exit_reason = |daemon_result: &Result<()>| if daemon_result.is_ok() || startup_cancelled {
        "signal"
    } else if startup_failed {
        "startup_failed"
    } else {
        "runtime_error"
    };
    // The settled finalizer records this as a provisional finalizing marker
    // before it retires the operator target. Only this run's own provisional
    // marker is later replaced, by compare-and-write, with the proved reason.
    let staged_reason = composition
        .service_stop
        .outcome()
        .map(ServiceStopOutcome::reason)
        .unwrap_or_else(|| exit_reason(&daemon_result));
    composition.staged_exit_record = Some((staged_reason,
        daemon_result.as_ref().err().map(|error| format!("{error:#}"))));
    composition.stop_accounting_publisher().await;
    let native_result = composition.advance_native_shutdown_on_settlement_stack();
    if composition.has_unresolved_custody() {
        // The custody exit below does not return, so name both causes first.
        if let Err(error) = &daemon_result {
            tracing::error!(error = %format!("{error:#}"), "daemon work failed before native custody hold");
        }
        if let Err(error) = &native_result {
            tracing::error!(error = %format!("{error:#}"), "native shutdown left custody unresolved");
        }
        if let Err(custody) = composition.hold_unresolved_custody().await {
            return Err(anyhow::Error::new(custody));
        }
    } else if native_result.is_err() {
        // Re-evaluated after any hold: only an error with no retained
        // custody left to resolve changes the daemon result.
        if daemon_result.is_ok() {
            daemon_result = Err(anyhow::anyhow!("native shutdown did not complete"));
        } else {
            tracing::error!("native shutdown refused without retained custody");
        }
    }

    // A fact latch at any point of the stop, including cleanup, makes this
    // the fact-refusal outcome; otherwise a reached window is a rollover.
    let service_outcome = composition.service_stop.outcome();
    composition.service_stop.outcome = service_outcome;
    let exit_reason = service_outcome
        .map(ServiceStopOutcome::reason)
        .unwrap_or_else(|| exit_reason(&daemon_result));
    let exit_detail = daemon_result
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"));
    composition.record_exit(exit_reason, exit_detail.as_deref(), daemon_result.as_ref().err());
    match service_outcome {
        Some(ServiceStopOutcome::ServiceRefusalStop) => {
            tracing::error!(
                shutdown_outcome = "service_refusal_stop",
                "native service refused a durable admission fact; orderly stop complete and supervised restart inhibited"
            );
            composition.inhibit_supervised_restart("service_refusal_stop");
        }
        Some(ServiceStopOutcome::ServiceWindowRollover) => {
            tracing::warn!(
                shutdown_outcome = "service_window_rollover",
                "native service window rolled over; orderly stop complete and the next incarnation may start"
            );
        }
        None => {}
    }
    match daemon_result.as_ref() {
        Ok(()) => tracing::info!(reason = exit_reason, "daemon exiting"),
        Err(error) => tracing::error!(reason = exit_reason, error = %error, "daemon exiting"),
    }
    if startup_cancelled {
        Ok(())
    } else {
        daemon_result
    }
}

fn classify_http_listener_exit(
    result: std::result::Result<std::io::Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match result {
        Err(error) => Err(anyhow::Error::new(error).context("http listener task stopped")),
        Ok(Err(error)) => Err(anyhow::Error::new(error).context("http listener failed")),
        Ok(Ok(())) if ryeosd::shutdown_requested() => Ok(()),
        Ok(Ok(())) => anyhow::bail!("http listener exited before shutdown"),
    }
}

fn classify_uds_listener_exit(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match result {
        Err(error) => Err(anyhow::Error::new(error).context("UDS listener task stopped")),
        Ok(Err(error)) => Err(error.context("UDS listener failed")),
        Ok(Ok(())) if ryeosd::shutdown_requested() => Ok(()),
        Ok(Ok(())) => anyhow::bail!("UDS listener exited before shutdown"),
    }
}

const LISTENER_SHUTDOWN_TIMEOUT: Duration = process::LISTENER_SHUTDOWN_BOUND;

async fn settle_http_listener(
    task: &mut tokio::task::JoinHandle<std::io::Result<()>>,
) -> Result<()> {
    match tokio::time::timeout(LISTENER_SHUTDOWN_TIMEOUT, &mut *task).await {
        Ok(joined) => classify_http_listener_exit(joined),
        Err(_) => {
            tracing::warn!("HTTP listener exceeded shutdown grace; aborting remaining connections");
            task.abort();
            let _ = task.await;
            Ok(())
        }
    }
}

async fn settle_uds_listener(task: &mut tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    match tokio::time::timeout(LISTENER_SHUTDOWN_TIMEOUT, &mut *task).await {
        Ok(joined) => classify_uds_listener_exit(joined),
        Err(_) => {
            tracing::warn!("UDS listener exceeded shutdown grace; aborting remaining connections");
            task.abort();
            let _ = task.await;
            Ok(())
        }
    }
}

/// Cross the durable ownership boundary for every active-thread resume intent.
/// The same path is used at startup (while the execution gate is armed) and by
/// the periodic live sweep (after the gate opens), so recovery semantics cannot
/// drift between boot and mid-life repair.
async fn dispatch_resume_intents(
    state: &AppState,
    intents: Vec<reconcile::ResumeIntent>,
) -> Result<()> {
    for intent in intents {
        let thread_id = intent.thread_id.clone();
        if matches!(
            intent.kind,
            reconcile::ResumeKind::OperatorContinuation | reconcile::ResumeKind::Continuation
        ) {
            match intent.launch_driver {
                ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime => {}
                ryeos_state::objects::ExecutionLaunchDriver::DirectItemExecutor => {
                    tracing::error!(
                        thread_id,
                        recovery_kind = ?intent.kind,
                        "continuation recovery requires the managed-runtime driver admitted by the continuation protocol"
                    );
                    let outcome =
                        ryeos_executor::execution::launch::settle_recovery_preparation_refusal(
                            state,
                            &thread_id,
                            "continuation_driver_invalid",
                            "continuation_driver",
                            "continuation recovery cannot use the direct-item launch driver",
                        )?;
                    tracing::warn!(
                        thread_id,
                        ?outcome,
                        "invalid continuation driver refusal classified"
                    );
                    continue;
                }
                ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler => {
                    anyhow::bail!(
                        "recovery invariant violated: in-process handler {} reached continuation dispatch ({:?})",
                        thread_id,
                        intent.kind
                    );
                }
            }
            let outcome = match intent.kind {
                reconcile::ResumeKind::OperatorContinuation => {
                    ryeos_executor::execution::launch::prepare_and_spawn_operator_successor_recovery(
                        state.clone(),
                        &thread_id,
                    )
                }
                reconcile::ResumeKind::Continuation => {
                    ryeos_executor::execution::launch::prepare_and_spawn_successor_recovery(
                        state.clone(),
                        &thread_id,
                    )
                }
                reconcile::ResumeKind::AdmittedRoot | reconcile::ResumeKind::NativeResume => {
                    unreachable!("continuation kinds matched above")
                }
            };
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    return Err(anyhow::anyhow!(error)).with_context(|| {
                        format!("durably claim successor recovery for {thread_id}")
                    });
                }
            };
            tracing::info!(thread_id, ?outcome, "successor recovery classified");
            continue;
        }

        match intent.launch_driver {
            ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime => {
                let outcome = match intent.kind {
                reconcile::ResumeKind::AdmittedRoot => {
                    ryeos_executor::execution::launch::prepare_and_spawn_admitted_root_recovery(
                        state.clone(),
                        &thread_id,
                    )
                }
                reconcile::ResumeKind::NativeResume => {
                    ryeos_executor::execution::launch::prepare_and_spawn_existing_native_resume_recovery(
                        state.clone(),
                        &thread_id,
                    )
                }
                reconcile::ResumeKind::Continuation
                | reconcile::ResumeKind::OperatorContinuation => {
                    unreachable!("continuation kinds handled above")
                }
            };
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        return Err(anyhow::anyhow!(error)).with_context(|| {
                            format!("durably claim managed recovery for {thread_id}")
                        });
                    }
                };
                tracing::info!(thread_id, ?outcome, recovery_kind = ?intent.kind, "managed recovery classified");
                continue;
            }
            ryeos_state::objects::ExecutionLaunchDriver::DirectItemExecutor => {}
            ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler => {
                anyhow::bail!(
                    "recovery invariant violated: in-process handler {} reached subprocess dispatch ({:?})",
                    thread_id,
                    intent.kind
                );
            }
        }

        let params = match state
            .state_store
            .get_launch_metadata(&thread_id)
            .and_then(|metadata| {
                let metadata = metadata.ok_or_else(|| {
                    anyhow::anyhow!("resume thread {thread_id} has no launch metadata")
                })?;
                let sealed = metadata.sealed_root_request.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("resume thread {thread_id} has no sealed admitted request")
                })?;
                ryeos_executor::execution::runner::execution_params_from_sealed_root_request(
                    state,
                    &thread_id,
                    &intent.resume_context,
                    sealed,
                    None,
                )
            }) {
            Ok(params) => params,
            Err(error) => {
                tracing::error!(
                    thread_id,
                    error = %error,
                    "resume parameters could not be reconstructed; recording an owner-fenced refusal"
                );
                let outcome =
                    ryeos_executor::execution::launch::settle_recovery_preparation_refusal(
                        state,
                        &thread_id,
                        "resume_rebuild_failed",
                        "resume_rebuild",
                        "the sealed request for this thread can no longer be reconstructed",
                    )?;
                tracing::warn!(
                    thread_id,
                    ?outcome,
                    "resume reconstruction refusal classified"
                );
                continue;
            }
        };
        // Both launchers judge the item's declared node guarantees against
        // this node's current signed capability report before re-spawning
        // it: a refusal finalizes the row, and a report that cannot be read
        // just now leaves it for the next sweep within a bounded window.
        let outcome = match intent.kind {
            reconcile::ResumeKind::AdmittedRoot => {
                ryeos_executor::execution::runner::run_existing_admitted_root(
                    state.clone(),
                    thread_id.clone(),
                    intent.chain_root_id,
                    params,
                    intent.prior_status,
                )
                .await
            }
            reconcile::ResumeKind::NativeResume => {
                ryeos_executor::execution::runner::run_existing_detached(
                    state.clone(),
                    thread_id.clone(),
                    intent.chain_root_id,
                    params,
                    intent.prior_status,
                )
                .await
            }
            reconcile::ResumeKind::Continuation | reconcile::ResumeKind::OperatorContinuation => {
                unreachable!("continuation kinds handled above")
            }
        };
        match outcome {
            Ok(outcome) => {
                tracing::info!(thread_id, ?outcome, recovery_kind = ?intent.kind, "direct-executor recovery classified");
            }
            Err(error) => {
                let terminal = state
                    .state_store
                    .get_thread(&thread_id)?
                    .and_then(|thread| {
                        ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
                    })
                    .is_some_and(|status| status.is_terminal());
                if !terminal {
                    return Err(anyhow::anyhow!(error)).with_context(|| {
                        format!(
                            "direct-executor recovery for {thread_id} returned without a durable disposition"
                        )
                    });
                }
                tracing::warn!(
                    thread_id,
                    error = %error,
                    recovery_kind = ?intent.kind,
                    "direct-executor recovery classified as terminal failure"
                );
            }
        }
    }
    Ok(())
}

/// Persist and enqueue every startup follow recovery action. Each preparation
/// call returns only after its launch claim has been transferred to the owned
/// detached task, which is the pre-Ready durability boundary.
fn prepare_follow_recovery_actions(
    state: &AppState,
    actions: Vec<reconcile::FollowReconcileAction>,
) -> Result<()> {
    for action in actions {
        let (label, target_thread_id, outcome) = match action {
            reconcile::FollowReconcileAction::Resume { follow_key } => {
                let target_thread_id = state
                    .state_store
                    .get_follow_waiter_by_key(&follow_key)?
                    .and_then(|waiter| waiter.parent_successor_thread_id)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "follow recovery waiter {follow_key} has no successor target"
                        )
                    })?;
                let outcome =
                    ryeos_executor::execution::launch::prepare_and_spawn_follow_resume_recovery(
                        state.clone(),
                        &follow_key,
                    );
                (
                    format!("parent-resume {follow_key}"),
                    target_thread_id,
                    outcome,
                )
            }
            reconcile::FollowReconcileAction::RelaunchChild { child_thread_id } => {
                let outcome =
                    ryeos_executor::execution::launch::prepare_and_spawn_follow_child_recovery(
                        state.clone(),
                        &child_thread_id,
                    );
                (
                    format!("child-relaunch {child_thread_id}"),
                    child_thread_id,
                    outcome,
                )
            }
        };
        match outcome {
            Ok(outcome) => tracing::info!(action = %label, ?outcome, "follow recovery classified"),
            Err(error) => {
                return Err(anyhow::anyhow!(error)).with_context(|| {
                    format!("durably claim follow recovery {label} for target {target_thread_id}")
                });
            }
        }
    }
    Ok(())
}

/// Verify the pre-Ready recovery ownership boundary for the complete initial
/// `created|running` set. The startup execution gate keeps every detached
/// recovery task inert here, so each row must be terminal, attached to a
/// verified RyeOS process, protected by a launch claim, or owned by a durable
/// follow/launch-window/remote-adoption state machine. The same check runs in
/// live reconciliation: the existing daemon-handler registry is also an exact
/// live owner there, never a substitute for durable recovery after restart.
fn ensure_recovery_targets_classified(state: &AppState, targets: &BTreeSet<String>) -> Result<()> {
    for thread_id in targets {
        let thread = state
            .state_store
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("recovery target disappeared: {thread_id}"))?;
        let status = ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "recovery target {thread_id} has unknown status {:?}",
                    thread.status
                )
            })?;
        let live_owned_process = thread
            .runtime
            .process_identity
            .as_ref()
            .is_some_and(|identity| {
                ryeos_app::process::execution_liveness(identity)
                    == ryeos_app::process::IdentityLiveness::Alive
                    && ryeos_app::process::execution_group_liveness(identity)
                        == ryeos_app::process::IdentityLiveness::Alive
            });
        let durable_follow_owner = state
            .state_store
            .get_follow_waiter_by_child_chain(&thread.chain_root_id)?
            .is_some()
            || state
                .state_store
                .get_follow_waiter_by_parent_thread(thread_id)?
                .is_some()
            || state
                .state_store
                .get_follow_waiter_by_successor(thread_id)?
                .is_some();
        let durable_window_owner = state
            .state_store
            .launch_window_is_member(&thread.chain_root_id)?;
        let durable_follow_resume_edge = match thread.upstream_thread_id.as_deref() {
            Some(upstream_id) => state
                .state_store
                .is_follow_resume_successor(upstream_id, thread_id)?,
            None => false,
        };
        let durable_remote_adoption_edge = status == ryeos_state::objects::ThreadStatus::Created
            && thread.upstream_thread_id.is_some()
            && state
                .state_store
                .remote_continuation_authority(&thread.chain_root_id, thread_id)?
                .is_some();
        let durable_source_handoff_owner = match state
            .state_store
            .active_source_worker_handoff_for_placement(thread_id)?
        {
            Some((_job, operation)) if operation.chain_root_id == thread.chain_root_id => true,
            Some((_job, operation)) => {
                anyhow::bail!(
                    "active source handoff {} contradicts recovery target {thread_id} chain authority",
                    operation.operation_id
                );
            }
            None => false,
        };
        if status.is_terminal()
            || live_owned_process
            // Reuse the owner checked by reconcile_in_process_handler. A
            // reservation alone is not liveness, and this volatile registry
            // is empty for predecessor handlers after daemon restart.
            || state.state_store.is_in_process_handler_active(thread_id)?
            || state.state_store.get_launch_claim(thread_id)?.is_some()
            || thread.runtime.recovery_wait.is_some()
            || durable_follow_owner
            || durable_follow_resume_edge
            || durable_remote_adoption_edge
            || durable_source_handoff_owner
            || durable_window_owner
        {
            continue;
        }
        // A retained workspace journal is a durable quarantine owner, not
        // permission to resume or clean up. Recheck its exact retained state;
        // neither an in-memory blocked set nor a NULL PID grants this status.
        if state
            .state_store
            .has_retained_workspace_quarantine(thread_id)?
        {
            tracing::warn!(
                thread_id,
                "recovery target remains workspace-quarantined; execution and cleanup stay fenced"
            );
            continue;
        }
        anyhow::bail!(
            "recovery target {thread_id} reached readiness without terminal state, a verified live process, an active daemon handler, or durable claim/wait/follow/handoff/window ownership"
        );
    }
    Ok(())
}

/// Drive follow reconcile actions during the periodic live sweep. Every launch
/// is claim-guarded, so concurrent drives are benign skips. Retryable launch
/// interruptions retain their durable waiter; permanent launch defects must
/// settle their exact target before returning. Missing dispositions fail only
/// this pass and are isolated from daemon availability by the outer loop.
async fn dispatch_follow_actions(
    state: &AppState,
    actions: Vec<reconcile::FollowReconcileAction>,
) -> Result<()> {
    for action in actions {
        use ryeos_executor::execution::launch::{SuccessorLaunchOutcome, launch_follow_child};
        let (label, target_thread_id, resume_follow_key, outcome) = match action {
            reconcile::FollowReconcileAction::Resume { follow_key } => {
                let target_thread_id = state
                    .state_store
                    .get_follow_waiter_by_key(&follow_key)?
                    .and_then(|waiter| waiter.parent_successor_thread_id)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "periodic follow waiter {follow_key} has no successor target"
                        )
                    })?;
                let outcome = ryeos_executor::execution::launch::launch_follow_resume_successor(
                    state.clone(),
                    &follow_key,
                )
                .await;
                (
                    format!("parent-resume {follow_key}"),
                    target_thread_id,
                    Some(follow_key),
                    outcome,
                )
            }
            reconcile::FollowReconcileAction::RelaunchChild { child_thread_id } => {
                // Reconcile parity: a fresh relaunch, no parent clamp/depth.
                let outcome =
                    launch_follow_child(state.clone(), &child_thread_id, None, None).await;
                (
                    format!("child-relaunch {child_thread_id}"),
                    child_thread_id,
                    None,
                    outcome,
                )
            }
        };
        match outcome {
            Ok(SuccessorLaunchOutcome::Launched(_)) => {}
            Ok(SuccessorLaunchOutcome::Skipped(reason)) => {
                tracing::debug!(action = %label, reason, "reconcile: follow action skipped");
            }
            Err(error) if error.retryable_launch_interruption() => {
                tracing::warn!(
                    action = %label,
                    thread_id = %target_thread_id,
                    error = %error,
                    "periodic follow recovery was interrupted; durable waiter retained for retry"
                );
            }
            Err(error) => {
                let terminal = state
                    .state_store
                    .get_thread(&target_thread_id)?
                    .and_then(|thread| {
                        ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
                    })
                    .is_some_and(|status| status.is_terminal());
                if terminal {
                    if let Some(follow_key) = resume_follow_key.as_deref()
                        && state
                            .state_store
                            .get_follow_waiter_by_key(follow_key)?
                            .is_some()
                    {
                        return Err(anyhow::anyhow!(error)).with_context(|| {
                            format!(
                                "periodic follow recovery {label} terminalized its successor but did not retire waiter {follow_key}"
                            )
                        });
                    }
                    tracing::warn!(
                        action = %label,
                        thread_id = %target_thread_id,
                        error = %error,
                        "periodic follow recovery classified as a permanent terminal refusal"
                    );
                    continue;
                }
                return Err(anyhow::anyhow!(error)).with_context(|| {
                    format!(
                        "periodic follow recovery {label} returned without a durable disposition"
                    )
                });
            }
        }
    }
    Ok(())
}

async fn run_periodic_recovery(state: AppState) -> Result<()> {
    if !ryeos_app::recovery_execution_gate::wait_if_armed().await {
        return Ok(());
    }
    if let Err(error) =
        ryeos_api::handlers::external_content_activate::recover_durable_activations(&state).await
    {
        tracing::error!(
            error = %error,
            "initial managed external-content recovery failed; durable jobs remain inspectable"
        );
    }
    if let Err(error) = recover_durable_project_head_reconciliations(&state).await {
        tracing::error!(
            error = %error,
            "initial remote project-head reconciliation failed; durable jobs remain retryable"
        );
    }
    if let Err(error) = recover_durable_worker_result_pulls(&state).await {
        tracing::error!(
            error = %error,
            "initial hosted worker-result pull recovery failed; durable jobs remain retryable"
        );
    }
    match state.threads.reconcile_remote_follow_terminal_deliveries() {
        Ok(reconciled) if reconciled != 0 => tracing::info!(
            reconciled,
            "verified or rebuilt remote follow-terminal delivery jobs from authoritative terminals"
        ),
        Ok(_) => {}
        Err(error) => tracing::error!(
            error = %error,
            "remote follow-terminal job reconstruction failed; signed terminals remain authoritative"
        ),
    }
    if let Err(error) =
        ryeos_api::handlers::dedicated_sessions::recover_durable_source_handoffs(&state).await
    {
        tracing::error!(
            error = %error,
            "initial source worker-handoff recovery failed; durable jobs remain retryable"
        );
    }
    if let Err(error) =
        ryeos_api::handlers::worker_placements::recover_durable_target_handoffs(&state).await
    {
        tracing::error!(
            error = %error,
            "initial target worker-handoff recovery failed; durable jobs remain retryable"
        );
    }
    if let Err(error) =
        ryeos_api::handlers::federated_follow::recover_durable_remote_follow_deliveries(&state)
            .await
    {
        tracing::error!(
            error = %error,
            "initial remote follow-terminal recovery failed; durable jobs remain retryable"
        );
    }
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let period = Duration::from_secs(120);
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = tick.tick() => {}
        }

        let pass_result = run_periodic_recovery_pass(&state).await;
        if let Err(error) = pass_result {
            tracing::error!(
                error = %error,
                "periodic recovery pass failed; durable recovery state preserved and daemon remains available"
            );
        }
    }
}

/// Provider cleanup can spend the signed contact timeout on each retained
/// source. Keep it outside startup readiness and the general recovery pass so
/// an unavailable provider cannot delay unrelated durable recovery. The
/// blocking owner records its one-shot contact before any provider mutation;
/// interruption leaves an inspectable obligation for the next pass or boot.
async fn run_bootstrap_source_cleanup_recovery(state: AppState) -> Result<()> {
    if !ryeos_app::recovery_execution_gate::wait_if_armed().await {
        return Ok(());
    }
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let period = Duration::from_secs(120);
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now(), period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = tick.tick() => {}
        }
        let recovery = recover_bootstrap_source_cleanups_async(state.clone());
        tokio::pin!(recovery);
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            result = &mut recovery => match result {
                Ok(report) => log_bootstrap_source_cleanup_recovery(report, "background"),
                Err(error) => tracing::error!(
                    error = %error,
                    "bootstrap source cleanup recovery failed; durable obligations remain"
                ),
            }
        }
    }
}

/// Settle the predecessor controller generation before Ready can release new
/// external execution. Import runs first so a sealed result that was already
/// delivered remains authoritative; fencing then makes every surviving
/// occurrence cleanup-only. Cleanup failures remain durably quarantined and
/// are retried by the periodic pass, but discovery/fencing failures abort
/// startup rather than admitting work beside an unfenced predecessor.
async fn recover_external_candidates_before_ready(state: &AppState) -> Result<()> {
    let recovered = state
        .external_candidate_imports
        .wake_recoverable(Arc::clone(&state.state_store), None)
        .context("discover recoverable external candidate imports")?;
    if recovered != 0 {
        tracing::info!(
            recovered,
            "woke recoverable external candidate imports before readiness"
        );
    }
    wait_for_external_candidate_imports_async(
        Arc::clone(&state.external_candidate_imports),
        lillux::time::Duration::from_secs(30),
    )
    .await
    .context("drain external candidate imports before restart fencing")?;

    let fenced =
        ryeos_app::external_placement::fence_external_candidates_after_controller_restart(state)
            .context("fence predecessor-generation external candidates")?;
    if fenced != 0 {
        tracing::warn!(
            fenced,
            "fenced predecessor-generation external candidates before readiness"
        );
    }

    log_external_candidate_cleanup_recovery(
        recover_external_candidate_cleanups_async(state.clone())
            .await
            .context("advance startup external candidate cleanup")?,
        "startup",
    );
    // Bootstrap sources are already durably fenced by their own journal.
    // Their provider cleanup runs after Ready under a separate supervised
    // owner, so a provider timeout cannot hold the startup gate.
    Ok(())
}

async fn run_cache_metric_flush_loop() -> Result<()> {
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let period = Duration::from_secs(5);
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                ryeos_tracing::flush_cache_metrics();
                return Ok(());
            }
            _ = tick.tick() => ryeos_tracing::flush_cache_metrics_due(),
        }
    }
}

async fn run_periodic_recovery_pass(state: &AppState) -> Result<()> {
    let recovered_project_applies =
        ryeos_api::handlers::project_apply_snapshot::retry_retained_project_snapshot_applies(state)
            .await
            .context("periodic retained project snapshot recovery")?;
    if recovered_project_applies != 0 {
        tracing::info!(
            recovered_project_applies,
            "periodic recovery resolved retained project schedule owners"
        );
    }
    let recovered_external_imports = state
        .external_candidate_imports
        .wake_recoverable(Arc::clone(&state.state_store), None)
        .context("periodic external candidate import recovery")?;
    if recovered_external_imports != 0 {
        tracing::info!(
            recovered_external_imports,
            "periodic recovery woke external candidate imports"
        );
    }
    log_external_candidate_cleanup_recovery(
        recover_external_candidate_cleanups_async(state.clone())
            .await
            .context("periodic external candidate cleanup recovery")?,
        "periodic",
    );
    let recovered_activations =
        ryeos_api::handlers::external_content_activate::recover_durable_activations(state)
            .await
            .context("periodic managed external-content recovery")?;
    if recovered_activations != 0 {
        tracing::info!(
            recovered_activations,
            "periodic recovery advanced managed external-content activations"
        );
    }
    let recovered_project_heads = recover_durable_project_head_reconciliations(state)
        .await
        .context("periodic remote project-head reconciliation")?;
    if recovered_project_heads != 0 {
        tracing::info!(
            recovered_project_heads,
            "periodic recovery completed remote project-head reconciliations"
        );
    }
    let recovered_worker_results = recover_durable_worker_result_pulls(state)
        .await
        .context("periodic hosted worker-result pull recovery")?;
    if recovered_worker_results != 0 {
        tracing::info!(
            recovered_worker_results,
            "periodic recovery completed hosted worker-result pulls"
        );
    }
    let recovered_source_handoffs =
        ryeos_api::handlers::dedicated_sessions::recover_durable_source_handoffs(state)
            .await
            .context("periodic source worker-handoff recovery")?;
    if recovered_source_handoffs != 0 {
        tracing::info!(
            recovered_source_handoffs,
            "periodic recovery completed source worker handoffs"
        );
    }
    let recovered_handoffs =
        ryeos_api::handlers::worker_placements::recover_durable_target_handoffs(state)
            .await
            .context("periodic target worker-handoff recovery")?;
    if recovered_handoffs != 0 {
        tracing::info!(
            recovered_handoffs,
            "periodic recovery completed target worker handoffs"
        );
    }
    let recovered_follow_deliveries =
        ryeos_api::handlers::federated_follow::recover_durable_remote_follow_deliveries(state)
            .await
            .context("periodic remote follow-terminal recovery")?;
    if recovered_follow_deliveries != 0 {
        tracing::info!(
            recovered_follow_deliveries,
            "periodic recovery completed remote follow-terminal deliveries"
        );
    }
    ryeos_app::cascade::repair_cancelled_window_members(state)
        .context("periodic cancelled launch-window repair")?;

    let active = reconcile::reconcile_live_threads(state)
        .await
        .context("periodic active-thread reconcile")?;
    let mut targets = active.active_thread_ids;
    dispatch_resume_intents(state, active.resume_intents)
        .await
        .context("periodic active-thread recovery dispatch")?;

    let follow_actions = reconcile::reconcile_follow(state).context("periodic follow reconcile")?;
    for action in &follow_actions {
        match action {
            reconcile::FollowReconcileAction::Resume { follow_key } => {
                let waiter = state
                    .state_store
                    .get_follow_waiter_by_key(follow_key)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("periodic follow waiter disappeared: {follow_key}")
                    })?;
                if let Some(successor) = waiter.parent_successor_thread_id {
                    targets.insert(successor);
                }
            }
            reconcile::FollowReconcileAction::RelaunchChild { child_thread_id } => {
                targets.insert(child_thread_id.clone());
            }
        }
    }
    dispatch_follow_actions(state, follow_actions).await?;

    for (child_thread_id, outcome) in
        ryeos_executor::execution::launch::prepare_launch_window_recovery(state)
            .context("periodic launch-window recovery")?
    {
        targets.insert(child_thread_id.clone());
        tracing::info!(
            child_thread_id,
            ?outcome,
            "periodic launch-window recovery classified"
        );
    }

    ensure_recovery_targets_classified(state, &targets)
        .context("periodic recovery ownership boundary")?;
    Ok(())
}

async fn recover_external_candidate_cleanups_async(
    state: AppState,
) -> Result<ryeos_app::external_placement::ExternalCandidateCleanupRecovery> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let task = lillux::task::spawn_host_task("external-cleanup-recovery", move || {
        let _ =
            sender.send(ryeos_app::external_placement::recover_external_candidate_cleanups(&state));
    })
    .context("start Lillux external cleanup recovery owner")?;
    task.detach();
    receiver
        .await
        .context("external cleanup recovery owner stopped without a result")?
}

async fn recover_bootstrap_source_cleanups_async(
    state: AppState,
) -> Result<ryeos_app::operator_runtime_snapshot::BootstrapSourceCleanupRecovery> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let task = lillux::task::spawn_host_task("bootstrap-source-cleanup-recovery", move || {
        let _ = sender
            .send(ryeos_app::operator_runtime_snapshot::recover_bootstrap_source_cleanups(&state));
    })
    .context("start Lillux bootstrap source cleanup recovery owner")?;
    task.detach();
    receiver
        .await
        .context("bootstrap source cleanup recovery owner stopped without a result")?
}

async fn wait_for_external_candidate_imports_async(
    imports: Arc<ryeos_app::external_candidate_import::ExternalCandidateImportPool>,
    timeout: lillux::time::Duration,
) -> Result<()> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let task = lillux::task::spawn_host_task("external-import-startup-fence", move || {
        let _ = sender.send(imports.wait_for_idle(timeout));
    })
    .context("start Lillux external import startup fence")?;
    task.detach();
    receiver
        .await
        .context("external import startup fence stopped without a result")?
}

fn log_external_candidate_cleanup_recovery(
    report: ryeos_app::external_placement::ExternalCandidateCleanupRecovery,
    pass: &'static str,
) {
    if report.discovered != 0 {
        tracing::info!(
            pass,
            discovered = report.discovered,
            proved = report.proved,
            pending = report.pending,
            failures = report.failures.len(),
            "advanced retained external candidate cleanup obligations"
        );
    }
    for (placement, error) in report.failures {
        tracing::error!(
            pass,
            placement = %placement,
            error = %error,
            "external candidate cleanup remains retained after reconciliation failure"
        );
    }
}

fn log_bootstrap_source_cleanup_recovery(
    report: ryeos_app::operator_runtime_snapshot::BootstrapSourceCleanupRecovery,
    pass: &'static str,
) {
    if report.discovered != 0 {
        tracing::info!(
            pass,
            discovered = report.discovered,
            terminal = report.terminal,
            pending = report.pending,
            failures = report.failures.len(),
            "advanced retained bootstrap source cleanup obligations"
        );
    }
    for (operation_id, error) in report.failures {
        tracing::error!(
            pass,
            operation_id = %operation_id,
            error = %error,
            "bootstrap source cleanup remains retained after reconciliation failure"
        );
    }
}

async fn run_ui_hint_loop(hub: Arc<ThreadEventHub>, ui: Arc<ryeos_ui::UiState>) -> Result<()> {
    let mut rx = hub.subscribe_all();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut activity_tick = tokio::time::interval(Duration::from_millis(750));
    activity_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pending_activity: HashMap<String, u64> = HashMap::new();

    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            recv = rx.recv() => {
                match recv {
                    Ok(event) => {
                        if ryeos_api::routes::invokers::stream_helpers::is_lifecycle_hint(
                            &event.event_type,
                        ) {
                            let status = ryeos_api::routes::invokers::stream_helpers::lifecycle_status(
                                &event.event_type,
                            );
                            let payload = serde_json::json!({
                                "kind": "thread",
                                "thread_id": &event.thread_id,
                                "chain_root_id": &event.chain_root_id,
                                "event_type": &event.event_type,
                                "status": status,
                                "updated_at": &event.ts,
                            });
                            for session_id in ui.browser_sessions.session_ids() {
                                ui.session_bus.publish(&session_id, "thread.hint", payload.clone());
                            }
                        } else if !event.event_type.starts_with("seat.") {
                            *pending_activity.entry(event.thread_id).or_insert(0) += 1;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        anyhow::bail!("thread event hub closed before daemon shutdown")
                    }
                }
            }
            _ = activity_tick.tick() => {
                if pending_activity.is_empty() {
                    continue;
                }
                let thread_ids = pending_activity.keys().cloned().collect::<Vec<_>>();
                let event_count = pending_activity.values().sum::<u64>();
                pending_activity.clear();
                let payload = serde_json::json!({
                    "kind": "activity",
                    "thread_ids": thread_ids,
                    "event_count": event_count,
                });
                for session_id in ui.browser_sessions.session_ids() {
                    ui.session_bus.publish(&session_id, "thread.hint", payload.clone());
                }
            }
        }
    }
}

/// A daemon whose composition does not select the scheduler may perform
/// terminal-history cleanup only while the pinned runtime and node namespaces
/// continue to prove no scheduler DB, fire history, or signed schedule source
/// exists. The process StateLock is also retained across startup recovery.
struct SchedulerFreeStartupAbsence {
    runtime: lillux::PinnedDirectory,
    node: Option<lillux::PinnedDirectory>,
    node_path: std::path::PathBuf,
}

fn require_absent_scheduler_history(
    app_root: &std::path::Path,
) -> Result<SchedulerFreeStartupAbsence> {
    let runtime_path = app_root.join(ryeos_engine::AI_DIR).join("state");
    let runtime = lillux::PinnedDirectory::open(&runtime_path)?
        .context("scheduler-free startup requires a pinned runtime directory")?;
    let node_path = app_root.join(ryeos_engine::AI_DIR).join("node");
    let node = lillux::PinnedDirectory::open(&node_path)?;
    let guard = SchedulerFreeStartupAbsence {
        runtime,
        node,
        node_path,
    };
    guard.verify_absent()?;
    Ok(guard)
}

impl SchedulerFreeStartupAbsence {
    fn verify_absent(&self) -> Result<()> {
        self.runtime.ensure_path_binding()?;
        for name in self.runtime.entry_names()? {
            let Some(name) = name.to_str() else {
                anyhow::bail!(
                    "cannot prove scheduler-history absence from a non-UTF-8 runtime entry"
                );
            };
            if name.starts_with("scheduler") || name == "schedules" {
                anyhow::bail!(
                    "scheduler history requires the selected scheduler composition; {}",
                    ryeos_app::execution_history_reset::SCHEDULER_STATE_REMEDY
                );
            }
        }
        if let Some(node) = self.node.as_ref() {
            node.ensure_path_binding()?;
            ryeos_app::execution_history_reset::require_no_signed_schedule_sources(node)?;
        } else if lillux::PinnedDirectory::open(&self.node_path)?.is_some() {
            anyhow::bail!("node configuration appeared during scheduler-free startup");
        }
        Ok(())
    }
}

#[cfg(test)]
mod scheduler_free_startup_tests {
    use super::*;

    fn make_state_dirs(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let runtime = root.join(ryeos_engine::AI_DIR).join("state");
        let node = root.join(ryeos_engine::AI_DIR).join("node");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&node).unwrap();
        (runtime, node)
    }

    #[test]
    fn scheduler_free_startup_accepts_only_scheduler_free_namespaces() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, node) = make_state_dirs(root.path());
        let guard = require_absent_scheduler_history(root.path()).unwrap();
        guard.verify_absent().unwrap();

        std::fs::write(runtime.join("scheduler.sqlite3"), b"untrusted").unwrap();
        assert!(guard.verify_absent().is_err());
        std::fs::remove_file(runtime.join("scheduler.sqlite3")).unwrap();

        // An empty schedules directory carries no schedule source.
        std::fs::create_dir(node.join("schedules")).unwrap();
        guard.verify_absent().unwrap();

        let source = node.join("schedules").join("maintenance-gc.yaml");
        std::fs::write(&source, b"signed schedule").unwrap();
        let refusal = guard.verify_absent().unwrap_err().to_string();
        assert!(
            refusal.contains(ryeos_app::execution_history_reset::SCHEDULER_STATE_REMEDY),
            "{refusal}"
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"signed schedule");
    }

    #[test]
    fn scheduler_free_startup_refuses_replaced_held_node_namespace() {
        let root = tempfile::tempdir().unwrap();
        let (_runtime, node) = make_state_dirs(root.path());
        let guard = require_absent_scheduler_history(root.path()).unwrap();
        let retired = node.with_extension("retired");
        std::fs::rename(&node, &retired).unwrap();
        std::fs::create_dir(&node).unwrap();
        assert!(guard.verify_absent().is_err());
    }
}

async fn supervise_background_tasks(
    state: AppState,
    ui: Arc<ryeos_ui::UiState>,
    scheduler_task: Option<tokio::task::JoinHandle<Result<()>>>,
) -> Result<()> {
    let mut tasks = tokio::task::JoinSet::<(&'static str, Result<()>)>::new();

    if let Some(scheduler_task) = scheduler_task {
        tasks.spawn(async move {
            let result = scheduler_task
                .await
                .context("scheduler timer task join failed")
                .and_then(|result| result);
            ("scheduler timer", result)
        });
    }

    let repair_store = state.state_store.clone();
    tasks.spawn(async move {
        (
            "projection repair",
            ryeos_app::projection_repair::run(repair_store, shutdown_signal()).await,
        )
    });

    let periodic_state = state.clone();
    tasks.spawn(async move {
        (
            "periodic recovery",
            run_periodic_recovery(periodic_state).await,
        )
    });

    let bootstrap_cleanup_state = state.clone();
    tasks.spawn(async move {
        (
            "bootstrap source cleanup recovery",
            run_bootstrap_source_cleanup_recovery(bootstrap_cleanup_state).await,
        )
    });

    tasks.spawn(async move { ("cache metric flush", run_cache_metric_flush_loop().await) });

    let hint_hub = state.event_streams.clone();
    tasks.spawn(async move { ("UI hint fanout", run_ui_hint_loop(hint_hub, ui).await) });

    let first_result = tokio::select! {
        _ = shutdown_signal() => Ok(()),
        joined = tasks.join_next() => {
            match joined {
                Some(Ok((name, result))) if ryeosd::shutdown_requested() => {
                    result.with_context(|| format!("{name} failed during shutdown"))
                }
                Some(Ok((name, Ok(())))) => {
                    anyhow::bail!("supervised {name} task exited before shutdown")
                }
                Some(Ok((name, Err(error)))) => {
                    Err(error).with_context(|| format!("supervised {name} task failed"))
                }
                Some(Err(error)) => {
                    Err(anyhow::Error::new(error).context("supervised daemon task panicked"))
                }
                None => anyhow::bail!("all supervised daemon tasks disappeared"),
            }
        }
    };

    // One process-wide signal closes every task and both listeners. Projection
    // repair is not aborted: if it is inside spawn_blocking, supervision waits
    // for that bounded journal-replay unit to finish before state is drained.
    ryeosd::request_shutdown();
    let mut drain_error = None;
    loop {
        let joined = tasks.join_next().await;
        let Some(joined) = joined else {
            break;
        };
        match joined {
            Ok((_, Ok(()))) => {}
            Ok((name, Err(error))) => {
                drain_error.get_or_insert_with(|| error.context(format!("{name} shutdown")));
            }
            Err(error) => {
                drain_error.get_or_insert_with(|| {
                    anyhow::Error::new(error).context("supervised task shutdown join")
                });
            }
        }
    }

    match first_result {
        Err(error) => Err(error),
        Ok(()) => match drain_error {
            Some(error) => Err(error),
            None => Ok(()),
        },
    }
}

/// Called only after the shutdown process owner proves exact group death.
/// The waiting task may have relinquished its launch owner during shutdown;
/// both existing settlement paths compare the same attachment and binding.
/// If descendants or another owner still retain contact, preserve the identity
/// for cold recovery instead of detaching it independently of membership.
fn settle_shutdown_process(
    state: &AppState,
    thread_id: &str,
    identity: &process::ExecutionProcessIdentity,
) -> Result<bool> {
    if let Some(binding) = state.state_store.thread_workspace_binding(thread_id)? {
        if state
            .state_store
            .settle_reaped_thread_workspace_owned(thread_id, &binding, identity)?
        {
            return Ok(true);
        }
        state
            .state_store
            .settle_dead_thread_workspace_if_matches(thread_id, &binding, identity)
    } else {
        state
            .state_store
            .clear_thread_process_if_matches(thread_id, identity)
    }
}

/// One attached process as the stop drains it.
struct DrainAttached {
    thread_id: String,
    // `None` when the stop could not read the thread row (latched snapshot).
    status: Option<String>,
    identity: process::ExecutionProcessIdentity,
    action: process::ShutdownAction,
}

/// Attached processes from the in-memory attachment records, without SQL.
/// A record with no identity can never be proved dead, so the snapshot is
/// then unclean.
fn latched_attached_snapshot(state: &AppState) -> (Vec<DrainAttached>, bool) {
    let mut clean = true;
    let mut attached = Vec::new();
    for record in state.state_store.attached_process_snapshot() {
        let ryeos_app::state_store::AttachedProcess {
            thread_id,
            process_identity,
            cancellation_mode,
        } = record;
        let Some(identity) = process_identity else {
            clean = false;
            tracing::warn!(thread_id, "attached record has no process identity to prove dead");
            continue;
        };
        attached.push(DrainAttached {
            thread_id,
            status: None,
            identity,
            action: process::resolve_shutdown_action(cancellation_mode),
        });
    }
    (attached, clean)
}

/// Attached processes read from their durable rows. Returns the snapshot,
/// whether it is clean, and whether any row read failed.
fn normal_attached_snapshot(state: &AppState) -> Result<(Vec<DrainAttached>, bool, bool)> {
    let attached_ids = state
        .state_store
        .list_attached_thread_ids()
        .context("list attached threads during shutdown")?;
    let mut clean = true;
    let mut read_failed = false;
    let mut attached = Vec::with_capacity(attached_ids.len());
    for thread_id in attached_ids {
        let thread = match state.state_store.get_thread(&thread_id) {
            Ok(Some(thread)) => thread,
            Ok(None) => {
                clean = false;
                tracing::warn!(thread_id, "attached runtime row has no thread");
                continue;
            }
            Err(error) => {
                clean = false;
                read_failed = true;
                tracing::warn!(thread_id, error = %error, "failed to load attached thread");
                continue;
            }
        };
        let Some(identity) = thread.runtime.process_identity.clone() else {
            clean = false;
            tracing::warn!(thread_id, "attached row lost its durable process identity");
            continue;
        };
        let action = process::resolve_shutdown_action(
            thread
                .runtime
                .launch_metadata
                .as_ref()
                .and_then(|metadata| metadata.cancellation_mode),
        );
        attached.push(DrainAttached {
            thread_id,
            status: Some(thread.status),
            identity,
            action,
        });
    }
    Ok((attached, clean, read_failed))
}

/// Normal-mode settlement of one attached process already proved dead.
/// `Ok(false)` keeps the exact authority for cold recovery.
fn settle_attached_normal(
    state: &AppState,
    thread_id: &str,
    identity: &process::ExecutionProcessIdentity,
) -> Result<bool> {
    if settle_shutdown_process(state, thread_id, identity)? {
        return Ok(true);
    }
    match state.state_store.get_thread(thread_id)? {
        Some(current) if current.runtime.process_identity.is_none() => {
            // A concurrent clear is settlement only when it also released
            // membership. Old NULL-PID borrowers stay fenced.
            Ok(state.state_store.thread_workspace_binding(thread_id)?.is_none())
        }
        Some(_) => {
            tracing::warn!(
                thread_id,
                "shutdown retains exact process authority for unsettled workspace recovery or a changed attachment"
            );
            Ok(false)
        }
        None => Ok(true),
    }
}

/// Record one attached process proved dead whose rows the latched drain
/// leaves to recovery: its settlement, and the resume re-arm unless its
/// thread is known terminal (recovery ignores entries for terminal rows).
fn defer_attached(deferred: &mut DeferredRows, item: &DrainAttached) {
    deferred
        .attached
        .insert(item.thread_id.clone(), item.identity.clone());
    if !item
        .status
        .as_deref()
        .is_some_and(ryeos_app::state_store::is_terminal_status)
    {
        deferred
            .resume_reset
            .insert(item.thread_id.clone(), item.identity.clone());
    }
}

/// The reader launch admission judges declared node guarantees with. Each
/// read re-reads the recorded report and verifies its digest and this
/// node's signature over it, then refuses it unless it still describes the
/// running node:
///
/// - The host generation (boot id and container enclosure) is observed
///   again at each read, so a report from before a host restart is refused.
/// - The policy generation is the one this daemon composed at startup. It
///   cannot change while the daemon runs, so comparing it refuses only a
///   report another daemon generation wrote over this one's.
/// - `credential:private_home` is re-observed at each read with the Lillux
///   private-directory walk from the app root to the credential home root;
///   a root that is no longer private to the node's account is reported
///   missing with the walk's reason, whatever the startup report said.
///
/// Every other fact is the snapshot the node took when it started and is
/// not observed again: durable state and the declared host lifetime, time,
/// the provider environment's names, the process-scope and isolation
/// grades, residency and the remaining host facts. A change to any of them
/// is seen only after the node restarts and records a new report.
///
/// A missing report, one whose digest or signature does not verify, and
/// one that is stale for this host or policy generation are refusals:
/// nothing this daemon does while it runs makes them stand. Only a failure
/// to read the report file, the node identity or the host generation is an
/// unreadable report, which a later attempt may read.
fn node_capability_report_reader(
    app_root: std::path::PathBuf,
    policy_generation: String,
) -> ryeos_app::node_requirement_admission::NodeCapabilityReportReader {
    ryeos_app::node_requirement_admission::NodeCapabilityReportReader::new(move || {
        current_node_capability_view(&app_root, &policy_generation)
    })
}

fn current_node_capability_view(
    app_root: &std::path::Path,
    policy_generation: &str,
) -> Result<lillux::process_control::HostCapabilities> {
    use ryeos_app::node_requirement_admission::{NodeReportUnreadable, NodeRequirementRefused};
    let signed = ryeos_node::capability_report::SignedNodeCapabilityReport::read_verified(app_root)?
        .ok_or_else(|| {
            NodeRequirementRefused(
                "launch refused: no verified node capability report is recorded".to_owned(),
            )
        })?;
    let staleness = signed.staleness(Some(policy_generation)).map_err(|error| {
        NodeReportUnreadable(format!("observe this host's generation: {error:#}"))
    })?;
    if let Some(reason) = staleness {
        anyhow::bail!(NodeRequirementRefused(format!(
            "launch refused: the recorded node capability report is stale: {reason}"
        )));
    }
    let mut view = lillux::process_control::HostCapabilities {
        provided: signed.report.provided,
        missing: signed.report.missing,
    };
    let credential_home = ryeos_node::capability_report::CREDENTIAL_PRIVATE_HOME;
    if view.provided.contains_key(credential_home) {
        let state_dir = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).state();
        let observed = lillux::process_control::PrivateDirectoryFacts::observe(
            app_root,
            &ryeos_app::private_artifact_home::root(&state_dir),
        );
        if let Err(reason) = lillux::process_control::decide_private_directory(&observed) {
            view.provided.remove(credential_home);
            view.refuse(
                credential_home,
                format!("credential home root is no longer private at admission: {reason}"),
            );
        }
    }
    Ok(view)
}

/// Prove one exact process group dead from its identity alone, killing it
/// when it is still alive. Reads no database.
fn kill_and_prove_group_dead(identity: &process::ExecutionProcessIdentity) -> bool {
    match process::execution_group_liveness(identity) {
        process::IdentityLiveness::DeadOrStale => true,
        process::IdentityLiveness::Alive => {
            let killed = process::kill_by_action(identity, process::ShutdownAction::Hard);
            killed.success
                && process::execution_group_liveness(identity)
                    == process::IdentityLiveness::DeadOrStale
        }
        process::IdentityLiveness::Unavailable => false,
    }
}

async fn drain_running_threads(state: &AppState, latch: &DrainLatch) -> DrainOutcome {
    let mut mode = latch.mode();
    let mut deferred = DeferredRows::default();
    // Serialize shutdown against every future UDS/internal attachment. An
    // attach that committed first appears below; one that arrives later is
    // rejected and its spawn owner aborts the exact process.
    if let Err(error) = state.state_store.close_process_attachment_admission() {
        tracing::error!(error = %error, "failed to close process attachment admission");
        return DrainOutcome::Unproved;
    }

    // Snapshot every root attachment before retiring independent hosted
    // workers. Retiring a worker can make its root controller exit and
    // compare-clear itself immediately; the shutdown coordinator must still
    // retain the exact old identity so it can prove absence and re-arm that
    // root's consumed restart budget. A latched stop reads the in-memory
    // attachment records, which carry the same committed identities.
    let (attached, mut attached_snapshot_clean) = match latch.refresh(&mut mode) {
        DrainMode::Latched => latched_attached_snapshot(state),
        DrainMode::Normal => match normal_attached_snapshot(state) {
            Ok((_, _, true)) if latch.refresh(&mut mode) == DrainMode::Latched => {
                latched_attached_snapshot(state)
            }
            Ok((attached, clean, _)) => (attached, clean),
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "failed to snapshot attached threads during shutdown");
                return DrainOutcome::Unproved;
            }
        },
    };

    // Close the generic persistent-session pool first. Pooled request workers
    // have no durable per-process row, so the pool itself is their only exact
    // shutdown owner. Exclusive workers are reaped through the same boundary,
    // then their durable identities are fenced below.
    let external_candidate_imports_drained = drain_external_candidate_imports(state).await;
    let pool_mode = latch.refresh(&mut mode);
    let persistent_session_pool_drained =
        drain_persistent_session_pool(state, pool_mode, &mut deferred).await;

    // Exclusive persistent-session workers are independent process groups,
    // not descendants represented by `thread_runtime`. Durably fence those
    // exact worker epochs before draining their root controllers. This
    // releases the workspace/profile lease only after process death is proved
    // and leaves the session in the ordinary restart-recovery state.
    let persistent_session_workers_drained =
        drain_persistent_session_workers(state, latch, &mut mode, &mut deferred).await;

    let drain_deadline = Instant::now()
        .checked_add(Duration::from_secs(
            process::MAX_GRACEFUL_SHUTDOWN_GRACE_SECS,
        ))
        .unwrap_or_else(Instant::now);
    let in_process_handlers = match state.state_store.active_in_process_handler_controls() {
        Ok(handlers) => handlers,
        Err(error) => {
            tracing::error!(
                error = %error,
                "failed to list in-process handlers during shutdown"
            );
            return DrainOutcome::Unproved;
        }
    };
    if !attached.is_empty() {
        tracing::info!(
            count = attached.len(),
            "draining attached threads concurrently"
        );
    }

    const HARD_KILL_PROOF_RESERVE: Duration = Duration::from_millis(250);
    let mut pending = Vec::with_capacity(attached.len());
    for item in attached {
        let kill_identity = item.identity.clone();
        let action = item.action;
        let kill_task = tokio::task::spawn_blocking(move || {
            let bounded_action = match action {
                process::ShutdownAction::Hard => process::ShutdownAction::Hard,
                process::ShutdownAction::Graceful(grace) => process::ShutdownAction::Graceful(
                    grace.min(
                        drain_deadline
                            .saturating_duration_since(Instant::now())
                            .saturating_sub(HARD_KILL_PROOF_RESERVE),
                    ),
                ),
            };
            process::kill_by_action(&kill_identity, bounded_action)
        });
        pending.push((item, kill_task));
    }

    for (item, kill_task) in pending {
        let thread_id = item.thread_id.as_str();
        let result = match kill_task.await {
            Ok(result) => result,
            Err(error) => {
                attached_snapshot_clean = false;
                tracing::warn!(thread_id, error = %error, "shutdown process-kill worker failed");
                continue;
            }
        };
        if !result.success {
            attached_snapshot_clean = false;
            tracing::warn!(
                thread_id,
                method = result.method,
                "failed to prove attached process termination during shutdown"
            );
            continue;
        }
        // The group is proved dead. A latched stop writes no settlement
        // row; active-thread recovery reconciles it from the durable row.
        let settled = match latch.refresh(&mut mode) {
            DrainMode::Latched => None,
            DrainMode::Normal => Some(settle_attached_normal(state, thread_id, &item.identity)),
        };
        let detached = match settled {
            None => {
                defer_attached(&mut deferred, &item);
                continue;
            }
            Some(Ok(true)) => true,
            Some(Ok(false)) => false,
            Some(Err(_)) if latch.refresh(&mut mode) == DrainMode::Latched => {
                defer_attached(&mut deferred, &item);
                continue;
            }
            Some(Err(error)) => {
                tracing::warn!(
                    thread_id,
                    error = %error,
                    "failed to clear shutdown process identity"
                );
                false
            }
        };
        if !detached {
            attached_snapshot_clean = false;
            continue;
        }
        if !item
            .status
            .as_deref()
            .is_some_and(ryeos_app::state_store::is_terminal_status)
        {
            let reset_result = match latch.refresh(&mut mode) {
                DrainMode::Latched => None,
                DrainMode::Normal => Some(state.state_store.reset_resume_attempts(thread_id)),
            };
            match reset_result {
                Some(Ok(_)) => {}
                None => {
                    deferred
                        .resume_reset
                        .insert(item.thread_id.clone(), item.identity.clone());
                }
                Some(Err(_)) if latch.refresh(&mut mode) == DrainMode::Latched => {
                    deferred
                        .resume_reset
                        .insert(item.thread_id.clone(), item.identity.clone());
                }
                Some(Err(error)) => {
                    attached_snapshot_clean = false;
                    tracing::warn!(
                        thread_id,
                        error = %error,
                        "failed to re-arm resume budget during drain"
                    );
                }
            }
        }
    }

    let mut in_process_completion_clean = true;
    for (thread_id, control) in in_process_handlers {
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        let completion = tokio::time::timeout(remaining, control.wait_completed()).await;
        match completion {
            Ok(
                ryeos_app::state_store::InProcessHandlerCompletion::TerminalConfirmed
                | ryeos_app::state_store::InProcessHandlerCompletion::BirthAborted,
            ) => {}
            Ok(ryeos_app::state_store::InProcessHandlerCompletion::OwnerLostUnsettled)
                if latch.refresh(&mut mode) == DrainMode::Latched =>
            {
                // The handler is gone; in-process reservation recovery
                // reconciles its reservation at the next start.
                deferred.in_process_owner_lost.push(thread_id);
            }
            Ok(ryeos_app::state_store::InProcessHandlerCompletion::OwnerLostUnsettled) => {
                in_process_completion_clean = false;
                tracing::error!(
                    thread_id,
                    "in-process handler owner exited without a confirmed terminal state"
                );
            }
            Err(_) => {
                in_process_completion_clean = false;
                tracing::error!(
                    thread_id,
                    "shutdown drain timed out waiting for in-process handler owner"
                );
            }
        }
    }

    // Normal: no attachment may remain. Latched: every remaining attachment
    // must be one this drain proved dead and deferred.
    let attached_drained = match state.state_store.list_attached_thread_ids() {
        Ok(remaining) => {
            let unproved = remaining
                .iter()
                .filter(|thread_id| match mode {
                    DrainMode::Normal => true,
                    DrainMode::Latched => !deferred.attached.contains_key(*thread_id),
                })
                .collect::<Vec<_>>();
            if unproved.is_empty() {
                in_process_completion_clean
            } else {
                tracing::error!(
                    remaining = ?unproved,
                    "shutdown drain exhausted with attached identities still present"
                );
                false
            }
        }
        Err(error) => {
            tracing::error!(error = %error, "failed final shutdown drain audit");
            false
        }
    };
    // Proved-dead attachments whose rows recovery reconciles leave the
    // in-memory records, so the live Store close sees no live attachment.
    if attached_drained && mode == DrainMode::Latched {
        state
            .state_store
            .forget_proven_dead_attachments(&deferred.attached);
    }
    let in_process_drained = match state.state_store.active_in_process_handler_controls() {
        Ok(remaining) if !remaining.is_empty() => {
            let thread_ids = remaining
                .into_iter()
                .map(|(thread_id, _)| thread_id)
                .collect::<Vec<_>>();
            tracing::error!(
                remaining = ?thread_ids,
                "shutdown drain exhausted with in-process handlers still active"
            );
            false
        }
        Ok(_) => true,
        Err(error) => {
            tracing::error!(
                error = %error,
                "failed final in-process-handler shutdown audit"
            );
            false
        }
    };
    // Never settle a durable reservation while its volatile owner is still
    // active. A timed-out drain is an unclean shutdown; startup reconciliation
    // owns the remaining reservation after this daemon exits. A latched stop
    // skips the audit's SQL and leaves it to that same startup recovery.
    let in_process_authoritative_clean = if !in_process_drained {
        false
    } else if latch.refresh(&mut mode) == DrainMode::Latched {
        deferred.in_process_audit_skipped = true;
        true
    } else {
        match audit_ownerless_in_process_reservations(state) {
            Ok(true) => true,
            Ok(false) | Err(_) if latch.refresh(&mut mode) == DrainMode::Latched => {
                deferred.in_process_audit_skipped = true;
                true
            }
            Ok(false) => false,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "failed to list live in-process reservations for authoritative shutdown audit"
                );
                false
            }
        }
    };
    let clean = external_candidate_imports_drained
        && persistent_session_pool_drained
        && persistent_session_workers_drained
        && attached_snapshot_clean
        && attached_drained
        && in_process_drained
        && in_process_authoritative_clean;
    match (clean, mode) {
        (false, _) => DrainOutcome::Unproved,
        (true, DrainMode::Normal) => DrainOutcome::Clean,
        (true, DrainMode::Latched) => DrainOutcome::ProcessCleanRowsDeferred(deferred),
    }
}

async fn drain_external_candidate_imports(state: &AppState) -> bool {
    let pool = Arc::clone(&state.external_candidate_imports);
    match tokio::task::spawn_blocking(move || {
        pool.shutdown_and_wait(Duration::from_secs(
            process::MAX_GRACEFUL_SHUTDOWN_GRACE_SECS,
        ))
    })
    .await
    {
        Ok(Ok(drained)) => {
            if drained != 0 {
                tracing::info!(
                    drained,
                    "drained external candidate imports before daemon exit"
                );
            }
            true
        }
        Ok(Err(error)) => {
            tracing::error!(
                error = %error,
                "external candidate import shutdown remains unproved"
            );
            false
        }
        Err(error) => {
            tracing::error!(
                error = %error,
                "external candidate import shutdown task panicked"
            );
            false
        }
    }
}

async fn drain_persistent_session_pool(
    state: &AppState,
    mode: DrainMode,
    deferred: &mut DeferredRows,
) -> bool {
    let pool = Arc::clone(&state.persistent_sessions);
    let retirement = match mode {
        DrainMode::Normal => ryeos_app::persistent_session::RetirementMode::Normal,
        DrainMode::Latched => ryeos_app::persistent_session::RetirementMode::Latched,
    };
    match tokio::task::spawn_blocking(move || {
        pool.shutdown_and_reap_all_in(
            Duration::from_secs(process::MAX_GRACEFUL_SHUTDOWN_GRACE_SECS),
            retirement,
        )
    })
    .await
    {
        Ok(Ok(reap)) => {
            let ryeos_app::persistent_session::PersistentSessionShutdownReap {
                reaped,
                durable_rows_deferred,
            } = reap;
            if reaped != 0 {
                tracing::info!(reaped, "reaped persistent-session pool before daemon exit");
            }
            deferred.pooled_owners = deferred.pooled_owners.saturating_add(durable_rows_deferred);
            true
        }
        Ok(Err(error)) => {
            tracing::error!(
                error = %error,
                "persistent-session pool shutdown cleanup remains unproved"
            );
            false
        }
        Err(error) => {
            tracing::error!(
                error = %error,
                "persistent-session pool shutdown task panicked"
            );
            false
        }
    }
}

/// Latched worker drain: kill each exact live worker group recorded in
/// memory and prove it dead without SQL. The fence and the resource-owner
/// proof are left to dedicated worker recovery.
async fn drain_persistent_session_workers_latched(
    state: &AppState,
    deferred: &mut DeferredRows,
) -> bool {
    let workers = state.state_store.live_worker_snapshot();
    if !workers.is_empty() {
        tracing::info!(
            count = workers.len(),
            "proving persistent-session worker death without service SQL"
        );
    }
    let mut pending = Vec::with_capacity(workers.len());
    for worker in workers {
        let identity = worker.process_identity.clone();
        let task = tokio::task::spawn_blocking(move || kill_and_prove_group_dead(&identity));
        pending.push((worker.worker_instance_id, task));
    }
    let mut clean = true;
    for (worker_instance_id, task) in pending {
        match task.await {
            Ok(true) => deferred.workers.push(worker_instance_id),
            Ok(false) => {
                clean = false;
                tracing::error!(
                    worker_instance_id = %worker_instance_id,
                    "persistent-session worker death remains unproved in a latched stop"
                );
            }
            Err(error) => {
                clean = false;
                tracing::error!(
                    worker_instance_id = %worker_instance_id,
                    error = %error,
                    "persistent-session worker death proof panicked"
                );
            }
        }
    }
    // Every worker still recorded live must be one proved dead here.
    let remaining = state
        .state_store
        .live_worker_snapshot()
        .into_iter()
        .map(|worker| worker.worker_instance_id)
        .filter(|worker_instance_id| !deferred.workers.contains(worker_instance_id))
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        tracing::error!(
            remaining = ?remaining,
            "latched stop left persistent-session workers without a death proof"
        );
        return false;
    }
    clean
}

async fn drain_persistent_session_workers(
    state: &AppState,
    latch: &DrainLatch,
    mode: &mut DrainMode,
    deferred: &mut DeferredRows,
) -> bool {
    if latch.refresh(mode) == DrainMode::Latched {
        return drain_persistent_session_workers_latched(state, deferred).await;
    }
    let workers = match state.state_store.live_worker_processes() {
        Ok(workers) => workers,
        Err(_) if latch.refresh(mode) == DrainMode::Latched => {
            return drain_persistent_session_workers_latched(state, deferred).await;
        }
        Err(error) => {
            tracing::error!(
                error = %error,
                "failed to list persistent-session workers during shutdown"
            );
            return false;
        }
    };
    if !workers.is_empty() {
        tracing::info!(
            count = workers.len(),
            "draining persistent-session workers concurrently"
        );
    }

    let mut pending = Vec::with_capacity(workers.len());
    for worker in workers {
        let worker_state = state.clone();
        let task = tokio::task::spawn_blocking(move || {
            let cleanup = ryeos_app::dedicated_session_service::retire_worker_process(
                &worker_state,
                &worker.placement_thread_id,
                &worker,
            );
            (worker, cleanup)
        });
        pending.push(task);
    }

    let mut clean = true;
    for task in pending {
        let (worker, cleanup) = match task.await {
            Ok(result) => result,
            Err(error) => {
                clean = false;
                tracing::error!(
                    error = %error,
                    "persistent-session shutdown worker panicked"
                );
                continue;
            }
        };
        let cleanup_state = match cleanup {
            Ok(cleanup_state) => cleanup_state,
            // Retirement writes service SQL; once the latch is set its
            // refusal says nothing about the process, so prove death from
            // the exact identity and leave the rows to worker recovery.
            Err(_) if latch.refresh(mode) == DrainMode::Latched => {
                let identity = worker.process_identity.clone();
                let proved = tokio::task::spawn_blocking(move || kill_and_prove_group_dead(&identity))
                    .await
                    .unwrap_or(false);
                if proved {
                    deferred.workers.push(worker.worker_instance_id.clone());
                } else {
                    clean = false;
                    tracing::error!(
                        worker_instance_id = %worker.worker_instance_id,
                        "persistent-session worker death remains unproved after a latched retirement"
                    );
                }
                continue;
            }
            Err(error) => {
                clean = false;
                tracing::error!(
                    worker_instance_id = %worker.worker_instance_id,
                    placement_thread_id = %worker.placement_thread_id,
                    error = %error,
                    "persistent-session worker retirement failed during shutdown"
                );
                continue;
            }
        };

        let fenced = match latch.refresh(mode) {
            DrainMode::Latched => None,
            DrainMode::Normal => Some(state.state_store.fence_abandoned_worker_process(
                &worker.worker_instance_id,
                &worker.placement_thread_id,
                worker.boot_epoch,
                cleanup_state,
            )),
        };
        match fenced {
            Some(Ok(())) => {}
            // A reaped worker is proved dead; the fence row is left to
            // worker recovery, which fences every prior-generation epoch.
            None if cleanup_state == "reaped" => {
                deferred.workers.push(worker.worker_instance_id.clone());
                continue;
            }
            Some(Err(_))
                if cleanup_state == "reaped" && latch.refresh(mode) == DrainMode::Latched =>
            {
                deferred.workers.push(worker.worker_instance_id.clone());
                continue;
            }
            None => {
                clean = false;
                tracing::error!(
                    worker_instance_id = %worker.worker_instance_id,
                    cleanup_state,
                    "persistent-session worker death remains unproved in a latched stop"
                );
                continue;
            }
            Some(Err(error)) => {
                // A concurrent worker-I/O failure may have fenced the exact epoch
                // after the shutdown snapshot. Accept only the matching durable
                // dead record; every other CAS loss remains an unclean shutdown.
                let already_fenced = state
                    .state_store
                    .worker_process(&worker.worker_instance_id)
                    .ok()
                    .flatten()
                    .is_some_and(|current| {
                        current.placement_thread_id == worker.placement_thread_id
                            && current.boot_epoch == worker.boot_epoch
                            && current.state == ryeos_app::runtime_db::WorkerProcessState::Dead
                            && current.cleanup_state == cleanup_state
                    });
                if !already_fenced {
                    clean = false;
                    tracing::error!(
                        worker_instance_id = %worker.worker_instance_id,
                        placement_thread_id = %worker.placement_thread_id,
                        cleanup_state,
                        error = %error,
                        "failed to persist persistent-session shutdown fence"
                    );
                    continue;
                }
            }
        }
        if cleanup_state != "reaped" {
            clean = false;
            tracing::error!(
                worker_instance_id = %worker.worker_instance_id,
                placement_thread_id = %worker.placement_thread_id,
                cleanup_state,
                "persistent-session worker death remains unproved after shutdown drain"
            );
        } else {
            tracing::info!(
                worker_instance_id = %worker.worker_instance_id,
                placement_thread_id = %worker.placement_thread_id,
                boot_epoch = worker.boot_epoch,
                "persistent-session worker epoch durably fenced for restart"
            );
        }
    }

    // Normal: the durable audit must find no live worker. Once latched, the
    // in-memory records must hold only workers proved dead in this drain.
    if latch.refresh(mode) == DrainMode::Latched {
        let remaining = state
            .state_store
            .live_worker_snapshot()
            .into_iter()
            .map(|worker| worker.worker_instance_id)
            .filter(|worker_instance_id| !deferred.workers.contains(worker_instance_id))
            .collect::<Vec<_>>();
        if !remaining.is_empty() {
            tracing::error!(
                remaining = ?remaining,
                "latched stop left persistent-session workers without a death proof"
            );
            return false;
        }
        return clean;
    }
    match state.state_store.live_worker_processes() {
        Ok(remaining) if remaining.is_empty() => clean,
        Ok(remaining) => {
            let worker_ids = remaining
                .into_iter()
                .map(|worker| worker.worker_instance_id)
                .collect::<Vec<_>>();
            tracing::error!(
                remaining = ?worker_ids,
                "shutdown drain exhausted with persistent-session workers still live or unproved"
            );
            false
        }
        Err(error) => {
            tracing::error!(
                error = %error,
                "failed final persistent-session worker shutdown audit"
            );
            false
        }
    }
}

fn audit_ownerless_in_process_reservations(state: &AppState) -> anyhow::Result<bool> {
    let active = state
        .state_store
        .active_in_process_handler_controls()
        .context("list active in-process owners before authoritative shutdown audit")?;
    if !active.is_empty() {
        anyhow::bail!("authoritative shutdown audit requires an empty in-process owner registry");
    }

    let mut clean = true;
    let mut after_thread_id = None;
    let mut audited = 0usize;
    loop {
        let reservations = state.state_store.in_process_handler_reservations_after(
            after_thread_id.as_deref(),
            ryeos_app::runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
        )?;
        if reservations.is_empty() {
            break;
        }
        audited = audited
            .checked_add(reservations.len())
            .context("in-process reservation audit count overflow")?;
        if audited > ryeos_app::runtime_db::MAX_IN_PROCESS_HANDLER_RESERVATIONS {
            anyhow::bail!(
                "in-process handler reservations exceed the current-schema limit of {}",
                ryeos_app::runtime_db::MAX_IN_PROCESS_HANDLER_RESERVATIONS
            );
        }
        for reservation in reservations {
            after_thread_id = Some(reservation.thread_id.clone());
            let thread_id = reservation.thread_id;
            // An in-process handler may be an inline child in its parent's
            // chain, so the snapshot is located through the handler's own
            // runtime row rather than as a chain root.
            match state
                .state_store
                .get_authoritative_in_process_handler_snapshot(&thread_id)
            {
                Ok(Some(snapshot))
                    if ryeos_app::state_store::is_terminal_status(snapshot.status.as_str()) =>
                {
                    let postcommit_repaired =
                        match state.threads.repair_recorded_service_terminal_postcommit(
                            &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                                thread_id: thread_id.clone(),
                                status: snapshot.status.as_str().to_string(),
                                outcome_code: snapshot.outcome_code.clone(),
                                result: snapshot.result.clone(),
                                error: snapshot.error.clone(),
                                metadata: None,
                                artifacts: Vec::new(),
                                final_cost: None,
                                summary_json: None,
                            },
                        ) {
                            Ok(()) => true,
                            Err(error) => {
                                clean = false;
                                tracing::error!(
                                    thread_id,
                                    reservation_phase = ?reservation.phase,
                                    error = %error,
                                    "failed terminal postcommit repair during shutdown audit"
                                );
                                false
                            }
                        };
                    let settled = postcommit_repaired
                        && match state
                            .state_store
                            .settle_ownerless_in_process_handler_reservation(&thread_id)
                        {
                            Ok(true) => true,
                            Ok(false) => {
                                clean = false;
                                tracing::error!(
                                    thread_id,
                                    reservation_phase = ?reservation.phase,
                                    "terminal in-process reservation disappeared before shutdown settlement"
                                );
                                false
                            }
                            Err(error) => {
                                clean = false;
                                tracing::error!(
                                    thread_id,
                                    reservation_phase = ?reservation.phase,
                                    error = %error,
                                    "failed to settle terminal in-process reservation during shutdown audit"
                                );
                                false
                            }
                        };
                    if settled {
                        let deletion = state
                            .state_store
                            .delete_ownerless_terminal_in_process_handler_reservation(&thread_id);
                        match deletion {
                            Ok(true) => {}
                            Ok(false) => {
                                clean = false;
                                tracing::error!(
                                    thread_id,
                                    reservation_phase = ?reservation.phase,
                                    "settled terminal in-process reservation disappeared before shutdown cleanup"
                                );
                            }
                            Err(error) => {
                                clean = false;
                                tracing::error!(
                                    thread_id,
                                    reservation_phase = ?reservation.phase,
                                    error = %error,
                                    "failed to retire terminal in-process reservation during shutdown audit"
                                );
                            }
                        }
                    }
                }
                Ok(Some(snapshot)) => {
                    clean = false;
                    tracing::error!(
                        thread_id,
                        reservation_phase = ?reservation.phase,
                        authoritative_status = %snapshot.status.as_str(),
                        "shutdown drain left a live in-process reservation nonterminal in authoritative state"
                    );
                }
                Ok(None) => {
                    clean = false;
                    tracing::error!(
                        thread_id,
                        reservation_phase = ?reservation.phase,
                        "shutdown drain found a live in-process reservation without an authoritative thread"
                    );
                }
                Err(error) => {
                    clean = false;
                    tracing::error!(
                        thread_id,
                        reservation_phase = ?reservation.phase,
                        error = %error,
                        "failed authoritative live in-process-reservation shutdown audit"
                    );
                }
            }
        }
    }
    Ok(clean)
}

async fn shutdown_signal() {
    if ryeosd::shutdown_requested() {
        return;
    }

    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let lifecycle_shutdown = async {
        match ryeosd::subscribe_shutdown() {
            Some(mut rx) => {
                let _ = rx.recv().await;
            }
            None => {
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
        _ = lifecycle_shutdown => {}
    }
    ryeosd::request_shutdown();
}

fn ensure_runtime_paths(config: &Config) -> Result<()> {
    if let Some(parent) = config.db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create db parent {}", parent.display()))?;
    }
    if let Some(parent) = config.uds_path.parent() {
        // Init/repair creates the private default `.ai/state/daemon` root.
        // An explicit socket path may intentionally name another existing
        // namespace, but it never authorizes ryeosd to create or chmod that
        // arbitrary parent (notably `/tmp`). The socket inode itself is made
        // owner-only immediately after binding.
        lillux::PinnedDirectory::open(parent)?
            .with_context(|| format!("configured uds parent is absent: {}", parent.display()))?
            .ensure_path_binding()
            .with_context(|| format!("configured uds parent changed: {}", parent.display()))?;
    }
    Ok(())
}

/// Establish ingress authority from the configured operator key loaded by
/// stopped-node bootstrap. A principal string alone is not authentication;
/// keep this proof here, never infer it in the generic service executor.
fn standalone_operator_handler_context(
    operator: &NodeIdentity,
) -> ryeos_app::handler_context::HandlerContext {
    ryeos_app::handler_context::HandlerContext::new_with_authority(
        operator.principal_id(),
        vec![], // Existing standalone filesystem authority does not mint live scopes.
        true,
        Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

/// Finish a standalone native host. A refusal keeps its owners inside the
/// returned error for the rest of this process.
fn finish_standalone_native(host: ryeos_app::standalone_native::StandaloneNativeHost) -> Result<()> {
    match host.finish() {
        Ok(_facts) => Ok(()),
        Err((Some(host), error)) => Err(ryeos_state::operational::CarriedOwners::carry(
            error.context("standalone native host finish refused"),
            host,
        )),
        Err((None, error)) => Err(error.context("standalone native host finish refused")),
    }
}

/// Close a Store opened on a standalone native host into the host's census
/// and finish the host, after a setup step refused once the Store was open.
/// The setup refusal stays the reported cause; a settlement refusal rides
/// with it and keeps any owners it carries.
fn settle_standalone_setup_failure(
    host: Option<ryeos_app::standalone_native::StandaloneNativeHost>,
    access: ryeos_app::service_registry::StandaloneStateAccess,
    state_store: Arc<ryeos_app::state_store::StateStore>,
    error: anyhow::Error,
) -> anyhow::Error {
    let Some(host) = host else {
        return error;
    };
    let closed = match access {
        ryeos_app::service_registry::StandaloneStateAccess::ProjectionRebuild
        | ryeos_app::service_registry::StandaloneStateAccess::ReadOnlyExisting => {
            host.close_control_store(state_store)
        }
        ryeos_app::service_registry::StandaloneStateAccess::ReadWrite => host.close_store(state_store),
    };
    let finished = finish_standalone_native(host);
    match (closed, finished) {
        (Ok(()), Ok(())) => error,
        (Err(settle), Ok(())) | (Ok(()), Err(settle)) => {
            let message = format!("standalone native Store did not settle: {settle:#}");
            ryeos_state::operational::CarriedOwners::carry(error.context(message), settle)
        }
        (Err(closed), Err(finished)) => {
            let message = format!(
                "standalone native Store did not settle: {closed:#}; host finish: {finished:#}"
            );
            let error = ryeos_state::operational::CarriedOwners::carry(error.context(message), closed);
            ryeos_state::operational::CarriedOwners::carry(error, finished)
        }
    }
}

/// Run a service in standalone mode (daemon is not running).
///
/// Performs bootstrap: load config, build engine + trust store,
/// load full node-config, build service registry, acquire state lock, dispatch via shared executor.
async fn run_service_standalone(
    config: &config::Config,
    service_ref: &str,
    params_json: Option<&str>,
    provider_environment: &ryeos_node::provider_env::StartupProviderEnvironment,
) -> Result<()> {
    use ryeos_executor::executor::{ExecutionContext, ExecutionMode};

    // Verify initialization
    ryeos_node::require_initialized(&config.app_root)?;

    // Acquire the state lock immediately after init verification, BEFORE
    // any expensive bootstrap work. Otherwise standalone mode would
    // perform identity/engine/node-config loads while a competing
    // daemon held the lock, only to fail late.
    let standalone_state_lock = Arc::new(
        state_lock::StateLock::acquire(&state_lock::default_lock_path(&config.app_root))
            .context("failed to acquire state lock — is the daemon running?")?,
    );
    bootstrap::verify_initialized(config)?;

    let identity = NodeIdentity::load(&config.node_signing_key_path)?;
    let repaired_bundles = ryeos_app::bundle_transaction::reconcile_all_bundle_transactions(
        &config.app_root,
        identity.signing_key(),
    )
    .context("reconcile interrupted bundle transactions")?;
    if !repaired_bundles.is_empty() {
        bootstrap::verify_initialized(config)?;
    }

    // Two-phase node-config bootstrap without daemon callback-socket authority:
    // standalone mode does not bind the configured UDS listener.
    let (engine, node_config_snapshot, node_policy_snapshot, isolation, capability_report) =
        bootstrap::load_node_config_two_phase_standalone(config, provider_environment)?;
    let external_artifacts = engine.with_checked_bundle_generation(|_| {
        ryeos_app::external_artifacts::resolve_external_execution_artifacts(
            &engine.bundle_roots,
            &engine.node_trust_store,
            &ryeos_app::private_artifact_home::root(&config.runtime_state_dir()),
        )
    })?;

    let params: serde_json::Value = match params_json {
        Some(json_str) => {
            serde_json::from_str(json_str).with_context(|| "parse --params as JSON")?
        }
        None => serde_json::json!({}),
    };
    // Standalone services are invoked by the configured local operator. The
    // node identity still signs node-owned state below, but it is the server's
    // attesting identity, not the caller principal. Keeping those authorities
    // distinct lets the same local-operator handler run live or standalone.
    let standalone_operator = NodeIdentity::load(&config.operator_signing_key_path)
        .context("load configured local operator identity for standalone service")?;
    let standalone_principal = standalone_operator.principal_id();
    let standalone_handler_context = standalone_operator_handler_context(&standalone_operator);
    let standalone_plan_ctx = ryeos_engine::contracts::PlanContext {
        requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: standalone_principal.clone(),
                scopes: vec![],
            },
        ),
        project_context: ryeos_engine::contracts::ProjectContext::None,
        subject_resolution_authority:
            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        current_site_id: "site:local".into(),
        origin_site_id: "site:local".into(),
        execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
        scheduled_fire: None,
        validate_only: false,
    };
    let service_canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(service_ref)
        .with_context(|| format!("invalid standalone service ref {service_ref}"))?;
    // Preserve registry admission when dispatching the already verified item
    // below. Discover this from the signed kind contract, never a kind name.
    anyhow::ensure!(
        engine
            .kinds
            .kinds_for_in_process_registry(
                ryeos_engine::kind_registry::InProcessRegistryKind::Services,
            )
            .any(|(kind, _)| kind == service_canonical.kind),
        "kind `{}` is not admitted to the Services in-process registry",
        service_canonical.kind,
    );
    let service_resolved = engine
        .resolve(&standalone_plan_ctx, &service_canonical)
        .with_context(|| format!("resolve standalone service {service_ref}"))?;
    let service_verified = engine
        .verify(&standalone_plan_ctx, service_resolved)
        .with_context(|| format!("verify standalone service {service_ref}"))?;
    let standalone_state_access = ryeos_app::service_registry::extract_standalone_state_access(
        &service_verified.resolved.metadata.extra,
    )?;
    // The installed composition decides whether this stopped node has a
    // scheduler store to consult; a node without it never opens or creates one.
    // A build that cannot run a selected scheduler refuses.
    let standalone_scheduler_selected = ryeos_api::scheduler_selection::resolve_scheduler_selection(
        &engine,
        cfg!(feature = "scheduling"),
    )
    .context("derive scheduler selection from the installed composition")?;
    #[cfg(not(feature = "scheduling"))]
    let _ = standalone_scheduler_selected;

    let kind_profiles = Arc::new(kind_profiles::KindProfileRegistry::build(Some(
        &engine.kinds,
    )));
    let services = Arc::new(build_service_registry()?);

    let runtime_state_dir = config.runtime_state_dir();
    let runtime_db_path = config.db_path.clone();
    let signer = Arc::new(state_store::NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
    let head_trust = Arc::new(head_trust);
    // A read/write standalone Store opens through the native gate on an
    // in-process host funded by the signed standalone class, and closes into
    // that host's census before the host finishes.
    // Projection rebuild and verification run in the stopped-node maintenance
    // shape: this invocation holds the runtime namespace lock for the host's
    // whole life.
    // Every fallible step that needs no Store runs before the native host
    // is issued, so none of them can strand an open Store or its host.
    let standalone_command_registry = Arc::new(
        ryeos_runtime::CommandRegistry::from_records(
            &node_config_snapshot.commands,
            &node_policy_snapshot
                .require::<ryeos_node_runtime::node_policy::sections::command_registration::CommandRegistrationAuthority>()?
                .runtime_policy(),
        )
        .context("failed to build command registry from node-config records")?,
    );
    let publication_policy = node_policy_snapshot
        .require::<ryeos_node_runtime::node_policy::sections::bundle_publication::BundlePublicationPolicy>()?
        .clone();
    let publisher = configured_bundle_publisher(&publication_policy)?;
    let standalone_vault = Arc::new(
        ryeos_app::vault::SealedEnvelopeVault::load(&config.app_root)
            .context("load sealed-envelope vault — did `ryeos init` run?")?,
    );
    let standalone_ignore_matcher = Arc::new(
        node_policy_snapshot
            .require::<ryeos_node_runtime::node_policy::sections::ingest_ignore::CompiledIngestIgnorePolicy>()?
            .matcher
            .clone(),
    );
    let standalone_namespace = match standalone_state_access {
        ryeos_app::service_registry::StandaloneStateAccess::ProjectionRebuild
        | ryeos_app::service_registry::StandaloneStateAccess::ReadOnlyExisting => {
            let runtime = Arc::new(
                lillux::PinnedDirectory::open(&runtime_state_dir)?
                    .with_context(|| format!("runtime state is absent: {}", runtime_state_dir.display()))?,
            );
            let namespace = runtime
                .lock_exclusive()
                .context("lock the runtime namespace for projection rebuild")?;
            Some((runtime, namespace))
        }
        ryeos_app::service_registry::StandaloneStateAccess::ReadWrite => None,
    };
    // Bootstrap admitted this service on the planned residency; the native
    // host must meet the same floor on the residency its region establishes.
    let admit_residency = |proof: &lillux::process_control::ResidencyProof| -> Result<()> {
        let mut established = capability_report.clone();
        established.establish_residency(proof.clone());
        established
            .require_floor_without_scopes()
            .context("node capability floor for a service without process scopes")
    };
    let standalone_native = match standalone_state_access {
        ryeos_app::service_registry::StandaloneStateAccess::ReadWrite => Some(
            ryeos_app::standalone_native::StandaloneNativeHost::issue(
                config,
                &identity,
                &standalone_state_lock,
                &admit_residency,
            )
            .context("issue the standalone native host")?,
        ),
        ryeos_app::service_registry::StandaloneStateAccess::ProjectionRebuild
        | ryeos_app::service_registry::StandaloneStateAccess::ReadOnlyExisting => {
            let (runtime, namespace) = standalone_namespace
                .as_ref()
                .context("stopped-node namespace absent")?;
            Some(
                ryeos_app::standalone_native::StandaloneNativeHost::issue_with_namespace(
                    config,
                    &identity,
                    &standalone_state_lock,
                    Arc::clone(runtime),
                    namespace,
                    &admit_residency,
                )
                .context("issue the standalone stopped-node native host")?,
            )
        }
    };
    let state_store = Arc::new(match standalone_state_access {
        ryeos_app::service_registry::StandaloneStateAccess::ReadWrite => {
            let host = standalone_native.as_ref().context("standalone native host absent")?;
            let _ = (runtime_state_dir, runtime_db_path);
            match host.open_store(config, &standalone_state_lock, signer, write_barrier.clone(), head_trust) {
                Ok(store) => store,
                Err(error) => {
                    let refusal = host.settle_refused_open(error);
                    let host = standalone_native.context("standalone native host absent")?;
                    return Err(match finish_standalone_native(host) {
                        Ok(()) => refusal,
                        Err(finish) => refusal.context(format!("{finish:#}")),
                    });
                }
            }
        }
        ryeos_app::service_registry::StandaloneStateAccess::ReadOnlyExisting => {
            let host = standalone_native.as_ref().context("standalone native host absent")?;
            let (runtime, namespace) = standalone_namespace
                .as_ref()
                .context("projection verification namespace absent")?;
            // A refused open has already settled its owners into the host's
            // census; the host then finishes before the refusal returns.
            match host.open_verification_store(config, runtime, namespace, signer, write_barrier.clone(), head_trust) {
                Ok(store) => store,
                Err(refusal) => {
                    let host = standalone_native.context("standalone native host absent")?;
                    return Err(match finish_standalone_native(host) {
                        Ok(()) => refusal,
                        Err(finish) => refusal.context(format!("{finish:#}")),
                    });
                }
            }
        }
        ryeos_app::service_registry::StandaloneStateAccess::ProjectionRebuild => {
            let host = standalone_native.as_ref().context("standalone native host absent")?;
            let (runtime, namespace) = standalone_namespace
                .as_ref()
                .context("projection rebuild namespace absent")?;
            let _ = (runtime_state_dir, runtime_db_path);
            // A refused open has already settled its owners into the host's
            // census; the host then finishes before the refusal returns.
            match host.open_rebuild_store(config, runtime, namespace, signer, write_barrier.clone(), head_trust) {
                Ok(store) => store,
                Err(refusal) => {
                    let host = standalone_native.context("standalone native host absent")?;
                    return Err(match finish_standalone_native(host) {
                        Ok(()) => refusal,
                        Err(finish) => refusal.context(format!("{finish:#}")),
                    });
                }
            }
        }
    });

    let events = Arc::new(event_store_service::EventStoreService::new(
        state_store.clone(),
    ));
    let event_streams = Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY));
    let threads = match thread_lifecycle::ThreadLifecycleService::new(
        state_store.clone(),
        engine.clone(),
        kind_profiles.clone(),
        events.clone(),
        event_streams.clone(),
        &identity.site_id(),
    ) {
        Ok(threads) => Arc::new(threads),
        Err(error) => {
            // The open Store closes into its host's census and the host
            // finishes before the setup refusal returns.
            drop(events);
            drop(event_streams);
            return Err(settle_standalone_setup_failure(
                standalone_native,
                standalone_state_access,
                state_store,
                error,
            ));
        }
    };
    let live_input = Arc::new(ryeos_app::live_input_queue::LiveInputQueue::new());
    threads.set_live_input_queue(live_input.clone());
    let commands = Arc::new(command_service::CommandService::new(
        state_store.clone(),
        kind_profiles,
        events.clone(),
    ));

    let standalone_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new());
    let standalone_ui_state = Arc::new(ryeos_ui::UiState::new());
    let standalone_node_config_validator =
        prospective_node_config_validator(standalone_ui_state.clone(), engine.clone());
    let app_state = state::AppState {
        config: Arc::new(config.clone()),
        daemon_build: ryeos_app::build_info::get_for_version(env!("CARGO_PKG_VERSION")),
        isolation,
        state_store: Arc::clone(&state_store),
        engine: engine.clone(),
        resolution_cache: std::sync::Arc::new(ryeos_app::resolution_cache::ResolutionCache::new(
            128,
        )),
        engine_cache: ryeos_app::engine_cache::EngineCache::new(
            ryeos_app::engine_cache::EngineCacheConfig::default(),
        ),
        identity: Arc::new(identity),
        threads,
        live_input,
        events,
        event_streams,
        commands,
        callback_tokens: Arc::new(ryeos_app::callback_token::CallbackCapabilityStore::new()),
        thread_auth: Arc::new(ryeos_app::callback_token::ThreadAuthStore::new()),
        controller_lifetime: Arc::new(standalone_state_lock.retain()),
        extensions: {
            let mut extensions = ryeos_app::extension_state::ExtensionState::new();
            extensions.insert(Arc::new(
                ryeos_api::project_deploy::ProjectScheduleRecoveryOwners::default(),
            ));
            // This is the stopped-node invocation's actual exclusion guard,
            // retained through service completion. Never install the running
            // daemon's lifecycle lock as standalone authority in extensions.
            extensions.insert(Arc::clone(&standalone_state_lock));
            extensions.insert(standalone_ui_state);
            extensions.insert(standalone_node_config_validator);
            let publication_authorities =
                ryeos_app::bundle_publication::producer::BundleReleaseAuthorityRouter {
                    publisher,
                    cas_composition: Some(Arc::new(
                        ryeos_app::bundle_publication::producer::VerifiedLocalCasCompositionAuthority::new(
                            Arc::clone(&state_store),
                        ),
                    )),
                    ..Default::default()
                };
            let publication_adapter = Arc::new(
                ryeos_app::bundle_publication::producer::PersistentBundleReleaseAdapter::new(
                    config.app_root.clone(), publication_policy,
                    publication_authorities,
                ),
            );
            extensions.insert(Arc::new(
                ryeos_app::bundle_publication::producer::BundleReleaseAuthorities::new(publication_adapter),
            ));
            Arc::new(extensions)
        },
        write_barrier: Arc::new(write_barrier),
        started_at: Instant::now(),
        started_at_iso: lillux::time::iso8601_now(),
        catalog_health: state::CatalogHealth {
            status: "standalone".into(),
            missing_services: vec![],
        },
        services,
        service_descriptors: service_descriptors(),
        node_config: node_config_snapshot.clone(),
        external_placement_backends: Arc::new(external_artifacts.placement_backends),
        // External connector generations must be supplied by signed bundle
        // composition; an empty registry refuses external profiles.
        external_candidate_connectors: Arc::new(external_artifacts.connectors),
        external_provider_configurations: Arc::new(external_artifacts.provider_configurations),
        external_candidate_imports: Arc::new(Default::default()),
        node_policy: node_policy_snapshot.clone(),
        vault: standalone_vault,
        provider_env: Arc::clone(provider_environment.environment()),
        command_registry: standalone_command_registry,
        authorizer: standalone_auth,
        ignore_matcher: standalone_ignore_matcher,
        vault_fingerprint: None,
        // Standalone service invocations perform no paid launches; hard
        // admission is uniformly unavailable here.
        accounting: None,
        persistent_sessions: Arc::new(
            ryeos_app::persistent_session::PersistentSessionPool::disabled(),
        ),
        execution_resources: Arc::new(
            ryeos_app::execution_resources::ExecutionResourcePool::deny_all(),
        ),
        scoped_producer_authorities: Arc::new(Default::default()),
        scoped_producer_processes: Arc::new(Default::default()),
        qualification_workspace_custody: Arc::new(Default::default()),
    };
    // A projection rebuild recovers pending terminal-chain removals against
    // scheduler fire pins. The stopped node has no scheduler service, so when
    // its composition selects the scheduler the rebuild opens the scheduler
    // store on the same host as its Store and installs that owner as the
    // lifecycle's scheduler participant; it is settled into the host's census
    // after the service, before the Store. A composition without the
    // scheduler installs no participant.
    #[cfg(feature = "scheduling")]
    let standalone_scheduler = match standalone_state_access {
        ryeos_app::service_registry::StandaloneStateAccess::ProjectionRebuild
            if standalone_scheduler_selected =>
        {
            let host = standalone_native.as_ref().context("standalone native host absent")?;
            let (runtime, _namespace) = standalone_namespace
                .as_ref()
                .context("projection rebuild namespace absent")?;
            let scheduler = match standalone_scheduler::StoppedNodeScheduler::open(
                host,
                runtime,
                &app_state.identity,
            ) {
                Ok(scheduler) => scheduler,
                Err(error) => {
                    drop(app_state);
                    return Err(settle_standalone_setup_failure(
                        standalone_native,
                        standalone_state_access,
                        state_store,
                        error.context("open the stopped-node scheduler for projection rebuild"),
                    ));
                }
            };
            let participant: Arc<dyn ryeos_app::scheduler_boundary::SchedulerParticipant> =
                scheduler.clone();
            if let Err(error) = app_state.install_scheduler_participant(participant) {
                drop(app_state);
                let error = match scheduler.settle(host) {
                    Ok(()) => error,
                    Err(settle) => ryeos_state::operational::CarriedOwners::carry(
                        error.context(format!("stopped-node scheduler did not settle: {settle:#}")),
                        settle,
                    ),
                };
                return Err(settle_standalone_setup_failure(
                    standalone_native,
                    standalone_state_access,
                    state_store,
                    error,
                ));
            }
            Some(scheduler)
        }
        ryeos_app::service_registry::StandaloneStateAccess::ProjectionRebuild
        | ryeos_app::service_registry::StandaloneStateAccess::ReadOnlyExisting
        | ryeos_app::service_registry::StandaloneStateAccess::ReadWrite => None,
    };

    let ctx = ExecutionContext {
        principal_fingerprint: standalone_principal,
        caller_scopes: vec![], // standalone: operator authority, no caps
        engine,
        plan_ctx: standalone_plan_ctx,
        requested_call: None,
    };

    let outcome = service_executor::execute_service_verified(
        service_verified,
        service_ref,
        params,
        ExecutionMode::Standalone,
        &ctx,
        &app_state,
        service_executor::ServiceRecordingContext {
            authority_source:
                service_executor::ServiceRecordingAuthoritySource::ExplicitProjectless,
            usage_subject: None,
            usage_subject_asserted_by: None,
        },
        None,
        None,
        Some(standalone_handler_context),
    )
    .await;
    // Every Store alias ends with the service state; the stopped-node
    // scheduler and the native Store then close into their host's census and
    // the host finishes. A scheduler refusal rides with the Store's result
    // and keeps the owners it carries.
    drop(ctx);
    drop(app_state);
    if let Some(host) = standalone_native {
        #[cfg(feature = "scheduling")]
        let scheduler_settled = match standalone_scheduler {
            Some(scheduler) => scheduler.settle(&host),
            None => Ok(()),
        };
        #[cfg(not(feature = "scheduling"))]
        let scheduler_settled: Result<()> = Ok(());
        let closed = match standalone_state_access {
            ryeos_app::service_registry::StandaloneStateAccess::ProjectionRebuild
            | ryeos_app::service_registry::StandaloneStateAccess::ReadOnlyExisting => {
                host.close_control_store(state_store)
            }
            ryeos_app::service_registry::StandaloneStateAccess::ReadWrite => host.close_store(state_store),
        };
        let closed = match (scheduler_settled, closed) {
            (Ok(()), closed) => closed,
            (Err(scheduler), Ok(())) => Err(scheduler),
            (Err(scheduler), Err(store)) => Err(ryeos_state::operational::CarriedOwners::carry(
                scheduler.context(format!("standalone native Store did not settle: {store:#}")),
                store,
            )),
        };
        let finished = finish_standalone_native(host);
        match (&outcome, closed, finished) {
            (_, Ok(()), Ok(())) => {}
            (Ok(_), Err(error), _) | (Ok(_), Ok(()), Err(error)) => return Err(error),
            (Err(_), closed, finished) => {
                if let Err(error) = closed.and(finished) {
                    tracing::error!(error = %format!("{error:#}"), "standalone native Store did not settle after the service failed");
                }
            }
        }
    }
    let result = outcome?;

    tracing::info!(
        endpoint = %result.endpoint,
        trust_class = ?result.trust_class,
        effective_caps = ?result.effective_caps,
        invocation_id = %result.invocation_id,
        recorded = result.recorded,
        "standalone service completed"
    );

    // Print result to stdout
    println!("{}", serde_json::to_string_pretty(&result.value)?);

    Ok(())
}

#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
mod selected_composition_tests {
    use super::*;
    use ryeos_executor::{SelectedInstallationRefusal, SelectedInvocationAttachment};

    #[test]
    fn native_finalizer_attempt_allowance_is_four_total_and_cannot_renew() {
        let mut attempts = NativeFinalizerAttempts::default();
        for _ in 0..ryeos_node::native_capacity::MAX_FINALIZATION_ATTEMPTS {
            attempts.claim().unwrap();
        }

        assert!(attempts.exhausted());
        assert!(attempts.claim().is_err());
        assert_eq!(attempts.0, ryeos_node::native_capacity::MAX_FINALIZATION_ATTEMPTS);
    }

    #[tokio::test]
    async fn exhausted_finalizer_returns_secret_free_custody_without_claiming_exit() {
        let root = tempfile::tempdir().unwrap();
        let (mut composition, _attachment) = fixture(root.path());
        composition.native_failure = Some(anyhow::anyhow!("sensitive source detail"));
        composition.native_finalizer_attempts.0 = ryeos_node::native_capacity::MAX_FINALIZATION_ATTEMPTS;

        let custody = composition.hold_unresolved_custody().await.unwrap_err();
        assert_eq!(custody.attempts_used, ryeos_node::native_capacity::MAX_FINALIZATION_ATTEMPTS);
        assert!(custody.native_failure_retained);
        assert!(!custody.to_string().contains("sensitive source detail"));
        let state_dir = composition.lifecycle_exit.as_ref().unwrap().state_dir.clone();
        assert!(matches!(
            lifecycle_marker::read(&state_dir),
            Some(lifecycle_marker::LifecycleMarker::Running { .. })
        ));

        // Keep Drop on the ordinary resolved path for this in-process control.
        composition.native_failure = None;
        composition.close_after_run();
    }

    fn fixture(
        root: &std::path::Path,
    ) -> (DaemonSelectedComposition, SelectedInvocationAttachment) {
        let (barrier, attachment) =
            ryeos_executor::SelectedInvocationCompositionBarrier::bootstrap();
        let lock_path = state_lock::default_lock_path(root);
        let lock = state_lock::StateLock::acquire(&lock_path).unwrap();
        let state_dir = ryeos_engine::roots::RuntimeRoot::new(root.to_owned()).state();
        std::fs::create_dir_all(&state_dir).unwrap();
        let started = lifecycle_marker::record_running(
            &state_dir,
            lifecycle_marker::LaunchRoute::Direct,
        )
        .unwrap();
        (
            DaemonSelectedComposition {
                root: barrier,
                launch_route: Some(lifecycle_marker::LaunchRoute::Direct),
                supervised_app_root: None,
                process_state_lock: Some(lock),
                #[cfg(target_os = "linux")]
                native_operator_lock: None,
                #[cfg(target_os = "linux")]
                native_operator_lock_registered: false,
                lifecycle_exit: Some(LifecycleExitGuard::new(
                    state_dir,
                    started,
                    lifecycle_marker::LaunchRoute::Direct,
                )),
                staged_exit_record: None,
                operator_target_retirement_entered: false,
                persistent_pool_custody: None,
                app_state_custody: None,
                live_state_store_custody: None,
                native_bootstrap_entered: false,
            native_inputs: None,
                native_reservation: None,
                native_factory: None,
                native_failure: None,
            services_drained: false,
            native_closed_store: None,
            native_outside_store: None,
            native_prepared_registry: None,
            native_store_close_failure: None,
            native_reservation_close_failure: None,
            native_settled_reservation: None,
            native_terminal: None,
                native_startup_recovery_failure: None,
                native_released_startup: None,
                native_finalizer_attempts: NativeFinalizerAttempts::default(),
                native_admission_fence: Arc::default(),
                native_control_observation: std::sync::Mutex::new(None),
                accounting_native: None,
                native_sources: composition_native::CompositionNativeSources::new(),
                accounting_publisher: Arc::default(),
                service_stop: ServiceStopState::default(),
                #[cfg(feature = "scheduling")]
                scheduler_db_custody: None,
                #[cfg(feature = "scheduling")]
                scheduler_admission_failure: None,
                #[cfg(feature = "scheduling")]
                scheduler_admission_sources: None,
                #[cfg(feature = "scheduling")]
                scheduler_participant_custody: None,
                #[cfg(feature = "scheduling")]
                scheduler_open_failure: None,
                #[cfg(feature = "scheduling")]
                scheduler_schema_failure: None,
                #[cfg(feature = "scheduling")]
                scheduler_recovered_schema: None,
                #[cfg(feature = "scheduling")]
                scheduler_admission_custody: None,
            },
            attachment,
        )
    }

    #[test]
    fn normal_exit_closes_empty_root_before_marker_and_keeps_same_exclusion() {
        let root = tempfile::tempdir().unwrap();
        let (mut composition, attachment) = fixture(root.path());
        let lock_path = state_lock::default_lock_path(root.path());
        let state_dir = composition
            .lifecycle_exit
            .as_ref()
            .unwrap()
            .state_dir
            .clone();
        assert!(matches!(
            lifecycle_marker::read(&state_dir),
            Some(lifecycle_marker::LifecycleMarker::Running { .. })
        ));
        composition.close_after_run();
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Closed
        );
        assert!(state_lock::StateLock::acquire(&lock_path).is_err());
        assert!(matches!(
            lifecycle_marker::read(&state_dir),
            Some(lifecycle_marker::LifecycleMarker::Running { .. })
        ));
        composition.record_exit("signal", None, None);
        assert!(
            matches!(lifecycle_marker::read(&state_dir), Some(lifecycle_marker::LifecycleMarker::Exited { reason, .. }) if reason == "signal")
        );
        assert!(state_lock::StateLock::acquire(&lock_path).is_err());
        drop(composition);
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Detached
        );
        let _replacement = state_lock::StateLock::acquire(&lock_path).unwrap();
    }

    #[test]
    fn floor_refusal_before_native_bootstrap_inhibits_restart_and_transient_failure_does_not() {
        let root = tempfile::tempdir().unwrap();
        let (mut composition, _attachment) = fixture(root.path());
        let state_dir = composition.lifecycle_exit.as_ref().unwrap().state_dir.clone();
        assert!(!composition.native_bootstrap_entered);
        // A failure that is not a floor refusal, before native bootstrap,
        // leaves the next start free to retry.
        let transient = anyhow::anyhow!("fixture transient failure").context("Phase 1: fixture");
        composition.inhibit_supervised_restart_after_startup_failure("startup_failed", Some(&transient));
        assert!(lifecycle_marker::read_restart_inhibition(&state_dir).unwrap().is_none());
        // The floor refusal is found through Phase 1 context and inhibits.
        let refused = anyhow::Error::new(ryeos_node::capability_report::CapabilityFloorRefused(
            "startup refused before native reservation: this host cannot prove what node policy \
             requires\n  requires accounting: fixture"
                .to_owned(),
        ))
        .context("Phase 1: node capability floor");
        composition.inhibit_supervised_restart_after_startup_failure("signal", Some(&refused));
        assert!(lifecycle_marker::read_restart_inhibition(&state_dir).unwrap().is_none());
        composition.inhibit_supervised_restart_after_startup_failure("startup_failed", Some(&refused));
        assert!(state_dir.join("restart-inhibited.json").is_file());
        let inhibition = lifecycle_marker::read_restart_inhibition(&state_dir).unwrap().unwrap();
        assert_eq!(inhibition.reason, "capability_floor_refused");
        assert!(
            inhibition.detail.as_deref().unwrap_or_default().contains("requires accounting: fixture"),
            "{inhibition:?}"
        );
    }

    #[test]
    fn floor_refusal_after_reservation_inhibits_restart_and_retains_typed_custody() {
        let root = tempfile::tempdir().unwrap();
        let (mut composition, _attachment) = fixture(root.path());
        let state_dir = composition.lifecycle_exit.as_ref().unwrap().state_dir.clone();
        let identity = NodeIdentity::create(&root.path().join("node.pem")).unwrap();
        let locked = || lillux::process_control::ResidencyProof::Locked { basis: "fixture".to_owned() };
        // Without a proof or a verified report the floor cannot be decided,
        // so the start is refused rather than admitted.
        let no_proof = record_established_residency(None, Ok(None), &identity, root.path()).unwrap_err();
        assert!(format!("{no_proof:#}").contains("no residency proof"), "{no_proof:#}");
        let no_report =
            record_established_residency(Some(locked()), Ok(None), &identity, root.path()).unwrap_err();
        assert!(format!("{no_report:#}").contains("no verified node capability report"), "{no_report:#}");
        let stale = record_established_residency(
            Some(locked()),
            Err(anyhow::anyhow!("stale report: fixture")),
            &identity,
            root.path(),
        )
        .unwrap_err();
        assert!(format!("{stale:#}").contains("stale report"), "{stale:#}");

        assert!(lifecycle_marker::read_restart_inhibition(&state_dir).unwrap().is_none());
        let floor = anyhow::anyhow!(
            "startup refused after native reservation, on the residency it established: fixture"
        );
        assert!(lillux_sqlite::native_refusal_is_fact(&floor));
        let returned = composition.refuse_established_residency(floor);
        assert!(returned.to_string().contains("custody retained"), "{returned}");
        assert!(state_dir.join("restart-inhibited.json").is_file());
        let inhibition = lifecycle_marker::read_restart_inhibition(&state_dir).unwrap().unwrap();
        assert_eq!(inhibition.reason, "capability_floor_refused");
        assert!(inhibition.detail.as_deref().unwrap_or_default().contains("fixture"), "{inhibition:?}");
        let retained = composition.native_failure.take().unwrap();
        assert!(retained.is::<EstablishedResidencyRefused>());
        assert!(format!("{retained}").contains("fixture"));
    }

    #[test]
    fn exhaustion_refusal_after_bootstrap_inhibits_restart_and_leaves_no_custody() {
        let root = tempfile::tempdir().unwrap();
        let (mut composition, attachment) = fixture(root.path());
        let state_dir = composition.lifecycle_exit.as_ref().unwrap().state_dir.clone();
        let lock_path = state_lock::default_lock_path(root.path());
        let capacity_namespace = root.path().join(".ai/node/native-capacity");
        // The refusal exactly as prepare_live_native reports an exhausted
        // allowance: the refused checkout is retained typed and then named
        // by its Debug form. It is a durable fact of this host, not
        // contention, the service edge or a transient shortage.
        let refused = anyhow::anyhow!(
            "live native reservation refused; original recovery custody retained: \
             LiveNativeCapacityReservationFailure {{ error: native capacity account \
             exhausted, .. }}"
        );
        assert!(lillux_sqlite::native_refusal_is_fact(&refused));
        let detail = format!("{refused:#}");

        // Before native bootstrap nothing can have been spent, so the same
        // refusal leaves the next start free to retry.
        assert!(!composition.native_bootstrap_entered);
        composition.inhibit_supervised_restart_after_startup_failure("startup_failed", Some(&refused));
        assert!(lifecycle_marker::read_restart_inhibition(&state_dir).unwrap().is_none());

        // Once bootstrap claimed its attempt the exhaustion refusal settles
        // through the native shutdown with every owner released, so the
        // startup-failure exit is taken rather than the unresolved-custody
        // exit, and the inhibition names the exhaustion.
        composition.native_bootstrap_entered = true;
        assert!(!composition.has_unresolved_custody());
        composition.record_exit("startup_failed", Some(&detail), Some(&refused));
        assert!(!composition.has_unresolved_custody());
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Closed);
        assert!(matches!(
            lifecycle_marker::read(&state_dir),
            Some(lifecycle_marker::LifecycleMarker::Exited { reason, .. }) if reason == "startup_failed"
        ));
        assert!(state_dir.join("restart-inhibited.json").is_file());
        let inhibition = lifecycle_marker::read_restart_inhibition(&state_dir).unwrap().unwrap();
        assert_eq!(inhibition.reason, "startup_failed");
        assert!(
            inhibition.detail.as_deref().unwrap_or_default().contains("native capacity account exhausted"),
            "{inhibition:?}"
        );
        // A refused debit writes nothing: no account or anchor record is
        // created or touched by the exit.
        assert!(!capacity_namespace.exists());
        drop(composition);
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Detached);
        let _replacement = state_lock::StateLock::acquire(&lock_path).unwrap();
    }

    #[test]
    fn floor_refusal_before_account_after_bootstrap_inhibits_restart_and_leaves_no_custody() {
        let root = tempfile::tempdir().unwrap();
        let (mut composition, attachment) = fixture(root.path());
        let state_dir = composition.lifecycle_exit.as_ref().unwrap().state_dir.clone();
        let lock_path = state_lock::default_lock_path(root.path());
        let capacity_namespace = root.path().join(".ai/node/native-capacity");
        // A signed envelope floor refused inside reserve, before any account
        // owner exists: a plain error, exactly as prepare_live_native returns
        // it once the sole input alias has been released. A floor of the
        // signed envelope is a durable fact of this host and policy.
        let refused = anyhow::anyhow!(
            "rollover drain margin 1s is below the bounded normal-stop floor 30s"
        );
        assert!(lillux_sqlite::native_refusal_is_fact(&refused));
        let detail = format!("{refused:#}");

        // Bootstrap claimed its attempt, but the refusal preceded the debit,
        // so no input alias, reservation or failure owner is retained and
        // the native shutdown has nothing to settle.
        composition.native_bootstrap_entered = true;
        assert!(composition.native_inputs.is_none());
        assert!(!composition.has_unresolved_custody());
        composition.advance_native_shutdown_on_settlement_stack().unwrap();
        assert!(!composition.has_unresolved_custody());
        composition.record_exit("startup_failed", Some(&detail), Some(&refused));
        assert!(!composition.has_unresolved_custody());
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Closed);
        assert!(matches!(
            lifecycle_marker::read(&state_dir),
            Some(lifecycle_marker::LifecycleMarker::Exited { reason, .. }) if reason == "startup_failed"
        ));
        assert!(state_dir.join("restart-inhibited.json").is_file());
        let inhibition = lifecycle_marker::read_restart_inhibition(&state_dir).unwrap().unwrap();
        assert_eq!(inhibition.reason, "startup_failed");
        assert!(
            inhibition.detail.as_deref().unwrap_or_default().contains("below the bounded normal-stop floor"),
            "{inhibition:?}"
        );
        // Nothing was debited, so no account or anchor record exists.
        assert!(!capacity_namespace.exists());
        drop(composition);
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Detached);
        let _replacement = state_lock::StateLock::acquire(&lock_path).unwrap();
    }

    #[test]
    fn startup_error_drops_external_envelope_with_failed_marker() {
        let root = tempfile::tempdir().unwrap();
        let (composition, attachment) = fixture(root.path());
        let state_dir = composition
            .lifecycle_exit
            .as_ref()
            .unwrap()
            .state_dir
            .clone();
        let result = (|| -> Result<()> {
            let _composition = composition;
            anyhow::bail!("fixture startup error");
        })();
        assert!(result.is_err());
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Detached
        );
        assert!(
            matches!(lifecycle_marker::read(&state_dir), Some(lifecycle_marker::LifecycleMarker::Exited { reason, .. }) if reason == "startup_failed")
        );
        let _replacement =
            state_lock::StateLock::acquire(&state_lock::default_lock_path(root.path())).unwrap();
    }

    #[test]
    fn run_unwind_keeps_envelope_and_original_lease_until_explicit_close() {
        let root = tempfile::tempdir().unwrap();
        let (mut composition, attachment) = fixture(root.path());
        let state_dir = composition
            .lifecycle_exit
            .as_ref()
            .unwrap()
            .state_dir
            .clone();
        let lease = composition.process_state_lock.as_ref().unwrap().retain();
        let lock_path = state_lock::default_lock_path(root.path());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _borrowed_composition = &composition;
            panic!("fixture run panic");
        }));
        assert!(result.is_err());
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Unarmed
        );
        assert!(matches!(
            lifecycle_marker::read(&state_dir),
            Some(lifecycle_marker::LifecycleMarker::Running { .. })
        ));
        composition.close_after_run();
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Closed
        );
        drop(composition);
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Detached
        );
        assert!(
            matches!(lifecycle_marker::read(&state_dir), Some(lifecycle_marker::LifecycleMarker::Exited { reason, .. }) if reason == "startup_failed")
        );
        assert!(state_lock::StateLock::acquire(&lock_path).is_err());
        drop(lease);
        let _replacement = state_lock::StateLock::acquire(&lock_path).unwrap();
    }
}

#[cfg(test)]
mod shutdown_mapping_tests {
    use super::build_service_registry;
    use ryeos_app::process::{ShutdownAction, resolve_shutdown_action};
    use ryeos_engine::contracts::CancellationMode;
    use std::time::Duration;

    #[test]
    fn standalone_ingress_carries_only_the_loaded_local_operator_authority() {
        let root = tempfile::tempdir().unwrap();
        let operator =
            ryeos_node_runtime::identity::NodeIdentity::create(&root.path().join("operator.pem")).unwrap();
        let context = super::standalone_operator_handler_context(&operator);
        assert!(context.verified);
        assert_eq!(context.fingerprint, operator.principal_id());
        assert!(context.scopes.is_empty());
        assert_eq!(
            context.authorized_key_class,
            Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::LocalClient)
        );
        assert!(context.authenticated_origin_site_id.is_none());
        context
            .validate_execution_authority(&operator.principal_id(), &[], "site:local", "site:local")
            .unwrap();
    }

    #[test]
    fn composed_service_registry_has_unique_refs_and_endpoints() {
        build_service_registry().expect("composed service registry must be unambiguous");
    }

    #[test]
    fn hard_mode_maps_to_hard_kill() {
        assert_eq!(
            resolve_shutdown_action(Some(CancellationMode::Hard)),
            ShutdownAction::Hard
        );
    }

    #[test]
    fn graceful_mode_is_capped_by_node_policy() {
        assert_eq!(
            resolve_shutdown_action(Some(CancellationMode::Graceful { grace_secs: 11 })),
            ShutdownAction::Graceful(Duration::from_secs(
                ryeos_app::process::MAX_GRACEFUL_SHUTDOWN_GRACE_SECS,
            ))
        );
    }

    #[test]
    fn no_mode_falls_back_to_three_second_default() {
        assert_eq!(
            resolve_shutdown_action(None),
            ShutdownAction::Graceful(Duration::from_secs(3))
        );
    }

    #[test]
    fn graceful_zero_is_preserved() {
        // Graceful{0} is distinct from Hard — we still attempt SIGTERM
        // first via the poll loop, just with a zero-length grace.
        assert_eq!(
            resolve_shutdown_action(Some(CancellationMode::Graceful { grace_secs: 0 })),
            ShutdownAction::Graceful(Duration::from_secs(0))
        );
    }
}

#[cfg(test)]
mod shutdown_custody_tests {
    use super::{DrainOutcome, complete_shutdown_drain, map_shutdown_drain_join};
    use std::future::Future as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    struct DropObserved(Arc<AtomicBool>);

    impl Drop for DropObserved {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn unresolved_drain_keeps_real_owner_pending_despite_prior_listener_error() {
        let prior = Err(anyhow::anyhow!("listener failed first"));
        let dropped = Arc::new(AtomicBool::new(false));
        let mut custody = Some(DropObserved(Arc::clone(&dropped)));
        let mut future = Box::pin(complete_shutdown_drain(Ok(DrainOutcome::Unproved), &prior, &mut custody));
        let waker = Waker::from(Arc::new(NoopWake));
        let mut context = Context::from_waker(&waker);
        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        assert!(!dropped.load(Ordering::Acquire));
        drop(future);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn drain_task_panic_keeps_real_owner_pending() {
        let task = tokio::spawn(async { panic!("injected shutdown coordinator panic") });
        let task_result = map_shutdown_drain_join(task.await.map(|()| DrainOutcome::Clean));
        let prior = Ok(());
        let dropped = Arc::new(AtomicBool::new(false));
        let mut custody = Some(DropObserved(Arc::clone(&dropped)));
        let mut future = Box::pin(complete_shutdown_drain(task_result, &prior, &mut custody));
        let waker = Waker::from(Arc::new(NoopWake));
        let mut context = Context::from_waker(&waker);
        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        assert!(!dropped.load(Ordering::Acquire));
        drop(future);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn successful_drain_releases_owner_and_preserves_listener_error() {
        let prior = Err(anyhow::anyhow!("listener failed first"));
        let dropped = Arc::new(AtomicBool::new(false));
        let mut custody = Some(DropObserved(Arc::clone(&dropped)));
        complete_shutdown_drain(Ok(DrainOutcome::Clean), &prior, &mut custody).await;
        assert!(custody.is_none());
        assert!(dropped.load(Ordering::Acquire));
    }
}

#[cfg(test)]
mod render_composition_custody_tests {
    use super::DaemonSelectedComposition;
    use std::sync::Arc;

    #[test]
    fn outer_composition_retains_the_same_pool_arc_as_unresolved_custody() {
        let mut composition = DaemonSelectedComposition::bootstrap();
        let pool = Arc::new(ryeos_app::persistent_session::PersistentSessionPool::disabled());
        composition.retain_persistent_pool(Arc::clone(&pool));

        assert!(composition.has_unresolved_custody());
        assert!(Arc::ptr_eq(
            composition.persistent_pool_custody.as_ref().unwrap(),
            &pool,
        ));

        // Clear fixture custody so this test can return without entering the
        // production Drop hold. AppState adoption is not exercised here.
        composition.persistent_pool_custody.take();
        assert!(!composition.has_unresolved_custody());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_operator_lock_alone_is_unresolved_custody() {
        let root = tempfile::tempdir().unwrap();
        let mut composition = DaemonSelectedComposition::bootstrap();
        assert!(!composition.has_unresolved_custody());
        let lock = lillux::ExactExclusiveFileLock::acquire(&root.path().join("operator.lock")).unwrap();
        composition.native_operator_lock = Some(
            ryeos_node::native_capacity::OperatorLockCustody::new(Arc::new(lock)));

        assert!(composition.has_unresolved_custody());

        // Release the fixture owner so Drop takes the resolved path.
        let owner = composition.native_operator_lock.take().unwrap();
        drop(owner.into_taken().ok().unwrap());
        assert!(!composition.has_unresolved_custody());
    }

    /// The abandoned counts are read from lillux's process-wide registries,
    /// never from composition fields. Nothing here abandons an owner: that
    /// would end this test process with status 70 at the next composition
    /// drop, which is the behaviour under test.
    #[test]
    fn abandoned_custody_is_read_from_the_process_registries() {
        let observed = super::AbandonedNativeCustody::observe();
        assert_eq!(observed.owners, lillux::custody::abandoned_custody().total);
        assert_eq!(observed.fork_exclusions, lillux::fork_exclusions_held_until_exit());
        assert_eq!(observed.total(), observed.owners + observed.fork_exclusions);
        assert_eq!(super::has_abandoned_native_custody(), observed.total() > 0);

        let composition = DaemonSelectedComposition::bootstrap();
        let custody = composition.unresolved_native_custody();
        assert_eq!(custody.abandoned_owners, observed.owners);
        assert_eq!(custody.fork_exclusions_held, observed.fork_exclusions);
    }

    #[test]
    fn abandoned_custody_names_its_own_exit_reason() {
        let composition = DaemonSelectedComposition::bootstrap();
        let mut custody = composition.unresolved_native_custody();
        custody.abandoned_owners = 0;
        custody.fork_exclusions_held = 0;
        assert!(!custody.abandoned());
        assert_eq!(custody.exit_reason(), "native_custody_unresolved");

        custody.abandoned_owners = 1;
        assert!(custody.abandoned());
        assert_eq!(custody.exit_reason(), "native_custody_abandoned");
        assert!(custody.to_string().contains("abandoned_owners=1 fork_exclusions_held=0"));

        custody.abandoned_owners = 0;
        custody.fork_exclusions_held = 2;
        assert!(custody.abandoned());
        assert_eq!(custody.exit_reason(), "native_custody_abandoned");
        assert!(custody.to_string().contains("abandoned_owners=0 fork_exclusions_held=2"));
    }
}

#[cfg(test)]
mod node_capability_report_reader_tests {
    use super::*;

    use lillux::process_control::{HOST_PROVIDER, HostCapabilities, HostFacts, ProofSource};
    use ryeos_app::extension_state::ExtensionState;
    use ryeos_app::node_requirement_admission::{
        CREDENTIAL_PROFILE_KIND, NodeRequirementRefused, admit_structured_session_start,
    };
    use ryeos_node::capability_report::{
        CAPABILITY_REPORT_SCHEMA, CREDENTIAL_PRIVATE_HOME, NodeCapabilityReport, NodeComposition,
        NodeExposure, ReportGeneration, SignedNodeCapabilityReport, match_floor,
    };

    const POLICY: &str = "sha256:fixture-policy";

    /// Admit a structured-session start whose profile requires time of the
    /// controller and a private credential home for the credential-profile
    /// kind, on the node `extensions` composes.
    fn admit_login(extensions: &ExtensionState) -> Result<()> {
        admit_structured_session_start(
            extensions,
            &serde_json::json!({"requires": {
                "workload_class": "fixture.login",
                "controller": ["time"],
                "credential_kinds": {"credential_profile": [
                    {"name": CREDENTIAL_PRIVATE_HOME, "accept": ["measured", "provider_attested", "trusted"]},
                ]},
            }}),
            Some(CREDENTIAL_PROFILE_KIND),
        )
    }

    fn node_reading(app_root: &std::path::Path) -> ExtensionState {
        let extensions = ExtensionState::new();
        extensions.insert(Arc::new(node_capability_report_reader(
            app_root.to_path_buf(),
            POLICY.to_owned(),
        )));
        extensions
    }

    /// Record a report signed by a fresh node identity under `app_root`,
    /// describing this host's current generation and `policy`.
    fn record_report(app_root: &std::path::Path, policy: &str) {
        let identity_dir = app_root.join(ryeos_engine::AI_DIR).join("node/identity");
        std::fs::create_dir_all(&identity_dir).unwrap();
        let identity = NodeIdentity::create(&identity_dir.join("node-key.pem")).unwrap();
        identity
            .write_public_identity(&identity_dir.join("public-identity.json"))
            .unwrap();
        let mut view = HostCapabilities::default();
        view.provide("time", ProofSource::Measured, HOST_PROVIDER, "fixture");
        view.provide(CREDENTIAL_PRIVATE_HOME, ProofSource::Trusted, HOST_PROVIDER, "fixture");
        let (boot_id, cgroup_enclosure) =
            lillux::process_control::current_host_generation().unwrap();
        let state_dir = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).state();
        let report = NodeCapabilityReport {
            schema: CAPABILITY_REPORT_SCHEMA.to_owned(),
            node_fingerprint: identity.fingerprint().to_owned(),
            daemon_digest: "sha256:fixture".to_owned(),
            generated_at: "fixture".to_owned(),
            generation: ReportGeneration { boot_id, cgroup_enclosure },
            subject: "node".to_owned(),
            composition: NodeComposition {
                profile: None,
                host_class: "hosted".to_owned(),
                bundles: Vec::new(),
                compiled_features: Vec::new(),
                policy_generation: policy.to_owned(),
            },
            exposure: NodeExposure::unattested(Vec::new()),
            host: HostFacts::observe(0, std::path::Path::new("/")),
            credential_home: lillux::process_control::PrivateDirectoryFacts::observe(
                app_root,
                &ryeos_app::private_artifact_home::root(&state_dir),
            ),
            floor: match_floor(&[], &view),
            provided: view.provided,
            missing: view.missing,
            capacity: Default::default(),
            established_residency: None,
        };
        report.sign(identity.signing_key()).unwrap().write(app_root).unwrap();
    }

    fn refusal(error: anyhow::Error) -> String {
        format!("{error:#}")
    }

    #[test]
    fn a_node_without_the_reader_or_a_report_refuses_declared_requirements() {
        let missing_extension = refusal(
            admit_login(&ExtensionState::new())
                .unwrap_err(),
        );
        assert!(
            missing_extension.contains("this node has no signed capability report"),
            "{missing_extension}"
        );
        let app_root = tempfile::tempdir().unwrap();
        let missing_report = refusal(
            admit_login(&node_reading(app_root.path()))
                .unwrap_err(),
        );
        assert!(
            missing_report.contains("no verified node capability report is recorded"),
            "{missing_report}"
        );
    }

    #[test]
    fn a_stale_report_is_refused() {
        let older = tempfile::tempdir().unwrap();
        let path = SignedNodeCapabilityReport::path(older.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "report": {"schema": "ryeos.node.capability-report/v5", "host": {"version": 5}}
            }))
            .unwrap(),
        )
        .unwrap();
        let text = refusal(
            admit_login(&node_reading(older.path()))
                .unwrap_err(),
        );
        assert!(text.contains("stale report"), "{text}");

        let overwritten = tempfile::tempdir().unwrap();
        record_report(overwritten.path(), "sha256:another-daemon-generation");
        let text = refusal(
            admit_login(&node_reading(overwritten.path()))
                .unwrap_err(),
        );
        assert!(text.contains("the recorded node capability report is stale"), "{text}");
        assert!(text.contains(&format!("node policy generation is now {POLICY}")), "{text}");
    }

    #[test]
    fn the_credential_home_is_re_observed_at_admission() {
        let app_root = tempfile::tempdir().unwrap();
        record_report(app_root.path(), POLICY);
        // The startup report proved the home; it is absent now.
        let error =
            admit_login(&node_reading(app_root.path()))
                .unwrap_err();
        let text = error
            .downcast_ref::<NodeRequirementRefused>()
            .unwrap_or_else(|| panic!("not a requirement refusal: {error:#}"))
            .0
            .clone();
        assert!(
            text.contains(&format!(
                "requires.credential_kinds.credential_profile {CREDENTIAL_PRIVATE_HOME}: \
                 credential home root is no longer private at admission"
            )),
            "{text}"
        );
        // Once the root is private to this account again, the report's
        // proof stands.
        let state_dir = ryeos_engine::roots::RuntimeRoot::new(app_root.path().to_path_buf()).state();
        ryeos_app::private_artifact_home::ensure_root(&state_dir).unwrap();
        admit_login(&node_reading(app_root.path()))
            .unwrap();
    }

    fn is_refusal(error: &anyhow::Error) -> bool {
        error.downcast_ref::<NodeRequirementRefused>().is_some()
            && !ryeos_app::node_requirement_admission::is_report_unreadable(error)
    }

    #[test]
    fn a_report_that_cannot_stand_is_refused_and_only_unreadable_io_is_retryable() {
        // No report recorded: a refusal, not a transient read failure.
        let empty = tempfile::tempdir().unwrap();
        let error = admit_login(&node_reading(empty.path())).unwrap_err();
        assert!(is_refusal(&error), "{error:#}");

        // A report from another policy generation: a refusal.
        let overwritten = tempfile::tempdir().unwrap();
        record_report(overwritten.path(), "sha256:another-daemon-generation");
        let error = admit_login(&node_reading(overwritten.path())).unwrap_err();
        assert!(is_refusal(&error), "{error:#}");

        // A report whose content no longer matches its digest: a refusal.
        let tampered = tempfile::tempdir().unwrap();
        record_report(tampered.path(), POLICY);
        let path = SignedNodeCapabilityReport::path(tampered.path());
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["report"]["daemon_digest"] = serde_json::json!("sha256:tampered");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let error = admit_login(&node_reading(tampered.path())).unwrap_err();
        assert!(is_refusal(&error), "{error:#}");
        assert!(format!("{error:#}").contains("digest does not match"), "{error:#}");

        // A report whose file cannot be read: the one transient failure.
        let unreadable = tempfile::tempdir().unwrap();
        let path = SignedNodeCapabilityReport::path(unreadable.path());
        std::fs::create_dir_all(&path).unwrap();
        let error = admit_login(&node_reading(unreadable.path())).unwrap_err();
        assert!(error.downcast_ref::<NodeRequirementRefused>().is_none(), "{error:#}");
        assert!(
            ryeos_app::node_requirement_admission::is_report_unreadable(&error),
            "{error:#}"
        );
    }
}
