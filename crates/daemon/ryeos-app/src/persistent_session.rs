//! Meaning-blind daemon ownership for callback-free persistent subprocesses.
//!
//! Kinds and domain adapters own frame-body meaning.  This module owns only
//! exact pool identity, bounded framing, serial request correlation,
//! cancellation, readiness, reuse, idle retirement, and process teardown.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_state::objects::{PersistentSessionLifecycleContract, PersistentSessionWireContract};

const IO_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STREAM_BACKLOG_EVENTS: usize = 128;
const TERMINAL_STREAM_RETENTION_MS: u64 = 5 * 60 * 1_000;
const ABANDONED_STREAM_RETENTION_MS: u64 = 2 * 60 * 60 * 1_000;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static STREAM_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Node-wide admission budgets for trusted, bundle-signed persistent workers.
/// Signed kind contracts can narrow them but cannot raise them. The per-tree
/// RLIMITs applied after admission are not aggregate hostile-workload
/// enforcement; that stronger boundary requires a cgroup-backed backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionPoolLimits {
    pub max_pool_groups: usize,
    pub max_total_processes: usize,
    pub max_total_address_space_bytes: u64,
    pub max_total_cpu_seconds: u64,
    /// Maximum per-worker RLIMIT_NPROC that a signed workload contract may
    /// request for the worker's real UID.
    pub max_real_uid_process_limit: u64,
    pub max_open_streams: usize,
    pub max_active_streams: usize,
    pub max_active_streams_per_subject: usize,
    pub max_stream_backlog_bytes: usize,
    pub max_total_backlog_bytes: usize,
}

impl Default for PersistentSessionPoolLimits {
    fn default() -> Self {
        Self {
            max_pool_groups: 128,
            max_total_processes: 8,
            max_total_address_space_bytes: 32 * 1024 * 1024 * 1024,
            max_total_cpu_seconds: 8 * 60 * 60,
            max_real_uid_process_limit: 4096,
            max_open_streams: 256,
            max_active_streams: 32,
            max_active_streams_per_subject: 4,
            max_stream_backlog_bytes: 16 * 1024 * 1024,
            max_total_backlog_bytes: 64 * 1024 * 1024,
        }
    }
}

impl PersistentSessionPoolLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_pool_groups == 0
            || self.max_pool_groups > 4096
            || self.max_total_processes == 0
            || self.max_total_processes > 256
            || self.max_total_address_space_bytes == 0
            || self.max_total_address_space_bytes > 4 * 1024 * 1024 * 1024 * 1024
            || self.max_total_cpu_seconds == 0
            || self.max_total_cpu_seconds > 365 * 24 * 60 * 60
            || self.max_real_uid_process_limit == 0
            || self.max_real_uid_process_limit > 4096
            || self.max_open_streams == 0
            || self.max_open_streams > 4096
            || self.max_active_streams == 0
            || self.max_active_streams > self.max_open_streams
            || self.max_active_streams_per_subject == 0
            || self.max_active_streams_per_subject > self.max_active_streams
            || self.max_stream_backlog_bytes == 0
            || self.max_stream_backlog_bytes > 64 * 1024 * 1024
            || self.max_total_backlog_bytes < self.max_stream_backlog_bytes
            || self.max_total_backlog_bytes > 1024 * 1024 * 1024
        {
            bail!("persistent-session node pool limits are incoherent");
        }
        Ok(())
    }
}

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

pub struct StartedPersistentSession {
    pub running: ryeos_engine::dispatch::RunningExecution,
    pub socket: UnixStream,
    /// Descriptor-backed workspace/content leases owned for exactly the
    /// process lifetime. Their concrete types remain outside pool semantics.
    pub lifelines: Vec<Box<dyn Send + Sync>>,
    /// When present, readiness must echo this exact daemon-minted identity in
    /// `{ "boot_identity": ... }`. Pooled request workers omit it.
    pub expected_boot_identity: Option<String>,
    /// Optional daemon-owned observation authority installed before the
    /// post-readiness reader is started. Workers never receive this closure;
    /// they can only submit bounded protocol frames for it to validate.
    pub observation_sink: Option<PersistentSessionObservationSink>,
}

