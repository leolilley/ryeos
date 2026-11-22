//! A deterministic platform for tests: a virtual clock, a task queue polled by
//! the test, and a scripted transport. Nothing here sleeps in real time;
//! virtual time advances only when every task is waiting on a timer.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::history::{
    HistoryClaim, HistorySink, LiveHistory, SeatCoordinate, read_history,
};
use ryeos_client_runtime::platform::{
    Auth, ByteSource, Clock, Elapsed, EstablishMode, EstablishedSession, MemoryStore, Platform,
    ReplacementMode, Spawner, TaskHandle, Transport,
};

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// Virtual milliseconds. `sleep` completes when the sim advances past its
/// deadline.
#[derive(Default)]
pub struct FakeClock {
    now: Cell<u64>,
    timers: RefCell<Vec<u64>>,
    /// Every requested sleep, in request order, in milliseconds.
    pub sleeps: RefCell<Vec<u64>>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Jump to the earliest pending timer not after `limit`. Returns whether
    /// time moved.
    pub fn advance_to_next(&self, limit: Option<u64>) -> bool {
        let mut timers = self.timers.borrow_mut();
        timers.retain(|deadline| *deadline > self.now.get());
        let Some(next) = timers.iter().copied().min() else {
            return false;
        };
        if limit.is_some_and(|limit| next > limit) {
            return false;
        }
        self.now.set(next);
        true
    }

