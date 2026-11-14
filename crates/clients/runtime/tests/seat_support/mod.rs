//! A scripted seat for the bootstrap and reproof tests: one session, one
//! shared-root attachment, and the exact wire answers the daemon gives for a
//! fresh claim, plus builders for retained history rows and replay streams.
//!
//! The deterministic platform draws the same entropy on every call, so the
//! intent a runtime freezes (creation id, claim nonce) can be recomputed here
//! and the claim request the runtime must send built ahead of time.

#![allow(dead_code)]

use std::cell::Cell;
use std::rc::Rc;

use ryeos_client_base::UI_BINDING_CONTRACT_REVISION;
use ryeos_client_base::ui::binding::{SEAT_PRODUCER_SCHEMA, UiSeatProducer, UiSeatProducerState};
use ryeos_client_base::ui::presentation_initialization::{
    UiPresentationInitialSelection, UiPresentationInitialization,
    UiPresentationInitializationRecord,
};
use ryeos_client_base::ui::{
    BrowserSession, BrowserViewport, RyeOsCore, RyeOsEffect, UiBindingAttachment,
    UiBindingAttachmentCoordinate, UiPresentationMembership, UiPresentationProducer,
    UiSeatControlPredecessor, UiSeatControlRequest, UiSeatProducerClaim,
};
use ryeos_client_runtime::ClientRuntime;
use ryeos_client_runtime::api::{
    SEAT_APPEND_PATH, SEAT_CLOSE_PATH, SEAT_HISTORY_PATH, SEAT_OPEN_PATH, SEAT_REPLAY_PATH,
    SESSION_CURRENT_PATH,
};
use ryeos_client_runtime::error::RuntimeError;
use ryeos_client_runtime::history::SeatCoordinate;
use ryeos_client_runtime::history::{BEGIN, COMPLETE, EVENTS, INVALIDATED, REPROOF_REQUIRED};
use ryeos_client_runtime::platform::{MemoryStore, Platform};
use ryeos_client_runtime::replay::CoreConfig;
use ryeos_client_runtime::seat::{RetryPolicy, SeatStart, random_uuid};
use serde_json::{Value, json};

use crate::common::sim::{
    Chunk, ChunkQueue, FakeAuth, FakeClock, FakePlatform, FakeSpawner, Reply, ScriptedTransport,
    Sim, chunk_queue,
};

pub const SESSION_ID: &str = "session-1";
pub const PRESENTATION: &str = "11111111-1111-4111-8111-111111111111";
pub const THREAD: &str = "seat-thread";
pub const CHAIN: &str = "chain-1";
/// The signed append bound of the root attachment.
pub const BOUND: u64 = 65_536;
/// A creation id for sessions whose presentation already exists.
pub const RETAINED_CREATION: &str = "33333333-3333-4333-8333-333333333333";

pub fn hex(digit: char) -> String {
    digit.to_string().repeat(64)
}

/// The event hash of the row at thread position `position`.
pub fn hash(position: u64) -> String {
    format!("{position:064x}")
}

/// The incarnation the daemon assigns to the scripted claimant.
pub fn incarnation() -> String {
    hex('b')
}

pub fn coordinate() -> SeatCoordinate {
    SeatCoordinate {
        presentation_id: PRESENTATION.to_owned(),
        thread_id: THREAD.to_owned(),
        chain_root_id: CHAIN.to_owned(),
    }
}

pub fn producer(incarnation: &str) -> UiPresentationProducer {
    UiPresentationProducer {
        presentation_id: PRESENTATION.to_owned(),
        thread_id: THREAD.to_owned(),
        producer_incarnation: incarnation.to_owned(),
    }
}

pub fn root_coordinate() -> UiBindingAttachmentCoordinate {
    UiBindingAttachmentCoordinate {
        binding_attachment_id: "root".to_owned(),
        binding_generation: 1,
        binding_digest: hex('a'),
    }
}

fn root_attachment() -> UiBindingAttachment {
    serde_json::from_value(json!({
        "binding_attachment_id": "root",
        "binding_generation": 1,
        "binding_digest": hex('a'),
        "root_ref": "config:test/shared",
        "root_generation": "generation",
        "project_path": null,
        "subject": {"kind": "projectless"},
        "posture": "observation_only",
        "binding_request_bounds": {
            "max_request_bytes": 4096,
            "max_input_bytes": 1024,
            "max_seat_append_bytes": BOUND,
        },
        "presentation": {
            "definition": {
                "kind": "shared_presentation",
                "config": {
                    "category": "ui",
                    "shared_presentation": {
                        "schema": 2,
                        "title": "RyeOS",
                        "launcher": "launch",
                        "dialogs": [{"id": "launch", "title": "Launch", "view": "view:test/work"}],
                        "capabilities": {"sources": [], "affordances": []},
                    },
                },
            },
            "views": {"view:test/work": {}},
        },
    }))
    .expect("the shared root descriptor decodes")
}

