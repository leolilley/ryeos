//! Stable bootstrap HTTP surface and daemon startup publication coordinator.
//!
//! The TCP and UDS accept loops are brought up with this state before the
//! projection is opened.  Both transports reload their published application
//! for every request/frame, so connections accepted during recovery become
//! useful without reconnecting once the ready application is published.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use ryeos_node::{
    LifecycleIdentity, LifecycleResponse, LifecycleWireState, StartupPhase, StartupSnapshot,
};

const STARTUP_RETRY_AFTER_MS: u64 = 1_000;

/// Best-effort cleanup for all ordinary return/error paths after listener
/// binding. Crash/SIGKILL is handled by stale-socket discovery on next boot.
pub struct DiscoveryCleanup {
    uds_path: PathBuf,
    uds_identity: SocketIdentity,
    daemon_json_path: PathBuf,
}

#[derive(Clone, Copy)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl DiscoveryCleanup {
    pub fn new(uds_path: PathBuf, daemon_json_path: PathBuf) -> Result<Self> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};

        let metadata = std::fs::symlink_metadata(&uds_path)
            .with_context(|| format!("inspect bound daemon socket {}", uds_path.display()))?;
        if !metadata.file_type().is_socket() {
            anyhow::bail!(
                "bound daemon control path is not a socket: {}",
                uds_path.display()
            );
        }
        Ok(Self {
            uds_path,
            uds_identity: SocketIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            daemon_json_path,
        })
    }

    fn remove_owned_socket(&self) {
        use std::os::unix::fs::MetadataExt;

        let Ok(metadata) = std::fs::symlink_metadata(&self.uds_path) else {
            return;
        };
        if metadata.dev() == self.uds_identity.device && metadata.ino() == self.uds_identity.inode {
            let _ = std::fs::remove_file(&self.uds_path);
        }
    }
}

impl Drop for DiscoveryCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.daemon_json_path);
        self.remove_owned_socket();
    }
}

struct DynamicHttpInner {
    lifecycle: ArcSwap<LifecycleResponse>,
    application: ArcSwapOption<ryeos_api::ApiState>,
    external_admission_open: AtomicBool,
}

/// Request-level HTTP publication state.  The outer router is immutable for
/// the lifetime of the process; only the application pointer and lifecycle
/// snapshot change.
#[derive(Clone)]
pub struct DynamicHttpState {
    inner: Arc<DynamicHttpInner>,
}

impl DynamicHttpState {
    fn bootstrap(lifecycle: LifecycleResponse) -> Result<Self> {
        lifecycle
            .validate()
            .map_err(|message| anyhow!("invalid initial lifecycle response: {message}"))?;
        Ok(Self {
            inner: Arc::new(DynamicHttpInner {
                lifecycle: ArcSwap::from_pointee(lifecycle),
                application: ArcSwapOption::empty(),
                external_admission_open: AtomicBool::new(false),
            }),
        })
    }

    fn publish_application(&self, application: ryeos_api::ApiState) {
        self.inner.application.store(Some(Arc::new(application)));
    }

    fn publish_lifecycle(&self, lifecycle: LifecycleResponse) -> Result<()> {
        lifecycle
            .validate()
            .map_err(|message| anyhow!("invalid lifecycle response: {message}"))?;
        let current = self.lifecycle();
        if lifecycle.identity != current.identity {
            anyhow::bail!("lifecycle identity cannot change after listener publication");
        }
        if current.status != LifecycleWireState::Starting {
            anyhow::bail!("terminal lifecycle state cannot be republished");
        }
        if lifecycle.startup.sequence <= current.startup.sequence {
            anyhow::bail!("lifecycle publication sequence must increase");
        }
        if lifecycle.ready && self.inner.application.load().is_none() {
            anyhow::bail!("cannot publish Ready before the HTTP application");
        }
        self.inner.lifecycle.store(Arc::new(lifecycle));
        Ok(())
    }

    fn lifecycle(&self) -> Arc<LifecycleResponse> {
        self.inner.lifecycle.load_full()
    }

    fn application(&self) -> Option<Arc<ryeos_api::ApiState>> {
        self.inner.application.load_full()
    }

    fn application_is_published(&self) -> bool {
        self.inner.application.load().is_some()
    }

