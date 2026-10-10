//! Meaning-blind daemon ownership for callback-free persistent subprocesses.
//!
//! Kinds and domain adapters own frame-body meaning.  This module owns only
//! exact pool identity, bounded framing, serial request correlation,
//! cancellation, readiness, reuse, idle retirement, and process teardown.

use lillux::time::{Duration, MonotonicDeadline};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_node_runtime::node_policy::sections::persistent_sessions::PersistentSessionPoolLimits;

use ryeos_state::objects::{PersistentSessionLifecycleContract, PersistentSessionWireContract};

const IO_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STREAM_BACKLOG_EVENTS: usize = 128;
const TERMINAL_STREAM_RETENTION_MS: u64 = 5 * 60 * 1_000;
const ABANDONED_STREAM_RETENTION_MS: u64 = 2 * 60 * 60 * 1_000;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static STREAM_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static RETIREMENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PersistentSessionFrame {
    pub protocol: String,
    pub version: u32,
    pub kind: PersistentSessionFrameKind,
    pub request_id: Option<String>,
    pub body: Option<Value>,
}

impl<'de> Deserialize<'de> for PersistentSessionFrame {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ExactFrame {
            protocol: String,
            version: u32,
            kind: PersistentSessionFrameKind,
            request_id: RequiredNullable<String>,
            body: RequiredNullable<Value>,
        }

        let exact = ExactFrame::deserialize(deserializer)?;
        Ok(Self {
            protocol: exact.protocol,
            version: exact.version,
            kind: exact.kind,
            request_id: exact.request_id.0,
            body: exact.body.0,
        })
    }
}

struct RequiredNullable<T>(Option<T>);

impl<'de, T> Deserialize<'de> for RequiredNullable<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistentSessionFrameKind {
    Ready,
    Request,
    /// Daemon-owned authority control. Public opaque commands cannot select
    /// this frame kind.
    Control,
    Delta,
    Final,
    Error,
    Cancel,
    ObservationBatch,
    ObservationAck,
}

/// Path-free result of read-only admission-policy projection. Raw cleanup or
/// process diagnostics remain operator-only and must never cross validation
/// or remote-execution response boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistentSessionContractEligibility {
    Ready,
    Disabled,
    Shutdown,
    ResourceRequestExceedsPolicy,
    WireFrameExceedsPolicy,
    CleanupUnproved,
}

impl PersistentSessionContractEligibility {
    pub fn code(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Disabled => "disabled",
            Self::Shutdown => "shutdown",
            Self::ResourceRequestExceedsPolicy => "resource_request_exceeds_policy",
            Self::WireFrameExceedsPolicy => "wire_frame_exceeds_policy",
            Self::CleanupUnproved => "cleanup_unproved",
        }
    }

    pub fn ready_for_admission(self) -> bool {
        self == Self::Ready
    }
}

pub struct StartedPersistentSession {
    pub running: ryeos_engine::dispatch::RunningExecution,
    pub socket: lillux::InheritedDuplexChannel,
    /// Descriptor-backed workspace/content leases owned for exactly the
    /// process lifetime. Their concrete types remain outside pool semantics.
    pub lifelines: Vec<Box<dyn Send + Sync>>,
    /// Lock-free health snapshot shared with the ordinary relay pump.
    pub relay_health: Option<PersistentSessionRelayHealthCell>,
    /// Optional typed owner for an ordinary external relay pump. Its health
    /// participates in request admission and idle retirement; it is joined
    /// before process reap or cleanup lifeline release.
    pub relay_owner: Option<Arc<dyn PersistentSessionRelayOwner>>,
    /// When present, readiness must echo this exact daemon-minted identity in
    /// `{ "boot_identity": ... }`. Pooled request workers omit it.
    pub expected_boot_identity: Option<String>,
    /// Optional daemon-owned observation authority installed before the
    /// post-readiness reader is started. Workers never receive this closure;
    /// they can only submit bounded protocol frames for it to validate.
    pub observation_sink: Option<PersistentSessionObservationSink>,
    /// One-shot settlement owned by the same process registry. It is invoked
    /// only after checked reap/death proof, never from request completion.
    pub cleanup_observer: Option<PersistentSessionCleanupObserver>,
    /// Non-financial launch-authority settlement after checked process reap.
    /// Failure prevents successful retirement independently of resource accounting.
    pub retirement_observer: Option<PersistentSessionCleanupObserver>,
    /// Finite resource occupancy authority for this exact process occurrence.
    /// `None` is valid only for sessions without a financially bounded
    /// selected execution resource.
    pub resource_occupancy_limit: Option<lillux::time::OccupancyLimit>,
    /// Exact node-policy cleanup reserve captured with this process owner.
    pub resource_cleanup_allowance_ms: Option<u64>,
    pub resource_attribution_sink: Option<PersistentSessionResourceAttributionSink>,
    /// Live eligibility of the exact resident owner gate. Checked for every
    /// request after leasing and before any worker contact.
    pub resource_eligibility: Option<PersistentSessionResourceEligibility>,
}

pub type PersistentSessionObservationSink =
    Arc<dyn Fn(Value) -> Result<Value> + Send + Sync + 'static>;
pub type PersistentSessionCleanupObserver = Arc<dyn Fn() -> Result<()> + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistentSessionRelayFailure {
    ContactFailed,
    PumpPanicked,
    SignedFrameBudgetExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistentSessionRelayHealth {
    Live,
    Failed(PersistentSessionRelayFailure),
    Stopped,
}

const RELAY_HEALTH_LIVE: u8 = 0;
const RELAY_HEALTH_CONTACT_FAILED: u8 = 1;
const RELAY_HEALTH_PUMP_PANICKED: u8 = 2;
const RELAY_HEALTH_FRAME_BUDGET_EXHAUSTED: u8 = 3;
const RELAY_HEALTH_STOPPED: u8 = 4;

#[derive(Clone)]
pub struct PersistentSessionRelayHealthCell(Arc<AtomicU8>);

impl PersistentSessionRelayHealthCell {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(RELAY_HEALTH_LIVE)))
    }

    pub fn snapshot(&self) -> PersistentSessionRelayHealth {
        match self.0.load(Ordering::Acquire) {
            RELAY_HEALTH_CONTACT_FAILED => {
                PersistentSessionRelayHealth::Failed(PersistentSessionRelayFailure::ContactFailed)
            }
            RELAY_HEALTH_PUMP_PANICKED => {
                PersistentSessionRelayHealth::Failed(PersistentSessionRelayFailure::PumpPanicked)
            }
            RELAY_HEALTH_FRAME_BUDGET_EXHAUSTED => PersistentSessionRelayHealth::Failed(
                PersistentSessionRelayFailure::SignedFrameBudgetExhausted,
            ),
            RELAY_HEALTH_STOPPED => PersistentSessionRelayHealth::Stopped,
            _ => PersistentSessionRelayHealth::Live,
        }
    }

    pub fn fail(&self, failure: PersistentSessionRelayFailure) {
        let failed = match failure {
            PersistentSessionRelayFailure::ContactFailed => RELAY_HEALTH_CONTACT_FAILED,
            PersistentSessionRelayFailure::PumpPanicked => RELAY_HEALTH_PUMP_PANICKED,
            PersistentSessionRelayFailure::SignedFrameBudgetExhausted => {
                RELAY_HEALTH_FRAME_BUDGET_EXHAUSTED
            }
        };
        let mut current = self.0.load(Ordering::Acquire);
        while current == RELAY_HEALTH_LIVE || current == RELAY_HEALTH_STOPPED {
            match self
                .0
                .compare_exchange_weak(current, failed, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    pub fn stop(&self) {
        let _ = self.0.compare_exchange(
            RELAY_HEALTH_LIVE,
            RELAY_HEALTH_STOPPED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// Typed supervision edge for the one ordinary external relay owned by a
/// persistent session. Implementations must make health reads nonblocking and
/// stop/join without holding an owner or pool lock.
pub trait PersistentSessionRelayOwner: Send + Sync + 'static {
    fn stop_and_join(&self) -> Result<()>;
}

pub struct PersistentSessionResourceRequestLease {
    finish: Option<Box<dyn FnOnce() -> Result<()> + Send + 'static>>,
}

impl PersistentSessionResourceRequestLease {
    pub fn new(finish: impl FnOnce() -> Result<()> + Send + 'static) -> Self {
        Self {
            finish: Some(Box::new(finish)),
        }
    }

    pub fn finish(mut self) -> Result<()> {
        self.finish
            .take()
            .expect("resource request lease finishes once")()
    }
}

impl Drop for PersistentSessionResourceRequestLease {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take()
            && let Err(error) = finish()
        {
            tracing::error!(error = %format!("{error:#}"), "resident resource request lease cleanup failed");
        }
    }
}

pub type PersistentSessionResourceEligibility =
    Arc<dyn Fn() -> Result<PersistentSessionResourceRequestLease> + Send + Sync + 'static>;
pub type PersistentSessionResourceAttributionSink = Arc<
    dyn Fn(
            &PersistentSessionRequestIdentity,
            &lillux::time::OccupancyCoordinate,
            &lillux::time::OccupancyCoordinate,
        ) -> Result<()>
        + Send
        + Sync
        + 'static,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentSessionRequestIdentity {
    pub thread_id: String,
    pub request_digest: ryeos_accounting::HexDigest,
}

struct BudgetedSessionFrame {
    frame: PersistentSessionFrame,
    _budget: BacklogBytePermit,
}

enum RetirementState {
    Open,
    InProgress(u64),
    Reaped,
    Unproved(String),
}

struct RetirementCoordinator {
    state: Mutex<RetirementState>,
    changed: Condvar,
}

impl RetirementCoordinator {
    fn new() -> Self {
        Self {
            state: Mutex::new(RetirementState::Open),
            changed: Condvar::new(),
        }
    }

    fn poison_guard(
        &self,
        process: &SessionProcess,
        mut state: MutexGuard<'_, RetirementState>,
    ) -> String {
        let reason = "persistent-session retirement coordinator was poisoned".to_owned();
        *state = RetirementState::Unproved(reason.clone());
        process.closed.store(true, Ordering::Release);
        drop(state);
        process.record_cleanup_unproved(&reason);
        self.changed.notify_all();
        reason
    }
}

struct RetirementAttempt<'a> {
    process: &'a SessionProcess,
    token: u64,
    complete: bool,
}

impl RetirementAttempt<'_> {
    fn finish(mut self, outcome: std::result::Result<(), String>) -> Result<()> {
        let mut state = match self.process.retirement.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                let reason = "persistent-session retirement coordinator was poisoned".to_owned();
                *state = RetirementState::Unproved(reason.clone());
                drop(state);
                self.process.record_cleanup_unproved(&reason);
                self.process.retirement.changed.notify_all();
                self.complete = true;
                return Err(anyhow!(reason));
            }
        };
        match &*state {
            RetirementState::InProgress(token) if *token == self.token => {}
            _ => bail!("persistent-session retirement token lost ownership"),
        }
        let failure = outcome.err();
        *state = failure
            .as_ref()
            .map_or(RetirementState::Reaped, |reason| {
                RetirementState::Unproved(reason.clone())
            });
        drop(state);
        if let Some(reason) = &failure {
            self.process.record_cleanup_unproved(reason);
        }
        self.complete = true;
        self.process.retirement.changed.notify_all();
        match failure {
            Some(reason) => Err(anyhow!(reason)),
            None => Ok(()),
        }
    }
}

impl Drop for RetirementAttempt<'_> {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        let reason = "persistent-session retirement unwound before proof".to_owned();
        self.process.record_cleanup_unproved(&reason);
        let (mut state, poisoned) = match self.process.retirement.state.lock() {
            Ok(state) => (state, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        };
        if poisoned
            || matches!(&*state, RetirementState::InProgress(token) if *token == self.token)
        {
            *state = RetirementState::Unproved(reason);
        }
        self.process.retirement.changed.notify_all();
    }
}

struct SessionProcess {
    wire: PersistentSessionWireContract,
    /// The exact identity already validated by exclusive readiness, retained
    /// so capture can compare the pool owner with its durable worker record.
    expected_boot_identity: Option<String>,
    writer: Mutex<lillux::InheritedDuplexChannel>,
    reader: Mutex<Option<SessionChannel>>,
    pending: Mutex<HashMap<String, SyncSender<std::result::Result<BudgetedSessionFrame, String>>>>,
    observation_sender: Mutex<Option<SyncSender<BudgetedSessionFrame>>>,
    initial_observation_sink: Mutex<Option<PersistentSessionObservationSink>>,
    reader_failure: Mutex<Option<String>>,
    running: Mutex<Option<ryeos_engine::dispatch::RunningExecution>>,
    /// Once ownership was consumed by an abort attempt whose reap proof
    /// failed, absence of `running` must never be reinterpreted as proof.
    cleanup_unproved: Mutex<Option<String>>,
    cleanup_observer: Mutex<Option<PersistentSessionCleanupObserver>>,
    relay_owner: Mutex<Option<Arc<dyn PersistentSessionRelayOwner>>>,
    relay_health: Option<PersistentSessionRelayHealthCell>,
    retirement: RetirementCoordinator,
    resource_occupancy_limit: Option<lillux::time::OccupancyLimit>,
    resource_cleanup_allowance_ms: Option<u64>,
    resource_attribution_sink: Option<PersistentSessionResourceAttributionSink>,
    resource_eligibility: Option<PersistentSessionResourceEligibility>,
    leased: AtomicBool,
    last_used_ms: AtomicU64,
    closed: Arc<AtomicBool>,
    backlog: Arc<BacklogBudget>,
    /// One maximum wire body is reserved for the reader's raw decode buffer.
    /// Decoded frames acquire additional exact serialized-byte permits before
    /// they can leave the reader thread.
    _reader_budget: BacklogBytePermit,
    /// Retired explicitly after proved process cleanup, even when callers
    /// retain another Arc to this session. Not evidence of process death.
    lifelines: Mutex<Vec<Box<dyn Send + Sync>>>,
    pooled_start: Option<Arc<PersistentSessionPooledStart>>,
}

const MAX_PENDING_SESSION_REQUESTS: usize = 32;

struct SessionChannel {
    socket: lillux::InheritedDuplexChannel,
    reader: FrameReader,
}

#[derive(Default)]
struct FrameReader {
    length: [u8; 4],
    length_read: usize,
    body: Vec<u8>,
    body_read: usize,
}

impl SessionProcess {
    fn begin_retirement(&self) -> Result<Option<RetirementAttempt<'_>>> {
        loop {
            let mut state = match self.retirement.state.lock() {
                Ok(state) => state,
                Err(poisoned) => {
                    let reason = self.retirement.poison_guard(self, poisoned.into_inner());
                    bail!("{reason}");
                }
            };
            match &*state {
                RetirementState::Open => {
                    let token = RETIREMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                    *state = RetirementState::InProgress(token);
                    self.closed.store(true, Ordering::Release);
                    self.backlog.changed.notify_all();
                    drop(state);
                    return Ok(Some(RetirementAttempt {
                        process: self,
                        token,
                        complete: false,
                    }));
                }
                RetirementState::InProgress(_) => {
                    state = match self.retirement.changed.wait(state) {
                        Ok(state) => state,
                        Err(poisoned) => {
                            let reason = self
                                .retirement
                                .poison_guard(self, poisoned.into_inner());
                            bail!("{reason}");
                        }
                    };
                    drop(state);
                }
                RetirementState::Reaped => return Ok(None),
                RetirementState::Unproved(reason) => {
                    bail!("persistent-session cleanup remains unproved: {reason}")
                }
            }
        }
    }

    fn record_cleanup_unproved(&self, reason: &str) {
        let mut slot = self
            .cleanup_unproved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.get_or_insert_with(|| reason.to_owned());
    }

    fn relay_health(&self) -> PersistentSessionRelayHealth {
        self.relay_health
            .as_ref()
            .map_or(PersistentSessionRelayHealth::Live, |health| health.snapshot())
    }

    fn relay_failed(&self) -> bool {
        matches!(self.relay_health(), PersistentSessionRelayHealth::Failed(_))
    }

    fn release_lifelines_after_proof(&self) -> Result<()> {
        let lifelines = std::mem::take(
            &mut *self
                .lifelines
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let mut lifelines = lifelines.into_iter();
        while let Some(lifeline) = lifelines.next() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(lifeline))).is_err() {
                let remaining = lifelines.collect::<Vec<_>>();
                self.lifelines
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .extend(remaining);
                bail!("persistent-session lifeline destructor unwound after checked reap")
            }
        }
        Ok(())
    }

    fn ensure_relay_live(&self) -> Result<()> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "persistent-session owner is closed for retirement"
        );
        match self.relay_health() {
            PersistentSessionRelayHealth::Live => Ok(()),
            PersistentSessionRelayHealth::Failed(cause) => {
                bail!("persistent-session ordinary relay failed: {cause:?}")
            }
            PersistentSessionRelayHealth::Stopped => {
                bail!("persistent-session ordinary relay stopped before owner retirement")
            }
        }
    }

    fn start_reader(self: &Arc<Self>, wire: PersistentSessionWireContract) -> Result<()> {
        if let Some(sink) = self
            .initial_observation_sink
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            self.install_observation_sink(sink)?;
        }
        let channel = self
            .reader
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .ok_or_else(|| anyhow!("persistent-session reader already started"))?;
        let process = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("ryeos-persistent-session-reader".to_owned())
            .spawn(move || run_session_reader(process, channel, wire))
            .context("spawn persistent-session reader")?;
        Ok(())
    }

    fn register_request(
        &self,
        request_id: &str,
    ) -> Result<Receiver<std::result::Result<BudgetedSessionFrame, String>>> {
        // One queued response per request is enough to decouple the dedicated
        // reader without multiplying a signed maximum frame by 64. Every
        // queued frame is also charged to the shared node byte budget.
        let (sender, receiver) = sync_channel(1);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.len() >= MAX_PENDING_SESSION_REQUESTS {
            bail!("persistent-session pending request bound is exhausted");
        }
        if pending.insert(request_id.to_owned(), sender).is_some() {
            bail!("persistent-session request identity was reused");
        }
        Ok(receiver)
    }

    fn unregister_request(&self, request_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(request_id);
    }

    fn write(
        &self,
        wire: &PersistentSessionWireContract,
        frame: &PersistentSessionFrame,
        deadline: MonotonicDeadline,
    ) -> Result<()> {
        if let Some(reason) = self
            .reader_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_deref()
        {
            bail!("persistent-session reader failed: {reason}");
        }
        let mut writer = lock_writer_before_deadline(&self.writer, deadline)?;
        write_frame(&mut *writer, wire, frame, deadline)
    }

    fn install_observation_sink(
        self: &Arc<Self>,
        sink: PersistentSessionObservationSink,
    ) -> Result<()> {
        let (sender, receiver) = sync_channel(1);
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("ryeos-persistent-session-observation-ingest".to_owned())
            .spawn(move || run_observation_ingest(weak, receiver, sink))
            .context("spawn bounded persistent-session observation ingest")?;
        {
            let mut slot = self
                .observation_sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.is_some() {
                bail!("persistent-session observation sink is already installed");
            }
            *slot = Some(sender);
        }
        Ok(())
    }

    fn deliver_observation(
        &self,
        frame: BudgetedSessionFrame,
        sink: &PersistentSessionObservationSink,
    ) -> Result<()> {
        let body = frame
            .frame
            .body
            .ok_or_else(|| anyhow!("observation batch has no body"))?;
        let acknowledgement = sink(body)?;
        let wire = self.wire.clone();
        self.write(
            &wire,
            &PersistentSessionFrame {
                protocol: wire.wire_protocol.clone(),
                version: wire.wire_version,
                kind: PersistentSessionFrameKind::ObservationAck,
                request_id: None,
                body: Some(acknowledgement),
            },
            MonotonicDeadline::after(Duration::from_secs(30)),
        )
    }

    fn failure_diagnostic(&self) -> Option<String> {
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let selected_original = self
            .pooled_start
            .as_ref()
            .is_some_and(|start| start.has_original_trusted_obligation());
        let owned = running.take()?;
        if selected_original || owned.has_selected_original_launch_context() {
            use ryeos_engine::dispatch::SelectedNativeExecutionWaitOutcome;
            let owner =
                match owned.wait_for_natural_exit_retaining_native(Duration::from_millis(200)) {
                    Ok(SelectedNativeExecutionWaitOutcome::Collected(owner))
                    | Ok(SelectedNativeExecutionWaitOutcome::Incomplete(owner))
                    | Ok(SelectedNativeExecutionWaitOutcome::Failed(owner))
                    | Err(owner) => owner,
                };
            // Reinstall before borrowing or formatting any diagnostics.
            *running = Some(owner);
            return running.as_ref().and_then(|owner| {
                // The collector may have consumed the live capture into its
                // retained drains/result. Borrow those bytes from the rooted
                // owner, bounding BEFORE allocating a lossy diagnostic string.
                let retained = owner.native_terminal_raw_view().and_then(|view| {
                    view.failure
                        .filter(|failure| !failure.is_empty())
                        .map(str::as_bytes)
                        .or_else(|| {
                            view.terminal_result
                                .filter(|result| !result.stderr.is_empty())
                                .map(|result| result.stderr.as_bytes())
                        })
                        .or_else(|| {
                            view.retained_drains
                                .map(|(_, _, stderr, _)| stderr)
                                .filter(|stderr| !stderr.is_empty())
                        })
                });
                if let Some(bytes) = retained {
                    const MAX_DIAGNOSTIC_BYTES: usize = 2_048;
                    let tail = &bytes[bytes.len().saturating_sub(MAX_DIAGNOSTIC_BYTES)..];
                    Some(String::from_utf8_lossy(tail).into_owned())
                } else {
                    owner.stderr_diagnostic_tail()
                }
            });
        }
        match owned.wait_for_natural_exit(Duration::from_millis(200)) {
            Ok(completion) => Some(process_completion_diagnostic(&completion)),
            Err(still_running) => {
                let diagnostic = still_running.stderr_diagnostic_tail();
                *running = Some(still_running);
                diagnostic
            }
        }
    }

    fn retire(&self) -> Result<()> {
        let Some(attempt) = self.begin_retirement()? else {
            return Ok(());
        };
        if let Some(start) = &self.pooled_start {
            if start.has_original_trusted_obligation() {
                start.retain_unsettled();
                return attempt.finish(Err(
                    "ordinary session retirement cannot settle original trusted driver funding"
                        .to_owned(),
                ));
            }
        }
        self.observation_sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();

        let prior_unproved = {
            self.cleanup_unproved
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        };
        if let Some(reason) = prior_unproved {
            return attempt.finish(Err(format!(
                "persistent-session cleanup remains unproved: {reason}"
            )));
        }

        // Clone the typed owner under its slot lock, then stop and join with
        // every SessionProcess and registry lock released.
        let relay_owner = self
            .relay_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned();
        if let Err(error) = safe_relay_join(relay_owner.as_ref()) {
            return attempt.finish(Err(format!("ordinary relay stop/join failed: {error:#}")));
        }
        drop(relay_owner);

        let running = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(running) = running else {
            return attempt.finish(Err(
                "persistent-session running owner was absent before checked reap proof".to_owned(),
            ));
        };
        if let Err(error) = safe_checked_reap(running) {
            let reason = format!("persistent-session cleanup could not be proved: {error}");
            return attempt.finish(Err(reason));
        }

        let cleanup_observer = self
            .cleanup_observer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned();
        if let Some(observer) = cleanup_observer {
            let settlement = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer()));
            match settlement {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return attempt.finish(Err(format!(
                        "persistent-session resource settlement remains unproved: {error:#}"
                    )));
                }
                Err(_) => {
                    return attempt.finish(Err(
                        "persistent-session resource settlement observer panicked".to_owned(),
                    ));
                }
            }
            self.cleanup_observer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
        }
        if let Some(start) = &self.pooled_start {
            if let Err(error) = start.prove_cleanup() {
                return attempt.finish(Err(format!(
                    "pooled persistent-session cleanup remains unproved: {error:#}"
                )));
            }
        }

        // Owner destructors run outside the slot lock. If one unwinds, all
        // later owners are restored to this registered process for quarantine.
        if let Err(error) = self.release_lifelines_after_proof() {
            return attempt.finish(Err(format!("{error:#}")));
        }

        // The joined relay object is dropped outside its slot mutex and only
        // after checked process and cleanup settlement.
        let relay_owner = self
            .relay_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(relay_owner))).is_err() {
            return attempt.finish(Err(
                "ordinary relay owner final drop panicked after checked reap; owner consumed".to_owned(),
            ));
        }
        attempt.finish(Ok(()))
    }
}

impl Drop for SessionProcess {
    fn drop(&mut self) {
        if let Some(start) = &self.pooled_start {
            if start.has_original_trusted_obligation() {
                if let Some(running) = self
                    .running
                    .get_mut()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                {
                    start.retain_running_unsettled(running);
                }
                start.retain_unsettled();
                return; // descriptor Drop is not driver/financial settlement
            }
        }
        if self
            .cleanup_unproved
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
        {
            if let Some(start) = &self.pooled_start {
                start.retain_unsettled();
            }
            return;
        }
        self.closed.store(true, Ordering::Release);
        self.backlog.changed.notify_all();
        self.observation_sender
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(running) = self
            .running
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            if let Err(error) = running.abort_and_reap_checked() {
                tracing::error!(%error, "persistent-session drop cleanup could not be proved");
                if let Some(start) = &self.pooled_start {
                    start.retain_unsettled();
                }
                return;
            }
        }
        if let Some(observer) = self
            .cleanup_observer
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            && let Err(error) = observer()
        {
            tracing::error!(%error, "persistent-session resource settlement failed after drop reap");
            if let Some(start) = &self.pooled_start {
                start.retain_unsettled();
            }
            return;
        }
        if let Some(start) = &self.pooled_start {
            if let Err(error) = start.prove_cleanup() {
                tracing::error!(%error, "original session cleanup remains unsettled");
                start.retain_unsettled();
            }
        }
    }
}

fn run_session_reader(
    process: Weak<SessionProcess>,
    mut channel: SessionChannel,
    wire: PersistentSessionWireContract,
) {
    let failure = loop {
        let Some(process) = process.upgrade() else {
            return;
        };
        let next = channel
            .reader
            .read_next(&mut channel.socket, wire.max_frame_bytes);
        let frame = match next {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                lillux::time::sleep(IO_POLL_INTERVAL);
                continue;
            }
            Err(error) => break format!("read response frame: {error:#}"),
        };
        if let Err(error) =
            require_frame_identity(&frame, &wire).and_then(|()| validate_frame_shape(&frame, None))
        {
            break format!("invalid response frame: {error:#}");
        }
        let frame_bytes = match serde_json::to_vec(&frame)
            .ok()
            .and_then(|encoded| encoded.len().checked_add(4))
        {
            Some(bytes) => bytes,
            None => break "response frame byte accounting overflowed".to_owned(),
        };
        let budget = match BacklogBytePermit::reserve_wait(
            Arc::clone(&process.backlog),
            frame_bytes,
            &process.closed,
        ) {
            Ok(budget) => budget,
            Err(error) => break format!("reserve response frame byte budget: {error:#}"),
        };
        let frame = BudgetedSessionFrame {
            frame,
            _budget: budget,
        };
        match frame.frame.kind {
            PersistentSessionFrameKind::Delta
            | PersistentSessionFrameKind::Final
            | PersistentSessionFrameKind::Error => {
                let request_id = frame
                    .frame
                    .request_id
                    .clone()
                    .expect("validated request id");
                let sender = process
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&request_id)
                    .cloned();
                let Some(sender) = sender else {
                    break format!("worker emitted a response for unknown request `{request_id}`");
                };
                if sender.send(Ok(frame)).is_err() {
                    break format!("request `{request_id}` response receiver disappeared");
                }
            }
            PersistentSessionFrameKind::ObservationBatch => {
                let sender = process
                    .observation_sender
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                let Some(sender) = sender else {
                    break "worker emitted an observation without an installed sink".to_owned();
                };
                if sender.send(frame).is_err() {
                    break "observation ingest worker stopped".to_owned();
                }
            }
            PersistentSessionFrameKind::Ready
            | PersistentSessionFrameKind::Request
            | PersistentSessionFrameKind::Control
            | PersistentSessionFrameKind::Cancel
            | PersistentSessionFrameKind::ObservationAck => {
                break "worker emitted a frame forbidden after readiness".to_owned();
            }
        }
    };

    if let Some(process) = process.upgrade() {
        let mut slot = process
            .reader_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(failure.clone());
        }
        drop(slot);
        let pending = process
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for sender in pending.values() {
            let _ = sender.try_send(Err(failure.clone()));
        }
    }
}

fn run_observation_ingest(
    process: Weak<SessionProcess>,
    incoming: Receiver<BudgetedSessionFrame>,
    sink: PersistentSessionObservationSink,
) {
    while let Ok(frame) = incoming.recv() {
        let Some(process) = process.upgrade() else {
            return;
        };
        if let Err(error) = process.deliver_observation(frame, &sink) {
            let failure = format!("persist observation batch: {error:#}");
            let mut slot = process
                .reader_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.is_none() {
                *slot = Some(failure.clone());
            }
            drop(slot);
            let pending = process
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for sender in pending.values() {
                let _ = sender.try_send(Err(failure.clone()));
            }
            return;
        }
    }
}

#[derive(Clone)]
struct GroupContract {
    lifecycle: PersistentSessionLifecycleContract,
    wire: PersistentSessionWireContract,
}

struct SessionGroup {
    contract: GroupContract,
    processes: Vec<Arc<SessionProcess>>,
    starts: Vec<Arc<PersistentSessionPooledStart>>,
}

/// Pool-minted original owner, retained inside the EXISTING SessionGroup before
/// its callback can prepare or contact a process. This is retention, not spawn,
/// resource admission, physical removal, or settlement authority. No decoder,
/// public constructor, pool back-reference or alternate registry exists.
pub struct PersistentSessionPooledStart {
    retained: Mutex<PooledStartRetention>,
    // SAME actual callback owner; readable before selected retention locking.
    original_callback_budget: OnceLock<crate::handler_context::OriginalExecutionBudget>,
    callback_in_flight: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PooledStartPhase {
    Preparing,
    Registered,
    Unsettled,
    Settled,
}

// DATA preparation only; no funding/contact/native readiness. Refused is sticky.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginalManifestPreparationPhase {
    Unattempted,
    UnfundedInFlight,
    Prepared,
    Refused,
}

struct PooledStartRetention {
    // Destroy ALL best-effort process owners before scratch/capture lifelines.
    // Group/pool destruction is never native or financial settlement proof.
    retained_running: Vec<ryeos_engine::dispatch::RunningExecution>,
    original_native_acceptance: Option<OriginalNamespaceReporterAcceptance>,
    original_startup_channel:
        Option<crate::persistent_session_admission::cleanup_channel::TrustedCleanupChannel>,
    quarantined_startup_channels:
        Vec<crate::persistent_session_admission::cleanup_channel::TrustedCleanupChannel>,
    startup_check_attempted: bool,
    startup_check_in_flight: bool,
    startup_check_refused: bool,
    // Only the unique inert checkout guard writes this reserved restoration
    // slot. Competing deposits use existing running/input/channel quarantine.
    original_startup_restored: Option<OriginalStartupRetainedOwners>,
    manifest_preparation: OriginalManifestPreparationPhase,
    // Only the unique inert precontact guard writes this reserved owner slot.
    // Its original inputs stay rooted through funding/physical settlement.
    original_manifest_restored:
        Option<crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs>,
    // Checked-out originals are never replaced by another supplied input.
    quarantined_inputs:
        Vec<crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs>,
    native_check_attempted: bool,
    native_check_in_flight: bool,
    native_check_refused: bool,
    original_held_attached: bool,
    phase: PooledStartPhase,
    lifelines: Vec<Box<dyn Send + Sync>>,
    resource_observer: Option<PersistentSessionCleanupObserver>,
    retirement_observer: Option<PersistentSessionCleanupObserver>,
    // ALL actually supplied closure owners are rooted before fallible checks.
    // Only the separate admitted originals below may execute as obligations.
    obligation_owners: Vec<PersistentSessionCleanupObserver>,
    admission_refused: bool,
    // Opaque SAME input owner and original financial attempt live in the
    // EXISTING start slot, before first possible RuntimeDb/ledger write.
    trusted_inputs: Option<crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs>,
    original_funding: Option<crate::accounting_db::precontact::OriginalFundingOwner>,
    original_contact_attempted: bool,
    original_spawn_permit:
        Option<crate::runtime_db::process_resource_launch::TrustedResourceSpawnPermit>,
    // SAME supplied guard, never reconstructed/downcast from lifelines. Keep
    // this typed reference last so every process/capture owner drops first.
    // Presence is raw custody only, not physical-close or funding authority.
    original_scratch: Option<Arc<crate::temp_dir_guard::TempDirGuard>>,
}

// Historical acceptance from the sole protected Collected branch, beside the
// exact restored execution whose private channel still owns all actual facts.
// No copied frame/receipt/result and no current-generation lease. A later
// retirement consumer must revalidate authority and all independent proofs.
struct OriginalNamespaceReporterAcceptance {
    held_occurrence: Arc<lillux::exec::ProcessLocalHeldOccurrence>,
    _capture: Arc<ryeos_engine::isolation::VerifiedIsolationBackendCapture>,
    original_deadline: lillux::time::MonotonicDeadline,
}

/// Inert original-owner checkout. SAME mutable channel retains its request and
/// candidate internally. No acceptance/Init constructor or production caller.
struct OriginalStartupCheckGuard<'a> {
    running: Option<ryeos_engine::dispatch::RunningExecution>,
    inputs: Option<crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs>,
    channel: Option<crate::persistent_session_admission::cleanup_channel::TrustedCleanupChannel>,
    held_occurrence: Arc<lillux::exec::ProcessLocalHeldOccurrence>,
    original: &'a PersistentSessionPooledStart,
    cancelled: &'a dyn Fn() -> bool,
}

struct OriginalStartupRetainedOwners {
    _running: Option<ryeos_engine::dispatch::RunningExecution>,
    _inputs: Option<crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs>,
    _channel: Option<crate::persistent_session_admission::cleanup_channel::TrustedCleanupChannel>,
}

/// Exclusive unfunded preparation. All partial document owners stay in SAME
/// inputs. Completion only restores DATA custody; native permission stays closed.
struct OriginalManifestPrecontactGuard<'a> {
    inputs: Option<crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs>,
    original: &'a PersistentSessionPooledStart,
    cancelled: &'a dyn Fn() -> bool,
    deadline: lillux::time::MonotonicDeadline,
    scratch: Arc<crate::temp_dir_guard::TempDirGuard>,
}

impl OriginalManifestPrecontactGuard<'_> {
    fn require_exclusive(&self) -> Result<()> {
        let retained = self
            .original
            .lock_original_contact_retention(Some(self.cancelled))?;
        let exclusive = !self.original.retained.is_poisoned()
            && retained.manifest_preparation == OriginalManifestPreparationPhase::UnfundedInFlight
            && retained.original_manifest_restored.is_none()
            && retained.trusted_inputs.is_none()
            && retained
                .original_scratch
                .as_ref()
                .is_some_and(|v| Arc::ptr_eq(v, &self.scratch))
            && !retained.admission_refused
            && retained.phase == PooledStartPhase::Preparing
            && retained.original_funding.is_none()
            && !retained.original_contact_attempted
            && retained.original_spawn_permit.is_none()
            && !retained.original_held_attached
            && retained.retained_running.is_empty()
            && !retained.native_check_attempted
            && !retained.native_check_in_flight
            && !retained.native_check_refused
            && !retained.startup_check_attempted
            && !retained.startup_check_in_flight
            && !retained.startup_check_refused
            && retained.original_startup_restored.is_none()
            && retained.quarantined_inputs.is_empty()
            && retained.quarantined_startup_channels.is_empty()
            && self.original.is_in_flight();
        drop(retained);
        anyhow::ensure!(
            exclusive,
            "original manifest checkout lost exclusive unfunded owners"
        );
        anyhow::ensure!(
            !(self.cancelled)() && !self.deadline.has_elapsed(),
            "original manifest preparation cancelled or expired after ownership check"
        );
        Ok(())
    }

    fn retain_original_manifest_documents(&mut self, state: &crate::state::AppState) -> Result<()> {
        self.require_exclusive()?;
        let budget = self
            .original
            .original_callback_budget
            .get()
            .context("original manifest preparation lacks SAME callback budget")?;
        self.inputs
            .as_mut()
            .context("original manifest inputs checked out")?
            .bind_original_isolation_expectation(state, budget)?;
        self.require_exclusive()?;
        // Cooperative whole-call boundaries ONLY; internal IO is not preempted.
        self.inputs
            .as_mut()
            .context("original manifest inputs checked out")?
            .retain_original_manifest_documents(state)?;
        self.require_exclusive()?;
        let _ = self
            .inputs
            .as_mut()
            .context("original manifest inputs checked out")?
            .original_manifest_documents(state)?;
        self.require_exclusive()?;
        self.inputs
            .as_ref()
            .context("original manifest inputs checked out")?
            .require_original_isolation_expectation(state)?;
        self.require_exclusive()
    }

    fn retain_gpu_loader_closure_inventory(
        &mut self,
        state: &crate::state::AppState,
        roots: ryeos_gpu_loader_owner_contract::BorrowedGpuLoaderRealizationRoots<'_>,
    ) -> Result<()> {
        self.require_exclusive()?;
        let result = self
            .inputs
            .as_mut()
            .context("original GPU loader inventory inputs checked out")?
            .retain_gpu_loader_closure_inventory(state, roots);
        result?;
        self.require_exclusive()
    }

    fn commit_prepared(mut self, state: &crate::state::AppState) -> Result<()> {
        self.require_exclusive()?;
        // Completion validates the actual prepared pair itself; mere checkout
        // cannot call this private operation into a fabricated Prepared phase.
        let _ = self
            .inputs
            .as_mut()
            .context("original prepared inputs checked out")?
            .original_manifest_documents(state)?;
        self.inputs
            .as_ref()
            .context("original prepared inputs checked out")?
            .require_original_isolation_expectation(state)?;
        self.inputs
            .as_mut()
            .context("original prepared inputs checked out")?
            .validate_gpu_loader_inventory_for_restore(state)?;
        self.require_exclusive()?;
        let mut retained = self
            .original
            .lock_original_contact_retention(Some(self.cancelled))?;
        let eligible = !self.original.retained.is_poisoned()
            && !self.deadline.has_elapsed()
            && retained.manifest_preparation == OriginalManifestPreparationPhase::UnfundedInFlight
            && retained.phase == PooledStartPhase::Preparing
            && !retained.admission_refused
            && retained.trusted_inputs.is_none()
            && retained.original_manifest_restored.is_none()
            && retained
                .original_scratch
                .as_ref()
                .is_some_and(|v| Arc::ptr_eq(v, &self.scratch))
            && retained.original_funding.is_none()
            && !retained.original_contact_attempted
            && retained.original_spawn_permit.is_none()
            && !retained.original_held_attached
            && retained.retained_running.is_empty()
            && !retained.native_check_attempted
            && !retained.native_check_in_flight
            && !retained.native_check_refused
            && !retained.startup_check_attempted
            && !retained.startup_check_in_flight
            && !retained.startup_check_refused
            && retained.original_startup_restored.is_none()
            && retained.quarantined_inputs.is_empty()
            && retained.quarantined_startup_channels.is_empty()
            && self.original.is_in_flight();
        if !eligible {
            drop(retained);
            bail!("original manifest restoration lost exclusive unfunded custody");
        }
        // One atomic SAME-slot move. No callback/IO/allocation/owner destruction.
        retained.trusted_inputs = self.inputs.take();
        retained.manifest_preparation = OriginalManifestPreparationPhase::Prepared;
        drop(retained);
        if (self.cancelled)() || self.deadline.has_elapsed() {
            self.original.refuse_manifest_precontact();
            bail!("original manifest preparation cancelled or expired after restoration");
        }
        Ok(())
    }
}