pub fn initialization(creation_request_id: &str) -> UiPresentationInitialization {
    UiPresentationInitialization {
        schema: 2,
        selection: UiPresentationInitialSelection::ConfiguredDefaults,
        presentation_id: PRESENTATION.to_owned(),
        creation_request_id: creation_request_id.to_owned(),
        root_binding: root_coordinate(),
        next_mount_id: 1,
        surfaces: Vec::new(),
        binding_attachments: vec![root_attachment()],
    }
}

pub fn initialization_record(creation_request_id: &str) -> UiPresentationInitializationRecord {
    UiPresentationInitializationRecord {
        operation_id: creation_request_id.to_owned(),
        initialization: initialization(creation_request_id),
    }
}

/// The authenticated session. With a creation id it already holds the
/// presentation's membership (as the daemon's session does once the
/// presentation is prepared); without one it holds only the root.
pub fn session(creation_request_id: Option<&str>) -> BrowserSession {
    let presentations = creation_request_id
        .map(|creation| UiPresentationMembership {
            presentation_id: PRESENTATION.to_owned(),
            root_binding: root_coordinate(),
            thread_id: Some(THREAD.to_owned()),
            chain_root_id: Some(CHAIN.to_owned()),
            next_mount_id: 1,
            initialization_digest: initialization(creation)
                .digest()
                .expect("the initialization digests"),
            surfaces: Vec::new(),
        })
        .into_iter()
        .collect();
    BrowserSession {
        ui_binding_contract_revision: UI_BINDING_CONTRACT_REVISION.to_owned(),
        session_id: SESSION_ID.to_owned(),
        user_principal_id: None,
        binding_attachments: vec![root_attachment()],
        root_attachment_id: "root".to_owned(),
        presentations,
        events_url: None,
    }
}

pub fn session_reply(session: &BrowserSession) -> Reply {
    Reply::Ok(serde_json::to_string(session).expect("the session encodes"))
}

// ---------------------------------------------------------------------------
// Control requests and the daemon's answers
// ---------------------------------------------------------------------------

pub fn claim_request_with_nonce(
    nonce: &str,
    predecessor: UiSeatControlPredecessor,
    claim: UiSeatProducerClaim,
) -> UiSeatControlRequest {
    let mut request = UiSeatControlRequest::Claim {
        schema: 2,
        operation_id: String::new(),
        presentation_id: PRESENTATION.to_owned(),
        thread_id: THREAD.to_owned(),
        chain_root_id: CHAIN.to_owned(),
        predecessor,
        intent_nonce: nonce.to_owned(),
        claim,
    };
    request.assign_identity().expect("the claim is well formed");
    request
}

/// The claim a runtime on `platform` freezes: its nonce is the platform's
/// next draw.
pub fn claim_request(
    platform: &FakePlatform,
    predecessor: UiSeatControlPredecessor,
    claim: UiSeatProducerClaim,
) -> UiSeatControlRequest {
    claim_request_with_nonce(&random_uuid(platform), predecessor, claim)
}

/// The relinquish a runtime on `platform` sends to give back the scripted
/// claimant, after observing control position 2.
pub fn relinquish_request(platform: &FakePlatform) -> UiSeatControlRequest {
    let mut request = UiSeatControlRequest::Relinquish {
        schema: 2,
        operation_id: String::new(),
        presentation_id: PRESENTATION.to_owned(),
        thread_id: THREAD.to_owned(),
        chain_root_id: CHAIN.to_owned(),
        predecessor: UiSeatControlPredecessor::Control {
            event_hash: hash(2),
        },
        intent_nonce: random_uuid(platform),
        producer_incarnation: incarnation(),
    };
    request
        .assign_identity()
        .expect("the relinquish is well formed");
    request
}

pub fn relinquish_ack(request: &UiSeatControlRequest) -> Reply {
    let request = serde_json::to_value(request).expect("the request encodes");
    Reply::Ok(
        json!({
            "presentation_id": request["presentation_id"],
            "thread_id": request["thread_id"],
            "producer_incarnation": request["producer_incarnation"],
            "operation_id": request["operation_id"],
            "state": "relinquished",
        })
        .to_string(),
    )
}

pub fn prepare_reply() -> Reply {
    Reply::Ok(
        json!({
            "presentation_id": PRESENTATION,
            "thread_id": THREAD,
            "chain_root_id": CHAIN,
            "root_binding": root_coordinate(),
        })
        .to_string(),
    )
}

