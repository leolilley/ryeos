//! The daemon-written lineage event for a child dispatched through a runtime's
//! action callback: when it is owed, its payload, idempotency, and the window
//! in which the parent settles before it can be written.
use super::*;
use crate::runtime_db::RuntimeActionMode;

fn thread_record(thread_id: &str) -> NewThreadRecord {
    let hash = "a".repeat(64);
    NewThreadRecord {
        thread_id: thread_id.to_string(),
        chain_root_id: thread_id.to_string(),
        kind: "directive".to_string(),
        item_ref: format!("directive:test/{thread_id}"),
        executor_ref: "native:test".to_string(),
        launch_mode: "wait".to_string(),
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        upstream_thread_id: None,
        requested_by: Some("fp:test".to_string()),
        project_root: None,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: Some(ryeos_state::objects::CapturedThreadHistoryPolicy {
            retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
            canonical_item_ref: format!("directive:test/{thread_id}"),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(
                ),
            },
        }),
    }
    owner: None,
}

fn spawned_events(state: &crate::state::AppState, parent: &str) -> Vec<Value> {
    state
        .state_store
        .replay_events(parent, Some(parent), None, 64, 1024 * 1024)
        .expect("replay parent events")
        .events
        .into_iter()
        .filter(|event| event.event_type == "child_thread_spawned")
        .map(|event| event.payload)
        .collect()
}

/// A running parent, a fresh-root child linked to it, and the retained action
/// intent that a callback dispatch reserves before creating the child.
fn dispatched_child(state: &crate::state::AppState, parent: &str, child: &str, operation: &str) {
    dispatched_child_in_mode(state, parent, child, operation, RuntimeActionMode::Detached);
}

fn dispatched_child_in_mode(
    state: &crate::state::AppState,
    parent: &str,
    child: &str,
    operation: &str,
    mode: RuntimeActionMode,
) {
    let store = &state.state_store;
    store
        .create_thread_for_test(&thread_record(parent))
        .expect("parent thread");
    store
        .mark_thread_running(parent, None)
        .expect("start parent");
    store
        .reserve_runtime_action_intent(operation, parent, mode, &"b".repeat(64), child, None)
        .expect("reserve action intent");
    store
        .create_thread_for_test(&thread_record(child))
        .expect("child thread");
    assert_eq!(
        store
            .record_child_link(parent, child, "dispatch")
            .expect("link child"),
        None
    );
}

fn settle(state: &crate::state::AppState, thread_id: &str) {
    state
        .state_store
        .finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: ThreadStatus::Completed.as_str().to_string(),
                outcome_code: Some(ThreadStatus::Completed.as_str().to_string()),
                result_json: None,
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .expect("settle thread");
}

#[test]
fn dispatched_child_gets_exactly_one_generic_lineage_event() {
    let root = tempfile::tempdir().expect("tempdir");
    let (_owner, state) = crate::state::test_support::build(root.path()).expect("compose");
    let operation = "c".repeat(64);
    dispatched_child(&state, "T-parent", "T-child", &operation);

    assert_eq!(
        state
            .threads
            .record_dispatch_child_lineage("T-child")
            .unwrap(),
        DispatchChildLineage::Recorded(ChildLineageAppendOutcome::Appended)
    );
    assert_eq!(
        spawned_events(&state, "T-parent"),
        vec![json!({
            "child_thread_id": "T-child",
            "item_id": "directive:test/T-child",
            "operation_id": operation,
            "spawn_reason": "dispatch",
        })]
    );

    // A repeated drive (a retry, or the reconcile pass) writes nothing more.
    assert_eq!(
        state
            .threads
            .record_dispatch_child_lineage("T-child")
            .unwrap(),
        DispatchChildLineage::Recorded(ChildLineageAppendOutcome::AlreadyPresent)
    );
    assert_eq!(spawned_events(&state, "T-parent").len(), 1);
}

#[test]
fn child_without_an_action_intent_is_owed_no_event() {
    let root = tempfile::tempdir().expect("tempdir");
    let (_owner, state) = crate::state::test_support::build(root.path()).expect("compose");
    let store = &state.state_store;
    store
        .create_thread_for_test(&thread_record("T-parent"))
        .unwrap();
    store.mark_thread_running("T-parent", None).unwrap();
    store
        .create_thread_for_test(&thread_record("T-child"))
        .unwrap();
    store
        .record_child_link("T-parent", "T-child", "dispatch")
        .unwrap();

    assert_eq!(
        state
            .threads
            .record_dispatch_child_lineage("T-child")
            .unwrap(),
        DispatchChildLineage::NotOwed
    );
    assert!(spawned_events(&state, "T-parent").is_empty());
}

