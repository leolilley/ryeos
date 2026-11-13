//! End to end: the node-owned signer registry and the `ryeos trust` verbs.
//!
//! The verbs are signed offline operations applied to a stopped node, so
//! these tests apply them in process with the on-volume local operator key,
//! exactly as `ryeos trust pin|scope|retire|revoke|succeed` does, then load
//! the registry the way every verification site does and judge real signed
//! content against it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use lillux::crypto::SigningKey;
use rand::rngs::OsRng;
use ryeos_engine::contracts::{SignatureEnvelope, SignatureHeader, TrustClass};
use ryeos_engine::error::EngineError;
use ryeos_engine::roots::RuntimeRoot;
use ryeos_engine::trust::{
    PublisherTrustDoc, SignerContext, SignerRole, SignerStatus, SuccessionStatement, TrustStore,
    compute_fingerprint, verify_item_signature, verify_node_signed_document,
};
use ryeos_node::offline_operation::BareHost;
use ryeos_node::offline_operation::apply::{LocalDraft, apply_local};
use ryeos_node_runtime::identity::NodeIdentity;
use serde_json::{Value, json};

const PROJECT: &str = "/srv/projects/alpha";
const OTHER_PROJECT: &str = "/srv/projects/beta";

/// A stopped node with an identity, a local operator key and an empty
/// registry.
struct StoppedNode {
    root: tempfile::TempDir,
    identity: NodeIdentity,
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
        Self { root, identity }
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
        let applied = apply_local(
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
        )?;
        Ok(applied.result)
    }

    /// `ryeos trust pin --from <key's trust document> --role <role> ...`
    fn pin(
        &self,
        key: &SigningKey,
        role: &str,
        spaces: &[&str],
        projects: &[&str],
    ) -> anyhow::Result<Value> {
        self.try_apply(
            "node.trust.pin",
            json!({ "role": role, "spaces": spaces, "kinds": [], "projects": projects }),
            &[("signer", trust_document(key).into_bytes())],
        )
    }

    fn registry(&self) -> TrustStore {
        TrustStore::load_node_registry(self.path()).expect("registry loads and verifies")
    }

    fn event_count(&self) -> usize {
        std::fs::read_dir(RuntimeRoot::new(self.path().to_path_buf()).trust_events_dir())
            .map(|entries| entries.count())
            .unwrap_or(0)
    }
}

fn author_key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

fn fingerprint(key: &SigningKey) -> String {
    compute_fingerprint(&key.verifying_key())
}

/// The `PUBLISHER_TRUST.toml` text `ryeos trust pin --from` reads.
fn trust_document(key: &SigningKey) -> String {
    PublisherTrustDoc {
        public_key: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
        ),
        fingerprint: fingerprint(key),
        owner: "e2e-signer".to_owned(),
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

/// An item body signed by `key`, with its parsed header.
fn signed_by(key: &SigningKey) -> (String, SignatureHeader) {
    let content = lillux::signature::sign_content("name: e2e-item\n", key, "#", None);
    let header = ryeos_engine::item_resolution::parse_signature_header(&content, &envelope())
        .expect("signature header");
    (content, header)
}

fn verify(
    store: &TrustStore,
    content: &str,
    header: &SignatureHeader,
    context: &SignerContext<'_>,
) -> Result<TrustClass, EngineError> {
    verify_item_signature(content, header, &envelope(), store, context).map(|(class, _)| class)
}

fn trust_code(error: &EngineError) -> &'static str {
    match error {
        EngineError::Trust(trust) => trust.code(),
        other => panic!("expected a typed trust error, got {other:?}"),
    }
}

#[test]
fn pin_writes_a_node_signed_record_and_a_key_event() {
    let node = StoppedNode::new();
    let key = author_key();
    assert_eq!(node.event_count(), 0);

    let result = node
        .pin(&key, "project_author", &[], &[PROJECT])
        .expect("pin applies");

    assert_eq!(result["events_written"], 1);
    assert_eq!(node.event_count(), 1);
    let store = node.registry();
    let record = store.signer(&fingerprint(&key)).expect("pinned record");
    assert_eq!(record.role, SignerRole::ProjectAuthor);
    assert_eq!(record.status, SignerStatus::Active);
    assert_eq!(record.scope.projects, vec![PROJECT.to_owned()]);
    // The record on disk is signed by the node key and no other.
    let path = RuntimeRoot::new(node.path().to_path_buf())
        .trust_signers_dir()
        .join(record.file_name());
    let text = std::fs::read_to_string(path).expect("record file");
    assert!(text.starts_with("# ryeos:signed:"), "{text}");
    assert!(text.contains(&compute_fingerprint(node.identity.verifying_key())));
}

