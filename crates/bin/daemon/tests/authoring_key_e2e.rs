//! End to end: the authoring key's registry scope and its rotation.
//!
//! The authoring key is pinned with no kinds, the operator widens it per kind
//! with `ryeos trust scope`, and `ryeos node authoring rotate` replaces it.
//! The verbs are signed offline operations applied to a stopped node, so these
//! tests apply them in process with the on-volume local operator key, exactly
//! as the commands do, then load the registry the way every verification site
//! does and judge real signed content against it.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_engine::contracts::{SignatureEnvelope, SignatureHeader, TrustClass};
use ryeos_engine::roots::RuntimeRoot;
use ryeos_engine::trust::{
    PublisherTrustDoc, SignerContext, SignerRole, SignerScope, SignerStatus, TrustStore,
    compute_fingerprint, verify_item_signature,
};
use ryeos_node::key_rotation::{RotatedKey, prepare_rotation};
use ryeos_node::offline_operation::BareHost;
use ryeos_node::offline_operation::apply::{LocalDraft, apply_local};
use ryeos_node_runtime::identity::NodeIdentity;
use serde_json::{Value, json};

const PROJECT: &str = "/srv/projects/alpha";

/// A stopped node with an identity, a local operator key, and an authoring
/// key at its canonical path, pinned the way `ryeos init` pins it.
struct StoppedNode {
    root: tempfile::TempDir,
    authoring: SigningKey,
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
        lillux::crypto::create_signing_key(&runtime.operator_signing_key_path())
            .expect("operator key");
        std::fs::create_dir_all(runtime.state()).expect("state directory");
        let authoring = lillux::crypto::create_signing_key(&runtime.authoring_signing_key_path())
            .expect("authoring key");
        let node = Self { root, authoring };
        node.apply(
            "node.trust.pin",
            json!({ "role": "authoring", "spaces": [], "kinds": [], "projects": [] }),
            &[("signer", trust_document(&node.authoring).into_bytes())],
        );
        node
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn apply(&self, operation: &str, arguments: Value, inputs: &[(&str, Vec<u8>)]) -> Value {
        apply_local(
            self.path(),
            &BareHost,
            LocalDraft {
                operation: operation.to_owned(),
                arguments,
                inputs: inputs
                    .iter()
                    .map(|(name, bytes)| ((*name).to_owned(), bytes.clone()))
                    .collect::<BTreeMap<_, _>>(),
                expected_result: BTreeMap::new(),
            },
        )
        .unwrap_or_else(|error| panic!("{operation} refused: {error:#}"))
        .result
    }

    fn registry(&self) -> TrustStore {
        TrustStore::load_node_registry(self.path()).expect("registry loads and verifies")
    }

    fn installed_authoring(&self) -> SigningKey {
        lillux::crypto::load_signing_key(
            &RuntimeRoot::new(self.path().to_path_buf()).authoring_signing_key_path(),
        )
        .expect("installed authoring key")
    }

    /// `ryeos trust scope <fp> --kind <kind>`
    fn admit_kind(&self, fingerprint: &str, kind: &str) {
        self.apply(
            "node.trust.scope",
            json!({
                "fingerprint": fingerprint,
                "spaces": ["project"],
                "kinds": [kind],
                "projects": [],
            }),
            &[],
        );
    }

    /// `ryeos node authoring rotate`
    fn rotate_authoring(&self) -> (Value, String) {
        let prepared = prepare_rotation(self.path(), RotatedKey::Authoring).expect("prepare");
        let result = self.apply(
            RotatedKey::Authoring.operation(),
            json!({}),
            &[("succession", prepared.statement)],
        );
        (result, prepared.new_fingerprint)
    }
}

fn fingerprint(key: &SigningKey) -> String {
    compute_fingerprint(&key.verifying_key())
}

fn trust_document(key: &SigningKey) -> String {
    PublisherTrustDoc {
        public_key: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
        ),
        fingerprint: fingerprint(key),
        owner: "authoring".to_owned(),
    }
    .to_toml()
}

fn envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "#".to_owned(),
        suffix: None,
        after_shebang: false,
    }
}

fn signed_by(key: &SigningKey) -> (String, SignatureHeader) {
    let content = lillux::signature::sign_content("name: authored-item\n", key, "#", None);
    let header = ryeos_engine::item_resolution::parse_signature_header(&content, &envelope())
        .expect("signature header");
    (content, header)
}

