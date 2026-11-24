//! Signed revision history of runtime-authored items, end to end.
//!
//! A planted tool calls `author_item` through the shipped callback client;
//! every create, upsert and remove must leave one signed record in the
//! project, signed by the node's authoring key, and the revision read service
//! must return the chain with what was checked. Tampering with a record, and
//! deleting the newest ones, must be detected.

use ryeosd::test_support as common;

use std::path::{Path, PathBuf};

use common::DaemonHarness;
use common::callback_probe::{ProbeAuthority, ProbeOutcome, plant_probe_tool, run_callback};
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use serde_json::{Value, json};

const PROBE_BINARY: &str = env!("CARGO_BIN_EXE_ryeos-callback-probe");
const ITEM: &str = "graph:authored/ledger";

fn graph_body(version: &str) -> String {
    format!(
        "category: authored\nversion: \"{version}\"\nconfig:\n  start: done\n  nodes:\n    done:\n      node_type: return\n"
    )
}

struct Rig {
    harness: DaemonHarness,
    fixture: FastFixture,
    _guard: tempfile::TempDir,
    project: PathBuf,
}

async fn rig() -> Rig {
    let (harness, fixture) = DaemonHarness::start_fast_with(
        |state, _, fixture| register_workflow_bundles(state, fixture),
        |_| {},
    )
    .await
    .expect("start daemon with the workflow bundles");
    let guard = tempfile::tempdir().expect("project tempdir");
    let project = guard.path().canonicalize().expect("canonical project path");
    plant_probe_tool(
        &project,
        &fixture.publisher,
        Path::new(PROBE_BINARY),
        &ProbeAuthority::new().item_authoring("graph", "authored/*"),
    )
    .expect("plant callback probe tool");
    Rig {
        harness,
        fixture,
        _guard: guard,
        project,
    }
}

impl Rig {
    async fn author(&self, mode: &str, version: &str, expected: Option<&str>) -> ProbeOutcome {
        let mut params = json!({
            "item_ref": ITEM,
            "content": graph_body(version),
            "mode": mode,
            "format_ext": ".yaml",
        });
        if let Some(expected) = expected {
            params["expected_digest"] = json!(expected);
        }
        run_callback(&self.harness, &self.project, "author_item", params)
            .await
            .expect("author_item callback")
    }

    async fn remove(&self, expected: Option<&str>) -> ProbeOutcome {
        let mut params = json!({"item_ref": ITEM, "mode": "remove"});
        if let Some(expected) = expected {
            params["expected_digest"] = json!(expected);
        }
        run_callback(&self.harness, &self.project, "author_item", params)
            .await
            .expect("author_item callback")
    }