    fn open_external_admission(&self) {
        self.inner
            .external_admission_open
            .store(true, Ordering::Release);
    }

    fn close_external_admission(&self) {
        self.inner
            .external_admission_open
            .store(false, Ordering::Release);
    }

    fn unpublish_application(&self) {
        self.inner.application.store(None);
    }

    fn admission_is_open(&self) -> bool {
        self.inner.external_admission_open.load(Ordering::Acquire)
    }
}

/// Build the stable process-lifetime HTTP router.  Reserved lifecycle paths
/// never enter content routing and therefore never mint service threads.
pub fn build_outer_router(state: DynamicHttpState) -> Router {
    Router::new()
        .route("/_ryeos/health", get(health))
        .route("/_ryeos/ready", get(ready))
        .fallback(application_dispatch)
        .with_state(state)
}

async fn health(State(state): State<DynamicHttpState>) -> Response {
    let lifecycle = state.lifecycle();
    if lifecycle.status == LifecycleWireState::Failed {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "status": "failed",
                "ready": false,
                "error": { "code": "node_startup_failed" },
                "startup": public_startup(&lifecycle.startup),
            })),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(json!({
            "status": if lifecycle.ready { "running" } else { "starting" },
            "ready": lifecycle.ready,
            "started_at": &lifecycle.identity.started_at,
            "ready_at": &lifecycle.ready_at,
            "startup": public_startup(&lifecycle.startup),
        })),
    )
        .into_response()
}

async fn ready(State(state): State<DynamicHttpState>) -> Response {
    let lifecycle = state.lifecycle();
    if lifecycle.status == LifecycleWireState::Running
        && lifecycle.ready
        && state.admission_is_open()
    {
        return (
            StatusCode::OK,
            Json(json!({
                "status": "running",
                "ready": true,
                "started_at": &lifecycle.identity.started_at,
                "ready_at": &lifecycle.ready_at,
                "startup": public_startup(&lifecycle.startup),
            })),
        )
            .into_response();
    }
    unavailable_response(&lifecycle)
}

async fn application_dispatch(State(state): State<DynamicHttpState>, request: Request) -> Response {
    let lifecycle = state.lifecycle();
    if lifecycle.status != LifecycleWireState::Running
        || !lifecycle.ready
        || !state.admission_is_open()
    {
        return unavailable_response(&lifecycle);
    }
    let Some(application) = state.application() else {
        // Publication ordering makes this unreachable, but failing closed here
        // protects the invariant if a future caller violates that ordering.
        return unavailable_response(&lifecycle);
    };
    ryeos_api::routes::dispatcher::route_dispatcher(State((*application).clone()), request).await
}

fn unavailable_response(lifecycle: &LifecycleResponse) -> Response {
    let failed = lifecycle.status == LifecycleWireState::Failed;
    let retry_after_ms = if failed {
        None
    } else {
        Some(STARTUP_RETRY_AFTER_MS)
    };
    let body = Json(json!({
        "error": {
            "code": if failed { "node_startup_failed" } else { "node_starting" },
            "message": if failed {
                "daemon initialization failed"
            } else {
                "daemon recovery is not complete"
            },
            "retryable": !failed,
            "startup": public_startup(&lifecycle.startup),
            "retry_after_ms": retry_after_ms,
        }
    }));
    if failed {
        return (StatusCode::SERVICE_UNAVAILABLE, body).into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "1")],
        body,
    )
        .into_response()
}

fn public_startup(snapshot: &StartupSnapshot) -> serde_json::Value {
    json!({
        "sequence": snapshot.sequence,
        "phase": snapshot.phase,
        "started_at": &snapshot.started_at,
        "phase_started_at": &snapshot.phase_started_at,
        "updated_at": &snapshot.updated_at,
        "ready_at": &snapshot.ready_at,
        "failed_at": &snapshot.failed_at,
        "elapsed_ms": snapshot.elapsed_ms,
        "chains_total": snapshot.chains_total,
        "chains_done": snapshot.chains_done,
        "threads_restored": snapshot.threads_restored,
        "events_projected": snapshot.events_projected,
        "pending_head_changes": snapshot.pending_head_changes,
        "recovery_threads": snapshot.recovery_threads,
        "message": &snapshot.message,
    })
}

