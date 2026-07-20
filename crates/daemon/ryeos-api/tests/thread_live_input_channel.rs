//! Cross-layer integration for the operator live-input data channel.
//!
//! Proves the daemon-side path the unit tests only cover in isolation, driving
//! the real services (LiveInputQueue + ThreadLifecycleService + StateStore) with
//! no mocks:
//!
//!   enqueue → drain + persist through the running-guarded append
//!   (`append_thread_events`) → durable `cognition_in` in the braid →
//!   finalize CLOSES the queue → a post-terminal poll appends nothing
//!   (running-guard) and the queue rejects new input.
//!
//! It bypasses the provider/runtime turn loop on purpose: the loop's folding
//! (steer/interrupt) is unit-tested in the directive runtime, and the full
//! behavioral loop is validated by the post-deploy smoke test (it needs a live
//! provider + a running-window, which isn't deterministically reproducible here).

mod test_state;

use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::live_input_queue::EnqueueOutcome;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{NewEventRecord, NewThreadRecord};
use ryeos_app::thread_lifecycle::ThreadFinalizeParams;
use ryeos_state::objects::{LiveInput, LiveInputIntent};

fn captured_policy() -> ryeos_state::objects::CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    ryeos_state::objects::CapturedThreadHistoryPolicy {
        retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
        canonical_item_ref: "directive:test/live".to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
            node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
        },
    }
}

/// Create a `directive`-kind thread directly through the state store (bypassing
/// the lifecycle kind-profile check the empty test registry would fail) and mark
/// it running.
fn create_running_directive(state: &AppState, thread_id: &str, requested_by: &str) {
    let rec = NewThreadRecord {
        thread_id: thread_id.to_string(),
        chain_root_id: thread_id.to_string(),
        kind: "directive".to_string(),
        item_ref: "directive:test/live".to_string(),
        executor_ref: "test/executor".to_string(),
        launch_mode: "inline".to_string(),
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        upstream_thread_id: None,
        requested_by: Some(requested_by.to_string()),
        project_root: None,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: Some(captured_policy()),
    };
    state
        .state_store
        .create_thread_for_test(&rec)
        .expect("create thread");
    state
        .state_store
        .mark_thread_running(thread_id, None)
        .expect("mark running");
}

/// The indexed `cognition_in` batch the poll handler builds from a drained input.
fn cognition_in_batch(content: &str) -> Vec<NewEventRecord> {
    vec![NewEventRecord {
        event_type: "cognition_in".to_string(),
        storage_class: "indexed".to_string(),
        payload: serde_json::json!({ "content": content }),
    }]
}

/// Contents of every `cognition_in` in a thread's braid, in order.
fn cognition_in_contents(state: &AppState, thread_id: &str) -> Vec<String> {
    let res = state
        .events
        .replay(&EventReplayParams {
            chain_root_id: None,
            thread_id: Some(thread_id.to_string()),
            after_chain_seq: None,
            limit: 200,
        })
        .expect("replay");
    res.events
        .into_iter()
        .filter(|e| e.event_type == "cognition_in")
        .filter_map(|e| {
            e.payload
                .get("content")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect()
}

/// Replicate the daemon's `handle_poll_input` persistence step against the real
/// services: drain → append through the running-guard → ack/release.
/// Returns whether the append persisted (Some => still running).
fn poll_and_persist(state: &AppState, thread_id: &str) -> bool {
    let drained = state.live_input.drain(thread_id);
    if drained.is_empty() {
        return false;
    }
    let events: Vec<NewEventRecord> = drained
        .iter()
        .flat_map(|s| cognition_in_batch(&s.content))
        .collect();
    let persisted = state
        .threads
        .append_thread_events(thread_id, thread_id, &events)
        .expect("append_thread_events");
    match persisted {
        Some(_) => {
            state.live_input.ack_drained(thread_id, drained.len());
            true
        }
        None => {
            // Not running: discard (release the reservation). Never a
            // cognition_in after terminal.
            state.live_input.ack_drained(thread_id, drained.len());
            false
        }
    }
}

#[tokio::test]
async fn live_input_persists_into_running_thread_then_finalize_closes_the_queue() {
    let (_tmp, state) = test_state::build_test_state();
    // The composition root wires this in production; do it here so finalize can
    // close the thread's queue.
    state.threads.set_live_input_queue(state.live_input.clone());

    let tid = "T-live-1";
    create_running_directive(&state, tid, "fp:owner");

    // 1. Operator enqueues a live input for the running thread.
    assert!(matches!(
        state
            .live_input
            .enqueue(tid, LiveInput::new("steer me", LiveInputIntent::Steer)),
        EnqueueOutcome::Accepted { .. }
    ));

    // 2. The runtime polls: the input is persisted as a durable cognition_in.
    assert!(poll_and_persist(&state, tid), "running → persisted");
    assert_eq!(
        cognition_in_contents(&state, tid),
        vec!["steer me".to_string()]
    );

    // 3. Finalize (any terminal status) closes the queue.
    let params: ThreadFinalizeParams =
        serde_json::from_value(serde_json::json!({ "thread_id": tid, "status": "completed" }))
            .expect("finalize params");
    state.threads.finalize_thread(&params).expect("finalize");
    assert!(state.live_input.is_closed(tid), "finalize closes the queue");

    // 4. Post-terminal: the queue refuses new input...
    assert_eq!(
        state
            .live_input
            .enqueue(tid, LiveInput::new("late", LiveInputIntent::Steer)),
        EnqueueOutcome::Closed
    );
    // ...and even a direct append is a no-op under the running-guard — no
    // cognition_in is ever added after terminal.
    let after = state
        .threads
        .append_thread_events(tid, tid, &cognition_in_batch("late"))
        .expect("append after terminal");
    assert!(
        after.is_none(),
        "terminal thread → running-guard rejects append"
    );
    assert_eq!(
        cognition_in_contents(&state, tid),
        vec!["steer me".to_string()],
        "no cognition_in leaked in after terminal"
    );
}

#[tokio::test]
async fn poll_before_terminal_persists_but_after_terminal_discards() {
    // The enqueue/finalize race, resolved deterministically: an input still
    // queued when the thread finalizes is dropped (queue closed), and the
    // running-guard means it could never have been appended post-terminal anyway.
    let (_tmp, state) = test_state::build_test_state();
    state.threads.set_live_input_queue(state.live_input.clone());

    let tid = "T-live-2";
    create_running_directive(&state, tid, "fp:owner");

    // Two inputs enqueued while running.
    state
        .live_input
        .enqueue(tid, LiveInput::new("first", LiveInputIntent::Steer));
    state
        .live_input
        .enqueue(tid, LiveInput::new("second", LiveInputIntent::Interrupt));

    // Finalize before the runtime ever polls.
    let params: ThreadFinalizeParams =
        serde_json::from_value(serde_json::json!({ "thread_id": tid, "status": "failed" }))
            .expect("finalize params");
    state.threads.finalize_thread(&params).expect("finalize");

    // The queued inputs were cleared at close; a poll now yields nothing and
    // nothing is persisted.
    assert!(
        !poll_and_persist(&state, tid),
        "closed queue → nothing to persist"
    );
    assert!(
        cognition_in_contents(&state, tid).is_empty(),
        "no cognition_in persisted for a thread that finalized before its inputs were polled"
    );
}
