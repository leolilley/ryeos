//! A pinned-project external-content consumer states its logical project, the
//! thing project-scoped signers are matched on, only for the snapshot that is
//! that project's current head for the calling principal. Bind, the bindings
//! listing and both rebind calls reach this one function through
//! `prepare_consumer`, so one gate covers all of them.

use ryeos_api::test_support as test_state;
use ryeos_app::state_store::NodeIdentitySigner;
use ryeos_executor::execution::project_source::{
    ProjectSourceError, canonical_project_ref, prepare_pinned_project_external_consumer,
};
use serde_json::json;

const CONSUMER: &str = "tool:ryeos/test/consumer";

fn typed_refusal(error: &anyhow::Error) -> Option<(&str, &str)> {
    match error.downcast_ref::<ProjectSourceError>() {
        Some(ProjectSourceError::SnapshotNotCurrentHead {
            project_path,
            snapshot_hash,
            head,
        }) => Some((snapshot_hash.as_str(), head.as_str())).filter(|_| !project_path.is_empty()),
        _ => None,
    }
}

#[test]
fn a_pinned_project_consumer_is_prepared_only_for_the_principals_current_head() {
    let (_node, _state_owner, state) = test_state::build_test_state_with_bundles();
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    let principal = operator.principal_id();
    let principal_key = ryeos_state::refs::principal_storage_key(&principal)
        .unwrap()
        .to_owned();

    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    let lookalike = directory.path().join("project-lookalike");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&lookalike).unwrap();
    // A trailing-separator, dot-segment spelling of the same project.
    let respelled = project.join("sub/..");
    std::fs::create_dir_all(project.join("sub")).unwrap();

    let project_ref = canonical_project_ref(project.to_str().unwrap()).unwrap();
    let project_hash = lillux::cas::sha256_hex(project_ref.as_bytes());

    let authority = state.state_store.pinned_state_authority().unwrap();
    let cas = authority.cas_store().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let head = cas.store_object(&json!({"snapshot": "head"})).unwrap();
    let other = cas.store_object(&json!({"snapshot": "other"})).unwrap();
    state
        .state_store
        .write_project_head_ref(
            &principal_key,
            &project_hash,
            &head,
            &NodeIdentitySigner::from_identity(&state.identity),
            &guard,
        )
        .unwrap();
    drop(guard);

    let prepare = |snapshot: &str, path: &std::path::Path| {
        prepare_pinned_project_external_consumer(
            &state,
            CONSUMER,
            snapshot,
            path.to_path_buf(),
            principal.clone(),
            vec![],
            "head-gate-test",
        )
        .err()
        .expect("the test consumer is not a real item, so preparation never succeeds")
    };

    // A snapshot that is not the stated path's head is refused, typed, before
    // any resolution runs.
    let mismatch = prepare(&other, &project);
    assert_eq!(
        typed_refusal(&mismatch),
        Some((other.as_str(), head.as_str()))
    );

    // A path with no head for this principal is refused the same way.
    let unknown = prepare(&head, &lookalike);
    assert_eq!(typed_refusal(&unknown), Some((head.as_str(), "none")));

    // The head itself passes the gate, as does another spelling of the same
    // project (it canonicalizes to the same ref). Preparation then fails
    // later, on the placeholder snapshot and consumer, with some other error.
    for path in [&project, &respelled] {
        let admitted = prepare(&head, path);
        assert!(
            typed_refusal(&admitted).is_none(),
            "the current head must pass the gate for {}: {admitted:#}",
            path.display()
        );
    }
}