struct CoordinatorState {
    snapshot: StartupSnapshot,
    phase_started: Instant,
    stage: StartupStage,
}

/// Internal monotonic startup ordering. `ReplayingHeadChanges` is visible at
/// three distinct points, so the wire phase alone cannot prove that startup did
/// not regress from follow reconciliation back into projection opening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupStage {
    Bootstrapping,
    OpeningProjection,
    RebuildingProjection,
    InitialHeadReplay,
    RecoveringSchedulerProjection,
    TerminalHeadReplay,
    ReconcilingThreads,
    ReconcilingFollow,
    PostReconcileHeadReplay,
    ReconcilingScheduler,
}

fn advance_startup_stage(current: StartupStage, phase: StartupPhase) -> Result<StartupStage> {
    use StartupPhase as Phase;
    use StartupStage as Stage;

    let next = match (current, phase) {
        (Stage::Bootstrapping, Phase::Bootstrapping) => Stage::Bootstrapping,
        (Stage::Bootstrapping, Phase::OpeningProjection) => Stage::OpeningProjection,

        (Stage::OpeningProjection, Phase::OpeningProjection) => Stage::OpeningProjection,
        (Stage::OpeningProjection, Phase::RebuildingProjection) => Stage::RebuildingProjection,
        (Stage::OpeningProjection, Phase::ReplayingHeadChanges) => Stage::InitialHeadReplay,
        (Stage::OpeningProjection, Phase::RecoveringSchedulerProjection) => {
            Stage::RecoveringSchedulerProjection
        }

        (Stage::RebuildingProjection, Phase::RebuildingProjection) => Stage::RebuildingProjection,
        (Stage::RebuildingProjection, Phase::ReplayingHeadChanges) => Stage::InitialHeadReplay,
        (Stage::RebuildingProjection, Phase::RecoveringSchedulerProjection) => {
            Stage::RecoveringSchedulerProjection
        }

        (Stage::InitialHeadReplay, Phase::ReplayingHeadChanges) => Stage::InitialHeadReplay,
        (Stage::InitialHeadReplay, Phase::RecoveringSchedulerProjection) => {
            Stage::RecoveringSchedulerProjection
        }

        (Stage::RecoveringSchedulerProjection, Phase::RecoveringSchedulerProjection) => {
            Stage::RecoveringSchedulerProjection
        }
        (Stage::RecoveringSchedulerProjection, Phase::ReplayingHeadChanges) => {
            Stage::TerminalHeadReplay
        }

        (Stage::TerminalHeadReplay, Phase::ReplayingHeadChanges) => Stage::TerminalHeadReplay,
        (Stage::TerminalHeadReplay, Phase::ReconcilingThreads) => Stage::ReconcilingThreads,

        (Stage::ReconcilingThreads, Phase::ReconcilingThreads) => Stage::ReconcilingThreads,
        (Stage::ReconcilingThreads, Phase::ReconcilingFollow) => Stage::ReconcilingFollow,

        (Stage::ReconcilingFollow, Phase::ReconcilingFollow) => Stage::ReconcilingFollow,
        (Stage::ReconcilingFollow, Phase::ReplayingHeadChanges) => Stage::PostReconcileHeadReplay,
        (Stage::ReconcilingFollow, Phase::ReconcilingScheduler) => Stage::ReconcilingScheduler,

        (Stage::PostReconcileHeadReplay, Phase::ReplayingHeadChanges) => {
            Stage::PostReconcileHeadReplay
        }
        (Stage::PostReconcileHeadReplay, Phase::ReconcilingScheduler) => {
            Stage::ReconcilingScheduler
        }

        (Stage::ReconcilingScheduler, Phase::ReconcilingScheduler) => Stage::ReconcilingScheduler,
        (_, Phase::Ready | Phase::Failed) => {
            anyhow::bail!("terminal startup phases are published only by ready/failed")
        }
        _ => anyhow::bail!(
            "startup phase regression or invalid reordering from {current:?} to {}",
            phase.as_str()
        ),
    };
    Ok(next)
}

/// Publishes one monotonic lifecycle stream to both stable front doors.
#[derive(Clone)]
pub struct StartupCoordinator {
    identity: LifecycleIdentity,
    process_started: Instant,
    state: Arc<Mutex<CoordinatorState>>,
    publication: Arc<Mutex<()>>,
    shutting_down: Arc<AtomicBool>,
    uds: crate::uds::server::DynamicServerState,
    http: DynamicHttpState,
}

