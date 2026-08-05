mod test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{NewEventRecord, NewThreadRecord};
use test_state::build_test_state;

fn captured_policy() -> ryeos_state::objects::CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    ryeos_state::objects::CapturedThreadHistoryPolicy {
        retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
        canonical_item_ref: "directive:test/field".to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
            node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
        },
    }
}

fn create_running_thread(state: &AppState, thread_id: &str) {
    state
        .state_store
        .create_thread_for_test(&NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test/field".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("fp:test-field".to_string()),
            project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(captured_policy()),
        })
        .unwrap();
    state
        .state_store
        .mark_thread_running(thread_id, None)
        .unwrap();
}

fn append_fixture_event(
    state: &AppState,
    thread_id: &str,
) -> ryeos_app::state_store::PersistedEventRecord {
    state
        .threads
        .append_thread_events(
            thread_id,
            thread_id,
            &[NewEventRecord {
                event_type: "cognition_in".to_string(),
                storage_class: "indexed".to_string(),
                payload: serde_json::json!({"content": "fixture"}),
            }],
        )
        .unwrap()
        .expect("fixture thread is running")
        .remove(0)
}

#[tokio::test]
async fn every_field_handler_rejects_before_parsing_an_unauthenticated_request() {
    let (_tmp, state) = build_test_state();
    let state = Arc::new(state);
    let unauthenticated = HandlerContext::new("anonymous:test".to_string(), Vec::new(), false);
    let malformed = serde_json::json!({"not": "a valid request for any field handler"});

    for descriptor in [
        &ryeos_ui::handlers::ui_field_project::DESCRIPTOR,
        &ryeos_ui::handlers::ui_field_runs::DESCRIPTOR,
        &ryeos_ui::handlers::ui_field_execution::DESCRIPTOR,
        &ryeos_ui::handlers::ui_field_definition::DESCRIPTOR,
    ] {
        let result =
            (descriptor.handler)(malformed.clone(), unauthenticated.clone(), state.clone()).await;
        let error = result.expect_err("field reads require seat authority");
        assert!(
            format!("{error:#}").contains("browser session or verified operator required"),
            "{} parsed or executed before authenticating",
            descriptor.service_ref,
        );
    }
}

#[tokio::test]
async fn execution_handler_rejects_cross_chain_and_hash_mismatched_braid_cuts() {
    let (_tmp, state) = build_test_state();
    create_running_thread(&state, "T-selected");
    create_running_thread(&state, "T-foreign");
    let selected = append_fixture_event(&state, "T-selected");
    let foreign = append_fixture_event(&state, "T-foreign");
    let state = Arc::new(state);
    let operator = HandlerContext::new("fp:test-field".to_string(), Vec::new(), true);

    let cross_chain = (ryeos_ui::handlers::ui_field_execution::DESCRIPTOR.handler)(
        serde_json::json!({
            "thread_id": "T-selected",
            "cursor": {
                "mode": "braid_cut",
                "anchor": {
                    "chain_root_id": foreign.chain_root_id,
                    "chain_seq": foreign.chain_seq,
                    "event_hash": foreign.event_hash,
                }
            }
        }),
        operator.clone(),
        state.clone(),
    )
    .await
    .expect_err("a foreign braid must not become the selected cut");
    assert!(format!("{cross_chain:#}").contains("outside the selected execution closure"));

    let hash_mismatch = (ryeos_ui::handlers::ui_field_execution::DESCRIPTOR.handler)(
        serde_json::json!({
            "thread_id": "T-selected",
            "cursor": {
                "mode": "braid_cut",
                "anchor": {
                    "chain_root_id": selected.chain_root_id,
                    "chain_seq": selected.chain_seq,
                    "event_hash": "f".repeat(64),
                }
            }
        }),
        operator,
        state,
    )
    .await
    .expect_err("a cut hash must bind the durable event");
    assert!(format!("{hash_mismatch:#}").contains("does not match durable chain history"));
}

#[tokio::test]
async fn execution_handler_applies_expansion_to_a_valid_braid_cut() {
    let (_tmp, state) = build_test_state();
    create_running_thread(&state, "T-cut");
    let event = append_fixture_event(&state, "T-cut");
    let state = Arc::new(state);
    let operator = HandlerContext::new("fp:test-field".to_string(), Vec::new(), true);
    let response = (ryeos_ui::handlers::ui_field_execution::DESCRIPTOR.handler)(
        serde_json::json!({
            "thread_id": "T-cut",
            "cursor": {
                "mode": "braid_cut",
                "anchor": {
                    "chain_root_id": event.chain_root_id,
                    "chain_seq": event.chain_seq,
                    "event_hash": event.event_hash,
                }
            },
            "expansions": [{
                "root_id": "thread:T-cut",
                "max_depth": 2,
                "max_entities": 20,
            }]
        }),
        operator,
        state,
    )
    .await
    .expect("valid cut expansion");
    assert_eq!(response["cursor"]["mode"], "braid_cut");
    assert_eq!(response["expansions"][0]["root_id"], "thread:T-cut");
}
