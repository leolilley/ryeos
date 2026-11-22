use std::path::Path;

use lillux::crypto::SigningKey;

use super::*;
use crate::authority::trust::{
    SignerContext, SignerRecord, SignerScope, SignerStatus, compute_fingerprint,
    trusted_signers_digest, verify_item_signature,
};
use crate::contracts::{SignatureEnvelope, SignatureHeader, TrustClass};
use ryeos_state::project_identity::{LogicalProject, ProjectIdentity, ProjectName};

const PROJECT: &str = "alpha";
const OTHER_PROJECT: &str = "beta";
const NOW: u64 = 1_800_000_000;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn fp(key: &SigningKey) -> String {
    compute_fingerprint(&key.verifying_key())
}

fn principal(key: &SigningKey) -> String {
    format!("fp:{}", fp(key))
}

/// The project `name` owned by `key`'s principal.
fn identity(key: &SigningKey, name: &str) -> ProjectIdentity {
    ProjectIdentity::new(principal(key), ProjectName::new(name).unwrap()).unwrap()
}

fn record(key: &SigningKey, role: SignerRole, status: SignerStatus) -> SignerRecord {
    let scope = match role {
        SignerRole::Authoring => SignerScope::authoring_default(),
        SignerRole::Operator => SignerScope::operator_default(),
        SignerRole::ProjectAuthor => SignerScope {
            spaces: vec![crate::authority::trust::SignerSpace::Project],
            kinds: vec!["*".to_owned()],
            projects: Vec::new(),
        },
        SignerRole::Publisher => SignerScope::publisher_default(),
    };
    let mut record = SignerRecord::new(&key.verifying_key(), "test", role, scope, status);
    record.event = "e".repeat(64);
    record
}

struct Node {
    root: tempfile::TempDir,
    node: SigningKey,
    operator: SigningKey,
    authoring: SigningKey,
}

impl Node {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            node: key(1),
            operator: key(2),
            authoring: key(3),
        }
    }

    /// The operator's project named `PROJECT`.
    fn project(&self) -> ProjectIdentity {
        identity(&self.operator, PROJECT)
    }

    /// The operator's project named `OTHER_PROJECT`.
    fn other_project(&self) -> ProjectIdentity {
        identity(&self.operator, OTHER_PROJECT)
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn trust(&self) -> TrustStore {
        self.trust_with(vec![
            record(&self.operator, SignerRole::Operator, SignerStatus::Active),
            record(&self.authoring, SignerRole::Authoring, SignerStatus::Active),
        ])
    }

    fn trust_with(&self, records: Vec<SignerRecord>) -> TrustStore {
        TrustStore::from_registry(self.node.verifying_key(), records).unwrap()
    }

    fn ledger(&self) -> EndorsementLedger {
        EndorsementLedger::load(self.path(), &self.node.verifying_key()).unwrap()
    }

    fn head(&self) -> String {
        self.ledger().head(&self.node.verifying_key())
    }

    /// The trust store with the index of everything recorded so far.
    fn indexed(&self, trust: &TrustStore) -> TrustStore {
        let index = EndorsementIndex::build(&self.ledger(), trust).unwrap();
        trust.clone().with_endorsement_index(index)
    }

    fn items_body(
        &self,
        endorser: &SigningKey,
        project_name: &str,
        items: Vec<EndorsedItem>,
    ) -> EndorsementBody {
        EndorsementBody::for_items(
            &identity(endorser, project_name),
            &principal(endorser),
            items,
            &lillux::time::iso8601_from_unix_secs(NOW),
            &nonce(),
            &self.head(),
        )
    }

    fn standing_body(
        &self,
        endorser: &SigningKey,
        project_name: &str,
        kinds: &[&str],
        not_after: Option<u64>,
    ) -> EndorsementBody {
        EndorsementBody::standing(
            &identity(endorser, project_name),
            &principal(endorser),
            kinds.iter().map(|kind| (*kind).to_owned()).collect(),
            not_after.map(lillux::time::iso8601_from_unix_secs),
            &lillux::time::iso8601_from_unix_secs(NOW),
            &nonce(),
            &self.head(),
        )
    }

    fn record(
        &self,
        trust: &TrustStore,
        signer: &SigningKey,
        body: EndorsementBody,
    ) -> Result<Recorded, EndorsementRefusal> {
        let signed = SignedEndorsement::sign(body, signer).unwrap();
        record_endorsement(self.path(), &self.node, trust, signed, NOW)
    }
}

fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{:032x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "#".to_owned(),
        suffix: None,
        after_shebang: false,
    }
}

/// An item `signer` signed, with the digest an endorsement names.
fn signed_item(signer: &SigningKey, body: &str) -> (String, SignatureHeader, String) {
    let content = lillux::signature::sign_content(body, signer, "#", None);
    let header =
        crate::item_resolution::parse_signature_header(&content, &envelope()).expect("header");
    let digest = header.content_hash.clone();
    (content, header, digest)
}

fn class(
    trust: &TrustStore,
    item: &(String, SignatureHeader, String),
    kind: &str,
    project: &ProjectIdentity,
) -> TrustClass {
    verify_item_signature(
        &item.0,
        &item.1,
        &envelope(),
        trust,
        &SignerContext::project(kind, &LogicalProject::Stated(project.clone())),
    )
    .map(|(class, _)| class)
    .unwrap()
}

fn item_for(kind: &str, digest: &str) -> EndorsedItem {
    EndorsedItem {
        item_ref: format!("{kind}:acme/thing"),
        kind: kind.to_owned(),
        digest: digest.to_owned(),
    }
}

#[test]
fn a_per_change_endorsement_flips_the_item_from_untrusted_to_trusted() {
    let node = Node::new();
    let trust = node.trust();
    let item = signed_item(&node.authoring, "name: view\n");
    assert_eq!(
        class(&trust, &item, "view", &node.project()),
        TrustClass::Untrusted
    );

    node.record(
        &trust,
        &node.operator,
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
    )
    .unwrap();
    let endorsed = node.indexed(&trust);
    assert_eq!(
        class(&endorsed, &item, "view", &node.project()),
        TrustClass::Trusted
    );
}

#[test]
fn a_changed_byte_or_another_kind_or_project_stays_untrusted() {
    let node = Node::new();
    let trust = node.trust();
    let item = signed_item(&node.authoring, "name: view\n");
    node.record(
        &trust,
        &node.operator,
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
    )
    .unwrap();
    let endorsed = node.indexed(&trust);

    let changed = signed_item(&node.authoring, "name: view2\n");
    assert_ne!(changed.2, item.2);
    assert_eq!(
        class(&endorsed, &changed, "view", &node.project()),
        TrustClass::Untrusted,
        "per-change digest mismatch"
    );
    assert_eq!(
        class(&endorsed, &item, "surface", &node.project()),
        TrustClass::Untrusted,
        "another kind"
    );
    assert_eq!(
        class(&endorsed, &item, "view", &node.other_project()),
        TrustClass::Untrusted,
        "another project"
    );
}

#[test]
fn a_standing_endorsement_covers_its_kinds_in_its_project_only() {
    let node = Node::new();
    let trust = node.trust();
    node.record(
        &trust,
        &node.operator,
        node.standing_body(&node.operator, PROJECT, &["knowledge"], None),
    )
    .unwrap();
    let endorsed = node.indexed(&trust);
    let item = signed_item(&node.authoring, "text\n");
    assert_eq!(
        class(&endorsed, &item, "knowledge", &node.project()),
        TrustClass::Trusted
    );
    assert_eq!(
        class(&endorsed, &item, "directive", &node.project()),
        TrustClass::Untrusted,
        "a kind the standing endorsement does not name"
    );
    assert_eq!(
        class(&endorsed, &item, "knowledge", &node.other_project()),
        TrustClass::Untrusted,
        "another project"
    );
}

#[test]
fn a_standing_endorsement_lapses_at_its_not_after() {
    let node = Node::new();
    let trust = node.trust();
    node.record(
        &trust,
        &node.operator,
        node.standing_body(&node.operator, PROJECT, &["knowledge"], Some(NOW + 600)),
    )
    .unwrap();
    let index = EndorsementIndex::build(&node.ledger(), &trust).unwrap();
    let digest = "a".repeat(64);
    assert!(index.covers(&node.project(), "knowledge", &digest, NOW + 600));
    assert!(!index.covers(&node.project(), "knowledge", &digest, NOW + 601));
    assert_ne!(
        index.identity(NOW),
        index.identity(NOW + 601),
        "the identity request caches key on retires when the endorsement lapses"
    );
}

