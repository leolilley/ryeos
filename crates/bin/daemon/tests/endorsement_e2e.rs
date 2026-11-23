//! End to end: an operator endorsement trusts an item the authoring key signed.
//!
//! A real daemon runs with an authoring-signed project item. The operator
//! signs an endorsement with their own key and submits it to the running node
//! (the steps `ryeos endorse` takes after it has read the item's digest): the
//! item's trust class flips from untrusted to trusted without a restart, a
//! changed byte flips it back, revoking the endorsement flips it back, and
//! every acceptance and revocation is a node-journal record. The daemon never
//! holds the operator key.

use std::path::Path;

use ryeosd::test_support as common;

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use lillux::crypto::SigningKey;
use ryeos_engine::contracts::SignatureEnvelope;
use ryeos_engine::trust::{
    EndorsedItem, EndorsementBody, SignedEndorsement, TrustStore, compute_fingerprint,
};
use ryeos_node::node_journal::{JournalType, NodeJournal};
use ryeos_node::offline_operation::BareHost;
use ryeos_node::offline_operation::apply::{LocalDraft, apply_local};
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_state::project_identity::{ProjectIdentity, ProjectName};
use serde_json::{Value, json};

const KNOWLEDGE_REF: &str = "knowledge:acme/note";
const PROJECT_NAME: &str = "endorsed-project";

/// The identity of the test project as `operator` owns it.
fn project_identity(operator: &SigningKey) -> ProjectIdentity {
    let owner = ryeos_engine::trust::SuccessionGraph::default()
        .ownership_of(&compute_fingerprint(&operator.verifying_key()))
        .expect("the operator key owns a principal")
        .principal;
    ProjectIdentity::new(owner, ProjectName::new(PROJECT_NAME).expect("project name"))
}

/// Make `project` the operator's registered live checkout: it declares
/// [`PROJECT_NAME`] and the operator registers it while the node is stopped,
/// so a launch from it states `pid:v1:<operator principal>/endorsed-project`.
async fn register_project(h: &mut DaemonHarness, operator: &SigningKey, project: &Path) {
    let ai = project.join(".ai");
    std::fs::create_dir_all(&ai).expect("project .ai directory");
    std::fs::write(ai.join("manifest.yaml"), format!("name: {PROJECT_NAME}\n"))
        .expect("project declaration");
    h.kill_daemon().await.expect("stop the daemon");
    apply_local(
        &h.state_path,
        &BareHost,
        operator,
        LocalDraft {
            operation: "node.project.register".to_owned(),
            arguments: json!({ "path": project.canonicalize().unwrap().to_str().unwrap() }),
            inputs: std::collections::BTreeMap::new(),
            expected_result: std::collections::BTreeMap::new(),
        },
    )
    .expect("register the project");
    h.respawn_with(|_| {}).await.expect("restart the daemon");
}

/// Give `project` a manifest declaring `name`.
fn declare_name(project: &Path, name: &str) {
    let ai = project.join(".ai");
    std::fs::create_dir_all(&ai).expect("project .ai directory");
    std::fs::write(ai.join("manifest.yaml"), format!("name: {name}\n")).expect("project manifest");
}

/// The authoring record `ryeos sign` leaves for the planted note: revision 1
/// of its chain, signed by `signer`.
fn record_authored_note(project: &Path, signer: &SigningKey) {
    use ryeos_node_runtime::item_revision::{ActorRole, ChainItem, Change, append};
    let signed = std::fs::read_to_string(project.join(".ai/knowledge/acme/note.md")).unwrap();
    let recorded = append(
        project,
        signer,
        ActorRole::Authoring,
        Change {
            item: &ChainItem::parse(KNOWLEDGE_REF).unwrap(),
            file_ext: ".md",
            before: None,
            after: Some(&signed),
            provenance: std::collections::BTreeMap::from([(
                "action".to_owned(),
                "sign".to_owned(),
            )]),
        },
    )
    .expect("authoring record");
    assert_eq!(recorded.seq, 1);
}

