//! End to end: live project registrations and the `ryeos project` verbs.
//!
//! The verbs are signed offline operations applied to a stopped node, so
//! these tests apply them in process with the on-volume local operator key,
//! exactly as `ryeos project register|unregister` does, then load the
//! registrations the way `ryeos node doctor` does. Registrations are inert
//! until the owner+name trust cut; nothing here judges trust.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use lillux::crypto::SigningKey;
use rand::rngs::OsRng;
use ryeos_engine::roots::RuntimeRoot;
use ryeos_engine::trust::{PublisherTrustDoc, compute_fingerprint};
use ryeos_node::offline_operation::BareHost;
use ryeos_node::offline_operation::apply::{LocalDraft, apply_local};
use ryeos_node::project_registry::{LoadedRegistration, load_registrations, projects_dir};
use ryeos_node_runtime::identity::NodeIdentity;
use serde_json::{Value, json};

/// The principal a key owns when it has no succession.
fn solo_principal(fingerprint: &str) -> ryeos_state::principal_id::PrincipalId {
    ryeos_engine::trust::SuccessionGraph::default()
        .ownership_of(fingerprint)
        .expect("a key owns a principal")
        .principal
}

struct StoppedNode {
    root: tempfile::TempDir,
    identity: NodeIdentity,
    operator: SigningKey,
}

impl StoppedNode {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("node root");
        let runtime = RuntimeRoot::new(root.path().to_path_buf());
        let identity =
            NodeIdentity::create(&runtime.node_signing_key_path()).expect("node identity");
        identity
            .write_public_identity(&ryeos_engine::trust::node_public_identity_path(root.path()))
            .expect("node public identity");
        let operator = lillux::crypto::create_signing_key(&runtime.operator_signing_key_path())
            .expect("operator key");
        ryeos_node_runtime::operator_identity::test_support::write_operator_records_for_key(
            root.path(),
            identity.signing_key(),
            &operator,
        );
        std::fs::create_dir_all(runtime.state()).expect("state directory");
        Self {
            root,
            identity,
            operator,
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    /// The principal the operator key owns: a key with no succession owns its
    /// own solo principal.
    fn operator_principal(&self) -> ryeos_state::principal_id::PrincipalId {
        solo_principal(&compute_fingerprint(&self.operator.verifying_key()))
    }

    fn try_apply(&self, operation: &str, arguments: Value) -> anyhow::Result<Value> {
        let applied = apply_local(
            self.path(),
            &BareHost,
            &self.operator,
            LocalDraft {
                operation: operation.to_owned(),
                arguments,
                inputs: BTreeMap::new(),
                expected_result: BTreeMap::new(),
            },
        )?;
        Ok(applied.result)
    }

    fn register(&self, arguments: Value) -> anyhow::Result<Value> {
        self.try_apply("node.project.register", arguments)
    }

    fn unregister(&self, arguments: Value) -> anyhow::Result<Value> {
        self.try_apply("node.project.unregister", arguments)
    }

    fn registrations(&self) -> Vec<LoadedRegistration> {
        load_registrations(self.path(), self.identity.verifying_key())
            .expect("registrations load and verify")
    }
}

/// A checkout outside the node root whose tree declares `name`.
fn checkout(parent: &Path, directory: &str, declared: Option<&str>) -> PathBuf {
    let root = parent.join(directory);
    std::fs::create_dir_all(root.join(".ai")).unwrap();
    if let Some(name) = declared {
        std::fs::write(root.join(".ai/manifest.yaml"), format!("name: {name}\n")).unwrap();
    }
    root.canonicalize().unwrap()
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn register_writes_a_node_signed_record_under_the_operator_and_declared_name() {
    let node = StoppedNode::new();
    let work = tempfile::tempdir().unwrap();
    let demo = checkout(work.path(), "demo", Some("demo"));

    let result = node.register(json!({ "path": text(&demo) })).unwrap();
    let identity = format!("{}/demo", node.operator_principal());
    assert_eq!(result["identity"], json!(identity));
    assert_eq!(result["changed"], json!(true));
    assert_eq!(result["paths"], json!([text(&demo)]));

    let loaded = node.registrations();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].registration.identity.to_string(), identity);
    assert_eq!(loaded[0].registration.paths, vec![demo.clone()]);
    let owner_directory = node.operator_principal().storage_component();
    assert_eq!(
        loaded[0].file,
        projects_dir(node.path())
            .join(owner_directory)
            .join("demo.yaml")
    );

    // Registering the same checkout again rewrites nothing.
    let bytes = std::fs::read(&loaded[0].file).unwrap();
    let again = node
        .register(json!({ "path": text(&demo), "name": "demo" }))
        .unwrap();
    assert_eq!(again["changed"], json!(false));
    assert_eq!(std::fs::read(&loaded[0].file).unwrap(), bytes);
}