impl StartupCoordinator {
    pub fn bootstrap(identity: LifecycleIdentity, process_started: Instant) -> Result<Self> {
        let startup = StartupSnapshot::bootstrapping(&identity.started_at);
        let lifecycle = LifecycleResponse::starting(identity.clone(), startup.clone());
        let http = DynamicHttpState::bootstrap(lifecycle)?;
        let publication = Arc::new(Mutex::new(()));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let shutdown_http = http.clone();
        let shutdown_publication = publication.clone();
        let shutdown_flag = shutting_down.clone();
        let uds = crate::uds::server::DynamicServerState::bootstrap_with_shutdown(
            LifecycleResponse::starting(identity.clone(), startup.clone()),
            Arc::new(move || {
                let _publication = shutdown_publication.lock();
                shutdown_flag.store(true, Ordering::Release);
                shutdown_http.close_external_admission();
                shutdown_http.unpublish_application();
                ryeosd::request_shutdown();
            }),
        )?;
        Ok(Self {
            identity,
            process_started,
            state: Arc::new(Mutex::new(CoordinatorState {
                snapshot: startup,
                phase_started: Instant::now(),
                stage: StartupStage::Bootstrapping,
            })),
            publication,
            shutting_down,
            uds,
            http,
        })
    }

    pub fn uds_state(&self) -> crate::uds::server::DynamicServerState {
        self.uds.clone()
    }

    pub fn http_state(&self) -> DynamicHttpState {
        self.http.clone()
    }