#[test]
fn parent_settling_before_the_append_writes_nothing_and_requires_fails_closed() {
    let root = tempfile::tempdir().expect("tempdir");
    let (_owner, state) = crate::state::test_support::build(root.path()).expect("compose");
    dispatched_child(&state, "T-parent", "T-child", &"d".repeat(64));

    // The window between creating and linking the child and recording its
    // lineage: the parent settles inside it.
    settle(&state, "T-parent");

    assert_eq!(
        state
            .threads
            .record_dispatch_child_lineage("T-child")
            .unwrap(),
        DispatchChildLineage::Recorded(ChildLineageAppendOutcome::ParentSettled)
    );
    assert!(
        state
            .threads
            .require_dispatch_child_lineage("T-child")
            .is_err(),
        "a live dispatch site must refuse a child whose parent cannot author its lineage"
    );
    assert!(spawned_events(&state, "T-parent").is_empty());
}

fn append_batch(
    state: &crate::state::AppState,
    thread_id: &str,
    events: Vec<(&str, Value)>,
) -> anyhow::Result<crate::event_store_service::EventAppendBatchResult> {
    state.events.append_batch(
        &crate::event_store_service::EventAppendBatchParams {
            thread_id: thread_id.to_string(),
            events: events
                .into_iter()
                .map(
                    |(event_type, payload)| crate::event_store_service::EventAppendItem {
                        event_type: event_type.to_string(),
                        payload,
                    },
                )
                .collect(),
            attachments: Vec::new(),
            idempotency_key: None,
        },
        &ryeos_node_runtime::node_policy::sections::event_storage::EventStoragePolicy::as_shipped(),
        &ryeos_node_runtime::node_policy::sections::thread_attachments::ThreadAttachmentsPolicy::as_shipped(),
    )
}

#[test]
fn an_inline_dispatched_child_gets_its_lineage_from_the_daemon_and_a_runtime_cannot_write_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let (_owner, state) = crate::state::test_support::build(root.path()).expect("compose");
    let operation = "e".repeat(64);
    dispatched_child_in_mode(
        &state,
        "T-parent",
        "T-child",
        &operation,
        RuntimeActionMode::Inline,
    );

    // The parent's own runtime append of the lineage event is refused, whole
    // batch included, and writes nothing.
    let forged = json!({
        "child_thread_id": "T-child",
        "node": "n",
        "step": 0,
        "item_id": "directive:test/T-child",
        "spawn_reason": "dispatch",
    });
    let error = append_batch(
        &state,
        "T-parent",
        vec![("observation", json!({})), ("child_thread_spawned", forged)],
    )
    .unwrap_err();
    assert!(error.to_string().contains("daemon-authored"), "{error:#}");
    assert!(spawned_events(&state, "T-parent").is_empty());

    // The daemon writes it from the retained action intent: the parent is the
    // intent's first caller, and the payload names the occurrence.
    assert_eq!(
        state
            .threads
            .record_dispatch_child_lineage("T-child")
            .unwrap(),
        DispatchChildLineage::Recorded(ChildLineageAppendOutcome::Appended)
    );
    assert_eq!(
        spawned_events(&state, "T-parent"),
        vec![json!({
            "child_thread_id": "T-child",
            "item_id": "directive:test/T-child",
            "operation_id": operation,
            "spawn_reason": "dispatch",
        })]
    );
}

fn usage_payload(completed_turns: u32, input_tokens: u64, spend_nanos: i64) -> Value {
    json!({
        "completed_turns": completed_turns,
        "input_tokens": input_tokens,
        "output_tokens": 1,
        "spend_usd": ryeos_accounting::UsdNanos::from_nanos(spend_nanos).unwrap(),
        "spawns_used": 0,
        "started_at": "2026-06-01T00:00:00Z",
        "settled_at": "2026-06-01T00:05:00Z",
        "last_settled_turn_seq": completed_turns,
        "elapsed_ms": 100,
    })
}