#[test]
fn revoking_an_endorsement_flips_the_item_back_and_a_restored_file_stays_dead() {
    let node = Node::new();
    let trust = node.trust();
    let item = signed_item(&node.authoring, "name: view\n");
    let recorded = node
        .record(
            &trust,
            &node.operator,
            node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
        )
        .unwrap();
    assert_eq!(
        class(&node.indexed(&trust), &item, "view", &node.project()),
        TrustClass::Trusted
    );

    let revoked = revoke_endorsement(
        node.path(),
        &node.node,
        &trust,
        &recorded.digest,
        &fp(&node.operator),
        "withdrawn",
        NOW + 5,
    )
    .unwrap();
    assert!(revoked.newly_revoked);
    assert_eq!(revoked.tombstone.revoked_by, principal(&node.operator));
    assert_eq!(
        class(&node.indexed(&trust), &item, "view", &node.project()),
        TrustClass::Untrusted
    );

    // The first revocation is the one that counts.
    let again = revoke_endorsement(
        node.path(),
        &node.node,
        &trust,
        &recorded.digest,
        &fp(&node.operator),
        "again",
        NOW + 9,
    )
    .unwrap();
    assert!(!again.newly_revoked);
    assert_eq!(again.tombstone.reason, "withdrawn");

    // Removing the endorsement file and putting it back leaves it dead: the
    // tombstone, not the file, decides.
    let path = endorsements_dir(node.path())
        .join(project_directory_name(&node.project()))
        .join(format!("{}.json", recorded.digest));
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, bytes).unwrap();
    assert_eq!(
        class(&node.indexed(&trust), &item, "view", &node.project()),
        TrustClass::Untrusted
    );

    // A fresh endorsement of the same item is a new, explicit act.
    node.record(
        &trust,
        &node.operator,
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
    )
    .unwrap();
    assert_eq!(
        class(&node.indexed(&trust), &item, "view", &node.project()),
        TrustClass::Trusted
    );
}

#[test]
fn only_a_pinned_active_operator_may_endorse() {
    let node = Node::new();
    let trust = node.trust();
    let item = signed_item(&node.authoring, "x\n");
    let items = || vec![item_for("view", &item.2)];

    let by_authoring = node
        .record(
            &trust,
            &node.authoring,
            node.items_body(&node.authoring, PROJECT, items()),
        )
        .unwrap_err();
    assert_eq!(by_authoring.code(), "endorsement_endorser_not_operator");

    let stranger = key(9);
    let by_unpinned = node
        .record(
            &trust,
            &stranger,
            node.items_body(&stranger, PROJECT, items()),
        )
        .unwrap_err();
    assert_eq!(by_unpinned.code(), "endorsement_endorser_not_operator");

    let revoked_trust = node.trust_with(vec![
        record(&node.operator, SignerRole::Operator, SignerStatus::Revoked),
        record(&node.authoring, SignerRole::Authoring, SignerStatus::Active),
    ]);
    let by_revoked = node
        .record(
            &revoked_trust,
            &node.operator,
            node.items_body(&node.operator, PROJECT, items()),
        )
        .unwrap_err();
    assert_eq!(by_revoked.code(), "endorsement_endorser_not_operator");

    let retired_trust = node.trust_with(vec![
        record(&node.operator, SignerRole::Operator, SignerStatus::Retired),
        record(&node.authoring, SignerRole::Authoring, SignerStatus::Active),
    ]);
    let by_retired = node
        .record(
            &retired_trust,
            &node.operator,
            node.items_body(&node.operator, PROJECT, items()),
        )
        .unwrap_err();
    assert_eq!(by_retired.code(), "endorsement_endorser_not_operator");
}

#[test]
fn an_endorsement_whose_operator_is_revoked_later_stops_applying() {
    let node = Node::new();
    let trust = node.trust();
    let item = signed_item(&node.authoring, "x\n");
    node.record(
        &trust,
        &node.operator,
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
    )
    .unwrap();
    let revoked_trust = node.trust_with(vec![
        record(&node.operator, SignerRole::Operator, SignerStatus::Revoked),
        record(&node.authoring, SignerRole::Authoring, SignerStatus::Active),
    ]);
    assert_eq!(
        class(
            &node.indexed(&revoked_trust),
            &item,
            "view",
            &node.project()
        ),
        TrustClass::Untrusted
    );
}

