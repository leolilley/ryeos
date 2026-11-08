//! Flow tests that need the runtime's private state: which producer the
//! runtime treats as its active seat, and which stream tags are current. They
//! run on the deterministic platform (virtual clock, scripted transport).

use std::rc::Rc;

use ryeos_client_base::ui::{
    BrowserSession, BrowserViewport, RyeOsCore, UiSeatControlPredecessor, UiSeatControlRequest,
    UiSeatProducerClaim,
};
use serde_json::json;

use super::*;
use crate::api::SEAT_TOUCH_PATH;
use crate::seat::LiveSeatHandoff;
use crate::sim::{FakePlatform, Reply, open_history};

fn producer(incarnation: char) -> UiPresentationProducer {
    UiPresentationProducer {
        presentation_id: "presentation-1".to_owned(),
        thread_id: "seat-thread".to_owned(),
        producer_incarnation: incarnation.to_string().repeat(64),
    }
}

fn active_seat(producer: &UiPresentationProducer) -> ActiveSeat {
    ActiveSeat {
        producer: producer.clone(),
        claim_request: UiSeatControlRequest::Claim {
            schema: 2,
            operation_id: String::new(),
            presentation_id: producer.presentation_id.clone(),
            thread_id: producer.thread_id.clone(),
            chain_root_id: "chain-1".to_owned(),
            predecessor: UiSeatControlPredecessor::Initialization {
                event_hash: "0".repeat(64),
            },
            intent_nonce: "nonce".to_owned(),
            claim: UiSeatProducerClaim::Fresh,
        },
        claim_next_engine_seq: 0,
    }
}

fn runtime(platform: &Rc<FakePlatform>) -> ClientRuntime<FakePlatform> {
    ClientRuntime::new(platform.clone(), Rc::new(|| {}), RetryPolicy::WEB)
}

fn core() -> RyeOsCore {
    RyeOsCore::new(
        BrowserSession::default(),
        "presentation-1",
        BrowserViewport::default(),
        0,
    )
}

/// While a replacement is being folded the shared producer is already the
/// successor's, but the runtime's active seat is still the predecessor's:
/// presence must not be renewed under the successor from the old core.
#[test]
fn presence_is_renewed_only_for_the_active_seats_own_producer() {
    let platform = FakePlatform::new();
    platform
        .transport
        .script(SEAT_TOUCH_PATH, vec![Reply::Network]);
    platform.clock.set_now(TOUCH_INTERVAL_MS + 1);
    let (old, successor) = (producer('a'), producer('b'));

    let mut runtime = runtime(&platform);
    runtime.seat = Some(active_seat(&old));
    *runtime.shared.producer.borrow_mut() = Some(successor.clone());
    runtime.maybe_touch();
    assert!(!runtime.touch_inflight, "a foreign producer must not touch");
    assert_eq!(platform.spawner.live(), 0);
    assert!(platform.transport.calls_to(SEAT_TOUCH_PATH).is_empty());

    // The same state with the matching producer does touch.
    *runtime.shared.producer.borrow_mut() = Some(old);
    runtime.maybe_touch();
    assert!(runtime.touch_inflight);
    platform.sim().run_for(10);
    assert_eq!(platform.transport.calls_to(SEAT_TOUCH_PATH).len(), 1);
}

/// Hint and tail messages that were queued before the close started belong to
/// streams that no longer run; the close drain must not fold them.
#[test]
fn begin_close_drops_queued_hint_and_tail_messages() {
    let platform = FakePlatform::new();
    let mut runtime = runtime(&platform);
    runtime.seat = Some(active_seat(&producer('a')));
    runtime.retag_streams();
    let mut core = core();

    // Control: a current hint message is folded.
    runtime.inbox.push(
        runtime.hints_tag.clone(),
        InboxPayload::Hint(json!({"kind": "threads"})),
    );
    assert!(runtime.drain(&mut core).changed);

    runtime.inbox.push(
        runtime.hints_tag.clone(),
        InboxPayload::Hint(json!({"kind": "threads"})),
    );
    runtime.inbox.push(
        runtime.hints_tag.clone(),
        InboxPayload::DaemonEvent(json!({"event_type": "x"})),
    );
    runtime.begin_close();
    let drained = runtime.drain(&mut core);
    assert!(!drained.changed, "queued hints were folded during close");
    assert!(drained.effects.is_empty());
}

fn handoff_for(
    platform: &Rc<FakePlatform>,
    producer: &UiPresentationProducer,
) -> LiveSeatHandoff<Frames<FakePlatform>> {
    LiveSeatHandoff {
        producer: producer.clone(),
        history: open_history(platform),
        claim_request: active_seat(producer).claim_request,
        claim_next_engine_seq: 7,
    }
}

/// A runtime that is mid-replacement: still tracking `old`, with `successor`
/// already the shared producer.
fn replacing(
    platform: &Rc<FakePlatform>,
    old: &UiPresentationProducer,
    successor: &UiPresentationProducer,
) -> ClientRuntime<FakePlatform> {
    let mut runtime = runtime(platform);
    runtime.seat = Some(active_seat(old));
    runtime.retag_streams();
    *runtime.shared.producer.borrow_mut() = Some(successor.clone());
    runtime
}