    pub fn phase(&self, phase: StartupPhase, message: impl Into<String>) -> Result<()> {
        let _publication = self
            .publication
            .lock()
            .map_err(|_| anyhow!("startup publication lock poisoned"))?;
        if self.shutting_down.load(Ordering::Acquire) || ryeosd::shutdown_requested() {
            anyhow::bail!("daemon shutdown began during startup");
        }
        if self.uds.lifecycle().status != LifecycleWireState::Starting {
            anyhow::bail!("startup phase cannot change after terminal publication");
        }
        let now = lillux::time::iso8601_now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("startup coordinator poisoned"))?;
        let previous_stage = state.stage;
        let next_stage = advance_startup_stage(previous_stage, phase)?;
        let previous = state.snapshot.phase;
        let previous_duration = state.phase_started.elapsed();
        let phase_changed = previous != phase;
        state.snapshot.sequence = state.snapshot.sequence.saturating_add(1);
        state.snapshot.phase = phase;
        state.snapshot.updated_at = now;
        state.snapshot.elapsed_ms = self.process_started.elapsed().as_millis() as u64;
        if phase_changed {
            state.snapshot.phase_started_at = state.snapshot.updated_at.clone();
            state.snapshot.chains_total = None;
            state.snapshot.chains_done = None;
            state.snapshot.threads_restored = None;
            state.snapshot.events_projected = None;
            state.snapshot.pending_head_changes = None;
            state.snapshot.recovery_threads = None;
            state.phase_started = Instant::now();
        }
        state.stage = next_stage;
        state.snapshot.message = Some(message.into());
        state.snapshot.error = None;
        let response = LifecycleResponse::starting(self.identity.clone(), state.snapshot.clone());
        drop(state);
        tracing::info!(
            previous_phase = previous.as_str(),
            previous_duration_ms = previous_duration.as_millis() as u64,
            phase = phase.as_str(),
            "daemon startup phase"
        );
        self.publish(response)
    }

    pub fn progress<F>(&self, update: F) -> Result<()>
    where
        F: FnOnce(&mut StartupSnapshot),
    {
        let _publication = self
            .publication
            .lock()
            .map_err(|_| anyhow!("startup publication lock poisoned"))?;
        if self.shutting_down.load(Ordering::Acquire) || ryeosd::shutdown_requested() {
            return Ok(());
        }
        if self.uds.lifecycle().status != LifecycleWireState::Starting {
            return Ok(());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("startup coordinator poisoned"))?;
        let previous = state.snapshot.clone();
        let mut next = previous.clone();
        next.sequence = next.sequence.saturating_add(1);
        next.updated_at = lillux::time::iso8601_now();
        next.elapsed_ms = self.process_started.elapsed().as_millis() as u64;
        update(&mut next);
        if next.phase != previous.phase {
            anyhow::bail!("startup progress cannot change phase; use phase publication");
        }
        validate_progress_transition(&previous, &next)?;
        state.snapshot = next.clone();
        let response = LifecycleResponse::starting(self.identity.clone(), next);
        drop(state);
        self.publish(response)
    }

    fn projection_recovery_progress(
        &self,
        progress: ryeos_state::ProjectionRecoveryProgress,
    ) -> Result<()> {
        let _publication = self
            .publication
            .lock()
            .map_err(|_| anyhow!("startup publication lock poisoned"))?;
        if self.shutting_down.load(Ordering::Acquire) || ryeosd::shutdown_requested() {
            return Ok(());
        }
        if self.uds.lifecycle().status != LifecycleWireState::Starting {
            return Ok(());
        }

        let phase = match progress.stage {
            ryeos_state::ProjectionRecoveryStage::Opening => StartupPhase::OpeningProjection,
            ryeos_state::ProjectionRecoveryStage::Rebuilding => StartupPhase::RebuildingProjection,
            ryeos_state::ProjectionRecoveryStage::ReplayingPending => {
                StartupPhase::ReplayingHeadChanges
            }
        };
        let message = match progress.stage {
            ryeos_state::ProjectionRecoveryStage::Opening => "opening thread projection",
            ryeos_state::ProjectionRecoveryStage::Rebuilding => {
                "building thread projection baseline"
            }
            ryeos_state::ProjectionRecoveryStage::ReplayingPending => {
                "replaying pending chain-head transitions"
            }
        };

        let now = lillux::time::iso8601_now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("startup coordinator poisoned"))?;
        let previous_stage = state.stage;
        let next_stage = advance_startup_stage(previous_stage, phase)?;
        let previous_snapshot = state.snapshot.clone();
        let previous_phase_started = state.phase_started;
        let phase_changed = state.snapshot.phase != phase;
        let previous_phase = state.snapshot.phase;
        let previous_duration = state.phase_started.elapsed();
        if phase_changed {
            state.snapshot.phase = phase;
            state.snapshot.phase_started_at = now.clone();
            state.snapshot.chains_total = None;
            state.snapshot.chains_done = None;
            state.snapshot.threads_restored = None;
            state.snapshot.events_projected = None;
            state.snapshot.pending_head_changes = None;
            state.phase_started = Instant::now();
        }
        state.stage = next_stage;
        state.snapshot.sequence = state.snapshot.sequence.saturating_add(1);
        state.snapshot.updated_at = now;
        state.snapshot.elapsed_ms = self.process_started.elapsed().as_millis() as u64;
        state.snapshot.message = Some(message.into());
        state.snapshot.chains_total = progress.chains_total.map(|value| value as u64);
        let chains_done = state
            .snapshot
            .chains_done
            .unwrap_or_default()
            .max(progress.chains_done as u64);
        let threads_restored = state
            .snapshot
            .threads_restored
            .unwrap_or_default()
            .max(progress.threads_restored as u64);
        let events_projected = state
            .snapshot
            .events_projected
            .unwrap_or_default()
            .max(progress.events_projected as u64);
        state.snapshot.chains_done = Some(chains_done);
        state.snapshot.threads_restored = Some(threads_restored);
        state.snapshot.events_projected = Some(events_projected);
        if let Some(total) = progress.pending_total {
            let remaining = total.saturating_sub(progress.pending_done) as u64;
            // A rebuilt generation performs an unacknowledged staged fold and
            // then an acknowledged installed fold. Both belong to one visible
            // phase, so the second necessary pass must not make progress move
            // backwards. Explicit phase entry remains the only counter reset.
            state.snapshot.pending_head_changes = Some(
                state
                    .snapshot
                    .pending_head_changes
                    .map_or(remaining, |observed| observed.min(remaining)),
            );
        }
        if let Err(error) = validate_progress_transition(&previous_snapshot, &state.snapshot) {
            state.snapshot = previous_snapshot;
            state.phase_started = previous_phase_started;
            state.stage = previous_stage;
            return Err(error);
        }
        let response = LifecycleResponse::starting(self.identity.clone(), state.snapshot.clone());
        drop(state);

        if phase_changed {
            tracing::info!(
                previous_phase = previous_phase.as_str(),
                previous_duration_ms = previous_duration.as_millis() as u64,
                phase = phase.as_str(),
                "daemon startup projection phase"
            );
        }
        self.publish(response)
    }

    /// Refresh elapsed time while a blocking startup unit is running.  This is
    /// intentionally low-rate; callers should use a one-second ticker.
    pub fn refresh(&self) -> Result<()> {
        self.progress(|_| {})
    }

    pub fn publish_application(
        &self,
        app: Arc<ryeos_app::state::AppState>,
        api: ryeos_api::ApiState,
    ) -> Result<()> {
        let _publication = self
            .publication
            .lock()
            .map_err(|_| anyhow!("startup publication lock poisoned"))?;
        if self.shutting_down.load(Ordering::Acquire) || ryeosd::shutdown_requested() {
            anyhow::bail!("cannot publish application after daemon shutdown began");
        }
        if self.uds.lifecycle().status != LifecycleWireState::Starting {
            anyhow::bail!("cannot publish application after terminal lifecycle publication");
        }
        if self.uds.application_is_published() || self.http.application_is_published() {
            anyhow::bail!("application surfaces may be published exactly once");
        }
        // Runtime callbacks use AppState; HTTP routes use the richer ApiState.
        // Both pointers are release-published before readiness can be observed.
        self.uds.publish_application(app);
        self.http.publish_application(api);
        Ok(())
    }

    pub fn ready(&self, thread_projection: serde_json::Value) -> Result<()> {
        let _publication = self
            .publication
            .lock()
            .map_err(|_| anyhow!("startup publication lock poisoned"))?;
        if self.shutting_down.load(Ordering::Acquire) || ryeosd::shutdown_requested() {
            anyhow::bail!("cannot publish Ready after daemon shutdown began");
        }
        if self.uds.lifecycle().status != LifecycleWireState::Starting {
            anyhow::bail!("Ready may be published exactly once");
        }
        if !self.uds.application_is_published() || !self.http.application_is_published() {
            anyhow::bail!("cannot publish Ready before both application surfaces");
        }
        let ready_at = lillux::time::iso8601_now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("startup coordinator poisoned"))?;
        if state.stage != StartupStage::ReconcilingScheduler
            || state.snapshot.phase != StartupPhase::ReconcilingScheduler
        {
            anyhow::bail!("Ready requires completed scheduler reconciliation");
        }
        let previous_phase = state.snapshot.phase;
        let previous_duration = state.phase_started.elapsed();
        state.snapshot.elapsed_ms = self.process_started.elapsed().as_millis() as u64;
        state.snapshot.message = Some("daemon recovery complete".into());
        let mut response =
            LifecycleResponse::running(self.identity.clone(), ready_at, state.snapshot.clone());
        response.thread_projection = Some(thread_projection);
        state.snapshot = response.startup.clone();
        drop(state);

        // Admission additionally checks lifecycle.ready, so opening this gate
        // first cannot admit a request before the Ready publication lands.
        self.http.open_external_admission();
        self.publish(response)?;
        tracing::info!(
            previous_phase = previous_phase.as_str(),
            previous_duration_ms = previous_duration.as_millis() as u64,
            elapsed_ms = self.process_started.elapsed().as_millis() as u64,
            "daemon startup ready"
        );
        Ok(())
    }

    /// Close every application admission path before runtime drain. Lifecycle
    /// status remains published on the stable bootstrap transports until their
    /// supervised shutdown completes.
    pub fn begin_shutdown(&self) {
        let _publication = self.publication.lock();
        self.shutting_down.store(true, Ordering::Release);
        self.http.close_external_admission();
        self.http.unpublish_application();
        self.uds.unpublish_application();
    }

    pub fn failed(&self, error: &anyhow::Error) -> Result<()> {
        let _publication = self
            .publication
            .lock()
            .map_err(|_| anyhow!("startup publication lock poisoned"))?;
        if self.uds.lifecycle().status != LifecycleWireState::Starting {
            return Ok(());
        }
        let failed_at = lillux::time::iso8601_now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("startup coordinator poisoned"))?;
        let previous_phase = state.snapshot.phase;
        let previous_duration = state.phase_started.elapsed();
        state.snapshot.elapsed_ms = self.process_started.elapsed().as_millis() as u64;
        let failure_detail = format!("{error:#}");
        state.snapshot.message = Some("daemon initialization failed".into());
        let response = LifecycleResponse::failed(
            self.identity.clone(),
            failed_at,
            failure_detail,
            state.snapshot.clone(),
        );
        state.snapshot = response.startup.clone();
        drop(state);
        tracing::error!(
            error = %error,
            previous_phase = previous_phase.as_str(),
            previous_duration_ms = previous_duration.as_millis() as u64,
            "daemon startup failed"
        );
        self.publish(response)
    }

    pub fn is_starting(&self) -> bool {
        self.uds.lifecycle().status == LifecycleWireState::Starting
    }

    fn publish(&self, response: LifecycleResponse) -> Result<()> {
        let uds_current = self.uds.lifecycle();
        let http_current = self.http.lifecycle();
        if *uds_current != *http_current {
            anyhow::bail!("HTTP and UDS lifecycle publications diverged");
        }
        self.uds.publish_lifecycle(response.clone())?;
        self.http.publish_lifecycle(response)
    }
}

