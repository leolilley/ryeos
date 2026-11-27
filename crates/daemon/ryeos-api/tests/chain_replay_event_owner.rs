//! `events/chain_replay` returns a chain's events to the chain root's owner
//! only for the threads that owner requested. A descendant thread requested by
//! another principal stays out of the root owner's replay; the node's admitted
//! operator reads the whole chain; a caller who does not own the root is
//! refused as before.

use ryeos_api::test_support as test_state;

use std::sync::Arc;

use ryeos_api::handler_context::HandlerContext;
use ryeos_api::handler_error::HandlerError;
use ryeos_api::handlers::events_chain_replay;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewThreadRecord;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;
use serde_json::Value;

const A: &str = "fp:1111111111111111111111111111111111111111111111111111111111111111";
const B: &str = "fp:2222222222222222222222222222222222222222222222222222222222222222";

fn thread(thread_id: &str, chain_root_id: &str, owner: &str) -> NewThreadRecord {
    let hash = "a".repeat(64);
    let is_root = thread_id == chain_root_id;
    NewThreadRecord {
        thread_id: thread_id.to_owned(),
        chain_root_id: chain_root_id.to_owned(),
        kind: "tool".to_owned(),
        item_ref: "tool:test/chain".to_owned(),
        executor_ref: "native:test".to_owned(),
        launch_mode: "wait".to_owned(),
        current_site_id: "site:test".to_owned(),
        origin_site_id: "site:test".to_owned(),
        upstream_thread_id: (!is_root).then(|| chain_root_id.to_owned()),
        requested_by: Some(owner.to_owned()),
        project_root: None,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: is_root.then(|| {
            ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
                canonical_item_ref: "tool:test/chain".to_owned(),
                item_content_hash: hash.clone(),
                item_signer_fingerprint: Some(hash.clone()),
                item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: hash,
                resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:
                        ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
                },
            }
        }),
    }
}

fn seed(state: &AppState) {
    state
        .state_store
        .create_thread_for_test(&thread("T-root", "T-root", A))
        .unwrap();
    state
        .state_store
        .create_thread_for_test(&thread("T-mine", "T-root", A))
        .unwrap();
    state
        .state_store
        .create_thread_for_test(&thread("T-theirs", "T-root", B))
        .unwrap();
}

fn user(key: &str) -> HandlerContext {
    HandlerContext::new(key.to_owned(), Vec::new(), true)
}

async fn replay(
    state: Arc<AppState>,
    ctx: HandlerContext,
    after: Option<i64>,
) -> Result<Value, HandlerError> {
    events_chain_replay::handle(
        events_chain_replay::Request {
            chain_root_id: "T-root".to_owned(),
            after_chain_seq: after,
            limit: 200,
        },
        ctx,
        state,
    )
    .await
}

fn thread_ids(answer: &Value) -> Vec<String> {
    answer["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["thread_id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_root_owner_reads_only_the_events_of_threads_they_requested() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    seed(&state);
    let state = Arc::new(state);

    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    let everything = replay(
        state.clone(),
        HandlerContext::new_with_authority(
            operator.principal_id(),
            Vec::new(),
            true,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        ),
        None,
    )
    .await
    .unwrap();
    let all = thread_ids(&everything);
    assert!(all.iter().any(|id| id == "T-root"));
    assert!(all.iter().any(|id| id == "T-mine"));
    assert!(all.iter().any(|id| id == "T-theirs"));

    let mine = replay(state.clone(), user(A), None).await.unwrap();
    let seen = thread_ids(&mine);
    assert!(seen.iter().any(|id| id == "T-root"));
    assert!(seen.iter().any(|id| id == "T-mine"));
    assert!(
        !seen.iter().any(|id| id == "T-theirs"),
        "another principal's thread must not reach the root owner: {seen:?}"
    );
    // The cursor is the unfiltered page's, so it advances over hidden events.
    assert_eq!(mine["next_cursor"], everything["next_cursor"]);
}

#[tokio::test]
async fn a_caller_who_does_not_own_the_root_is_refused_as_not_found() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    seed(&state);
    let error = replay(Arc::new(state), user(B), None)
        .await
        .expect_err("B does not own the chain root");
    assert!(matches!(error, HandlerError::NotFound));
}