pub fn observation(control_predecessor: Value, producer: Value) -> Reply {
    Reply::Ok(
        json!({
            "presentation_id": PRESENTATION,
            "thread_id": THREAD,
            "chain_root_id": CHAIN,
            "root_binding": root_coordinate(),
            "revision": 1,
            "control_predecessor": control_predecessor,
            "producer": producer,
        })
        .to_string(),
    )
}

/// A presentation nobody has claimed: its predecessor is the initialization.
pub fn unclaimed() -> Reply {
    observation(
        json!({"kind": "initialization", "event_hash": hash(1)}),
        json!({"kind": "unclaimed"}),
    )
}

/// `incarnation` is the active writer; the last control is position 2.
pub fn active(incarnation: &str) -> Reply {
    observation(
        json!({"kind": "control", "event_hash": hash(2)}),
        json!({"kind": "active", "producer_incarnation": incarnation}),
    )
}

pub fn relinquished(incarnation: &str) -> Reply {
    observation(
        json!({"kind": "control", "event_hash": hash(2)}),
        json!({"kind": "relinquished", "producer_incarnation": incarnation}),
    )
}

pub fn claim_ack_value(request: &UiSeatControlRequest, incarnation: &str, next: u64) -> Value {
    let request = serde_json::to_value(request).expect("the request encodes");
    json!({
        "presentation_id": request["presentation_id"],
        "thread_id": request["thread_id"],
        "chain_root_id": request["chain_root_id"],
        "operation_id": request["operation_id"],
        "state": "active",
        "producer_incarnation": incarnation,
        "next_engine_seq": next,
    })
}

pub fn claim_ack(request: &UiSeatControlRequest, incarnation: &str, next: u64) -> Reply {
    Reply::Ok(claim_ack_value(request, incarnation, next).to_string())
}

// ---------------------------------------------------------------------------
// History rows and streams
// ---------------------------------------------------------------------------

fn row(position: u64, event_type: &str, payload: Value) -> Value {
    let previous = if position == 1 {
        hex('0')
    } else {
        hash(position - 1)
    };
    json!({
        "event_hash": hash(position),
        "thread_id": THREAD,
        "chain_root_id": CHAIN,
        "thread_seq": position,
        "prev_thread_event_hash": previous,
        "chain_seq": position,
        "event_type": event_type,
        "payload": payload,
    })
}

pub fn init_row(position: u64, creation_request_id: &str) -> Value {
    row(
        position,
        "ui_seat.presentation_initialized.v1",
        serde_json::to_value(initialization_record(creation_request_id))
            .expect("the record encodes"),
    )
}

pub fn facet_row(position: u64, engine_seq: u64, key: &str, value: Value) -> Value {
    row(
        position,
        "seat.facet",
        json!({
            "seq": engine_seq,
            "operation_id": "00000000-0000-8000-8000-000000000001",
            "payload": {"key": key, "value": value},
        }),
    )
}

/// The producer control row that records `request` (a claim) as acknowledged.
pub fn claim_row(
    position: u64,
    request: &UiSeatControlRequest,
    incarnation: &str,
    next_engine_seq: u64,
) -> Value {
    let UiSeatControlRequest::Claim { claim, .. } = request else {
        panic!("only a claim is recorded by this builder");
    };
    let predecessor = match claim {
        UiSeatProducerClaim::Fresh => None,
        UiSeatProducerClaim::Resume {
            expected_producer_incarnation,
        }
        | UiSeatProducerClaim::Takeover {
            expected_producer_incarnation,
        } => Some(expected_producer_incarnation.clone()),
    };
    let control = UiSeatProducer {
        schema_version: SEAT_PRODUCER_SCHEMA.to_owned(),
        request: request.clone(),
        operation_id: request.operation_id().to_owned(),
        claim: Some(claim.clone()),
        predecessor,
        producer_incarnation: incarnation.to_owned(),
        state: UiSeatProducerState::Active,
        next_engine_seq,
    };
    row(
        position,
        "ui_seat.producer.v3",
        serde_json::to_value(control).expect("the control encodes"),
    )
}

pub fn frame(event: &str, data: Value) -> Chunk {
    Chunk::Bytes(format!("event: {event}\ndata: {data}\n\n").into_bytes())
}

fn begin_frame() -> Chunk {
    frame(
        BEGIN,
        json!({
            "schema": 1,
            "presentation_id": PRESENTATION,
            "chain_root_id": CHAIN,
            "thread_id": THREAD,
        }),
    )
}

