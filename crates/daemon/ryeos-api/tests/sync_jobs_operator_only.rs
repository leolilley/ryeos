//! Sync jobs are node-global with no owner on the record, so only the node's
//! admitted operator reads them. Any other verified key is refused with the
//! typed operator refusal before any job is read.

use ryeos_api::test_support as test_state;

use std::sync::Arc;

use ryeos_api::handlers::{sync_jobs_inspect, sync_jobs_list};
use ryeos_app::handler_context::HandlerContext;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;

fn other_key() -> HandlerContext {
    HandlerContext::new_with_authority(
        format!("fp:{}", "b".repeat(64)),
        vec![
            "ryeos.execute.service.sync/jobs/list".to_owned(),
            "ryeos.execute.service.sync/jobs/inspect".to_owned(),
        ],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

#[tokio::test]
async fn a_key_that_is_not_the_operator_is_refused_the_job_list() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let error = sync_jobs_list::handle_for_operator(
        sync_jobs_list::Request {
            state: None,
            limit: 10,
        },
        &other_key(),
        Arc::new(state),
    )
    .await
    .expect_err("a non-operator key reads no jobs");
    assert!(
        format!("{error:#}").contains("local_operator_required")
            || format!("{error:#}").contains("operator"),
        "{error:#}"
    );
}

#[tokio::test]
async fn a_key_that_is_not_the_operator_is_refused_the_same_for_any_job_id() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let ask = |job_id: &str| {
        let state = state.clone();
        let job_id = job_id.to_owned();
        async move {
            sync_jobs_inspect::handle_for_operator(
                sync_jobs_inspect::Request { job_id },
                &other_key(),
                state,
            )
            .await
            .expect_err("a non-operator key inspects no job")
        }
    };
    let missing = ask("remote-execute:nowhere").await;
    let other = ask("remote-execute:also-nowhere").await;
    // The refusal is fixed text, so it cannot tell a present job from an absent one.
    assert_eq!(format!("{missing:#}"), format!("{other:#}"));
}
