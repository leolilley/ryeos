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
        presentation_id: "11111111-1111-4111-8111-111111111111".to_owned(),
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
        "11111111-1111-4111-8111-111111111111",
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

/// Delivery-state flows of a runtime that holds a live seat: what a lost,
/// refused or overlapping seat task leaves behind, and what close does and
/// does not publish.
mod seat_delivery {
    use super::*;
    use crate::api::{SEAT_APPEND_PATH, SEAT_CLOSE_PATH, SEAT_OPEN_PATH, SEAT_TOUCH_PATH};
    use crate::platform::{Clock, Spawner, Store};
    use ryeos_client_base::ui::SeatEventKind;

    const KEY: &str = "retained-key";

    fn ready(
        platform: &Rc<FakePlatform>,
        who: &UiPresentationProducer,
    ) -> ClientRuntime<FakePlatform> {
        let mut runtime = runtime(platform);
        runtime.seat = Some(active_seat(who));
        runtime.retag_streams();
        *runtime.shared.producer.borrow_mut() = Some(who.clone());
        runtime.shared.delivery.borrow_mut().reset_for(
            SeatCoordinate {
                presentation_id: "11111111-1111-4111-8111-111111111111".to_owned(),
                thread_id: "seat-thread".to_owned(),
                chain_root_id: "chain-1".to_owned(),
            },
            "session",
            UiBindingAttachmentCoordinate {
                binding_attachment_id: "root".to_owned(),
                binding_generation: 1,
                binding_digest: "digest".to_owned(),
            },
            KEY.to_owned(),
            0,
            1_000_000,
        );
        runtime
    }

    fn facet(seq: u64) -> SeatEvent {
        SeatEvent {
            seq,
            producer_engine_seq: None,
            kind: SeatEventKind::Facet {
                key: "draft".to_owned(),
                value: json!(1),
            },
        }
    }

    fn touched() -> Reply {
        Reply::Ok(
            json!({
                "presentation_id": "11111111-1111-4111-8111-111111111111",
                "thread_id": "seat-thread",
                "touched": true,
            })
            .to_string(),
        )
    }

    /// Start an append the way the runtime's writer does: one task that
    /// sends the captured prefix and reports through the seat stream.
    fn start_append(
        platform: &Rc<FakePlatform>,
        runtime: &mut ClientRuntime<FakePlatform>,
        who: &UiPresentationProducer,
        events: Vec<SeatEvent>,
    ) {
        let shared = runtime.shared.clone();
        let inbox = runtime.inbox.clone();
        let tag = runtime.seat_tag.clone();
        let who = who.clone();
        runtime.append_inflight = true;
        let task = platform.spawner.spawn(async move {
            let result = send_append(&shared, &who, &events, RetryPolicy::WEB).await;
            inbox.push(
                tag,
                InboxPayload::Task(TaskResult::new(SeatTask::<FakePlatform>::Append {
                    producer: who,
                    events,
                    upto: 1,
                    result,
                })),
            );
        });
        runtime.io_tasks.push(task);
    }

