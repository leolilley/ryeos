//! End to end: the authoring key's registry scope and its rotation.
//!
//! The authoring key is pinned with no kinds and nothing it signs is trusted
//! by that scope: an operator endorsement is what trusts an authored item. The
//! registry scope of an authoring key is not widened (`ryeos trust scope`
//! refuses, and a node whose record was widened under the old rule refuses to
//! load until the scope is reset), and `ryeos node authoring rotate` replaces
//! the key. The verbs are signed offline operations applied to a stopped node,
//! so these tests apply them in process with the on-volume local operator key,
//! exactly as the commands do, then load the registry the way every
//! verification site does and judge real signed content against it.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_engine::contracts::{LogicalProject, SignatureEnvelope, SignatureHeader, TrustClass};
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

/// The stated project the verifications below happen in.
fn stated_project() -> LogicalProject {
    LogicalProject::Stated(
        format!("pid:v1:{}/alpha", "a".repeat(64))
            .parse()
            .expect("project identity"),
    )
}

/// A stopped node with an identity, a local operator key, and an authoring
/// key at its canonical path, pinned the way `ryeos init` pins it.
struct StoppedNode {
    root: tempfile::TempDir,
    authoring: SigningKey,
    /// The operator's command-line tool holds this key; the test plays it.
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
        let authoring = lillux::crypto::create_signing_key(&runtime.authoring_signing_key_path())
            .expect("authoring key");
        let node = Self {
            root,
            authoring,
            operator,
        };
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
        self.try_apply(operation, arguments, inputs)
            .unwrap_or_else(|error| panic!("{operation} refused: {error:#}"))
    }

    fn try_apply(
        &self,
        operation: &str,
        arguments: Value,
        inputs: &[(&str, Vec<u8>)],
    ) -> anyhow::Result<Value> {
        apply_local(
            self.path(),
            &BareHost,
            &self.operator,
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
        .map(|applied| applied.result)
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

    /// The state of a node whose operator widened the authoring key under the
    /// rule this build retires: a signed scope-changed event and the record it
    /// commits to, written the way the registry writer did.
    fn legacy_widen(&self, fingerprint: &str, kind: &str) {
        use ryeos_engine::trust::{
            TRUST_EVENT_SCHEMA, TrustEvent, TrustEventKind, events_dir, signers_dir,
        };
        let view = ryeos_node::trust_registry::load_view(self.path()).expect("registry view");
        let mut record = view
            .records
            .iter()
            .find(|record| record.fingerprint == fingerprint)
            .expect("authoring record")
            .clone();
        record.scope.kinds = vec![kind.to_owned()];
        let node = NodeIdentity::load(
            &RuntimeRoot::new(self.path().to_path_buf()).node_signing_key_path(),
        )
        .expect("node identity");
        let event = TrustEvent {
            schema: TRUST_EVENT_SCHEMA.to_owned(),
            seq: view.chain.len() as u64 + 1,
            previous: view.chain.last().map(TrustEvent::digest),
            kind: TrustEventKind::ScopeChanged,
            signer: fingerprint.to_owned(),
            record_digest: record.content_digest(),
            request_digest: "0".repeat(64),
            operator: compute_fingerprint(&self.operator.verifying_key()),
            predecessor: None,
        };
        std::fs::write(
            events_dir(self.path()).join(event.file_name()),
            event.render_signed(node.signing_key()).expect("event"),
        )
        .expect("write event");
        record.event = event.digest();
        std::fs::write(
            signers_dir(self.path()).join(record.file_name()),
            record.render_signed(node.signing_key()).expect("record"),
        )
        .expect("write record");
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
    let project = stated_project();
    verify_item_signature(
        &content,
        &header,
        &envelope(),
        store,
        &SignerContext::project(kind, &project),
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
fn trust_scope_refuses_to_widen_the_authoring_key_and_pin_refuses_a_wide_one() {
    let node = StoppedNode::new();
    let fp = fingerprint(&node.authoring);
    let error = node
        .try_apply(
            "node.trust.scope",
            json!({ "fingerprint": fp, "spaces": ["project"], "kinds": ["knowledge"], "projects": [] }),
            &[],
        )
        .expect_err("widening an authoring key is refused");
    let text = format!("{error:#}");
    assert!(text.contains("trust_authoring_scope_retired"), "{text}");
    assert!(text.contains("ryeos endorse"), "{text}");

    let second = lillux::crypto::SigningKey::from_bytes(&[77; 32]);
    let error = node
        .try_apply(
            "node.trust.pin",
            json!({ "role": "authoring", "spaces": [], "kinds": ["knowledge"], "projects": [] }),
            &[("signer", trust_document(&second).into_bytes())],
        )
        .expect_err("pinning a wide authoring key is refused");
    assert!(
        format!("{error:#}").contains("trust_authoring_scope_retired"),
        "{error:#}"
    );

    // Restoring the authoring scope is the one scope change it accepts, and it
    // is a no-op on a record that already holds it.
    let result = node.apply(
        "node.trust.scope",
        json!({ "fingerprint": fp, "spaces": ["project"], "kinds": [], "projects": [] }),
        &[],
    );
    assert_eq!(result["events_written"], 0, "{result}");
}

#[test]
fn a_node_with_a_widened_authoring_scope_refuses_to_load_until_it_is_reset() {
    let node = StoppedNode::new();
    let fp = fingerprint(&node.authoring);
    node.legacy_widen(&fp, "knowledge");

    // Every load of the registry names the retired scope and the cure.
    let error = TrustStore::load_node_registry(node.path()).expect_err("widened scope refused");
    assert_eq!(error.code(), "trust_authoring_scope_retired");
    let text = error.to_string();
    assert!(text.contains(&fp), "{text}");
    assert!(text.contains("ryeos trust scope"), "{text}");
    assert!(text.contains("ryeos endorse"), "{text}");

    // The cure is the scope reset on the stopped node; items it signed are
    // untrusted until endorsed.
    let result = node.apply(
        "node.trust.scope",
        json!({ "fingerprint": fp, "spaces": ["project"], "kinds": [], "projects": [] }),
        &[],
    );
    assert_eq!(result["events_written"], 1, "{result}");
    let store = node.registry();
    assert_eq!(
        store.signer(&fp).expect("record").scope,
        SignerScope::authoring_default()
    );
    assert_eq!(
        class_of(&store, &node.authoring, "knowledge"),
        TrustClass::Untrusted
    );
}

#[test]
fn authoring_rotation_retires_the_old_key_and_the_new_one_signs_nothing_trusted() {
    let node = StoppedNode::new();
    let old = node.authoring.clone();

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
    assert_eq!(new_record.scope, SignerScope::authoring_default());

    // A retired authoring key still verifies what it signed, as untrusted
    // until an operator endorses it; so does its successor.
    assert_eq!(class_of(&store, &old, "knowledge"), TrustClass::Untrusted);
    assert_eq!(
        class_of(&store, &installed, "knowledge"),
        TrustClass::Untrusted
    );
}

#[test]
fn a_revoked_authoring_key_verifies_nothing_it_signed() {
    let node = StoppedNode::new();
    let old = node.authoring.clone();
    node.rotate_authoring();
    node.apply(
        "node.trust.revoke",
        json!({ "fingerprint": fingerprint(&old) }),
        &[],
    );
    let store = node.registry();
    let (content, header) = signed_by(&old);
    let project = stated_project();
    let error = verify_item_signature(
        &content,
        &header,
        &envelope(),
        &store,
        &SignerContext::project("knowledge", &project),
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