pub type PersistentSessionObservationSink =
    Arc<dyn Fn(Value) -> Result<Value> + Send + Sync + 'static>;

struct BudgetedSessionFrame {
    frame: PersistentSessionFrame,
    _budget: BacklogBytePermit,
}

struct SessionProcess {
    wire: PersistentSessionWireContract,
    writer: Mutex<UnixStream>,
    reader: Mutex<Option<SessionChannel>>,
    pending: Mutex<HashMap<String, SyncSender<std::result::Result<BudgetedSessionFrame, String>>>>,
    observation_sender: Mutex<Option<SyncSender<BudgetedSessionFrame>>>,
    initial_observation_sink: Mutex<Option<PersistentSessionObservationSink>>,
    reader_failure: Mutex<Option<String>>,
    running: Mutex<Option<ryeos_engine::dispatch::RunningExecution>>,
    /// Once ownership was consumed by an abort attempt whose reap proof
    /// failed, absence of `running` must never be reinterpreted as proof.
    cleanup_unproved: Mutex<Option<String>>,
    leased: AtomicBool,
    last_used_ms: AtomicU64,
    closed: Arc<AtomicBool>,
    backlog: Arc<BacklogBudget>,
    /// One maximum wire body is reserved for the reader's raw decode buffer.
    /// Decoded frames acquire additional exact serialized-byte permits before
    /// they can leave the reader thread.
    _reader_budget: BacklogBytePermit,
    _lifelines: Vec<Box<dyn Send + Sync>>,
}

const MAX_PENDING_SESSION_REQUESTS: usize = 32;

struct SessionChannel {
    socket: UnixStream,
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
        deadline: Instant,
    ) -> Result<()> {
        if let Some(reason) = self
            .reader_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_deref()
        {
            bail!("persistent-session reader failed: {reason}");
        }
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_frame(&mut writer, wire, frame, deadline)
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
            Instant::now() + Duration::from_secs(30),
        )
    }

    fn failure_diagnostic(&self) -> Option<String> {
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let owned = running.take()?;
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
        self.closed.store(true, Ordering::Release);
        self.backlog.changed.notify_all();
        self.observation_sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(reason) = self
            .cleanup_unproved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_deref()
        {
            bail!("persistent-session cleanup remains unproved: {reason}");
        }
        if let Some(running) = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            if let Err(error) = running.abort_and_reap_checked() {
                let reason = format!("persistent-session cleanup could not be proved: {error}");
                *self
                    .cleanup_unproved
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason.clone());
                bail!("{reason}");
            }
        }
        Ok(())
    }
}

impl Drop for SessionProcess {
    fn drop(&mut self) {
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
                std::thread::sleep(IO_POLL_INTERVAL);
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
    spawning: usize,
}

struct PoolState {
    groups: HashMap<String, SessionGroup>,
    exclusive: HashMap<String, ExclusiveSessionEntry>,
    exclusive_reservations: HashMap<String, GroupContract>,
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

struct PoolInner {
    state: Mutex<PoolState>,
    changed: Condvar,
    limits: PersistentSessionPoolLimits,
    enabled: bool,
    shutdown: Arc<AtomicBool>,
}

struct ReadyProcessFailure {
    error: anyhow::Error,
    cleanup_unproved: bool,
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
            .insert(session_id.to_owned(), contract.clone());
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
        let deadline =
            Instant::now() + Duration::from_millis(contract.lifecycle.request_timeout_ms);
        let result = execute_on_process(
            &process,
            &contract.wire,
            PersistentSessionFrameKind::Request,
            request_body,
            &cancelled,
            deadline,
            &mut on_delta,
        )
        .map_err(|error| attach_process_diagnostic(&process, error));
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
        let deadline =
            Instant::now() + Duration::from_millis(contract.lifecycle.request_timeout_ms);
        let result = execute_on_process(
            &process,
            &contract.wire,
            PersistentSessionFrameKind::Control,
            control_body,
            &|| false,
            deadline,
            &mut |_| Ok(()),
        )
        .map_err(|error| attach_process_diagnostic(&process, error));
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
        mut spawn: F,
        cancelled: C,
        mut on_delta: D,
    ) -> Result<Value>
    where
        F: FnMut() -> Result<StartedPersistentSession>,
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
        let deadline = Instant::now() + Duration::from_millis(lifecycle.request_timeout_ms);
        let process = self.acquire(pool_key, lifecycle, wire, &mut spawn, &cancelled, deadline)?;
        let result = execute_on_process(
            &process,
            wire,
            PersistentSessionFrameKind::Request,
            request_body,
            &cancelled,
            deadline,
            &mut on_delta,
        );
        let result = result.map_err(|error| attach_process_diagnostic(&process, error));
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

