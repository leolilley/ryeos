//! The single handoff slot of a seat: a claimed live stream waits there for
//! the reducer to adopt it, and only one can wait at a time.

mod common;

use common::sim::{FakePlatform, open_history};
use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::{UiSeatControlPredecessor, UiSeatControlRequest, UiSeatProducerClaim};
use ryeos_client_runtime::seat::{LiveSeatHandoff, SeatShared};
use std::rc::Rc;

fn producer(incarnation: char) -> UiPresentationProducer {
    UiPresentationProducer {
        presentation_id: "presentation-1".to_owned(),
        thread_id: "seat-thread".to_owned(),
        producer_incarnation: incarnation.to_string().repeat(64),
    }
}

fn handoff(
    platform: &Rc<FakePlatform>,
    incarnation: char,
    next_engine_seq: u64,
) -> LiveSeatHandoff<common::sim::ChunkSource> {
    let producer = producer(incarnation);
    LiveSeatHandoff {
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
        producer,
        history: open_history(platform),
        claim_next_engine_seq: next_engine_seq,
    }
}

/// A second reservation is refused while one waits, and the refused handoff
/// comes back intact so its owner can fence it. Taking the first frees the
/// slot again.
#[test]
fn the_handoff_slot_holds_one_claimed_stream_until_it_is_taken() {
    let platform = FakePlatform::new();
    let shared = SeatShared::new(platform.clone());
    assert!(shared.take_handoff().is_none(), "the slot starts empty");

    assert!(shared.reserve_handoff(handoff(&platform, 'a', 3)).is_ok());
    let refused = shared
        .reserve_handoff(handoff(&platform, 'b', 9))
        .expect_err("a second reservation is refused");
    assert_eq!(refused.producer, producer('b'));
    assert_eq!(refused.claim_next_engine_seq, 9);

    // The refusal left the first handoff untouched.
    let held = shared.take_handoff().expect("the first handoff is held");
    assert_eq!(held.producer, producer('a'));
    assert_eq!(held.claim_next_engine_seq, 3);
    assert!(shared.take_handoff().is_none(), "a handoff is taken once");

    assert!(shared.reserve_handoff(refused).is_ok());
    let again = shared.take_handoff().expect("the slot accepts again");
    assert_eq!(again.producer, producer('b'));
}