    /// Renewal ticks that overlap send one request: the in-flight touch holds
    /// the slot until its answer is folded, and the next renewal waits a full
    /// interval after that.
    #[test]
    fn overlapping_presence_ticks_send_one_touch() {
        let platform = FakePlatform::new();
        platform.transport.script(SEAT_TOUCH_PATH, vec![touched()]);
        platform.clock.set_now(TOUCH_INTERVAL_MS + 1);
        let who = producer('a');
        let mut runtime = ready(&platform, &who);

        runtime.maybe_touch();
        runtime.maybe_touch();
        runtime.maybe_touch();
        assert!(runtime.touch_inflight);
        platform.sim().run_for(10);
        let sent = platform.transport.calls_to(SEAT_TOUCH_PATH);
        assert_eq!(sent.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(
            body,
            json!({
                "presentation_id": "11111111-1111-4111-8111-111111111111",
                "thread_id": "seat-thread",
                "producer_incarnation": who.producer_incarnation,
            })
        );

        let mut core = core();
        let drained = runtime.drain(&mut core);
        assert!(drained.stop.is_none(), "{:?}", drained.stop);
        assert!(!runtime.touch_inflight);
        assert!(drained.seat.ready);
        runtime.maybe_touch();
        assert!(
            !runtime.touch_inflight,
            "the next renewal is a full interval away"
        );

        platform.clock.set_now(2 * TOUCH_INTERVAL_MS + 2);
        runtime.maybe_touch();
        assert!(runtime.touch_inflight);
        platform.sim().run_for(10);
        assert_eq!(platform.transport.calls_to(SEAT_TOUCH_PATH).len(), 2);
    }

    /// A definite refusal of presence fences the producer that was refused
    /// and reports `writer_refused` once. A late refusal that belongs to a
    /// predecessor neither reports nor fences its successor.
    #[test]
    fn a_refused_touch_fences_only_its_own_producer_and_reports_once() {
        let platform = FakePlatform::new();
        platform
            .transport
            .script(SEAT_TOUCH_PATH, vec![Reply::Status(403)]);
        platform.clock.set_now(TOUCH_INTERVAL_MS + 1);
        let who = producer('a');
        let mut runtime = ready(&platform, &who);
        runtime.maybe_touch();
        platform.sim().run_for(10);

        let mut core = core();
        let drained = runtime.drain(&mut core);
        let stop = drained.stop.clone().expect("the refusal is reported");
        let StopReason::Unavailable(unavailable) = &stop else {
            panic!("{stop:?}");
        };
        assert_eq!(unavailable.kind(), "writer_refused");
        assert!(runtime.shared.current_producer().is_none());
        assert!(runtime.shared.delivery.borrow().blocked);
        assert!(!drained.seat.ready);
        assert!(drained.seat.blocked);
        assert!(runtime.drain(&mut core).stop.is_none(), "reported once");
        assert_eq!(platform.transport.calls_to(SEAT_TOUCH_PATH).len(), 1);

        // A refusal that arrives for a predecessor after a replacement.
        let (old, successor) = (producer('a'), producer('b'));
        let mut runtime = ready(&platform, &old);
        *runtime.shared.producer.borrow_mut() = Some(successor.clone());
        runtime.touch_inflight = true;
        let mut out = Drained::default();
        runtime.fold_task(
            &mut core,
            &TaskResult::new(SeatTask::<FakePlatform>::Touch {
                producer: old,
                outcome: TouchOutcome::Refused,
            }),
            &mut out,
        );
        assert!(out.stop.is_none(), "{:?}", out.stop);
        assert!(!runtime.touch_inflight);
        assert_eq!(runtime.shared.current_producer(), Some(successor));
        assert!(!runtime.shared.delivery.borrow().blocked);
    }

    /// A mechanical close gives nothing back: it sends no control request,
    /// keeps the writer as it was, and a refusal that arrives afterwards
    /// publishes nothing. After a relinquish a late refusal is ignored too.
    #[test]
    fn a_late_refusal_after_close_publishes_nothing_and_close_sends_no_control_request() {
        let platform = FakePlatform::new();
        platform
            .transport
            .script(SEAT_TOUCH_PATH, vec![Reply::Status(403)]);
        platform.clock.set_now(TOUCH_INTERVAL_MS + 1);
        let who = producer('a');
        let mut runtime = ready(&platform, &who);
        runtime.maybe_touch();
        runtime.detach();
        platform.sim().run_for(10);
        assert_eq!(platform.transport.calls_to(SEAT_TOUCH_PATH).len(), 1);

        let mut core = core();
        let drained = runtime.drain(&mut core);
        assert!(drained.stop.is_none(), "{:?}", drained.stop);
        assert!(!drained.changed);
        assert!(platform.transport.calls_to(SEAT_OPEN_PATH).is_empty());
        assert!(platform.transport.calls_to(SEAT_CLOSE_PATH).is_empty());
        assert_eq!(runtime.shared.current_producer(), Some(who.clone()));
        assert!(!runtime.shared.delivery.borrow().blocked);

        // The writer was already given back when the refusal is folded.
        *runtime.shared.producer.borrow_mut() = None;
        let mut out = Drained::default();
        runtime.fold_task(
            &mut core,
            &TaskResult::new(SeatTask::<FakePlatform>::Touch {
                producer: who,
                outcome: TouchOutcome::Refused,
            }),
            &mut out,
        );
        assert!(out.stop.is_none(), "{:?}", out.stop);
    }

    /// An append whose outcome is unknown is reported as `append_unknown`
    /// exactly once. The writer is fenced, the retained request is exactly
    /// what was sent, and nothing more is sent: no renewal, no claim.
    #[test]
    fn an_unknown_append_is_reported_once_and_blocks_further_mutation() {
        let platform = FakePlatform::new();
        platform
            .transport
            .script(SEAT_APPEND_PATH, vec![Reply::Network]);
        let who = producer('a');
        let mut runtime = ready(&platform, &who);
        start_append(&platform, &mut runtime, &who, vec![facet(0)]);
        platform.sim().run_for(10_000);
        assert_eq!(platform.transport.calls_to(SEAT_APPEND_PATH).len(), 5);

        let mut core = core();
        let drained = runtime.drain(&mut core);
        assert!(!runtime.append_inflight);
        let stop = drained
            .stop
            .clone()
            .expect("the unknown outcome is reported");
        let StopReason::Unavailable(unavailable) = &stop else {
            panic!("{stop:?}");
        };
        assert_eq!(unavailable.kind(), "append_unknown");
        assert!(runtime.shared.current_producer().is_none());
        assert!(runtime.shared.delivery.borrow().blocked);
        assert!(drained.seat.blocked && !drained.seat.ready);

        let stored = platform.store.get(KEY).expect("the request stays retained");
        let record = retention::RetainedPendingAppend::parse(&stored).unwrap();
        assert_eq!(
            record.encoded_request,
            platform.transport.calls_to(SEAT_APPEND_PATH)[0]
        );
        assert_eq!(
            runtime
                .shared
                .delivery
                .borrow()
                .pending
                .as_ref()
                .map(|p| p.record.clone()),
            Some(record)
        );

        assert!(runtime.drain(&mut core).stop.is_none(), "reported once");
        platform
            .clock
            .set_now(platform.clock.now_ms() + 10 * TOUCH_INTERVAL_MS);
        runtime.after_mutation(&core);
        platform.sim().run_for(10);
        assert!(platform.transport.calls_to(SEAT_TOUCH_PATH).is_empty());
        assert!(platform.transport.calls_to(SEAT_OPEN_PATH).is_empty());
        assert_eq!(platform.transport.calls_to(SEAT_APPEND_PATH).len(), 5);
        assert_eq!(platform.store.get(KEY), Some(stored));
    }

    /// Closing while an append is admitted: the relinquish is refused before
    /// any request and does not start closing; a detach lets the append task
    /// finish on its own, and the retained request survives for recovery.
    #[test]
    fn close_during_an_admitted_append_keeps_the_retained_request() {
        let platform = FakePlatform::new();
        platform
            .transport
            .script(SEAT_APPEND_PATH, vec![Reply::Network]);
        let who = producer('a');
        let mut runtime = ready(&platform, &who);
        start_append(&platform, &mut runtime, &who, vec![facet(0)]);
        platform.sim().run_for(50);
        assert!(!platform.transport.calls_to(SEAT_APPEND_PATH).is_empty());
        let stored = platform
            .store
            .get(KEY)
            .expect("retained before the first send");
        let mut core = core();
        assert!(
            !runtime.may_close(&core),
            "an admitted append holds the close"
        );

        let outcome = platform.sim().run(runtime.relinquish());
        assert!(
            matches!(outcome, Err(RuntimeError::Fatal(_))),
            "{outcome:?}"
        );
        assert!(!runtime.closing);
        assert!(platform.transport.calls_to(SEAT_OPEN_PATH).is_empty());
        assert!(platform.transport.calls_to(SEAT_CLOSE_PATH).is_empty());
        assert_eq!(runtime.shared.current_producer(), Some(who));

        runtime.detach();
        platform.sim().run_for(10_000);
        assert_eq!(platform.store.get(KEY), Some(stored));
        assert!(runtime.shared.delivery.borrow().pending.is_some());
        let drained = runtime.drain(&mut core);
        assert!(drained.stop.is_none(), "{:?}", drained.stop);
        assert!(platform.transport.calls_to(SEAT_CLOSE_PATH).is_empty());
    }
}

/// The relinquish a page sends as it goes away: only for the current producer
/// with settled delivery and its own learned control predecessor, and then
/// exactly that producer's relinquish of that predecessor.
mod unload_relinquish {
    use super::*;

