//! DTO/ownership qualification only: exact signed thread testimony is exposed,
//! but the returned coordinate is not an import or project-closure receipt.
mod test_state;

use std::sync::Arc;

use ryeos_api::handler_context::HandlerContext;
use ryeos_api::handler_error::HandlerError;
use ryeos_api::handlers::threads_get;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{FinalizeThreadRecord, NewThreadRecord};
use ryeos_state::objects::{
    EnvironmentAuthority, ExecutionProjectAuthority, PinnedProjectRealization,
    PinnedTerminalPublication,
};
use serde_json::{Value, json};

fn create_thread(state: &AppState, thread_id: &str, owner: &str) {
    let hash = "a".repeat(64);
    let base = "b".repeat(64);
    let project_authority = ExecutionProjectAuthority::pinned(
        "d".repeat(64),
        None,
        base.clone(),
        PinnedProjectRealization::Cow {
            terminal_publication: PinnedTerminalPublication::RetainResult,
        },
        EnvironmentAuthority::None,
        Vec::new(),
    )
    .unwrap();
    state
        .state_store
        .create_thread_for_test(&NewThreadRecord {
            thread_id: thread_id.to_owned(),
            chain_root_id: thread_id.to_owned(),
            kind: "tool".to_owned(),
            item_ref: "tool:test/retained".to_owned(),
            executor_ref: "native:test".to_owned(),
            launch_mode: "wait".to_owned(),
            current_site_id: "site:test".to_owned(),
            origin_site_id: "site:test".to_owned(),
            upstream_thread_id: None,
            requested_by: Some(owner.to_owned()),
            project_root: None,
            project_authority,
            base_project_snapshot_hash: Some(base),
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
                canonical_item_ref: "tool:test/retained".to_owned(),
                item_content_hash: hash.clone(),
                item_signer_fingerprint: Some(hash.clone()),
                item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: hash,
                resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:
                        ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
                },
            }),
        })
        .unwrap();
}

fn complete(state: &AppState, thread_id: &str, result: Option<&str>) {
    state
        .state_store
        .finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: "completed".to_owned(),
                outcome_code: Some("success".to_owned()),
                result_json: Some(json!({"ok":true})),
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: result.map(str::to_owned),
                result_workspace_output_capture_hash: None,
            },
        )
        .unwrap();
}

async fn get(state: Arc<AppState>, id: &str, owner: &str) -> Result<Value, HandlerError> {
    threads_get::handle(
        threads_get::Request {
            thread_id: id.to_owned(),
        },
        HandlerContext::new(owner.to_owned(), Vec::new(), true),
        state,
    )
    .await
}

#[tokio::test]
async fn exact_thread_get_exposes_retained_result_and_preserves_owner_boundary() {
    let (_tmp, state) = test_state::build_test_state();
    let owner = format!("fp:{}", "1".repeat(64));
    let result = "c".repeat(64);
    create_thread(&state, "T-retained-result", &owner);
    complete(&state, "T-retained-result", Some(&result));
    create_thread(&state, "T-no-retained-result", &owner);
    complete(&state, "T-no-retained-result", None);
    let state = Arc::new(state);

    let retained = get(state.clone(), "T-retained-result", &owner)
        .await
        .unwrap();
    assert_eq!(retained["thread"]["thread_id"], "T-retained-result");
    assert_eq!(retained["thread"]["status"], "completed");
    assert_eq!(retained["thread"]["result_project_snapshot_hash"], result);
    let absent = get(state.clone(), "T-no-retained-result", &owner)
        .await
        .unwrap();
    assert!(
        absent["thread"]
            .get("result_project_snapshot_hash")
            .unwrap()
            .is_null()
    );
    assert!(
        get(state.clone(), "T-missing-result", &owner)
            .await
            .unwrap()
            .is_null()
    );
    let wrong_owner = get(
        state,
        "T-retained-result",
        &format!("fp:{}", "2".repeat(64)),
    )
    .await
    .unwrap_err();
    assert!(matches!(wrong_owner, HandlerError::NotFound));
}

#[tokio::test]
async fn projected_result_hash_drift_does_not_override_the_exact_thread_snapshot() {
    let (_tmp, state) = test_state::build_test_state();
    let owner = format!("fp:{}", "1".repeat(64));
    let result = "c".repeat(64);
    create_thread(&state, "T-retained-result", &owner);
    complete(&state, "T-retained-result", Some(&result));
    create_thread(&state, "T-later-result", &owner);
    complete(&state, "T-later-result", Some(&"e".repeat(64)));

    // Corrupt only the rebuildable DTO source, not its signed history.
    // The single-thread detail must keep reading its existing immutable owner.
    state
        .state_store
        .with_state_db(|db| {
            db.projection().connection().execute(
                "UPDATE threads SET result_project_snapshot_hash = ?1 WHERE thread_id = ?2",
                ["f".repeat(64), "T-retained-result".to_owned()],
            )?;
            Ok(())
        })
        .unwrap();
    let state = Arc::new(state);
    let retained = get(state.clone(), "T-retained-result", &owner)
        .await
        .unwrap();
    assert_eq!(retained["thread"]["result_project_snapshot_hash"], result);
    let later = get(state, "T-later-result", &owner).await.unwrap();
    assert_eq!(
        later["thread"]["result_project_snapshot_hash"],
        "e".repeat(64)
    );
}
