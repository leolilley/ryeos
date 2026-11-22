//! Same-producer reproof on the deterministic platform: the runtime only
//! reproves a settled, current seat, promotes the new cut when the host
//! folds it, schedules another proof for a wake that arrives meanwhile, and
//! fences a malformed wake. `reproof_seat` itself is driven directly for the
//! refusals that depend on the shared delivery state and on the session
//! re-read, and for the claim witness it proves against.

mod common;
mod seat_support;

use common::sim::Reply;
use ryeos_client_base::ui::{BrowserViewport, SeatEvent, SeatEventKind};
use ryeos_client_runtime::api::{SEAT_APPEND_PATH, SESSION_CURRENT_PATH};
use ryeos_client_runtime::history::{HistoryCompletion, REPROOF_REQUIRED};
use ryeos_client_runtime::replay::CoreConfig;
use ryeos_client_runtime::runtime::StopReason;
use ryeos_client_runtime::seat::{
    AppendFailure, RetryPolicy, SeatShared, Unavailable, reproof_seat, send_append,
};
use seat_support::*;
use serde_json::json;

/// The wire of a fresh start, before any reproof.
const START: [&str; 6] = [
    "open:prepare",
    "open:observe",
    "open:claim",
    "session",
    "history",
    "session",
];

fn writer_refused(stop: &Option<StopReason>) -> bool {
    matches!(
        stop,
        Some(StopReason::Unavailable(Unavailable::WriterRefused(_)))
    )
}

/// A wake starts a proof only after the host's drain; the finished proof is
/// invisible until the next drain folds it, and neither claims, relinquishes
/// nor appends anything. The proof is the claim's own stream read again:
/// session, history, session.
#[test]
fn a_reproof_is_published_only_when_the_host_folds_it() {
    let mut s = started();
    s.world.proof(&s.platform, true);
    s.live.borrow_mut().push_back(wake());
    s.platform.sim().run_for(10);

    let drained = s.runtime.drain(&mut s.core);
    assert!(drained.stop.is_none());
    assert!(drained.seat.reproof_pending, "the wake is recorded");
    assert_eq!(
        count(&s.platform, "history"),
        1,
        "the proof task has not run yet"
    );

    s.platform.sim().run_for(10);
    assert_eq!(count(&s.platform, "history"), 2);
    assert!(
        s.runtime.seat_status().reproof_pending,
        "a finished proof is not published before it is folded"
    );

    let drained = s.runtime.drain(&mut s.core);
    assert!(drained.stop.is_none(), "{:?}", drained.stop);
    assert!(!drained.seat.reproof_pending);
    assert!(drained.seat.ready);
    assert_eq!(s.runtime.producer(), Some(s.producer.clone()));

    let mut expected: Vec<&str> = START.to_vec();
    expected.extend(["session", "history", "session"]);
    assert_eq!(wire(&s.platform), expected);
    assert_eq!(
        s.core.seat.events().len(),
        s.core.acknowledged_seat_prefix_len(),
        "nothing from history is queued for append"
    );
}

/// A wake that is already waiting on the new proof's stream when that proof
/// completes schedules another proof once the stream is adopted: three
/// history reads in all, still one claim.
#[test]
fn a_wake_during_completion_schedules_another_proof() {
    let mut s = started();
    let mut second = proof_chunks(&s.world.rows);
    second.push(wake());
    push_history(&s.platform, second, true);
    s.world.proof(&s.platform, true);
    s.live.borrow_mut().push_back(wake());

    s.platform.sim().run_for(10);
    assert!(s.runtime.drain(&mut s.core).seat.reproof_pending);
    s.platform.sim().run_for(10);
    let drained = s.runtime.drain(&mut s.core);
    assert!(!drained.seat.reproof_pending, "the first proof was adopted");
    assert_eq!(count(&s.platform, "history"), 2);

    // The adopted stream already holds the next wake.
    s.platform.sim().run_for(10);
    let drained = s.runtime.drain(&mut s.core);
    assert!(drained.seat.reproof_pending);
    assert!(drained.stop.is_none());
    s.platform.sim().run_for(10);
    let drained = s.runtime.drain(&mut s.core);
    assert!(!drained.seat.reproof_pending);
    assert!(drained.stop.is_none(), "{:?}", drained.stop);

    assert_eq!(count(&s.platform, "history"), 3);
    assert_eq!(count(&s.platform, "open:claim"), 1);
    assert_eq!(count(&s.platform, "close"), 0);
    assert_eq!(count(&s.platform, "append"), 0);
    assert_eq!(s.runtime.producer(), Some(s.producer.clone()));
}

