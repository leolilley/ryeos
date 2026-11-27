//! `threads/result` returns the body a thread finished with to the thread's
//! owner only, names it by the digest `threads/get` also carries, answers a
//! thread that is not the caller's exactly as one that does not exist, and is
//! refused (never cut) when the body is over the item's bound.

use ryeos_api::test_support as test_state;

use std::sync::Arc;

use ryeos_api::handler_context::HandlerContext;
use ryeos_api::handler_error::HandlerError;
use ryeos_api::handlers::{threads_get, threads_result};
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
            owner: None,
        })
        .unwrap();
}

fn complete(state: &AppState, thread_id: &str, body: Value) {
    state
        .state_store
        .finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: "completed".to_owned(),
                outcome_code: Some("success".to_owned()),
                result_json: Some(body),
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

const OWNER: &str = "fp:1111111111111111111111111111111111111111111111111111111111111111";
const OTHER: &str = "fp:2222222222222222222222222222222222222222222222222222222222222222";

fn ctx(key: &str) -> HandlerContext {
    HandlerContext::new(key.to_owned(), Vec::new(), true)
}

async fn result(state: Arc<AppState>, id: &str, key: &str) -> Result<Value, HandlerError> {
    threads_result::handle(
        threads_result::Request {
            thread_id: id.to_owned(),
        },
        ctx(key),
        state,
    )
    .await
}

#[tokio::test]
async fn the_owner_gets_the_body_and_its_digest_matches_the_thread_read() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    create_thread(&state, "T-body", OWNER);
    complete(&state, "T-body", json!({"answer": 42}));
    let state = Arc::new(state);

    let answer = result(state.clone(), "T-body", OWNER).await.unwrap();
    assert_eq!(answer["thread_id"], "T-body");
    assert_eq!(answer["result"], json!({"answer": 42}));
    let read = threads_get::handle(
        threads_get::Request {
            thread_id: "T-body".to_owned(),
        },
        ctx(OWNER),
        state,
    )
    .await
    .unwrap();
    assert_eq!(read["result_digest"], answer["result_digest"]);
    assert_eq!(
        answer["result_digest"],
        json!(ryeos_state::objects::canonical_value_digest(&json!({"answer": 42})).unwrap())
    );
}

#[tokio::test]
async fn another_key_gets_the_answer_a_missing_thread_gets() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    create_thread(&state, "T-private", OWNER);
    complete(&state, "T-private", json!({"secret": true}));
    let state = Arc::new(state);

    let stranger = result(state.clone(), "T-private", OTHER).await.unwrap_err();
    let missing = result(state, "T-nowhere", OTHER).await.unwrap_err();
    assert!(matches!(stranger, HandlerError::NotFound));
    assert!(matches!(missing, HandlerError::NotFound));
}

#[test]
fn a_body_over_the_items_bound_is_refused_not_cut() {
    use ryeos_node_runtime::disclosure::{DisclosureClass, ServiceDisclosure, shape_result};
    let item: Value = serde_yaml::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../bundles/core/.ai/services/threads/result.yaml"),
        )
        .unwrap(),
    )
    .unwrap();
    let tier = ServiceDisclosure::parse(&item["disclosure"]).unwrap();
    let small = json!({"thread_id": "T", "result_digest": "d", "result": {"a": 1}});
    let kept = shape_result(
        DisclosureClass::Principal,
        Some(&tier),
        small.clone(),
        tier.bounds(),
    )
    .unwrap();
    assert_eq!(kept, small);
    let big = json!({
        "thread_id": "T", "result_digest": "d",
        "result": {"blob": "x".repeat(usize::try_from(tier.bounds().max_result_bytes).unwrap() + 1)},
    });
    let refusal =
        shape_result(DisclosureClass::Principal, Some(&tier), big, tier.bounds()).unwrap_err();
    assert_eq!(refusal.code(), "read_result_too_large");
}