fn validate_progress_transition(previous: &StartupSnapshot, next: &StartupSnapshot) -> Result<()> {
    if next.sequence <= previous.sequence {
        anyhow::bail!("startup progress sequence must increase");
    }
    if next.elapsed_ms < previous.elapsed_ms {
        anyhow::bail!("startup elapsed time cannot move backwards");
    }
    if let (Some(done), Some(total)) = (next.chains_done, next.chains_total) {
        if done > total {
            anyhow::bail!("startup chains_done cannot exceed chains_total");
        }
    }
    if next.phase != previous.phase {
        return Ok(());
    }
    if next.phase_started_at != previous.phase_started_at {
        anyhow::bail!("startup phase_started_at cannot change within a phase");
    }

    require_non_decreasing("chains_total", previous.chains_total, next.chains_total)?;
    require_non_decreasing("chains_done", previous.chains_done, next.chains_done)?;
    require_non_decreasing(
        "threads_restored",
        previous.threads_restored,
        next.threads_restored,
    )?;
    require_non_decreasing(
        "events_projected",
        previous.events_projected,
        next.events_projected,
    )?;
    require_non_decreasing(
        "recovery_threads",
        previous.recovery_threads,
        next.recovery_threads,
    )?;
    require_non_increasing(
        "pending_head_changes",
        previous.pending_head_changes,
        next.pending_head_changes,
    )?;
    Ok(())
}