/// After an in-place replacement the successor is the runtime's active seat,
/// the claimed stream is adopted, and everything still tagged for the
/// predecessor is refused.
#[test]
fn turnover_makes_the_successor_current_and_drops_the_predecessors_messages() {
    let platform = FakePlatform::new();
    let (old, successor) = (producer('a'), producer('b'));
    let mut runtime = replacing(&platform, &old, &successor);
    let old_seat_tag = runtime.seat_tag.clone();
    let old_hints_tag = runtime.hints_tag.clone();
    assert!(runtime.inbox.accepts(Stream::Seat, &old_seat_tag));
    assert!(
        runtime
            .shared
            .reserve_handoff(handoff_for(&platform, &successor))
            .is_ok()
    );

    let mut core = core();
    let mut follow = Vec::new();
    let mut out = Drained::default();
    runtime.turn_over(&mut core, &mut follow, &mut out);

    assert!(out.stop.is_none(), "{:?}", out.stop);
    let seat = runtime.seat.as_ref().expect("the successor is the seat");
    assert_eq!(seat.producer, successor);
    assert_eq!(seat.claim_next_engine_seq, 7);
    assert!(
        runtime.shared.take_handoff().is_none(),
        "the handoff was consumed"
    );
    assert!(
        runtime.live_task.is_some(),
        "the claimed stream is being read"
    );
    assert!(runtime.shared.is_current(&successor));
    assert!(!runtime.shared.is_current(&old));

    // Streams carry the successor's tags; the predecessor's are stale.
    assert_eq!(
        runtime.seat_tag.producer.as_deref(),
        Some(successor.producer_incarnation.as_str())
    );
    assert!(!runtime.inbox.accepts(Stream::Seat, &old_seat_tag));
    assert!(!runtime.inbox.accepts(Stream::Hints, &old_hints_tag));
    assert!(
        !runtime
            .inbox
            .push(old_seat_tag, InboxPayload::SeatAck(json!({})))
    );
    assert!(
        runtime
            .inbox
            .push(runtime.seat_tag.clone(), InboxPayload::SeatAck(json!({})))
    );
}

/// A handoff that belongs to some other producer is never adopted: the
/// current producer is fenced and the runtime stops.
#[test]
fn turnover_refuses_a_handoff_for_another_producer() {
    let platform = FakePlatform::new();
    let (old, successor) = (producer('a'), producer('b'));
    let mut runtime = replacing(&platform, &old, &successor);
    assert!(
        runtime
            .shared
            .reserve_handoff(handoff_for(&platform, &producer('c')))
            .is_ok()
    );

    let mut out = Drained::default();
    runtime.turn_over(&mut core(), &mut Vec::new(), &mut out);

    assert!(
        matches!(&out.stop, Some(StopReason::Fatal(message)) if message.contains("another producer")),
        "{:?}",
        out.stop
    );
    assert!(runtime.shared.current_producer().is_none());
    assert!(runtime.shared.delivery.borrow().blocked);
    assert_eq!(
        runtime.seat.as_ref().map(|seat| seat.producer.clone()),
        Some(old),
        "the predecessor's seat was not replaced"
    );
}

/// A replacement with no claimed stream waiting, or with no producer at all,
/// cannot turn over.
#[test]
fn turnover_needs_both_a_producer_and_a_handoff() {
    let platform = FakePlatform::new();
    let (old, successor) = (producer('a'), producer('b'));

    let mut runtime = replacing(&platform, &old, &successor);
    let mut out = Drained::default();
    runtime.turn_over(&mut core(), &mut Vec::new(), &mut out);
    assert!(
        matches!(&out.stop, Some(StopReason::Fatal(message)) if message.contains("not handed off")),
        "{:?}",
        out.stop
    );
    assert!(runtime.shared.current_producer().is_none());
    assert!(runtime.shared.delivery.borrow().blocked);

    let mut runtime = replacing(&platform, &old, &successor);
    *runtime.shared.producer.borrow_mut() = None;
    let mut out = Drained::default();
    runtime.turn_over(&mut core(), &mut Vec::new(), &mut out);
    assert!(
        matches!(&out.stop, Some(StopReason::Fatal(message)) if message.contains("not ready")),
        "{:?}",
        out.stop
    );
}

/// A claimed stream that effects produced without the reducer replacing the
/// session is not left live: it is fenced and the runtime stops.
#[test]
fn an_unadopted_handoff_is_fenced_when_the_session_did_not_change() {
    let platform = FakePlatform::new();
    let (old, successor) = (producer('a'), producer('b'));
    let mut runtime = replacing(&platform, &old, &successor);
    assert!(
        runtime
            .shared
            .reserve_handoff(handoff_for(&platform, &successor))
            .is_ok()
    );

    let mut out = Drained::default();
    runtime.fold_effect_results(&mut core(), Vec::new(), &mut out);

    assert!(!out.turned_over);
    assert!(
        matches!(out.stop, Some(StopReason::Fatal(_))),
        "{:?}",
        out.stop
    );
    assert!(runtime.shared.take_handoff().is_none());
    assert!(runtime.shared.current_producer().is_none());
    assert!(runtime.shared.delivery.borrow().blocked);
}