fn events_frame(rows: &[Value]) -> Chunk {
    frame(EVENTS, json!({"schema": 1, "events": rows}))
}

/// A stream proved to its completion fence at the last row.
pub fn proof_chunks(rows: &[Value]) -> Vec<Chunk> {
    let tip = rows.last().expect("a proof has rows");
    vec![
        begin_frame(),
        events_frame(rows),
        frame(
            COMPLETE,
            json!({
                "schema": 1,
                "tip_hash": tip["event_hash"],
                "thread_seq": tip["thread_seq"],
                "event_count": rows.len(),
            }),
        ),
    ]
}

/// A stream that ends before its fence.
pub fn truncated_chunks(rows: &[Value]) -> Vec<Chunk> {
    vec![begin_frame(), events_frame(rows)]
}

/// A stream whose fixed anchor was overtaken: it is invalidated before the
/// fence.
pub fn invalidated_chunks(rows: &[Value]) -> Vec<Chunk> {
    vec![
        begin_frame(),
        events_frame(rows),
        frame(INVALIDATED, json!({"schema": 1})),
    ]
}

/// A live reproof wake.
pub fn wake() -> Chunk {
    frame(REPROOF_REQUIRED, json!({"schema": 1}))
}

/// Queue the answer of the next `seat/history` request.
pub fn push_history(platform: &FakePlatform, chunks: Vec<Chunk>, keep_open: bool) -> ChunkQueue {
    let queue = chunk_queue(chunks);
    platform
        .transport
        .push_history(Ok((queue.clone(), keep_open)));
    queue
}

// ---------------------------------------------------------------------------
// Observing the wire
// ---------------------------------------------------------------------------

/// The seat-related requests made so far, in order, named by what they do.
pub fn wire(platform: &FakePlatform) -> Vec<String> {
    platform
        .transport
        .calls
        .borrow()
        .iter()
        .filter_map(|(path, body)| match path.as_str() {
            SEAT_OPEN_PATH => {
                let body: Value = serde_json::from_str(body).expect("a control body is JSON");
                Some(format!("open:{}", body["action"].as_str().unwrap_or("?")))
            }
            SESSION_CURRENT_PATH => Some("session".to_owned()),
            SEAT_HISTORY_PATH => Some("history".to_owned()),
            SEAT_CLOSE_PATH => Some("close".to_owned()),
            SEAT_APPEND_PATH => Some("append".to_owned()),
            SEAT_REPLAY_PATH => Some("replay".to_owned()),
            _ => None,
        })
        .collect()
}

pub fn count(platform: &FakePlatform, label: &str) -> usize {
    wire(platform).iter().filter(|name| *name == label).count()
}

/// The raw bodies posted to `path`.
pub fn raw(platform: &FakePlatform, path: &str) -> Vec<String> {
    platform.transport.calls_to(path)
}

/// The decoded control bodies posted to `seat/open` with this action.
pub fn open_bodies(platform: &FakePlatform, action: &str) -> Vec<String> {
    raw(platform, SEAT_OPEN_PATH)
        .into_iter()
        .filter(|body| {
            serde_json::from_str::<Value>(body)
                .is_ok_and(|body| body["action"].as_str() == Some(action))
        })
        .collect()
}

pub fn json_of(text: &str) -> Value {
    serde_json::from_str(text).expect("a wire body is JSON")
}

/// The message of a refusal that never reached an outcome.
pub fn refusal<T>(outcome: Result<T, RuntimeError>) -> String {
    match outcome {
        Err(RuntimeError::Fatal(message)) => message,
        Err(other) => panic!("expected a fatal refusal, got {other:?}"),
        Ok(_) => panic!("expected a refusal, but the call succeeded"),
    }
}

// ---------------------------------------------------------------------------
// A fresh claim
// ---------------------------------------------------------------------------

/// Everything a fresh start on a [`FakePlatform`] sends and receives.
pub struct FreshWorld {
    /// The creation id the runtime froze, which the initialization row names.
    pub creation: String,
    /// The claim the runtime must send.
    pub request: UiSeatControlRequest,
    /// The proof rows: the initialization and the claimant's own control.
    pub rows: Vec<Value>,
    /// The session the host starts from (no membership yet).
    pub before: BrowserSession,
    /// The session once the presentation is prepared.
    pub after: BrowserSession,
}

impl FreshWorld {
    pub fn new(platform: &FakePlatform) -> Self {
        let creation = random_uuid(platform);
        let request = claim_request(
            platform,
            UiSeatControlPredecessor::Initialization {
                event_hash: hash(1),
            },
            UiSeatProducerClaim::Fresh,
        );
        let rows = vec![
            init_row(1, &creation),
            claim_row(2, &request, &incarnation(), 0),
        ];
        Self {
            before: session(None),
            after: session(Some(&creation)),
            creation,
            request,
            rows,
        }
    }