/// The class `key`'s signature earns for an item of `kind` in the project.
fn class_of(store: &TrustStore, key: &SigningKey, kind: &str) -> TrustClass {
    let (content, header) = signed_by(key);
    verify_item_signature(
        &content,
        &header,
        &envelope(),
        store,
        &SignerContext::project(kind, Some(Path::new(PROJECT))),
    )
    .map(|(class, _)| class)
    .expect("verification")
}

#[test]
fn the_authoring_key_is_pinned_with_no_kinds_and_signs_nothing_trusted() {
    let node = StoppedNode::new();
    let store = node.registry();
    let record = store
        .signer(&fingerprint(&node.authoring))
        .expect("authoring record");
    assert_eq!(record.role, SignerRole::Authoring);
    assert_eq!(record.status, SignerStatus::Active);
    assert_eq!(record.scope, SignerScope::authoring_default());
    for kind in ["knowledge", "directive", "graph", "tool"] {
        assert_eq!(
            class_of(&store, &node.authoring, kind),
            TrustClass::Untrusted,
            "{kind}"
        );
    }
}

#[test]
fn widening_the_scope_for_one_kind_trusts_only_that_kind() {
    let node = StoppedNode::new();
    node.admit_kind(&fingerprint(&node.authoring), "knowledge");
    let store = node.registry();
    assert_eq!(
        class_of(&store, &node.authoring, "knowledge"),
        TrustClass::Trusted
    );
    assert_eq!(
        class_of(&store, &node.authoring, "graph"),
        TrustClass::Untrusted
    );
}

#[test]
fn authoring_rotation_retires_the_old_key_and_the_new_one_keeps_the_widened_scope() {
    let node = StoppedNode::new();
    let old = node.authoring.clone();
    node.admit_kind(&fingerprint(&old), "knowledge");

    let (result, new_fingerprint) = node.rotate_authoring();
    assert_eq!(result["old_fingerprint"], fingerprint(&old));
    assert_eq!(result["new_fingerprint"], new_fingerprint);
    assert_eq!(result["already_rotated"], false);

    // The new key is the one at the canonical path.
    let installed = node.installed_authoring();
    assert_eq!(fingerprint(&installed), new_fingerprint);
    assert!(
        !RuntimeRoot::new(node.path().to_path_buf())
            .authoring_dir()
            .join("rotation")
            .exists()
    );

    let store = node.registry();
    let old_record = store.signer(&fingerprint(&old)).expect("old record");
    let new_record = store.signer(&new_fingerprint).expect("new record");
    assert_eq!(old_record.status, SignerStatus::Retired);
    assert_eq!(new_record.status, SignerStatus::Active);
    assert_eq!(new_record.role, SignerRole::Authoring);
    assert_eq!(new_record.scope, old_record.scope);

    // Items the retired key signed still verify, within the scope it kept.
    assert_eq!(
        class_of(&store, &old, "knowledge"),
        TrustClass::Trusted,
        "a retired key still verifies what it signed"
    );
    // The new key signs trusted knowledge and nothing else.
    assert_eq!(
        class_of(&store, &installed, "knowledge"),
        TrustClass::Trusted
    );
    assert_eq!(class_of(&store, &installed, "graph"), TrustClass::Untrusted);
}

#[test]
fn a_revoked_authoring_key_verifies_nothing_it_signed() {
    let node = StoppedNode::new();
    let old = node.authoring.clone();
    node.admit_kind(&fingerprint(&old), "knowledge");
    node.rotate_authoring();
    node.apply(
        "node.trust.revoke",
        json!({ "fingerprint": fingerprint(&old) }),
        &[],
    );
    let store = node.registry();
    let (content, header) = signed_by(&old);
    let error = verify_item_signature(
        &content,
        &header,
        &envelope(),
        &store,
        &SignerContext::project("knowledge", Some(Path::new(PROJECT))),
    )
    .expect_err("a revoked signer is refused");
    assert!(
        error.to_string().contains("trust_signer_revoked"),
        "{error}"
    );
}

#[test]
fn rotating_twice_follows_the_succession_chain() {
    let node = StoppedNode::new();
    let first = node.authoring.clone();
    let (_, second) = node.rotate_authoring();
    let (_, third) = node.rotate_authoring();
    let store = node.registry();
    assert_eq!(
        store.signer(&fingerprint(&first)).unwrap().status,
        SignerStatus::Retired
    );
    assert_eq!(store.signer(&second).unwrap().status, SignerStatus::Retired);
    assert_eq!(store.signer(&third).unwrap().status, SignerStatus::Active);
    assert_eq!(fingerprint(&node.installed_authoring()), third);
}