    fn ready(
        platform: &Rc<FakePlatform>,
        who: &UiPresentationProducer,
    ) -> ClientRuntime<FakePlatform> {
        let mut runtime = runtime(platform);
        runtime.seat = Some(active_seat(who));
        *runtime.shared.producer.borrow_mut() = Some(who.clone());
        runtime.shared.delivery.borrow_mut().reset_for(
            crate::history::SeatCoordinate {
                presentation_id: "11111111-1111-4111-8111-111111111111".to_owned(),
                thread_id: "seat-thread".to_owned(),
                chain_root_id: "chain-1".to_owned(),
            },
            "session",
            ryeos_client_base::ui::UiBindingAttachmentCoordinate {
                binding_attachment_id: "root".to_owned(),
                binding_generation: 1,
                binding_digest: "digest".to_owned(),
            },
            "key".to_owned(),
            0,
            1_000_000,
        );
        runtime
    }

    fn control() -> UiSeatControlPredecessor {
        UiSeatControlPredecessor::Control {
            event_hash: "e".repeat(64),
        }
    }

    #[test]
    fn nothing_is_sent_without_this_producers_own_settled_basis() {
        let platform = FakePlatform::new();
        let who = producer('a');
        let runtime = ready(&platform, &who);
        let hook = runtime.unload_relinquish();
        assert_eq!(hook().unwrap_err(), "its own control was not learned");

        *runtime.shared.relinquish_basis.borrow_mut() = Some((producer('b'), control()));
        assert_eq!(
            hook().unwrap_err(),
            "the learned control names another producer"
        );

        *runtime.shared.relinquish_basis.borrow_mut() = Some((who.clone(), control()));
        runtime.shared.delivery.borrow_mut().blocked = true;
        assert_eq!(hook().unwrap_err(), "delivery is blocked");
        runtime.shared.delivery.borrow_mut().blocked = false;

        let (path, body) = hook().expect("a settled producer with its basis gives the seat back");
        assert_eq!(path, crate::api::SEAT_CLOSE_PATH);
        let request: UiSeatControlRequest = serde_json::from_str(&body).unwrap();
        let UiSeatControlRequest::Relinquish {
            predecessor,
            producer_incarnation,
            presentation_id,
            ..
        } = &request
        else {
            panic!("{request:?}");
        };
        assert_eq!(predecessor, &control());
        assert_eq!(producer_incarnation, &who.producer_incarnation);
        assert_eq!(presentation_id, &who.presentation_id);
        request
            .validate()
            .expect("the relinquish is a valid control request");
    }
}