impl Drop for OriginalManifestPrecontactGuard<'_> {
    fn drop(&mut self) {
        if self.inputs.is_none() {
            return;
        }
        // Mandatory custody completion may block beyond original deadline.
        // Only this unique checkout writes the reserved restoration slot.
        let mut retained = self
            .original
            .retained
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        retained.original_manifest_restored = self.inputs.take();
        retained.manifest_preparation = OriginalManifestPreparationPhase::Refused;
        retained.admission_refused = true;
        retained.phase = PooledStartPhase::Unsettled;
    }
}

impl OriginalStartupCheckGuard<'_> {
    fn retain_original_manifest_documents(&mut self, state: &crate::state::AppState) -> Result<()> {
        self.require_exclusive()?;
        // SAME prepared inputs are guard-owned, outside the locked slot.
        // Validation/re-read only: no second producer or fallback sealing.
        self.inputs
            .as_mut()
            .context("actual original inputs checked out")?
            .original_manifest_documents(state)?;
        self.require_exclusive()?;
        // No descriptor export/acceptance/Init, and Drop always restores owners.
        Ok(())
    }
    fn require_exclusive(&self) -> Result<()> {
        // Ownership checks only. Never hold this mutex during source/CAS/live
        // or FD4 operations. Poison retains originals but cannot admit a join.
        let retained = self
            .original
            .lock_original_contact_retention(Some(self.cancelled))?;
        let exclusive = !self.original.retained.is_poisoned()
            && retained.startup_check_attempted
            && retained.startup_check_in_flight
            && !retained.startup_check_refused
            && !retained.admission_refused
            && retained.manifest_preparation == OriginalManifestPreparationPhase::Prepared
            && retained.original_manifest_restored.is_none()
            && !retained.native_check_attempted
            && !retained.native_check_in_flight
            && !retained.native_check_refused
            && retained.original_native_acceptance.is_none()
            && retained.phase == PooledStartPhase::Preparing
            && retained.retained_running.is_empty()
            && retained.trusted_inputs.is_none()
            && retained.original_startup_channel.is_none()
            && retained.original_startup_restored.is_none()
            && retained.quarantined_inputs.is_empty()
            && retained.quarantined_startup_channels.is_empty()
            && retained.original_spawn_permit.is_some()
            && retained.original_contact_attempted
            && retained.original_held_attached
            && self.original.is_in_flight()
            && retained
                .original_funding
                .as_ref()
                .and_then(|funding| funding.acknowledged_held_occurrence())
                .is_some_and(|held| Arc::ptr_eq(held, &self.held_occurrence));
        drop(retained);
        anyhow::ensure!(
            exclusive,
            "original startup checkout lost exclusive actual owners"
        );
        anyhow::ensure!(
            !(self.cancelled)()
                && self
                    .original
                    .original_callback_budget
                    .get()
                    .is_some_and(|budget| !budget.contact_expiry().has_elapsed()),
            "original startup validation cancelled or expired"
        );
        Ok(())
    }

    fn prepare_request_inputs(
        &self,
        state: &crate::state::AppState,
    ) -> Result<crate::persistent_session_admission::cleanup_channel::PreflightInputs> {
        self.require_exclusive()?;
        let inputs = self
            .inputs
            .as_ref()
            .context("original inputs checked out")?;
        let original = inputs.original_startup_request_inputs(state)?;
        let request =
            crate::persistent_session_admission::cleanup_channel::PreflightInputs::from_original(
                &original,
            )?;
        self.require_exclusive()?;
        // No queue/read/acceptance/Init in N1. Source provider implementation,
        // typed applied-receipt readiness and authorized lifecycle remain gates.
        Ok(request)
    }

    fn restore_owned(&mut self, retained: &mut PooledStartRetention) {
        // This private Option destination is reserved by the unique checkout;
        // no competing deposit writes it and this Drop occurs exactly once.
        // Keeping a bundle beside the quarantine avoids vector allocation and
        // replacement of colliding originals during poisoned/unwind restore.
        retained.original_startup_restored = Some(OriginalStartupRetainedOwners {
            _running: self.running.take(),
            _inputs: self.inputs.take(),
            _channel: self.channel.take(),
        });
    }
}

impl Drop for OriginalStartupCheckGuard<'_> {
    fn drop(&mut self) {
        // Unwind/poison/expiry must restore custody, not attempt acceptance or
        // obtain another budget. Blocking restoration is NOT timed admission.
        let original = self.original;
        let mut retained = original.retained.lock().unwrap_or_else(|p| p.into_inner());
        self.restore_owned(&mut retained);
        retained.startup_check_in_flight = false;
        retained.startup_check_refused = true;
        retained.manifest_preparation = OriginalManifestPreparationPhase::Refused;
        retained.admission_refused = true;
        retained.phase = PooledStartPhase::Unsettled;
        // Deliberately no success/disarm method: N1 cannot mint acceptance.
    }
}

// The guard can restore only objects it actually owns. A panic inside a
// consuming wait may destroy the moved RunningExecution; higher originals
// stay refused and funded/scratch uncertainty is never converted to proof.
struct OriginalNativeCheckGuard<'a> {
    running: Option<ryeos_engine::dispatch::RunningExecution>,
    held_occurrence: Arc<lillux::exec::ProcessLocalHeldOccurrence>,
    inputs: Option<crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs>,
    original: &'a PersistentSessionPooledStart,
    fallback_armed: bool,
}

impl OriginalNativeCheckGuard<'_> {
    fn has_exclusive_retention(&self, retained: &PooledStartRetention) -> bool {
        retained.native_check_attempted
            && retained.manifest_preparation == OriginalManifestPreparationPhase::Unattempted
            && retained.original_manifest_restored.is_none()
            && retained.native_check_in_flight
            && !retained.native_check_refused
            && !retained.admission_refused
            && retained.phase == PooledStartPhase::Unsettled
            && retained.original_native_acceptance.is_none()
            && retained.trusted_inputs.is_none()
            && retained.retained_running.is_empty()
            && retained.quarantined_inputs.is_empty()
            && retained.original_spawn_permit.is_some()
            && retained.original_contact_attempted
            && retained.original_held_attached
            && self.original.is_in_flight()
            && retained
                .original_funding
                .as_ref()
                .and_then(|funding| funding.acknowledged_held_occurrence())
                .is_some_and(|actual| Arc::ptr_eq(actual, &self.held_occurrence))
    }

    fn require_exclusive(&self) -> Result<()> {
        let retained = self
            .original
            .retained
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let exclusive = self.has_exclusive_retention(&retained);
        drop(retained);
        anyhow::ensure!(
            exclusive,
            "original native check lost exclusive retained custody"
        );
        Ok(())
    }

    fn restore_owned(&mut self, retained: &mut PooledStartRetention) {
        if let Some(running) = self.running.take() {
            retained.retained_running.push(running);
        }
        if let Some(inputs) = self.inputs.take() {
            if let Some(unexpected) = retained.trusted_inputs.take() {
                retained.quarantined_inputs.push(unexpected);
            }
            retained.trusted_inputs = Some(inputs);
        }
    }

    // One locked transaction; rejection returns the staged owner to the caller
    // so its destruction and error formatting happen after releasing the lock.
    fn commit_acceptance(
        &mut self,
        acceptance: OriginalNamespaceReporterAcceptance,
    ) -> std::result::Result<(), OriginalNamespaceReporterAcceptance> {
        let original = self.original;
        let mut retained = original.retained.lock().unwrap_or_else(|p| p.into_inner());
        let eligible = self.fallback_armed
            && self.running.is_some()
            && self.inputs.is_some()
            && self.has_exclusive_retention(&retained)
            && Arc::ptr_eq(&acceptance.held_occurrence, &self.held_occurrence);
        // Restore exact checked-out originals FIRST, preserving all collisions.
        self.restore_owned(&mut retained);
        let commit = eligible
            && retained.native_check_attempted
            && retained.native_check_in_flight
            && !retained.native_check_refused
            && !retained.admission_refused
            && retained.phase == PooledStartPhase::Unsettled
            && retained.original_native_acceptance.is_none()
            && retained.trusted_inputs.is_some()
            && retained.retained_running.len() == 1
            && retained.quarantined_inputs.is_empty()
            && retained.original_spawn_permit.is_some()
            && retained.original_contact_attempted
            && retained.original_held_attached
            && original.is_in_flight()
            && retained
                .original_funding
                .as_ref()
                .and_then(|funding| funding.acknowledged_held_occurrence())
                .is_some_and(|actual| Arc::ptr_eq(actual, &self.held_occurrence))
            && !acceptance.original_deadline.has_elapsed();
        if commit {
            retained.original_native_acceptance = Some(acceptance);
            retained.native_check_in_flight = false;
            retained.admission_refused = true;
            // attempted/Unsettled remain; native_check_refused stays false.
            // No fallible work between committing and disarming fallback.
            self.fallback_armed = false;
            Ok(())
        } else {
            retained.native_check_in_flight = false;
            retained.native_check_refused = true;
            retained.admission_refused = true;
            retained.phase = PooledStartPhase::Unsettled;
            Err(acceptance)
        }
    }
}

impl Drop for OriginalNativeCheckGuard<'_> {
    fn drop(&mut self) {
        if !self.fallback_armed {
            return;
        }
        let original = self.original;
        let mut retained = original.retained.lock().unwrap_or_else(|p| p.into_inner());
        self.restore_owned(&mut retained);
        retained.native_check_in_flight = false;
        retained.native_check_refused = true;
        retained.admission_refused = true;
        retained.phase = PooledStartPhase::Unsettled;
        // No callback, validation, formatting, object destruction or wait
        // while locked; every actual collision remains in existing custody.
    }
}

enum OriginalNativeCheckOutcome {
    CollectedFacts(OriginalNamespaceReporterAcceptance),
    Incomplete,
    Failed,
    Unsupported,
}

impl PersistentSessionPooledStart {
    /// Mandatory CUSTODY-only refusal. Call only after releasing borrowed
    /// retention guards; completion can block and is NOT timed admission.
    fn refuse_manifest_precontact(&self) {
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        retained.manifest_preparation = OriginalManifestPreparationPhase::Refused;
        retained.admission_refused = true;
        retained.phase = PooledStartPhase::Unsettled;
    }

    fn checkout_original_manifest_precontact<'a>(
        &'a self,
        cancelled: &'a dyn Fn() -> bool,
    ) -> Result<OriginalManifestPrecontactGuard<'a>> {
        let mut retained = match self.lock_original_contact_retention(Some(cancelled)) {
            Ok(retained) => retained,
            Err(error) => {
                self.refuse_manifest_precontact();
                return Err(error);
            }
        };
        let eligible = !self.retained.is_poisoned()
            && !Self::native_check_blocks_mutation(&retained)
            && !retained.admission_refused
            && retained.phase == PooledStartPhase::Preparing
            && retained.original_funding.is_none()
            && retained
                .trusted_inputs
                .as_ref()
                .is_some_and(|v| v.has_selected_startup_contract())
            && retained.original_scratch.is_some()
            && !retained.original_contact_attempted
            && retained.original_spawn_permit.is_none()
            && !retained.original_held_attached
            && retained.retained_running.is_empty()
            && retained.original_startup_restored.is_none()
            && retained.original_manifest_restored.is_none()
            && self.is_in_flight();
        if !eligible {
            retained.manifest_preparation = OriginalManifestPreparationPhase::Refused;
            retained.admission_refused = true;
            retained.phase = PooledStartPhase::Unsettled;
            drop(retained);
            bail!(
                "original manifest checkout requires exclusive unfunded signed inputs and scratch"
            );
        }
        retained.manifest_preparation = OriginalManifestPreparationPhase::UnfundedInFlight;
        let guard = OriginalManifestPrecontactGuard {
            inputs: retained.trusted_inputs.take(),
            original: self,
            cancelled,
            deadline: self
                .original_callback_budget
                .get()
                .expect("budget checked acquiring retention")
                .contact_expiry(),
            scratch: Arc::clone(
                retained
                    .original_scratch
                    .as_ref()
                    .expect("actual scratch checked before checkout"),
            ),
        };
        drop(retained);
        guard.require_exclusive()?;
        Ok(guard)
    }
    /// Inert N1 preparation seam; no caller until separate N2 authorization.
    /// Pair creation is outside the retention mutex. No queue/release/Init.
    fn prepare_original_startup_channel(
        &self,
    ) -> Result<ryeos_engine::isolation::IsolationTargetChannelAuthority> {
        let deadline = self
            .original_callback_budget
            .get()
            .context("startup channel lacks actual original callback budget")?
            .contact_expiry();
        let (channel, target) = crate::persistent_session_admission::cleanup_channel::TrustedCleanupChannel::prepare_preflight(deadline)?;
        // Owned channel deposit cannot be lost merely because admission locking
        // expired. This lock retains only; it performs no channel/CAS/live IO.
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        let duplicate = retained.original_startup_channel.is_some();
        if duplicate {
            retained.quarantined_startup_channels.push(channel);
        } else {
            retained.original_startup_channel = Some(channel);
        }
        let eligible = !duplicate
            && !self.retained.is_poisoned()
            && !deadline.has_elapsed()
            && !Self::native_check_blocks_mutation(&retained)
            && !retained.admission_refused
            && retained.phase == PooledStartPhase::Preparing
            && self.is_in_flight()
            && retained
                .trusted_inputs
                .as_ref()
                .is_some_and(|inputs| inputs.has_selected_startup_contract());
        if !eligible {
            retained.startup_check_refused = true;
            retained.admission_refused = true;
        }
        drop(retained);
        anyhow::ensure!(
            eligible,
            "original startup channel lacks exclusive signed original input"
        );
        Ok(target)
    }

    /// Own supplied originals BEFORE checking eligibility. Inert: no Executor
    /// caller, terminal polling, pending-receipt string classifier or Init.
    fn checkout_original_startup<'a>(
        &'a self,
        cancelled: &'a dyn Fn() -> bool,
        running: ryeos_engine::dispatch::RunningExecution,
        lifelines: Vec<Box<dyn Send + Sync>>,
        resource: Option<PersistentSessionCleanupObserver>,
        retirement: Option<PersistentSessionCleanupObserver>,
    ) -> Result<OriginalStartupCheckGuard<'a>> {
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        retained.retained_running.push(running);
        retained.lifelines.extend(lifelines);
        stage_pooled_obligation_owners(&mut retained, &resource, &retirement);
        let eligible = !self.retained.is_poisoned()
            && retained.manifest_preparation == OriginalManifestPreparationPhase::Prepared
            && !retained.native_check_attempted
            && !retained.native_check_in_flight
            && !retained.native_check_refused
            && !retained.startup_check_attempted
            && !retained.startup_check_in_flight
            && !retained.startup_check_refused
            && retained.original_manifest_restored.is_none()
            && retained.quarantined_inputs.is_empty()
            && retained.quarantined_startup_channels.is_empty()
            && !retained.admission_refused
            && retained.phase == PooledStartPhase::Preparing
            && retained.retained_running.len() == 1
            && retained.trusted_inputs.is_some()
            && retained.original_startup_channel.is_some()
            && retained.original_startup_restored.is_none()
            && retained.original_spawn_permit.is_some()
            && retained.original_contact_attempted
            && retained.original_held_attached
            && retained
                .original_funding
                .as_ref()
                .and_then(|funding| funding.acknowledged_held_occurrence())
                .is_some()
            && self.is_in_flight()
            && self
                .original_callback_budget
                .get()
                .is_some_and(|budget| !budget.contact_expiry().has_elapsed());
        retained.startup_check_attempted = true;
        if !eligible {
            retained.startup_check_refused = true;
            retained.admission_refused = true;
            retained.phase = PooledStartPhase::Unsettled;
            drop(retained);
            bail!("original startup requires exclusive Attached funded original custody");
        }
        retained.startup_check_in_flight = true;
        let guard = OriginalStartupCheckGuard {
            running: retained.retained_running.pop(),
            inputs: retained.trusted_inputs.take(),
            channel: retained.original_startup_channel.take(),
            held_occurrence: Arc::clone(
                retained
                    .original_funding
                    .as_ref()
                    .and_then(|funding| funding.acknowledged_held_occurrence())
                    .expect("actual acknowledged occurrence checked before checkout"),
            ),
            original: self,
            cancelled,
        };
        drop(retained);
        guard.require_exclusive()?;
        Ok(guard)
    }

    // Root EVERY supplied guard before checking multiplicity. A duplicate
    // remains in existing lifelines; the first typed original is never replaced.
    fn retain_original_scratch(
        retained: &mut PooledStartRetention,
        scratch: &Arc<crate::temp_dir_guard::TempDirGuard>,
    ) -> bool {
        retained.lifelines.push(Box::new(Arc::clone(scratch)));
        if retained.original_scratch.is_some() {
            return false;
        }
        retained.original_scratch = Some(Arc::clone(scratch));
        true
    }

    fn native_check_blocks_mutation(retained: &PooledStartRetention) -> bool {
        retained.manifest_preparation != OriginalManifestPreparationPhase::Unattempted
            || retained.original_manifest_restored.is_some()
            || retained.native_check_attempted
            || retained.native_check_in_flight
            || retained.native_check_refused
            || !retained.quarantined_inputs.is_empty()
            || retained.startup_check_attempted
            || retained.startup_check_in_flight
            || retained.startup_check_refused
            || !retained.quarantined_startup_channels.is_empty()
    }

    fn new() -> Self {
        Self {
            retained: Mutex::new(PooledStartRetention {
                retained_running: Vec::new(),
                original_native_acceptance: None,
                original_startup_channel: None,
                quarantined_startup_channels: Vec::new(),
                startup_check_attempted: false,
                startup_check_in_flight: false,
                startup_check_refused: false,
                original_startup_restored: None,
                manifest_preparation: OriginalManifestPreparationPhase::Unattempted,
                original_manifest_restored: None,
                quarantined_inputs: Vec::new(),
                native_check_attempted: false,
                native_check_in_flight: false,
                native_check_refused: false,
                original_held_attached: false,
                phase: PooledStartPhase::Preparing,
                lifelines: Vec::new(),
                resource_observer: None,
                retirement_observer: None,
                obligation_owners: Vec::new(),
                admission_refused: false,
                trusted_inputs: None,
                original_funding: None,
                original_contact_attempted: false,
                original_spawn_permit: None,
                original_scratch: None,
            }),
            original_callback_budget: OnceLock::new(),
            callback_in_flight: AtomicBool::new(true),
        }
    }

    /// Retain the actual verified callback clock in this same pooled-start slot.
    /// Ordinary scope JSON, replay and a second callback cannot replace it.
    pub fn retain_original_callback_budget(
        &self,
        budget: &crate::handler_context::OriginalExecutionBudget,
        allocation_scope: &ryeos_state::objects::AdmittedAccountingScope,
    ) -> Result<()> {
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        if Self::native_check_blocks_mutation(&retained)
            || retained.phase != PooledStartPhase::Preparing
            || retained.admission_refused
            || self.original_callback_budget.get().is_some()
        {
            retained.admission_refused = true;
            bail!("original pooled callback budget cannot be replaced or replayed");
        }
        // Keep the supplied original even if subsequent validation refuses.
        if self.original_callback_budget.set(budget.clone()).is_err() {
            retained.admission_refused = true;
            bail!("original pooled callback budget cannot replace its one-use retained owner");
        }
        if budget.precontact_binding().scope != allocation_scope
            || budget.contact_expiry().has_elapsed()
        {
            retained.admission_refused = true;
            bail!("original pooled callback budget differs from allocation scope or expired");
        }
        Ok(())
    }

    /// Stage the verified input and SAME scratch owner before any write.
    /// No decoder, replacement budget, reconstructed selection or launcher.
    /// Production native admission remains behind its existing closed guards.
    pub fn prepare_trusted_original_funding(
        &self,
        state: &crate::state::AppState,
        inputs: crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs,
        allocation_scope: &ryeos_state::objects::AdmittedAccountingScope,
        original: &crate::runtime_db::ProcessResourceReservationRecord,
        scratch: Arc<crate::temp_dir_guard::TempDirGuard>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool> {
        self.prepare_trusted_original_funding_inner(
            state,
            inputs,
            allocation_scope,
            original,
            scratch,
            cancelled,
            None,
        )
    }

    /// GPU-only data preparation over the exact already-retained realization
    /// roots. This stays in the same unique unfunded precontact guard.
    pub fn prepare_trusted_gpu_original_funding(
        &self,
        state: &crate::state::AppState,
        inputs: crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs,
        allocation_scope: &ryeos_state::objects::AdmittedAccountingScope,
        original: &crate::runtime_db::ProcessResourceReservationRecord,
        scratch: Arc<crate::temp_dir_guard::TempDirGuard>,
        cancelled: &dyn Fn() -> bool,
        roots: ryeos_gpu_loader_owner_contract::BorrowedGpuLoaderRealizationRoots<'_>,
    ) -> Result<bool> {
        self.prepare_trusted_original_funding_inner(
            state,
            inputs,
            allocation_scope,
            original,
            scratch,
            cancelled,
            Some(roots),
        )
    }

    fn prepare_trusted_original_funding_inner(
        &self,
        state: &crate::state::AppState,
        inputs: crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs,
        allocation_scope: &ryeos_state::objects::AdmittedAccountingScope,
        original: &crate::runtime_db::ProcessResourceReservationRecord,
        scratch: Arc<crate::temp_dir_guard::TempDirGuard>,
        cancelled: &dyn Fn() -> bool,
        gpu_roots: Option<ryeos_gpu_loader_owner_contract::BorrowedGpuLoaderRealizationRoots<'_>>,
    ) -> Result<bool> {
        // Mandatory custody deposit BEFORE selector/budget/cancellation checks.
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        let first_scratch = Self::retain_original_scratch(&mut retained, &scratch);
        if !first_scratch
            || Self::native_check_blocks_mutation(&retained)
            || retained.trusted_inputs.is_some()
            || retained.original_funding.is_some()
        {
            retained.quarantined_inputs.push(inputs);
            retained.manifest_preparation = OriginalManifestPreparationPhase::Refused;
            retained.admission_refused = true;
            retained.phase = PooledStartPhase::Unsettled;
            drop(retained);
            bail!("original trusted funding attempt cannot replace verified input owner");
        }
        retained.trusted_inputs = Some(inputs);
        let selected = retained
            .trusted_inputs
            .as_ref()
            .expect("actual input deposited")
            .has_selected_startup_contract();
        drop(retained);
        let result = (|| {
            let budget = self
                .original_callback_budget
                .get()
                .context("original trusted funding lacks retained callback budget")?;
            anyhow::ensure!(
                budget.precontact_binding().scope == allocation_scope,
                "original document funding differs from retained callback scope"
            );
            anyhow::ensure!(
                !cancelled() && !budget.contact_expiry().has_elapsed(),
                "original trusted funding cancelled or expired before preparation"
            );
            let gpu_roots_supplied = gpu_roots.is_some();
            if selected {
                let mut guard = self.checkout_original_manifest_precontact(cancelled)?;
                guard.retain_original_manifest_documents(state)?;
                if let Some(roots) = gpu_roots {
                    guard.retain_gpu_loader_closure_inventory(state, roots)?;
                }
                guard.commit_prepared(state)?;
            } else {
                anyhow::ensure!(
                    !gpu_roots_supplied,
                    "GPU loader roots were supplied without selected startup inputs"
                );
            }
            // SAME predicate before reacquisition; no callback under retention.
            let mut retained = self.lock_original_contact_retention(Some(cancelled))?;
            let document_phase = if selected {
                retained.manifest_preparation == OriginalManifestPreparationPhase::Prepared
                    && !retained.native_check_attempted
                    && !retained.native_check_in_flight
                    && !retained.native_check_refused
                    && !retained.startup_check_attempted
                    && !retained.startup_check_in_flight
                    && !retained.startup_check_refused
                    && retained.original_manifest_restored.is_none()
                    && retained.original_startup_restored.is_none()
                    && retained.quarantined_inputs.is_empty()
                    && retained.quarantined_startup_channels.is_empty()
            } else {
                !Self::native_check_blocks_mutation(&retained)
            };
            let eligible = document_phase
                && !self.retained.is_poisoned()
                && !retained.admission_refused
                && retained.phase == PooledStartPhase::Preparing
                && retained.trusted_inputs.is_some()
                && retained.original_funding.is_none()
                && retained
                    .original_scratch
                    .as_ref()
                    .is_some_and(|v| Arc::ptr_eq(v, &scratch))
                && !retained.original_contact_attempted
                && retained.original_spawn_permit.is_none()
                && !retained.original_held_attached
                && retained.retained_running.is_empty()
                && self.is_in_flight();
            if !eligible {
                drop(retained);
                bail!("original pooled owner no longer admits exact funding continuation");
            }
            if !selected {
                // Historical unselected funding route unchanged. Selected bind,
                // CAS producer and validated borrow already ran outside this lock.
                retained
                    .trusted_inputs
                    .as_mut()
                    .expect("original input retained")
                    .bind_original_isolation_expectation(state, budget)?;
            }
            let callback = budget.precontact_binding();
            let funding = crate::accounting_db::precontact::OriginalFundingOwner::prepare(
                state,
                retained
                    .trusted_inputs
                    .as_ref()
                    .expect("same restored input retained"),
                budget,
                allocation_scope,
                original,
                callback.callback_thread_id,
                callback.callback_launch_owner,
            )?;
            // Root SAME attempt BEFORE first RuntimeDb/ledger write; no reset.
            retained.original_funding = Some(funding);
            state
                .state_store
                .reserve_process_resource_launch_with_scratch_until(
                    original,
                    &scratch,
                    budget.contact_expiry(),
                )?;
            retained
                .original_funding
                .as_mut()
                .expect("funding retained before RuntimeDb")
                .reserve()
        })();
        if !matches!(&result, Ok(true)) {
            // All borrowed guards in closure have been released. Custody-only
            // completion may block after cancellation/expiry; never timed IO.
            self.refuse_manifest_precontact();
        }
        result
    }

    /// Bound only BORROWED contact/attachment acquisition by the SAME budget.
    /// Owned lifeline deposits keep their existing retention on every refusal.
    fn lock_original_contact_retention(
        &self,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<std::sync::MutexGuard<'_, PooledStartRetention>> {
        if cancelled.is_some_and(|predicate| predicate()) {
            bail!("original callback cancelled before contact retention");
        }
        let deadline = self
            .original_callback_budget
            .get()
            .context("original contact retention lacks actual callback budget")?
            .contact_expiry();
        loop {
            // Predicate may itself lock retention: always sample BEFORE acquisition.
            if cancelled.is_some_and(|predicate| predicate()) {
                bail!("original callback cancelled while acquiring contact retention");
            }
            if deadline.has_elapsed() {
                bail!("original callback expired while acquiring contact retention");
            }
            let acquired = match self.retained.try_lock() {
                Ok(retained) => Some(retained),
                Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => None,
            };
            if let Some(retained) = acquired {
                if deadline.has_elapsed() {
                    drop(retained);
                    bail!("original callback expired after acquiring contact retention");
                }
                return Ok(retained);
            }
            lillux::time::sleep(Duration::from_millis(1).min(deadline.remaining()));
        }
    }

    /// Consume the same funded owner and journaled Runtime intent once.
    /// The actual permit remains rooted here on every later refusal.
    pub(crate) fn take_original_funded_contact(
        &self,
        state: &crate::state::AppState,
        selected: &crate::execution_resources::SelectedExecutionResources,
        owned_capsule_hash: &str,
        owner_kind: &str,
        owner_coordinate: &str,
    ) -> Result<(
        lillux::time::OriginalNamespaceOccupancyHandoff,
        crate::runtime_db::ProcessResourceReservationRecord,
    )> {
        let mut retained = self.lock_original_contact_retention(None)?;
        if Self::native_check_blocks_mutation(&retained)
            || retained.phase != PooledStartPhase::Preparing
            || retained.admission_refused
            || retained.original_contact_attempted
        {
            retained.admission_refused = true;
            bail!("original pooled contact cannot be replaced or replayed");
        }
        retained.original_contact_attempted = true;
        let result = (|| {
            retained
                .trusted_inputs
                .as_ref()
                .context("original contact lacks retained verified inputs")?
                .require_original_isolation_expectation(state)?;
            let funding = retained
                .original_funding
                .as_mut()
                .context("original contact lacks same funded owner")?;
            let (original, deadline) = funding.original_contact_reservation(
                owned_capsule_hash,
                owner_kind,
                owner_coordinate,
            )?;
            if original.selections != selected.selections() {
                bail!("original funded contact differs from current exact plan selection");
            }
            let permit = state
                .state_store
                .begin_trusted_process_resource_spawn_until(original, deadline)?;
            funding.bind_contact_intent(permit)?;
            let (permit, window) = funding.take_contact_permit()?;
            let intent = permit.reservation().clone();
            retained.original_spawn_permit = Some(permit);
            Ok((window, intent))
        })();
        if result.is_err() {
            retained.admission_refused = true;
        }
        result
    }

    /// Attach the actual held Engine owner to the original financial operation.
    /// No second debit, reconstructed process identity or release authorization.
    pub(crate) fn attach_original_funded_held(
        &self,
        held: &ryeos_engine::dispatch::SpawnedExecutionAwaitingAttachment,
    ) -> Result<crate::process::ExecutionProcessIdentity> {
        let mut retained = self.lock_original_contact_retention(None)?;
        if Self::native_check_blocks_mutation(&retained)
            || retained.phase != PooledStartPhase::Preparing
            || retained.admission_refused
            || !retained.original_contact_attempted
            || retained.original_spawn_permit.is_none()
        {
            retained.admission_refused = true;
            bail!("original held attachment lacks its retained one-use contact permit");
        }
        let result = retained
            .original_funding
            .as_mut()
            .context("original held attachment lost its funded owner")?
            .attach_held(held)
            .map(|(identity, _binding)| identity);
        if result.is_err() {
            retained.admission_refused = true;
        } else {
            retained.original_held_attached = true;
        }
        result
    }

    /// Actual selected pooled callback only, after held release and before Ready.
    /// Root every supplied object first; all outcomes remain refused for Ready,
    /// startup/driver/physical retirement and financial settlement.
    /// The actual Executor held/attach/release caller supplies source lineage;
    /// only its protected Collected branch can commit reporter acceptance.
    /// Acceptance is historical and never independent retirement authority.
    pub fn retain_and_check_original_native(
        &self,
        state: &crate::state::AppState,
        running: ryeos_engine::dispatch::RunningExecution,
        lifelines: Vec<Box<dyn Send + Sync>>,
        resource: Option<PersistentSessionCleanupObserver>,
        retirement: Option<PersistentSessionCleanupObserver>,
    ) -> Result<()> {
        let mut guard = {
            let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
            retained.retained_running.push(running);
            retained.lifelines.extend(lifelines);
            stage_pooled_obligation_owners(&mut retained, &resource, &retirement);
            let eligible = !Self::native_check_blocks_mutation(&retained)
                && !retained.admission_refused
                && retained.phase == PooledStartPhase::Preparing
                && retained.retained_running.len() == 1
                && retained.trusted_inputs.is_some()
                && retained.original_funding.is_some()
                && retained
                    .original_funding
                    .as_ref()
                    .and_then(|funding| funding.acknowledged_held_occurrence())
                    .is_some()
                && retained.original_spawn_permit.is_some()
                && retained.original_contact_attempted
                && retained.original_held_attached
                && self.is_in_flight()
                && self.original_callback_budget.get().is_some();
            retained.native_check_attempted = true;
            retained.phase = PooledStartPhase::Unsettled;
            if !eligible {
                retained.native_check_refused = true;
                retained.admission_refused = true;
                drop(retained);
                return Err(anyhow!(
                    "original native diagnostic lacks exclusive original held/funded custody"
                )
                .context(PersistentSessionCleanupUnproved));
            }
            retained.native_check_in_flight = true;
            OriginalNativeCheckGuard {
                running: retained.retained_running.pop(),
                held_occurrence: Arc::clone(
                    retained
                        .original_funding
                        .as_ref()
                        .and_then(|funding| funding.acknowledged_held_occurrence())
                        .expect("actual acknowledged funding link checked before checkout"),
                ),
                inputs: retained.trusted_inputs.take(),
                original: self,
                fallback_armed: true,
            }
        };
        let observed = (|| -> Result<OriginalNativeCheckOutcome> {
            guard.require_exclusive()?;
            let inputs = guard
                .inputs
                .as_ref()
                .expect("actual original input checked out");
            inputs.require_original_isolation_expectation(state)?;
            let capture = Arc::clone(inputs.original_isolation_capture(state)?);
            guard
                .running
                .as_ref()
                .expect("actual original execution checked out")
                .require_original_backend_capture(&capture)
                .map_err(anyhow::Error::new)?;
            guard
                .running
                .as_ref()
                .expect("actual execution before wait")
                .require_original_held_occurrence(&guard.held_occurrence)
                .map_err(anyhow::Error::new)?;
            let original_budget = self
                .original_callback_budget
                .get()
                .context("original native diagnostic lost callback budget")?;
            let slice =
                Duration::from_millis(200).min(original_budget.contact_expiry().remaining());
            guard.require_exclusive()?;
            let running = guard
                .running
                .take()
                .expect("same execution before consuming wait");
            // No catch_unwind promise: panic INSIDE wait(self) can destroy the
            // moved execution. Higher inputs/funding/scratch stay refused.
            let outcome = running.wait_for_natural_exit_retaining_native(slice);
            let phase = match outcome {
                Ok(ryeos_engine::dispatch::SelectedNativeExecutionWaitOutcome::Collected(
                    running,
                )) => {
                    guard.running = Some(running);
                    None
                }
                Ok(ryeos_engine::dispatch::SelectedNativeExecutionWaitOutcome::Incomplete(
                    running,
                )) => {
                    guard.running = Some(running);
                    Some(OriginalNativeCheckOutcome::Incomplete)
                }
                Ok(ryeos_engine::dispatch::SelectedNativeExecutionWaitOutcome::Failed(running)) => {
                    guard.running = Some(running);
                    Some(OriginalNativeCheckOutcome::Failed)
                }
                Err(running) => {
                    guard.running = Some(running);
                    Some(OriginalNativeCheckOutcome::Unsupported)
                }
            };
            if let Some(phase) = phase {
                return Ok(phase);
            }
            {
                guard.require_exclusive()?;
                guard
                    .running
                    .as_ref()
                    .expect("typed outcome restored before held match")
                    .require_original_held_occurrence(&guard.held_occurrence)
                    .map_err(anyhow::Error::new)?;
                guard
                    .running
                    .as_mut()
                    .expect("typed outcome restored before check")
                    .require_cached_original_native_correspondence(&capture)
                    .map_err(anyhow::Error::new)?;
                inputs.require_original_isolation_expectation(state)?;
                guard
                    .running
                    .as_ref()
                    .expect("same owner after terminal correspondence")
                    .require_original_held_occurrence(&guard.held_occurrence)
                    .map_err(anyhow::Error::new)?;
                guard.require_exclusive()?;
                Ok(OriginalNativeCheckOutcome::CollectedFacts(
                    OriginalNamespaceReporterAcceptance {
                        held_occurrence: Arc::clone(&guard.held_occurrence),
                        _capture: capture,
                        original_deadline: original_budget.contact_expiry(),
                    },
                ))
            }
        })();
        let observed = match observed {
            Ok(OriginalNativeCheckOutcome::CollectedFacts(acceptance)) => {
                match guard.commit_acceptance(acceptance) {
                    Ok(()) => Ok(None),
                    Err(rejected) => {
                        drop(rejected); // outside the retained lock
                        Err(anyhow!(
                            "original native acceptance lost atomic commit eligibility"
                        ))
                    }
                }
            }
            Ok(phase) => Ok(Some(phase)),
            Err(error) => Err(error),
        };
        // Restore exact objects/collisions BEFORE diagnostics or callback return.
        drop(guard);
        let error = match observed {
            Ok(None) => anyhow!(
                "original namespace reporter accepted; readiness/driver/physical retirement remains unproved"
            ),
            Ok(Some(OriginalNativeCheckOutcome::Incomplete)) => {
                anyhow!("original native collection remains incomplete")
            }
            Ok(Some(OriginalNativeCheckOutcome::Failed)) => {
                anyhow!("original native collection retained a refusal")
            }
            Ok(Some(OriginalNativeCheckOutcome::Unsupported)) => {
                anyhow!("original native collection lacks selected context")
            }
            Ok(Some(OriginalNativeCheckOutcome::CollectedFacts(_))) => {
                unreachable!("collected acceptance was consumed by atomic commit")
            }
            Err(error) => error.context("original protected native diagnostic refused"),
        };
        Err(error.context(PersistentSessionCleanupUnproved))
    }

    /// An ordinary process-group reap is not trusted driver/physical release.
    /// No public completion callback or decoded row can clear this obligation.
    fn has_original_trusted_obligation(&self) -> bool {
        let retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        retained.trusted_inputs.is_some()
            || retained.original_manifest_restored.is_some()
            || retained.manifest_preparation != OriginalManifestPreparationPhase::Unattempted
            || retained.original_funding.is_some()
            || retained.native_check_attempted
            || retained.native_check_in_flight
            || retained.native_check_refused
            || !retained.quarantined_inputs.is_empty()
    }

    /// Move actual ORIGINAL guards/bindings into the already owned start slot.
    /// The trusted executor does this before its held-spawn call. This method
    /// cannot release a process or grant cleanup from a supplied record.
    pub fn retain_before_contact(&self, lifelines: Vec<Box<dyn Send + Sync>>) -> Result<()> {
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        retained.lifelines.extend(lifelines); // Root supplied owners even on refusal.
        if Self::native_check_blocks_mutation(&retained)
            || retained.phase != PooledStartPhase::Preparing
            || retained.admission_refused
        {
            retained.admission_refused = true;
            bail!("pooled start no longer accepts pre-contact retention");
        }
        Ok(())
    }

    /// Stage ALL supplied owners first; atomically validate both kinds before
    /// changing any executable original. Repeated SAME Arc capture is idempotent.
    pub fn retain_cleanup_obligations(
        &self,
        resource: Option<PersistentSessionCleanupObserver>,
        retirement: Option<PersistentSessionCleanupObserver>,
    ) -> Result<()> {
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        stage_pooled_obligation_owners(&mut retained, &resource, &retirement);
        if Self::native_check_blocks_mutation(&retained)
            || retained.phase != PooledStartPhase::Preparing
            || retained.admission_refused
        {
            retained.admission_refused = true;
            bail!("pooled start no longer accepts cleanup obligations");
        }
        let result = admit_original_pooled_obligations(&mut retained, resource, retirement);
        if result.is_err() {
            retained.admission_refused = true;
        }
        result
    }

    fn retain_ready_parts(&self, started: &mut StartedPersistentSession) -> Result<()> {
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        retained.lifelines.append(&mut started.lifelines);
        if let Some(relay_owner) = started.relay_owner.as_ref() {
            retained.lifelines.push(Box::new(Arc::clone(relay_owner)));
        }
        stage_pooled_obligation_owners(
            &mut retained,
            &started.cleanup_observer,
            &started.retirement_observer,
        );
        if Self::native_check_blocks_mutation(&retained) || retained.admission_refused {
            bail!("pooled start previously substituted an original cleanup obligation");
        }
        if retained.trusted_inputs.is_some() || retained.original_funding.is_some() {
            retained.admission_refused = true;
            bail!(
                "trusted pooled ready publication requires original held/runtime/driver ownership join"
            );
        }
        let result = admit_original_pooled_obligations(
            &mut retained,
            started.cleanup_observer.clone(),
            started.retirement_observer.clone(),
        );
        if result.is_err() {
            retained.admission_refused = true;
        }
        result?;
        // Started omission never erases a pre-contact obligation. The SAME
        // original Arcs reach ready_process and its once-only process owner.
        started.cleanup_observer = retained.resource_observer.clone();
        started.retirement_observer = retained.retirement_observer.clone();
        Ok(())
    }

    fn is_in_flight(&self) -> bool {
        self.callback_in_flight.load(Ordering::Acquire)
    }

    fn complete_callback(&self) {
        // Called only with PoolState held AFTER final admission/failure state.
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        if retained.manifest_preparation != OriginalManifestPreparationPhase::Unattempted
            || retained.original_manifest_restored.is_some()
        {
            retained.manifest_preparation = OriginalManifestPreparationPhase::Refused;
            retained.admission_refused = true;
            retained.phase = PooledStartPhase::Unsettled;
        }
        self.callback_in_flight.store(false, Ordering::Release);
    }

    fn has_retained_custody(&self) -> bool {
        let retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        Self::native_check_blocks_mutation(&retained)
            || !retained.retained_running.is_empty()
            || !retained.lifelines.is_empty()
            || retained.resource_observer.is_some()
            || retained.retirement_observer.is_some()
            || !retained.obligation_owners.is_empty()
            || retained.trusted_inputs.is_some()
            || retained.original_funding.is_some()
    }

    fn publish(&self) -> bool {
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        if Self::native_check_blocks_mutation(&retained)
            || retained.admission_refused
            || retained.trusted_inputs.is_some()
            || retained.original_funding.is_some()
            || !retained.retained_running.is_empty()
        {
            retained.phase = PooledStartPhase::Unsettled;
            retained.admission_refused = true;
            return false;
        }
        retained.phase = PooledStartPhase::Registered;
        true
    }

    // Actual pooled-ready refusal and SessionProcess::drop deposits only.
    // Root every actual owner before checking multiplicity; never evict one.
    fn retain_running_unsettled(&self, running: ryeos_engine::dispatch::RunningExecution) {
        let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
        retained.retained_running.push(running);
        retained.phase = PooledStartPhase::Unsettled;
        if Self::native_check_blocks_mutation(&retained) || retained.retained_running.len() > 1 {
            retained.native_check_refused |=
                retained.native_check_attempted || retained.native_check_in_flight;
            retained.admission_refused = true;
        }
    }

    fn retain_unsettled(&self) {
        self.retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .phase = PooledStartPhase::Unsettled;
    }

    /// Private pool call only AFTER checked reap and every existing observer.
    /// It cannot remove a preserved scratch or clear its durable resource row.
    fn prove_cleanup(&self) -> Result<()> {
        // Trusted component funding has no accepted driver/scratch settlement
        // join yet. Never mark it settled from ordinary group cleanup.
        if self.has_original_trusted_obligation() {
            self.retain_unsettled();
            bail!("original trusted funding/physical retirement remains unproved");
        }
        let (lifelines, resource, retirement, supplied_owners) = {
            let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
            if Self::native_check_blocks_mutation(&retained)
                || !retained.retained_running.is_empty()
            {
                retained.phase = PooledStartPhase::Unsettled;
                retained.admission_refused = true;
                bail!("retained original running custody cannot be settled by ordinary cleanup");
            }
            (
                std::mem::take(&mut retained.lifelines),
                retained.resource_observer.take(),
                retained.retirement_observer.take(),
                std::mem::take(&mut retained.obligation_owners),
            )
        };
        // Guard/closure destruction may do filesystem/accounting work.
        drop(resource);
        drop(retirement);
        drop(supplied_owners);
        let mut lifelines = lifelines.into_iter();
        while let Some(lifeline) = lifelines.next() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(lifeline))).is_err() {
                let remaining = lifelines.collect::<Vec<_>>();
                let mut retained = self.retained.lock().unwrap_or_else(|p| p.into_inner());
                retained.lifelines.extend(remaining);
                retained.phase = PooledStartPhase::Unsettled;
                retained.admission_refused = true;
                bail!("pooled persistent-session lifeline destructor unwound after checked reap");
            }
        }
        self.retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .phase = PooledStartPhase::Settled;
        Ok(())
    }
}