#[test]
fn pinning_the_same_key_again_changes_nothing() {
    let node = StoppedNode::new();
    let key = author_key();
    node.pin(&key, "project_author", &[], &[PROJECT])
        .expect("first pin");
    let again = node
        .pin(&key, "project_author", &[], &[PROJECT])
        .expect("second pin");
    assert_eq!(again["events_written"], 0);
    assert_eq!(node.event_count(), 1);
}

#[test]
fn scope_replaces_the_projects_a_signer_may_sign_for() {
    let node = StoppedNode::new();
    let key = author_key();
    node.pin(&key, "project_author", &[], &[PROJECT])
        .expect("pin");
    let (content, header) = signed_by(&key);
    let other = Path::new(OTHER_PROJECT);
    assert_eq!(
        verify(
            &node.registry(),
            &content,
            &header,
            &SignerContext::project("tool", Some(other))
        )
        .unwrap(),
        TrustClass::Untrusted
    );

    let result = node.apply(
        "node.trust.scope",
        json!({
            "fingerprint": fingerprint(&key),
            "spaces": ["project"],
            "kinds": ["*"],
            "projects": [PROJECT, OTHER_PROJECT],
        }),
        &[],
    );

    assert_eq!(result["events_written"], 1);
    assert_eq!(node.event_count(), 2);
    assert_eq!(
        verify(
            &node.registry(),
            &content,
            &header,
            &SignerContext::project("tool", Some(other))
        )
        .unwrap(),
        TrustClass::Trusted
    );
}

#[test]
fn scoped_signer_out_of_scope_is_untrusted_in_a_project_and_refused_elsewhere() {
    let node = StoppedNode::new();
    let key = author_key();
    node.pin(&key, "project_author", &[], &[PROJECT])
        .expect("pin");
    let store = node.registry();
    let (content, header) = signed_by(&key);

    // In the project it is scoped to, the signer is trusted.
    let listed = SignerContext::project("tool", Some(Path::new(PROJECT)));
    assert_eq!(
        verify(&store, &content, &header, &listed).unwrap(),
        TrustClass::Trusted
    );
    // In another project, or with no project, its items are untrusted but
    // still load as untrusted project items.
    for context in [
        SignerContext::project("tool", Some(Path::new(OTHER_PROJECT))),
        SignerContext::project("tool", None),
    ] {
        assert_eq!(
            verify(&store, &content, &header, &context).unwrap(),
            TrustClass::Untrusted
        );
    }
    // Outside project space the verification is refused with a typed code
    // that names the cure.
    for context in [SignerContext::bundle("tool"), SignerContext::node("tool")] {
        let error = verify(&store, &content, &header, &context).unwrap_err();
        assert_eq!(trust_code(&error), "signer_out_of_scope");
        assert!(error.to_string().contains("ryeos trust scope"), "{error}");
    }
}

#[test]
fn retired_signer_still_verifies_existing_items() {
    let node = StoppedNode::new();
    let key = author_key();
    node.pin(&key, "project_author", &[], &[PROJECT])
        .expect("pin");

    let result = node.apply(
        "node.trust.retire",
        json!({ "fingerprint": fingerprint(&key) }),
        &[],
    );

    assert_eq!(result["events_written"], 1);
    let store = node.registry();
    assert_eq!(
        store.signer(&fingerprint(&key)).unwrap().status,
        SignerStatus::Retired
    );
    let (content, header) = signed_by(&key);
    let listed = SignerContext::project("tool", Some(Path::new(PROJECT)));
    assert_eq!(
        verify(&store, &content, &header, &listed).unwrap(),
        TrustClass::Trusted
    );
}