    pub fn set_now(&self, now: u64) {
        self.now.set(now);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }

    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let deadline = self.now.get().saturating_add(millis);
        self.timers.borrow_mut().push(deadline);
        self.sleeps.borrow_mut().push(millis);
        std::future::poll_fn(move |_| {
            if self.now.get() >= deadline {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }

    fn timeout<F: Future>(
        &self,
        duration: Duration,
        future: F,
    ) -> impl Future<Output = Result<F::Output, Elapsed>> {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let deadline = self.now.get().saturating_add(millis);
        self.timers.borrow_mut().push(deadline);
        let mut future = Box::pin(future);
        std::future::poll_fn(move |context| {
            if let Poll::Ready(value) = future.as_mut().poll(context) {
                return Poll::Ready(Ok(value));
            }
            if self.now.get() >= deadline {
                Poll::Ready(Err(Elapsed))
            } else {
                Poll::Pending
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Spawner
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TaskFlags {
    aborted: Cell<bool>,
    finished: Cell<bool>,
}

pub struct FakeHandle(Rc<TaskFlags>);

impl TaskHandle for FakeHandle {
    fn abort(&self) {
        self.0.aborted.set(true);
    }

    fn is_finished(&self) -> bool {
        self.0.finished.get() || self.0.aborted.get()
    }
}

impl Drop for FakeHandle {
    fn drop(&mut self) {
        self.0.aborted.set(true);
    }
}

type Task = (Pin<Box<dyn Future<Output = ()>>>, Rc<TaskFlags>);

#[derive(Default, Clone)]
pub struct FakeSpawner {
    tasks: Rc<RefCell<Vec<Task>>>,
}

impl FakeSpawner {
    /// Poll every live task once. Tasks spawned meanwhile are polled next
    /// round.
    pub fn poll_all(&self, context: &mut Context<'_>) {
        let current = std::mem::take(&mut *self.tasks.borrow_mut());
        let mut kept = Vec::new();
        for (mut future, flags) in current {
            if flags.aborted.get() {
                continue;
            }
            if future.as_mut().poll(context).is_ready() {
                flags.finished.set(true);
            } else {
                kept.push((future, flags));
            }
        }
        self.tasks.borrow_mut().splice(0..0, kept);
    }

    pub fn live(&self) -> usize {
        self.tasks.borrow().len()
    }

    /// End every task without finishing it, as a panicked or dropped task
    /// ends: it reports finished and never delivers anything.
    pub fn abort_all(&self) {
        for (_, flags) in self.tasks.borrow().iter() {
            flags.aborted.set(true);
        }
    }
}

impl Spawner for FakeSpawner {
    type Handle = FakeHandle;

    fn spawn(&self, future: impl Future<Output = ()> + 'static) -> FakeHandle {
        let flags = Rc::new(TaskFlags::default());
        self.tasks
            .borrow_mut()
            .push((Box::pin(future), flags.clone()));
        FakeHandle(flags)
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// One scripted answer.
#[derive(Debug, Clone)]
pub enum Reply {
    Ok(String),
    Status(u16),
    Network,
    Unreadable,
}

/// One unit of a streamed body.
#[derive(Debug, Clone)]
pub enum Chunk {
    Bytes(Vec<u8>),
    Wait(u64),
    Fail,
}

pub type ChunkQueue = Rc<RefCell<VecDeque<Chunk>>>;

pub fn chunk_queue(chunks: impl IntoIterator<Item = Chunk>) -> ChunkQueue {
    Rc::new(RefCell::new(chunks.into_iter().collect()))
}

pub struct ChunkSource {
    queue: ChunkQueue,
    clock: Rc<FakeClock>,
    keep_open: bool,
}

impl ByteSource for ChunkSource {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, RuntimeError> {
        loop {
            let next = self.queue.borrow_mut().pop_front();
            match next {
                Some(Chunk::Bytes(bytes)) => return Ok(Some(bytes)),
                Some(Chunk::Wait(millis)) => {
                    self.clock.sleep(Duration::from_millis(millis)).await;
                }
                Some(Chunk::Fail) => {
                    return Err(RuntimeError::Network {
                        path: "/stream".to_owned(),
                        message: "reset".to_owned(),
                    });
                }
                None if self.keep_open => {
                    let queue = self.queue.clone();
                    std::future::poll_fn(move |_| {
                        if queue.borrow().is_empty() {
                            Poll::Pending
                        } else {
                            Poll::Ready(())
                        }
                    })
                    .await;
                }
                None => return Ok(None),
            }
        }
    }
}

pub struct ScriptedTransport {
    clock: Rc<FakeClock>,
    /// `(path, body)` of every POST and `(path, "")` of every GET/stream.
    pub calls: RefCell<Vec<(String, String)>>,
    replies: RefCell<HashMap<String, VecDeque<Reply>>>,
    streams: RefCell<HashMap<String, VecDeque<(ChunkQueue, bool)>>>,
    history: RefCell<VecDeque<Result<(ChunkQueue, bool), RuntimeError>>>,
}

impl ScriptedTransport {
    pub fn new(clock: Rc<FakeClock>) -> Self {
        Self {
            clock,
            calls: RefCell::new(Vec::new()),
            replies: RefCell::new(HashMap::new()),
            streams: RefCell::new(HashMap::new()),
            history: RefCell::new(VecDeque::new()),
        }
    }

    /// Script the answers of `path`, in order; the last one repeats.
    pub fn script(&self, path: &str, replies: Vec<Reply>) {
        self.replies
            .borrow_mut()
            .insert(path.to_owned(), replies.into());
    }

    /// Queue one connection for `open_sse(path)`.
    pub fn push_stream(&self, path: &str, queue: ChunkQueue, keep_open: bool) {
        self.streams
            .borrow_mut()
            .entry(path.to_owned())
            .or_default()
            .push_back((queue, keep_open));
    }

    /// Queue the answer of the next `open_history`.
    pub fn push_history(&self, answer: Result<(ChunkQueue, bool), RuntimeError>) {
        self.history.borrow_mut().push_back(answer);
    }

    pub fn calls_to(&self, path: &str) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|(called, _)| called == path)
            .map(|(_, body)| body.clone())
            .collect()
    }

    fn answer(&self, path: &str) -> Result<String, RuntimeError> {
        let mut replies = self.replies.borrow_mut();
        let Some(queue) = replies.get_mut(path) else {
            return Err(RuntimeError::Network {
                path: path.to_owned(),
                message: "no script".to_owned(),
            });
        };
        let reply = if queue.len() > 1 {
            queue.pop_front().expect("non-empty")
        } else {
            queue.front().cloned().expect("script has an entry")
        };
        match reply {
            Reply::Ok(text) => Ok(text),
            Reply::Status(status) => Err(RuntimeError::Daemon {
                path: path.to_owned(),
                status,
                message: "refused".to_owned(),
            }),
            Reply::Network => Err(RuntimeError::Network {
                path: path.to_owned(),
                message: "network".to_owned(),
            }),
            Reply::Unreadable => Err(RuntimeError::UnreadableResponse {
                path: path.to_owned(),
            }),
        }
    }

    fn source(&self, queue: ChunkQueue, keep_open: bool) -> ChunkSource {
        ChunkSource {
            queue,
            clock: self.clock.clone(),
            keep_open,
        }
    }
}

impl Transport for ScriptedTransport {
    type Sse = ChunkSource;
    type Frames = ChunkSource;

    async fn get(&self, path: &str) -> Result<String, RuntimeError> {
        self.calls
            .borrow_mut()
            .push((path.to_owned(), String::new()));
        self.answer(path)
    }

    async fn post_encoded(&self, path: &str, body: String) -> Result<String, RuntimeError> {
        self.calls.borrow_mut().push((path.to_owned(), body));
        self.answer(path)
    }

    async fn open_sse(&self, path: &str) -> Result<ChunkSource, RuntimeError> {
        self.calls
            .borrow_mut()
            .push((path.to_owned(), String::new()));
        let next = self
            .streams
            .borrow_mut()
            .get_mut(path)
            .and_then(VecDeque::pop_front);
        match next {
            Some((queue, keep_open)) => Ok(self.source(queue, keep_open)),
            None => Err(RuntimeError::Network {
                path: path.to_owned(),
                message: "no stream".to_owned(),
            }),
        }
    }

    async fn open_history(&self, path: &str, body: String) -> Result<ChunkSource, RuntimeError> {
        self.calls.borrow_mut().push((path.to_owned(), body));
        match self.history.borrow_mut().pop_front() {
            Some(Ok((queue, keep_open))) => Ok(self.source(queue, keep_open)),
            Some(Err(error)) => Err(error),
            None => Err(RuntimeError::Network {
                path: path.to_owned(),
                message: "no history".to_owned(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------

pub struct FakeAuth;

impl Auth for FakeAuth {
    async fn establish(&self, _mode: EstablishMode) -> Result<EstablishedSession, RuntimeError> {
        Err(RuntimeError::Fatal(
            "fake auth establishes nothing".to_owned(),
        ))
    }

    async fn redeem(&self, _launch_url: &str) -> Result<EstablishedSession, RuntimeError> {
        Err(RuntimeError::Fatal("fake auth redeems nothing".to_owned()))
    }

    fn replacement(&self) -> ReplacementMode {
        ReplacementMode::InPlace
    }
}

pub struct FakePlatform {
    pub clock: Rc<FakeClock>,
    pub spawner: FakeSpawner,
    pub transport: ScriptedTransport,
    pub auth: FakeAuth,
    pub store: MemoryStore,
}

impl FakePlatform {
    pub fn new() -> Rc<Self> {
        let clock = Rc::new(FakeClock::new());
        Rc::new(Self {
            transport: ScriptedTransport::new(clock.clone()),
            clock,
            spawner: FakeSpawner::default(),
            auth: FakeAuth,
            store: MemoryStore::new(),
        })
    }

    pub fn sim(self: &Rc<Self>) -> Sim {
        Sim {
            clock: self.clock.clone(),
            spawner: self.spawner.clone(),
        }
    }
}

impl Platform for FakePlatform {
    type Clock = FakeClock;
    type Spawner = FakeSpawner;
    type Transport = ScriptedTransport;
    type Auth = FakeAuth;
    type Store = MemoryStore;

    fn clock(&self) -> &FakeClock {
        &self.clock
    }

    fn spawner(&self) -> &FakeSpawner {
        &self.spawner
    }

    fn transport(&self) -> &ScriptedTransport {
        &self.transport
    }

    fn auth(&self) -> &FakeAuth {
        &self.auth
    }

    fn store(&self) -> &MemoryStore {
        &self.store
    }

    fn random_bytes(&self, out: &mut [u8]) {
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = u8::try_from(index % 251)
                .unwrap_or(0)
                .wrapping_mul(7)
                .wrapping_add(3);
        }
    }
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Drives a main future and the spawned tasks against the virtual clock.
pub struct Sim {
    pub clock: Rc<FakeClock>,
    pub spawner: FakeSpawner,
}

impl Sim {
    /// Run `future` to completion. Virtual time advances only when all tasks
    /// are waiting on timers. Panics if nothing can make progress.
    pub fn run<F: Future>(&self, future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        let mut idle_rounds = 0_u32;
        for _ in 0..1_000_000 {
            for _ in 0..4 {
                if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                    return value;
                }
                self.spawner.poll_all(&mut context);
            }
            if self.clock.advance_to_next(None) {
                idle_rounds = 0;
            } else {
                idle_rounds += 1;
                assert!(idle_rounds < 16, "sim deadlock: no task can make progress");
            }
        }
        panic!("sim did not finish");
    }

    /// Let tasks run, advancing virtual time up to `millis` from now.
    pub fn run_for(&self, millis: u64) {
        let target = self.clock.now_ms().saturating_add(millis);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            for _ in 0..8 {
                self.spawner.poll_all(&mut context);
            }
            if !self.clock.advance_to_next(Some(target)) {
                break;
            }
        }
        self.clock.set_now(target);
        for _ in 0..8 {
            self.spawner.poll_all(&mut context);
        }
    }
}

/// Poll `future` once with a no-op waker.
pub fn poll_once<F: Future>(future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

// ---------------------------------------------------------------------------
// A completed history stream
// ---------------------------------------------------------------------------

struct DiscardSink;

impl HistorySink for DiscardSink {
    fn stage_events(&mut self, _records: &[serde_json::Value]) -> Result<(), String> {
        Ok(())
    }

    fn stage_transfer_proof(&mut self, _proof: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }

    fn stage_cancellation_proof(&mut self, _proof: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }
}

/// A seat-history stream proved to its completion fence (an initialization
/// row and an active producer row on seat `P`/`R`/`T`) that stays open for
/// live events. Stands in for the live reader a claim hands over.
pub fn open_history(platform: &Rc<FakePlatform>) -> LiveHistory<ChunkSource> {
    let (zero, a, b) = ("0".repeat(64), "a".repeat(64), "b".repeat(64));
    let operation = "11111111-1111-8111-8111-111111111111";
    let frame = |event: &str, data: String| format!("event: {event}\ndata: {data}\n\n");
    let begin = frame(
        "ui_seat.replay.begin.v1",
        r#"{"schema":1,"presentation_id":"P","chain_root_id":"R","thread_id":"T"}"#.to_owned(),
    );
    let initialization = format!(
        r#"{{"event_hash":"{a}","thread_id":"T","chain_root_id":"R","thread_seq":2,"prev_thread_event_hash":"{zero}","chain_seq":3,"event_type":"ui_seat.presentation_initialized.v1","payload":{{}}}}"#
    );
    let producer = format!(
        r#"{{"event_hash":"{b}","thread_id":"T","chain_root_id":"R","thread_seq":3,"prev_thread_event_hash":"{a}","chain_seq":4,"event_type":"ui_seat.producer.v3","payload":{{"schema_version":"ryeos.ui.seat.producer.v3","operation_id":"{operation}","producer_incarnation":"{b}","next_engine_seq":0,"state":"active","request":{{"action":"claim","operation_id":"{operation}","presentation_id":"P","chain_root_id":"R","thread_id":"T"}}}}}}"#
    );
    let events = frame(
        "ui_seat.replay.events.v1",
        format!(r#"{{"schema":1,"events":[{initialization},{producer}]}}"#),
    );
    let complete = frame(
        "ui_seat.replay.complete.v1",
        format!(r#"{{"schema":1,"tip_hash":"{b}","thread_seq":3,"event_count":2}}"#),
    );
    let queue = chunk_queue(
        [begin, events, complete]
            .into_iter()
            .map(|text| Chunk::Bytes(text.into_bytes())),
    );
    platform.transport.push_history(Ok((queue, true)));
    let coordinate = SeatCoordinate {
        presentation_id: "P".to_owned(),
        chain_root_id: "R".to_owned(),
        thread_id: "T".to_owned(),
    };
    let claim = HistoryClaim {
        operation_id: operation.to_owned(),
        producer_incarnation: b,
        next_engine_seq: 0,
        request: None,
    };
    platform
        .sim()
        .run(read_history(
            &platform.transport,
            &coordinate,
            &claim,
            &mut DiscardSink,
        ))
        .expect("the scripted history completes")
}