fn stage_pooled_obligation_owners(
    retained: &mut PooledStartRetention,
    resource: &Option<PersistentSessionCleanupObserver>,
    retirement: &Option<PersistentSessionCleanupObserver>,
) {
    for supplied in [resource.as_ref(), retirement.as_ref()]
        .into_iter()
        .flatten()
    {
        if !retained
            .obligation_owners
            .iter()
            .any(|owner| Arc::ptr_eq(owner, supplied))
        {
            retained.obligation_owners.push(Arc::clone(supplied));
        }
    }
}

fn admit_original_pooled_obligations(
    retained: &mut PooledStartRetention,
    resource: Option<PersistentSessionCleanupObserver>,
    retirement: Option<PersistentSessionCleanupObserver>,
) -> Result<()> {
    for (original, supplied) in [
        (&retained.resource_observer, &resource),
        (&retained.retirement_observer, &retirement),
    ] {
        if let (Some(original), Some(supplied)) = (original, supplied) {
            if !Arc::ptr_eq(original, supplied) {
                bail!("pooled start substituted an original cleanup obligation");
            }
        }
    }
    let selected_resource = retained.resource_observer.as_ref().or(resource.as_ref());
    let selected_retirement = retained
        .retirement_observer
        .as_ref()
        .or(retirement.as_ref());
    if let (Some(resource), Some(retirement)) = (selected_resource, selected_retirement) {
        if Arc::ptr_eq(resource, retirement) {
            bail!("one cleanup closure cannot substitute two distinct pooled obligations");
        }
    }
    // Commit both only after ALL checks; new other-kind owners are not
    // executable when a different original kind makes the call fail.
    if retained.resource_observer.is_none() {
        retained.resource_observer = resource;
    }
    if retained.retirement_observer.is_none() {
        retained.retirement_observer = retirement;
    }
    Ok(())
}

impl SessionGroup {
    fn process_count(&self) -> usize {
        self.processes.len().saturating_add(
            self.starts
                .iter()
                .filter(|start| {
                    !self.processes.iter().any(|process| {
                        process
                            .pooled_start
                            .as_ref()
                            .is_some_and(|owned| Arc::ptr_eq(owned, start))
                    })
                })
                .count(),
        )
    }
}

/// A callback/ready unwind retains the same original group slot and poisons
/// admission. Weak pool ownership prevents a registry/custody reference cycle.
struct PooledStartProgress {
    inner: Weak<PoolInner>,
    original_start: Arc<PersistentSessionPooledStart>,
    active: bool,
}

impl Drop for PooledStartProgress {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(inner) = self.inner.upgrade() {
            let mut state = inner.state.lock().unwrap_or_else(|p| p.into_inner());
            state.cleanup_unproved.get_or_insert_with(|| {
                "pooled start callback/readiness unwound before ownership publication".to_owned()
            });
            self.original_start.retain_unsettled();
            self.original_start.complete_callback();
            inner.changed.notify_all();
        } else {
            self.original_start.retain_unsettled();
        }
    }
}

struct PooledReadyFailure {
    error: anyhow::Error,
    cleanup_unproved: bool,
    original_start: Arc<PersistentSessionPooledStart>,
}

/// Values that must remain reachable when an exclusive start cannot establish
/// relay, reap, or settlement proof. Kept in the reservation entry so capacity
/// and owner custody stay in the same registry lane.
struct ExclusiveUnprovedStartCustody {
    relay_health: Option<PersistentSessionRelayHealthCell>,
    relay_owner: Option<Arc<dyn PersistentSessionRelayOwner>>,
    lifelines: Vec<Box<dyn Send + Sync>>,
    observer: Option<PersistentSessionCleanupObserver>,
}

enum ExclusiveQuarantinedOwner {
    Start(ExclusiveUnprovedStartCustody),
    ReadyProcess(Arc<SessionProcess>),
}

struct ExclusiveReservationEntry {
    contract: GroupContract,
    quarantined: Vec<ExclusiveQuarantinedOwner>,
}

#[derive(Clone)]
struct ExclusiveCustodyRegistration {
    inner: Arc<PoolInner>,
    session_id: String,
}

struct ExclusiveReadyCustodyGuard {
    custody: Option<ExclusiveUnprovedStartCustody>,
    registration: Option<ExclusiveCustodyRegistration>,
}

struct ExclusiveReadyProcessGuard {
    process: Option<Arc<SessionProcess>>,
    registration: ExclusiveCustodyRegistration,
}

impl ExclusiveReadyProcessGuard {
    fn new(process: Arc<SessionProcess>, registration: ExclusiveCustodyRegistration) -> Self {
        Self {
            process: Some(process),
            registration,
        }
    }

    fn disarm(&mut self) {
        self.process.take();
    }
}

impl Drop for ExclusiveReadyProcessGuard {
    fn drop(&mut self) {
        let Some(process) = self.process.take() else { return };
        let mut state = self
            .registration
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(reservation) = state
            .exclusive_reservations
            .get_mut(&self.registration.session_id)
        else {
            state.cleanup_unproved.get_or_insert_with(|| {
                "exclusive process retirement unwound after its reservation disappeared".to_owned()
            });
            return;
        };
        reservation
            .quarantined
            .push(ExclusiveQuarantinedOwner::ReadyProcess(process));
        state
            .cleanup_unproved
            .get_or_insert_with(|| "exclusive process retirement unwound before registry publication".to_owned());
        self.registration.inner.changed.notify_all();
    }
}

impl ExclusiveReadyCustodyGuard {
    fn take(&mut self) -> ExclusiveUnprovedStartCustody {
        self.custody
            .take()
            .expect("readiness custody guard owns its exact start values")
    }
}

impl std::ops::Deref for ExclusiveReadyCustodyGuard {
    type Target = ExclusiveUnprovedStartCustody;

    fn deref(&self) -> &Self::Target {
        self.custody
            .as_ref()
            .expect("readiness custody remains armed until transfer")
    }
}

impl Drop for ExclusiveReadyCustodyGuard {
    fn drop(&mut self) {
        let (Some(custody), Some(registration)) = (self.custody.take(), self.registration.take())
        else {
            return;
        };
        let mut state = registration
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(reservation) = state.exclusive_reservations.get_mut(&registration.session_id) else {
            state.cleanup_unproved.get_or_insert_with(|| {
                "exclusive readiness unwound after its reservation disappeared".to_owned()
            });
            return;
        };
        reservation
            .quarantined
            .push(ExclusiveQuarantinedOwner::Start(custody));
        state
            .cleanup_unproved
            .get_or_insert_with(|| "exclusive readiness unwound before ownership publication".to_owned());
        registration.inner.changed.notify_all();
    }
}

struct PoolState {
    groups: HashMap<String, SessionGroup>,
    exclusive: HashMap<String, ExclusiveSessionEntry>,
    exclusive_reservations: HashMap<String, ExclusiveReservationEntry>,
    exclusive_failure_cleanup: HashMap<String, &'static str>,
    /// Starts admitted by `reserve_exclusive` that have not yet completed the
    /// caller's durable binding publication. Shutdown waits for these exact
    /// ownership units before snapshotting and reaping the pool.
    exclusive_starts_in_flight: usize,
    /// Once process-tree cleanup cannot be proved, the pool admits no further
    /// work in this daemon generation. Capacity must never be recycled while
    /// an old ownership unit may still exist.
    cleanup_unproved: Option<String>,
}

struct ExclusiveSessionEntry {
    contract: GroupContract,
    process: Arc<SessionProcess>,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum PooledStartPauseStage {
    OutcomePublication,
    ReaderAdmission,
}
#[cfg(test)]
struct PooledStartTestPause {
    stage: PooledStartPauseStage,
    entered: SyncSender<()>,
    release: Mutex<Receiver<()>>,
}

struct PoolInner {
    state: Mutex<PoolState>,
    #[cfg(test)]
    pooled_start_test_pause: Mutex<Option<Arc<PooledStartTestPause>>>,
    changed: Condvar,
    limits: PersistentSessionPoolLimits,
    enabled: bool,
    shutdown: Arc<AtomicBool>,
}

struct ReadyProcessFailure {
    error: anyhow::Error,
    cleanup_unproved: bool,
    custody: Option<ExclusiveUnprovedStartCustody>,
}

/// Typed evidence that an exclusive bind failure consumed process ownership
/// without proving the exact process group reaped. Callers must preserve the
/// durable worker and credential fences when this marker is present.
#[derive(Debug)]
pub struct PersistentSessionCleanupUnproved;

impl std::fmt::Display for PersistentSessionCleanupUnproved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("persistent-session cleanup is unproved")
    }
}

impl std::error::Error for PersistentSessionCleanupUnproved {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionStreamEvent {
    pub sequence: u64,
    pub kind: PersistentSessionStreamEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistentSessionStreamEventKind {
    Delta,
    Final,
    Error,
}

#[derive(Debug)]
pub struct PersistentSessionStreamPage {
    pub events: Vec<PersistentSessionStreamEvent>,
    pub terminal: bool,
}

struct StreamBuffer {
    next_sequence: u64,
    events: VecDeque<PersistentSessionStreamEvent>,
    terminal: bool,
    backlog_bytes: usize,
}

struct StreamRecord {
    owner: String,
    quota_subject: String,
    cancelled: Arc<AtomicBool>,
    buffer: Mutex<StreamBuffer>,
    changed: Condvar,
    last_touched_ms: AtomicU64,
    backlog: Arc<BacklogBudget>,
    max_backlog_bytes: usize,
}

struct StreamRegistry {
    records: Mutex<HashMap<String, Arc<StreamRecord>>>,
    reservations: Mutex<StreamReservationState>,
    limits: PersistentSessionPoolLimits,
    backlog: Arc<BacklogBudget>,
    executor: StreamExecutor,
    enabled: bool,
    shutdown: Arc<AtomicBool>,
}

type StreamTask = Box<dyn FnOnce() + Send + 'static>;

struct StreamExecutor {
    sender: std::sync::mpsc::SyncSender<StreamTask>,
}

impl StreamExecutor {
    fn new(worker_count: usize) -> Result<Self> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(worker_count);
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("ryeos-persistent-stream-{index}"))
                .spawn(move || {
                    loop {
                        let task = receiver
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .recv();
                        let Ok(task) = task else {
                            break;
                        };
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
                    }
                })
                .context("spawn persistent-session bounded stream worker")?;
        }
        Ok(Self { sender })
    }

    fn submit(&self, task: StreamTask) -> Result<()> {
        self.sender
            .send(task)
            .map_err(|_| anyhow!("persistent-session bounded stream executor is unavailable"))
    }
}

#[derive(Default)]
struct StreamReservationState {
    owners: HashMap<String, String>,
}

/// Capacity acquired before the caller crosses an irreversible contact
/// boundary. Dropping an unused reservation returns all quota immediately.
pub struct PersistentSessionStreamReservation {
    streams: Arc<StreamRegistry>,
    owner: String,
    quota_subject: String,
    active: bool,
}

struct BacklogBudget {
    bytes: Mutex<usize>,
    changed: Condvar,
    max_bytes: usize,
}

struct BacklogBytePermit {
    backlog: Arc<BacklogBudget>,
    bytes: usize,
}

impl BacklogBytePermit {
    fn try_reserve(backlog: Arc<BacklogBudget>, bytes: usize) -> Result<Self> {
        if bytes == 0 || bytes > backlog.max_bytes {
            bail!("persistent-session frame exceeds the node IPC byte budget");
        }
        let mut retained = backlog
            .bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if retained.saturating_add(bytes) > backlog.max_bytes {
            bail!("persistent-session node IPC byte budget is exhausted");
        }
        *retained += bytes;
        drop(retained);
        Ok(Self { backlog, bytes })
    }

    fn reserve_wait(
        backlog: Arc<BacklogBudget>,
        bytes: usize,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        if bytes == 0 || bytes > backlog.max_bytes {
            bail!("persistent-session frame exceeds the node IPC byte budget");
        }
        let mut retained = backlog
            .bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if cancelled.load(Ordering::Acquire) {
                bail!("persistent-session process closed while awaiting IPC byte capacity");
            }
            if retained.saturating_add(bytes) <= backlog.max_bytes {
                *retained += bytes;
                drop(retained);
                return Ok(Self { backlog, bytes });
            }
            let (next, _) = backlog
                .changed
                .wait_timeout(retained, IO_POLL_INTERVAL)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            retained = next;
        }
    }
}

impl Drop for BacklogBytePermit {
    fn drop(&mut self) {
        release_backlog_bytes(&self.backlog, self.bytes);
    }
}

#[derive(Clone)]
pub struct PersistentSessionPool {
    inner: Arc<PoolInner>,
    streams: Arc<StreamRegistry>,
}

/// Exact process-registry evidence returned by an exclusive retirement.
/// `Absent` and `Reserved` are intentionally distinct from `Reaped`: neither
/// is process-death proof for a durable worker identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusiveRetirementOutcome {
    Reaped,
    Unproved,
    Reserved,
    Absent,
}

/// A node-capacity reservation for one exclusive persistent subprocess.
/// Dropping it before binding releases the reservation without contacting a
/// worker. The process can never enter a pooled group.
pub struct ExclusivePersistentSessionReservation {
    inner: Arc<PoolInner>,
    backlog: Arc<BacklogBudget>,
    session_id: String,
    contract: GroupContract,
    active: bool,
    start_guard: Option<ExclusivePersistentSessionStartGuard>,
}

/// Move-only ownership of one admitted exclusive start. It begins at capacity
/// reservation and remains live through held spawn, durable attachment,
/// process release, pool binding, and the caller's durable binding commit.
pub struct ExclusivePersistentSessionStartGuard {
    inner: Arc<PoolInner>,
    active: bool,
}

impl Default for PersistentSessionPool {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistentSessionPool {
    pub fn new() -> Self {
        Self::with_limits(PersistentSessionPoolLimits::default())
            .expect("built-in persistent-session limits are valid")
    }

    pub fn with_limits(limits: PersistentSessionPoolLimits) -> Result<Self> {
        Self::build(limits, true)
    }

    pub fn disabled() -> Self {
        Self::build(PersistentSessionPoolLimits::default(), false)
            .expect("built-in disabled persistent-session limits are valid")
    }

    fn build(limits: PersistentSessionPoolLimits, enabled: bool) -> Result<Self> {
        limits.validate()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(PoolInner {
            #[cfg(test)]
            pooled_start_test_pause: Mutex::new(None),
            state: Mutex::new(PoolState {
                groups: HashMap::new(),
                exclusive: HashMap::new(),
                exclusive_reservations: HashMap::new(),
                exclusive_failure_cleanup: HashMap::new(),
                exclusive_starts_in_flight: 0,
                cleanup_unproved: None,
            }),
            changed: Condvar::new(),
            limits: limits.clone(),
            enabled,
            shutdown: Arc::clone(&shutdown),
        });
        spawn_idle_reaper(Arc::downgrade(&inner));
        let backlog = Arc::new(BacklogBudget {
            bytes: Mutex::new(0),
            changed: Condvar::new(),
            max_bytes: limits.max_total_backlog_bytes,
        });
        let stream_executor = StreamExecutor::new(if enabled {
            limits.max_active_streams
        } else {
            1
        })?;
        let streams = Arc::new(StreamRegistry {
            records: Mutex::new(HashMap::new()),
            reservations: Mutex::new(StreamReservationState::default()),
            limits,
            backlog,
            executor: stream_executor,
            enabled,
            shutdown,
        });
        spawn_stream_reaper(Arc::downgrade(&streams));
        Ok(Self { inner, streams })
    }

    /// Read-only policy and lifecycle eligibility check for an exact prepared
    /// session contract. This does not reserve capacity, create a pool group,
    /// mint a lease, or promise that transient capacity will still be
    /// available when execution reaches its authoritative reservation.
    pub fn validate_contract_eligibility(
        &self,
        lifecycle: &PersistentSessionLifecycleContract,
        wire: &PersistentSessionWireContract,
    ) -> Result<PersistentSessionContractEligibility> {
        lifecycle.validate()?;
        wire.validate()?;
        if !self.inner.enabled {
            return Ok(PersistentSessionContractEligibility::Disabled);
        }
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Ok(PersistentSessionContractEligibility::Shutdown);
        }
        if lifecycle.real_uid_process_limit > self.inner.limits.max_real_uid_process_limit
            || lifecycle.max_address_space_bytes > self.inner.limits.max_total_address_space_bytes
            || lifecycle.max_cpu_seconds > self.inner.limits.max_total_cpu_seconds
        {
            return Ok(PersistentSessionContractEligibility::ResourceRequestExceedsPolicy);
        }
        if usize::try_from(wire.max_frame_bytes)
            .ok()
            .and_then(|bytes| bytes.checked_add(4))
            .is_none_or(|bytes| bytes > self.streams.limits.max_total_backlog_bytes)
        {
            return Ok(PersistentSessionContractEligibility::WireFrameExceedsPolicy);
        }
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.cleanup_unproved.is_some() {
            return Ok(PersistentSessionContractEligibility::CleanupUnproved);
        }
        Ok(PersistentSessionContractEligibility::Ready)
    }

    /// Reserve node-wide capacity for one session-owned process. This shares
    /// the same aggregate admission counters as pooled persistent workers but
    /// never grants request reuse or cross-session lookup.
    pub fn reserve_exclusive(
        &self,
        session_id: &str,
        lifecycle: &PersistentSessionLifecycleContract,
        wire: &PersistentSessionWireContract,
    ) -> Result<ExclusivePersistentSessionReservation> {
        if !self.inner.enabled {
            bail!("persistent sessions are disabled by node policy");
        }
        self.ensure_admission_open()?;
        validate_exclusive_session_id(session_id)?;
        lifecycle.validate()?;
        wire.validate()?;
        if lifecycle.real_uid_process_limit > self.inner.limits.max_real_uid_process_limit {
            bail!("persistent-session real-UID process request exceeds node policy");
        }
        let contract = GroupContract {
            lifecycle: lifecycle.clone(),
            wire: wire.clone(),
        };
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_admission_open()?;
        if let Some(reason) = state.cleanup_unproved.as_deref() {
            bail!(
                "persistent-session ownership is quarantined after unproved process cleanup: {reason}"
            );
        }
        if state.exclusive.contains_key(session_id)
            || state.exclusive_reservations.contains_key(session_id)
            || state.exclusive_failure_cleanup.contains_key(session_id)
        {
            bail!("exclusive persistent session already exists");
        }
        let (processes, address_space, cpu_seconds) = aggregate_process_capacity(&state);
        if processes >= self.inner.limits.max_total_processes
            || address_space.saturating_add(lifecycle.max_address_space_bytes)
                > self.inner.limits.max_total_address_space_bytes
            || cpu_seconds.saturating_add(lifecycle.max_cpu_seconds)
                > self.inner.limits.max_total_cpu_seconds
        {
            bail!("persistent-session node process capacity is exhausted");
        }
        let exclusive_starts_in_flight = state
            .exclusive_starts_in_flight
            .checked_add(1)
            .ok_or_else(|| anyhow!("exclusive persistent-session start counter overflow"))?;
        state
            .exclusive_reservations
            .insert(
                session_id.to_owned(),
                ExclusiveReservationEntry {
                    contract: contract.clone(),
                    quarantined: Vec::new(),
                },
            );
        state.exclusive_starts_in_flight = exclusive_starts_in_flight;
        Ok(ExclusivePersistentSessionReservation {
            inner: Arc::clone(&self.inner),
            backlog: Arc::clone(&self.streams.backlog),
            session_id: session_id.to_owned(),
            contract,
            active: true,
            start_guard: Some(ExclusivePersistentSessionStartGuard {
                inner: Arc::clone(&self.inner),
                active: true,
            }),
        })
    }

    pub fn execute_exclusive<C, D>(
        &self,
        session_id: &str,
        request_body: Value,
        cancelled: C,
        mut on_delta: D,
    ) -> Result<Value>
    where
        C: Fn() -> bool,
        D: FnMut(Value) -> Result<()>,
    {
        self.execute_exclusive_with_deadline(
            session_id,
            request_body,
            cancelled,
            |value| on_delta(value).map(|()| None),
            None,
        )
    }

    /// Apply an already-admitted absolute deadline to the ordinary request
    /// I/O deadline, including time waiting for the shared writer.
    /// A delta consumer may return an acknowledgement only after accepting
    /// that exact request-correlated progress through its authority owner.
    pub fn execute_exclusive_with_deadline<C, D>(
        &self,
        session_id: &str,
        request_body: Value,
        cancelled: C,
        on_delta: D,
        absolute_deadline: Option<MonotonicDeadline>,
    ) -> Result<Value>
    where
        C: Fn() -> bool,
        D: FnMut(Value) -> Result<Option<Value>>,
    {
        self.execute_exclusive_attributed_with_deadline(
            session_id,
            None,
            request_body,
            cancelled,
            on_delta,
            absolute_deadline,
        )
    }

    /// Check the selected exclusive session's complete Request frame before a
    /// domain owner commits its one-use possible-contact boundary. The actual
    /// request id consists of two decimal u64 values separated by `-`; using
    /// their maximum width makes this a conservative size check. This is only
    /// a preflight: the sender still checks the real frame at the write edge.
    pub fn ensure_exclusive_request_fits(
        &self,
        session_id: &str,
        request_body: &Value,
        expected_wire_max_bytes: u32,
    ) -> Result<()> {
        self.ensure_admission_open()?;
        validate_exclusive_session_id(session_id)?;
        let wire = {
            let state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let entry = state
                .exclusive
                .get(session_id)
                .ok_or_else(|| anyhow!("exclusive persistent session is not attached"))?;
            entry.contract.wire.clone()
        };
        if wire.max_frame_bytes != expected_wire_max_bytes {
            bail!("exclusive Request changed its selected signed wire maximum");
        }
        let maximum_request_id = format!("{}-{}", u64::MAX, u64::MAX);
        encode_frame(
            &wire,
            &PersistentSessionFrame {
                protocol: wire.wire_protocol.clone(),
                version: wire.wire_version,
                kind: PersistentSessionFrameKind::Request,
                request_id: Some(maximum_request_id),
                body: Some(request_body.clone()),
            },
        )?;
        Ok(())
    }

