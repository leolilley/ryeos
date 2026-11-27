//! The attention acknowledge service: who may call it, and what it records.

use ryeos_ui::test_support as test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{FinalizeThreadRecord, NewThreadRecord};
use serde_json::{Value, json};
use test_state::{build_test_state, build_test_state_with_live_bundles, local_operator_context};

fn captured_policy() -> ryeos_state::objects::CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    ryeos_state::objects::CapturedThreadHistoryPolicy {
        retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
        canonical_item_ref: "directive:test/attention".to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
            node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
        },
    }
}

fn failed_run(state: &AppState, thread_id: &str, requested_by: &str) {
    state
        .state_store
        .create_thread_for_test(&NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test/attention".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "wait".to_string(),
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
            owner: None,
        })
        .unwrap();
    state
        .state_store
        .finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: "failed".to_owned(),
                outcome_code: None,
                result_json: None,
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .unwrap();
}

async fn acknowledge(
    state: &Arc<AppState>,
    ctx: &HandlerContext,
    item_id: &str,
) -> anyhow::Result<Value> {
    (ryeos_ui::handlers::ui_work::ACKNOWLEDGE_DESCRIPTOR.handler)(
        json!({"item_id": item_id}),
        ctx.clone(),
        state.clone(),
    )
    .await
}

async fn queue(state: &Arc<AppState>, ctx: &HandlerContext) -> Value {
    (ryeos_ui::handlers::ui_work::ATTENTION_DESCRIPTOR.handler)(
        json!({"limit": 50}),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("the operator reads their own queue")
}

#[test]
fn the_service_requires_its_own_capability() {
    let descriptor = ryeos_ui::handlers::ui_work::ACKNOWLEDGE_DESCRIPTOR;
    assert_eq!(descriptor.endpoint, "ui.ryeos.work.acknowledge");
    assert_eq!(
        descriptor.required_caps,
        &["ryeos.execute.service.ui/ryeos-ui/work/acknowledge"]
    );
}

#[tokio::test]
async fn an_unadmitted_caller_is_refused_before_the_request_is_read() {
    let (_tmp, _owner, state) = build_test_state();
    let state = Arc::new(state);
    let anonymous = HandlerContext::new("anonymous:test".to_string(), Vec::new(), false);
    let error = (ryeos_ui::handlers::ui_work::ACKNOWLEDGE_DESCRIPTOR.handler)(
        json!({"not": "a valid acknowledge request"}),
        anonymous,
        state,
    )
    .await
    .expect_err("acknowledging requires an admitted operator");
    assert!(
        format!("{error:#}").contains("admitted operator required"),
        "{error:#}"
    );
}

#[tokio::test]
async fn an_operator_acknowledges_only_their_own_items_and_the_queue_follows() {
    let (_tmp, _owner, state) = build_test_state_with_live_bundles();
    let operator = local_operator_context(&state, Vec::new());
    failed_run(&state, "T-mine", &operator.fingerprint);
    failed_run(&state, "T-theirs", "fp:someone-else");
    let state = Arc::new(state);

    let before = queue(&state, &operator).await;
    assert_eq!(before["counts"]["total"], 1);
    assert_eq!(before["attention"][0]["id"], "failed_run:T-mine");
    assert_eq!(before["attention"][0]["actions"]["acknowledge"], true);

    // Another principal's item, and an item that does not exist, are the same
    // refusal: the caller learns nothing about them.
    for id in ["failed_run:T-theirs", "failed_run:T-nobody"] {
        let error = acknowledge(&state, &operator, id)
            .await
            .expect_err("not in the caller's queue");
        assert!(
            matches!(
                error.downcast_ref::<HandlerError>(),
                Some(HandlerError::NotFound)
            ),
            "{id}: {error:#}"
        );
    }
    // Malformed requests are refused too.
    let error = (ryeos_ui::handlers::ui_work::ACKNOWLEDGE_DESCRIPTOR.handler)(
        json!({"item_id": "failed_run:T-mine", "principal": "fp:someone-else"}),
        operator.clone(),
        state.clone(),
    )
    .await
    .expect_err("a caller names no principal");
    assert!(matches!(
        error.downcast_ref::<HandlerError>(),
        Some(HandlerError::BadRequest(_))
    ));

    let done = acknowledge(&state, &operator, "failed_run:T-mine")
        .await
        .unwrap();
    assert_eq!(done["already"], false);
    let after = queue(&state, &operator).await;
    assert_eq!(after["counts"]["total"], 0);
    assert_eq!(after["attention"], json!([]));
}
