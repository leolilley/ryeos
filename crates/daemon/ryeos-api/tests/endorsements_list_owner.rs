//! The endorsement ledger is the node's. The operator reads all of it; any
//! other key reads only the endorsements whose project it speaks for, and the
//! ledger head, which moves when anyone endorses, is the operator's alone.

use std::sync::Arc;

use ryeos_api::handlers::endorsements::{ListRequest, list};
use ryeos_api::test_support as test_state;
use ryeos_app::handler_context::HandlerContext;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;

fn key(byte: char) -> HandlerContext {
    HandlerContext::new_with_authority(
        format!("fp:{}", byte.to_string().repeat(64)),
        vec!["ryeos.execute.service.endorsements/list".to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

#[tokio::test]
async fn a_non_operator_key_never_sees_the_ledger_head() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    for project in [None, Some("pid:v1:none/nobody".to_owned())] {
        let answer = list(ListRequest { project }, key('b'), state.clone())
            .await
            .expect("a verified key reads its own, empty, view");
        assert!(answer.get("ledger_head").is_none(), "{answer}");
        assert_eq!(answer["endorsements"], serde_json::json!([]), "{answer}");
    }
}

#[tokio::test]
async fn the_operator_still_reads_the_head() {
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
    let answer = list(ListRequest { project: None }, context, Arc::new(state))
        .await
        .expect("the operator reads the ledger");
    assert!(answer.get("ledger_head").is_some(), "{answer}");
}