#[test]
fn a_bad_signature_a_delegation_and_a_stale_time_are_refused() {
    let node = Node::new();
    let trust = node.trust();
    let item = signed_item(&node.authoring, "x\n");
    let body = node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]);

    // Signed by another key but naming the operator.
    let forged = SignedEndorsement::sign(body.clone(), &key(8)).unwrap();
    assert_eq!(
        record_endorsement(node.path(), &node.node, &trust, forged, NOW)
            .unwrap_err()
            .code(),
        "endorsement_bad_signature"
    );

    // Altered after signing.
    let mut altered = SignedEndorsement::sign(body.clone(), &node.operator).unwrap();
    altered.body.project = node.other_project();
    assert_eq!(
        record_endorsement(node.path(), &node.node, &trust, altered, NOW)
            .unwrap_err()
            .code(),
        "endorsement_bad_signature"
    );

    let mut delegated = body.clone();
    delegated.via = Some("b".repeat(64));
    let delegated = SignedEndorsement::sign(delegated, &node.operator).unwrap();
    assert_eq!(
        record_endorsement(node.path(), &node.node, &trust, delegated, NOW)
            .unwrap_err()
            .code(),
        "endorsement_delegation_unsupported"
    );

    let on_time = SignedEndorsement::sign(body, &node.operator).unwrap();
    assert_eq!(
        record_endorsement(
            node.path(),
            &node.node,
            &trust,
            on_time.clone(),
            NOW + MAX_ENDORSEMENT_CLOCK_SKEW_SECS + 1
        )
        .unwrap_err()
        .code(),
        "endorsement_time_out_of_window"
    );
    record_endorsement(node.path(), &node.node, &trust, on_time, NOW + 60).unwrap();
}

#[test]
fn two_endorsements_signed_against_the_same_ledger_head_apply_once() {
    let node = Node::new();
    let trust = node.trust();
    let a = signed_item(&node.authoring, "a\n");
    let b = signed_item(&node.authoring, "b\n");
    let first = SignedEndorsement::sign(
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &a.2)]),
        &node.operator,
    )
    .unwrap();
    let second = SignedEndorsement::sign(
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &b.2)]),
        &node.operator,
    )
    .unwrap();
    record_endorsement(node.path(), &node.node, &trust, first, NOW).unwrap();
    let refused = record_endorsement(node.path(), &node.node, &trust, second, NOW).unwrap_err();
    assert_eq!(refused.code(), "endorsement_ledger_moved");
}

#[test]
fn recording_the_same_endorsement_twice_is_idempotent() {
    let node = Node::new();
    let trust = node.trust();
    let item = signed_item(&node.authoring, "a\n");
    let signed = SignedEndorsement::sign(
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
        &node.operator,
    )
    .unwrap();
    let first = record_endorsement(node.path(), &node.node, &trust, signed.clone(), NOW).unwrap();
    let second = record_endorsement(node.path(), &node.node, &trust, signed, NOW + 3).unwrap();
    assert!(first.newly_recorded);
    assert!(!second.newly_recorded);
    assert_eq!(first.digest, second.digest);
    assert_eq!(first.ledger_head, second.ledger_head);
}

#[test]
fn a_non_authoring_key_is_unaffected_by_endorsements() {
    let node = Node::new();
    let project_author = key(5);
    let trust = node.trust_with(vec![
        record(&node.operator, SignerRole::Operator, SignerStatus::Active),
        record(&node.authoring, SignerRole::Authoring, SignerStatus::Active),
        {
            // A project author scoped to one kind in this project.
            let mut record = record(
                &project_author,
                SignerRole::ProjectAuthor,
                SignerStatus::Active,
            );
            record.scope.kinds = vec!["knowledge".to_owned()];
            record
        },
    ]);
    let authored = signed_item(&project_author, "x\n");
    assert_eq!(
        class(&trust, &authored, "knowledge", &node.project()),
        TrustClass::Trusted,
        "its registry scope still decides"
    );
    assert_eq!(
        class(&trust, &authored, "view", &node.project()),
        TrustClass::Untrusted,
        "outside its scope"
    );
    // An endorsement naming this very digest does not widen a non-authoring
    // key's scope.
    node.record(
        &trust,
        &node.operator,
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &authored.2)]),
    )
    .unwrap();
    let endorsed = node.indexed(&trust);
    assert_eq!(
        class(&endorsed, &authored, "view", &node.project()),
        TrustClass::Untrusted
    );
    assert_eq!(
        class(&endorsed, &authored, "knowledge", &node.project()),
        TrustClass::Trusted
    );
}