#[test]
fn a_second_checkout_joins_the_record_and_unregister_removes_paths_then_the_record() {
    let node = StoppedNode::new();
    let work = tempfile::tempdir().unwrap();
    let first = checkout(work.path(), "a-first", Some("demo"));
    let second = checkout(work.path(), "b-second", Some("demo"));
    node.register(json!({ "path": text(&first) })).unwrap();
    node.register(json!({ "path": text(&second) })).unwrap();
    assert_eq!(
        node.registrations()[0].registration.paths,
        vec![first.clone(), second.clone()]
    );

    let removed = node.unregister(json!({ "path": text(&first) })).unwrap();
    assert_eq!(removed["changed"], json!(true));
    assert_eq!(removed["paths"], json!([text(&second)]));
    assert_eq!(
        node.registrations()[0].registration.paths,
        vec![second.clone()]
    );

    let gone = node.unregister(json!({ "path": text(&second) })).unwrap();
    assert_eq!(gone["paths"], json!([]));
    assert!(node.registrations().is_empty());

    // Unregistering what is not registered changes nothing.
    let nothing = node.unregister(json!({ "path": text(&second) })).unwrap();
    assert_eq!(nothing["changed"], json!(false));
}

#[test]
fn a_deleted_checkout_is_unregistered_by_its_stated_name() {
    let node = StoppedNode::new();
    let work = tempfile::tempdir().unwrap();
    let demo = checkout(work.path(), "demo", Some("demo"));
    node.register(json!({ "path": text(&demo) })).unwrap();
    std::fs::remove_dir_all(&demo).unwrap();

    let error = node.unregister(json!({ "path": text(&demo) })).unwrap_err();
    assert!(
        format!("{error:#}").contains("project_name_required"),
        "{error:#}"
    );
    let removed = node
        .unregister(json!({ "path": text(&demo), "name": "demo" }))
        .unwrap();
    assert_eq!(removed["changed"], json!(true));
    assert!(node.registrations().is_empty());
}

#[test]
fn a_checkout_must_declare_the_registered_name() {
    let node = StoppedNode::new();
    let work = tempfile::tempdir().unwrap();
    let undeclared = checkout(work.path(), "plain", None);
    let error = node
        .register(json!({ "path": text(&undeclared) }))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("project_manifest_missing"),
        "{error:#}"
    );

    let demo = checkout(work.path(), "demo", Some("demo"));
    let error = node
        .register(json!({ "path": text(&demo), "name": "other" }))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("project_name_mismatch"),
        "{error:#}"
    );
    assert!(node.registrations().is_empty());
}

#[test]
fn node_internal_roots_are_refused() {
    let node = StoppedNode::new();
    let inside = checkout(node.path(), "inside", Some("inside"));
    let error = node.register(json!({ "path": text(&inside) })).unwrap_err();
    assert!(
        format!("{error:#}").contains("inside the node app root"),
        "{error:#}"
    );
    assert!(node.registrations().is_empty());
}

#[test]
fn the_node_key_and_the_authoring_key_own_no_project() {
    let node = StoppedNode::new();
    let work = tempfile::tempdir().unwrap();
    let demo = checkout(work.path(), "demo", Some("demo"));

    let node_owner = solo_principal(node.identity.fingerprint()).to_string();
    let error = node
        .register(json!({ "path": text(&demo), "owner": node_owner }))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("project_owner_is_node_key"),
        "{error:#}"
    );

    let authoring = SigningKey::generate(&mut OsRng);
    let trust = PublisherTrustDoc {
        public_key: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(authoring.verifying_key().as_bytes())
        ),
        fingerprint: compute_fingerprint(&authoring.verifying_key()),
        owner: "e2e-authoring".to_owned(),
    }
    .to_toml();
    apply_local(
        node.path(),
        &BareHost,
        &node.operator,
        LocalDraft {
            operation: "node.trust.pin".to_owned(),
            arguments: json!({ "role": "authoring", "spaces": ["project"], "kinds": [], "projects": [] }),
            inputs: BTreeMap::from([("signer".to_owned(), trust.into_bytes())]),
            expected_result: BTreeMap::new(),
        },
    )
    .expect("pin the authoring key");
    let authoring_owner =
        solo_principal(&compute_fingerprint(&authoring.verifying_key())).to_string();
    let error = node
        .register(json!({ "path": text(&demo), "owner": authoring_owner }))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("project_owner_is_authoring_key"),
        "{error:#}"
    );

    // Another principal the operator names explicitly is accepted.
    let other = format!("pid:v1:{}", "c".repeat(64));
    let result = node
        .register(json!({ "path": text(&demo), "owner": other }))
        .unwrap();
    assert_eq!(result["identity"], json!(format!("{other}/demo")));
}

#[test]
fn a_record_edited_after_signing_is_refused_when_loaded() {
    let node = StoppedNode::new();
    let work = tempfile::tempdir().unwrap();
    let demo = checkout(work.path(), "demo", Some("demo"));
    node.register(json!({ "path": text(&demo) })).unwrap();
    let file = node.registrations()[0].file.clone();
    let edited = std::fs::read_to_string(&file)
        .unwrap()
        .replace(text(&demo), "/somewhere/else");
    std::fs::write(&file, edited).unwrap();
    assert!(load_registrations(node.path(), node.identity.verifying_key()).is_err());

    // A stray file beside the records is refused too.
    std::fs::write(&file, b"not a record").unwrap();
    assert!(load_registrations(node.path(), node.identity.verifying_key()).is_err());
}