/// A wake that is not exactly `{"schema":1}` is not a wake: the live stream
/// is treated as invalidated, delivery is fenced, the writer is dropped and
/// no proof is opened.
#[test]
fn a_malformed_wake_fences_the_seat() {
    for data in [json!({"schema": 1, "extra": true}), json!({"schema": 2})] {
        let mut s = started();
        s.live
            .borrow_mut()
            .push_back(frame(REPROOF_REQUIRED, data.clone()));
        s.platform.sim().run_for(10);

        let drained = s.runtime.drain(&mut s.core);
        assert!(writer_refused(&drained.stop), "{data}: {:?}", drained.stop);
        assert!(!drained.seat.reproof_pending, "{data}");
        assert!(drained.seat.blocked, "{data}");
        assert!(s.runtime.producer().is_none(), "{data}");
        assert_eq!(count(&s.platform, "history"), 1, "{data}");
    }
}

/// A wake does not start a proof over an unacknowledged local event: the
/// event is written first, and when the writer is refused no proof is ever
/// opened.
#[test]
fn a_reproof_waits_for_the_seat_to_settle() {
    let mut s = started();
    s.core.seat.append_facet("draft", json!(1));
    s.platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Status(403)]);
    s.world.proof(&s.platform, true);
    s.live.borrow_mut().push_back(wake());
    s.platform.sim().run_for(10);

    let first = s.runtime.drain(&mut s.core);
    assert!(first.seat.reproof_pending);
    assert_eq!(
        count(&s.platform, "history"),
        1,
        "no proof over unsettled state"
    );
    s.platform.sim().run_for(10);
    let second = s.runtime.drain(&mut s.core);

    assert!(
        writer_refused(&first.stop) || writer_refused(&second.stop),
        "{:?} {:?}",
        first.stop,
        second.stop
    );
    assert!(count(&s.platform, "append") >= 1);
    assert_eq!(count(&s.platform, "history"), 1, "the proof never opened");
    assert!(s.runtime.producer().is_none());
}

/// Drive `reproof_seat` directly over a shared seat state the test controls.
fn shared_for(s: &Started, frontier: u64) -> std::rc::Rc<SeatShared<common::sim::FakePlatform>> {
    let shared = SeatShared::new(s.platform.clone());
    shared.delivery.borrow_mut().reset_for(
        coordinate(),
        SESSION_ID,
        root_coordinate(),
        "retained-key".to_owned(),
        frontier,
        BOUND,
    );
    *shared.producer.borrow_mut() = Some(s.producer.clone());
    shared
}

/// A reproof never discards an unresolved append: while one is pending it
/// refuses before reading the session or opening any stream.
#[test]
fn a_reproof_cannot_discard_an_unresolved_append() {
    let s = started();
    let guard = s
        .core
        .begin_authenticated_reproof()
        .expect("a settled core");
    let shared = shared_for(&s, 0);
    s.platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Network]);
    let appended = s.platform.sim().run(send_append(
        &shared,
        &s.producer,
        &[SeatEvent {
            seq: 0,
            producer_engine_seq: None,
            kind: SeatEventKind::Facet {
                key: "draft".to_owned(),
                value: json!(1),
            },
        }],
        RetryPolicy::WEB,
    ));
    assert!(matches!(appended, Err(AppendFailure::Unknown(_))));
    let sent = s.platform.transport.calls.borrow().len();

    let outcome = s.platform.sim().run(reproof_seat(
        &shared,
        guard,
        &s.producer,
        &s.world.request,
        0,
        &s.world.after,
        BrowserViewport::default(),
        CoreConfig::default(),
    ));
    let message = refusal(outcome);
    assert!(message.contains("unresolved append"), "{message}");
    assert_eq!(s.platform.transport.calls.borrow().len(), sent);
    assert!(
        shared.delivery.borrow().pending.is_some(),
        "the append stays"
    );
}