#[test]
fn an_endorsement_cannot_trust_an_unpinned_or_revoked_authoring_key() {
    let node = Node::new();
    let unpinned = key(6);
    let trust = node.trust();
    let item = signed_item(&unpinned, "x\n");
    node.record(
        &trust,
        &node.operator,
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
    )
    .unwrap();
    assert_eq!(
        class(&node.indexed(&trust), &item, "view", &node.project()),
        TrustClass::Untrusted,
        "an unpinned key"
    );

    let revoked = node.trust_with(vec![
        record(&node.operator, SignerRole::Operator, SignerStatus::Active),
        record(
            &node.authoring,
            SignerRole::Authoring,
            SignerStatus::Revoked,
        ),
    ]);
    let authored = signed_item(&node.authoring, "y\n");
    node.record(
        &trust,
        &node.operator,
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &authored.2)]),
    )
    .unwrap();
    let endorsed = node.indexed(&revoked);
    let error = verify_item_signature(
        &authored.0,
        &authored.1,
        &envelope(),
        &endorsed,
        &SignerContext::project("view", &LogicalProject::Stated(node.project())),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("trust_signer_revoked"),
        "{error}"
    );
}

#[test]
fn endorsement_never_makes_a_non_project_item_trusted() {
    let node = Node::new();
    let trust = node.trust();
    let item = signed_item(&node.authoring, "x\n");
    node.record(
        &trust,
        &node.operator,
        node.items_body(&node.operator, PROJECT, vec![item_for("tool", &item.2)]),
    )
    .unwrap();
    let endorsed = node.indexed(&trust);
    let error = verify_item_signature(
        &item.0,
        &item.1,
        &envelope(),
        &endorsed,
        &SignerContext::bundle("tool"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("signer_out_of_scope"), "{error}");
}

#[test]
fn the_store_fingerprint_moves_with_the_index() {
    let node = Node::new();
    let trust = node.trust();
    let before = trust.fingerprint();
    let item = signed_item(&node.authoring, "x\n");
    node.record(
        &trust,
        &node.operator,
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
    )
    .unwrap();
    let after = node.indexed(&trust).fingerprint();
    assert_ne!(before, after);
}

/// A node with a public identity and signed registry records, loaded the way
/// the daemon loads it: the store is live.
fn live_store(node: &Node) -> TrustStore {
    // A node with a public identity and signed registry records.
    let identity = node.path().join(".ai/node/identity");
    std::fs::create_dir_all(&identity).unwrap();
    std::fs::write(
        identity.join("public-identity.json"),
        serde_json::json!({
            "kind": "identity/v1",
            "principal_id": principal(&node.node),
            "signing_key": format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD
                    .encode(node.node.verifying_key().as_bytes())
            ),
        })
        .to_string(),
    )
    .unwrap();
    let signers = crate::authority::trust::signers_dir(node.path());
    std::fs::create_dir_all(&signers).unwrap();
    for record in [
        record(&node.operator, SignerRole::Operator, SignerStatus::Active),
        record(&node.authoring, SignerRole::Authoring, SignerStatus::Active),
    ] {
        std::fs::write(
            signers.join(record.file_name()),
            record.render_signed(&node.node).unwrap(),
        )
        .unwrap();
    }
    TrustStore::load_node_registry(node.path()).unwrap()
}

#[test]
fn a_live_store_follows_a_recording_and_a_snapshot_does_not() {
    let node = Node::new();
    let live = live_store(&node);
    let item = signed_item(&node.authoring, "x\n");
    assert_eq!(
        class(&live, &item, "view", &node.project()),
        TrustClass::Untrusted
    );
    let frozen = live.snapshot();

    node.record(
        &live,
        &node.operator,
        node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
    )
    .unwrap();
    let identity = live.refresh_endorsements(node.path()).unwrap();
    assert_eq!(
        class(&live, &item, "view", &node.project()),
        TrustClass::Trusted
    );
    assert_eq!(
        class(&frozen, &item, "view", &node.project()),
        TrustClass::Untrusted,
        "a request's snapshot sees one index for its whole life"
    );
    assert_eq!(
        live.endorsement_index()
            .identity(lillux::time::unix_secs_now()),
        identity
    );

    // A restart sees the same endorsements.
    let restarted = TrustStore::load_node_registry(node.path()).unwrap();
    assert_eq!(
        class(&restarted, &item, "view", &node.project()),
        TrustClass::Trusted
    );
}