    fn acquire<F, C>(
        &self,
        key: &str,
        lifecycle: &PersistentSessionLifecycleContract,
        wire: &PersistentSessionWireContract,
        spawn: &mut F,
        cancelled: &C,
        deadline: Instant,
    ) -> Result<Arc<SessionProcess>>
    where
        F: FnMut() -> Result<StartedPersistentSession>,
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
            if Instant::now() >= deadline {
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
                    spawning: 0,
                });
            if group.contract.lifecycle != expected.lifecycle
                || group.contract.wire != expected.wire
            {
                bail!("persistent-session pool key was reused with a different contract");
            }
            if let Some(process) = group.processes.iter().find(|process| {
                process
                    .leased
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            }) {
                if cancelled() || Instant::now() >= deadline {
                    process.leased.store(false, Ordering::Release);
                    self.inner.changed.notify_all();
                    bail!("persistent-session request ended before worker contact");
                }
                return Ok(Arc::clone(process));
            }
            if group.processes.len() + group.spawning < usize::from(lifecycle.max_processes)
                && total_processes < self.inner.limits.max_total_processes
                && total_address_space.saturating_add(lifecycle.max_address_space_bytes)
                    <= self.inner.limits.max_total_address_space_bytes
                && total_cpu_seconds.saturating_add(lifecycle.max_cpu_seconds)
                    <= self.inner.limits.max_total_cpu_seconds
            {
                if cancelled() || Instant::now() >= deadline {
                    bail!("persistent-session request ended before worker spawn");
                }
                group.spawning += 1;
                drop(state);
                let started = match spawn() {
                    Ok(started) => {
                        ready_process(started, wire, lifecycle, Arc::clone(&self.streams.backlog))
                    }
                    Err(error) => Err(ReadyProcessFailure {
                        error,
                        cleanup_unproved: false,
                    }),
                };
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if self.inner.shutdown.load(Ordering::Acquire) {
                    let group = state.groups.get_mut(key).ok_or_else(|| {
                        anyhow!("persistent-session group disappeared during shutdown")
                    })?;
                    group.spawning = group.spawning.saturating_sub(1);
                    if group.processes.is_empty() && group.spawning == 0 {
                        state.groups.remove(key);
                    }
                    drop(state);
                    let cleanup_error = match started {
                        Ok(process) => process.retire().err(),
                        Err(failure) if failure.cleanup_unproved => Some(failure.error),
                        Err(_) => None,
                    };
                    if let Some(error) = cleanup_error {
                        self.poison_after_unproved_cleanup(error.to_string());
                        self.inner.changed.notify_all();
                        return Err(anyhow!(
                            "persistent-session spawn crossed daemon shutdown and cleanup could not be proved: {error}"
                        ));
                    }
                    self.inner.changed.notify_all();
                    bail!("persistent-session admission is closed for daemon shutdown");
                }
                let failure = {
                    let group = state.groups.get_mut(key).ok_or_else(|| {
                        anyhow!("persistent-session group disappeared during spawn")
                    })?;
                    group.spawning = group.spawning.saturating_sub(1);
                    match started {
                        Ok(process) => {
                            let process = Arc::new(process);
                            if let Err(error) = process.start_reader(wire.clone()) {
                                let cleanup = process.retire().err();
                                return Err(match cleanup {
                                    Some(cleanup) => error.context(format!(
                                        "persistent-session reader start cleanup failed: {cleanup}"
                                    )),
                                    None => error,
                                });
                            }
                            process.leased.store(true, Ordering::Release);
                            group.processes.push(Arc::clone(&process));
                            self.inner.changed.notify_all();
                            return Ok(process);
                        }
                        Err(failure) => failure,
                    }
                };
                if failure.cleanup_unproved && state.cleanup_unproved.is_none() {
                    state.cleanup_unproved = Some(failure.error.to_string());
                }
                if state
                    .groups
                    .get(key)
                    .is_some_and(|group| group.processes.is_empty() && group.spawning == 0)
                {
                    state.groups.remove(key);
                }
                self.inner.changed.notify_all();
                return Err(failure.error);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
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
            if group.processes.is_empty() && group.spawning == 0 {
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
            .is_some_and(|group| group.processes.is_empty() && group.spawning == 0)
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

        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let (pooled, exclusive, prior_unproved) = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while state.groups.values().any(|group| group.spawning != 0)
                || state.exclusive_starts_in_flight != 0
            {
                let remaining = deadline.saturating_duration_since(Instant::now());
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
                    && (state.groups.values().any(|group| group.spawning != 0)
                        || state.exclusive_starts_in_flight != 0)
                {
                    bail!("persistent-session shutdown timed out waiting for admitted starts");
                }
            }
            if !state.exclusive_reservations.is_empty() {
                bail!("persistent-session shutdown found an ownerless exclusive reservation");
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
            .retain(|_, group| !group.processes.is_empty() || group.spawning != 0);
        if let Some(reason) = prior_unproved {
            cleanup_errors.push(reason);
        }
        if !cleanup_errors.is_empty() {
            let reason = cleanup_errors.join("; ");
            state.cleanup_unproved.get_or_insert_with(|| reason.clone());
            bail!("persistent-session shutdown cleanup remains unproved: {reason}");
        }
        if state
            .groups
            .values()
            .any(|group| !group.processes.is_empty())
            || !state.exclusive.is_empty()
        {
            bail!("persistent-session shutdown left an owned process in the pool registry");
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
            self.release_reservation();
            let StartedPersistentSession { running, .. } = started;
            return match running.abort_and_reap_checked() {
                Ok(()) => Err(anyhow!(
                    "persistent-session admission is closed for daemon shutdown"
                )),
                Err(cleanup) => {
                    let reason = format!(
                        "exclusive process crossed daemon shutdown and cleanup could not be proved: {cleanup}"
                    );
                    let mut state = self
                        .inner
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.cleanup_unproved.get_or_insert_with(|| reason.clone());
                    Err(anyhow!(reason).context(PersistentSessionCleanupUnproved))
                }
            };
        }
        let ready = match ready_process(
            started,
            &self.contract.wire,
            &self.contract.lifecycle,
            Arc::clone(&self.backlog),
        ) {
            Ok(process) => {
                let process = Arc::new(process);
                if self.inner.shutdown.load(Ordering::Acquire) {
                    self.release_reservation();
                    let cleanup = process.retire().err();
                    return Err(match cleanup {
                        Some(cleanup) => {
                            let reason = format!(
                                "exclusive process crossed daemon shutdown and cleanup could not be proved: {cleanup}"
                            );
                            let mut state = self
                                .inner
                                .state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            state.cleanup_unproved.get_or_insert_with(|| reason.clone());
                            anyhow!(reason).context(PersistentSessionCleanupUnproved)
                        }
                        None => {
                            anyhow!("persistent-session admission is closed for daemon shutdown")
                        }
                    });
                }
                if let Err(error) = process.start_reader(self.contract.wire.clone()) {
                    self.release_reservation();
                    let cleanup = process.retire().err();
                    return Err(match cleanup {
                        Some(cleanup) => {
                            let reason =
                                format!("exclusive reader start cleanup failed: {cleanup}");
                            let mut state = self
                                .inner
                                .state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            state.cleanup_unproved.get_or_insert_with(|| reason.clone());
                            error
                                .context(reason)
                                .context(PersistentSessionCleanupUnproved)
                        }
                        None => error,
                    });
                }
                process
            }
            Err(failure) => {
                self.release_reservation();
                if failure.cleanup_unproved {
                    let mut state = self
                        .inner
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state
                        .cleanup_unproved
                        .get_or_insert_with(|| failure.error.to_string());
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
            state.exclusive_reservations.remove(&self.session_id);
            drop(state);
            let cleanup = ready.retire().err();
            return Err(match cleanup {
                Some(cleanup) => {
                    let reason = format!(
                        "exclusive process crossed daemon shutdown and cleanup could not be proved: {cleanup}"
                    );
                    let mut state = self
                        .inner
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.cleanup_unproved.get_or_insert_with(|| reason.clone());
                    anyhow!(reason).context(PersistentSessionCleanupUnproved)
                }
                None => anyhow!("persistent-session admission is closed for daemon shutdown"),
            });
        }
        let Some(reserved) = state.exclusive_reservations.remove(&self.session_id) else {
            drop(state);
            let cleanup = ready.retire().err();
            return Err(match cleanup {
                Some(cleanup) => {
                    let reason = format!(
                        "exclusive persistent-session reservation was lost; cleanup failed: {cleanup}"
                    );
                    let mut state = self
                        .inner
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.cleanup_unproved.get_or_insert_with(|| reason.clone());
                    anyhow!(reason).context(PersistentSessionCleanupUnproved)
                }
                None => anyhow!("exclusive persistent-session reservation was lost"),
            });
        };
        if reserved.lifecycle != self.contract.lifecycle || reserved.wire != self.contract.wire {
            drop(state);
            let cleanup = ready.retire().err();
            return Err(match cleanup {
                Some(cleanup) => {
                    let reason = format!(
                        "exclusive persistent-session reservation changed; cleanup failed: {cleanup}"
                    );
                    let mut state = self
                        .inner
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.cleanup_unproved.get_or_insert_with(|| reason.clone());
                    anyhow!(reason).context(PersistentSessionCleanupUnproved)
                }
                None => anyhow!("exclusive persistent-session reservation changed"),
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
                        let mut state = self
                            .inner
                            .state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state.cleanup_unproved.get_or_insert_with(|| reason.clone());
                        anyhow!(reason).context(PersistentSessionCleanupUnproved)
                    }
                    None => anyhow!("exclusive persistent session was concurrently attached"),
                });
            }
        }
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
        self.release_reservation();
    }
}

fn aggregate_process_capacity(state: &PoolState) -> (usize, u64, u64) {
    let pooled = state
        .groups
        .values()
        .fold((0usize, 0u64, 0u64), |totals, group| {
            let count = group.processes.len().saturating_add(group.spawning);
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
        .chain(state.exclusive_reservations.values())
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

fn ready_process(
    started: StartedPersistentSession,
    wire: &PersistentSessionWireContract,
    lifecycle: &PersistentSessionLifecycleContract,
    backlog: Arc<BacklogBudget>,
) -> std::result::Result<SessionProcess, ReadyProcessFailure> {
    let StartedPersistentSession {
        running,
        mut socket,
        lifelines,
        expected_boot_identity,
        observation_sink,
    } = started;
    let reader_budget_bytes = (wire.max_frame_bytes as usize).saturating_add(4);
    let reader_budget =
        match BacklogBytePermit::try_reserve(Arc::clone(&backlog), reader_budget_bytes) {
            Ok(budget) => budget,
            Err(error) => {
                let error = attach_running_diagnostic(&running, error);
                return match running.abort_and_reap_checked() {
                    Ok(()) => Err(ReadyProcessFailure {
                        error,
                        cleanup_unproved: false,
                    }),
                    Err(cleanup) => Err(ReadyProcessFailure {
                        error: error.context(format!(
                            "persistent-session IPC-budget cleanup could not be proved: {cleanup}"
                        )),
                        cleanup_unproved: true,
                    }),
                };
            }
        };
    let timeout = Duration::from_millis(lifecycle.ready_timeout_ms);
    socket
        .set_nonblocking(true)
        .context("configure persistent-session channel as nonblocking")
        .map_err(|error| ReadyProcessFailure {
            error,
            cleanup_unproved: false,
        })?;
    let deadline = Instant::now() + timeout;
    let mut reader = FrameReader::default();
    let frame = match loop {
        match reader.read_next(&mut socket, wire.max_frame_bytes) {
            Ok(Some(frame)) => break Ok(frame),
            Ok(None) if Instant::now() < deadline => {
                sleep_until_io_retry(deadline);
                continue;
            }
            Ok(None) => break Err(anyhow!("persistent-session readiness timed out")),
            Err(error) => break Err(error.context("read persistent-session readiness frame")),
        }
    } {
        Ok(frame) => frame,
        Err(error) => {
            return match running.wait_for_natural_exit(Duration::from_millis(200)) {
                Ok(completion) => Err(ReadyProcessFailure {
                    error: attach_completion_diagnostic(&completion, error),
                    cleanup_unproved: false,
                }),
                Err(running) => {
                    let error = attach_running_diagnostic(&running, error);
                    match running.abort_and_reap_checked() {
                        Ok(()) => Err(ReadyProcessFailure {
                            error,
                            cleanup_unproved: false,
                        }),
                        Err(cleanup) => Err(ReadyProcessFailure {
                            error: error.context(format!(
                                "persistent-session readiness cleanup could not be proved: {cleanup}"
                            )),
                            cleanup_unproved: true,
                        }),
                    }
                }
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
        return match running.abort_and_reap_checked() {
            Ok(()) => Err(ReadyProcessFailure {
                error,
                cleanup_unproved: false,
            }),
            Err(cleanup) => Err(ReadyProcessFailure {
                error: error.context(format!(
                    "persistent-session readiness cleanup could not be proved: {cleanup}"
                )),
                cleanup_unproved: true,
            }),
        };
    }
    let writer = match socket.try_clone() {
        Ok(writer) => writer,
        Err(error) => {
            let error = attach_running_diagnostic(
                &running,
                anyhow!(error).context("clone persistent-session writer descriptor"),
            );
            return match running.abort_and_reap_checked() {
                Ok(()) => Err(ReadyProcessFailure {
                    error,
                    cleanup_unproved: false,
                }),
                Err(cleanup) => Err(ReadyProcessFailure {
                    error: error.context(format!(
                        "persistent-session descriptor-clone cleanup could not be proved: {cleanup}"
                    )),
                    cleanup_unproved: true,
                }),
            };
        }
    };
    Ok(SessionProcess {
        wire: wire.clone(),
        writer: Mutex::new(writer),
        reader: Mutex::new(Some(SessionChannel { socket, reader })),
        pending: Mutex::new(HashMap::new()),
        observation_sender: Mutex::new(None),
        initial_observation_sink: Mutex::new(observation_sink),
        reader_failure: Mutex::new(None),
        running: Mutex::new(Some(running)),
        cleanup_unproved: Mutex::new(None),
        leased: AtomicBool::new(false),
        last_used_ms: AtomicU64::new(now_ms()),
        closed: Arc::new(AtomicBool::new(false)),
        backlog,
        _reader_budget: reader_budget,
        _lifelines: lifelines,
    })
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
    deadline: Instant,
    on_delta: &mut D,
) -> Result<Value>
where
    C: Fn() -> bool,
    D: FnMut(Value) -> Result<()>,
{
    let request_id = format!(
        "{}-{}",
        now_ms(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let receiver = process.register_request(&request_id)?;
    let outcome = (|| {
        if cancelled() || Instant::now() >= deadline {
            bail!("persistent-session request ended before worker contact");
        }
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
            if Instant::now() >= deadline {
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
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(IO_POLL_INTERVAL);
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
            match frame.kind {
                PersistentSessionFrameKind::Delta => {
                    on_delta(frame.body.expect("delta body validated"))?;
                }
                PersistentSessionFrameKind::Final => {
                    if cancel_sent {
                        bail!("persistent-session request was cancelled");
                    }
                    return Ok(frame.body.expect("final body validated"));
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

fn require_frame_identity(
    frame: &PersistentSessionFrame,
    wire: &PersistentSessionWireContract,
) -> Result<()> {
    if frame.protocol != wire.wire_protocol || frame.version != wire.wire_version {
        bail!("persistent-session frame protocol identity mismatch");
    }
    Ok(())
}

fn write_frame(
    stream: &mut UnixStream,
    wire: &PersistentSessionWireContract,
    frame: &PersistentSessionFrame,
    deadline: Instant,
) -> Result<()> {
    let encoded = encode_frame(wire, frame)?;
    let mut written = 0;
    while written < encoded.len() {
        // Use the descriptor operation directly. This protocol is admitted as
        // an inherited byte-stream FD; it does not require socket-specific
        // send authority, which may be deliberately absent in a sandbox.
        // RyeOS binaries retain Rust's default ignored-SIGPIPE disposition, so
        // a closed peer remains an ordinary EPIPE error.
        let sent = unsafe {
            libc::write(
                stream.as_raw_fd(),
                encoded[written..].as_ptr().cast(),
                encoded.len() - written,
            )
        };
        let outcome = if sent < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(sent as usize)
        };
        match outcome {
            Ok(0) => bail!("persistent-session channel closed while writing a frame"),
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if is_io_timeout(&error) && Instant::now() < deadline => {
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

fn sleep_until_io_retry(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if !remaining.is_zero() {
        std::thread::sleep(IO_POLL_INTERVAL.min(remaining));
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
        PersistentSessionFrameKind::ObservationBatch
        | PersistentSessionFrameKind::ObservationAck => {
            frame.request_id.is_none() && frame.body.is_some()
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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
                std::thread::sleep(Duration::from_secs(60));
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
                std::thread::sleep(Duration::from_secs(1));
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                let now = now_ms();
                let mut retired = Vec::new();
                let state = inner
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for (key, group) in &state.groups {
                    let idle = group.contract.lifecycle.idle_timeout_ms;
                    for process in &group.processes {
                        let expired = !process.leased.load(Ordering::Acquire)
                            && now.saturating_sub(process.last_used_ms.load(Ordering::Acquire))
                                >= idle;
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
                                if group.processes.is_empty() && group.spawning == 0 {
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
        use std::os::fd::{AsRawFd as _, OwnedFd};

        use ryeos_engine::contracts::{
            EffectivePrincipal, EngineContext, ExecutionDecorations, ExecutionPlan, LaunchMode,
            PlanCapabilities, PlanNode, PlanNodeId, PlanSubprocessSpec, Principal, ProjectContext,
        };

        let app_root = tempfile::tempdir()?;
        let node_dir = app_root.path().join(".ai/node");
        std::fs::create_dir_all(&node_dir)?;
        std::fs::write(
            node_dir.join("isolation.yaml"),
            "version: 1\nmode: disabled\nbackend: null\nfilesystem:\n  readable: []\n  writable: [\"{project}\"]\nnetwork:\n  mode: isolated\nenvironment:\n  allow: [\"*\"]\nlimits:\n  open_files: 128\n  stdout_bytes: 1048576\n  stderr_bytes: 1048576\n  verified_artifact_file_bytes: 67108864\n  verified_artifact_total_bytes: 268435456\n  verified_artifact_files: 4096\n",
        )?;
        let isolation = Arc::new(ryeos_engine::isolation::IsolationRuntime::load(
            app_root.path(),
        )?);
        let (daemon_socket, worker_socket) = UnixStream::pair()?;
        let worker_file = Arc::new(std::fs::File::from(OwnedFd::from(worker_socket)));
        let worker_fd = worker_file.as_raw_fd();
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
            env: HashMap::from([("RYEOS_SESSION_FD".to_owned(), worker_fd.to_string())]),
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
            isolation_target_channel: None,
            isolation_workspace: None,
            subprocess_limits: None,
            inherited_fds: vec![Arc::clone(&worker_file)],
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
            lifelines: vec![Box::new(app_root), Box::new(worker_file)],
            expected_boot_identity: None,
            observation_sink,
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
            Instant::now() + Duration::from_secs(1),
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
            Instant::now() + Duration::from_secs(1),
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
