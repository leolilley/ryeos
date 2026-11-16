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
            canonical_item_ref: "directive:test".to_string(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy:
                    ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
            },
        }),
    }
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
    let store = &state.state_store;
    store
        .create_thread_for_test(&thread_record(parent))
        .expect("parent thread");
    store.mark_thread_running(parent, None).expect("start parent");
    store
        .reserve_runtime_action_intent(
            operation,
            parent,
            RuntimeActionMode::Detached,
            &"b".repeat(64),
            child,
            None,
        )
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
        state.threads.record_dispatch_child_lineage("T-child").unwrap(),
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
        state.threads.record_dispatch_child_lineage("T-child").unwrap(),
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
        state.threads.record_dispatch_child_lineage("T-child").unwrap(),
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
        state.threads.record_dispatch_child_lineage("T-child").unwrap(),
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