    /// The `seat/open` answers of a fresh claim: prepare, observe, claim, and
    /// then (for the give-back observation) the claimant as active writer.
    pub fn open_replies(&self) -> Vec<Reply> {
        vec![
            prepare_reply(),
            unclaimed(),
            claim_ack(&self.request, &incarnation(), 0),
            active(&incarnation()),
        ]
    }

    /// Script `seat/open`, `seat/close` and `session/current`.
    pub fn script(&self, platform: &FakePlatform) {
        platform
            .transport
            .script(SEAT_OPEN_PATH, self.open_replies());
        platform.transport.script(
            SEAT_CLOSE_PATH,
            vec![relinquish_ack(&relinquish_request(platform))],
        );
        platform
            .transport
            .script(SESSION_CURRENT_PATH, vec![session_reply(&self.after)]);
    }

    /// Queue one complete proof stream of the fresh claim.
    pub fn proof(&self, platform: &FakePlatform, keep_open: bool) -> ChunkQueue {
        push_history(platform, proof_chunks(&self.rows), keep_open)
    }
}

pub fn new_runtime(platform: &Rc<FakePlatform>) -> ClientRuntime<FakePlatform> {
    ClientRuntime::new(platform.clone(), Rc::new(|| {}), RetryPolicy::WEB)
}

pub fn start_with(
    platform: &Rc<FakePlatform>,
    runtime: &mut ClientRuntime<FakePlatform>,
    session: &BrowserSession,
    mode: SeatStart,
) -> Result<(RyeOsCore, Vec<RyeOsEffect>), RuntimeError> {
    platform.sim().run(runtime.start(
        session.clone(),
        BrowserViewport::default(),
        mode,
        CoreConfig::default(),
    ))
}

pub fn start_fresh(
    platform: &Rc<FakePlatform>,
    runtime: &mut ClientRuntime<FakePlatform>,
    world: &FreshWorld,
) -> Result<(RyeOsCore, Vec<RyeOsEffect>), RuntimeError> {
    start_with(platform, runtime, &world.before, SeatStart::Fresh)
}

/// A runtime that completed a fresh start, with the live stream of its claim
/// still open for the test to push into.
pub struct Started {
    pub platform: Rc<FakePlatform>,
    pub world: FreshWorld,
    pub runtime: ClientRuntime<FakePlatform>,
    pub core: RyeOsCore,
    pub live: ChunkQueue,
    pub producer: UiPresentationProducer,
}

pub fn started() -> Started {
    let platform = FakePlatform::new();
    let world = FreshWorld::new(&platform);
    world.script(&platform);
    let live = world.proof(&platform, true);
    let mut runtime = new_runtime(&platform);
    let (core, _effects) =
        start_fresh(&platform, &mut runtime, &world).expect("the fresh start succeeds");
    Started {
        producer: producer(&incarnation()),
        platform,
        world,
        runtime,
        core,
        live,
    }
}

// ---------------------------------------------------------------------------
// A platform whose entropy never repeats
// ---------------------------------------------------------------------------

/// [`FakePlatform`] with entropy that differs on every draw, for tests that
/// need two runtimes to tell themselves apart.
pub struct CountingPlatform {
    pub inner: Rc<FakePlatform>,
    draws: Cell<u8>,
}

impl CountingPlatform {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            inner: FakePlatform::new(),
            draws: Cell::new(0),
        })
    }

    pub fn sim(&self) -> Sim {
        self.inner.sim()
    }
}

impl Platform for CountingPlatform {
    type Clock = FakeClock;
    type Spawner = FakeSpawner;
    type Transport = ScriptedTransport;
    type Auth = FakeAuth;
    type Store = MemoryStore;

    fn clock(&self) -> &FakeClock {
        &self.inner.clock
    }

    fn spawner(&self) -> &FakeSpawner {
        &self.inner.spawner
    }

    fn transport(&self) -> &ScriptedTransport {
        &self.inner.transport
    }

    fn auth(&self) -> &FakeAuth {
        &self.inner.auth
    }

    fn store(&self) -> &MemoryStore {
        &self.inner.store
    }

    fn random_bytes(&self, out: &mut [u8]) {
        let draw = self.draws.get();
        self.draws.set(draw.wrapping_add(1));
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = u8::try_from(index % 251)
                .unwrap_or(0)
                .wrapping_mul(7)
                .wrapping_add(3)
                .wrapping_add(draw.wrapping_mul(37));
        }
    }
}
