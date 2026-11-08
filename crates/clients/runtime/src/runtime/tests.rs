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
use crate::sim::{FakePlatform, Reply};

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
