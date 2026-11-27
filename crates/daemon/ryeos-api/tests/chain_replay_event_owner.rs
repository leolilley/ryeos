//! `events/chain_replay` returns a chain's events to the chain root's owner
//! only for the threads that owner requested. A descendant thread requested by
//! another principal stays out of the root owner's replay; the node's admitted
//! operator reads the whole chain; a caller who does not own the root is
//! refused as before. The timeline fold reads the chain through the same rule:
//! its event page holds only the threads the reader may see.

use ryeos_api::test_support as test_state;

use std::sync::Arc;

use ryeos_api::handler_context::HandlerContext;
use ryeos_api::handler_error::HandlerError;
use ryeos_api::handlers::events_chain_replay;
use ryeos_api::timeline_host::{ThreadFilter, chain_usage_for, visible_page};
use ryeos_app::state::AppState;
use ryeos_app::state_store::{NewEventRecord, NewThreadRecord};
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

/// A chain rooted at `root_owner` with a descendant requested by `B`.
fn seed_chain_for(state: &AppState, root: &str, root_owner: &str) {
    state
        .state_store
        .create_thread_for_test(&thread(root, root, root_owner))
        .unwrap();
    state
        .state_store
        .create_thread_for_test(&thread(&format!("{root}-other"), root, B))
        .unwrap();
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

/// Record `input_tokens` of cumulative usage for `thread_id` the way a runtime
/// does: a `thread_usage` event, which the projection keeps as the thread's
/// latest usage row.
fn record_usage(state: &AppState, thread_id: &str, input_tokens: u64) {
    let event_type = ryeos_runtime::events::RuntimeEventType::ThreadUsage;
    state
        .state_store
        .append_events(
            "T-root",
            thread_id,
            &[NewEventRecord {
                event_type: event_type.as_str().to_owned(),
                storage_class: event_type.storage_class().as_str().to_owned(),
                payload: serde_json::json!({
                    "completed_turns": 1,
                    "input_tokens": input_tokens,
                    "output_tokens": 1,
                    "spend_usd": ryeos_accounting::UsdNanos::from_nanos(1_000).unwrap(),
                    "spawns_used": 0,
                    "started_at": "2026-06-01T00:00:00Z",
                    "settled_at": "2026-06-01T00:05:00Z",
                    "last_settled_turn_seq": 1,
                    "elapsed_ms": 100,
                }),
            }],
        )
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
    replay_chain(state, ctx, "T-root", after).await
}

async fn replay_chain(
    state: Arc<AppState>,
    ctx: HandlerContext,
    chain_root_id: &str,
    after: Option<i64>,
) -> Result<Value, HandlerError> {
    events_chain_replay::handle(
        events_chain_replay::Request {
            chain_root_id: chain_root_id.to_owned(),
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
    // The operator owns the root of a second chain, which holds a thread
    // requested by B; the operator sees both.
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    seed_chain_for(&state, "O-root", &operator.principal_id());
    let state = Arc::new(state);

    let operator_view = replay_chain(
        state.clone(),
        HandlerContext::new_with_authority(
            operator.principal_id(),
            Vec::new(),
            true,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        ),
        "O-root",
        None,
    )
    .await
    .unwrap();
    let all = thread_ids(&operator_view);
    assert!(all.iter().any(|id| id == "O-root"), "{all:?}");
    assert!(all.iter().any(|id| id == "O-root-other"), "{all:?}");

    let mine = replay(state.clone(), user(A), None).await.unwrap();
    let seen = thread_ids(&mine);
    assert!(seen.iter().any(|id| id == "T-root"));
    assert!(seen.iter().any(|id| id == "T-mine"));
    assert!(
        !seen.iter().any(|id| id == "T-theirs"),
        "another principal's thread must not reach the root owner: {seen:?}"
    );
    assert!(mine["next_cursor"].is_null() || mine["next_cursor"].is_number());
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

fn timeline_threads(state: &AppState, filter: Option<&ThreadFilter<'_>>) -> Vec<String> {
    let mut judged = std::collections::HashMap::new();
    let page = visible_page(state, "T-root", None, filter, &mut judged).unwrap();
    let mut ids: Vec<String> = page
        .events
        .iter()
        .map(|event| event["thread_id"].as_str().unwrap().to_owned())
        .collect();
    ids.dedup();
    ids
}

#[test]
fn the_timeline_page_holds_only_the_threads_the_reader_owns_and_the_operator_sees_all() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    seed(&state);

    let all = timeline_threads(&state, None);
    assert!(all.iter().any(|id| id == "T-theirs"), "{all:?}");

    let ctx = user(A);
    let owns = |thread_id: &str| {
        state
            .state_store
            .get_thread(thread_id)
            .ok()
            .flatten()
            .is_some_and(|detail| ctx.is_owner(detail.requested_by.as_deref()))
    };
    let filter = ThreadFilter { admits: &owns };
    let mine = timeline_threads(&state, Some(&filter));
    assert!(mine.iter().any(|id| id == "T-root"), "{mine:?}");
    assert!(mine.iter().any(|id| id == "T-mine"), "{mine:?}");
    assert!(!mine.iter().any(|id| id == "T-theirs"), "{mine:?}");
}

#[test]
fn a_readers_usage_totals_count_only_the_threads_they_own() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    seed(&state);
    record_usage(&state, "T-mine", 100);
    record_usage(&state, "T-theirs", 1000);

    let whole_chain = chain_usage_for(&state, "T-root", None).unwrap();
    assert_eq!(whole_chain.input_tokens, 1100);

    let ctx = user(A);
    let owns = |thread_id: &str| {
        state
            .state_store
            .get_thread(thread_id)
            .ok()
            .flatten()
            .is_some_and(|detail| ctx.is_owner(detail.requested_by.as_deref()))
    };
    let filter = ThreadFilter { admits: &owns };
    let mine = chain_usage_for(&state, "T-root", Some(&filter)).unwrap();
    assert_eq!(mine.input_tokens, 100, "B's thread is not counted");
}
