//! Flows around the runtime's retry and close behaviour, on the deterministic
//! platform (virtual clock, scripted transport; nothing here sleeps in real
//! time). The flows that need the runtime's private state (producer gating of
//! presence, queued hint and tail messages at close) live beside it in
//! `src/runtime/tests.rs`.

mod common;

use std::cell::Cell;
use std::rc::Rc;

use common::sim::{FakePlatform, Reply, poll_once};
use ryeos_client_base::ui::{
    BrowserSession, BrowserViewport, RyeOsCore, RyeOsEffect, RyeOsEffectKind,
    UiBindingAttachmentCoordinate,
};
use ryeos_client_runtime::ClientRuntime;
use ryeos_client_runtime::api::SEAT_APPEND_PATH;
use ryeos_client_runtime::effects::{AdapterFuture, AdapterHandler};
use ryeos_client_runtime::history::SeatCoordinate;
use ryeos_client_runtime::inbox::WakeFn;
use ryeos_client_runtime::seat::{
    AppendFailure, AuthContext, ExpectedAck, RetryPolicy, post_append_exactly,
};

fn auth_context() -> AuthContext {
    AuthContext {
        session_id: "session".to_owned(),
        root: UiBindingAttachmentCoordinate {
            binding_attachment_id: "root".into(),
            binding_generation: 1,
            binding_digest: "digest".into(),
        },
        coordinate: SeatCoordinate {
            presentation_id: "presentation".to_owned(),
            thread_id: "thread".to_owned(),
            chain_root_id: "chain".to_owned(),
        },
    }
}

fn expected_ack() -> ExpectedAck {
    ExpectedAck {
        producer: "p".repeat(64),
        operation_id: "operation".to_owned(),
        first_engine_seq: 0,
        last_engine_seq: 0,
        event_count: 1,
        payload_digest: "d".repeat(64),
    }
}

/// An append whose daemon never answers: every attempt is a network failure.
fn unreachable_daemon() -> Rc<FakePlatform> {
    let platform = FakePlatform::new();
    platform
        .transport
        .script(SEAT_APPEND_PATH, vec![Reply::Network]);
    platform
}

/// The start-time reconcile runs the bounded policy: it gives up after its
/// attempts with an unknown outcome instead of retrying forever, on the
/// schedule 100, 200, 400, 800 ms.
#[test]
fn the_bounded_policy_gives_up_instead_of_hanging() {
    let platform = unreachable_daemon();
    let outcome = platform.sim().run(post_append_exactly(
        &*platform,
        &auth_context(),
        "{}",
        &expected_ack(),
        &[],
        RetryPolicy::WEB,
        &|| true,
    ));
    assert!(matches!(outcome, Err(AppendFailure::Unknown(_))));
    assert_eq!(platform.transport.calls_to(SEAT_APPEND_PATH).len(), 5);
    assert_eq!(*platform.clock.sleeps.borrow(), vec![100, 200, 400, 800]);
}

/// An unbounded retry stops as soon as its delivery is no longer current
/// (its producer was replaced, or the delivery was fenced), without a further
/// request for bytes nobody is waiting for.
#[test]
fn a_retry_stops_when_its_producer_is_no_longer_current() {
    let platform = unreachable_daemon();
    let checks = Cell::new(0_u32);
    // Current for the first two checks, replaced from the third on.
    let still_current = || {
        checks.set(checks.get() + 1);
        checks.get() <= 2
    };
    let outcome = platform.sim().run(post_append_exactly(
        &*platform,
        &auth_context(),
        "{}",
        &expected_ack(),
        &[],
        RetryPolicy::TERMINAL,
        &still_current,
    ));
    assert!(matches!(outcome, Err(AppendFailure::Unknown(_))));
    assert_eq!(platform.transport.calls_to(SEAT_APPEND_PATH).len(), 2);
    assert_eq!(*platform.clock.sleeps.borrow(), vec![100]);
}

/// An effect task that ends without delivering (a panic, a drop) is reaped
/// when the host asks whether it may close: its effect gets a failure result,
/// the next drain folds it, and nothing is left waiting for it.
#[test]
fn a_panicked_effect_task_yields_a_failure_result() {
    let platform = FakePlatform::new();
    let woken = Rc::new(Cell::new(0_u32));
    let wake: WakeFn = {
        let woken = woken.clone();
        Rc::new(move || woken.set(woken.get() + 1))
    };
    // The host's handler never answers, so the effect stays in flight.
    let adapter: AdapterHandler =
        Rc::new(|_effect: RyeOsEffect| -> AdapterFuture { Box::pin(std::future::pending()) });
    let mut runtime =
        ClientRuntime::new(platform.clone(), wake, RetryPolicy::WEB).with_adapter(adapter);
    let mut core = RyeOsCore::new(
        BrowserSession::default(),
        "presentation",
        BrowserViewport::default(),
        0,
    );

    runtime.submit(vec![RyeOsEffect {
        id: 1,
        kind: RyeOsEffectKind::SetLocationHash {
            hash: "#x".to_owned(),
        },
    }]);
    platform.sim().run_for(10);
    assert!(
        poll_once(runtime.idle()).is_none(),
        "the effect is still in flight"
    );

    platform.spawner.abort_all();
    // Reaping happens here; the result is delivered by the next drain.
    assert!(runtime.may_close(&core));
    assert!(woken.get() > 0, "the host is woken to fold the failure");
    let drained = runtime.drain(&mut core);
    assert!(drained.changed, "the failure result was folded");
    assert!(
        poll_once(runtime.idle()).is_some(),
        "no effect is left waiting"
    );
}