#[test]
fn a_request_that_froze_its_trust_builds_one_signer_list_whatever_is_revoked_meanwhile() {
    let node = Node::new();
    let live = live_store(&node);
    let item = signed_item(&node.authoring, "x\n");
    let recorded = node
        .record(
            &live,
            &node.operator,
            node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
        )
        .unwrap();
    live.refresh_endorsements(node.path()).unwrap();

    // The request freezes its view at admission; its method-config load and
    // its limits load each build a signer list from that one view.
    let frozen = live.snapshot();
    let project = LogicalProject::Stated(node.project());
    let at_method_config_load = live.launch_signer_entries(&frozen, &project);

    revoke_endorsement(
        node.path(),
        &node.node,
        &live,
        &recorded.digest,
        &fp(&node.operator),
        "withdrawn",
        NOW + 5,
    )
    .unwrap();
    live.refresh_endorsements(node.path()).unwrap();

    let at_limits_load = live.launch_signer_entries(&frozen, &project);
    assert_eq!(
        trusted_signers_digest(&at_method_config_load),
        trusted_signers_digest(&at_limits_load),
        "one request, one trust view"
    );
    assert!(
        at_limits_load
            .iter()
            .any(|entry| entry.endorsements.is_some())
    );

    // The next request freezes the revoked state.
    let next = live.launch_signer_entries(&live.snapshot(), &project);
    assert_ne!(
        trusted_signers_digest(&at_limits_load),
        trusted_signers_digest(&next)
    );
    assert!(next.iter().all(|entry| entry.endorsements.is_none()));
}

#[test]
fn a_tampered_or_misfiled_document_refuses_the_load() {
    let node = Node::new();
    let trust = node.trust();
    let item = signed_item(&node.authoring, "x\n");
    let recorded = node
        .record(
            &trust,
            &node.operator,
            node.items_body(&node.operator, PROJECT, vec![item_for("view", &item.2)]),
        )
        .unwrap();
    let path = endorsements_dir(node.path())
        .join(project_directory_name(&node.project()))
        .join(format!("{}.json", recorded.digest));
    let original = std::fs::read_to_string(&path).unwrap();

    std::fs::write(
        &path,
        original.replace("view:acme/thing", "view:acme/other"),
    )
    .unwrap();
    let error = EndorsementLedger::load(node.path(), &node.node.verifying_key()).unwrap_err();
    assert_eq!(error.code(), "trust_record_tampered");

    std::fs::write(&path, &original).unwrap();
    let misfiled =
        endorsements_dir(node.path()).join(project_directory_name(&node.other_project()));
    std::fs::create_dir_all(&misfiled).unwrap();
    std::fs::rename(&path, misfiled.join(format!("{}.json", recorded.digest))).unwrap();
    let error = EndorsementLedger::load(node.path(), &node.node.verifying_key()).unwrap_err();
    assert_eq!(error.code(), "endorsement_record_invalid");
}

#[test]
fn shape_rules_refuse_wildcards_empty_targets_and_foreign_projects() {
    let node = Node::new();
    let operator = principal(&node.operator);
    let at = lillux::time::iso8601_from_unix_secs(NOW);
    let head = node.head();
    let project = node.project();
    let check = |body: EndorsementBody| body.validate_shape().unwrap_err().code();
    let standing = |kinds: Vec<String>| {
        EndorsementBody::standing(&project, &operator, kinds, None, &at, &nonce(), &head)
    };
    assert_eq!(
        check(standing(vec!["*".to_owned()])),
        "endorsement_malformed"
    );
    assert_eq!(check(standing(Vec::new())), "endorsement_malformed");
    assert_eq!(
        check(EndorsementBody::for_items(
            &project,
            &operator,
            Vec::new(),
            &at,
            &nonce(),
            &head
        )),
        "endorsement_malformed"
    );
    assert_eq!(
        check(EndorsementBody::standing(
            &identity(&key(9), PROJECT),
            &operator,
            vec!["view".to_owned()],
            None,
            &at,
            &nonce(),
            &head
        )),
        "endorsement_malformed"
    );
    assert_eq!(
        check(EndorsementBody::for_items(
            &project,
            &operator,
            vec![EndorsedItem {
                item_ref: "view:a".to_owned(),
                kind: "view".to_owned(),
                digest: "NOT-A-DIGEST".to_owned(),
            }],
            &at,
            &nonce(),
            &head
        )),
        "endorsement_malformed"
    );
    assert_eq!(
        check(EndorsementBody::standing(
            &project,
            &operator,
            vec!["view".to_owned()],
            Some(lillux::time::iso8601_from_unix_secs(NOW - 5)),
            &at,
            &nonce(),
            &head
        )),
        "endorsement_malformed",
        "not_after before endorsed_at"
    );
}