#[test]
fn revoked_signer_verifies_nothing_and_cannot_be_pinned_again() {
    let node = StoppedNode::new();
    let key = author_key();
    node.pin(&key, "project_author", &[], &[PROJECT])
        .expect("pin");

    node.apply(
        "node.trust.revoke",
        json!({ "fingerprint": fingerprint(&key) }),
        &[],
    );

    let store = node.registry();
    assert_eq!(
        store.signer(&fingerprint(&key)).unwrap().status,
        SignerStatus::Revoked
    );
    let (content, header) = signed_by(&key);
    let listed = SignerContext::project("tool", Some(Path::new(PROJECT)));
    let error = verify(&store, &content, &header, &listed).unwrap_err();
    assert_eq!(trust_code(&error), "trust_signer_revoked");

    let refusal = node
        .pin(&key, "project_author", &[], &[PROJECT])
        .expect_err("a revoked key cannot be pinned again");
    assert!(
        format!("{refusal:#}").contains("revoked"),
        "unexpected refusal: {refusal:#}"
    );
}

#[test]
fn succession_moves_role_and_scope_to_the_new_key_and_retires_the_old() {
    let node = StoppedNode::new();
    let old = author_key();
    let new = author_key();
    node.pin(&old, "project_author", &[], &[PROJECT])
        .expect("pin the old key");
    let statement = SuccessionStatement::new(
        &fingerprint(&old),
        &new.verifying_key(),
        &lillux::time::iso8601_now(),
    )
    .render_signed(&old)
    .expect("statement signed by the old key");

    let result = node.apply(
        "node.trust.succeed",
        json!({ "old_fingerprint": fingerprint(&old) }),
        &[("succession", statement.into_bytes())],
    );

    assert!(result["events_written"].as_u64().unwrap() >= 1);
    let store = node.registry();
    let old_record = store.signer(&fingerprint(&old)).expect("old record");
    assert_eq!(old_record.status, SignerStatus::Retired);
    let new_record = store.signer(&fingerprint(&new)).expect("new record");
    assert_eq!(new_record.status, SignerStatus::Active);
    assert_eq!(new_record.role, SignerRole::ProjectAuthor);
    assert_eq!(new_record.scope.projects, vec![PROJECT.to_owned()]);
    let (content, header) = signed_by(&new);
    let listed = SignerContext::project("tool", Some(Path::new(PROJECT)));
    assert_eq!(
        verify(&store, &content, &header, &listed).unwrap(),
        TrustClass::Trusted
    );
}

#[test]
fn a_succession_statement_signed_by_another_key_is_refused() {
    let node = StoppedNode::new();
    let old = author_key();
    let new = author_key();
    let forger = author_key();
    node.pin(&old, "project_author", &[], &[PROJECT])
        .expect("pin the old key");
    let forged = SuccessionStatement::new(
        &fingerprint(&old),
        &new.verifying_key(),
        &lillux::time::iso8601_now(),
    )
    .render_signed(&forger)
    .expect("statement");

    let refusal = node
        .try_apply(
            "node.trust.succeed",
            json!({ "old_fingerprint": fingerprint(&old) }),
            &[("succession", forged.into_bytes())],
        )
        .expect_err("a statement the old key did not sign must be refused");

    // The refusal names the cure and nothing was pinned.
    assert!(!format!("{refusal:#}").is_empty());
    assert!(node.registry().signer(&fingerprint(&new)).is_none());
}

#[test]
fn node_documents_verify_under_the_node_key_only() {
    let node = StoppedNode::new();
    // An operator-role signer whose registry scope admits node space.
    let operator_like = author_key();
    node.pin(&operator_like, "operator", &[], &[])
        .expect("pin an operator-role signer");
    let node_key = node.identity.signing_key().clone();
    let node_vk = node.identity.verifying_key();

    let (forged, forged_header) = signed_by(&operator_like);
    let error = verify_node_signed_document(&forged, &forged_header, &envelope(), node_vk)
        .expect_err("a pinned signer cannot sign node documents");
    assert!(
        error.to_string().contains("not by this node's key"),
        "{error}"
    );

    let (genuine, genuine_header) = signed_by(&node_key);
    verify_node_signed_document(&genuine, &genuine_header, &envelope(), node_vk)
        .expect("the node key signs node documents");
}

#[test]
fn a_record_edited_after_signing_is_refused_when_the_registry_loads() {
    let node = StoppedNode::new();
    let key = author_key();
    node.pin(&key, "project_author", &[], &[PROJECT])
        .expect("pin");
    let path: PathBuf = RuntimeRoot::new(node.path().to_path_buf())
        .trust_signers_dir()
        .join(format!("{}.yaml", fingerprint(&key)));
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace(PROJECT, OTHER_PROJECT)).unwrap();

    let error = TrustStore::load_node_registry(node.path()).expect_err("tampered record");

    assert_eq!(error.code(), "trust_record_tampered");
}