    pub fn execute_exclusive_attributed_with_deadline<C, D>(
        &self,
        session_id: &str,
        request_identity: Option<&PersistentSessionRequestIdentity>,
        request_body: Value,
        cancelled: C,
        mut on_delta: D,
        absolute_deadline: Option<MonotonicDeadline>,
    ) -> Result<Value>
    where
        C: Fn() -> bool,
        D: FnMut(Value) -> Result<Option<Value>>,
    {
        self.ensure_admission_open()?;
        validate_exclusive_session_id(session_id)?;
        let (process, contract) = {
            let state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.ensure_admission_open()?;
            let entry = state
                .exclusive
                .get(session_id)
                .ok_or_else(|| anyhow!("exclusive persistent session is not attached"))?;
            (Arc::clone(&entry.process), entry.contract.clone())
        };
        if process
            .leased
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!("exclusive persistent session already has an active request");
        }
        if let Err(error) =
            require_resource_service_window(&process, contract.lifecycle.request_timeout_ms)
        {
            let retirement = self.retire_exclusive(session_id)?;
            process.leased.store(false, Ordering::Release);
            return Err(match retirement {
                ExclusiveRetirementOutcome::Reaped | ExclusiveRetirementOutcome::Absent => error,
                ExclusiveRetirementOutcome::Reserved => error.context(
                    "resource lifetime expired while the exclusive session remained reserved",
                ),
                ExclusiveRetirementOutcome::Unproved => error
                    .context("resource lifetime expired and exclusive cleanup could not be proved"),
            });
        }
        let (resource_request_lease, attribution_start) =
            match begin_resource_request_evidence(&process, request_identity, true) {
                Ok(evidence) => evidence,
                Err(error) => {
                    let cleanup = process.retire().err();
                    process.leased.store(false, Ordering::Release);
                    return Err(match cleanup {
                        Some(cleanup) => error.context(format!(
                            "exclusive resource-owner cleanup could not be proved: {cleanup}"
                        )),
                        None => error,
                    });
                }
            };
        let deadline =
            exclusive_request_deadline(contract.lifecycle.request_timeout_ms, absolute_deadline);
        let mut result = execute_on_process(
            &process,
            &contract.wire,
            PersistentSessionFrameKind::Request,
            request_body,
            &cancelled,
            deadline,
            &mut on_delta,
        )
        .map_err(|error| attach_process_diagnostic(&process, error));
        finish_resource_request_evidence(
            &process,
            request_identity,
            attribution_start.as_ref(),
            resource_request_lease,
            &mut result,
        );
        if result.is_err() {
            let cleanup = process.retire().err();
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let cleanup_state = if let Some(cleanup) = cleanup {
                state
                    .cleanup_unproved
                    .get_or_insert_with(|| cleanup.to_string());
                "unproved"
            } else {
                if state
                    .exclusive
                    .get(session_id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.process, &process))
                {
                    state.exclusive.remove(session_id);
                }
                "reaped"
            };
            state
                .exclusive_failure_cleanup
                .insert(session_id.to_owned(), cleanup_state);
            self.inner.changed.notify_all();
        } else {
            process.last_used_ms.store(now_ms(), Ordering::Release);
            process.leased.store(false, Ordering::Release);
            self.inner.changed.notify_all();
        }
        result
    }

    /// Deliver an authority-bearing daemon control frame. This surface is
    /// intentionally distinct from the public opaque-command path.
    pub fn execute_exclusive_control(
        &self,
        session_id: &str,
        control_body: Value,
    ) -> Result<Value> {
        self.execute_exclusive_control_with_deadline(session_id, control_body, None)
    }

    pub fn execute_exclusive_control_with_deadline(
        &self,
        session_id: &str,
        control_body: Value,
        absolute_deadline: Option<MonotonicDeadline>,
    ) -> Result<Value> {
        self.ensure_admission_open()?;
        validate_exclusive_session_id(session_id)?;
        let (process, contract) = {
            let state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.ensure_admission_open()?;
            let entry = state
                .exclusive
                .get(session_id)
                .ok_or_else(|| anyhow!("exclusive persistent session is not attached"))?;
            (Arc::clone(&entry.process), entry.contract.clone())
        };
        if process
            .leased
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!("exclusive persistent session already has an active control request");
        }
        if let Err(error) =
            require_resource_service_window(&process, contract.lifecycle.request_timeout_ms)
        {
            let retirement = self.retire_exclusive(session_id)?;
            process.leased.store(false, Ordering::Release);
            return Err(match retirement {
                ExclusiveRetirementOutcome::Reaped | ExclusiveRetirementOutcome::Absent => error,
                ExclusiveRetirementOutcome::Reserved => error.context(
                    "resource lifetime expired while the exclusive session remained reserved",
                ),
                ExclusiveRetirementOutcome::Unproved => error
                    .context("resource lifetime expired and exclusive cleanup could not be proved"),
            });
        }
        let (resource_request_lease, _) =
            match begin_resource_request_evidence(&process, None, false) {
                Ok(evidence) => evidence,
                Err(error) => {
                    let cleanup = process.retire().err();
                    process.leased.store(false, Ordering::Release);
                    return Err(match cleanup {
                    Some(cleanup) => error.context(format!(
                        "exclusive control resource-owner cleanup could not be proved: {cleanup}"
                    )),
                    None => error,
                });
                }
            };
        let deadline =
            exclusive_request_deadline(contract.lifecycle.request_timeout_ms, absolute_deadline);
        let mut result = execute_on_process(
            &process,
            &contract.wire,
            PersistentSessionFrameKind::Control,
            control_body,
            &|| false,
            deadline,
            &mut |_| Ok(None),
        )
        .map_err(|error| attach_process_diagnostic(&process, error));
        finish_resource_request_evidence(&process, None, None, resource_request_lease, &mut result);
        if result.is_err() {
            let cleanup = process.retire().err();
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let cleanup_state = if let Some(cleanup) = cleanup {
                state
                    .cleanup_unproved
                    .get_or_insert_with(|| cleanup.to_string());
                "unproved"
            } else {
                if state
                    .exclusive
                    .get(session_id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.process, &process))
                {
                    state.exclusive.remove(session_id);
                }
                "reaped"
            };
            state
                .exclusive_failure_cleanup
                .insert(session_id.to_owned(), cleanup_state);
            self.inner.changed.notify_all();
        } else {
            process.last_used_ms.store(now_ms(), Ordering::Release);
            process.leased.store(false, Ordering::Release);
            self.inner.changed.notify_all();
        }
        result
    }

    /// Consume the cleanup proof recorded when an exclusive request failed.
    /// The caller uses this to fence the matching durable worker/session epoch;
    /// no replacement reservation is admitted while the proof is unconsumed.
    pub fn take_exclusive_failure_cleanup_state(
        &self,
        session_id: &str,
    ) -> Result<Option<&'static str>> {
        validate_exclusive_session_id(session_id)?;
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(state.exclusive_failure_cleanup.remove(session_id))
    }

    /// Consume cleanup testimony after the durable possible-contact boundary.
    ///
    /// A missing in-memory proof is never evidence that the worker was reaped.
    /// It can occur when an exclusive request refuses before the process I/O
    /// helper installs its ordinary retirement result. The durable owner has
    /// already recorded possible contact at this point, so it must fence the
    /// worker as `unproved` and settle the command outcome as unknown instead
    /// of abandoning that settlement with a second error.
    pub fn take_exclusive_failure_cleanup_state_or_unproved(
        &self,
        session_id: &str,
    ) -> Result<&'static str> {
        Ok(self
            .take_exclusive_failure_cleanup_state(session_id)?
            .unwrap_or("unproved"))
    }

    /// Read the existing exclusive owner without consuming any cleanup proof.
    /// `None` means pool absence only; callers still need durable no-contact or
    /// exact process-death authority. Pending and uncertain owners always refuse.
    pub fn exclusive_capture_boot_identity(&self, session_id: &str) -> Result<Option<String>> {
        validate_exclusive_session_id(session_id)?;
        let process = {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| anyhow!("persistent-session pool poisoned"))?;
            if state.cleanup_unproved.is_some()
                || state.exclusive_reservations.contains_key(session_id)
                || state.exclusive_failure_cleanup.contains_key(session_id)
            {
                bail!(
                    "workspace capture is fenced by pending or unsettled exclusive worker contact"
                );
            }
            let Some(entry) = state.exclusive.get(session_id) else {
                return Ok(None);
            };
            Arc::clone(&entry.process)
        };
        // Retirement may hold the process slot during a blocking reap. Never
        // hold the pool or another process mutex while waiting for that owner;
        // contended cleanup is not a stable capture point.
        let cleanup_unknown = process
            .cleanup_unproved
            .try_lock()
            .map_err(|_| anyhow!("session cleanup owner is busy or poisoned"))?
            .is_some();
        let running_present = process
            .running
            .try_lock()
            .map_err(|_| anyhow!("session process owner is busy or poisoned"))?
            .is_some();
        if process.closed.load(Ordering::Acquire) || cleanup_unknown || !running_present {
            bail!("workspace capture is fenced by a draining or cleanup-unknown exclusive worker");
        }
        let identity = process
            .expected_boot_identity
            .as_ref()
            .ok_or_else(|| anyhow!("exclusive capture owner has no verified boot identity"))?;
        Ok(Some(identity.clone()))
    }

    pub fn retire_exclusive(&self, session_id: &str) -> Result<ExclusiveRetirementOutcome> {
        validate_exclusive_session_id(session_id)?;
        let process = {
            let state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.exclusive_reservations.contains_key(session_id) {
                return Ok(ExclusiveRetirementOutcome::Reserved);
            }
            state
                .exclusive
                .get(session_id)
                .map(|entry| Arc::clone(&entry.process))
        };
        let Some(process) = process else {
            return Ok(ExclusiveRetirementOutcome::Absent);
        };
        let result = process.retire();
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let outcome = match result {
            Ok(()) => {
                if state
                    .exclusive
                    .get(session_id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.process, &process))
                {
                    state.exclusive.remove(session_id);
                }
                ExclusiveRetirementOutcome::Reaped
            }
            Err(error) => {
                state
                    .cleanup_unproved
                    .get_or_insert_with(|| error.to_string());
                ExclusiveRetirementOutcome::Unproved
            }
        };
        self.inner.changed.notify_all();
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute<F, C, D>(
        &self,
        pool_key: &str,
        lifecycle: &PersistentSessionLifecycleContract,
        wire: &PersistentSessionWireContract,
        request_body: Value,
        spawn: F,
        cancelled: C,
        on_delta: D,
    ) -> Result<Value>
    where
        F: FnMut() -> Result<StartedPersistentSession>,
        C: Fn() -> bool,
        D: FnMut(Value) -> Result<()>,
    {
        self.execute_attributed(
            pool_key,
            lifecycle,
            wire,
            None,
            request_body,
            spawn,
            cancelled,
            on_delta,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_attributed<F, C, D>(
        &self,
        pool_key: &str,
        lifecycle: &PersistentSessionLifecycleContract,
        wire: &PersistentSessionWireContract,
        request_identity: Option<&PersistentSessionRequestIdentity>,
        request_body: Value,
        mut spawn: F,
        cancelled: C,
        on_delta: D,
    ) -> Result<Value>
    where
        F: FnMut() -> Result<StartedPersistentSession>,
        C: Fn() -> bool,
        D: FnMut(Value) -> Result<()>,
    {
        self.execute_attributed_owned_start(
            pool_key,
            lifecycle,
            wire,
            request_identity,
            request_body,
            |_| spawn(),
            cancelled,
            on_delta,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_attributed_owned_start<F, C, D>(
        &self,
        pool_key: &str,
        lifecycle: &PersistentSessionLifecycleContract,
        wire: &PersistentSessionWireContract,
        request_identity: Option<&PersistentSessionRequestIdentity>,
        request_body: Value,
        mut spawn: F,
        cancelled: C,
        mut on_delta: D,
    ) -> Result<Value>
    where
        F: FnMut(&PersistentSessionPooledStart) -> Result<StartedPersistentSession>,
        C: Fn() -> bool,
        D: FnMut(Value) -> Result<()>,
    {
        if !self.inner.enabled {
            bail!("persistent sessions are disabled by node policy");
        }
        self.ensure_admission_open()?;
        validate_pool_key(pool_key)?;
        lifecycle.validate()?;
        wire.validate()?;
        let deadline =
            MonotonicDeadline::after(Duration::from_millis(lifecycle.request_timeout_ms));
        let process =
            self.acquire_owned(pool_key, lifecycle, wire, &mut spawn, &cancelled, deadline)?;
        let resource_request_lease = match &process.resource_eligibility {
            Some(eligible) => match eligible() {
                Ok(lease) => Some(lease),
                Err(error) => {
                    let cleanup = process.retire().err();
                    if let Some(cleanup) = cleanup.as_ref() {
                        self.poison_after_unproved_cleanup(cleanup.to_string());
                    } else {
                        self.remove(pool_key, &process);
                    }
                    process.leased.store(false, Ordering::Release);
                    self.inner.changed.notify_all();
                    return Err(match cleanup {
                        Some(cleanup) => error.context(format!(
                            "ineligible resource-owner cleanup could not be proved: {cleanup}"
                        )),
                        None => error,
                    });
                }
            },
            None => None,
        };
        let attribution_start = match (&process.resource_attribution_sink, request_identity) {
            (Some(_), Some(_)) => match lillux::time::occupancy_now()
                .map_err(anyhow::Error::msg)
                .context("sample persistent-session request occupancy start")
            {
                Ok(start) => Some(start),
                Err(error) => {
                    let cleanup = process.retire().err();
                    if let Some(cleanup) = cleanup.as_ref() {
                        self.poison_after_unproved_cleanup(cleanup.to_string());
                    } else {
                        self.remove(pool_key, &process);
                    }
                    process.leased.store(false, Ordering::Release);
                    self.inner.changed.notify_all();
                    return Err(match cleanup {
                        Some(cleanup) => error.context(format!(
                            "resource-meter failure cleanup could not be proved: {cleanup}"
                        )),
                        None => error,
                    });
                }
            },
            (Some(_), None) => {
                let error = anyhow!(
                    "resource-bearing persistent-session request lacks attribution identity"
                );
                let cleanup = process.retire().err();
                if let Some(cleanup) = cleanup.as_ref() {
                    self.poison_after_unproved_cleanup(cleanup.to_string());
                } else {
                    self.remove(pool_key, &process);
                }
                process.leased.store(false, Ordering::Release);
                self.inner.changed.notify_all();
                return Err(match cleanup {
                    Some(cleanup) => error.context(format!(
                        "unattributed resource-owner cleanup could not be proved: {cleanup}"
                    )),
                    None => error,
                });
            }
            (None, _) => None,
        };
        let result = execute_on_process(
            &process,
            wire,
            PersistentSessionFrameKind::Request,
            request_body,
            &cancelled,
            deadline,
            &mut |value| on_delta(value).map(|()| None),
        );
        let mut result = result.map_err(|error| attach_process_diagnostic(&process, error));
        if let (Some(sink), Some(identity), Some(start)) = (
            &process.resource_attribution_sink,
            request_identity,
            attribution_start.as_ref(),
        ) {
            let attribution = lillux::time::occupancy_now()
                .map_err(anyhow::Error::msg)
                .context("sample persistent-session request occupancy end")
                .and_then(|end| sink(identity, start, &end));
            if let Err(error) = attribution {
                result = Err(match result {
                    Ok(_) => error,
                    Err(request) => request.context(format!(
                        "resource request attribution also failed: {error:#}"
                    )),
                });
            }
        }
        if let Some(lease) = resource_request_lease
            && let Err(error) = lease.finish()
        {
            result = Err(match result {
                Ok(_) => error,
                Err(request) => request.context(format!(
                    "resource request lease cleanup also failed: {error:#}"
                )),
            });
        }
        match result {
            Ok(value) => {
                process.last_used_ms.store(now_ms(), Ordering::Release);
                process.leased.store(false, Ordering::Release);
                self.inner.changed.notify_all();
                Ok(value)
            }
            Err(error) => {
                let cleanup = process.retire().err();
                if let Some(cleanup) = cleanup.as_ref() {
                    self.poison_after_unproved_cleanup(cleanup.to_string());
                } else {
                    self.remove(pool_key, &process);
                }
                process.leased.store(false, Ordering::Release);
                self.inner.changed.notify_all();
                match cleanup {
                    Some(cleanup) => Err(error.context(cleanup.to_string())),
                    None => Err(error),
                }
            }
        }
    }

    /// Start one meaning-blind background stream. The caller supplies the
    /// admitted operation; this registry owns only bounded sequencing,
    /// backpressure, cancellation, retry-safe polling, and retirement.
    pub fn start_stream<F>(&self, owner: &str, quota_subject: &str, operation: F) -> Result<String>
    where
        F: FnOnce(Arc<AtomicBool>, Arc<dyn Fn(Value) -> Result<()> + Send + Sync>) -> Result<Value>
            + Send
            + 'static,
    {
        self.reserve_stream_capacity(owner, quota_subject)?
            .start(operation)
    }

    /// Reserve the complete registry/thread capacity for a new stream without
    /// making it visible as an active stream or contacting a worker. Callers
    /// with durable at-most-once ledgers must obtain this permit before they
    /// write the contact claim.
    pub fn reserve_stream_capacity(
        &self,
        owner: &str,
        quota_subject: &str,
    ) -> Result<PersistentSessionStreamReservation> {
        if !self.streams.enabled {
            bail!("persistent sessions are disabled by node policy");
        }
        if self.streams.shutdown.load(Ordering::Acquire) {
            bail!("persistent-session admission is closed for daemon shutdown");
        }
        validate_stream_owner(owner)?;
        validate_stream_owner(quota_subject)?;
        self.sweep_streams();
        let mut reservations = self
            .streams
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let records = self
            .streams
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.streams.shutdown.load(Ordering::Acquire) {
            bail!("persistent-session admission is closed for daemon shutdown");
        }
        if records.values().any(|record| record.owner == owner) {
            bail!("persistent-session stream owner already has a current-daemon stream");
        }
        if reservations.owners.contains_key(owner) {
            bail!("persistent-session stream owner already has a pending capacity reservation");
        }
        if records.len().saturating_add(reservations.owners.len())
            >= self.streams.limits.max_open_streams
        {
            bail!("persistent-session stream registry is at capacity");
        }
        let mut active = reservations.owners.len();
        let mut subject_active = reservations
            .owners
            .values()
            .filter(|subject| subject.as_str() == quota_subject)
            .count();
        for existing in records.values() {
            let terminal = existing
                .buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .terminal;
            if !terminal {
                active += 1;
                if existing.quota_subject == quota_subject {
                    subject_active += 1;
                }
            }
        }
        if active >= self.streams.limits.max_active_streams
            || subject_active >= self.streams.limits.max_active_streams_per_subject
        {
            bail!("persistent-session active stream quota is exhausted");
        }
        reservations
            .owners
            .insert(owner.to_owned(), quota_subject.to_owned());
        drop(records);
        drop(reservations);
        Ok(PersistentSessionStreamReservation {
            streams: Arc::clone(&self.streams),
            owner: owner.to_owned(),
            quota_subject: quota_subject.to_owned(),
            active: true,
        })
    }

    /// Return the current-daemon stream for an exact owner without creating
    /// one. Durable contact claims use this to distinguish an active retry
    /// from an unsafe refire after the stream registry was lost.
    pub fn existing_stream_id(&self, owner: &str) -> Result<Option<String>> {
        validate_stream_owner(owner)?;
        self.sweep_streams();
        let records = self
            .streams
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(records
            .iter()
            .find(|(_, record)| record.owner == owner)
            .map(|(stream_id, _)| stream_id.clone()))
    }

    pub fn poll_stream(
        &self,
        owner: &str,
        stream_id: &str,
        after_sequence: u64,
        wait_ms: u64,
        max_events: usize,
    ) -> Result<PersistentSessionStreamPage> {
        validate_stream_owner(owner)?;
        if !lillux::valid_hash(stream_id)
            || wait_ms > 30_000
            || max_events == 0
            || max_events > STREAM_BACKLOG_EVENTS
        {
            bail!("persistent-session stream poll is outside substrate bounds");
        }
        let record = self.stream_record(owner, stream_id)?;
        record.last_touched_ms.store(now_ms(), Ordering::Release);
        let mut buffer = record
            .buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let latest = buffer.next_sequence.saturating_sub(1);
        if after_sequence > latest {
            bail!("persistent-session stream cursor is ahead of the produced sequence");
        }
        while buffer
            .events
            .front()
            .is_some_and(|event| event.sequence <= after_sequence)
        {
            if let Some(event) = buffer.events.pop_front() {
                let bytes = stream_event_bytes(&event)?;
                buffer.backlog_bytes = buffer.backlog_bytes.saturating_sub(bytes);
                release_backlog_bytes(&record.backlog, bytes);
            }
        }
        record.changed.notify_all();
        if buffer.events.is_empty() && !buffer.terminal && wait_ms != 0 {
            let (next, _) = record
                .changed
                .wait_timeout(buffer, Duration::from_millis(wait_ms))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            buffer = next;
        }
        let events = buffer.events.iter().take(max_events).cloned().collect();
        Ok(PersistentSessionStreamPage {
            events,
            terminal: buffer.terminal,
        })
    }

    pub fn cancel_stream(&self, owner: &str, stream_id: &str) -> Result<()> {
        let record = self.stream_record(owner, stream_id)?;
        record.cancelled.store(true, Ordering::Release);
        record.last_touched_ms.store(now_ms(), Ordering::Release);
        record.changed.notify_all();
        Ok(())
    }

    pub fn close_stream(&self, owner: &str, stream_id: &str) -> Result<()> {
        validate_stream_owner(owner)?;
        let mut records = self
            .streams
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = records
            .get(stream_id)
            .cloned()
            .ok_or_else(|| anyhow!("persistent-session stream does not exist"))?;
        if record.owner != owner {
            bail!("persistent-session stream owner mismatch");
        }
        let terminal = record
            .buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .terminal;
        if !terminal {
            bail!("persistent-session stream cannot close before terminal outcome");
        }
        records.remove(stream_id);
        drop(records);
        release_stream_record(&record);
        Ok(())
    }

    /// Retire a current-daemon stream after a stronger durable outcome has
    /// become authoritative. This is intentionally owner-checked and releases
    /// every retained backlog byte before returning.
    pub fn retire_stream(&self, owner: &str, stream_id: &str) -> Result<()> {
        validate_stream_owner(owner)?;
        let mut records = self
            .streams
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = records
            .get(stream_id)
            .cloned()
            .ok_or_else(|| anyhow!("persistent-session stream does not exist"))?;
        if record.owner != owner {
            bail!("persistent-session stream owner mismatch");
        }
        records.remove(stream_id);
        drop(records);
        release_stream_record(&record);
        Ok(())
    }

    fn stream_record(&self, owner: &str, stream_id: &str) -> Result<Arc<StreamRecord>> {
        validate_stream_owner(owner)?;
        if !lillux::valid_hash(stream_id) {
            bail!("persistent-session stream id is not canonical");
        }
        let records = self
            .streams
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = records
            .get(stream_id)
            .cloned()
            .ok_or_else(|| anyhow!("persistent-session stream does not exist"))?;
        if record.owner != owner {
            bail!("persistent-session stream owner mismatch");
        }
        Ok(record)
    }

    fn sweep_streams(&self) {
        sweep_stream_registry(&self.streams);
    }

    #[cfg(test)]
    fn pause_pooled_start(&self, stage: PooledStartPauseStage) {
        let pause = self.inner.pooled_start_test_pause.lock().unwrap().clone();
        if let Some(pause) = pause.filter(|pause| pause.stage == stage) {
            pause.entered.send(()).unwrap();
            pause
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .expect("finite owned-start test pause was not released");
        }
    }

    #[cfg(test)]
    fn acquire<F, C>(
        &self,
        key: &str,
        lifecycle: &PersistentSessionLifecycleContract,
        wire: &PersistentSessionWireContract,
        spawn: &mut F,
        cancelled: &C,
        deadline: MonotonicDeadline,
    ) -> Result<Arc<SessionProcess>>
    where
        F: FnMut() -> Result<StartedPersistentSession>,
        C: Fn() -> bool,
    {
        self.acquire_owned(key, lifecycle, wire, &mut |_| spawn(), cancelled, deadline)
    }

    fn acquire_owned<F, C>(
        &self,
        key: &str,
        lifecycle: &PersistentSessionLifecycleContract,
        wire: &PersistentSessionWireContract,
        spawn: &mut F,
        cancelled: &C,
        deadline: MonotonicDeadline,
    ) -> Result<Arc<SessionProcess>>
    where
        F: FnMut(&PersistentSessionPooledStart) -> Result<StartedPersistentSession>,
        C: Fn() -> bool,
    {
        let expected = GroupContract {
            lifecycle: lifecycle.clone(),
            wire: wire.clone(),
        };
        if lifecycle.real_uid_process_limit > self.inner.limits.max_real_uid_process_limit {
            bail!("persistent-session real-UID process request exceeds node policy");
        }
        loop {
            self.ensure_admission_open()?;
            if cancelled() {
                self.remove_empty_group(key);
                bail!("persistent-session request was cancelled before worker contact");
            }
            if deadline.has_elapsed() {
                self.remove_empty_group(key);
                bail!("persistent-session request exceeded its signed timeout while queued");
            }
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.ensure_admission_open()?;
            if let Some(reason) = state.cleanup_unproved.as_deref() {
                bail!(
                    "persistent-session pool is quarantined after unproved process cleanup: {reason}"
                );
            }
            if !state.groups.contains_key(key)
                && state.groups.len() >= self.inner.limits.max_pool_groups
            {
                bail!("persistent-session pool group quota is exhausted");
            }
            let (total_processes, total_address_space, total_cpu_seconds) =
                aggregate_process_capacity(&state);
            let group = state
                .groups
                .entry(key.to_owned())
                .or_insert_with(|| SessionGroup {
                    contract: expected.clone(),
                    processes: Vec::new(),
                    starts: Vec::new(),
                });
            if group.contract.lifecycle != expected.lifecycle
                || group.contract.wire != expected.wire
            {
                bail!("persistent-session pool key was reused with a different contract");
            }
            if let Some(process) = group.processes.iter().find(|process| {
                resource_service_window_is_sufficient(process, lifecycle.request_timeout_ms)
                    && process
                        .leased
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
            }) {
                if cancelled() || deadline.has_elapsed() {
                    process.leased.store(false, Ordering::Release);
                    self.inner.changed.notify_all();
                    bail!("persistent-session request ended before worker contact");
                }
                return Ok(Arc::clone(process));
            }
            if group.process_count() < usize::from(lifecycle.max_processes)
                && total_processes < self.inner.limits.max_total_processes
                && total_address_space.saturating_add(lifecycle.max_address_space_bytes)
                    <= self.inner.limits.max_total_address_space_bytes
                && total_cpu_seconds.saturating_add(lifecycle.max_cpu_seconds)
                    <= self.inner.limits.max_total_cpu_seconds
            {
                if cancelled() || deadline.has_elapsed() {
                    bail!("persistent-session request ended before worker spawn");
                }
                let original_start = Arc::new(PersistentSessionPooledStart::new());
                group.starts.push(Arc::clone(&original_start));
                drop(state);
                let mut progress = PooledStartProgress {
                    inner: Arc::downgrade(&self.inner),
                    original_start: Arc::clone(&original_start),
                    active: true,
                };
                let mut started = match spawn(&original_start) {
                    Ok(started) => ready_pooled_process(
                        started,
                        Arc::clone(&original_start),
                        wire,
                        lifecycle,
                        Arc::clone(&self.streams.backlog),
                    ),
                    Err(error) => Err(PooledReadyFailure {
                        cleanup_unproved: original_start.has_retained_custody()
                            || error.is::<PersistentSessionCleanupUnproved>(),
                        error,
                        original_start: Arc::clone(&original_start),
                    }),
                };
                if let Err(failure) = &mut started {
                    // A ready/callback failure cannot erase an actual original
                    // trusted attempt just because ordinary cleanup completed.
                    failure.cleanup_unproved |=
                        failure.original_start.has_original_trusted_obligation();
                }
                let failure_reason = if let Err(failure) = &started {
                    if failure.cleanup_unproved {
                        failure.original_start.retain_unsettled();
                    } else {
                        if let Err(error) = failure.original_start.prove_cleanup() {
                            tracing::error!(%error, "original pooled failure cannot settle trusted funding");
                            failure.original_start.retain_unsettled();
                        }
                    }
                    Some(failure.error.to_string()) // Outside PoolState/owner destruction.
                } else {
                    None
                };
                #[cfg(test)]
                self.pause_pooled_start(PooledStartPauseStage::OutcomePublication);
                let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
                match started {
                    Ok(process) => {
                        let process = Arc::new(process);
                        let group = match state.groups.get_mut(key) {
                            Some(group) => group,
                            None => {
                                drop(state);
                                return Err(anyhow!(
                                    "persistent-session original start group disappeared"
                                ));
                            }
                        };
                        process.leased.store(true, Ordering::Release);
                        // The SAME start stays owned/in-flight through reader admission.
                        group.processes.push(Arc::clone(&process));
                        let original_published = original_start.publish();
                        let closed = self.inner.shutdown.load(Ordering::Acquire);
                        drop(state);
                        #[cfg(test)]
                        self.pause_pooled_start(PooledStartPauseStage::ReaderAdmission);
                        let admission = if !original_published {
                            Err(anyhow!("original pooled custody refuses ready publication"))
                        } else if closed {
                            Err(anyhow!(
                                "persistent-session admission is closed for daemon shutdown"
                            ))
                        } else {
                            process.start_reader(wire.clone())
                        };
                        if admission.is_ok() {
                            let mut state =
                                self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
                            if !self.inner.shutdown.load(Ordering::Acquire)
                                && !process.closed.load(Ordering::Acquire)
                            {
                                let group = state
                                    .groups
                                    .get_mut(key)
                                    .expect("original owned start prevents group removal");
                                group
                                    .starts
                                    .retain(|slot| !Arc::ptr_eq(slot, &original_start));
                                original_start.complete_callback();
                                progress.active = false;
                                self.inner.changed.notify_all();
                                return Ok(process);
                            }
                            drop(state);
                        }
                        let error = admission.err().unwrap_or_else(|| {
                            anyhow!("persistent-session start crossed daemon shutdown")
                        });
                        let cleanup = process.retire().err(); // No PoolState mutex.
                        let cleanup_reason = cleanup.as_ref().map(ToString::to_string);
                        if cleanup.is_none() {
                            self.remove(key, &process);
                        } else {
                            original_start.retain_unsettled();
                        }
                        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some(reason) = cleanup_reason {
                            state.cleanup_unproved.get_or_insert(reason);
                        }
                        if let Some(group) = state.groups.get_mut(key) {
                            group
                                .starts
                                .retain(|slot| !Arc::ptr_eq(slot, &original_start));
                        }
                        original_start.complete_callback();
                        progress.active = false;
                        if state.groups.get(key).is_some_and(|group| {
                            group.processes.is_empty() && group.starts.is_empty()
                        }) {
                            state.groups.remove(key);
                        }
                        self.inner.changed.notify_all();
                        return Err(match cleanup { Some(cleanup) => error.context(format!("persistent-session pre-registry cleanup remains unproved: {cleanup}")), None => error });
                    }
                    Err(failure) => {
                        if failure.cleanup_unproved {
                            state
                                .cleanup_unproved
                                .get_or_insert(failure_reason.unwrap());
                        } else if let Some(group) = state.groups.get_mut(key) {
                            group
                                .starts
                                .retain(|slot| !Arc::ptr_eq(slot, &failure.original_start));
                        }
                        failure.original_start.complete_callback();
                        progress.active = false;
                        if state.groups.get(key).is_some_and(|group| {
                            group.processes.is_empty() && group.starts.is_empty()
                        }) {
                            state.groups.remove(key);
                        }
                        self.inner.changed.notify_all();
                        return Err(failure.error);
                    }
                }
            }
            let remaining = deadline.remaining();
            let wait = remaining.min(IO_POLL_INTERVAL);
            let (next, _) = self
                .inner
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
            drop(state);
        }
    }

    fn remove(&self, key: &str, process: &Arc<SessionProcess>) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(group) = state.groups.get_mut(key) {
            group
                .processes
                .retain(|candidate| !Arc::ptr_eq(candidate, process));
            if group.processes.is_empty() && group.starts.is_empty() {
                state.groups.remove(key);
            }
        }
    }

    fn remove_empty_group(&self, key: &str) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .groups
            .get(key)
            .is_some_and(|group| group.processes.is_empty() && group.starts.is_empty())
        {
            state.groups.remove(key);
        }
    }

    fn poison_after_unproved_cleanup(&self, reason: String) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.cleanup_unproved.is_none() {
            state.cleanup_unproved = Some(reason);
        }
        self.inner.changed.notify_all();
    }

    fn ensure_admission_open(&self) -> Result<()> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            bail!("persistent-session admission is closed for daemon shutdown");
        }
        Ok(())
    }

    /// Permanently close current-daemon admission and prove every process
    /// owned by this pool has been reaped. Pooled request workers have no
    /// durable per-process row, so daemon shutdown must call this before it
    /// publishes its own exit. Exclusive workers are also reaped here; their
    /// durable worker epochs remain subject to the separate identity fence.
    pub fn shutdown_and_reap_all(&self, timeout: Duration) -> Result<usize> {
        self.inner.shutdown.store(true, Ordering::Release);
        self.inner.changed.notify_all();

        {
            let records = self
                .streams
                .records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for record in records.values() {
                record.cancelled.store(true, Ordering::Release);
                record.changed.notify_all();
            }
        }

        let deadline = MonotonicDeadline::after(timeout);
        let (pooled, exclusive, prior_unproved) = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while state
                .groups
                .values()
                .any(|group| group.starts.iter().any(|start| start.is_in_flight()))
                || state.exclusive_starts_in_flight != 0
            {
                let remaining = deadline.remaining();
                if remaining.is_zero() {
                    bail!("persistent-session shutdown timed out waiting for admitted starts");
                }
                let (next, wait) = self
                    .inner
                    .changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state = next;
                if wait.timed_out()
                    && (state
                        .groups
                        .values()
                        .any(|group| group.starts.iter().any(|start| start.is_in_flight()))
                        || state.exclusive_starts_in_flight != 0)
                {
                    bail!("persistent-session shutdown timed out waiting for admitted starts");
                }
            }
            if !state.exclusive_reservations.is_empty() {
                let retained = state
                    .exclusive_reservations
                    .values()
                    .map(ExclusiveReservationEntry::quarantined_owner_count)
                    .sum::<usize>();
                if retained > 0 {
                    bail!("persistent-session shutdown retains {retained} unproved exclusive custody owners in their charged reservations");
                }
                bail!("persistent-session shutdown found an exclusive reservation without a bound owner");
            }
            let pooled = state
                .groups
                .iter()
                .flat_map(|(key, group)| {
                    group
                        .processes
                        .iter()
                        .map(move |process| (key.clone(), Arc::clone(process)))
                })
                .collect::<Vec<_>>();
            let exclusive = state
                .exclusive
                .iter()
                .map(|(session_id, entry)| (session_id.clone(), Arc::clone(&entry.process)))
                .collect::<Vec<_>>();
            (pooled, exclusive, state.cleanup_unproved.clone())
        };

        let mut reaped = 0usize;
        let mut cleanup_errors = Vec::new();
        for (key, process) in pooled {
            match process.retire() {
                Ok(()) => {
                    reaped = reaped.saturating_add(1);
                    self.remove(&key, &process);
                }
                Err(error) => cleanup_errors.push(error.to_string()),
            }
        }
        for (session_id, process) in exclusive {
            match process.retire() {
                Ok(()) => {
                    reaped = reaped.saturating_add(1);
                    let mut state = self
                        .inner
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if state
                        .exclusive
                        .get(&session_id)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.process, &process))
                    {
                        state.exclusive.remove(&session_id);
                    }
                }
                Err(error) => cleanup_errors.push(error.to_string()),
            }
        }

        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .groups
            .retain(|_, group| !group.processes.is_empty() || !group.starts.is_empty());
        if let Some(reason) = prior_unproved {
            cleanup_errors.push(reason);
        }
        if let Some(reason) = state.cleanup_unproved.as_ref() {
            if !cleanup_errors.contains(reason) {
                cleanup_errors.push(reason.clone());
            }
        }
        if !cleanup_errors.is_empty() {
            let reason = cleanup_errors.join("; ");
            state.cleanup_unproved.get_or_insert_with(|| reason.clone());
            bail!("persistent-session shutdown cleanup remains unproved: {reason}");
        }
        if state
            .groups
            .values()
            .any(|group| !group.processes.is_empty() || !group.starts.is_empty())
            || !state.exclusive.is_empty()
        {
            bail!(
                "persistent-session shutdown left an owned process or start in the pool registry"
            );
        }
        self.inner.changed.notify_all();
        Ok(reaped)
    }
}

impl ExclusivePersistentSessionReservation {
    /// Complete readiness and publish the process into the exclusive registry.
    /// The caller must already have persisted the exact held-process identity
    /// before supplying a released process here.
    pub fn bind(
        mut self,
        started: StartedPersistentSession,
    ) -> Result<ExclusivePersistentSessionStartGuard> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            let StartedPersistentSession {
                running,
                relay_owner,
                relay_health,
                cleanup_observer,
                retirement_observer,
                lifelines,
                ..
            } = started;
            let custody = ExclusiveUnprovedStartCustody {
                relay_health,
                relay_owner,
                lifelines,
                observer: combine_retirement_observers(cleanup_observer, retirement_observer),
            };
            let relay_join_error = safe_relay_join(custody.relay_owner.as_ref()).err();
            let reap_error = safe_checked_reap(running).err();
            if relay_join_error.is_some() || reap_error.is_some() {
                let reason = format!(
                    "exclusive process crossed daemon shutdown without join/reap proof: relay={relay_join_error:?}; reap={reap_error:?}"
                );
                self.quarantine_start_custody(custody, reason.clone());
                return Err(anyhow!(
                    "exclusive process crossed daemon shutdown and cleanup could not be proved: {reason}"
                )
                .context(PersistentSessionCleanupUnproved));
            }
            let settlement = settle_ready_cleanup(
                anyhow!("persistent-session admission is closed for daemon shutdown"),
                custody,
            );
            if settlement.cleanup_unproved {
                let reason = settlement.error.to_string();
                if let Some(custody) = settlement.custody {
                    self.quarantine_start_custody(custody, reason.clone());
                }
                return Err(settlement.error.context(PersistentSessionCleanupUnproved));
            }
            self.release_reservation();
            return Err(anyhow!(
                "persistent-session admission is closed for daemon shutdown"
            ));
        }
        let registration = ExclusiveCustodyRegistration {
            inner: Arc::clone(&self.inner),
            session_id: self.session_id.clone(),
        };
        let (ready, mut ready_process_guard) = match ready_process_registered(
            started,
            &self.contract.wire,
            &self.contract.lifecycle,
            Arc::clone(&self.backlog),
            Some(registration.clone()),
        ) {
            Ok(process) => {
                let process = Arc::new(process);
                let mut process_guard =
                    ExclusiveReadyProcessGuard::new(Arc::clone(&process), registration);
                if self.inner.shutdown.load(Ordering::Acquire) {
                    let cleanup = process.retire().err();
                    return Err(match cleanup {
                        Some(cleanup) => {
                            let reason = format!(
                                "exclusive process crossed daemon shutdown and cleanup could not be proved: {cleanup}"
                            );
                            self.quarantine_ready_process(Arc::clone(&process), reason.clone());
                            process_guard.disarm();
                            anyhow!(reason).context(PersistentSessionCleanupUnproved)
                        }
                        None => {
                            self.release_reservation();
                            process_guard.disarm();
                            anyhow!("persistent-session admission is closed for daemon shutdown")
                        }
                    });
                }
                if let Err(error) = process.start_reader(self.contract.wire.clone()) {
                    let cleanup = process.retire().err();
                    return Err(match cleanup {
                        Some(cleanup) => {
                            let reason =
                                format!("exclusive reader start cleanup failed: {cleanup}");
                            self.quarantine_ready_process(Arc::clone(&process), reason.clone());
                            process_guard.disarm();
                            error
                                .context(reason)
                                .context(PersistentSessionCleanupUnproved)
                        }
                        None => {
                            self.release_reservation();
                            process_guard.disarm();
                            error
                        }
                    });
                }
                (process, process_guard)
            }
            Err(failure) => {
                if failure.cleanup_unproved {
                    if let Some(custody) = failure.custody {
                        self.quarantine_start_custody(custody, failure.error.to_string());
                    } else {
                        self.release_reservation();
                    }
                } else {
                    self.release_reservation();
                }
                return Err(if failure.cleanup_unproved {
                    failure.error.context(PersistentSessionCleanupUnproved)
                } else {
                    failure.error
                });
            }
        };
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.inner.shutdown.load(Ordering::Acquire) {
            drop(state);
            let cleanup = ready.retire().err();
            return Err(match cleanup {
                Some(cleanup) => {
                    let reason = format!(
                        "exclusive process crossed daemon shutdown and cleanup could not be proved: {cleanup}"
                    );
                    self.quarantine_ready_process(Arc::clone(&ready), reason.clone());
                    ready_process_guard.disarm();
                    anyhow!(reason).context(PersistentSessionCleanupUnproved)
                }
                None => {
                    self.release_reservation();
                    ready_process_guard.disarm();
                    anyhow!("persistent-session admission is closed for daemon shutdown")
                }
            });
        }
        let Some(reserved) = state.exclusive_reservations.get(&self.session_id) else {
            drop(state);
            let cleanup = ready.retire().err();
            return Err(match cleanup {
                Some(cleanup) => {
                    let reason = format!(
                        "exclusive persistent-session reservation was lost; cleanup failed: {cleanup}"
                    );
                    // A missing reservation violates the ownership invariant;
                    // retain the process in the existing exclusive owner map
                    // when possible before returning the quarantine marker.
                    let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
                    state.cleanup_unproved.get_or_insert_with(|| reason.clone());
                    state.exclusive.entry(self.session_id.clone()).or_insert_with(|| ExclusiveSessionEntry {
                        contract: self.contract.clone(),
                        process: Arc::clone(&ready),
                    });
                    self.active = false;
                    drop(state);
                    self.start_guard.take();
                    ready_process_guard.disarm();
                    anyhow!(reason).context(PersistentSessionCleanupUnproved)
                }
                None => {
                    self.release_reservation();
                    ready_process_guard.disarm();
                    anyhow!("exclusive persistent-session reservation was lost")
                }
            });
        };
        if reserved.contract.lifecycle != self.contract.lifecycle
            || reserved.contract.wire != self.contract.wire
        {
            drop(state);
            let cleanup = ready.retire().err();
            return Err(match cleanup {
                Some(cleanup) => {
                    let reason = format!(
                        "exclusive persistent-session reservation changed; cleanup failed: {cleanup}"
                    );
                    self.quarantine_ready_process(Arc::clone(&ready), reason.clone());
                    ready_process_guard.disarm();
                    anyhow!(reason).context(PersistentSessionCleanupUnproved)
                }
                None => {
                    self.release_reservation();
                    ready_process_guard.disarm();
                    anyhow!("exclusive persistent-session reservation changed")
                }
            });
        }
        match state.exclusive.entry(self.session_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(ExclusiveSessionEntry {
                    contract: self.contract.clone(),
                    process: ready,
                });
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                drop(state);
                let cleanup = ready.retire().err();
                return Err(match cleanup {
                    Some(cleanup) => {
                        let reason = format!(
                            "exclusive session was concurrently attached; cleanup failed: {cleanup}"
                        );
                        self.quarantine_ready_process(Arc::clone(&ready), reason.clone());
                        ready_process_guard.disarm();
                        anyhow!(reason).context(PersistentSessionCleanupUnproved)
                    }
                    None => {
                        self.release_reservation();
                        ready_process_guard.disarm();
                        anyhow!("exclusive persistent session was concurrently attached")
                    }
                });
            }
        }
        state.exclusive_reservations.remove(&self.session_id);
        ready_process_guard.disarm();
        self.active = false;
        let start_guard = self
            .start_guard
            .take()
            .ok_or_else(|| anyhow!("exclusive persistent-session start guard was lost"))?;
        self.inner.changed.notify_all();
        Ok(start_guard)
    }

    fn release_reservation(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.exclusive_reservations.remove(&self.session_id);
        self.active = false;
        self.inner.changed.notify_all();
    }

    fn quarantine_start_custody(
        &mut self,
        custody: ExclusiveUnprovedStartCustody,
        reason: String,
    ) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let reservation = state
            .exclusive_reservations
            .get_mut(&self.session_id)
            .expect("active exclusive reservation remains registered during custody transfer");
        reservation
            .quarantined
            .push(ExclusiveQuarantinedOwner::Start(custody));
        state.cleanup_unproved.get_or_insert(reason);
        self.active = false;
        self.inner.changed.notify_all();
        drop(state);
        self.start_guard.take();
    }

    fn quarantine_ready_process(&mut self, process: Arc<SessionProcess>, reason: String) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let reservation = state
            .exclusive_reservations
            .get_mut(&self.session_id)
            .expect("active exclusive reservation remains registered during process custody transfer");
        reservation
            .quarantined
            .push(ExclusiveQuarantinedOwner::ReadyProcess(process));
        state.cleanup_unproved.get_or_insert(reason);
        self.active = false;
        self.inner.changed.notify_all();
        drop(state);
        self.start_guard.take();
    }
}

impl ExclusivePersistentSessionStartGuard {
    fn release(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.exclusive_starts_in_flight = state.exclusive_starts_in_flight.saturating_sub(1);
        self.active = false;
        self.inner.changed.notify_all();
    }
}

impl Drop for ExclusivePersistentSessionStartGuard {
    fn drop(&mut self) {
        self.release();
    }
}

impl Drop for ExclusivePersistentSessionReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retained = state
            .exclusive_reservations
            .get(&self.session_id)
            .is_some_and(|reservation| !reservation.quarantined.is_empty());
        if retained {
            state.cleanup_unproved.get_or_insert_with(|| {
                "exclusive reservation retained custody during bind unwind".to_owned()
            });
        } else {
            state.exclusive_reservations.remove(&self.session_id);
        }
        self.active = false;
        self.inner.changed.notify_all();
        drop(state);
        self.start_guard.take();
    }
}

impl ExclusiveReservationEntry {
    fn quarantined_owner_count(&self) -> usize {
        self.quarantined
            .iter()
            .map(|owner| match owner {
                ExclusiveQuarantinedOwner::Start(custody) => {
                    (if custody.relay_health.is_some() { 1 } else { 0 })
                        + (if custody.relay_owner.is_some() { 1 } else { 0 })
                        + custody.lifelines.len()
                        + (if custody.observer.is_some() { 1 } else { 0 })
                }
                ExclusiveQuarantinedOwner::ReadyProcess(process) => {
                    if Arc::strong_count(process) > 0 { 1 } else { 0 }
                }
            })
            .sum()
    }
}

fn aggregate_process_capacity(state: &PoolState) -> (usize, u64, u64) {
    let pooled = state
        .groups
        .values()
        .fold((0usize, 0u64, 0u64), |totals, group| {
            let count = group.process_count();
            let count_u64 = u64::try_from(count).unwrap_or(u64::MAX);
            (
                totals.0.saturating_add(count),
                totals.1.saturating_add(
                    group
                        .contract
                        .lifecycle
                        .max_address_space_bytes
                        .saturating_mul(count_u64),
                ),
                totals.2.saturating_add(
                    group
                        .contract
                        .lifecycle
                        .max_cpu_seconds
                        .saturating_mul(count_u64),
                ),
            )
        });
    state
        .exclusive
        .values()
        .map(|entry| &entry.contract)
        .chain(
            state
                .exclusive_reservations
                .values()
                .map(|reservation| &reservation.contract),
        )
        .fold(pooled, |totals, contract| {
            (
                totals.0.saturating_add(1),
                totals
                    .1
                    .saturating_add(contract.lifecycle.max_address_space_bytes),
                totals.2.saturating_add(contract.lifecycle.max_cpu_seconds),
            )
        })
}