    async fn service(&self, service: &str, params: Value) -> Value {
        let (status, body) = self
            .harness
            .post_execute(service, ".", params)
            .await
            .expect("service call");
        assert!(status.is_success(), "{service}: {status} {body:#}");
        body.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{service}: no result in {body:#}"))
    }

    async fn history(&self) -> Value {
        self.service(
            "service:revisions/list",
            json!({"project_path": self.project.to_str().unwrap(), "item_ref": ITEM}),
        )
        .await
    }

    fn item_path(&self) -> PathBuf {
        self.project.join(".ai/graphs/authored/ledger.yaml")
    }

    fn record_path(&self, seq: u64) -> PathBuf {
        self.project
            .join(format!(".ai/revisions/graph/authored/ledger/{seq:08}.yaml"))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn author_upsert_and_remove_record_a_signed_chain() {
    let rig = rig().await;
    let created = rig.author("create", "1.0.0", None).await.accepted();
    assert_eq!(created["revision"], 1, "{created:#}");
    assert_eq!(created["previous_digest"], Value::Null, "{created:#}");
    let first_bytes = std::fs::read(rig.item_path()).unwrap();

    let upserted = rig
        .author(
            "upsert",
            "2.0.0",
            Some(created["content_digest"].as_str().unwrap()),
        )
        .await
        .accepted();
    assert_eq!(upserted["revision"], 2, "{upserted:#}");
    assert_eq!(
        upserted["previous_digest"], created["revision_digest"],
        "{upserted:#}"
    );
    let second_bytes = std::fs::read(rig.item_path()).unwrap();

    let removed = rig
        .remove(Some(upserted["content_digest"].as_str().unwrap()))
        .await
        .accepted();
    assert_eq!(removed["mode"], "remove", "{removed:#}");
    assert_eq!(removed["revision"], 3, "{removed:#}");
    assert_eq!(
        removed["previous_digest"], upserted["revision_digest"],
        "{removed:#}"
    );
    assert!(!rig.item_path().exists(), "the removed item must be gone");

    let history = rig.history().await;
    assert_eq!(history["total"], 3, "{history:#}");
    assert_eq!(history["sound"], true, "{history:#}");
    assert_eq!(history["head"]["status"], "attested", "{history:#}");
    assert_eq!(history["head"]["seq"], 3, "{history:#}");
    assert_eq!(history["file"], "removed", "{history:#}");
    let records = history["records"].as_array().expect("records");
    let authoring_signer = format!("fp:{}", rig.fixture.authoring_fp());
    for record in records {
        assert_eq!(record["verify"]["status"], "verified", "{record:#}");
        assert_eq!(
            record["verify"]["signer"],
            json!(authoring_signer),
            "{record:#}"
        );
        assert_eq!(record["actor"]["role"], "authoring", "{record:#}");
    }
    assert_eq!(records[0]["hash"], created["revision_digest"]);
    assert_eq!(records[1]["hash"], upserted["revision_digest"]);
    assert_eq!(records[2]["hash"], removed["revision_digest"]);
    assert_eq!(records[0]["before_digest"], Value::Null);
    assert_eq!(
        records[0]["after_digest"],
        json!(lillux::sha256_hex(&first_bytes))
    );
    assert_eq!(records[1]["before_digest"], records[0]["after_digest"]);
    assert_eq!(
        records[1]["after_digest"],
        json!(lillux::sha256_hex(&second_bytes))
    );
    assert_eq!(records[2]["tombstone"], true, "{:#}", records[2]);
    assert_eq!(records[2]["after_digest"], Value::Null);
    assert_eq!(records[2]["before_digest"], records[1]["after_digest"]);
    // The provenance of the authoring run is inside the signed record.
    assert!(
        records[0]["provenance"]["thread_id"].is_string(),
        "{:#}",
        records[0]
    );
    assert_eq!(
        records[0]["provenance"]["authoring_capability"],
        "ryeos.author.graph.authored/ledger"
    );

    // One record carries the signed item bytes inline, and verifies.
    let second = rig
        .service(
            "service:revisions/get",
            json!({"project_path": rig.project.to_str().unwrap(), "item_ref": ITEM, "seq": 2}),
        )
        .await;
    assert_eq!(
        second["record"]["verify"]["status"], "verified",
        "{second:#}"
    );
    assert_eq!(
        second["record"]["body"]["content"],
        json!(String::from_utf8(second_bytes).unwrap()),
        "{second:#}"
    );
    assert!(rig.record_path(3).is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn removal_is_guarded_by_the_incumbents_digest() {
    let rig = rig().await;
    let created = rig.author("create", "1.0.0", None).await.accepted();

    // No digest: refused, naming the digest to resend.
    let unguarded = rig.remove(None).await;
    assert_eq!(
        unguarded.refusal_code(),
        "author_item_expected_digest_required"
    );
    let ProbeOutcome::Refused { message, .. } = &unguarded else {
        unreachable!()
    };
    assert!(
        message.contains(created["content_digest"].as_str().unwrap()),
        "{message}"
    );

    // A stale digest: a conflict, and nothing is removed or recorded.
    let stale = rig.remove(Some("not-the-digest")).await;
    let ProbeOutcome::Refused { message, .. } = &stale else {
        panic!("a stale digest must be refused: {stale:?}")
    };
    assert!(message.contains("conflict"), "{message}");
    assert!(rig.item_path().exists());
    assert_eq!(rig.history().await["total"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_operator_authored_item_is_not_removable_through_author_item() {
    let rig = rig().await;
    let handmade = rig.project.join(".ai/graphs/authored/ledger.yaml");
    std::fs::create_dir_all(handmade.parent().unwrap()).unwrap();
    std::fs::write(
        &handmade,
        lillux::signature::sign_content(&graph_body("9.9.9"), &rig.fixture.publisher, "#", None),
    )
    .unwrap();

    let outcome = rig.remove(Some("anything")).await;
    assert_eq!(
        outcome.refusal_code(),
        "author_item_remove_not_runtime_authored"
    );
    assert!(handmade.exists(), "the operator's item must stay");
    assert!(!rig.project.join(".ai/revisions").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_cannot_author_a_revision_record() {
    let rig = rig().await;
    rig.author("create", "1.0.0", None).await.accepted();
    let outcome = run_callback(
        &rig.harness,
        &rig.project,
        "author_item",
        json!({
            "item_ref": "revision:graph/authored/ledger/00000009",
            "content": "schema: forged\n",
            "mode": "create",
            "format_ext": ".yaml",
        }),
    )
    .await
    .expect("author_item callback");
    assert_eq!(outcome.refusal_code(), "author_item_kind_not_authorable");
    assert!(!rig.record_path(9).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tampered_record_is_named_by_the_read() {
    let rig = rig().await;
    let created = rig.author("create", "1.0.0", None).await.accepted();
    rig.author(
        "upsert",
        "2.0.0",
        Some(created["content_digest"].as_str().unwrap()),
    )
    .await
    .accepted();
    let path = rig.record_path(1);
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, original.replace("authored", "tampered")).unwrap();

    let history = rig.history().await;
    assert_eq!(history["sound"], false, "{history:#}");
    let records = history["records"].as_array().unwrap();
    assert_eq!(records[0]["verify"]["status"], "failed", "{history:#}");
    assert_eq!(
        records[0]["verify"]["reason"], "signature_invalid",
        "{history:#}"
    );
    // The record after it no longer continues what it names.
    assert_eq!(
        records[1]["verify"]["reason"], "previous_mismatch",
        "{history:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_newest_records_is_detected_and_blocks_extension() {
    let rig = rig().await;
    let created = rig.author("create", "1.0.0", None).await.accepted();
    let upserted = rig
        .author(
            "upsert",
            "2.0.0",
            Some(created["content_digest"].as_str().unwrap()),
        )
        .await
        .accepted();
    std::fs::remove_file(rig.record_path(2)).unwrap();

    let history = rig.history().await;
    assert_eq!(history["sound"], false, "{history:#}");
    assert_eq!(history["head"]["status"], "truncated", "{history:#}");
    assert_eq!(history["head"]["head_seq"], 2, "{history:#}");
    assert_eq!(history["head"]["tip_seq"], 1, "{history:#}");

    let refused = rig
        .author(
            "upsert",
            "3.0.0",
            Some(upserted["content_digest"].as_str().unwrap()),
        )
        .await;
    assert_eq!(refused.refusal_code(), "revision_chain_truncated");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hand_edit_reads_as_an_unrecorded_change() {
    let rig = rig().await;
    rig.author("create", "1.0.0", None).await.accepted();
    std::fs::write(rig.item_path(), "edited: by hand\n").unwrap();
    let history = rig.history().await;
    assert_eq!(history["file"], "unrecorded_change", "{history:#}");
    assert_eq!(history["sound"], true, "{history:#}");
}

// ── announcing revisions ────────────────────────────────────────────────

/// The newest revision of each item the node has announced for the project.
fn announced(rig: &Rig) -> std::collections::BTreeMap<String, u64> {
    let cursor = ryeos_engine::roots::RuntimeRoot::new(rig.harness.state_path.clone())
        .item_revised_cursor_dir();
    let project = rig.project.to_str().unwrap();
    let path = cursor.join(format!("{}.json", lillux::sha256_hex(project.as_bytes())));
    let Ok(bytes) = std::fs::read(&path) else {
        return Default::default();
    };
    let document: Value = serde_json::from_slice(&bytes).expect("cursor document");
    assert_eq!(document["project"], json!(project), "{document:#}");
    serde_json::from_value(document["items"].clone()).expect("cursor items")
}

/// What `ryeos sign` leaves behind when the operator signs the item in their
/// own process: a new signed file and a record signed with their key.
fn operator_signs(rig: &Rig, version: &str) -> u64 {
    use ryeos_node_runtime::item_revision::{ActorRole, ChainItem, Change, append};
    let before = std::fs::read(rig.item_path()).ok();
    let signed =
        lillux::signature::sign_content(&graph_body(version), &rig.fixture.user, "#", None);
    std::fs::write(rig.item_path(), &signed).unwrap();
    append(
        &rig.project,
        &rig.fixture.user,
        ActorRole::Operator,
        Change {
            item: &ChainItem::parse(ITEM).unwrap(),
            file_ext: ".yaml",
            before: before.as_deref(),
            after: Some(&signed),
            provenance: std::collections::BTreeMap::from([(
                "action".to_owned(),
                "sign".to_owned(),
            )]),
        },
    )
    .expect("operator record")
    .seq
}

#[tokio::test(flavor = "multi_thread")]
async fn author_item_announces_each_change_once_it_is_durable() {
    let rig = rig().await;
    assert!(announced(&rig).is_empty());
    let created = rig.author("create", "1.0.0", None).await.accepted();
    assert_eq!(announced(&rig), [(ITEM.to_owned(), 1)].into());
    let upserted = rig
        .author(
            "upsert",
            "2.0.0",
            Some(created["content_digest"].as_str().unwrap()),
        )
        .await
        .accepted();
    assert_eq!(announced(&rig), [(ITEM.to_owned(), 2)].into());
    rig.remove(Some(upserted["content_digest"].as_str().unwrap()))
        .await
        .accepted();
    assert_eq!(announced(&rig), [(ITEM.to_owned(), 3)].into());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_node_announces_an_operator_change_only_after_verifying_its_record() {
    let rig = rig().await;
    rig.author("create", "1.0.0", None).await.accepted();
    assert_eq!(announced(&rig), [(ITEM.to_owned(), 1)].into());

    let seq = operator_signs(&rig, "2.0.0");
    assert_eq!(seq, 2);
    // Recorded in the project, not yet announced: the CLI has not called.
    assert_eq!(announced(&rig), [(ITEM.to_owned(), 1)].into());

    let project = rig.project.to_str().unwrap();
    let result = rig
        .service(
            "service:items/revised",
            json!({"project_path": project, "item_refs": [ITEM]}),
        )
        .await;
    let announcements = result["announced"].as_array().expect("announced");
    assert_eq!(announcements.len(), 1, "{result:#}");
    assert_eq!(announcements[0]["item_ref"], ITEM, "{result:#}");
    assert_eq!(announcements[0]["revision"], 2, "{result:#}");
    assert_eq!(
        announcements[0]["digest"],
        json!(lillux::sha256_hex(&std::fs::read(rig.item_path()).unwrap())),
        "{result:#}"
    );
    assert_eq!(announced(&rig), [(ITEM.to_owned(), 2)].into());

    // A record the node cannot verify announces nothing: replace the newest
    // record with one signed by a key the registry does not pin.
    let seq = operator_signs(&rig, "3.0.0");
    let path = rig.record_path(seq);
    let record = std::fs::read_to_string(&path).unwrap();
    let (_, body) = record.split_once('\n').unwrap();
    let stranger = lillux::crypto::SigningKey::from_bytes(&[77; 32]);
    std::fs::write(
        &path,
        lillux::signature::sign_content(body, &stranger, "#", None),
    )
    .unwrap();
    let (status, body) = rig
        .harness
        .post_execute(
            "service:items/revised",
            ".",
            json!({"project_path": project, "item_refs": [ITEM]}),
        )
        .await
        .unwrap();
    assert_eq!(status.as_u16(), 403, "{body:#}");
    assert_eq!(announced(&rig), [(ITEM.to_owned(), 2)].into());

    // An item with no records is refused, and so is a request that names
    // neither or both of its two forms.
    let (status, _) = rig
        .harness
        .post_execute(
            "service:items/revised",
            ".",
            json!({"project_path": project, "item_refs": ["graph:authored/never"]}),
        )
        .await
        .unwrap();
    assert_eq!(status.as_u16(), 404);
    let (status, _) = rig
        .harness
        .post_execute(
            "service:items/revised",
            ".",
            json!({"project_path": project}),
        )
        .await
        .unwrap();
    assert_eq!(status.as_u16(), 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_request_announces_nothing() {
    let rig = rig().await;
    rig.author("create", "1.0.0", None).await.accepted();
    operator_signs(&rig, "2.0.0");
    let project = rig.project.to_str().unwrap();
    let body = serde_json::to_vec(&json!({
        "item_ref": "service:items/revised",
        "ref_bindings": {},
        "parameters": {"project_path": project, "item_refs": [ITEM]},
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    }))
    .unwrap();
    let node_key = rig.harness.node_key.clone().expect("node key");
    let post = |headers: Vec<(String, String)>, body: Vec<u8>| {
        let url = format!("http://{}/execute", rig.harness.bind);
        async move {
            let mut request = reqwest::Client::new()
                .post(url)
                .header("content-type", "application/json")
                .body(body);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            request.send().await.unwrap().status().as_u16()
        }
    };

    // Signed by a key the node does not know.
    let stranger = lillux::crypto::SigningKey::from_bytes(&[88; 32]);
    let headers =
        common::build_signed_headers_for_bytes(&stranger, &node_key, "POST", "/execute", &body);
    let status = post(headers, body.clone()).await;
    assert!(status == 401 || status == 403, "stranger: {status}");

    // Signed by the operator, then altered in flight.
    let operator = rig.harness.user_key.clone().expect("operator key");
    let headers =
        common::build_signed_headers_for_bytes(&operator, &node_key, "POST", "/execute", &body);
    let mut altered = body.clone();
    let at = altered
        .windows(5)
        .position(|window| window == b"graph")
        .expect("body mentions the item");
    altered[at] = b'G';
    let status = post(headers, altered).await;
    assert!(status == 401 || status == 403, "altered: {status}");

    // Not signed at all.
    let status = post(Vec::new(), body.clone()).await;
    assert!(status == 401 || status == 403, "unsigned: {status}");

    assert_eq!(announced(&rig), [(ITEM.to_owned(), 1)].into());

    // The same request, genuinely signed, announces.
    let headers =
        common::build_signed_headers_for_bytes(&operator, &node_key, "POST", "/execute", &body);
    assert_eq!(post(headers, body).await, 200);
    assert_eq!(announced(&rig), [(ITEM.to_owned(), 2)].into());
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_announces_the_revisions_the_node_has_not_announced() {
    let mut rig = rig().await;
    rig.author("create", "1.0.0", None).await.accepted();
    assert_eq!(announced(&rig), [(ITEM.to_owned(), 1)].into());
    rig.harness.stop_gracefully().await.expect("orderly stop");

    // The operator signs the item while the node is stopped.
    operator_signs(&rig, "2.0.0");
    assert_eq!(announced(&rig), [(ITEM.to_owned(), 1)].into());

    rig.harness
        .respawn_with(|_| {})
        .await
        .expect("the node starts again");
    let mut seen = announced(&rig);
    for _ in 0..50 {
        if seen.get(ITEM) == Some(&2) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        seen = announced(&rig);
    }
    assert_eq!(seen, [(ITEM.to_owned(), 2)].into());
}
