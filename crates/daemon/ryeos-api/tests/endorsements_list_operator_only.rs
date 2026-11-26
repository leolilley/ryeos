//! The endorsement ledger is the node's, so only its operator reads it. A
//! non-operator key that holds the capability is refused, whatever project it
//! names, so the answer for another principal's project is not a presence
//! oracle; the operator's call is not refused.

use std::sync::Arc;

use ryeos_api::handlers::endorsements::{ListRequest, list};
use ryeos_api::test_support as test_state;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;

fn is_operator_required(error: &HandlerError) -> bool {
    matches!(error, HandlerError::Structured { code, status: 403, .. } if code == "operator_required")
}

#[tokio::test]
async fn a_non_operator_key_is_refused_the_ledger_whatever_project_it_names() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let stranger = HandlerContext::new_with_authority(
        format!("fp:{}", "b".repeat(64)),
        vec!["ryeos.execute.service.endorsements/list".to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    for project in [None, Some("/srv/another-principals-project".to_owned())] {
        let refused = list(
            ListRequest {
                project: project.clone(),
            },
            stranger.clone(),
            state.clone(),
        )
        .await
        .expect_err("a non-operator key reads no ledger");
        assert!(is_operator_required(&refused), "{project:?}: {refused:?}");
    }
}

#[tokio::test]
async fn the_operator_is_not_refused() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .expect("fixture operator");
    let context = HandlerContext::new_with_authority(
        operator.principal_id(),
        vec!["ryeos.execute.service.endorsements/list".to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let answer = list(ListRequest { project: None }, context, Arc::new(state)).await;
    if let Err(error) = &answer {
        assert!(!is_operator_required(error), "{error:?}");
    }
}