impl PersistentSessionStreamReservation {
    pub fn start<F>(mut self, operation: F) -> Result<String>
    where
        F: FnOnce(Arc<AtomicBool>, Arc<dyn Fn(Value) -> Result<()> + Send + Sync>) -> Result<Value>
            + Send
            + 'static,
    {
        if self.streams.shutdown.load(Ordering::Acquire) {
            self.release();
            bail!("persistent-session admission is closed for daemon shutdown");
        }
        let sequence = STREAM_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let stream_id = lillux::sha256_hex(
            format!("{}\u{1f}{}\u{1f}{sequence}", self.owner, now_ms()).as_bytes(),
        );
        let record = Arc::new(StreamRecord {
            owner: self.owner.clone(),
            quota_subject: self.quota_subject.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            buffer: Mutex::new(StreamBuffer {
                next_sequence: 1,
                events: VecDeque::new(),
                terminal: false,
                backlog_bytes: 0,
            }),
            changed: Condvar::new(),
            last_touched_ms: AtomicU64::new(now_ms()),
            backlog: Arc::clone(&self.streams.backlog),
            max_backlog_bytes: self.streams.limits.max_stream_backlog_bytes,
        });
        {
            let mut records = self
                .streams
                .records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.streams.shutdown.load(Ordering::Acquire) {
                drop(records);
                self.release();
                bail!("persistent-session admission is closed for daemon shutdown");
            }
            if records
                .values()
                .any(|existing| existing.owner == self.owner)
            {
                bail!("persistent-session stream owner became active during reservation");
            }
            if records
                .insert(stream_id.clone(), Arc::clone(&record))
                .is_some()
            {
                bail!("persistent-session stream identity collision");
            }
        }
        self.release();

        let cancelled = Arc::clone(&record.cancelled);
        let delta_record = Arc::clone(&record);
        let publish_delta: Arc<dyn Fn(Value) -> Result<()> + Send + Sync> = Arc::new(move |body| {
            append_stream_event(
                &delta_record,
                PersistentSessionStreamEventKind::Delta,
                Some(body),
                None,
            )
        });
        let terminal_record = Arc::clone(&record);
        let submit_result = self.streams.executor.submit(Box::new(move || {
            let result = operation(Arc::clone(&cancelled), publish_delta);
            match result {
                Ok(body) => {
                    let _ = append_stream_event(
                        &terminal_record,
                        PersistentSessionStreamEventKind::Final,
                        Some(body),
                        None,
                    );
                }
                Err(error) => {
                    let _ = append_stream_event(
                        &terminal_record,
                        PersistentSessionStreamEventKind::Error,
                        None,
                        Some(bounded_stream_error(&error.to_string())),
                    );
                }
            }
        }));
        if let Err(error) = submit_result {
            let mut records = self
                .streams
                .records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            records.remove(&stream_id);
            drop(records);
            release_stream_record(&record);
            return Err(error).context("submit persistent-session stream owner");
        }
        Ok(stream_id)
    }

    fn release(&mut self) {
        if !self.active {
            return;
        }
        let mut reservations = self
            .streams
            .reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if reservations.owners.get(&self.owner).map(String::as_str)
            == Some(self.quota_subject.as_str())
        {
            reservations.owners.remove(&self.owner);
        }
        self.active = false;
    }
}

impl Drop for PersistentSessionStreamReservation {
    fn drop(&mut self) {
        self.release();
    }
}

fn append_stream_event(
    record: &StreamRecord,
    kind: PersistentSessionStreamEventKind,
    body: Option<Value>,
    error: Option<String>,
) -> Result<()> {
    let terminal = matches!(
        kind,
        PersistentSessionStreamEventKind::Final | PersistentSessionStreamEventKind::Error
    );
    let backlog_limit = if terminal {
        STREAM_BACKLOG_EVENTS
    } else {
        STREAM_BACKLOG_EVENTS.saturating_sub(1)
    };
    let event = PersistentSessionStreamEvent {
        sequence: 0,
        kind,
        body,
        error,
    };
    let event_bytes = stream_event_bytes(&event)?;
    if event_bytes > record.max_backlog_bytes || event_bytes > record.backlog.max_bytes {
        bail!("persistent-session stream event exceeds its byte budget");
    }
    if terminal && record.cancelled.load(Ordering::Acquire) {
        let mut buffer = record
            .buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut released = 0usize;
        while let Some(queued) = buffer.events.pop_front() {
            let bytes = stream_event_bytes(&queued)?;
            buffer.backlog_bytes = buffer.backlog_bytes.saturating_sub(bytes);
            released = released.saturating_add(bytes);
        }
        drop(buffer);
        release_backlog_bytes(&record.backlog, released);
    }
    reserve_backlog_bytes(&record.backlog, event_bytes, &record.cancelled, terminal)?;
    let mut buffer = record
        .buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        while (buffer.events.len() >= backlog_limit
            || buffer.backlog_bytes.saturating_add(event_bytes) > record.max_backlog_bytes)
            && !record.cancelled.load(Ordering::Acquire)
        {
            buffer = record
                .changed
                .wait(buffer)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if record.cancelled.load(Ordering::Acquire) && !terminal {
            drop(buffer);
            release_backlog_bytes(&record.backlog, event_bytes);
            bail!("persistent-session stream was cancelled");
        }
        break;
    }
    if buffer.terminal {
        drop(buffer);
        release_backlog_bytes(&record.backlog, event_bytes);
        bail!("persistent-session stream already reached terminal outcome");
    }
    let sequence = buffer.next_sequence;
    buffer.next_sequence = buffer.next_sequence.saturating_add(1);
    buffer.backlog_bytes += event_bytes;
    buffer.events.push_back(PersistentSessionStreamEvent {
        sequence,
        kind,
        body: event.body,
        error: event.error,
    });
    buffer.terminal = terminal;
    record.last_touched_ms.store(now_ms(), Ordering::Release);
    record.changed.notify_all();
    Ok(())
}

fn reserve_backlog_bytes(
    backlog: &BacklogBudget,
    bytes: usize,
    cancelled: &AtomicBool,
    terminal: bool,
) -> Result<()> {
    let mut total = backlog
        .bytes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if cancelled.load(Ordering::Acquire) && !terminal {
            bail!("persistent-session stream was cancelled");
        }
        if total.saturating_add(bytes) <= backlog.max_bytes {
            *total += bytes;
            return Ok(());
        }
        let (next, _) = backlog
            .changed
            .wait_timeout(total, IO_POLL_INTERVAL)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        total = next;
    }
}

fn stream_event_bytes(event: &PersistentSessionStreamEvent) -> Result<usize> {
    serde_json::to_vec(event)
        .map(|bytes| bytes.len())
        .context("encode persistent-session stream event for byte accounting")
}

fn release_backlog_bytes(backlog: &BacklogBudget, bytes: usize) {
    if bytes == 0 {
        return;
    }
    let mut total = backlog
        .bytes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *total = total.saturating_sub(bytes);
    backlog.changed.notify_all();
}

fn release_stream_record(record: &StreamRecord) {
    record.cancelled.store(true, Ordering::Release);
    let mut buffer = record
        .buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let retained = buffer.backlog_bytes;
    buffer.backlog_bytes = 0;
    buffer.events.clear();
    drop(buffer);
    release_backlog_bytes(&record.backlog, retained);
    record.changed.notify_all();
}

fn validate_stream_owner(owner: &str) -> Result<()> {
    if owner.is_empty() || owner.len() > 512 || owner.chars().any(char::is_control) {
        bail!("persistent-session stream owner is not canonical and bounded");
    }
    Ok(())
}

fn bounded_stream_error(error: &str) -> String {
    const MAX_CHARS: usize = 2_048;
    const OMITTED: &str = "… (earlier error text omitted)\n";
    let character_count = error.chars().count();
    if character_count <= MAX_CHARS {
        return error.to_owned();
    }
    let retained = MAX_CHARS.saturating_sub(OMITTED.chars().count());
    let tail = error
        .chars()
        .skip(character_count.saturating_sub(retained))
        .collect::<String>();
    format!("{OMITTED}{tail}")
}

fn ready_pooled_process(
    mut started: StartedPersistentSession,
    original_start: Arc<PersistentSessionPooledStart>,
    wire: &PersistentSessionWireContract,
    lifecycle: &PersistentSessionLifecycleContract,
    backlog: Arc<BacklogBudget>,
) -> std::result::Result<SessionProcess, PooledReadyFailure> {
    if original_start.has_original_trusted_obligation() {
        // Keep the original refusal: no held/runtime/driver readiness join exists.
        // Deposit BEFORE validation, diagnostics, callbacks or ordinary abort.
        original_start.retain_running_unsettled(started.running);
        {
            let mut retained = original_start
                .retained
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            retained.lifelines.append(&mut started.lifelines);
            if let Some(relay_owner) = started.relay_owner.as_ref() {
                retained.lifelines.push(Box::new(Arc::clone(relay_owner)));
            }
            stage_pooled_obligation_owners(
                &mut retained,
                &started.cleanup_observer,
                &started.retirement_observer,
            );
            retained.admission_refused = true;
            retained.phase = PooledStartPhase::Unsettled;
        }
        return Err(PooledReadyFailure {
            error: anyhow!(
                "trusted pooled ready publication requires original held/runtime/driver ownership join"
            ),
            cleanup_unproved: true,
            original_start,
        });
    }
    if let Err(error) = original_start.retain_ready_parts(&mut started) {
        let relay_join = started
            .relay_owner
            .as_ref()
            .and_then(|owner| owner.stop_and_join().err());
        let cleanup = started.running.abort_and_reap_checked();
        return Err(PooledReadyFailure {
            error: match (relay_join, cleanup) {
                (None, Ok(())) => error,
                (Some(join), Ok(())) => error.context(format!(
                    "ordinary relay join before pooled readiness refusal failed: {join:#}"
                )),
                (None, Err(cleanup)) => error.context(format!(
                    "readiness obligation mismatch reap unproved: {cleanup}"
                )),
                (Some(join), Err(cleanup)) => error
                    .context(format!("ordinary relay join failed: {join:#}"))
                    .context(format!("readiness obligation mismatch reap unproved: {cleanup}")),
            },
            cleanup_unproved: true,
            original_start,
        });
    }
    match ready_process(started, wire, lifecycle, backlog) {
        Ok(mut process) => {
            process.pooled_start = Some(original_start);
            Ok(process)
        }
        Err(failure) => Err(PooledReadyFailure {
            error: failure.error,
            cleanup_unproved: failure.cleanup_unproved,
            original_start: {
                if let Some(mut custody) = failure.custody {
                    let mut retained = original_start
                        .retained
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    retained.lifelines.append(&mut custody.lifelines);
                    if let Some(health) = custody.relay_health.take() {
                        retained.lifelines.push(Box::new(health));
                    }
                    if let Some(owner) = custody.relay_owner.take() {
                        retained.lifelines.push(Box::new(owner));
                    }
                    if let Some(observer) = custody.observer.take() {
                        retained.obligation_owners.push(observer);
                    }
                    retained.admission_refused = true;
                    retained.phase = PooledStartPhase::Unsettled;
                }
                original_start
            },
        }),
    }
}

fn ready_process(
    started: StartedPersistentSession,
    wire: &PersistentSessionWireContract,
    lifecycle: &PersistentSessionLifecycleContract,
    backlog: Arc<BacklogBudget>,
) -> std::result::Result<SessionProcess, ReadyProcessFailure> {
    ready_process_registered(started, wire, lifecycle, backlog, None)
}

fn ready_process_registered(
    started: StartedPersistentSession,
    wire: &PersistentSessionWireContract,
    lifecycle: &PersistentSessionLifecycleContract,
    backlog: Arc<BacklogBudget>,
    registration: Option<ExclusiveCustodyRegistration>,
) -> std::result::Result<SessionProcess, ReadyProcessFailure> {
    let StartedPersistentSession {
        running,
        mut socket,
        lifelines,
        relay_health,
        relay_owner,
        expected_boot_identity,
        observation_sink,
        cleanup_observer,
        retirement_observer,
        resource_occupancy_limit,
        resource_cleanup_allowance_ms,
        resource_attribution_sink,
        resource_eligibility,
    } = started;
    let resource_cleanup_present = cleanup_observer.is_some();
    let mut custody = ExclusiveReadyCustodyGuard {
        custody: Some(ExclusiveUnprovedStartCustody {
            relay_health,
            relay_owner,
            lifelines,
            observer: combine_retirement_observers(cleanup_observer, retirement_observer),
        }),
        registration,
    };
    macro_rules! reject_ready {
        ($running:expr, $error:expr $(,)?) => {
            refuse_ready_process($running, $error, custody.take())
        };
    }
    if custody.relay_health.is_some() != custody.relay_owner.is_some() {
        return Err(reject_ready!(
            running,
            anyhow!("persistent-session relay health and owner were not transferred together"),
        ));
    }
    if let Some(health) = &custody.relay_health {
        let snapshot = health.snapshot();
        if snapshot != PersistentSessionRelayHealth::Live {
            return Err(reject_ready!(
                running,
                anyhow!("ordinary relay was terminal before persistent-session readiness: {snapshot:?}"),
            ));
        }
    }
    if resource_cleanup_present
        && (resource_occupancy_limit.is_none()
            || resource_attribution_sink.is_none()
            || resource_eligibility.is_none())
    {
        let error =
            anyhow!("resource-bearing persistent session has no bounded occupancy authority");
        return Err(reject_ready!(running, error));
    }
    if !resource_cleanup_present
        && (resource_occupancy_limit.is_some()
            || resource_attribution_sink.is_some()
            || resource_eligibility.is_some())
    {
        let error = anyhow!("persistent-session resource evidence is incomplete");
        return Err(reject_ready!(running, error));
    }
    if let Some(limit) = &resource_occupancy_limit {
        let Some(cleanup_ms) = resource_cleanup_allowance_ms else {
            return Err(reject_ready!(
                running,
                anyhow!("bounded persistent-session resource lacks a cleanup reserve"),
            ));
        };
        let cleanup = Duration::from_millis(cleanup_ms);
        let request = Duration::from_millis(lifecycle.request_timeout_ms);
        match limit.window(cleanup).map_err(anyhow::Error::msg) {
            Ok(lillux::time::OccupancyWindowState::Service { remaining })
                if remaining >= request => {}
            Ok(_) => {
                let error = anyhow!(
                    "persistent-session resource lifetime cannot admit one complete request"
                );
                return Err(reject_ready!(running, error));
            }
            Err(error) => {
                return Err(reject_ready!(running, error));
            }
        }
    }
    let reader_budget_bytes = (wire.max_frame_bytes as usize).saturating_add(4);
    let reader_budget =
        match BacklogBytePermit::try_reserve(Arc::clone(&backlog), reader_budget_bytes) {
            Ok(budget) => budget,
            Err(error) => {
                let error = attach_running_diagnostic(&running, error);
                return Err(reject_ready!(running, error));
            }
        };
    let timeout = Duration::from_millis(lifecycle.ready_timeout_ms);
    if let Err(error) = socket.set_nonblocking(true) {
        return Err(reject_ready!(
            running,
            anyhow!(error).context("configure persistent-session channel as nonblocking"),
        ));
    }
    let deadline = MonotonicDeadline::after(timeout);
    let mut reader = FrameReader::default();
    let frame = match loop {
        match reader.read_next(&mut socket, wire.max_frame_bytes) {
            Ok(Some(frame)) => break Ok(frame),
            Ok(None) if !deadline.has_elapsed() => {
                sleep_until_io_retry(deadline);
                continue;
            }
            Ok(None) => break Err(anyhow!("persistent-session readiness timed out")),
            Err(error) => break Err(error.context("read persistent-session readiness frame")),
        }
    } {
        Ok(frame) => frame,
        Err(error) => {
            let natural_exit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                running.wait_for_natural_exit(Duration::from_millis(200))
            }));
            return match natural_exit {
                Ok(Ok(completion)) => {
                    let error = attach_completion_diagnostic(&completion, error);
                    let relay_join_error = safe_relay_join(custody.relay_owner.as_ref()).err();
                    if let Some(join) = relay_join_error {
                        Err(ReadyProcessFailure {
                            error: error.context(format!("ordinary relay join before readiness settlement failed: {join:#}")),
                            cleanup_unproved: true,
                            custody: Some(custody.take()),
                        })
                    } else {
                        Err(settle_ready_cleanup(error, custody.take()))
                    }
                }
                Ok(Err(running)) => {
                    let error = attach_running_diagnostic(&running, error);
                    Err(reject_ready!(running, error))
                }
                Err(_) => Err(ReadyProcessFailure {
                    error: error.context("natural-exit wait panicked after consuming the process owner"),
                    cleanup_unproved: true,
                    custody: Some(custody.take()),
                }),
            };
        }
    };
    let readiness = require_frame_identity(&frame, wire)
        .and_then(|()| validate_frame_shape(&frame, expected_boot_identity.as_deref()))
        .and_then(|()| {
            if frame.kind == PersistentSessionFrameKind::Ready {
                Ok(())
            } else {
                Err(anyhow!(
                    "persistent-session worker did not send the required readiness frame"
                ))
            }
        });
    if let Err(error) = readiness {
        let error = attach_running_diagnostic(&running, error);
        return Err(reject_ready!(running, error));
    }
    let writer = match socket.try_clone() {
        Ok(writer) => writer,
        Err(error) => {
            let error = attach_running_diagnostic(
                &running,
                anyhow!(error).context("clone persistent-session writer descriptor"),
            );
            return Err(reject_ready!(running, error));
        }
    };
    if let Some(health) = &custody.relay_health {
        let snapshot = health.snapshot();
        if snapshot != PersistentSessionRelayHealth::Live {
            return Err(reject_ready!(
                running,
                anyhow!("ordinary relay became terminal before owner publication: {snapshot:?}"),
            ));
        }
    }
    let mut custody = custody.take();
    Ok(SessionProcess {
        wire: wire.clone(),
        expected_boot_identity,
        writer: Mutex::new(writer),
        reader: Mutex::new(Some(SessionChannel { socket, reader })),
        pending: Mutex::new(HashMap::new()),
        observation_sender: Mutex::new(None),
        initial_observation_sink: Mutex::new(observation_sink),
        reader_failure: Mutex::new(None),
        running: Mutex::new(Some(running)),
        cleanup_unproved: Mutex::new(None),
        cleanup_observer: Mutex::new(custody.observer.take()),
        relay_owner: Mutex::new(custody.relay_owner.take()),
        relay_health: custody.relay_health.take(),
        retirement: RetirementCoordinator::new(),
        resource_occupancy_limit,
        resource_cleanup_allowance_ms,
        resource_attribution_sink,
        resource_eligibility,
        leased: AtomicBool::new(false),
        last_used_ms: AtomicU64::new(now_ms()),
        closed: Arc::new(AtomicBool::new(false)),
        backlog,
        _reader_budget: reader_budget,
        lifelines: Mutex::new(std::mem::take(&mut custody.lifelines)),
        pooled_start: None,
    })
}

fn settle_ready_cleanup(
    error: anyhow::Error,
    mut custody: ExclusiveUnprovedStartCustody,
) -> ReadyProcessFailure {
    let settlement = custody.observer.as_ref().map(|observer| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer()))
    });
    let settlement_error = match settlement {
        Some(Ok(Ok(()))) | None => None,
        Some(Ok(Err(error))) => Some(format!("{error:#}")),
        Some(Err(_)) => Some("cleanup observer panicked".to_owned()),
    };
    if let Some(settlement_error) = settlement_error {
        return ReadyProcessFailure {
            error: error.context(format!("persistent-session settlement after reap failed: {settlement_error}")),
            cleanup_unproved: true,
            custody: Some(custody),
        };
    }
    custody.observer = None;
    while !custody.lifelines.is_empty() {
        let lifeline = custody.lifelines.remove(0);
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(lifeline))).is_err() {
            return ReadyProcessFailure {
                error: error.context("persistent-session lifeline destructor panicked after checked reap"),
                cleanup_unproved: true,
                custody: Some(custody),
            };
        }
    }
    if let Some(owner) = custody.relay_owner.take() {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(owner))).is_err() {
            return ReadyProcessFailure {
                error: error.context("ordinary relay owner destructor panicked after checked reap"),
                cleanup_unproved: true,
                custody: Some(custody),
            };
        }
    }
    ReadyProcessFailure {
        error,
        cleanup_unproved: false,
        custody: None,
    }
}

fn safe_relay_join(owner: Option<&Arc<dyn PersistentSessionRelayOwner>>) -> Result<()> {
    let Some(owner) = owner else { return Ok(()) };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.stop_and_join())) {
        Ok(result) => result,
        Err(_) => bail!("ordinary relay stop/join panicked"),
    }
}

fn safe_checked_reap(
    running: ryeos_engine::dispatch::RunningExecution,
) -> std::result::Result<(), String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        running.abort_and_reap_checked()
    })) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("{error}")),
        Err(_) => Err("checked process reap panicked after consuming its execution owner".to_owned()),
    }
}

fn refuse_ready_process(
    running: ryeos_engine::dispatch::RunningExecution,
    mut error: anyhow::Error,
    mut custody: ExclusiveUnprovedStartCustody,
) -> ReadyProcessFailure {
    let relay_join = safe_relay_join(custody.relay_owner.as_ref()).err();
    if let Some(join) = relay_join.as_ref() {
        error = error.context(format!("ordinary relay join before readiness cleanup failed: {join:#}"));
    }
    let cleanup_error = safe_checked_reap(running).err();
    if let Some(cleanup) = cleanup_error.as_ref() {
        error = error.context(format!("persistent-session refusal cleanup unproved: {cleanup}"));
    }
    if relay_join.is_some() || cleanup_error.is_some() {
        return ReadyProcessFailure {
            error,
            cleanup_unproved: true,
            custody: Some(custody),
        };
    }
    settle_ready_cleanup(error, custody)
}

/// Retain both original settlement obligations in one observer, attempting
/// them in order and preserving both errors. This grants no resource authority.
pub fn combine_retirement_observers(
    resource: Option<PersistentSessionCleanupObserver>,
    retirement: Option<PersistentSessionCleanupObserver>,
) -> Option<PersistentSessionCleanupObserver> {
    match (resource, retirement) {
        (None, observer) | (observer, None) => observer,
        (Some(resource), Some(retirement)) => Some(Arc::new(move || {
            // Both obligations must be attempted even when one fails.
            match (resource(), retirement()) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(retirement)) => Err(error.context(format!(
                    "launch-authority settlement also failed: {retirement:#}"
                ))),
            }
        })),
    }
}

fn attach_process_diagnostic(process: &SessionProcess, error: anyhow::Error) -> anyhow::Error {
    match process.failure_diagnostic() {
        Some(diagnostic) => error.context(diagnostic),
        None => error,
    }
}

fn attach_running_diagnostic(
    running: &ryeos_engine::dispatch::RunningExecution,
    error: anyhow::Error,
) -> anyhow::Error {
    match running.stderr_diagnostic_tail() {
        Some(stderr) => error.context(format!("persistent-session process stderr tail:\n{stderr}")),
        None => error,
    }
}

fn attach_completion_diagnostic(
    completion: &ryeos_engine::contracts::ExecutionCompletion,
    error: anyhow::Error,
) -> anyhow::Error {
    error.context(process_completion_diagnostic(completion))
}