#[test]
fn a_running_thread_cannot_regress_its_usage_and_a_resent_settlement_is_accepted() {
    let root = tempfile::tempdir().expect("tempdir");
    let (_owner, state) = crate::state::test_support::build(root.path()).expect("compose");
    state
        .state_store
        .create_thread_for_test(&thread_record("T-usage"))
        .unwrap();
    state
        .state_store
        .mark_thread_running("T-usage", None)
        .unwrap();

    append_batch(
        &state,
        "T-usage",
        vec![("thread_usage", usage_payload(2, 200, 5_000))],
    )
    .expect("first settlement");
    // The same payload again, as a runner resends after a lost acknowledgement.
    append_batch(
        &state,
        "T-usage",
        vec![("thread_usage", usage_payload(2, 200, 5_000))],
    )
    .expect("identical resend");
    append_batch(
        &state,
        "T-usage",
        vec![("thread_usage", usage_payload(3, 300, 9_000))],
    )
    .expect("cumulative step");

    for lower in [usage_payload(2, 300, 9_000), usage_payload(3, 299, 9_000)] {
        let error = append_batch(&state, "T-usage", vec![("thread_usage", lower)]).unwrap_err();
        assert!(error.to_string().contains("not cumulative"), "{error:#}");
    }
    // Daemon-authored types stay refused alongside the legitimate append, and
    // the refused batch wrote nothing.
    let error = append_batch(
        &state,
        "T-usage",
        vec![
            ("thread_usage", usage_payload(4, 400, 9_500)),
            ("thread_continued", json!({"previous_thread_id": "T-x"})),
        ],
    )
    .unwrap_err();
    assert!(error.to_string().contains("daemon-authored"), "{error:#}");
    let held = state
        .state_store
        .latest_thread_event_by_type("T-usage", "thread_usage")
        .unwrap()
        .expect("held settlement");
    assert_eq!(held.payload["completed_turns"], json!(3));
}

/// Two callers racing to record the same parent→child lineage event: the write
/// permit and store lock cover the once-only check and the append, so exactly
/// one appends and the other finds the event already written.
#[test]
fn racing_lineage_appends_leave_exactly_one_event() {
    let root = tempfile::tempdir().expect("tempdir");
    let (_owner, state) = crate::state::test_support::build(root.path()).expect("compose");
    let operation = "f".repeat(64);
    dispatched_child(&state, "T-parent", "T-child", &operation);
    let payload = json!({
        "child_thread_id": "T-child",
        "item_id": "directive:test/T-child",
        "operation_id": operation,
        "spawn_reason": "dispatch",
    });

    let barrier = std::sync::Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let racers = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    state
                        .threads
                        .append_child_thread_spawned_once(
                            "T-parent",
                            "T-parent",
                            "T-child",
                            payload.clone(),
                        )
                        .expect("append once")
                })
            })
            .collect::<Vec<_>>();
        racers
            .into_iter()
            .map(|racer| racer.join().expect("racer"))
            .collect::<Vec<_>>()
    });

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ChildLineageAppendOutcome::Appended)
            .count(),
        1,
        "exactly one racer appends: {outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ChildLineageAppendOutcome::AlreadyPresent)
            .count(),
        1,
        "the other finds the event written: {outcomes:?}"
    );
    assert_eq!(spawned_events(&state, "T-parent"), vec![payload]);
}

/// A same-chain child (its `upstream_thread_id` is the parent) already has an
/// edge derived from that field, and still owes the daemon-written event.
#[test]
fn same_chain_child_with_a_derived_edge_still_gets_its_lineage_event() {
    let root = tempfile::tempdir().expect("tempdir");
    let (_owner, state) = crate::state::test_support::build(root.path()).expect("compose");
    let operation = "9".repeat(64);
    let store = &state.state_store;
    store
        .create_thread_for_test(&thread_record("T-parent"))
        .expect("parent thread");
    store
        .mark_thread_running("T-parent", None)
        .expect("start parent");
    store
        .reserve_runtime_action_intent(
            &operation,
            "T-parent",
            RuntimeActionMode::Inline,
            &"b".repeat(64),
            "T-child",
            None,
        )
        .expect("reserve action intent");
    let mut child = thread_record("T-child");
    child.chain_root_id = "T-parent".to_string();
    child.upstream_thread_id = Some("T-parent".to_string());
    // A non-root thread carries no captured history policy of its own.
    child.captured_history_policy = None;
    store
        .create_thread_for_test(&child)
        .expect("same-chain child");
    store
        .record_child_link("T-parent", "T-child", "dispatch")
        .expect("link child");

    assert_eq!(
        state
            .threads
            .record_dispatch_child_lineage("T-child")
            .unwrap(),
        DispatchChildLineage::Recorded(ChildLineageAppendOutcome::Appended)
    );
    assert_eq!(spawned_events(&state, "T-parent").len(), 1);
}