/// The newest revision of each item the node has announced for `project`.
fn announced(h: &DaemonHarness, project: &str) -> std::collections::BTreeMap<String, u64> {
    let cursor =
        ryeos_engine::roots::RuntimeRoot::new(h.state_path.clone()).item_revised_cursor_dir();
    let path = cursor.join(format!("{}.json", lillux::sha256_hex(project.as_bytes())));
    let Ok(bytes) = std::fs::read(&path) else {
        return Default::default();
    };
    let document: Value = serde_json::from_slice(&bytes).expect("cursor document");
    assert_eq!(document["project"], json!(project), "{document:#}");
    serde_json::from_value(document["items"].clone()).expect("cursor items")
}

fn canonical(path: &Path) -> String {
    path.canonicalize().unwrap().to_str().unwrap().to_owned()
}

fn envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "<!--".to_owned(),
        suffix: Some("-->".to_owned()),
        after_shebang: false,
    }
}

/// Write `.ai/knowledge/acme/note.md` signed by `signer`; returns the digest of
/// its signed bytes after the signature line.
fn plant_note(project: &Path, text: &str, signer: &SigningKey) -> String {
    let path = project.join(".ai/knowledge/acme/note.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let content = format!("```yaml\nname: note\ncategory: \"acme\"\n```\n\n{text}\n");
    let signed = lillux::signature::sign_content(&content, signer, "<!--", Some("-->"));
    std::fs::write(&path, &signed).unwrap();
    ryeos_engine::item_resolution::parse_signature_header(&signed, &envelope())
        .expect("signature header")
        .content_hash
}

async fn service(h: &DaemonHarness, service: &str, params: Value) -> (u16, Value) {
    let (status, body) = h
        .post_execute(service, ".", params)
        .await
        .expect("post /execute");
    (status.as_u16(), body)
}

async fn trust_class(h: &DaemonHarness, project: &Path) -> String {
    let (status, body) = service(
        h,
        "service:items/effective",
        json!({
            "canonical_ref": KNOWLEDGE_REF,
            "project_path": project.to_str().unwrap(),
        }),
    )
    .await;
    assert_eq!(status, 200, "{body:#}");
    body.pointer("/result/trust_class")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no trust class: {body:#}"))
        .to_owned()
}

/// What `ryeos endorse` does once it has the digests: read the ledger head
/// from the node, sign the body with the operator key here, submit it.
async fn endorse_items(h: &DaemonHarness, operator: &SigningKey, digest: &str) -> (u16, Value) {
    let (status, facts) = service(h, "service:endorsements/record", json!({})).await;
    assert_eq!(status, 200, "{facts:#}");
    let head = facts["result"]["ledger_head"].as_str().expect("head");
    let nonce: [u8; 16] = rand::random();
    let body = EndorsementBody::for_items(
        &project_identity(operator),
        &format!("fp:{}", compute_fingerprint(&operator.verifying_key())),
        vec![EndorsedItem {
            item_ref: KNOWLEDGE_REF.to_owned(),
            kind: "knowledge".to_owned(),
            digest: digest.to_owned(),
        }],
        &lillux::time::iso8601_now(),
        &nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        head,
    );
    let signed = SignedEndorsement::sign(body, operator).expect("sign");
    service(
        h,
        "service:endorsements/record",
        json!({ "endorsement": signed }),
    )
    .await
}

fn journal_types(app_root: &Path) -> Vec<JournalType> {
    let node = NodeIdentity::load(&app_root.join(".ai/node/identity/private_key.pem"))
        .expect("node identity");
    NodeJournal::for_app_root(app_root, node)
        .read()
        .expect("the journal verifies against the node key")
        .records
        .iter()
        .map(|record| record.body.record_type)
        .collect()
}

async fn start() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with(
        |state_path, _, fixture| register_workflow_bundles(state_path, fixture),
        |_| {},
    )
    .await
    .expect("start daemon with the workflow bundles")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_endorsement_trusts_an_authored_item_until_it_changes_or_is_revoked() {
    let (mut h, fixture) = start().await;
    let project = tempfile::tempdir().expect("project");
    let digest = plant_note(project.path(), "one", &fixture.authoring);
    register_project(&mut h, &fixture.user, project.path()).await;
    assert_eq!(trust_class(&h, project.path()).await, "untrusted_project");

    let (status, recorded) = endorse_items(&h, &fixture.user, &digest).await;
    assert_eq!(status, 200, "{recorded:#}");
    let endorsement = recorded["result"]["endorsement_digest"]
        .as_str()
        .expect("digest")
        .to_owned();
    assert_eq!(recorded["result"]["newly_recorded"], true, "{recorded:#}");
    assert_eq!(
        recorded["result"]["journal"]["recorded"], true,
        "{recorded:#}"
    );
    assert_eq!(
        trust_class(&h, project.path()).await,
        "trusted_project",
        "the running node follows the endorsement without a restart"
    );

    // One changed byte: the digest no longer matches.
    plant_note(project.path(), "two", &fixture.authoring);
    assert_eq!(trust_class(&h, project.path()).await, "untrusted_project");
    plant_note(project.path(), "one", &fixture.authoring);
    assert_eq!(
        trust_class(&h, project.path()).await,
        "trusted_project",
        "the same signed bytes are covered again"
    );

    // The endorsement survives a restart.
    h.stop_gracefully().await.expect("orderly stop");
    h.respawn_with(|_| {}).await.expect("the node starts again");
    assert_eq!(trust_class(&h, project.path()).await, "trusted_project");

    // The list shows it in force, then revoked.
    let (status, listed) = service(&h, "service:endorsements/list", json!({})).await;
    assert_eq!(status, 200, "{listed:#}");
    let entry = &listed["result"]["endorsements"][0];
    assert_eq!(entry["endorsement_digest"], endorsement, "{listed:#}");
    assert_eq!(entry["in_force"], true, "{listed:#}");

    let (status, revoked) = service(
        &h,
        "service:endorsements/revoke",
        json!({ "endorsement": endorsement, "reason": "withdrawn in test" }),
    )
    .await;
    assert_eq!(status, 200, "{revoked:#}");
    assert_eq!(revoked["result"]["newly_revoked"], true, "{revoked:#}");
    assert_eq!(
        revoked["result"]["revoked_by"],
        format!("fp:{}", fixture.user_fp()),
        "{revoked:#}"
    );
    assert_eq!(trust_class(&h, project.path()).await, "untrusted_project");
    let (_, listed) = service(&h, "service:endorsements/list", json!({})).await;
    assert_eq!(listed["result"]["endorsements"][0]["in_force"], false);
    assert!(listed["result"]["endorsements"][0]["revoked"].is_object());

    // The offline view agrees: the same registry load a restart performs.
    let offline = TrustStore::load_node_registry(&h.state_path).expect("registry loads");
    assert!(offline.endorsement_index().is_empty());

    let types = journal_types(&h.state_path);
    let endorsements: Vec<_> = types
        .iter()
        .filter(|record| {
            matches!(
                record,
                JournalType::EndorsementAccepted | JournalType::EndorsementRevoked
            )
        })
        .copied()
        .collect();
    assert_eq!(
        endorsements,
        vec![
            JournalType::EndorsementAccepted,
            JournalType::EndorsementRevoked
        ],
        "{types:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_an_operator_signature_endorses_and_the_ledger_head_orders_writers() {
    let (mut h, fixture) = start().await;
    let project = tempfile::tempdir().expect("project");
    let digest = plant_note(project.path(), "one", &fixture.authoring);
    register_project(&mut h, &fixture.user, project.path()).await;

    // The authoring key cannot endorse its own output.
    let (status, refused) = endorse_items(&h, &fixture.authoring, &digest).await;
    assert_eq!(status, 403, "{refused:#}");
    assert!(
        refused
            .to_string()
            .contains("endorsement_endorser_not_operator"),
        "{refused:#}"
    );
    assert_eq!(trust_class(&h, project.path()).await, "untrusted_project");

    // Two endorsements signed against the same head: one applies.
    let (_, facts) = service(&h, "service:endorsements/record", json!({})).await;
    let head = facts["result"]["ledger_head"]
        .as_str()
        .expect("head")
        .to_owned();
    let sign = |item: &str| {
        let nonce: [u8; 16] = rand::random();
        SignedEndorsement::sign(
            EndorsementBody::standing(
                &project_identity(&fixture.user),
                &format!("fp:{}", fixture.user_fp()),
                vec![item.to_owned()],
                None,
                &lillux::time::iso8601_now(),
                &nonce
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
                &head,
            ),
            &fixture.user,
        )
        .expect("sign")
    };
    let (status, first) = service(
        &h,
        "service:endorsements/record",
        json!({ "endorsement": sign("knowledge") }),
    )
    .await;
    assert_eq!(status, 200, "{first:#}");
    let (status, second) = service(
        &h,
        "service:endorsements/record",
        json!({ "endorsement": sign("directive") }),
    )
    .await;
    assert_eq!(status, 409, "{second:#}");
    assert!(
        second.to_string().contains("endorsement_ledger_moved"),
        "{second:#}"
    );

    // The standing endorsement covers the knowledge kind in this project.
    assert_eq!(trust_class(&h, project.path()).await, "trusted_project");
}

/// What `ryeos endorse` does after the endorsement is recorded: tell the node,
/// which finds the endorsement in its ledger and announces the items it covers
/// so a seat that shows one can pick up its new trust.
#[tokio::test(flavor = "multi_thread")]
async fn recording_an_endorsement_announces_the_items_it_covers() {
    let (h, fixture) = start().await;
    let project = tempfile::tempdir().expect("project");
    declare_name(project.path(), PROJECT_NAME);
    let digest = plant_note(project.path(), "one", &fixture.authoring);
    record_authored_note(project.path(), &fixture.authoring);
    let project_path = canonical(project.path());
    assert!(announced(&h, &project_path).is_empty());

    let (status, recorded) = endorse_items(&h, &fixture.user, &digest).await;
    assert_eq!(status, 200, "{recorded:#}");
    let endorsement = recorded["result"]["endorsement_digest"]
        .as_str()
        .expect("digest")
        .to_owned();

    let (status, announced) = service(
        &h,
        "service:items/revised",
        json!({ "project_path": project_path, "endorsement": endorsement }),
    )
    .await;
    assert_eq!(status, 200, "{announced:#}");
    let items = announced["result"]["announced"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{announced:#}");
    assert_eq!(items[0]["item_ref"], KNOWLEDGE_REF, "{announced:#}");
    assert_eq!(items[0]["digest"], json!(digest), "{announced:#}");
    assert_eq!(items[0]["revision"], json!(1), "{announced:#}");
    // The node published it: its announcement cursor, written beside the hub
    // publish that feeds the seat fanout, now holds the item's revision.
    assert_eq!(
        announced(&h, &project_path),
        [(KNOWLEDGE_REF.to_owned(), 1)].into()
    );

    // The endorsement is found under the identity the checkout states, never
    // by the path the caller names: a checkout that declares no name, or
    // declares another name, announces nothing for it.
    let unnamed = tempfile::tempdir().expect("unnamed project");
    std::fs::create_dir_all(unnamed.path().join(".ai")).expect("unnamed .ai");
    let (status, refused) = service(
        &h,
        "service:items/revised",
        json!({ "project_path": canonical(unnamed.path()), "endorsement": endorsement }),
    )
    .await;
    assert_eq!(status, 409, "{refused:#}");
    assert!(
        refused.to_string().contains("project_manifest_missing"),
        "{refused:#}"
    );
    let other = tempfile::tempdir().expect("other project");
    declare_name(other.path(), "other-project");
    let (status, refused) = service(
        &h,
        "service:items/revised",
        json!({ "project_path": canonical(other.path()), "endorsement": endorsement }),
    )
    .await;
    assert_eq!(status, 404, "{refused:#}");
    assert!(
        refused.to_string().contains("endorsement_not_in_force"),
        "{refused:#}"
    );

    // A digest the ledger does not hold announces nothing.
    let (status, unknown) = service(
        &h,
        "service:items/revised",
        json!({ "project_path": project_path, "endorsement": "0".repeat(64) }),
    )
    .await;
    assert_eq!(status, 404, "{unknown:#}");

    // A withdrawn endorsement announces nothing either.
    let (status, revoked) = service(
        &h,
        "service:endorsements/revoke",
        json!({ "endorsement": endorsement, "reason": "withdrawn in test" }),
    )
    .await;
    assert_eq!(status, 200, "{revoked:#}");
    let (status, withdrawn) = service(
        &h,
        "service:items/revised",
        json!({ "project_path": project_path, "endorsement": endorsement }),
    )
    .await;
    assert_eq!(status, 409, "{withdrawn:#}");
}