fn require_non_decreasing(name: &str, previous: Option<u64>, next: Option<u64>) -> Result<()> {
    match (previous, next) {
        (Some(_), None) => anyhow::bail!("startup {name} cannot disappear within a phase"),
        (Some(previous), Some(next)) if next < previous => {
            anyhow::bail!("startup {name} cannot decrease within a phase")
        }
        _ => Ok(()),
    }
}

fn require_non_increasing(name: &str, previous: Option<u64>, next: Option<u64>) -> Result<()> {
    match (previous, next) {
        (Some(_), None) => anyhow::bail!("startup {name} cannot disappear within a phase"),
        (Some(previous), Some(next)) if next > previous => {
            anyhow::bail!("startup {name} cannot increase within a phase")
        }
        _ => Ok(()),
    }
}

impl ryeos_state::ProjectionRecoveryObserver for StartupCoordinator {
    fn update(&self, progress: ryeos_state::ProjectionRecoveryProgress) {
        if let Err(error) = self.projection_recovery_progress(progress) {
            tracing::warn!(%error, "failed to publish projection recovery progress");
        }
    }

    fn is_cancelled(&self) -> bool {
        ryeosd::shutdown_requested()
    }
}

/// Keep elapsed startup time fresh while a long blocking open/rebuild runs.
pub async fn progress_ticker(coordinator: StartupCoordinator) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;
    loop {
        tick.tick().await;
        if !coordinator.is_starting() {
            return;
        }
        if let Err(error) = coordinator.refresh() {
            tracing::warn!(%error, "failed to refresh startup lifecycle progress");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_cleanup_removes_only_the_socket_inode_it_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let uds_path = tmp.path().join("ryeosd.sock");
        let daemon_json_path = tmp.path().join("daemon.json");
        let original = std::os::unix::net::UnixListener::bind(&uds_path).unwrap();
        let cleanup = DiscoveryCleanup::new(uds_path.clone(), daemon_json_path).unwrap();

        std::fs::remove_file(&uds_path).unwrap();
        let replacement = std::os::unix::net::UnixListener::bind(&uds_path).unwrap();
        drop(cleanup);

        assert!(uds_path.exists());
        drop(replacement);
        drop(original);
    }

    fn next_progress(previous: &StartupSnapshot) -> StartupSnapshot {
        let mut next = previous.clone();
        next.sequence += 1;
        next.elapsed_ms += 1;
        next.updated_at = "2026-07-14T00:00:01Z".to_owned();
        next
    }

    #[test]
    fn progress_rejects_regressing_or_disappearing_counters() {
        let mut previous = StartupSnapshot::bootstrapping("2026-07-14T00:00:00Z");
        previous.chains_done = Some(2);

        let mut regressed = next_progress(&previous);
        regressed.chains_done = Some(1);
        assert!(validate_progress_transition(&previous, &regressed).is_err());

        let mut disappeared = next_progress(&previous);
        disappeared.chains_done = None;
        assert!(validate_progress_transition(&previous, &disappeared).is_err());
    }

    #[test]
    fn progress_rejects_done_beyond_total() {
        let previous = StartupSnapshot::bootstrapping("2026-07-14T00:00:00Z");
        let mut next = next_progress(&previous);
        next.chains_total = Some(2);
        next.chains_done = Some(3);
        assert!(validate_progress_transition(&previous, &next).is_err());
    }

    #[test]
    fn pending_head_count_may_only_count_down_within_phase() {
        let mut previous = StartupSnapshot::bootstrapping("2026-07-14T00:00:00Z");
        previous.pending_head_changes = Some(2);

        let mut decreased = next_progress(&previous);
        decreased.pending_head_changes = Some(1);
        validate_progress_transition(&previous, &decreased).unwrap();

        let mut increased = next_progress(&previous);
        increased.pending_head_changes = Some(3);
        assert!(validate_progress_transition(&previous, &increased).is_err());
    }

    #[test]
    fn a_new_phase_may_reset_progress_counters() {
        let mut previous = StartupSnapshot::bootstrapping("2026-07-14T00:00:00Z");
        previous.chains_done = Some(2);
        let mut next = next_progress(&previous);
        next.phase = StartupPhase::OpeningProjection;
        next.phase_started_at = next.updated_at.clone();
        next.chains_done = None;
        validate_progress_transition(&previous, &next).unwrap();
    }

    #[test]
    fn startup_stage_allows_the_documented_rebuild_and_post_reconcile_replay() {
        let mut stage = StartupStage::Bootstrapping;
        for phase in [
            StartupPhase::OpeningProjection,
            StartupPhase::RebuildingProjection,
            StartupPhase::ReplayingHeadChanges,
            StartupPhase::RecoveringSchedulerProjection,
            StartupPhase::ReplayingHeadChanges,
            StartupPhase::ReconcilingThreads,
            StartupPhase::ReconcilingFollow,
            StartupPhase::ReplayingHeadChanges,
            StartupPhase::ReconcilingScheduler,
        ] {
            stage = advance_startup_stage(stage, phase).unwrap();
        }
        assert_eq!(stage, StartupStage::ReconcilingScheduler);
    }

    #[test]
    fn startup_stage_rejects_phase_regression_and_reordering() {
        assert!(advance_startup_stage(
            StartupStage::InitialHeadReplay,
            StartupPhase::OpeningProjection,
        )
        .is_err());
        assert!(advance_startup_stage(
            StartupStage::RecoveringSchedulerProjection,
            StartupPhase::ReconcilingThreads,
        )
        .is_err());
        assert!(advance_startup_stage(
            StartupStage::ReconcilingFollow,
            StartupPhase::ReconcilingThreads,
        )
        .is_err());
        assert!(advance_startup_stage(
            StartupStage::ReconcilingScheduler,
            StartupPhase::ReplayingHeadChanges,
        )
        .is_err());
    }
}
