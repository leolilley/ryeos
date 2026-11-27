// Who may run a live project at a path (`admit_live_project_path`). The
// operator names any directory the node-internal guard allows. Any other key
// is admitted only at a canonical root an owner it speaks for registered,
// exactly, and every other outcome is one refusal with one body.

use ryeos_app::live_project_path::{LiveProjectPathRefusal, admit_live_project_path};
use ryeos_state::project_identity::ProjectIdentity;
use ryeos_ui::test_support as test_state;

/// Two keys with no registry record: each acts for its own solo principal.
const ALPHA: &str = "fp:a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BETA: &str = "fp:b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

fn principal_of(key: &str) -> ryeos_state::principal_id::PrincipalId {
    ryeos_engine::trust::SuccessionGraph::default()
        .ownership_of(key.strip_prefix("fp:").unwrap())
        .unwrap()
        .principal
}

/// A checkout on disk, registered to `key`'s owner under `name`.
fn register_for(
    state: &ryeos_app::state::AppState,
    key: &str,
    name: &str,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let folder = tempfile::TempDir::new().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    std::fs::create_dir_all(canonical.join(".ai")).unwrap();
    std::fs::write(
        canonical.join(ryeos_state::project_identity::PROJECT_DECLARATION_PATH),
        format!("name: {name}\n"),
    )
    .unwrap();
    let identity = ProjectIdentity::new(principal_of(key), name.parse().unwrap());
    ryeos_node::project_registry::register(&ryeos_node::project_registry::RegistrationChange {
        app_root: &state.config.app_root,
        node: &state.identity,
        request_digest: &"0".repeat(64),
        path: &canonical,
        identity: &identity,
    })
    .expect("register the checkout");
    (folder, canonical)
}

#[test]
fn a_key_is_admitted_at_exactly_the_root_its_owner_registered() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let (_folder, root) = register_for(&state, ALPHA, "alpha");

    let admitted = admit_live_project_path(&state, ALPHA, false, &root).expect("registered root");
    assert_eq!(admitted.canonical, root);

    // A symlink to the registered root resolves to it.
    let link_parent = tempfile::TempDir::new().unwrap();
    let link = link_parent.path().join("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    let through_link =
        admit_live_project_path(&state, ALPHA, false, &link).expect("symlink to the root");
    assert_eq!(through_link.canonical, root);
}

#[test]
fn another_key_a_subdirectory_and_an_ancestor_are_refused() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let (_folder, root) = register_for(&state, ALPHA, "alpha");
    let inside = root.join("sub");
    std::fs::create_dir_all(&inside).unwrap();
    let parent = root.parent().unwrap().to_owned();

    for (key, path) in [
        (BETA, root.clone()),
        (ALPHA, inside),
        (ALPHA, parent),
        (ALPHA, std::path::PathBuf::from("/")),
    ] {
        let refused = admit_live_project_path(&state, key, false, &path)
            .expect_err(&format!("{key} at {} must be refused", path.display()));
        assert_eq!(refused, LiveProjectPathRefusal::NotRegisteredToCaller);
    }
}

#[test]
fn every_non_operator_failure_is_byte_equal() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let (_folder, _root) = register_for(&state, ALPHA, "alpha");
    let unregistered = tempfile::TempDir::new().unwrap();
    let app_root = state.config.app_root.canonicalize().unwrap();
    let file = unregistered.path().join("a-file");
    std::fs::write(&file, "x").unwrap();

    let cases = [
        unregistered.path().to_owned(),
        unregistered.path().join("does-not-exist"),
        std::path::PathBuf::from("relative/path"),
        file,
        app_root,
        std::path::PathBuf::from("/nonexistent-root-for-live-project-test"),
    ];
    let mut bodies = std::collections::BTreeSet::new();
    for path in &cases {
        let refused = admit_live_project_path(&state, BETA, false, path)
            .expect_err(&format!("{} must be refused", path.display()));
        bodies.insert(refused.to_string());
    }
    assert_eq!(bodies.len(), 1, "the refusals differ: {bodies:?}");
    assert!(
        bodies
            .iter()
            .next()
            .unwrap()
            .starts_with(ryeos_app::live_project_path::LIVE_PROJECT_NOT_REGISTERED_TO_CALLER)
    );
}

#[test]
fn the_operator_keeps_its_own_answers() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let operator = test_state::local_operator_context(&state, vec!["*".into()]);
    let unregistered = tempfile::TempDir::new().unwrap();

    // Any directory the node-internal guard allows, registered or not.
    let admitted =
        admit_live_project_path(&state, &operator.fingerprint, true, unregistered.path())
            .expect("the operator names an unregistered directory");
    assert_eq!(
        admitted.canonical,
        unregistered.path().canonicalize().unwrap()
    );

    let app_root = state.config.app_root.canonicalize().unwrap();
    assert!(matches!(
        admit_live_project_path(&state, &operator.fingerprint, true, &app_root),
        Err(LiveProjectPathRefusal::NodeInternal(_))
    ));
    assert!(matches!(
        admit_live_project_path(
            &state,
            &operator.fingerprint,
            true,
            &unregistered.path().join("missing")
        ),
        Err(LiveProjectPathRefusal::BadPath(_))
    ));
    assert!(matches!(
        admit_live_project_path(&state, &operator.fingerprint, true, "relative".as_ref()),
        Err(LiveProjectPathRefusal::BadPath(_))
    ));
}