fn process_completion_diagnostic(
    completion: &ryeos_engine::contracts::ExecutionCompletion,
) -> String {
    let exit_code = completion
        .error
        .as_ref()
        .and_then(|error| error.get("exit_code"))
        .and_then(Value::as_i64)
        .map_or_else(|| "unknown".to_owned(), |code| code.to_string());
    let stderr = completion
        .error
        .as_ref()
        .and_then(|error| error.get("stderr"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if stderr.is_empty() {
        format!("persistent-session process exited with code {exit_code}")
    } else {
        format!("persistent-session process exited with code {exit_code}; stderr:\n{stderr}")
    }
}

fn execute_on_process<C, D>(
    process: &SessionProcess,
    wire: &PersistentSessionWireContract,
    request_kind: PersistentSessionFrameKind,
    request_body: Value,
    cancelled: &C,
    deadline: MonotonicDeadline,
    on_delta: &mut D,
) -> Result<Value>
where
    C: Fn() -> bool,
    D: FnMut(Value) -> Result<Option<Value>>,
{
    let request_id = format!(
        "{}-{}",
        now_ms(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let receiver = process.register_request(&request_id)?;
    let outcome = (|| {
        let resource_expired = || {
            process
                .resource_occupancy_limit
                .as_ref()
                .is_some_and(|limit| limit.is_expired().unwrap_or(true))
        };
        if cancelled() || deadline.has_elapsed() || resource_expired() {
            bail!("persistent-session request ended before worker contact");
        }
        process.ensure_relay_live()?;
        process
            .write(
                wire,
                &PersistentSessionFrame {
                    protocol: wire.wire_protocol.clone(),
                    version: wire.wire_version,
                    kind: request_kind,
                    request_id: Some(request_id.clone()),
                    body: Some(request_body),
                },
                deadline,
            )
            .context("send persistent-session request frame")?;
        let mut cancel_sent = false;
        loop {
            process.ensure_relay_live()?;
            if resource_expired() {
                bail!("persistent-session resource occupancy authority expired");
            }
            if deadline.has_elapsed() {
                bail!("persistent-session request exceeded its signed timeout");
            }
            if cancelled() && !cancel_sent {
                process
                    .write(
                        wire,
                        &PersistentSessionFrame {
                            protocol: wire.wire_protocol.clone(),
                            version: wire.wire_version,
                            kind: PersistentSessionFrameKind::Cancel,
                            request_id: Some(request_id.clone()),
                            body: None,
                        },
                        deadline,
                    )
                    .context("send persistent-session cancellation frame")?;
                cancel_sent = true;
            }
            let wait = deadline.remaining().min(IO_POLL_INTERVAL);
            let budgeted = match receiver.recv_timeout(wait) {
                Ok(Ok(frame)) => frame,
                Ok(Err(reason)) => bail!("persistent-session reader failed: {reason}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("persistent-session response channel disconnected")
                }
            };
            let BudgetedSessionFrame {
                frame,
                _budget: _frame_budget,
            } = budgeted;
            process.ensure_relay_live()?;
            match frame.kind {
                PersistentSessionFrameKind::Delta => {
                    if let Some(acknowledgement) =
                        on_delta(frame.body.expect("delta body validated"))?
                    {
                        process.write(
                            wire,
                            &PersistentSessionFrame {
                                protocol: wire.wire_protocol.clone(),
                                version: wire.wire_version,
                                kind: PersistentSessionFrameKind::ObservationAck,
                                request_id: Some(request_id.clone()),
                                body: Some(acknowledgement),
                            },
                            deadline,
                        )?;
                    }
                }
                PersistentSessionFrameKind::Final => {
                    return accept_final_response(
                        process,
                        frame.body.expect("final body validated"),
                        cancel_sent,
                    );
                }
                PersistentSessionFrameKind::Error => {
                    let detail = frame
                        .body
                        .as_ref()
                        .and_then(|body| body.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("worker returned an error");
                    bail!("persistent-session worker error: {detail}");
                }
                _ => bail!("persistent-session reader routed an invalid response frame"),
            }
        }
    })();
    process.unregister_request(&request_id);
    outcome
}

fn accept_final_response(
    process: &SessionProcess,
    body: Value,
    cancel_sent: bool,
) -> Result<Value> {
    // This lock-free health snapshot is the final-acceptance linearization
    // point. A failure published first wins over a queued Final.
    process.ensure_relay_live()?;
    if cancel_sent {
        bail!("persistent-session request was cancelled");
    }
    Ok(body)
}

fn begin_resource_request_evidence(
    process: &SessionProcess,
    request_identity: Option<&PersistentSessionRequestIdentity>,
    require_attribution: bool,
) -> Result<(
    Option<PersistentSessionResourceRequestLease>,
    Option<lillux::time::OccupancyCoordinate>,
)> {
    let lease = process
        .resource_eligibility
        .as_ref()
        .map(|eligible| eligible())
        .transpose()?;
    let start = match (&process.resource_attribution_sink, request_identity) {
        (Some(_), Some(_)) if require_attribution => Some(
            lillux::time::occupancy_now()
                .map_err(anyhow::Error::msg)
                .context("sample exclusive resource request occupancy start")?,
        ),
        (Some(_), None) if require_attribution => {
            bail!("resource-bearing exclusive request lacks attribution identity")
        }
        _ => None,
    };
    Ok((lease, start))
}

fn finish_resource_request_evidence(
    process: &SessionProcess,
    request_identity: Option<&PersistentSessionRequestIdentity>,
    attribution_start: Option<&lillux::time::OccupancyCoordinate>,
    lease: Option<PersistentSessionResourceRequestLease>,
    result: &mut Result<Value>,
) {
    if let (Some(sink), Some(identity), Some(start)) = (
        &process.resource_attribution_sink,
        request_identity,
        attribution_start,
    ) {
        let attribution = lillux::time::occupancy_now()
            .map_err(anyhow::Error::msg)
            .context("sample exclusive resource request occupancy end")
            .and_then(|end| sink(identity, start, &end));
        if let Err(error) = attribution {
            *result = Err(match std::mem::replace(result, Ok(Value::Null)) {
                Ok(_) => error,
                Err(request) => request.context(format!(
                    "exclusive resource request attribution also failed: {error:#}"
                )),
            });
        }
    }
    if let Some(lease) = lease
        && let Err(error) = lease.finish()
    {
        *result = Err(match std::mem::replace(result, Ok(Value::Null)) {
            Ok(_) => error,
            Err(request) => request.context(format!(
                "exclusive resource request lease cleanup also failed: {error:#}"
            )),
        });
    }
}

fn require_frame_identity(
    frame: &PersistentSessionFrame,
    wire: &PersistentSessionWireContract,
) -> Result<()> {
    if frame.protocol != wire.wire_protocol || frame.version != wire.wire_version {
        bail!("persistent-session frame protocol identity mismatch");
    }
    Ok(())
}

fn resource_service_window_is_sufficient(
    process: &SessionProcess,
    request_timeout_ms: u64,
) -> bool {
    require_resource_service_window(process, request_timeout_ms).is_ok()
}

fn require_resource_service_window(
    process: &SessionProcess,
    request_timeout_ms: u64,
) -> Result<()> {
    let Some(limit) = &process.resource_occupancy_limit else {
        return Ok(());
    };
    let required = Duration::from_millis(request_timeout_ms);
    match limit
        .window(Duration::from_millis(
            process
                .resource_cleanup_allowance_ms
                .context("bounded resource owner lacks cleanup allowance")?,
        ))
        .map_err(anyhow::Error::msg)?
    {
        lillux::time::OccupancyWindowState::Service { remaining } if remaining >= required => {
            Ok(())
        }
        lillux::time::OccupancyWindowState::Service { .. }
        | lillux::time::OccupancyWindowState::Cleanup { .. }
        | lillux::time::OccupancyWindowState::Expired => {
            bail!("persistent-session resource service window is exhausted")
        }
    }
}

fn exclusive_request_deadline(
    request_timeout_ms: u64,
    absolute_deadline: Option<MonotonicDeadline>,
) -> MonotonicDeadline {
    let signed_deadline = MonotonicDeadline::after(Duration::from_millis(request_timeout_ms));
    absolute_deadline.map_or(signed_deadline, |absolute| signed_deadline.min(absolute))
}

fn lock_writer_before_deadline(
    writer: &Mutex<lillux::InheritedDuplexChannel>,
    deadline: MonotonicDeadline,
) -> Result<std::sync::MutexGuard<'_, lillux::InheritedDuplexChannel>> {
    loop {
        if deadline.has_elapsed() {
            bail!("persistent-session frame deadline expired while waiting for its writer");
        }
        match writer.try_lock() {
            Ok(writer) => return Ok(writer),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => sleep_until_io_retry(deadline),
        }
    }
}

fn write_frame(
    stream: &mut impl Write,
    wire: &PersistentSessionWireContract,
    frame: &PersistentSessionFrame,
    deadline: MonotonicDeadline,
) -> Result<()> {
    let encoded = encode_frame(wire, frame)?;
    let mut written = 0;
    while written < encoded.len() {
        // The writer lock or a previous partial write may consume the entire
        // remaining budget. Never initiate another write syscall after expiry,
        // even if the descriptor is immediately writable at that point.
        if deadline.has_elapsed() {
            bail!("persistent-session frame write exceeded its deadline");
        }
        // The typed Lillux endpoint owns the underlying descriptor operation;
        // persistent-session protocol code only reads and writes bytes.
        let outcome = stream.write(&encoded[written..]);
        match outcome {
            Ok(0) => bail!("persistent-session channel closed while writing a frame"),
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if is_io_timeout(&error) && !deadline.has_elapsed() => {
                sleep_until_io_retry(deadline);
            }
            Err(error) if is_io_timeout(&error) => {
                bail!("persistent-session frame write exceeded its deadline")
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn sleep_until_io_retry(deadline: MonotonicDeadline) {
    let remaining = deadline.remaining();
    if !remaining.is_zero() {
        lillux::time::sleep(IO_POLL_INTERVAL.min(remaining));
    }
}

fn encode_frame(
    wire: &PersistentSessionWireContract,
    frame: &PersistentSessionFrame,
) -> Result<Vec<u8>> {
    require_frame_identity(frame, wire)?;
    validate_frame_shape(frame, None)?;
    let body = serde_json::to_vec(frame)?;
    if body.len() > wire.max_frame_bytes as usize {
        bail!("persistent-session output frame exceeds its signed bound");
    }
    let len = u32::try_from(body.len()).context("persistent-session frame length overflow")?;
    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

fn decode_frame_body(body: &[u8], max_frame_bytes: u32) -> Result<PersistentSessionFrame> {
    if body.is_empty() || body.len() > max_frame_bytes as usize {
        bail!("persistent-session input frame violates its signed bound");
    }
    let frame: PersistentSessionFrame =
        serde_json::from_slice(body).context("decode persistent-session frame")?;
    // Serde treats absent nullable fields as `None`; the wire does not. Parse
    // the already duplicate-checked document once more to prove all five keys
    // were physically present. Unknown and duplicate fields fail above.
    let value: Value = serde_json::from_slice(body).context("inspect persistent-session frame")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("persistent-session frame must be an object"))?;
    if object.len() != 5
        || !["protocol", "version", "kind", "request_id", "body"]
            .iter()
            .all(|key| object.contains_key(*key))
    {
        bail!("persistent-session frame must contain exactly five protocol fields");
    }
    validate_decoded_frame_shape(&frame)?;
    Ok(frame)
}

/// Validate wire shape before the caller knows whether this is pooled
/// readiness or exclusive-session readiness. The exclusive boot identity is
/// checked against daemon-minted authority by `ready_process`; the decoder may
/// only prove that its carrier is canonical.
fn validate_decoded_frame_shape(frame: &PersistentSessionFrame) -> Result<()> {
    if frame.kind != PersistentSessionFrameKind::Ready {
        return validate_frame_shape(frame, None);
    }
    let valid_body = match frame.body.as_ref() {
        None => true,
        Some(body) => body.as_object().is_some_and(|object| {
            object.len() == 1
                && object
                    .get("boot_identity")
                    .and_then(Value::as_str)
                    .is_some_and(lillux::valid_hash)
        }),
    };
    if frame.request_id.is_some() || !valid_body {
        bail!("persistent-session frame fields contradict its kind");
    }
    Ok(())
}

impl FrameReader {
    fn read_next<R: Read>(
        &mut self,
        stream: &mut R,
        max_frame_bytes: u32,
    ) -> Result<Option<PersistentSessionFrame>> {
        while self.length_read < self.length.len() {
            match stream.read(&mut self.length[self.length_read..]) {
                Ok(0) => bail!("persistent-session channel closed while reading frame length"),
                Ok(read) => self.length_read += read,
                Err(error) if is_io_timeout(&error) => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("read persistent-session frame length"),
            }
        }
        if self.body.is_empty() {
            let length = u32::from_be_bytes(self.length);
            if length == 0 || length > max_frame_bytes {
                bail!("persistent-session input frame violates its signed bound");
            }
            self.body.resize(length as usize, 0);
        }
        while self.body_read < self.body.len() {
            match stream.read(&mut self.body[self.body_read..]) {
                Ok(0) => bail!("persistent-session channel closed while reading frame body"),
                Ok(read) => self.body_read += read,
                Err(error) if is_io_timeout(&error) => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("read persistent-session frame body"),
            }
        }
        let frame = decode_frame_body(&self.body, max_frame_bytes)?;
        *self = Self::default();
        Ok(Some(frame))
    }
}

fn validate_frame_shape(
    frame: &PersistentSessionFrame,
    expected_boot_identity: Option<&str>,
) -> Result<()> {
    if frame.kind == PersistentSessionFrameKind::ObservationBatch
        && frame.body.as_ref().is_some_and(|body| {
            serde_json::to_vec(body).is_ok_and(|encoded| {
                encoded.len() > ryeos_state::objects::MAX_STRUCTURED_OBSERVATION_BATCH_BYTES
            })
        })
    {
        bail!("persistent-session observation batch exceeds its serialized-byte ceiling");
    }
    let valid = match frame.kind {
        PersistentSessionFrameKind::Ready => {
            frame.request_id.is_none()
                && match expected_boot_identity {
                    None => frame.body.is_none(),
                    Some(expected) => frame.body.as_ref().is_some_and(|body| {
                        body.as_object().is_some_and(|object| {
                            object.len() == 1
                                && object.get("boot_identity").and_then(Value::as_str)
                                    == Some(expected)
                        })
                    }),
                }
        }
        PersistentSessionFrameKind::Request
        | PersistentSessionFrameKind::Control
        | PersistentSessionFrameKind::Delta
        | PersistentSessionFrameKind::Final
        | PersistentSessionFrameKind::Error => {
            frame.request_id.as_ref().is_some_and(|id| !id.is_empty()) && frame.body.is_some()
        }
        PersistentSessionFrameKind::Cancel => {
            frame.request_id.as_ref().is_some_and(|id| !id.is_empty()) && frame.body.is_none()
        }
        PersistentSessionFrameKind::ObservationBatch => {
            frame.request_id.is_none() && frame.body.is_some()
        }
        PersistentSessionFrameKind::ObservationAck => {
            frame.request_id.as_ref().is_none_or(|id| !id.is_empty()) && frame.body.is_some()
        }
    };
    if !valid
        || frame
            .request_id
            .as_ref()
            .is_some_and(|id| id.len() > 256 || id.chars().any(char::is_control))
    {
        bail!("persistent-session frame fields contradict its kind");
    }
    Ok(())
}

fn is_io_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn validate_pool_key(key: &str) -> Result<()> {
    if key.len() != 64
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("persistent-session pool key is not a canonical digest");
    }
    Ok(())
}

fn validate_exclusive_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty()
        || session_id.len() > 256
        || session_id.trim() != session_id
        || session_id.chars().any(char::is_control)
    {
        bail!("exclusive persistent-session id is not canonical and bounded");
    }
    Ok(())
}

fn now_ms() -> u64 {
    lillux::time::timestamp_millis().try_into().unwrap_or(0)
}

fn sweep_stream_registry(streams: &StreamRegistry) {
    let now = now_ms();
    let mut records = streams
        .records
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    records.retain(|_, record| {
        let terminal = record
            .buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .terminal;
        let age = now.saturating_sub(record.last_touched_ms.load(Ordering::Acquire));
        let keep = if terminal {
            age <= TERMINAL_STREAM_RETENTION_MS
        } else {
            age <= ABANDONED_STREAM_RETENTION_MS
        };
        if !keep {
            record.cancelled.store(true, Ordering::Release);
            let mut buffer = record
                .buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let retained = buffer.backlog_bytes;
            buffer.backlog_bytes = 0;
            buffer.events.clear();
            drop(buffer);
            release_backlog_bytes(&record.backlog, retained);
            record.changed.notify_all();
        }
        keep
    });
}

fn spawn_stream_reaper(streams: Weak<StreamRegistry>) {
    std::thread::Builder::new()
        .name("ryeos-session-stream-reaper".to_owned())
        .spawn(move || {
            loop {
                lillux::time::sleep(Duration::from_secs(60));
                let Some(streams) = streams.upgrade() else {
                    break;
                };
                sweep_stream_registry(&streams);
                drop(streams);
            }
        })
        .expect("spawn persistent-session stream reaper");
}

fn spawn_idle_reaper(inner: Weak<PoolInner>) {
    std::thread::Builder::new()
        .name("ryeos-session-reaper".to_owned())
        .spawn(move || {
            loop {
                lillux::time::sleep(Duration::from_secs(1));
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                let now = now_ms();
                let mut retired = Vec::new();
                let mut retired_exclusive = Vec::new();
                let state = inner
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for (key, group) in &state.groups {
                    let idle = group.contract.lifecycle.idle_timeout_ms;
                    for process in &group.processes {
                        let expired = !process.leased.load(Ordering::Acquire)
                            && (process.relay_failed()
                                || now.saturating_sub(process.last_used_ms.load(Ordering::Acquire))
                                >= idle
                                || !resource_service_window_is_sufficient(
                                    process,
                                    group.contract.lifecycle.request_timeout_ms,
                                ));
                        if expired
                            && process
                                .leased
                                .compare_exchange(
                                    false,
                                    true,
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                )
                                .is_ok()
                        {
                            retired.push((key.clone(), Arc::clone(process)));
                        }
                    }
                }
                for (session_id, entry) in &state.exclusive {
                    let expired = entry.process.relay_failed()
                        || entry
                            .process
                            .resource_occupancy_limit
                            .as_ref()
                            .is_some_and(|limit| {
                                !matches!(
                                    limit.window(Duration::from_millis(
                                        entry
                                            .process
                                            .resource_cleanup_allowance_ms
                                            .unwrap_or(u64::MAX),
                                    )),
                                    Ok(lillux::time::OccupancyWindowState::Service { .. })
                                )
                            });
                    if expired
                        && entry
                            .process
                            .leased
                            .compare_exchange(
                                false,
                                true,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok()
                    {
                        retired_exclusive
                            .push((session_id.clone(), Arc::clone(&entry.process)));
                    }
                }
                drop(state);
                for (key, process) in retired {
                    match process.retire() {
                        Ok(()) => {
                            let mut state = inner
                                .state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if let Some(group) = state.groups.get_mut(&key) {
                                group
                                    .processes
                                    .retain(|candidate| !Arc::ptr_eq(candidate, &process));
                                if group.processes.is_empty() && group.starts.is_empty() {
                                    state.groups.remove(&key);
                                }
                            }
                        }
                        Err(error) => {
                            let mut state = inner
                                .state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if state.cleanup_unproved.is_none() {
                                state.cleanup_unproved = Some(error.to_string());
                            }
                            tracing::error!(%error, "idle persistent-session cleanup could not be proved; pool quarantined");
                        }
                    }
                }
                for (session_id, process) in retired_exclusive {
                    match process.retire() {
                        Ok(()) => {
                            let mut state = inner
                                .state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if state
                                .exclusive
                                .get(&session_id)
                                .is_some_and(|entry| Arc::ptr_eq(&entry.process, &process))
                            {
                                state.exclusive.remove(&session_id);
                                state
                                    .exclusive_failure_cleanup
                                    .insert(session_id, "reaped");
                            }
                        }
                        Err(error) => {
                            let mut state = inner
                                .state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            state
                                .cleanup_unproved
                                .get_or_insert_with(|| error.to_string());
                            state
                                .exclusive_failure_cleanup
                                .insert(session_id, "unproved");
                            tracing::error!(%error, "expired exclusive-session cleanup could not be proved; pool quarantined");
                        }
                    }
                }
                inner.changed.notify_all();
                // Do not retain the pool through the next sleep. The
                // background reaper must not become an accidental owner.
                drop(inner);
            }
        })
        .expect("spawn persistent-session idle reaper");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_initial_missing_budget_and_cancel_refuse_with_actual_mutex_held() {
        for cancelled in [false, true] {
            let original = super::PersistentSessionPooledStart::new();
            let held = std::sync::Barrier::new(2);
            let release = std::sync::Barrier::new(2);
            std::thread::scope(|threads| {
                threads.spawn(|| {
                    let _retained = original.retained.lock().unwrap();
                    held.wait();
                    release.wait();
                });
                held.wait();
                let (entered, entry) = std::sync::mpsc::channel();
                let original_ref = &original;
                let operation = threads.spawn(move || {
                    entered.send(()).unwrap();
                    original_ref
                        .checkout_original_manifest_precontact(&|| cancelled)
                        .is_err()
                });
                entry.recv().unwrap();
                release.wait();
                assert!(operation.join().unwrap());
            });
            let retained = original.retained.lock().unwrap();
            assert_eq!(
                retained.manifest_preparation,
                super::OriginalManifestPreparationPhase::Refused
            );
            assert!(retained.admission_refused);
            assert!(retained.trusted_inputs.is_none() && retained.original_funding.is_none());
        }
    }

    #[test]
    fn manifest_initial_acquisition_obeys_actual_original_budget_under_contention() {
        let root = tempfile::tempdir().unwrap();
        let fixture = crate::handler_context::budget_test_support::build(
            root.path(),
            lillux::time::Duration::from_secs(1),
        )
        .unwrap();
        let budget = fixture.context.original_execution_budget().unwrap();
        let original = super::PersistentSessionPooledStart::new();
        original
            .retain_original_callback_budget(budget, budget.precontact_binding().scope)
            .unwrap();
        let held = std::sync::Barrier::new(2);
        std::thread::scope(|threads| {
            threads.spawn(|| {
                let _retained = original.retained.lock().unwrap();
                held.wait();
                // Original expiry, not a renewed timeout. Release permits the
                // potentially blocking CUSTODY refusal to complete afterward.
                while !budget.contact_expiry().has_elapsed() {
                    lillux::time::sleep(std::time::Duration::from_millis(1));
                }
            });
            held.wait();
            assert!(
                original
                    .checkout_original_manifest_precontact(&|| false)
                    .is_err()
            );
        });
        assert!(budget.contact_expiry().has_elapsed());
        assert_eq!(original.original_callback_budget.get().unwrap(), budget);
        let retained = original.retained.lock().unwrap();
        assert_eq!(
            retained.manifest_preparation,
            super::OriginalManifestPreparationPhase::Refused
        );
        assert!(retained.trusted_inputs.is_none() && retained.original_funding.is_none());
        drop(retained);
        assert!(
            original
                .checkout_original_manifest_precontact(&|| false)
                .is_err()
        );
    }

    #[test]
    fn manifest_initial_contention_rechecks_same_actual_cancellation_predicate() {
        let root = tempfile::tempdir().unwrap();
        let fixture = crate::handler_context::budget_test_support::build(
            root.path(),
            lillux::time::Duration::from_secs(5),
        )
        .unwrap();
        let budget = fixture.context.original_execution_budget().unwrap();
        let original = super::PersistentSessionPooledStart::new();
        original
            .retain_original_callback_budget(budget, budget.precontact_binding().scope)
            .unwrap();
        let held = std::sync::Barrier::new(2);
        let (cancelled_signal, cancellation) = std::sync::mpsc::channel();
        let observations = std::cell::Cell::new(0);
        std::thread::scope(|threads| {
            let original_ref = &original;
            let held_ref = &held;
            threads.spawn(move || {
                let _retained = original_ref.retained.lock().unwrap();
                held_ref.wait();
                cancellation.recv().unwrap();
            });
            held.wait();
            let cancelled = || {
                observations.set(observations.get() + 1);
                if observations.get() == 3 {
                    cancelled_signal.send(()).unwrap();
                }
                observations.get() >= 3
            };
            assert!(
                original
                    .checkout_original_manifest_precontact(&cancelled)
                    .is_err()
            );
        });
        assert!(observations.get() >= 3);
        assert_eq!(original.original_callback_budget.get().unwrap(), budget);
        let retained = original.retained.lock().unwrap();
        assert_eq!(
            retained.manifest_preparation,
            super::OriginalManifestPreparationPhase::Refused
        );
        assert!(retained.trusted_inputs.is_none() && retained.original_funding.is_none());
    }
    #[test]
    fn inert_manifest_checkout_calls_actual_cancellation_outside_retention_lock() {
        let original = super::PersistentSessionPooledStart::new();
        let cancelled = || {
            assert!(original.retained.try_lock().is_ok());
            true
        };
        assert!(
            original
                .checkout_original_manifest_precontact(&cancelled)
                .is_err()
        );
        let retained = original.retained.lock().unwrap();
        assert_eq!(
            retained.manifest_preparation,
            OriginalManifestPreparationPhase::Refused
        );
        assert!(retained.trusted_inputs.is_none() && retained.original_manifest_restored.is_none());
        assert!(retained.phase == super::PooledStartPhase::Unsettled);
        assert!(super::PersistentSessionPooledStart::native_check_blocks_mutation(&retained));
    }

    #[test]
    fn inert_manifest_checkout_refuses_missing_actual_budget_funding_and_inputs_once() {
        let original = super::PersistentSessionPooledStart::new();
        let cancelled = || false;
        assert!(
            original
                .checkout_original_manifest_precontact(&cancelled)
                .is_err()
        );
        assert!(
            original
                .checkout_original_manifest_precontact(&cancelled)
                .is_err()
        );
        assert!(original.has_original_trusted_obligation());
        assert!(original.has_retained_custody());
        assert!(!original.publish());
        assert!(original.prove_cleanup().is_err());
        assert!(original.retained.lock().unwrap().original_funding.is_none());
    }

    #[test]
    fn inert_manifest_checkout_poison_preserves_refusal_without_owner_fabrication() {
        let original = super::PersistentSessionPooledStart::new();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _retained = original.retained.lock().unwrap();
            panic!("actual metadata-lock poison control");
        }));
        assert!(poisoned.is_err());
        assert!(
            original
                .checkout_original_manifest_precontact(&|| false)
                .is_err()
        );
        let retained = original.retained.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(
            retained.manifest_preparation,
            OriginalManifestPreparationPhase::Refused
        );
        assert!(retained.admission_refused);
        assert!(retained.original_manifest_restored.is_none());
        assert!(retained.phase == super::PooledStartPhase::Unsettled);
    }
    use super::*;

    // Genuine NEGATIVES only, all UNCOMPILED/UNRUN. No original backend,
    // qualified manifest product or Prepared-positive owner is fabricated.
    #[cfg(target_os = "linux")]
    fn manifest_negative_fixture() -> (
        tempfile::TempDir,
        crate::handler_context::budget_test_support::Fixture,
        String,
        Arc<crate::temp_dir_guard::TempDirGuard>,
        crate::runtime_db::ProcessResourceReservationRecord,
    ) {
        use crate::runtime_db::process_resource_custody::*;
        let root = tempfile::tempdir().unwrap();
        let mut fixture = crate::handler_context::budget_test_support::build(
            root.path(),
            Duration::from_secs(60),
        )
        .unwrap();
        let selected =
            ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
                source_root: "source".into(),
                member: "startup.json".into(),
                sha256: lillux::sha256_hex(
                    b"static selected member, no original product qualification",
                ),
            };
        // Populate existing signed factory into SAME budget State, not a second
        // independently built State/proof. Obtain inputs through production verify.
        let hash = crate::persistent_session_admission::factory_controls::populate_fixture(
            &mut fixture.state,
            Some(selected),
        );
        let inputs =
            crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs::verify(
                &fixture.state,
                &hash,
            )
            .unwrap();
        let pinned = fixture.state.state_store.pinned_state_authority().unwrap();
        let runtime = pinned.runtime_directory();
        let (path, scratch) = crate::temp_dir_guard::create_projectless_workspace(
            &runtime.path().join("cache"),
            "manifest-negative-original",
        )
        .unwrap();
        std::fs::write(path.join("sentinel"), b"same original scratch").unwrap();
        let (parent, name) = scratch.owned_scratch_parent().unwrap();
        // Intent DATA from actual owners; never reserved during fixture setup.
        let intent = crate::runtime_db::ProcessResourceReservationRecord {
            owner_kind: "pooled_session".into(),
            owner_coordinate: name.to_str().unwrap().into(),
            daemon_generation_id: crate::runtime_db::daemon_generation_id().into(),
            selections: inputs.selected().selections().to_vec(),
            allocation_limit: inputs
                .selected()
                .max_concurrent_exclusive_allocations()
                .unwrap(),
            launch_authority:
                crate::runtime_db::ProcessResourceLaunchAuthority::TrustedProcessGroup {
                    cleanup_contract_digest: inputs.cleanup_contract_digest().into(),
                    host_lifetime: lillux::ProcessHostLifetime::capture_current().unwrap(),
                    phase: crate::runtime_db::TrustedResourceLaunchPhase::Reserved,
                },
            runtime_custody: Some(ProcessResourceRuntimeCustody {
                version: ProcessResourceRuntimeCustody::VERSION,
                session_capsule_hash: hash.clone(),
                execution_realization_hash: inputs.capsule().execution_realization_hash.clone(),
                source_binding_hash: inputs.capsule().source_binding_hash.clone(),
                materializations: vec![],
                workspace: ProcessWorkspaceCustody::ResourceOwnedScratch {
                    runtime_directory_identity: runtime.identity().unwrap(),
                    parent_directory_identity: parent.identity().unwrap(),
                    scratch_name: name.to_str().unwrap().into(),
                    workspace_identity: scratch.owned_scratch_root().unwrap().identity().unwrap(),
                },
                physical_state: ProcessCustodyPhysicalState::Retained,
            }),
        };
        (root, fixture, hash, scratch, intent)
    }

    #[cfg(target_os = "linux")]
    fn manifest_negative_ledger_effects(
        state: &crate::state::AppState,
        intent: &crate::runtime_db::ProcessResourceReservationRecord,
    ) -> (i64, i64) {
        // Read actual durable rows for this exact original intent. Equality
        // proves no COMMITTED effect only, not absence of rolled-back attempts.
        let db = state.accounting.as_ref().unwrap();
        let conn = rusqlite::Connection::open_with_flags(
            db.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let original_id = intent.reservation_id().unwrap();
        let gates = conn.query_row(
            "SELECT COUNT(*) FROM resource_owner_accounting_gate WHERE original_reservation_id=?1",
            [&original_id], |row| row.get(0),
        ).unwrap();
        let operations = conn.query_row(
            "SELECT COUNT(*) FROM resource_financial_operation AS operation JOIN resource_owner_accounting_gate AS gate ON operation.owner_gate_id=gate.owner_gate_id WHERE gate.original_reservation_id=?1",
            [&original_id], |row| row.get(0),
        ).unwrap();
        (gates, operations)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn genuine_manifest_binder_refusal_restores_same_input_before_committed_financial_effect() {
        let (_root, fixture, hash, scratch, intent) = manifest_negative_fixture();
        let inputs =
            crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs::verify(
                &fixture.state,
                &hash,
            )
            .unwrap();
        let devices = Arc::clone(inputs.selected().devices().unwrap());
        let budget = fixture.context.original_execution_budget().unwrap();
        let start = PersistentSessionPooledStart::new();
        start
            .retain_original_callback_budget(budget, budget.precontact_binding().scope)
            .unwrap();
        let runtime_before = fixture
            .state
            .state_store
            .process_resource_reservations()
            .unwrap();
        let ledger_before = manifest_negative_ledger_effects(&fixture.state, &intent);
        let cancelled = || {
            assert!(start.retained.try_lock().is_ok());
            false
        };
        let error = start
            .prepare_trusted_original_funding(
                &fixture.state,
                inputs,
                budget.precontact_binding().scope,
                &intent,
                Arc::clone(&scratch),
                &cancelled,
            )
            .unwrap_err();
        // First refusal: SAME State uses disabled_for_authoring isolation.
        // Registered-backend and qualified-product checks are not reached.
        assert!(format!("{error:#}").contains("original"));
        let retained = start.retained.lock().unwrap();
        assert_eq!(
            retained.manifest_preparation,
            OriginalManifestPreparationPhase::Refused
        );
        let original = retained.original_manifest_restored.as_ref().unwrap();
        original.require_same_state(&fixture.state).unwrap();
        assert_eq!(original.capsule().content_hash().unwrap(), hash);
        assert!(Arc::ptr_eq(
            original.selected().devices().unwrap(),
            &devices
        ));
        assert!(retained.trusted_inputs.is_none() && retained.original_funding.is_none());
        assert!(Arc::ptr_eq(
            retained.original_scratch.as_ref().unwrap(),
            &scratch
        ));
        assert!(PersistentSessionPooledStart::native_check_blocks_mutation(
            &retained
        ));
        drop(retained);
        assert_eq!(
            fixture
                .state
                .state_store
                .process_resource_reservations()
                .unwrap(),
            runtime_before
        );
        assert_eq!(
            manifest_negative_ledger_effects(&fixture.state, &intent),
            ledger_before
        );
        assert_eq!(ledger_before, (0, 0));
        assert_eq!(
            std::fs::read(scratch.path().unwrap().join("sentinel")).unwrap(),
            b"same original scratch"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn genuine_manifest_cancel_after_checkout_restores_original_devices_and_scratch() {
        let (_root, fixture, hash, scratch, intent) = manifest_negative_fixture();
        let inputs =
            crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs::verify(
                &fixture.state,
                &hash,
            )
            .unwrap();
        let devices = Arc::clone(inputs.selected().devices().unwrap());
        let budget = fixture.context.original_execution_budget().unwrap();
        let start = PersistentSessionPooledStart::new();
        start
            .retain_original_callback_budget(budget, budget.precontact_binding().scope)
            .unwrap();
        let observed = std::cell::Cell::new(false);
        let cancelled = || {
            let retained = start.retained.lock().unwrap();
            let in_flight =
                retained.manifest_preparation == OriginalManifestPreparationPhase::UnfundedInFlight;
            drop(retained);
            if in_flight {
                observed.set(true);
            }
            in_flight
        };
        assert!(
            start
                .prepare_trusted_original_funding(
                    &fixture.state,
                    inputs,
                    budget.precontact_binding().scope,
                    &intent,
                    Arc::clone(&scratch),
                    &cancelled,
                )
                .is_err()
        );
        assert!(observed.get());
        let retained = start.retained.lock().unwrap();
        assert_eq!(
            retained.manifest_preparation,
            OriginalManifestPreparationPhase::Refused
        );
        assert!(Arc::ptr_eq(
            retained
                .original_manifest_restored
                .as_ref()
                .unwrap()
                .selected()
                .devices()
                .unwrap(),
            &devices
        ));
        assert!(Arc::ptr_eq(
            retained.original_scratch.as_ref().unwrap(),
            &scratch
        ));
        assert!(retained.original_funding.is_none() && retained.trusted_inputs.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn genuine_manifest_unwind_and_poison_conserve_checked_out_owner_without_prepared_setter() {
        for poison in [false, true] {
            let (_root, fixture, hash, scratch, intent) = manifest_negative_fixture();
            let inputs =
                crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs::verify(
                    &fixture.state,
                    &hash,
                )
                .unwrap();
            let devices = Arc::clone(inputs.selected().devices().unwrap());
            let budget = fixture.context.original_execution_budget().unwrap();
            let start = PersistentSessionPooledStart::new();
            start
                .retain_original_callback_budget(budget, budget.precontact_binding().scope)
                .unwrap();
            let cancelled = || {
                let retained = start.retained.lock().unwrap();
                if retained.manifest_preparation
                    == OriginalManifestPreparationPhase::UnfundedInFlight
                {
                    if !poison {
                        drop(retained);
                    }
                    panic!("genuine operation unwind after unique checkout");
                }
                false
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                start.prepare_trusted_original_funding(
                    &fixture.state,
                    inputs,
                    budget.precontact_binding().scope,
                    &intent,
                    Arc::clone(&scratch),
                    &cancelled,
                )
            }));
            assert!(outcome.is_err());
            assert_eq!(start.retained.is_poisoned(), poison);
            let retained = start.retained.lock().unwrap_or_else(|p| p.into_inner());
            assert_eq!(
                retained.manifest_preparation,
                OriginalManifestPreparationPhase::Refused
            );
            assert!(retained.admission_refused && retained.phase == PooledStartPhase::Unsettled);
            assert!(Arc::ptr_eq(
                retained
                    .original_manifest_restored
                    .as_ref()
                    .unwrap()
                    .selected()
                    .devices()
                    .unwrap(),
                &devices
            ));
            assert!(retained.original_funding.is_none() && retained.trusted_inputs.is_none());
            assert!(Arc::ptr_eq(
                retained.original_scratch.as_ref().unwrap(),
                &scratch
            ));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn genuine_competing_manifest_deposit_quarantines_actual_owner_and_preserves_original_slot() {
        let (_root, fixture, hash, scratch, intent) = manifest_negative_fixture();
        let inputs =
            crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs::verify(
                &fixture.state,
                &hash,
            )
            .unwrap();
        let competing =
            crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs::verify(
                &fixture.state,
                &hash,
            )
            .unwrap();
        let devices = Arc::clone(inputs.selected().devices().unwrap());
        let other_devices = Arc::clone(competing.selected().devices().unwrap());
        let pinned = fixture.state.state_store.pinned_state_authority().unwrap();
        let (_, competing_scratch) = crate::temp_dir_guard::create_projectless_workspace(
            &pinned.runtime_directory().path().join("cache"),
            "manifest-negative-competitor",
        )
        .unwrap();
        let competitor = std::cell::RefCell::new(Some(competing));
        let cancellation = AtomicBool::new(false);
        let operation_cancelled = || cancellation.load(Ordering::Acquire);
        let budget = fixture.context.original_execution_budget().unwrap();
        let start = PersistentSessionPooledStart::new();
        start
            .retain_original_callback_budget(budget, budget.precontact_binding().scope)
            .unwrap();
        let cancelled = || {
            let retained = start.retained.lock().unwrap();
            let in_flight =
                retained.manifest_preparation == OriginalManifestPreparationPhase::UnfundedInFlight;
            drop(retained);
            if in_flight {
                if let Some(competing) = competitor.borrow_mut().take() {
                    // A genuinely supplied second owner, not replacement proof.
                    assert!(
                        start
                            .prepare_trusted_original_funding(
                                &fixture.state,
                                competing,
                                budget.precontact_binding().scope,
                                &intent,
                                Arc::clone(&competing_scratch),
                                &operation_cancelled,
                            )
                            .is_err()
                    );
                }
            }
            operation_cancelled()
        };
        assert!(
            start
                .prepare_trusted_original_funding(
                    &fixture.state,
                    inputs,
                    budget.precontact_binding().scope,
                    &intent,
                    Arc::clone(&scratch),
                    &cancelled,
                )
                .is_err()
        );
        assert!(competitor.borrow().is_none());
        let retained = start.retained.lock().unwrap();
        assert_eq!(
            retained.manifest_preparation,
            OriginalManifestPreparationPhase::Refused
        );
        assert!(Arc::ptr_eq(
            retained
                .original_manifest_restored
                .as_ref()
                .unwrap()
                .selected()
                .devices()
                .unwrap(),
            &devices
        ));
        assert_eq!(retained.quarantined_inputs.len(), 1);
        assert!(Arc::ptr_eq(
            retained.quarantined_inputs[0].selected().devices().unwrap(),
            &other_devices
        ));
        assert!(Arc::ptr_eq(
            retained.original_scratch.as_ref().unwrap(),
            &scratch
        ));
        assert!(retained.lifelines.len() >= 2);
        assert!(retained.original_funding.is_none() && retained.trusted_inputs.is_none());
    }

    #[test]
    fn original_typed_scratch_keeps_exact_guard_and_duplicate_actual_lifelines() {
        let start = PersistentSessionPooledStart::new();
        let (_first_root, first_path, first) = owned_start_scratch();
        let (_second_root, second_path, second) = owned_start_scratch();
        let first_identity = first.owned_scratch_root().unwrap().identity().unwrap();
        let first_weak = Arc::downgrade(&first);
        let second_weak = Arc::downgrade(&second);
        {
            let mut retained = start.retained.lock().unwrap();
            assert!(PersistentSessionPooledStart::retain_original_scratch(
                &mut retained,
                &first
            ));
            assert!(Arc::ptr_eq(
                retained.original_scratch.as_ref().unwrap(),
                &first
            ));
            assert!(!PersistentSessionPooledStart::retain_original_scratch(
                &mut retained,
                &first
            ));
            assert!(!PersistentSessionPooledStart::retain_original_scratch(
                &mut retained,
                &second
            ));
            assert!(Arc::ptr_eq(
                retained.original_scratch.as_ref().unwrap(),
                &first
            ));
            assert_eq!(retained.lifelines.len(), 3);
            assert!(retained.trusted_inputs.is_none() && retained.original_funding.is_none());
            assert!(retained.original_native_acceptance.is_none());
        }
        drop(first);
        drop(second);
        assert!(first_path.join("sentinel").is_file());
        assert!(second_path.join("sentinel").is_file());
        assert_eq!(
            first_weak
                .upgrade()
                .unwrap()
                .owned_scratch_root()
                .unwrap()
                .identity()
                .unwrap(),
            first_identity
        );
        assert!(second_weak.upgrade().is_some());
        assert!(start.has_retained_custody());
        // Actual descriptor/guard custody only; no opaque trusted inputs or
        // financial/native/physical-close proof is manufactured by this test.
    }

    #[test]
    fn refused_original_scratch_deposits_preserve_first_guard_and_all_incoming_owners() {
        let start = PersistentSessionPooledStart::new();
        let (_first_root, first_path, first) = owned_start_scratch();
        let (_second_root, second_path, second) = owned_start_scratch();
        let first_weak = Arc::downgrade(&first);
        let second_weak = Arc::downgrade(&second);
        {
            let mut retained = start.retained.lock().unwrap();
            // Existing actual refusal state; custody deposition must still
            // precede the production preparation caller's refusal checks.
            retained.native_check_attempted = true;
            retained.native_check_refused = true;
            retained.admission_refused = true;
            retained.phase = PooledStartPhase::Unsettled;
            assert!(PersistentSessionPooledStart::retain_original_scratch(
                &mut retained,
                &first
            ));
            assert!(PersistentSessionPooledStart::native_check_blocks_mutation(
                &retained
            ));
            assert!(!PersistentSessionPooledStart::retain_original_scratch(
                &mut retained,
                &second
            ));
            assert!(Arc::ptr_eq(
                retained.original_scratch.as_ref().unwrap(),
                &first
            ));
            assert!(retained.native_check_attempted && retained.native_check_refused);
            assert!(retained.admission_refused && retained.phase == PooledStartPhase::Unsettled);
            assert!(retained.original_native_acceptance.is_none());
            assert_eq!(retained.lifelines.len(), 2);
        }
        drop(first);
        drop(second);
        assert!(first_weak.upgrade().is_some() && second_weak.upgrade().is_some());
        assert!(first_path.join("sentinel").is_file() && second_path.join("sentinel").is_file());
        assert!(!start.publish());
        assert!(start.prove_cleanup().is_err());
        assert!(first_weak.upgrade().is_some() && second_weak.upgrade().is_some());
        // This is the production scratch-deposition seam plus existing refusal
        // behavior, not a genuine full funding-preparation or native positive.
    }

    #[cfg(target_os = "linux")]
    fn actual_ordinary_held_link() -> Arc<lillux::exec::ProcessLocalHeldOccurrence> {
        let held = lillux::spawn_awaiting_attachment(lillux::SubprocessRequest {
            cmd: "/bin/sh".into(),
            argv0: None,
            args: vec!["-c".into(), "exec /bin/sleep 30".into()],
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: 30.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        })
        .unwrap();
        let link = Arc::new(held.capture_existing_held_occurrence());
        drop(held); // ordinary best-effort child destruction, no native proof
        link
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn armed_native_guard_restores_actual_execution_amid_collision_and_refuses() {
        let start = PersistentSessionPooledStart::new();
        let (_scratch_root, path, scratch) = owned_start_scratch();
        let first = fake_framed_session().unwrap();
        let second = fake_framed_session().unwrap();
        {
            let mut retained = start.retained.lock().unwrap();
            retained.native_check_attempted = true;
            retained.native_check_in_flight = true;
            retained.phase = PooledStartPhase::Unsettled;
            retained.retained_running.push(second.running);
            retained.lifelines.push(Box::new(Arc::clone(&scratch)));
        }
        let guard = OriginalNativeCheckGuard {
            running: Some(first.running),
            held_occurrence: actual_ordinary_held_link(),
            inputs: None,
            original: &start,
            fallback_armed: true,
        };
        assert!(guard.require_exclusive().is_err());
        drop(guard);
        drop(scratch);
        {
            let retained = start.retained.lock().unwrap();
            assert_eq!(retained.retained_running.len(), 2);
            assert!(retained.original_native_acceptance.is_none());
            assert!(retained.native_check_attempted && retained.native_check_refused);
            assert!(!retained.native_check_in_flight);
            assert!(retained.admission_refused && retained.phase == PooledStartPhase::Unsettled);
        }
        assert!(path.join("sentinel").is_file());
        assert!(!start.publish());
        assert!(start.prove_cleanup().is_err());
        // Actual ordinary owners only: missing protected inputs/funding means
        // this is a restoration/refusal control, never an acceptance positive.
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disarmed_native_guard_drop_does_not_add_rejection_or_restore_twice() {
        let start = PersistentSessionPooledStart::new();
        let running = fake_framed_session().unwrap().running;
        {
            let mut retained = start.retained.lock().unwrap();
            retained.native_check_attempted = true;
            retained.phase = PooledStartPhase::Unsettled;
            retained.admission_refused = true;
            retained.retained_running.push(running);
        }
        let guard = OriginalNativeCheckGuard {
            running: None,
            held_occurrence: actual_ordinary_held_link(),
            inputs: None,
            original: &start,
            fallback_armed: false,
        };
        drop(guard);
        {
            let retained = start.retained.lock().unwrap();
            assert_eq!(retained.retained_running.len(), 1);
            assert!(retained.native_check_attempted);
            assert!(!retained.native_check_in_flight && !retained.native_check_refused);
            assert!(retained.admission_refused && retained.phase == PooledStartPhase::Unsettled);
            assert!(retained.original_native_acceptance.is_none());
        }
        assert!(!start.publish());
        assert!(start.prove_cleanup().is_err());
        // Isolated Drop transition only. No staged acceptance, genuine funded
        // fixture or successful atomic commit is manufactured by this test.
    }

    #[test]
    fn refused_native_callback_roots_actual_owners_and_blocks_publication_and_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let fixture =
            crate::handler_context::budget_test_support::build(root.path(), Duration::from_secs(2))
                .unwrap();
        let start = PersistentSessionPooledStart::new();
        let (_scratch_root, path, guard) = owned_start_scratch();
        let called = Arc::new(AtomicBool::new(false));
        let observer: PersistentSessionCleanupObserver = {
            let called = Arc::clone(&called);
            Arc::new(move || {
                called.store(true, Ordering::Release);
                Ok(())
            })
        };
        // Actual ordinary child/state only: missing original funding/inputs
        // must refuse before any native check and can never become evidence.
        let first = fake_framed_session().unwrap();
        let error = start
            .retain_and_check_original_native(
                &fixture.state,
                first.running,
                vec![Box::new(Arc::clone(&guard))],
                Some(Arc::clone(&observer)),
                None,
            )
            .unwrap_err();
        assert!(error.is::<PersistentSessionCleanupUnproved>());
        let second = fake_framed_session().unwrap();
        assert!(
            start
                .retain_and_check_original_native(
                    &fixture.state,
                    second.running,
                    Vec::new(),
                    Some(observer),
                    None,
                )
                .is_err()
        );
        drop(guard);
        assert!(start.has_retained_custody());
        assert!(!start.publish());
        assert!(start.prove_cleanup().is_err());
        assert!(!called.load(Ordering::Acquire));
        {
            let retained = start.retained.lock().unwrap();
            assert_eq!(retained.retained_running.len(), 2);
            assert!(retained.native_check_attempted && retained.native_check_refused);
            assert!(retained.original_native_acceptance.is_none());
            assert!(!retained.native_check_in_flight);
            assert!(retained.phase == PooledStartPhase::Unsettled);
            assert!(!retained.obligation_owners.is_empty());
        }
        assert!(path.join("sentinel").is_file());
        let (_extra_root, extra_path, extra_guard) = owned_start_scratch();
        assert!(
            start
                .retain_before_contact(vec![Box::new(Arc::clone(&extra_guard))])
                .is_err()
        );
        drop(extra_guard);
        assert!(extra_path.join("sentinel").is_file());
        // Destruction is best-effort only; there is no settlement assertion.
    }

    #[test]
    fn unsupported_native_wait_returns_actual_execution_for_ordinary_readiness() {
        let mut started = fake_framed_session().unwrap();
        assert!(!started.running.has_selected_original_launch_context());
        let owner = match started
            .running
            .wait_for_natural_exit_retaining_native(Duration::ZERO)
        {
            Err(owner) => owner,
            Ok(_) => panic!("ordinary owner must not become a selected outcome"),
        };
        assert!(!owner.has_selected_original_launch_context());
        assert!(owner.native_terminal_raw_view().is_none());
        started.running = owner;
        let pool = PersistentSessionPool::new();
        let backlog = Arc::clone(&pool.streams.backlog);
        let process = match ready_process(started, &test_wire(), &owned_start_lifecycle(), backlog)
        {
            Ok(process) => process,
            Err(failure) => panic!("returned actual owner lost readiness: {}", failure.error),
        };
        process.retire().unwrap();
    }

    #[test]
    fn duplicate_actual_running_owners_quarantine_scratch_and_refuse_cleanup() {
        let (_root, path, guard) = owned_start_scratch();
        let start = PersistentSessionPooledStart::new();
        start
            .retain_before_contact(vec![Box::new(Arc::clone(&guard))])
            .unwrap();
        let first = fake_framed_session().unwrap();
        let second = fake_framed_session().unwrap();
        start.retain_running_unsettled(first.running);
        start.retain_running_unsettled(second.running);
        drop(guard);
        assert!(start.has_retained_custody());
        assert!(start.prove_cleanup().is_err());
        {
            let retained = start.retained.lock().unwrap();
            assert_eq!(retained.retained_running.len(), 2);
            assert!(retained.admission_refused);
            assert!(retained.phase == PooledStartPhase::Unsettled);
            assert!(!retained.lifelines.is_empty());
        }
        assert_eq!(
            std::fs::read(path.join("sentinel")).unwrap(),
            b"original-slot-bytes"
        );
        // Existing pool/start destruction is best-effort only, not settlement.
        drop(start);
        assert!(!path.exists());
    }

    #[test]
    fn actual_callback_budget_expires_under_held_contact_retention_lock_without_renewal() {
        let root = tempfile::tempdir().unwrap();
        let fixture =
            crate::handler_context::budget_test_support::build(root.path(), Duration::from_secs(1))
                .unwrap();
        let original = fixture.context.original_execution_budget().unwrap();
        let start = PersistentSessionPooledStart::new();
        start
            .retain_original_callback_budget(original, original.precontact_binding().scope)
            .unwrap();
        let held = start.retained.lock().unwrap();
        assert!(start.lock_original_contact_retention(None).is_err());
        assert!(original.contact_expiry().has_elapsed());
        assert_eq!(start.original_callback_budget.get().unwrap(), original);
        drop(held);
        // Releasing actual contention cannot renew the expired original.
        assert!(start.lock_original_contact_retention(None).is_err());
        assert!(
            start
                .retain_original_callback_budget(original, original.precontact_binding().scope)
                .is_err()
        );
    }

    #[test]
    fn absolute_contact_deadline_caps_existing_signed_io_deadline() {
        let expired = MonotonicDeadline::after(Duration::ZERO);
        assert!(exclusive_request_deadline(60_000, Some(expired)).has_elapsed());
        let distant = MonotonicDeadline::after(Duration::from_secs(60));
        assert!(
            exclusive_request_deadline(100, Some(distant)).remaining()
                <= Duration::from_millis(100)
        );
        let interactive = exclusive_request_deadline(100, None);
        assert!(!interactive.has_elapsed());
        assert!(interactive.remaining() <= Duration::from_millis(100));
    }

    #[test]
    fn expired_contact_deadline_prevents_request_and_control_frame_writes() {
        #[derive(Default)]
        struct RecordingWriter {
            writes: usize,
        }

        impl Write for RecordingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.writes += 1;
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let wire = test_wire();
        for kind in [
            PersistentSessionFrameKind::Request,
            PersistentSessionFrameKind::Control,
        ] {
            let mut writer = RecordingWriter::default();
            let frame = PersistentSessionFrame {
                protocol: wire.wire_protocol.clone(),
                version: wire.wire_version,
                kind,
                request_id: Some("expired-request".into()),
                body: Some(serde_json::json!({"kind":"fixture"})),
            };
            let error = write_frame(
                &mut writer,
                &wire,
                &frame,
                MonotonicDeadline::after(Duration::ZERO),
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("frame write exceeded its deadline")
            );
            assert_eq!(writer.writes, 0);
        }
    }

    #[test]
    fn writer_contention_cannot_outlive_the_contact_deadline() {
        let (writer, _peer) = lillux::inherited_duplex_channel_pair().unwrap();
        let writer = Mutex::new(writer);
        let held = writer.lock().unwrap();
        // A same-thread holder makes an unconditional lock deadlock. The
        // bounded acquisition must return without needing that lock released.
        assert!(
            lock_writer_before_deadline(
                &writer,
                MonotonicDeadline::after(Duration::from_millis(1)),
            )
            .is_err()
        );
        drop(held);
        assert!(
            lock_writer_before_deadline(&writer, MonotonicDeadline::after(Duration::from_secs(1)),)
                .is_ok()
        );
    }

    #[test]
    fn bounded_stream_error_retains_the_terminal_exception() {
        let error = format!("{}FINAL_EXCEPTION", "traceback frame\n".repeat(512));
        let bounded = bounded_stream_error(&error);
        assert!(bounded.chars().count() <= 2_048);
        assert!(bounded.starts_with("… (earlier error text omitted)"));
        assert!(bounded.ends_with("FINAL_EXCEPTION"));
    }

    fn host_executable(name: &str) -> String {
        let search_path = std::env::var_os("PATH").expect("test PATH is set");
        std::env::split_paths(&search_path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
            .and_then(|candidate| std::fs::canonicalize(candidate).ok())
            .unwrap_or_else(|| panic!("test executable `{name}` is unavailable"))
            .to_string_lossy()
            .into_owned()
    }

    fn fake_framed_session_with_observation_sink(
        observation_sink: Option<PersistentSessionObservationSink>,
    ) -> Result<StartedPersistentSession> {
        use std::collections::HashMap;

        use ryeos_engine::contracts::{
            EffectivePrincipal, EngineContext, ExecutionDecorations, ExecutionPlan, LaunchMode,
            PlanCapabilities, PlanNode, PlanNodeId, PlanSubprocessSpec, Principal, ProjectContext,
        };

        let app_root = tempfile::tempdir()?;
        let policy_dir = app_root.path().join(".ai/test-fixtures");
        std::fs::create_dir_all(&policy_dir)?;
        std::fs::write(
            policy_dir.join("isolation-policy.yaml"),
            serde_yaml::to_string(
                &ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring(),
            )?,
        )?;
        let isolation = Arc::new(ryeos_engine::isolation::IsolationRuntime::load(
            app_root.path(),
        )?);
        let (daemon_channel, worker_channel) =
            lillux::inherited_duplex_channel_pair().map_err(anyhow::Error::msg)?;
        let daemon_socket = daemon_channel;
        let target_channel = ryeos_engine::isolation::IsolationTargetChannelAuthority::new(
            worker_channel,
            0,
            "RYEOS_SESSION_FD",
        )?;
        let script = r#"
import json, os, struct
fd = int(os.environ['RYEOS_SESSION_FD'])
def read_exact(size):
    value = b''
    while len(value) < size:
        part = os.read(fd, size - len(value))
        if not part:
            raise SystemExit(0)
        value += part
    return value
def write_all(value):
    offset = 0
    while offset < len(value):
        offset += os.write(fd, value[offset:])
def send(kind, request_id, body):
    value = {'protocol':'test.session','version':1,'kind':kind,'request_id':request_id,'body':body}
    raw = json.dumps(value, separators=(',', ':'), allow_nan=False).encode()
    write_all(struct.pack('>I', len(raw)) + raw)
def receive():
    head = read_exact(4)
    size = struct.unpack('>I', head)[0]
    return json.loads(read_exact(size))
send('ready', None, None)
while True:
    frame = receive()
    if frame['kind'] == 'request':
        send('delta', frame['request_id'], {'text':'fixture'})
        if frame['body'].get('await_delta_ack'):
            acknowledgement = receive()
            if (acknowledgement['kind'] != 'observation_ack'
                    or acknowledgement['request_id'] != frame['request_id']
                    or acknowledgement['body'] != {'accepted':'fixture'}):
                raise SystemExit(3)
        if frame['body'].get('emit_observation'):
            send('observation_batch', None, {
                'first_sequence':1,
                'count':1,
                'batch_digest':'fixture-digest',
                'events':[{'event_type':'fixture','payload':{}}],
                'session_observations':[]
            })
            acknowledgement = receive()
            if acknowledgement['kind'] != 'observation_ack':
                raise SystemExit(2)
        send('final', frame['request_id'], {'echo':frame['body']})
    elif frame['kind'] == 'control':
        if frame['body'].get('force_expired'):
            send('final', frame['request_id'], {
                'resolved':False,
                'outcome':'expired',
                'request_id':frame['body']['request_id'],
                'request_digest':frame['body']['request_digest']
            })
        else:
            send('final', frame['request_id'], {'echo':frame['body']})
    elif frame['kind'] == 'cancel':
        send('error', frame['request_id'], {'message':'fixture observed cancellation'})
"#;
        let spec = PlanSubprocessSpec {
            cmd: host_executable("python3"),
            verified_command: None,
            args: vec!["-S".into(), "-c".into(), script.into()],
            cwd: None,
            env: HashMap::new(),
            env_sources: HashMap::new(),
            stdin: None,
            timeout_secs: 30,
            execution: ExecutionDecorations::default(),
        };
        let plan = ExecutionPlan {
            plan_id: "plan:fake-persistent-session".to_owned(),
            root_executor_id: "runtime:fixture".to_owned(),
            root_ref: "worker:fixture/session".to_owned(),
            item_kind: "worker".to_owned(),
            nodes: vec![PlanNode::DispatchSubprocess {
                id: PlanNodeId("spawn".to_owned()),
                spec: Box::new(spec),
                tool_path: None,
                executor_chain: Vec::new(),
            }],
            entrypoint: PlanNodeId("spawn".to_owned()),
            capabilities: PlanCapabilities::default(),
            materialization_requirements: Vec::new(),
            network_authority_ceiling:
                ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy,
            filesystem_authority_ceiling:
                ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
            target_requirement: None,
            current_target_request: None,
            endpoint_requirement: ryeos_engine::contracts::ExecutionEndpointRequirement::Local {},
            external_endpoint_binding: None,
            subprocess_limits: None,
            resource_authority_ceiling:
                ryeos_engine::contracts::ExecutionResourceAuthorityCeiling::NodePolicy,
            cache_key: "fixture".to_owned(),
            thread_kind: Some("worker".to_owned()),
            executor_chain: Vec::new(),
            executor_authorities: Vec::new(),
            runtime_identity: None,
            debug_raw: false,
        };
        let context = EngineContext {
            app_root: app_root.path().to_path_buf(),
            isolation,
            isolation_project_authority:
                ryeos_engine::isolation::IsolationProjectAuthority::External,
            isolation_immutable_project: None,
            isolation_workspace_view: None,
            isolation_filesystem_authority_ceiling:
                ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
            isolation_network_authority_ceiling:
                ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy,
            isolation_live_access_authority: Some(
                ryeos_engine::isolation::IsolationLiveAccessAuthority::UnconfinedHost {
                    authorized_write_namespaces: vec!["project".into()],
                },
            ),
            isolation_state_root: None,
            isolation_checkpoint_dir: None,
            isolation_checkpoint_authority: None,
            isolation_daemon_socket_path: None,
            isolation_bundle_roots: Vec::new(),
            isolation_node_trusted_keys_dir: None,
            isolation_verified_code: Vec::new(),
            isolation_verified_command: None,
            isolation_external_read_only_mounts: Vec::new(),
            isolation_writable_runtime_view_mounts: Vec::new(),
            isolation_target_channels: vec![target_channel],
            isolation_workspace: None,
            subprocess_limits: None,
            inherited_fds: Vec::new(),
            thread_id: "session:fixture".to_owned(),
            chain_root_id: "session:fixture".to_owned(),
            current_site_id: "site:fixture".to_owned(),
            origin_site_id: "site:fixture".to_owned(),
            upstream_site_id: None,
            upstream_thread_id: None,
            continuation_from_id: None,
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fixture".to_owned(),
                scopes: Vec::new(),
            }),
            project_context: ProjectContext::None,
            launch_mode: LaunchMode::Wait,
        };
        let pending = ryeos_engine::dispatch::spawn_plan(&plan, &context)
            .map_err(|error| anyhow!("spawn fake persistent-session worker: {error}"))?;
        let running = pending
            .release_after_attachment()
            .map_err(|error| anyhow!("release fake persistent-session worker: {error}"))?;
        Ok(StartedPersistentSession {
            running,
            socket: daemon_socket,
            lifelines: vec![Box::new(app_root)],
            relay_health: None,
            relay_owner: None,
            expected_boot_identity: None,
            observation_sink,
            cleanup_observer: None,
            retirement_observer: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
            resource_attribution_sink: None,
            resource_eligibility: None,
        })
    }

    fn fake_framed_session() -> Result<StartedPersistentSession> {
        fake_framed_session_with_observation_sink(None)
    }

    fn narrow_stream_limits() -> PersistentSessionPoolLimits {
        PersistentSessionPoolLimits {
            max_pool_groups: 2,
            max_total_processes: 1,
            max_total_address_space_bytes: 64 * 1024 * 1024,
            max_total_cpu_seconds: 1,
            max_real_uid_process_limit: 1,
            max_open_streams: 4,
            max_active_streams: 2,
            max_active_streams_per_subject: 1,
            max_stream_backlog_bytes: 1024,
            max_total_backlog_bytes: 2048,
        }
    }

    fn test_lifecycle() -> PersistentSessionLifecycleContract {
        PersistentSessionLifecycleContract {
            max_processes: 1,
            max_inflight_per_process: 1,
            max_address_space_bytes: 64 * 1024 * 1024,
            max_cpu_seconds: 1,
            real_uid_process_limit: 1,
            ready_timeout_ms: 100,
            request_timeout_ms: 100,
            idle_timeout_ms: 100,
        }
    }

    fn test_wire() -> PersistentSessionWireContract {
        PersistentSessionWireContract {
            channel_env: "RYEOS_SESSION_FD".to_owned(),
            wire_protocol: "test.session".to_owned(),
            wire_version: 1,
            max_frame_bytes: 4096,
        }
    }

    fn owned_start_scratch() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        Arc<crate::temp_dir_guard::TempDirGuard>,
    ) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("cache")).unwrap();
        let (path, guard) = crate::temp_dir_guard::create_projectless_workspace(
            &root.path().join("cache"),
            "owned-start-control",
        )
        .unwrap();
        std::fs::write(path.join("sentinel"), b"original-slot-bytes").unwrap();
        (root, path, guard)
    }

    fn owned_start_lifecycle() -> PersistentSessionLifecycleContract {
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 2_000;
        lifecycle.request_timeout_ms = 2_000;
        lifecycle.idle_timeout_ms = 60_000;
        lifecycle
    }

    struct TestRelayOwner(std::sync::atomic::AtomicU64);

    impl PersistentSessionRelayOwner for TestRelayOwner {
        fn stop_and_join(&self) -> Result<()> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    struct PanickingRelayOwner;

    impl PersistentSessionRelayOwner for PanickingRelayOwner {
        fn stop_and_join(&self) -> Result<()> {
            panic!("injected relay join panic")
        }
    }

    struct PanickingDropRelayOwner;

    impl PersistentSessionRelayOwner for PanickingDropRelayOwner {
        fn stop_and_join(&self) -> Result<()> {
            Ok(())
        }
    }

    impl Drop for PanickingDropRelayOwner {
        fn drop(&mut self) {
            panic!("injected final relay owner drop panic")
        }
    }

    #[test]
    fn exclusive_join_panic_is_an_unproved_result_with_owner_still_reachable() {
        let owner: Arc<dyn PersistentSessionRelayOwner> = Arc::new(PanickingRelayOwner);
        let result = safe_relay_join(Some(&owner));
        assert!(result.is_err());
        assert_eq!(Arc::strong_count(&owner), 1);
    }

    #[test]
    fn readiness_refusal_join_panic_keeps_custody_and_skips_settlement() {
        let started = fake_framed_session().unwrap();
        let settlement_calls = Arc::new(AtomicU64::new(0));
        let observed_calls = Arc::clone(&settlement_calls);
        let failure = refuse_ready_process(
            started.running,
            anyhow!("injected readiness refusal"),
            ExclusiveUnprovedStartCustody {
                relay_health: Some(PersistentSessionRelayHealthCell::new()),
                relay_owner: Some(Arc::new(PanickingRelayOwner)),
                lifelines: started.lifelines,
                observer: Some(Arc::new(move || {
                    observed_calls.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                })),
            },
        );
        assert!(failure.cleanup_unproved);
        assert!(failure.custody.is_some());
        assert_eq!(settlement_calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn runtime_retirement_catches_consumed_final_relay_owner_drop() {
        let owner: Arc<dyn PersistentSessionRelayOwner> = Arc::new(PanickingDropRelayOwner);
        let process = ready_test_process(PersistentSessionRelayHealthCell::new(), Arc::clone(&owner))
            .unwrap();
        drop(owner);
        assert!(process.retire().is_err());
        assert!(process.cleanup_unproved.lock().unwrap().is_some());
    }

    #[test]
    fn exclusive_unproved_start_custody_stays_in_its_charged_reservation() {
        let pool = PersistentSessionPool::new();
        let session_id = "q".repeat(64);
        let mut reservation = pool
            .reserve_exclusive(&session_id, &test_lifecycle(), &test_wire())
            .unwrap();
        let owner: Arc<dyn PersistentSessionRelayOwner> =
            Arc::new(TestRelayOwner(std::sync::atomic::AtomicU64::new(0)));
        reservation.quarantine_start_custody(
            ExclusiveUnprovedStartCustody {
                relay_health: Some(PersistentSessionRelayHealthCell::new()),
                relay_owner: Some(Arc::clone(&owner)),
                lifelines: vec![Box::new(Arc::new(AtomicU64::new(0)))],
                observer: Some(Arc::new(|| Ok(()))),
            },
            "injected unproved reap".to_owned(),
        );
        let state = pool.inner.state.lock().unwrap();
        let retained = state.exclusive_reservations.get(&session_id).unwrap();
        assert_eq!(retained.quarantined.len(), 1);
        assert!(retained.quarantined_owner_count() >= 4);
        assert!(state.cleanup_unproved.is_some());
        assert_eq!(aggregate_process_capacity(&state).0, 1);
        assert_eq!(Arc::strong_count(&owner), 2);
    }

    #[test]
    fn readiness_unwind_guard_deposits_custody_before_reservation_drop() {
        let pool = PersistentSessionPool::new();
        let session_id = "u".repeat(64);
        let reservation = pool
            .reserve_exclusive(&session_id, &test_lifecycle(), &test_wire())
            .unwrap();
        let guard = ExclusiveReadyCustodyGuard {
            custody: Some(ExclusiveUnprovedStartCustody {
                relay_health: None,
                relay_owner: None,
                lifelines: vec![Box::new(Arc::new(AtomicU64::new(0)))],
                observer: None,
            }),
            registration: Some(ExclusiveCustodyRegistration {
                inner: Arc::clone(&pool.inner),
                session_id: session_id.clone(),
            }),
        };
        drop(guard);
        drop(reservation);
        let state = pool.inner.state.lock().unwrap();
        assert_eq!(state.exclusive_reservations.len(), 1);
        assert_eq!(
            state.exclusive_reservations[&session_id].quarantined_owner_count(),
            1
        );
        assert!(state.cleanup_unproved.is_some());
    }

    fn ready_test_process(
        health: PersistentSessionRelayHealthCell,
        relay_owner: Arc<dyn PersistentSessionRelayOwner>,
    ) -> Result<SessionProcess> {
        let mut started = fake_framed_session()?;
        started.relay_health = Some(health);
        started.relay_owner = Some(relay_owner);
        let pool = PersistentSessionPool::new();
        ready_process(
            started,
            &test_wire(),
            &owned_start_lifecycle(),
            Arc::clone(&pool.streams.backlog),
        )
        .map_err(|failure| failure.error)
    }

    #[test]
    fn ordinary_relay_health_failure_is_sticky_over_stop() {
        let health = PersistentSessionRelayHealthCell::new();
        health.stop();
        assert_eq!(health.snapshot(), PersistentSessionRelayHealth::Stopped);
        health.fail(PersistentSessionRelayFailure::PumpPanicked);
        assert_eq!(
            health.snapshot(),
            PersistentSessionRelayHealth::Failed(PersistentSessionRelayFailure::PumpPanicked)
        );
        health.stop();
        assert_eq!(
            health.snapshot(),
            PersistentSessionRelayHealth::Failed(PersistentSessionRelayFailure::PumpPanicked)
        );
    }

    #[test]
    fn terminal_relay_health_refuses_ready_before_owner_publication() {
        let health = PersistentSessionRelayHealthCell::new();
        health.fail(PersistentSessionRelayFailure::SignedFrameBudgetExhausted);
        let owner = Arc::new(TestRelayOwner(std::sync::atomic::AtomicU64::new(0)));
        let mut started = fake_framed_session().unwrap();
        started.relay_health = Some(health);
        started.relay_owner = Some(owner.clone());
        let pool = PersistentSessionPool::new();
        let failure = match ready_process(
            started,
            &test_wire(),
            &owned_start_lifecycle(),
            Arc::clone(&pool.streams.backlog),
        ) {
            Ok(process) => {
                let _ = process.retire();
                panic!("terminal relay owner was published as Ready");
            }
            Err(failure) => failure,
        };
        assert!(!failure.cleanup_unproved);
        assert_eq!(owner.0.load(Ordering::Acquire), 1);
    }

    #[test]
    fn failed_relay_refuses_first_worker_contact_and_retires_exact_owner() {
        let health = PersistentSessionRelayHealthCell::new();
        let owner = Arc::new(TestRelayOwner(std::sync::atomic::AtomicU64::new(0)));
        let process = ready_test_process(health.clone(), owner.clone()).unwrap();
        health.fail(PersistentSessionRelayFailure::ContactFailed);
        let error = execute_on_process(
            &process,
            &test_wire(),
            PersistentSessionFrameKind::Request,
            serde_json::json!({"probe":true}),
            &|| false,
            MonotonicDeadline::after(Duration::from_secs(1)),
            &mut |_| Ok(None),
        )
        .unwrap_err();
        assert!(error.to_string().contains("ordinary relay failed"));
        process.retire().unwrap();
        assert_eq!(owner.0.load(Ordering::Acquire), 1);
    }

    #[test]
    fn terminal_relay_failure_wins_before_final_acceptance_snapshot() {
        let health = PersistentSessionRelayHealthCell::new();
        let owner = Arc::new(TestRelayOwner(std::sync::atomic::AtomicU64::new(0)));
        let process = ready_test_process(health.clone(), owner.clone()).unwrap();
        let accepted_before_failure = accept_final_response(
            &process,
            serde_json::json!({"ok":true}),
            false,
        )
        .unwrap();
        health.fail(PersistentSessionRelayFailure::ContactFailed);
        assert_eq!(accepted_before_failure, serde_json::json!({"ok":true}));
        assert!(
            accept_final_response(&process, serde_json::json!({"ok":false}), false).is_err()
        );
        process.retire().unwrap();
    }

    #[test]
    fn retirement_unwind_publishes_sticky_unproved_and_wakes_waiters() {
        let health = PersistentSessionRelayHealthCell::new();
        let owner = Arc::new(TestRelayOwner(std::sync::atomic::AtomicU64::new(0)));
        let process = Arc::new(
            ready_test_process(health, owner.clone()).unwrap(),
        );
        let attempt = process.begin_retirement().unwrap().unwrap();
        let waiter_process = Arc::clone(&process);
        let waiter = std::thread::spawn(move || waiter_process.begin_retirement().is_err());
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(attempt);
            panic!("injected retirement unwind");
        }));
        assert!(unwind.is_err());
        assert!(waiter.join().unwrap());
        assert!(matches!(
            *process.retirement.state.lock().unwrap(),
            RetirementState::Unproved(_)
        ));
        assert!(process.cleanup_unproved.lock().unwrap().is_some());
        let running = process.running.lock().unwrap().take().unwrap();
        running.abort_and_reap_checked().unwrap();
    }

    #[test]
    fn retirement_with_preexisting_cleanup_failure_returns_without_mutex_reentry() {
        let health = PersistentSessionRelayHealthCell::new();
        let owner = Arc::new(TestRelayOwner(std::sync::atomic::AtomicU64::new(0)));
        let process = ready_test_process(health, owner.clone()).unwrap();
        *process.cleanup_unproved.lock().unwrap() = Some("injected prior failure".to_owned());
        assert!(process.retire().is_err());
        assert!(matches!(
            *process.retirement.state.lock().unwrap(),
            RetirementState::Unproved(_)
        ));
        assert_eq!(owner.0.load(Ordering::Acquire), 0);
        let running = process.running.lock().unwrap().take().unwrap();
        running.abort_and_reap_checked().unwrap();
    }

    #[test]
    fn poisoned_retirement_coordinator_publishes_unproved_not_reaped() {
        let health = PersistentSessionRelayHealthCell::new();
        let owner = Arc::new(TestRelayOwner(std::sync::atomic::AtomicU64::new(0)));
        let process = Arc::new(ready_test_process(health, owner.clone()).unwrap());
        let poisoner_process = Arc::clone(&process);
        assert!(std::thread::spawn(move || {
            let _state = poisoner_process.retirement.state.lock().unwrap();
            panic!("inject coordinator poison");
        })
        .join()
        .is_err());
        assert!(process.begin_retirement().is_err());
        assert!(matches!(
            *process.retirement.state.lock().unwrap_or_else(|p| p.into_inner()),
            RetirementState::Unproved(_)
        ));
        assert!(process.cleanup_unproved.lock().unwrap().is_some());
        let running = process.running.lock().unwrap().take().unwrap();
        running.abort_and_reap_checked().unwrap();
    }

    #[test]
    fn owned_pooled_start_retains_original_scratch_before_callback_error() {
        let pool = PersistentSessionPool::new();
        let (_root, path, guard) = owned_start_scratch();
        let key = "a".repeat(64);
        let error = pool
            .execute_attributed_owned_start(
                &key,
                &owned_start_lifecycle(),
                &test_wire(),
                None,
                serde_json::json!({}),
                |original| {
                    let state = pool.inner.state.lock().unwrap();
                    let group = state.groups.get(&key).unwrap();
                    assert_eq!(group.starts.len(), 1);
                    assert!(std::ptr::eq(Arc::as_ptr(&group.starts[0]), original));
                    assert_eq!(aggregate_process_capacity(&state).0, 1);
                    drop(state);
                    original.retain_before_contact(vec![Box::new(Arc::clone(&guard))])?;
                    Err(anyhow!("control refuses before actual process contact"))
                },
                || false,
                |_| Ok(()),
            )
            .unwrap_err();
        assert!(error.to_string().contains("before actual process contact"));
        drop(guard);
        assert_eq!(
            std::fs::read(path.join("sentinel")).unwrap(),
            b"original-slot-bytes"
        );
        let state = pool.inner.state.lock().unwrap();
        assert!(state.cleanup_unproved.is_some());
        assert_eq!(aggregate_process_capacity(&state).0, 1);
        assert_eq!(state.groups[&key].starts.len(), 1);
        assert!(!state.groups[&key].starts[0].is_in_flight());
        drop(state);
        pool.remove_empty_group(&key);
        assert!(
            pool.shutdown_and_reap_all(Duration::from_millis(200))
                .is_err()
        );
        assert!(path.join("sentinel").is_file());
        // Resource-free local control: no journal/restart/device proof asserted.
    }

    #[test]
    fn owned_pooled_start_unwind_keeps_same_group_capacity_and_original_bytes() {
        let pool = PersistentSessionPool::new();
        let (_root, path, guard) = owned_start_scratch();
        let key = "b".repeat(64);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.execute_attributed_owned_start(
                &key,
                &owned_start_lifecycle(),
                &test_wire(),
                None,
                serde_json::json!({}),
                |original| {
                    original.retain_before_contact(vec![Box::new(Arc::clone(&guard))])?;
                    panic!("control callback unwind before process contact");
                },
                || false,
                |_| Ok(()),
            )
        }));
        assert!(result.is_err());
        drop(guard);
        let state = pool.inner.state.lock().unwrap();
        assert!(state.cleanup_unproved.is_some());
        assert_eq!(aggregate_process_capacity(&state).0, 1);
        assert_eq!(state.groups[&key].starts.len(), 1);
        assert!(!state.groups[&key].starts[0].is_in_flight());
        assert!(path.join("sentinel").is_file());
    }

    #[test]
    fn owned_pooled_start_ready_refusal_keeps_lifelines_when_observer_refuses() {
        let pool = PersistentSessionPool::new();
        let (_root, path, guard) = owned_start_scratch();
        let key = "c".repeat(64);
        let mut wire = test_wire();
        wire.wire_protocol = "different.actual.ready.protocol".to_owned();
        let observer_called = Arc::new(AtomicBool::new(false));
        let error = pool
            .execute_attributed_owned_start(
                &key,
                &owned_start_lifecycle(),
                &wire,
                None,
                serde_json::json!({}),
                |original| {
                    original.retain_before_contact(vec![Box::new(Arc::clone(&guard))])?;
                    let mut started = fake_framed_session()?;
                    let called = Arc::clone(&observer_called);
                    started.retirement_observer = Some(Arc::new(move || {
                        called.store(true, Ordering::Release);
                        Err(anyhow!("local retirement uncertainty control"))
                    }));
                    Ok(started)
                },
                || false,
                |_| Ok(()),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("local retirement uncertainty"));
        assert!(observer_called.load(Ordering::Acquire));
        drop(guard);
        let state = pool.inner.state.lock().unwrap();
        assert!(state.cleanup_unproved.is_some());
        let group = &state.groups[&key];
        assert!(group.processes.is_empty());
        assert_eq!(group.starts.len(), 1);
        assert!(group.starts[0].has_retained_custody());
        assert!(!group.starts[0].is_in_flight());
        assert!(path.join("sentinel").is_file());
    }

    #[test]
    fn owned_pooled_start_shutdown_crossing_retains_published_owner_on_uncertainty() {
        let pool = PersistentSessionPool::new();
        let (_root, path, guard) = owned_start_scratch();
        let key = "d".repeat(64);
        let error = pool
            .execute_attributed_owned_start(
                &key,
                &owned_start_lifecycle(),
                &test_wire(),
                None,
                serde_json::json!({}),
                |original| {
                    original.retain_before_contact(vec![Box::new(Arc::clone(&guard))])?;
                    let mut started = fake_framed_session()?;
                    started.retirement_observer =
                        Some(Arc::new(|| Err(anyhow!("shutdown uncertainty control"))));
                    pool.inner.shutdown.store(true, Ordering::Release);
                    Ok(started)
                },
                || false,
                |_| Ok(()),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("shutdown uncertainty"));
        drop(guard);
        let state = pool.inner.state.lock().unwrap();
        assert!(state.cleanup_unproved.is_some());
        let group = &state.groups[&key];
        assert!(group.starts.is_empty());
        assert_eq!(group.processes.len(), 1);
        assert!(
            group.processes[0]
                .pooled_start
                .as_ref()
                .unwrap()
                .has_retained_custody()
        );
        assert!(group.processes[0].retire().is_err());
        assert_eq!(
            std::fs::read(path.join("sentinel")).unwrap(),
            b"original-slot-bytes"
        );
    }

    #[test]
    fn owned_pooled_start_proved_retirement_releases_original_guard_with_extra_holder() {
        let pool = PersistentSessionPool::new();
        let (_root, path, guard) = owned_start_scratch();
        let key = "e".repeat(64);
        pool.execute_attributed_owned_start(
            &key,
            &owned_start_lifecycle(),
            &test_wire(),
            None,
            serde_json::json!({"message":"owned-start"}),
            |original| {
                original.retain_before_contact(vec![Box::new(Arc::clone(&guard))])?;
                fake_framed_session()
            },
            || false,
            |_| Ok(()),
        )
        .unwrap();
        let process = {
            let state = pool.inner.state.lock().unwrap();
            let group = &state.groups[&key];
            assert!(group.starts.is_empty());
            assert_eq!(group.processes.len(), 1);
            Arc::clone(&group.processes[0])
        };
        drop(guard);
        assert!(path.join("sentinel").is_file());
        assert!(
            process
                .pooled_start
                .as_ref()
                .unwrap()
                .has_retained_custody()
        );
        assert_eq!(
            pool.shutdown_and_reap_all(Duration::from_secs(2)).unwrap(),
            1
        );
        assert!(!path.exists());
        assert!(
            !process
                .pooled_start
                .as_ref()
                .unwrap()
                .has_retained_custody()
        );
        process.retire().unwrap();
    }

    fn install_owned_start_pause(
        pool: &PersistentSessionPool,
        stage: PooledStartPauseStage,
    ) -> (Receiver<()>, SyncSender<()>) {
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        *pool.inner.pooled_start_test_pause.lock().unwrap() =
            Some(Arc::new(PooledStartTestPause {
                stage,
                entered: entered_tx,
                release: Mutex::new(release_rx),
            }));
        (entered_rx, release_tx)
    }

    fn wait_owned_start_shutdown_entered(pool: &PersistentSessionPool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !pool.inner.shutdown.load(Ordering::Acquire) {
            assert!(
                std::time::Instant::now() < deadline,
                "actual shutdown store was not observed"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn owned_pooled_start_shutdown_waits_for_failure_publication_and_current_quarantine() {
        let pool = PersistentSessionPool::new();
        let (_root, path, guard) = owned_start_scratch();
        let key = "f".repeat(64);
        let (entered, release) =
            install_owned_start_pause(&pool, PooledStartPauseStage::OutcomePublication);
        let worker_pool = pool.clone();
        let worker_key = key.clone();
        let worker_guard = Arc::clone(&guard);
        let worker = std::thread::spawn(move || {
            let mut wire = test_wire();
            wire.wire_protocol = "refused.actual.ready.protocol".to_owned();
            worker_pool.execute_attributed_owned_start(
                &worker_key,
                &owned_start_lifecycle(),
                &wire,
                None,
                serde_json::json!({}),
                |original| {
                    original.retain_before_contact(vec![Box::new(Arc::clone(&worker_guard))])?;
                    let mut started = fake_framed_session()?;
                    started.retirement_observer =
                        Some(Arc::new(|| Err(anyhow!("failure-publication uncertainty"))));
                    Ok(started)
                },
                || false,
                |_| Ok(()),
            )
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        {
            let state = pool.inner.state.lock().unwrap();
            assert!(state.cleanup_unproved.is_none()); // Exact formerly vulnerable window.
            let slot = &state.groups[&key].starts[0];
            assert!(slot.retained.lock().unwrap().phase == PooledStartPhase::Unsettled);
            assert!(slot.is_in_flight()); // Phase is NOT callback completion.
        }
        let shutdown_pool = pool.clone();
        let (done_tx, done_rx) = sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            done_tx
                .send(shutdown_pool.shutdown_and_reap_all(Duration::from_secs(2)))
                .unwrap()
        });
        wait_owned_start_shutdown_entered(&pool);
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        release.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        shutdown.join().unwrap();
        drop(guard);
        let state = pool.inner.state.lock().unwrap();
        assert!(state.cleanup_unproved.is_some());
        assert_eq!(state.groups[&key].starts.len(), 1);
        assert!(!state.groups[&key].starts[0].is_in_flight());
        assert!(path.join("sentinel").is_file());
    }

    #[test]
    fn owned_pooled_start_shutdown_waits_through_reader_admission() {
        let pool = PersistentSessionPool::new();
        let (_root, path, guard) = owned_start_scratch();
        let key = "1".repeat(64);
        let (entered, release) =
            install_owned_start_pause(&pool, PooledStartPauseStage::ReaderAdmission);
        let worker_pool = pool.clone();
        let worker_key = key.clone();
        let worker_guard = Arc::clone(&guard);
        let worker = std::thread::spawn(move || {
            worker_pool.execute_attributed_owned_start(
                &worker_key,
                &owned_start_lifecycle(),
                &test_wire(),
                None,
                serde_json::json!({}),
                |original| {
                    original.retain_before_contact(vec![Box::new(Arc::clone(&worker_guard))])?;
                    fake_framed_session()
                },
                || false,
                |_| Ok(()),
            )
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        {
            let state = pool.inner.state.lock().unwrap();
            let group = &state.groups[&key];
            assert_eq!(group.processes.len(), 1);
            assert_eq!(group.starts.len(), 1);
            assert!(group.starts[0].is_in_flight());
            assert_eq!(aggregate_process_capacity(&state).0, 1); // SAME occurrence counted once.
        }
        let shutdown_pool = pool.clone();
        let (done_tx, done_rx) = sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            done_tx
                .send(shutdown_pool.shutdown_and_reap_all(Duration::from_secs(2)))
                .unwrap()
        });
        wait_owned_start_shutdown_entered(&pool);
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        release.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_ok()
        );
        shutdown.join().unwrap();
        drop(guard);
        assert!(!path.exists());
        assert!(pool.inner.state.lock().unwrap().groups.is_empty());
    }

    #[test]
    fn owned_pooled_start_original_refusing_obligation_survives_started_omission_once() {
        let pool = PersistentSessionPool::new();
        let (_root, path, guard) = owned_start_scratch();
        let key = "2".repeat(64);
        let calls = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&calls);
        let original_observer: PersistentSessionCleanupObserver = Arc::new(move || {
            counted.fetch_add(1, Ordering::AcqRel);
            Err(anyhow!(
                "original pre-contact obligation refuses retirement"
            ))
        });
        pool.execute_attributed_owned_start(
            &key,
            &owned_start_lifecycle(),
            &test_wire(),
            None,
            serde_json::json!({}),
            |original| {
                original.retain_before_contact(vec![Box::new(Arc::clone(&guard))])?;
                original.retain_cleanup_obligations(None, Some(Arc::clone(&original_observer)))?;
                original.retain_cleanup_obligations(None, Some(Arc::clone(&original_observer)))?;
                let started = fake_framed_session()?;
                assert!(
                    started.cleanup_observer.is_none() && started.retirement_observer.is_none()
                );
                Ok(started)
            },
            || false,
            |_| Ok(()),
        )
        .unwrap();
        assert!(pool.shutdown_and_reap_all(Duration::from_secs(2)).is_err());
        assert_eq!(calls.load(Ordering::Acquire), 1);
        drop(guard);
        let process = {
            let state = pool.inner.state.lock().unwrap();
            assert!(state.cleanup_unproved.is_some());
            Arc::clone(&state.groups[&key].processes[0])
        };
        assert!(process.retire().is_err());
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert!(
            process
                .pooled_start
                .as_ref()
                .unwrap()
                .has_retained_custody()
        );
        assert!(path.join("sentinel").is_file());
    }

    #[test]
    fn owned_pooled_start_reader_refusal_keeps_original_owner_and_obligation() {
        let pool = PersistentSessionPool::new();
        let (_root, path, guard) = owned_start_scratch();
        let key = "3".repeat(64);
        let (entered, release) =
            install_owned_start_pause(&pool, PooledStartPauseStage::ReaderAdmission);
        let worker_pool = pool.clone();
        let worker_key = key.clone();
        let worker_guard = Arc::clone(&guard);
        let calls = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&calls);
        let worker = std::thread::spawn(move || {
            worker_pool.execute_attributed_owned_start(
                &worker_key,
                &owned_start_lifecycle(),
                &test_wire(),
                None,
                serde_json::json!({}),
                |original| {
                    original.retain_before_contact(vec![Box::new(Arc::clone(&worker_guard))])?;
                    let counted = Arc::clone(&counted);
                    original.retain_cleanup_obligations(
                        None,
                        Some(Arc::new(move || {
                            counted.fetch_add(1, Ordering::AcqRel);
                            Err(anyhow!("reader refusal original retirement uncertainty"))
                        })),
                    )?;
                    fake_framed_session()
                },
                || false,
                |_| Ok(()),
            )
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let process = {
            let state = pool.inner.state.lock().unwrap();
            Arc::clone(&state.groups[&key].processes[0])
        };
        // Close the ACTUAL pre-reader channel to exercise the real refusal.
        assert!(process.reader.lock().unwrap().take().is_some());
        release.send(()).unwrap();
        let error = worker.join().unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("reader already started"));
        assert_eq!(calls.load(Ordering::Acquire), 1);
        drop(guard);
        let state = pool.inner.state.lock().unwrap();
        assert!(state.cleanup_unproved.is_some());
        assert_eq!(state.groups[&key].processes.len(), 1);
        assert!(state.groups[&key].starts.is_empty());
        assert!(
            process
                .pooled_start
                .as_ref()
                .unwrap()
                .has_retained_custody()
        );
        assert!(!process.pooled_start.as_ref().unwrap().is_in_flight());
        assert!(path.join("sentinel").is_file());
    }

    #[test]
    fn owned_pooled_start_mismatched_obligations_retain_all_supplied_owners_without_execution() {
        struct ClosureOwner(Arc<AtomicBool>);
        impl Drop for ClosureOwner {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        for stage in [0, 1, 2] {
            let supplied_at_ready = stage == 1;
            let pool = PersistentSessionPool::new();
            let (_root, path, guard) = owned_start_scratch();
            let key = if supplied_at_ready { "4" } else { "5" }.repeat(64);
            let resource_dropped = Arc::new(AtomicBool::new(false));
            let retirement_dropped = Arc::new(AtomicBool::new(false));
            let supplied_calls = Arc::new(AtomicU64::new(0));
            let resource_owner = ClosureOwner(Arc::clone(&resource_dropped));
            let resource_calls = Arc::clone(&supplied_calls);
            let supplied_resource: PersistentSessionCleanupObserver = Arc::new(move || {
                std::hint::black_box(&resource_owner);
                resource_calls.fetch_add(1, Ordering::AcqRel);
                Ok(())
            });
            let retirement_owner = ClosureOwner(Arc::clone(&retirement_dropped));
            let retirement_calls = Arc::clone(&supplied_calls);
            let supplied_retirement: PersistentSessionCleanupObserver = Arc::new(move || {
                std::hint::black_box(&retirement_owner);
                retirement_calls.fetch_add(1, Ordering::AcqRel);
                Ok(())
            });
            let original_calls = Arc::new(AtomicU64::new(0));
            let called = Arc::clone(&original_calls);
            let original_retirement: PersistentSessionCleanupObserver = Arc::new(move || {
                called.fetch_add(1, Ordering::AcqRel);
                Err(anyhow!(
                    "original refusing obligation remains authoritative"
                ))
            });
            let error = pool
                .execute_attributed_owned_start(
                    &key,
                    &owned_start_lifecycle(),
                    &test_wire(),
                    None,
                    serde_json::json!({}),
                    |original| {
                        original.retain_before_contact(vec![Box::new(Arc::clone(&guard))])?;
                        original.retain_cleanup_obligations(
                            None,
                            Some(Arc::clone(&original_retirement)),
                        )?;
                        if supplied_at_ready {
                            let mut started = fake_framed_session()?;
                            started.cleanup_observer = Some(Arc::clone(&supplied_resource));
                            started.retirement_observer = Some(Arc::clone(&supplied_retirement));
                            Ok(started)
                        } else {
                            let refusal = original.retain_cleanup_obligations(
                                Some(Arc::clone(&supplied_resource)),
                                Some(Arc::clone(&supplied_retirement)),
                            );
                            if stage == 2 {
                                assert!(refusal.is_err()); // Ignoring Err cannot bypass refusal.
                                fake_framed_session()
                            } else {
                                refusal?;
                                panic!("mismatched original obligation was accepted");
                            }
                        }
                    },
                    || false,
                    |_| Ok(()),
                )
                .unwrap_err();
            assert!(format!("{error:#}").contains("substituted an original cleanup obligation"));
            // Remove EVERY caller holder; only the original pool slot may retain
            // these actual RAII owners. Neither supplied closure may execute.
            drop(supplied_resource);
            drop(supplied_retirement);
            drop(guard);
            assert!(!resource_dropped.load(Ordering::Acquire));
            assert!(!retirement_dropped.load(Ordering::Acquire));
            assert_eq!(supplied_calls.load(Ordering::Acquire), 0);
            assert_eq!(original_calls.load(Ordering::Acquire), 0);
            let state = pool.inner.state.lock().unwrap();
            assert!(state.cleanup_unproved.is_some());
            let slot = &state.groups[&key].starts[0];
            assert!(!slot.is_in_flight());
            let retained = slot.retained.lock().unwrap();
            assert!(retained.resource_observer.is_none()); // Failed tuple made no partial admission.
            assert!(Arc::ptr_eq(
                retained.retirement_observer.as_ref().unwrap(),
                &original_retirement
            ));
            assert!(path.join("sentinel").is_file());
        }
    }

    fn wait_for_terminal(
        pool: &PersistentSessionPool,
        owner: &str,
        stream_id: &str,
    ) -> Vec<PersistentSessionStreamEvent> {
        let mut after = 0;
        let mut events = Vec::new();
        for _ in 0..20 {
            let page = pool
                .poll_stream(owner, stream_id, after, 250, STREAM_BACKLOG_EVENTS)
                .unwrap();
            if let Some(last) = page.events.last() {
                after = last.sequence;
            }
            events.extend(page.events);
            if page.terminal {
                return events;
            }
        }
        panic!("persistent-session stream did not terminate");
    }

    #[test]
    fn frame_round_trip_is_strict_and_bounded() {
        let wire = PersistentSessionWireContract {
            channel_env: "RYEOS_SESSION_FD".to_owned(),
            wire_protocol: "test.session".to_owned(),
            wire_version: 1,
            max_frame_bytes: 4096,
        };
        let expected = PersistentSessionFrame {
            protocol: wire.wire_protocol.clone(),
            version: wire.wire_version,
            kind: PersistentSessionFrameKind::Delta,
            request_id: Some("request-1".to_owned()),
            body: Some(serde_json::json!({"text": "hello"})),
        };
        let framed = encode_frame(&wire, &expected).unwrap();
        let declared_len = u32::from_be_bytes(framed[..4].try_into().unwrap()) as usize;
        assert_eq!(declared_len, framed.len() - 4);
        assert_eq!(
            decode_frame_body(&framed[4..], wire.max_frame_bytes).unwrap(),
            expected
        );
    }

    #[test]
    fn exclusive_session_is_bound_once_and_never_enters_a_pool() {
        let pool = PersistentSessionPool::new();
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 2_000;
        lifecycle.request_timeout_ms = 2_000;
        let wire = test_wire();
        let session_id = "e".repeat(64);
        let reservation = pool
            .reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap();
        assert!(
            pool.reserve_exclusive(&session_id, &lifecycle, &wire)
                .is_err()
        );
        reservation.bind(fake_framed_session().unwrap()).unwrap();
        let result = pool
            .execute_exclusive(
                &session_id,
                serde_json::json!({"message": "exclusive"}),
                || false,
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!({"echo": {"message": "exclusive"}})
        );
        assert!(
            pool.reserve_exclusive(&session_id, &lifecycle, &wire)
                .is_err()
        );
        pool.retire_exclusive(&session_id).unwrap();
        pool.reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap();
    }

    #[test]
    fn zero_resource_exclusive_request_accepts_command_identity_without_attribution() {
        let pool = PersistentSessionPool::new();
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 2_000;
        lifecycle.request_timeout_ms = 2_000;
        let wire = test_wire();
        let session_id = "z".repeat(64);
        pool.reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap()
            .bind(fake_framed_session().unwrap())
            .unwrap();
        let identity = PersistentSessionRequestIdentity {
            thread_id: "T-zero-resource".to_owned(),
            request_digest: ryeos_accounting::HexDigest::new("a".repeat(64)).unwrap(),
        };

        let result = pool
            .execute_exclusive_attributed_with_deadline(
                &session_id,
                Some(&identity),
                serde_json::json!({"message":"zero-resource"}),
                || false,
                |_| Ok(None),
                None,
            )
            .unwrap();

        assert_eq!(result["echo"]["message"], "zero-resource");
        assert!(
            pool.take_exclusive_failure_cleanup_state(&session_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            pool.retire_exclusive(&session_id).unwrap(),
            ExclusiveRetirementOutcome::Reaped
        );
    }

    #[test]
    fn exclusive_capture_readiness_refuses_pending_and_preserves_cleanup_proof() {
        let pool = PersistentSessionPool::new();
        let id = "c".repeat(64);
        assert!(pool.exclusive_capture_boot_identity(&id).unwrap().is_none());
        let reservation = pool
            .reserve_exclusive(&id, &test_lifecycle(), &test_wire())
            .unwrap();
        assert!(pool.exclusive_capture_boot_identity(&id).is_err());
        drop(reservation);
        {
            let mut state = pool.inner.state.lock().unwrap();
            state.exclusive_failure_cleanup.insert(id.clone(), "reaped");
        }
        assert!(pool.exclusive_capture_boot_identity(&id).is_err());
        assert_eq!(
            pool.take_exclusive_failure_cleanup_state(&id).unwrap(),
            Some("reaped")
        );
        pool.inner.state.lock().unwrap().cleanup_unproved =
            Some("fixture unknown process".to_owned());
        assert!(pool.exclusive_capture_boot_identity(&id).is_err());
    }

    #[test]
    fn pre_io_exclusive_failure_is_retained_as_unproved() {
        let pool = PersistentSessionPool::new();
        let id = "u".repeat(64);
        pool.reserve_exclusive(&id, &test_lifecycle(), &test_wire())
            .unwrap()
            .bind(fake_framed_session().unwrap())
            .unwrap();
        let process = {
            let state = pool.inner.state.lock().unwrap();
            Arc::clone(&state.exclusive.get(&id).unwrap().process)
        };
        process.leased.store(true, Ordering::Release);
        let error = pool
            .execute_exclusive(&id, serde_json::json!({}), || false, |_| Ok(()))
            .unwrap_err();
        assert!(error.to_string().contains("already has an active request"));
        assert_eq!(
            pool.take_exclusive_failure_cleanup_state_or_unproved(&id)
                .unwrap(),
            "unproved"
        );

        process.leased.store(false, Ordering::Release);
        assert_eq!(
            pool.retire_exclusive(&id).unwrap(),
            ExclusiveRetirementOutcome::Reaped
        );

        pool.inner
            .state
            .lock()
            .unwrap()
            .exclusive_failure_cleanup
            .insert(id.clone(), "reaped");
        assert_eq!(
            pool.take_exclusive_failure_cleanup_state_or_unproved(&id)
                .unwrap(),
            "reaped"
        );
        assert!(
            pool.take_exclusive_failure_cleanup_state(&id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn exclusive_retirement_releases_lifelines_with_an_extra_session_owner() {
        struct RetainedLifetime(Arc<AtomicBool>);
        impl Drop for RetainedLifetime {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let pool = PersistentSessionPool::new();
        let id = "e".repeat(64);
        pool.reserve_exclusive(&id, &test_lifecycle(), &test_wire())
            .unwrap()
            .bind(fake_framed_session().unwrap())
            .unwrap();
        let retained = {
            let state = pool.inner.state.lock().unwrap();
            Arc::clone(&state.exclusive.get(&id).unwrap().process)
        };
        let released = Arc::new(AtomicBool::new(false));
        retained
            .lifelines
            .lock()
            .unwrap()
            .push(Box::new(RetainedLifetime(Arc::clone(&released))));
        assert_eq!(
            pool.retire_exclusive(&id).unwrap(),
            ExclusiveRetirementOutcome::Reaped
        );
        assert!(released.load(Ordering::Acquire));
        assert!(retained.lifelines.lock().unwrap().is_empty());
        retained.retire().unwrap();
    }

    #[test]
    fn retirement_settlement_is_independent_of_financial_resource_authority() {
        let pool = PersistentSessionPool::new();
        let id = "retirement-only".to_owned();
        let settled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&settled);
        let mut started = fake_framed_session().unwrap();
        started.retirement_observer = Some(Arc::new(move || {
            observed.store(true, Ordering::Release);
            Ok(())
        }));
        pool.reserve_exclusive(&id, &test_lifecycle(), &test_wire())
            .unwrap()
            .bind(started)
            .unwrap();
        assert!(!settled.load(Ordering::Acquire));
        assert_eq!(
            pool.retire_exclusive(&id).unwrap(),
            ExclusiveRetirementOutcome::Reaped
        );
        assert!(settled.load(Ordering::Acquire));
    }

    #[test]
    fn retirement_settlement_failure_prevents_success_and_retry_bypass() {
        let pool = PersistentSessionPool::new();
        let id = "retirement-failure";
        let mut started = fake_framed_session().unwrap();
        started.retirement_observer = Some(Arc::new(|| {
            anyhow::bail!("launch reservation test refusal")
        }));
        pool.reserve_exclusive(id, &test_lifecycle(), &test_wire())
            .unwrap()
            .bind(started)
            .unwrap();
        for _ in 0..2 {
            assert_eq!(
                pool.retire_exclusive(id).unwrap(),
                ExclusiveRetirementOutcome::Unproved
            );
        }
    }

    // Exercise the same observer composition as the Q caller through actual
    // Started readiness and exclusive retirement. These are session lifecycle
    // controls, not Q journal, callback or native-settlement testimony.
    fn assert_nonfinancial_retirement(process_fails: bool, connector_fails: bool) {
        struct RetainedLifetime(Arc<AtomicBool>);
        impl Drop for RetainedLifetime {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let process_calls = Arc::clone(&calls);
        let connector_calls = Arc::clone(&calls);
        let observer = combine_retirement_observers(
            Some(Arc::new(move || {
                process_calls.lock().unwrap().push("process");
                anyhow::ensure!(!process_fails, "original process settlement refused");
                Ok(())
            })),
            Some(Arc::new(move || {
                connector_calls.lock().unwrap().push("connector");
                anyhow::ensure!(!connector_fails, "original connector settlement refused");
                Ok(())
            })),
        )
        .unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let mut started = fake_framed_session().unwrap();
        started
            .lifelines
            .push(Box::new(RetainedLifetime(Arc::clone(&released))));
        let lifeline_count = started.lifelines.len();
        started.retirement_observer = Some(Arc::clone(&observer));
        assert!(started.cleanup_observer.is_none());
        assert!(started.resource_occupancy_limit.is_none());
        assert!(started.resource_cleanup_allowance_ms.is_none());
        assert!(started.resource_attribution_sink.is_none());
        assert!(started.resource_eligibility.is_none());

        let pool = PersistentSessionPool::new();
        let id = "nonfinancial-retirement";
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 2_000;
        pool.reserve_exclusive(id, &lifecycle, &test_wire())
            .unwrap()
            .bind(started)
            .unwrap();
        let retained = {
            let state = pool.inner.state.lock().unwrap();
            Arc::clone(&state.exclusive.get(id).unwrap().process)
        };
        assert!(Arc::ptr_eq(
            retained.cleanup_observer.lock().unwrap().as_ref().unwrap(),
            &observer,
        ));
        assert!(calls.lock().unwrap().is_empty());
        assert!(!released.load(Ordering::Acquire));

        if process_fails || connector_fails {
            for _ in 0..2 {
                assert_eq!(
                    pool.retire_exclusive(id).unwrap(),
                    ExclusiveRetirementOutcome::Unproved
                );
                assert_eq!(*calls.lock().unwrap(), ["process", "connector"]);
                assert_eq!(retained.lifelines.lock().unwrap().len(), lifeline_count);
                assert!(!released.load(Ordering::Acquire));
                let reason = retained.cleanup_unproved.lock().unwrap();
                let reason = reason.as_ref().unwrap();
                assert_eq!(
                    reason.contains("original process settlement refused"),
                    process_fails
                );
                assert_eq!(
                    reason.contains("original connector settlement refused"),
                    connector_fails
                );
            }
        } else {
            assert_eq!(
                pool.retire_exclusive(id).unwrap(),
                ExclusiveRetirementOutcome::Reaped
            );
            assert!(released.load(Ordering::Acquire));
            assert!(retained.lifelines.lock().unwrap().is_empty());
            retained.retire().unwrap();
            assert_eq!(*calls.lock().unwrap(), ["process", "connector"]);
        }
    }

    #[test]
    fn nonfinancial_retirement_composite_accepts_started_and_releases_lifelines() {
        assert_nonfinancial_retirement(false, false);
    }

    #[test]
    fn nonfinancial_retirement_process_refusal_keeps_lifelines() {
        assert_nonfinancial_retirement(true, false);
    }

    #[test]
    fn nonfinancial_retirement_connector_refusal_keeps_lifelines() {
        assert_nonfinancial_retirement(false, true);
    }

    #[test]
    fn nonfinancial_retirement_both_refusals_keep_errors_and_lifelines() {
        assert_nonfinancial_retirement(true, true);
    }

    #[test]
    fn financial_cleanup_without_resource_tuple_still_refuses_started() {
        let calls = Arc::new(Mutex::new(0));
        let observed = Arc::clone(&calls);
        let mut started = fake_framed_session().unwrap();
        started.cleanup_observer = Some(Arc::new(move || {
            *observed.lock().unwrap() += 1;
            Ok(())
        }));
        let pool = PersistentSessionPool::new();
        let id = "missing-resource-tuple";
        let result = pool
            .reserve_exclusive(id, &test_lifecycle(), &test_wire())
            .unwrap()
            .bind(started);
        let error = match result {
            Ok(_) => panic!("resource cleanup without occupancy authority became ready"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}")
                .contains("resource-bearing persistent session has no bounded occupancy authority")
        );
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(!pool.inner.state.lock().unwrap().exclusive.contains_key(id));
    }

    #[test]
    fn retirement_settlement_attempts_both_obligations_and_preserves_failure() {
        let retired = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&retired);
        let observer = combine_retirement_observers(
            Some(Arc::new(|| anyhow::bail!("resource test failure"))),
            Some(Arc::new(move || {
                observed.store(true, Ordering::Release);
                anyhow::bail!("retirement test failure")
            })),
        )
        .unwrap();
        let error = observer().unwrap_err();
        assert!(retired.load(Ordering::Acquire));
        let message = format!("{error:#}");
        assert!(message.contains("resource test failure"));
        assert!(message.contains("retirement test failure"));
    }

    #[test]
    fn exclusive_retirement_cannot_retry_past_failed_resource_settlement() {
        let pool = PersistentSessionPool::new();
        let id = "f".repeat(64);
        pool.reserve_exclusive(&id, &test_lifecycle(), &test_wire())
            .unwrap()
            .bind(fake_framed_session().unwrap())
            .unwrap();
        let retained = {
            let state = pool.inner.state.lock().unwrap();
            Arc::clone(&state.exclusive.get(&id).unwrap().process)
        };
        *retained.cleanup_observer.lock().unwrap() = Some(Arc::new(|| {
            anyhow::bail!("test resource settlement refused")
        }));
        retained.lifelines.lock().unwrap().push(Box::new(()));
        let retained_count = retained.lifelines.lock().unwrap().len();
        for _ in 0..2 {
            assert_eq!(
                pool.retire_exclusive(&id).unwrap(),
                ExclusiveRetirementOutcome::Unproved
            );
            assert_eq!(retained.lifelines.lock().unwrap().len(), retained_count);
        }
    }

    #[test]
    fn typed_control_expiry_final_keeps_the_exclusive_session_usable() {
        let pool = PersistentSessionPool::new();
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 2_000;
        lifecycle.request_timeout_ms = 2_000;
        let wire = test_wire();
        let session_id = "x".repeat(64);
        pool.reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap()
            .bind(fake_framed_session().unwrap())
            .unwrap();
        let expired = pool
            .execute_exclusive_control(
                &session_id,
                serde_json::json!({
                    "force_expired":true,
                    "request_id":"approval-one",
                    "request_digest":"a".repeat(64),
                }),
            )
            .unwrap();
        assert_eq!(expired["outcome"], "expired");
        assert_eq!(expired["request_digest"], "a".repeat(64));

        let next = pool
            .execute_exclusive(
                &session_id,
                serde_json::json!({"message":"still-live"}),
                || false,
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(next["echo"]["message"], "still-live");
        assert!(
            pool.take_exclusive_failure_cleanup_state(&session_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn exclusive_retirement_never_conflates_reserved_absent_and_reaped() {
        let pool = PersistentSessionPool::new();
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 2_000;
        let wire = test_wire();
        let session_id = "r".repeat(64);
        let reservation = pool
            .reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap();
        assert_eq!(
            pool.retire_exclusive(&session_id).unwrap(),
            ExclusiveRetirementOutcome::Reserved
        );
        drop(reservation);
        assert_eq!(
            pool.retire_exclusive(&session_id).unwrap(),
            ExclusiveRetirementOutcome::Absent
        );
        pool.reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap()
            .bind(fake_framed_session().unwrap())
            .unwrap();
        assert_eq!(
            pool.retire_exclusive(&session_id).unwrap(),
            ExclusiveRetirementOutcome::Reaped
        );
    }

    #[test]
    fn shutdown_reaps_pooled_and_exclusive_processes_and_closes_admission() {
        let pool = PersistentSessionPool::new();
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 2_000;
        lifecycle.request_timeout_ms = 2_000;
        lifecycle.idle_timeout_ms = 60_000;
        let wire = test_wire();

        let pooled = pool
            .execute(
                &"b".repeat(64),
                &lifecycle,
                &wire,
                serde_json::json!({"message":"pooled"}),
                fake_framed_session,
                || false,
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(pooled["echo"]["message"], "pooled");

        let session_id = "s".repeat(64);
        pool.reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap()
            .bind(fake_framed_session().unwrap())
            .unwrap();

        assert_eq!(
            pool.shutdown_and_reap_all(Duration::from_secs(2)).unwrap(),
            2
        );
        assert_eq!(
            pool.shutdown_and_reap_all(Duration::from_secs(2)).unwrap(),
            0
        );
        assert_eq!(
            pool.retire_exclusive(&session_id).unwrap(),
            ExclusiveRetirementOutcome::Absent
        );
        let error = pool
            .execute(
                &"a".repeat(64),
                &lifecycle,
                &wire,
                serde_json::json!({}),
                fake_framed_session,
                || false,
                |_| Ok(()),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("admission is closed for daemon shutdown")
        );
        assert!(
            pool.reserve_exclusive(&"n".repeat(64), &lifecycle, &wire)
                .err()
                .expect("shutdown must reject exclusive admission")
                .to_string()
                .contains("admission is closed for daemon shutdown")
        );
    }

    #[test]
    fn shutdown_waits_for_and_reaps_a_late_exclusive_bind() {
        let pool = PersistentSessionPool::new();
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 2_000;
        let wire = test_wire();
        let session_id = "l".repeat(64);
        let reservation = pool
            .reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap();

        let shutdown_pool = pool.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            done_tx
                .send(shutdown_pool.shutdown_and_reap_all(Duration::from_secs(2)))
                .unwrap();
        });
        while !pool.inner.shutdown.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let error = reservation
            .bind(fake_framed_session().unwrap())
            .err()
            .expect("late exclusive bind must be rejected");
        assert!(
            error
                .to_string()
                .contains("admission is closed for daemon shutdown")
        );
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("shutdown must finish after the admitted start settles")
                .unwrap(),
            0
        );
        shutdown.join().unwrap();
        assert_eq!(
            pool.shutdown_and_reap_all(Duration::from_secs(2)).unwrap(),
            0
        );
    }

    #[test]
    fn exclusive_session_demultiplexes_observations_and_acks_after_sink_success() {
        let pool = PersistentSessionPool::new();
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 2_000;
        lifecycle.request_timeout_ms = 2_000;
        let wire = test_wire();
        let session_id = "o".repeat(64);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let observation_sink: PersistentSessionObservationSink = Arc::new(move |body| {
            captured
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(body);
            Ok(serde_json::json!({
                "through_sequence": 1,
                "batch_digest": "fixture-digest"
            }))
        });
        pool.reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap()
            .bind(fake_framed_session_with_observation_sink(Some(observation_sink)).unwrap())
            .unwrap();
        let result = pool
            .execute_exclusive(
                &session_id,
                serde_json::json!({"emit_observation": true}),
                || false,
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!({"echo": {"emit_observation": true}})
        );
        let observed = observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0]["batch_digest"], "fixture-digest");
    }

    #[test]
    fn exclusive_failure_retains_cleanup_proof_until_durable_owner_consumes_it() {
        let pool = PersistentSessionPool::new();
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 2_000;
        lifecycle.request_timeout_ms = 2_000;
        let wire = test_wire();
        let session_id = "f".repeat(64);
        pool.reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap()
            .bind(fake_framed_session().unwrap())
            .unwrap();
        assert!(
            pool.execute_exclusive(&session_id, serde_json::json!({}), || true, |_| Ok(()))
                .is_err()
        );
        assert!(
            pool.reserve_exclusive(&session_id, &lifecycle, &wire)
                .is_err()
        );
        assert_eq!(
            pool.take_exclusive_failure_cleanup_state(&session_id)
                .unwrap(),
            Some("reaped")
        );
        pool.reserve_exclusive(&session_id, &lifecycle, &wire)
            .unwrap();
    }

    #[test]
    fn cancel_frame_matches_the_worker_wire_fixture() {
        let frame = PersistentSessionFrame {
            protocol: "ryeos.persistent-session".to_owned(),
            version: 1,
            kind: PersistentSessionFrameKind::Cancel,
            request_id: Some("cancel-fixture".to_owned()),
            body: None,
        };

        assert_eq!(
            serde_json::to_vec(&frame).unwrap(),
            br#"{"protocol":"ryeos.persistent-session","version":1,"kind":"cancel","request_id":"cancel-fixture","body":null}"#
        );
    }

    #[test]
    fn frame_decoder_refuses_missing_duplicate_and_kind_incoherent_fields() {
        let missing_body =
            br#"{"protocol":"test.session","version":1,"kind":"cancel","request_id":"x"}"#;
        assert!(decode_frame_body(missing_body, 4096).is_err());
        let duplicate = br#"{"protocol":"test.session","protocol":"test.session","version":1,"kind":"cancel","request_id":"x","body":null}"#;
        assert!(decode_frame_body(duplicate, 4096).is_err());
        let incoherent = br#"{"protocol":"test.session","version":1,"kind":"ready","request_id":"x","body":null}"#;
        assert!(decode_frame_body(incoherent, 4096).is_err());
    }

    #[test]
    fn readiness_identity_is_exact_and_cannot_be_replayed_without_the_boot_value() {
        let expected = "a".repeat(64);
        let valid = PersistentSessionFrame {
            protocol: "fixture".to_owned(),
            version: 1,
            kind: PersistentSessionFrameKind::Ready,
            request_id: None,
            body: Some(serde_json::json!({"boot_identity": expected})),
        };
        assert!(validate_frame_shape(&valid, Some(&"a".repeat(64))).is_ok());

        let missing = PersistentSessionFrame {
            body: None,
            ..valid.clone()
        };
        assert!(validate_frame_shape(&missing, Some(&"a".repeat(64))).is_err());

        let stale = PersistentSessionFrame {
            body: Some(serde_json::json!({"boot_identity": "stale"})),
            ..valid
        };
        assert!(validate_frame_shape(&stale, Some(&"a".repeat(64))).is_err());
    }

    #[test]
    fn decoder_defers_exclusive_readiness_identity_to_the_boot_authority() {
        let body = br#"{"protocol":"fixture","version":1,"kind":"ready","request_id":null,"body":{"boot_identity":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#;
        let frame = decode_frame_body(body, 4096).unwrap();
        assert!(validate_frame_shape(&frame, Some(&"a".repeat(64))).is_ok());
        assert!(validate_frame_shape(&frame, Some(&"b".repeat(64))).is_err());
    }

    #[test]
    fn observation_frames_are_bounded_before_entering_any_queue() {
        let admitted = PersistentSessionFrame {
            protocol: "fixture".to_owned(),
            version: 1,
            kind: PersistentSessionFrameKind::ObservationBatch,
            request_id: None,
            body: Some(serde_json::json!({"events":[],"session_observations":[]})),
        };
        assert!(validate_frame_shape(&admitted, None).is_ok());

        let excessive = PersistentSessionFrame {
            body: Some(serde_json::json!({
                "payload":"x".repeat(
                    ryeos_state::objects::MAX_STRUCTURED_OBSERVATION_BATCH_BYTES
                )
            })),
            ..admitted
        };
        assert!(validate_frame_shape(&excessive, None).is_err());
    }

    #[test]
    fn command_progress_ack_retains_its_request_coordinate() {
        let frame = PersistentSessionFrame {
            protocol: "test.session".to_owned(),
            version: 1,
            kind: PersistentSessionFrameKind::ObservationAck,
            request_id: Some("request-one".to_owned()),
            body: Some(serde_json::json!({"command_progress_digest":"a".repeat(64)})),
        };
        assert!(validate_frame_shape(&frame, None).is_ok());
        let mut empty = frame.clone();
        empty.request_id = Some(String::new());
        assert!(validate_frame_shape(&empty, None).is_err());
        let mut absent = frame;
        absent.body = None;
        assert!(validate_frame_shape(&absent, None).is_err());
    }

    #[test]
    fn command_progress_ack_is_sent_only_after_delta_acceptance() {
        let pool = PersistentSessionPool::new();
        let mut lifecycle = test_lifecycle();
        lifecycle.ready_timeout_ms = 5_000;
        lifecycle.request_timeout_ms = 5_000;
        let wire = test_wire();
        let session_id = "progress-ack-fixture";
        pool.reserve_exclusive(session_id, &lifecycle, &wire)
            .unwrap()
            .bind(fake_framed_session().unwrap())
            .unwrap();
        let mut accepted = false;
        let result = pool
            .execute_exclusive_with_deadline(
                session_id,
                serde_json::json!({"await_delta_ack":true}),
                || false,
                |delta| {
                    assert_eq!(delta, serde_json::json!({"text":"fixture"}));
                    accepted = true;
                    Ok(Some(serde_json::json!({"accepted":"fixture"})))
                },
                None,
            )
            .unwrap();
        assert!(accepted);
        assert_eq!(result, serde_json::json!({"echo":{"await_delta_ack":true}}));
    }

    #[test]
    fn incremental_reader_retains_a_partial_frame_across_timeout() {
        struct FragmentedReader {
            bytes: Vec<u8>,
            position: usize,
            pause_at: usize,
            paused: bool,
        }

        impl Read for FragmentedReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.position == self.pause_at && !self.paused {
                    self.paused = true;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "fixture timeout",
                    ));
                }
                if self.position == self.bytes.len() {
                    return Ok(0);
                }
                let boundary = if self.paused {
                    self.bytes.len()
                } else {
                    self.pause_at
                };
                let available = boundary.saturating_sub(self.position);
                let count = available.min(output.len());
                output[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
                self.position += count;
                Ok(count)
            }
        }

        let wire = test_wire();
        let expected = PersistentSessionFrame {
            protocol: wire.wire_protocol.clone(),
            version: wire.wire_version,
            kind: PersistentSessionFrameKind::Final,
            request_id: Some("fragmented".to_owned()),
            body: Some(serde_json::json!({"ok": true})),
        };
        let encoded = encode_frame(&wire, &expected).unwrap();
        let mut fragmented = FragmentedReader {
            bytes: encoded,
            position: 0,
            pause_at: 7,
            paused: false,
        };
        let mut reader = FrameReader::default();
        assert!(
            reader
                .read_next(&mut fragmented, wire.max_frame_bytes)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            reader
                .read_next(&mut fragmented, wire.max_frame_bytes)
                .unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn real_framed_process_streams_final_and_observes_cancellation() {
        let pool =
            PersistentSessionPool::with_limits(PersistentSessionPoolLimits::default()).unwrap();
        let lifecycle = PersistentSessionLifecycleContract {
            max_processes: 1,
            max_inflight_per_process: 1,
            max_address_space_bytes: 512 * 1024 * 1024,
            max_cpu_seconds: 10,
            real_uid_process_limit: 16,
            ready_timeout_ms: 2_000,
            request_timeout_ms: 2_000,
            idle_timeout_ms: 2_000,
        };
        let wire = test_wire();
        let mut deltas = Vec::new();
        let result = pool
            .execute(
                &"a".repeat(64),
                &lifecycle,
                &wire,
                serde_json::json!({"prompt":"hello"}),
                fake_framed_session,
                || false,
                |delta| {
                    deltas.push(delta);
                    Ok(())
                },
            )
            .unwrap_or_else(|error| panic!("fake persistent-session execution failed: {error:#}"));
        assert_eq!(deltas, [serde_json::json!({"text":"fixture"})]);
        assert_eq!(result, serde_json::json!({"echo":{"prompt":"hello"}}));

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_check = Arc::clone(&cancelled);
        let cancelled_after_delta = Arc::clone(&cancelled);
        let error = pool
            .execute(
                &"b".repeat(64),
                &lifecycle,
                &wire,
                serde_json::json!({"prompt":"cancel"}),
                fake_framed_session,
                move || cancelled_for_check.load(Ordering::Acquire),
                move |_delta| {
                    cancelled_after_delta.store(true, Ordering::Release);
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn cancelled_acquisition_never_spawns_or_contacts_a_worker() {
        let pool = PersistentSessionPool::with_limits(narrow_stream_limits()).unwrap();
        let spawn_count = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&spawn_count);
        let result = pool.acquire(
            &"a".repeat(64),
            &test_lifecycle(),
            &test_wire(),
            &mut move || {
                counted.fetch_add(1, Ordering::AcqRel);
                bail!("cancelled acquisition must not spawn")
            },
            &|| true,
            MonotonicDeadline::after(Duration::from_secs(1)),
        );
        let error = match result {
            Ok(_) => panic!("cancelled acquisition unexpectedly returned a worker"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("cancelled before worker contact")
        );
        assert_eq!(spawn_count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn unproved_cleanup_quarantines_capacity_before_replacement_spawn() {
        let pool = PersistentSessionPool::with_limits(narrow_stream_limits()).unwrap();
        pool.poison_after_unproved_cleanup("injected cleanup refusal".to_owned());
        let spawn_count = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&spawn_count);
        let result = pool.acquire(
            &"a".repeat(64),
            &test_lifecycle(),
            &test_wire(),
            &mut move || {
                counted.fetch_add(1, Ordering::AcqRel);
                bail!("quarantined pool must not spawn")
            },
            &|| false,
            MonotonicDeadline::after(Duration::from_secs(1)),
        );
        let error = match result {
            Ok(_) => panic!("quarantined pool returned replacement capacity"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("injected cleanup refusal"));
        assert_eq!(spawn_count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn background_stream_is_ordered_owned_and_terminal() {
        let pool = PersistentSessionPool::new();
        let stream_id = pool
            .start_stream("attempt-a", "thread-a", |_cancelled, publish| {
                publish(serde_json::json!({"text": "a"}))?;
                publish(serde_json::json!({"text": "b"}))?;
                Ok(serde_json::json!({"done": true}))
            })
            .unwrap();

        assert!(
            pool.poll_stream("attempt-b", &stream_id, 0, 0, 1)
                .unwrap_err()
                .to_string()
                .contains("owner mismatch")
        );
        let events = wait_for_terminal(&pool, "attempt-a", &stream_id);
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(events[0].kind, PersistentSessionStreamEventKind::Delta);
        assert_eq!(events[1].kind, PersistentSessionStreamEventKind::Delta);
        assert_eq!(events[2].kind, PersistentSessionStreamEventKind::Final);
        pool.close_stream("attempt-a", &stream_id).unwrap();
    }

    #[test]
    fn cancellation_reaches_the_owned_operation_and_records_one_error() {
        let pool = PersistentSessionPool::new();
        let stream_id = pool
            .start_stream("attempt-cancel", "thread-cancel", |cancelled, _publish| {
                while !cancelled.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                bail!("operation observed cancellation")
            })
            .unwrap();
        pool.cancel_stream("attempt-cancel", &stream_id).unwrap();
        let events = wait_for_terminal(&pool, "attempt-cancel", &stream_id);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, PersistentSessionStreamEventKind::Error);
        assert!(
            events[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("observed cancellation"))
        );
    }

    #[test]
    fn per_subject_active_stream_quota_fails_before_operation_contact() {
        let pool = PersistentSessionPool::with_limits(narrow_stream_limits()).unwrap();
        let first = pool
            .start_stream("attempt-first", "thread-one", |cancelled, _publish| {
                while !cancelled.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                bail!("first operation cancelled")
            })
            .unwrap();
        let contacted = Arc::new(AtomicBool::new(false));
        let contacted_by_operation = Arc::clone(&contacted);
        let error = pool
            .start_stream(
                "attempt-second",
                "thread-one",
                move |_cancelled, _publish| {
                    contacted_by_operation.store(true, Ordering::Release);
                    Ok(Value::Null)
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("stream quota"));
        assert!(!contacted.load(Ordering::Acquire));
        pool.cancel_stream("attempt-first", &first).unwrap();
        let _ = wait_for_terminal(&pool, "attempt-first", &first);
    }

    #[test]
    fn unused_capacity_reservation_is_invisible_and_fully_released() {
        let pool = PersistentSessionPool::with_limits(narrow_stream_limits()).unwrap();
        let reservation = pool
            .reserve_stream_capacity("attempt-reserved", "thread-one")
            .unwrap();
        assert_eq!(pool.existing_stream_id("attempt-reserved").unwrap(), None);
        assert!(
            pool.reserve_stream_capacity("attempt-other", "thread-one")
                .is_err()
        );
        drop(reservation);
        assert!(
            pool.reserve_stream_capacity("attempt-other", "thread-one")
                .is_ok()
        );
    }

    #[test]
    fn close_removes_the_stream_and_releases_all_backlog_bytes() {
        let pool = PersistentSessionPool::with_limits(narrow_stream_limits()).unwrap();
        let stream_id = pool
            .start_stream("attempt-close", "thread-close", |_cancelled, _publish| {
                Ok(serde_json::json!({"done": true}))
            })
            .unwrap();
        let _ = wait_for_terminal(&pool, "attempt-close", &stream_id);
        assert!(
            *pool
                .streams
                .backlog
                .bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                > 0
        );
        pool.close_stream("attempt-close", &stream_id).unwrap();
        assert_eq!(
            *pool
                .streams
                .backlog
                .bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            0
        );
        assert!(pool.existing_stream_id("attempt-close").unwrap().is_none());
    }

    #[test]
    fn explicitly_disabled_node_policy_refuses_contact() {
        let pool = PersistentSessionPool::disabled();
        let error = match pool.reserve_stream_capacity("attempt-disabled", "thread-disabled") {
            Ok(_) => panic!("disabled pool unexpectedly reserved stream capacity"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("disabled by node policy"));
    }

    #[test]
    fn read_only_contract_eligibility_observes_policy_without_reserving() {
        let pool = PersistentSessionPool::with_limits(narrow_stream_limits()).unwrap();
        let mut admissible_wire = test_wire();
        admissible_wire.max_frame_bytes = 1024;
        assert_eq!(
            pool.validate_contract_eligibility(&test_lifecycle(), &admissible_wire)
                .unwrap(),
            PersistentSessionContractEligibility::Ready
        );
        assert!(pool.inner.state.lock().unwrap().groups.is_empty());
        assert_eq!(
            pool.validate_contract_eligibility(&test_lifecycle(), &test_wire())
                .unwrap(),
            PersistentSessionContractEligibility::WireFrameExceedsPolicy
        );

        let mut excessive = test_lifecycle();
        excessive.real_uid_process_limit = 2;
        assert_eq!(
            pool.validate_contract_eligibility(&excessive, &admissible_wire)
                .unwrap(),
            PersistentSessionContractEligibility::ResourceRequestExceedsPolicy
        );
        assert!(pool.inner.state.lock().unwrap().groups.is_empty());

        let disabled = PersistentSessionPool::disabled();
        assert_eq!(
            disabled
                .validate_contract_eligibility(&test_lifecycle(), &admissible_wire)
                .unwrap(),
            PersistentSessionContractEligibility::Disabled
        );
    }

    #[test]
    fn oversized_delta_is_rejected_within_the_byte_budget() {
        let pool = PersistentSessionPool::with_limits(narrow_stream_limits()).unwrap();
        let stream_id = pool
            .start_stream("attempt-large", "thread-large", |_cancelled, publish| {
                publish(Value::String("x".repeat(2048)))?;
                Ok(Value::Null)
            })
            .unwrap();
        let events = wait_for_terminal(&pool, "attempt-large", &stream_id);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, PersistentSessionStreamEventKind::Error);
        assert!(
            events[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("byte budget"))
        );
        assert!(
            *pool
                .streams
                .backlog
                .bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                <= narrow_stream_limits().max_total_backlog_bytes
        );
    }
}