/// If the membership is revoked before the proof is read, or while it is
/// read, the reproof ends with a refusal: it opens no stream in the first
/// case, and in neither case sends any seat mutation or hands anything back.
#[test]
fn a_revoked_owner_publishes_and_stages_nothing() {
    let s = started();
    let guard = s
        .core
        .begin_authenticated_reproof()
        .expect("a settled core");
    let shared = shared_for(&s, 0);
    let revoked = session_reply(&s.world.before);
    let sent_before = s.platform.transport.calls.borrow().len();
    let mutations = |s: &Started| {
        count(&s.platform, "open:claim")
            + count(&s.platform, "open:observe")
            + count(&s.platform, "close")
            + count(&s.platform, "append")
    };
    let mutations_before = mutations(&s);

    // Revoked before the first read.
    s.platform
        .transport
        .script(SESSION_CURRENT_PATH, vec![revoked.clone()]);
    let outcome = s.platform.sim().run(reproof_seat(
        &shared,
        guard.clone(),
        &s.producer,
        &s.world.request,
        0,
        &s.world.after,
        BrowserViewport::default(),
        CoreConfig::default(),
    ));
    let message = refusal(outcome);
    assert!(message.contains("membership changed"), "{message}");
    assert_eq!(count(&s.platform, "history"), 1, "no new stream was opened");

    // Revoked while the proof was read: the proof is read, then discarded.
    s.platform.transport.script(
        SESSION_CURRENT_PATH,
        vec![session_reply(&s.world.after), revoked],
    );
    s.world.proof(&s.platform, true);
    let outcome = s.platform.sim().run(reproof_seat(
        &shared,
        guard,
        &s.producer,
        &s.world.request,
        0,
        &s.world.after,
        BrowserViewport::default(),
        CoreConfig::default(),
    ));
    let message = refusal(outcome);
    assert!(message.contains("authority changed"), "{message}");
    assert_eq!(count(&s.platform, "history"), 2);
    assert_eq!(mutations(&s), mutations_before);
    assert!(s.platform.transport.calls.borrow().len() > sent_before);
}

/// The proof is made against the original claim's own frontier, not the
/// writer's current one: with the writer well past its claim, a stream that
/// retains the claim is accepted, and the same stream is refused when it is
/// compared with the current frontier instead. The delivery state is not
/// touched either way.
#[test]
fn the_original_claim_witness_is_kept_apart_from_the_frontier() {
    let s = started();
    let guard = s
        .core
        .begin_authenticated_reproof()
        .expect("a settled core");
    let shared = shared_for(&s, 7);
    s.platform
        .transport
        .script(SESSION_CURRENT_PATH, vec![session_reply(&s.world.after)]);

    s.world.proof(&s.platform, true);
    let output = s
        .platform
        .sim()
        .run(reproof_seat(
            &shared,
            guard.clone(),
            &s.producer,
            &s.world.request,
            0,
            &s.world.after,
            BrowserViewport::default(),
            CoreConfig::default(),
        ))
        .unwrap_or_else(|error| panic!("the claim witness is accepted: {error}"));
    assert_eq!(
        output.completion,
        HistoryCompletion {
            tip_hash: hash(2),
            thread_seq: 2,
            event_count: 2,
        }
    );
    assert_eq!(shared.delivery.borrow().next_engine_seq, 7);
    assert!(shared.is_current(&s.producer));
    drop(output);

    s.world.proof(&s.platform, true);
    let outcome = s.platform.sim().run(reproof_seat(
        &shared,
        guard,
        &s.producer,
        &s.world.request,
        7,
        &s.world.after,
        BrowserViewport::default(),
        CoreConfig::default(),
    ));
    let message = refusal(outcome);
    assert!(
        message.contains("does not retain the acknowledged producer claim"),
        "{message}"
    );
    assert_eq!(shared.delivery.borrow().next_engine_seq, 7);
}
