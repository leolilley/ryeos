//! A pinned-project external-content consumer states its logical project, the
//! thing project-scoped signers are matched on, only for the snapshot that is
//! the current head, for the calling principal, of the project it declares. Bind, the bindings
//! listing and both rebind calls reach this one function through
//! `prepare_consumer`, so one gate covers all of them.

use ryeos_api::test_support as test_state;
use ryeos_app::state_store::NodeIdentitySigner;
use ryeos_executor::execution::project_source::{
    ProjectSourceError, prepare_pinned_project_external_consumer,
};
use ryeos_state::objects::{ProjectFile, ProjectSnapshot, ProjectTree};
use ryeos_state::project_identity::{ProjectHeadKey, ProjectName};
use serde_json::json;
use std::collections::BTreeMap;

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

/// Stores a snapshot whose tree declares `name` in `.ai/manifest.yaml` and
/// holds one more file carrying `marker`, and returns its hash.
fn store_declared_snapshot(cas: &lillux::CasStore, name: &str, marker: &str) -> String {
    let declaration = format!("name: {name}\n");
    let mut files = BTreeMap::new();
    for (path, bytes) in [
        (".ai/manifest.yaml", declaration.as_bytes()),
        (".ai/config/gate/marker.yaml", marker.as_bytes()),
    ] {
        let file = ProjectFile {
            blob_hash: cas.store_blob(bytes).unwrap(),
            size: u64::try_from(bytes.len()).unwrap(),
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        files.insert(path.to_owned(), cas.store_object(&file.to_value()).unwrap());
    }
    let snapshot = ProjectSnapshot {
        project_tree_hash: cas.store_object(&ProjectTree { files }.to_value()).unwrap(),
        effective_policy_hash: cas.store_object(&json!({"policy": "test"})).unwrap(),
        message: None,
        parent_hashes: Vec::new(),
        created_at: lillux::time::iso8601_now(),
        source: "push".to_owned(),
    };
    cas.store_object(&snapshot.to_value()).unwrap()
}

#[test]
fn a_pinned_project_consumer_is_prepared_only_for_the_principals_current_head() {
    let (_node, _state_owner, state) = test_state::build_test_state_with_bundles();
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    let principal = operator.principal_id();

    let directory = tempfile::tempdir().unwrap();
    // The path an operator states is a label only: no spelling of it, and no
    // directory at all, changes which head is read.
    let project = directory.path().join("project");
    let unrelated = directory.path().join("unrelated-label");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&unrelated).unwrap();
    std::fs::create_dir_all(project.join("sub")).unwrap();
    let respelled = project.join("sub/..");

    let name = ProjectName::new("gate-project").unwrap();
    let owner = ryeos_app::project_owner::owner_principal(&state, &principal).unwrap();
    let key = ProjectHeadKey::owned_by(&owner, &name);
    let advancer =
        ryeos_app::project_owner::head_advancer(&state, &principal, key.owner()).unwrap();
    assert!(key.ref_path().starts_with("projects/"));

    let authority = state.state_store.pinned_state_authority().unwrap();
    let cas = authority.cas_store().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let head = store_declared_snapshot(&cas, name.as_str(), "head");
    let other = store_declared_snapshot(&cas, name.as_str(), "other");
    // Declares a project this principal has no head for.
    let lookalike = store_declared_snapshot(&cas, "gate-project-lookalike", "head");
    state
        .state_store
        .write_project_head_ref(
            &key,
            &head,
            &advancer,
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

    // A snapshot that is not the head of the project it declares is refused,
    // typed, before any resolution runs.
    let mismatch = prepare(&other, &project);
    assert_eq!(
        typed_refusal(&mismatch),
        Some((other.as_str(), head.as_str()))
    );

    // A snapshot declaring a project this principal has no head for is
    // refused the same way.
    let unknown = prepare(&lookalike, &project);
    assert_eq!(typed_refusal(&unknown), Some((lookalike.as_str(), "none")));

    // The head itself passes the gate whatever label path accompanies it.
    // Preparation then fails later, on the placeholder consumer, with some
    // other error.
    for path in [&project, &respelled, &unrelated] {
        let admitted = prepare(&head, path);
        assert!(
            typed_refusal(&admitted).is_none(),
            "the current head must pass the gate for {}: {admitted:#}",
            path.display()
        );
    }
}
