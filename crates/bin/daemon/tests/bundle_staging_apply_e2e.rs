//! End to end: a staged bundle is activated by a signed request at the next
//! start, refused with a closed code when anything about it is wrong, and
//! cleared by the operator while the node runs.
//!
//! Node A publishes a bundle; node B, a separate daemon with its own keys,
//! pulls it, which stages it. The node's own operator then signs a
//! `node.bundle.apply-staged` request and queues it; the next start applies it
//! before any other startup check. The tests drive the real daemon through
//! stop and start: the apply is never invoked in process.
//!
//! Crash boundaries inside the install (the undo that fails, and a death after
//! the registration commit with another tree in place) cannot be cut from a
//! test binary: the fault hook is compiled only into the `ryeos-api` unit
//! tests. They are covered there, by
//! `a_failed_undo_leaves_a_journal_that_reconciles_to_the_undo_not_the_install`
//! and `a_staged_rerun_seals_then_refuses_when_the_installed_tree_is_not_the_stage`
//! in `handlers/bundle_install.rs`, which assert the same two outcomes: the
//! node boots without the bundle after a failed undo, and boots with the
//! registration sealed and the `bundle_already_installed` refusal after the
//! other tree.

use ryeosd::test_support as common;

use std::path::{Path, PathBuf};

use common::DaemonHarness;
use common::fast_fixture::{
    FastFixture, register_workflow_bundles, write_authorized_key_with_scopes,
};
use lillux::crypto::SigningKey;
use ryeos_engine::roots::RuntimeRoot;
use ryeos_engine::trust::PublicationRecord;
use ryeos_node::offline_operation::bundle_apply::{
    BundleApplyStagedArguments, bundle_set_after, installed_bundle_names,
};
use ryeos_node::offline_operation::sign::{Delivery, SignDraft, sign_request_now};
use serde_json::{Value, json};

const BUNDLE: &str = "ryeos-docs";
const OPERATION: &str = "node.bundle.apply-staged";
const LIST_CAP: &str = "ryeos.execute.service.bundle/list";
const CLEAR_CAP: &str = "ryeos.execute.service.bundle/stage-clear";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn principal_key() -> SigningKey {
    SigningKey::from_bytes(&[0x61; 32])
}

fn stranger_key() -> SigningKey {
    SigningKey::from_bytes(&[0x62; 32])
}

async fn execute_with(
    h: &DaemonHarness,
    key: &SigningKey,
    item_ref: &str,
    params: Value,
) -> (u16, Value) {
    let node_key = h.node_key.as_ref().expect("fast fixture node key");
    let body = json!({
        "item_ref": item_ref,
        "ref_bindings": {},
        "project_path": null,
        "parameters": params,
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    });
    let bytes = serde_json::to_vec(&body).unwrap();
    let mut request = reqwest::Client::new()
        .post(format!("http://{}/execute", h.bind))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in
        common::build_signed_headers_for_bytes(key, node_key, "POST", "/execute", &bytes)
    {
        request = request.header(name, value);
    }
    let response = request.send().await.expect("post /execute");
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or_else(|_| json!({})))
}

/// A service call signed by the node's own operator.
async fn service(h: &DaemonHarness, service: &str, params: Value) -> (u16, Value) {
    let key = h.user_key.clone().expect("fast fixture operator key");
    execute_with(h, &key, service, params).await
}

async fn start_publisher() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with(
        |state_path, _, fixture| register_workflow_bundles(state_path, fixture),
        |_| {},
    )
    .await
    .expect("start the publishing node")
}

/// A second node with its own node and operator keys and only the
/// Core-knowledge profile: it has never seen the published bundle.
async fn start_puller() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_core_knowledge_with_node_and_user_keys(
        SigningKey::from_bytes(&[0x5b; 32]),
        SigningKey::from_bytes(&[0x5c; 32]),
    )
    .await
    .expect("start the pulling node")
}

struct Publication {
    record_hash: String,
    node_fingerprint: String,
    url: String,
}

async fn publish(a: &DaemonHarness, fixture: &FastFixture) -> Publication {
    let (status, frozen) = service(
        a,
        "service:publication/freeze",
        json!({ "bundle_name": BUNDLE }),
    )
    .await;
    assert_eq!(status, 200, "{frozen:#}");
    let result = &frozen["result"];
    let node_fingerprint = result["node_fingerprint"].as_str().unwrap().to_owned();
    let read_bound = result["largest_file_bytes"]
        .as_u64()
        .unwrap()
        .max(result["manifest_bytes"].as_u64().unwrap())
        .max(ryeos_engine::trust::MIN_PUBLICATION_READ_BYTES);
    let now = lillux::time::unix_secs_now();
    let record = PublicationRecord::new(
        &hex(&[0x41; 16]),
        &hex(&rand::random::<[u8; 16]>()),
        &node_fingerprint,
        result["manifest_hash"].as_str().unwrap(),
        BUNDLE,
        result["bundle_version"].as_str().unwrap(),
        &lillux::time::iso8601_from_unix_secs(now + 3600),
        &lillux::time::iso8601_from_unix_secs(now),
        read_bound,
    );
    let signature = record.sign_operator(&fixture.user).expect("sign");
    let (status, published) = service(
        a,
        "service:publication/publish",
        json!({ "record": record, "signature": signature }),
    )
    .await;
    assert_eq!(status, 200, "{published:#}");
    Publication {
        url: format!("http://{}/published/{}", a.bind, record.id),
        record_hash: published["result"]["record_hash"]
            .as_str()
            .unwrap()
            .to_owned(),
        node_fingerprint,
    }
}

/// Pull the published bundle onto `b`: a stage, returned by its tree digest.
async fn pull(b: &DaemonHarness, publication: &Publication) -> String {
    let (status, staged) = service(
        b,
        "service:bundle/pull",
        json!({
            "url": publication.url,
            "node": format!("fp:{}", publication.node_fingerprint),
            "expect": publication.record_hash,
        }),
    )
    .await;
    assert_eq!(status, 200, "{staged:#}");
    assert_eq!(staged["result"]["status"], "staged", "{staged:#}");
    staged["result"]["tree_digest"]
        .as_str()
        .expect("tree digest")
        .to_owned()
}

/// A puller holding one stage of the published bundle, with the publisher that
/// served it still running for a test that pulls again.
async fn puller_with_a_stage() -> (
    DaemonHarness,
    DaemonHarness,
    FastFixture,
    Publication,
    String,
) {
    let (a, a_fixture) = start_publisher().await;
    let (b, b_fixture) = start_puller().await;
    let publication = publish(&a, &a_fixture).await;
    let digest = pull(&b, &publication).await;
    (a, b, b_fixture, publication, digest)
}

fn stage_dir(b: &DaemonHarness, digest: &str) -> PathBuf {
    b.state_path.join(".ai/state/bundle-staging").join(digest)
}

fn app_root(b: &DaemonHarness) -> &Path {
    &b.state_path
}

async fn get_signed(b: &DaemonHarness, path: &str) -> (u16, Value) {
    let user_key = b.user_key.as_ref().expect("fast fixture operator key");
    let node_key = b.node_key.as_ref().expect("fast fixture node key");
    let mut request = reqwest::Client::new().get(format!("http://{}{path}", b.bind));
    for (name, value) in
        common::build_signed_headers_for_bytes(user_key, node_key, "GET", path, b"")
    {
        request = request.header(name, value);
    }
    let response = request.send().await.expect("get");
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or_else(|_| json!({})))
}

/// The arguments a correct apply of `digest` carries on `b` as it stands.
fn arguments(b: &DaemonHarness, name: &str, digest: &str) -> BundleApplyStagedArguments {
    let installed = installed_bundle_names(app_root(b)).expect("installed names");
    BundleApplyStagedArguments {
        name: name.to_owned(),
        staged_digest: digest.to_owned(),
        replace: false,
        expected_bundle_set: bundle_set_after(&installed, name, digest).expect("bundle set"),
    }
}

/// Sign `arguments` with `key` against the facts the running node reports.
async fn sign_apply(
    b: &DaemonHarness,
    key: &SigningKey,
    arguments: &BundleApplyStagedArguments,
) -> Delivery {
    let (status, report) = get_signed(b, "/offline-operations/facts").await;
    assert!(
        (200..300).contains(&status),
        "facts route: {status} {report}"
    );
    let facts = report["facts"].as_object().expect("facts object");
    let spec = ryeos_node::offline_operation::table::lookup(OPERATION).expect("operation");
    let pins = ryeos_node::offline_operation::facts::MANDATORY_FACTS
        .iter()
        .chain(spec.preconditions)
        .map(|fact| {
            (
                (*fact).to_owned(),
                facts[*fact].as_str().expect("fact value").to_owned(),
            )
        })
        .collect();
    sign_request_now(
        SignDraft {
            node_fingerprint: report["node_fingerprint"]
                .as_str()
                .expect("node fingerprint")
                .to_owned(),
            operation: OPERATION.to_owned(),
            arguments: serde_json::to_value(arguments).expect("arguments"),
            inputs: Default::default(),
            expected_result: Default::default(),
            pins,
            lifetime_minutes: None,
            remote: None,
        },
        key,
    )
    .expect("sign the request")
}

/// Queue `arguments` signed by the node's operator, without stopping the node.
/// Returns the request digest.
async fn queue_apply(b: &DaemonHarness, arguments: &BundleApplyStagedArguments) -> String {
    let key = b.user_key.clone().expect("fast fixture operator key");
    let delivery = sign_apply(b, &key, arguments).await;
    let request_digest = delivery.request.digest().expect("request digest");
    let (status, reply) = b
        .post_json(
            "/offline-operations",
            serde_json::to_value(&delivery).expect("delivery json"),
        )
        .await
        .expect("post the request");
    assert!(status.is_success(), "{status}: {reply}");
    assert_eq!(reply["request_digest"], request_digest, "{reply}");
    request_digest
}

/// Stop the node and start it again: the queued request is applied at start.
async fn restart_to_apply(b: &mut DaemonHarness) {
    b.stop_gracefully().await.expect("stop the node");
    b.respawn_with(|_| {}).await.expect("the node boots");
}

/// Queue `arguments`, restart, and return the receipt's result.
async fn apply(b: &mut DaemonHarness, arguments: &BundleApplyStagedArguments) -> Value {
    let request_digest = queue_apply(b, arguments).await;
    restart_to_apply(b).await;
    receipt_result(b, &request_digest)
}

fn receipt_result(b: &DaemonHarness, request_digest: &str) -> Value {
    let node = ryeos_node_runtime::identity::NodeIdentity::load(
        &RuntimeRoot::new(app_root(b).to_path_buf()).node_signing_key_path(),
    )
    .expect("node identity");
    let receipts =
        ryeos_node::offline_operation::read_receipts(app_root(b), &node).expect("read the ledger");
    receipts
        .receipts
        .into_iter()
        .find(|receipt| receipt.body.request_digest == request_digest)
        .unwrap_or_else(|| panic!("no receipt for request {request_digest}"))
        .body
        .result
}

async fn listing(b: &DaemonHarness) -> Value {
    let (status, listed) = service(b, "service:bundle/list", json!({})).await;
    assert_eq!(status, 200, "{listed:#}");
    listed["result"].clone()
}

fn staged_entry(listed: &Value, name: &str) -> Option<Value> {
    listed["staged"]
        .as_array()
        .expect("staged")
        .iter()
        .find(|entry| entry["name"] == name)
        .cloned()
}

fn is_installed(listed: &Value, name: &str) -> bool {
    listed["bundles"]
        .as_array()
        .expect("bundles")
        .iter()
        .any(|entry| entry["name"] == name)
}

fn registrations(b: &DaemonHarness) -> Vec<(String, Vec<u8>)> {
    let mut found: Vec<(String, Vec<u8>)> =
        std::fs::read_dir(b.state_path.join(".ai/node/bundles"))
            .expect("registrations")
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
    found.sort();
    found
}

fn nothing_was_installed(b: &DaemonHarness) {
    assert!(
        !b.state_path.join(".ai/bundles").join(BUNDLE).exists(),
        "a refused apply left a bundle behind"
    );
    assert!(
        !b.state_path
            .join(".ai/node/bundles")
            .join(format!("{BUNDLE}.yaml"))
            .exists(),
        "a refused apply registered a bundle"
    );
    let leftovers: Vec<String> = std::fs::read_dir(b.state_path.join(".ai/bundles"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with('.') && name.ends_with(".staging"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "a refused apply left install staging behind: {leftovers:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_staged_bundle_is_installed_by_the_operators_signed_request_at_the_next_start() {
    let (_a, mut b, _fixture, _publication, digest) = puller_with_a_stage().await;
    let args = arguments(&b, BUNDLE, &digest);
    let expected_set = args.expected_bundle_set.clone();

    // Before the request is applied the stage is only a stage.
    assert!(!is_installed(&listing(&b).await, BUNDLE));
    assert!(stage_dir(&b, &digest).is_dir());

    let request_digest = queue_apply(&b, &args).await;
    assert!(
        stage_dir(&b, &digest).is_dir(),
        "queueing a request must not apply it"
    );
    restart_to_apply(&mut b).await;

    // The node boots with the bundle installed and registered, the stage gone.
    let listed = listing(&b).await;
    assert!(is_installed(&listed, BUNDLE), "{listed:#}");
    assert!(staged_entry(&listed, BUNDLE).is_none(), "{listed:#}");
    assert!(
        !stage_dir(&b, &digest).exists(),
        "the stage was not removed"
    );
    assert!(b.state_path.join(".ai/bundles").join(BUNDLE).is_dir());
    assert!(
        b.state_path
            .join(".ai/node/bundles")
            .join(format!("{BUNDLE}.yaml"))
            .is_file()
    );

    // The ledger receipt is node-signed, chained, and names the staged digest.
    let result = receipt_result(&b, &request_digest);
    assert_eq!(result["outcome"], "applied", "{result:#}");
    assert_eq!(result["staged_digest"], digest, "{result:#}");
    assert_eq!(result["name"], BUNDLE, "{result:#}");
    assert_eq!(result["bundle_set_after"], expected_set, "{result:#}");
    assert_eq!(result["stage_removed"], true, "{result:#}");
    let (status, status_report) = get_signed(&b, "/offline-operations").await;
    assert_eq!(status, 200, "{status_report}");
    let receipts = status_report["receipts"].as_array().expect("receipts");
    assert_eq!(receipts.len(), 1, "{status_report}");
    assert_eq!(receipts[0]["operation"], OPERATION);
    assert_eq!(receipts[0]["outcome"], "applied");
    assert!(
        status_report["queued"].as_array().unwrap().is_empty(),
        "the applied request was not consumed: {status_report}"
    );

    // The node keeps booting with the bundle installed.
    b.stop_gracefully().await.expect("stop");
    b.respawn_with(|_| {}).await.expect("boot again");
    assert!(is_installed(&listing(&b).await, BUNDLE));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_apply_leaves_the_node_booting_without_the_bundle_and_says_why() {
    let (_a, mut b, fixture, _publication, digest) = puller_with_a_stage().await;
    // A principal that may read the bundle list and nothing else.
    let principal = principal_key();
    write_authorized_key_with_scopes(&b.state_path, &principal, &fixture.node, &[LIST_CAP])
        .expect("authorize the principal");
    let registrations_before = registrations(&b);
    let good = arguments(&b, BUNDLE, &digest);

    // A request that expects another bundle set is refused before any copy.
    let mut wrong_set = good.clone();
    wrong_set.expected_bundle_set = bundle_set_after(
        &installed_bundle_names(app_root(&b)).unwrap(),
        BUNDLE,
        &"f".repeat(64),
    )
    .unwrap();
    let result = apply(&mut b, &wrong_set).await;
    assert_eq!(result["outcome"], "refused", "{result:#}");
    assert_eq!(result["code"], "bundle_set_mismatch", "{result:#}");
    nothing_was_installed(&b);
    assert_eq!(registrations(&b), registrations_before);

    // A stage that names another bundle than the request does.
    let other = arguments(&b, "another-name", &digest);
    let result = apply(&mut b, &other).await;
    assert_eq!(result["code"], "stage_name_mismatch", "{result:#}");
    nothing_was_installed(&b);

    // A changed byte in the staged tree is refused by the tree digest, the
    // node boots without the bundle, and the registrations are untouched.
    let victim = first_regular_file(&stage_dir(&b, &digest).join("tree"));
    let original = std::fs::read(&victim).expect("read the staged file");
    let mut tampered = original.clone();
    tampered[0] ^= 0xff;
    std::fs::write(&victim, &tampered).expect("tamper with the stage");
    let request_digest = queue_apply(&b, &good).await;
    restart_to_apply(&mut b).await;
    let result = receipt_result(&b, &request_digest);
    assert_eq!(result["outcome"], "refused", "{result:#}");
    assert_eq!(result["code"], "tree_digest_mismatch", "{result:#}");
    assert_eq!(result["staged_digest"], digest, "{result:#}");
    nothing_was_installed(&b);
    assert_eq!(registrations(&b), registrations_before);
    let (_, status_report) = get_signed(&b, "/offline-operations").await;
    let last = status_report["receipts"]
        .as_array()
        .unwrap()
        .last()
        .cloned();
    let last = last.expect("a receipt");
    assert_eq!(last["outcome"], "refused", "{status_report}");
    assert_eq!(last["code"], "tree_digest_mismatch", "{status_report}");

    // The operator sees the refusal and its code; a principal sees `staged`.
    let listed = listing(&b).await;
    assert!(!is_installed(&listed, BUNDLE), "{listed:#}");
    let entry = staged_entry(&listed, BUNDLE).expect("the refused stage is still listed");
    assert_eq!(entry["status"], "apply_refused", "{listed:#}");
    assert_eq!(entry["apply_refusal"]["code"], "tree_digest_mismatch");
    assert_eq!(entry["apply_refusal"]["request_digest"], request_digest);
    let (status, seen) = execute_with(&b, &principal, "service:bundle/list", json!({})).await;
    assert_eq!(status, 200, "{seen:#}");
    let entry = staged_entry(&seen["result"], BUNDLE).expect("a principal sees the stage");
    assert_eq!(
        entry,
        json!({ "name": BUNDLE, "status": "staged" }),
        "{seen:#}"
    );

    // A stage whose record does not verify is refused and no marker is
    // written for it.
    std::fs::write(&victim, &original).expect("restore the staged byte");
    let marker = stage_dir(&b, &digest).join("apply.refused");
    assert!(marker.is_file(), "the refusals left no marker");
    std::fs::remove_file(&marker).expect("remove the marker");
    let record_path = stage_dir(&b, &digest).join("stage.record");
    let record = std::fs::read(&record_path).expect("read the record");
    let mut broken = record.clone();
    let middle = broken.len() / 2;
    broken[middle] ^= 0x01;
    std::fs::write(&record_path, &broken).expect("break the record");
    let result = apply(&mut b, &good).await;
    assert_eq!(result["outcome"], "refused", "{result:#}");
    assert_eq!(result["code"], "stage_record_unverified", "{result:#}");
    assert!(
        !marker.exists(),
        "a marker was written for a stage that does not verify"
    );
    nothing_was_installed(&b);
    std::fs::write(&record_path, &record).expect("restore the record");

    // With the bytes restored, the same stage applies by a new signed request.
    let result = apply(&mut b, &good).await;
    assert_eq!(result["outcome"], "applied", "{result:#}");
    assert_eq!(result["staged_digest"], digest, "{result:#}");
    let listed = listing(&b).await;
    assert!(is_installed(&listed, BUNDLE), "{listed:#}");
    assert!(staged_entry(&listed, BUNDLE).is_none(), "{listed:#}");
    assert!(!stage_dir(&b, &digest).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_nodes_own_operator_key_can_sign_an_apply() {
    let (_a, b, fixture, _publication, digest) = puller_with_a_stage().await;
    let principal = principal_key();
    write_authorized_key_with_scopes(
        &b.state_path,
        &principal,
        &fixture.node,
        &[
            LIST_CAP,
            CLEAR_CAP,
            "ryeos.offline.node.bundle.apply-staged",
        ],
    )
    .expect("authorize the principal");
    let args = arguments(&b, BUNDLE, &digest);

    // A key the node has never heard of, and a key holding a grant (even one
    // that names the exact scope), are each refused at delivery: nothing is
    // queued and the stage is untouched.
    for key in [stranger_key(), principal] {
        let signer = lillux::signature::compute_fingerprint(&key.verifying_key());
        let delivery = sign_apply(&b, &key, &args).await;
        let (status, reply) = b
            .post_json(
                "/offline-operations",
                serde_json::to_value(&delivery).unwrap(),
            )
            .await
            .expect("post the request");
        assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{reply}");
        if signer == lillux::signature::compute_fingerprint(&stranger_key().verifying_key()) {
            assert_eq!(reply["reason"], "unknown_signer", "{reply}");
        }
        let (_, report) = get_signed(&b, "/offline-operations").await;
        assert!(
            report["queued"].as_array().unwrap().is_empty(),
            "a refused request was queued: {report}"
        );
        assert!(stage_dir(&b, &digest).is_dir());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_operator_clears_stages_by_digest_by_refusal_or_all_and_nobody_else_can() {
    let (_a, mut b, fixture, publication, digest) = puller_with_a_stage().await;
    let principal = principal_key();
    write_authorized_key_with_scopes(
        &b.state_path,
        &principal,
        &fixture.node,
        &[LIST_CAP, CLEAR_CAP],
    )
    .expect("authorize the principal");

    // A key that holds the capability but is not the operator cannot clear,
    // not even to free its own share.
    let (status, refused) = execute_with(
        &b,
        &principal,
        "service:bundle/stage-clear",
        json!({ "digests": [digest] }),
    )
    .await;
    assert_ne!(status, 200, "{refused:#}");
    assert!(
        stage_dir(&b, &digest).is_dir(),
        "a non-operator cleared a stage"
    );

    // Exactly one selector, and a stage the node does not hold is typed.
    let (status, refused) = service(&b, "service:bundle/stage-clear", json!({})).await;
    assert_eq!(status, 400, "{refused:#}");
    let (status, refused) = service(
        &b,
        "service:bundle/stage-clear",
        json!({ "all": true, "apply_refused": true }),
    )
    .await;
    assert_eq!(status, 400, "{refused:#}");
    let (status, refused) = service(
        &b,
        "service:bundle/stage-clear",
        json!({ "digests": ["e".repeat(64)] }),
    )
    .await;
    assert_eq!(status, 404, "{refused:#}");
    assert!(
        refused.to_string().contains("bundle_stage_not_found"),
        "{refused:#}"
    );
    assert!(stage_dir(&b, &digest).is_dir());

    // Nothing is refused yet, so clearing refused stages removes nothing.
    let (status, cleared) = service(
        &b,
        "service:bundle/stage-clear",
        json!({ "apply_refused": true }),
    )
    .await;
    assert_eq!(status, 200, "{cleared:#}");
    assert!(cleared["result"]["removed"].as_array().unwrap().is_empty());
    assert_eq!(cleared["result"]["kept"].as_array().unwrap().len(), 1);

    // Refuse an apply, then clear the refused stage.
    let mut wrong_set = arguments(&b, BUNDLE, &digest);
    wrong_set.expected_bundle_set = "a".repeat(64);
    let result = apply(&mut b, &wrong_set).await;
    assert_eq!(result["code"], "bundle_set_mismatch", "{result:#}");
    let entry = staged_entry(&listing(&b).await, BUNDLE).expect("the stage stays");
    assert_eq!(entry["status"], "apply_refused");
    let (status, cleared) = service(
        &b,
        "service:bundle/stage-clear",
        json!({ "apply_refused": true }),
    )
    .await;
    assert_eq!(status, 200, "{cleared:#}");
    let removed = cleared["result"]["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 1, "{cleared:#}");
    assert_eq!(removed[0]["tree_digest"], digest, "{cleared:#}");
    assert_eq!(removed[0]["name"], BUNDLE, "{cleared:#}");
    assert!(!stage_dir(&b, &digest).exists());
    assert!(staged_entry(&listing(&b).await, BUNDLE).is_none());

    // By digest.
    let again = pull(&b, &publication).await;
    assert_eq!(again, digest, "the same bytes are the same stage");
    let (status, cleared) = service(
        &b,
        "service:bundle/stage-clear",
        json!({ "digests": [digest] }),
    )
    .await;
    assert_eq!(status, 200, "{cleared:#}");
    assert_eq!(cleared["result"]["removed"].as_array().unwrap().len(), 1);
    assert!(!stage_dir(&b, &digest).exists());

    // All: the stage, a crash remnant and a temporary directory old enough to
    // belong to a transfer that is gone, but not a younger one, which may be a
    // transfer in flight.
    pull(&b, &publication).await;
    let area = b.state_path.join(".ai/state/bundle-staging");
    let remnant = area.join("c".repeat(64));
    lillux::create_directory_exact_mode(&remnant, 0o700).expect("plant a remnant");
    let stale = area.join(".tmp-stale");
    lillux::create_directory_exact_mode(&stale, 0o700).expect("plant a stale temp");
    std::fs::File::open(&stale)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400))
        .expect("age the stale temp");
    let fresh = area.join(".tmp-fresh");
    lillux::create_directory_exact_mode(&fresh, 0o700).expect("plant a fresh temp");
    let (status, cleared) = service(&b, "service:bundle/stage-clear", json!({ "all": true })).await;
    assert_eq!(status, 200, "{cleared:#}");
    assert_eq!(cleared["result"]["removed"].as_array().unwrap().len(), 1);
    assert_eq!(cleared["result"]["remnants"], 2, "{cleared:#}");
    assert!(!stage_dir(&b, &digest).exists());
    assert!(!remnant.exists() && !stale.exists());
    assert!(fresh.is_dir(), "a transfer in flight was removed");
    std::fs::remove_dir(&fresh).expect("remove the fresh temp");

    // A stage cleared after its apply was queued and before the apply runs:
    // the apply is refused, consumed, and the node boots.
    let digest = pull(&b, &publication).await;
    let args = arguments(&b, BUNDLE, &digest);
    let request_digest = queue_apply(&b, &args).await;
    let (status, cleared) = service(
        &b,
        "service:bundle/stage-clear",
        json!({ "digests": [digest] }),
    )
    .await;
    assert_eq!(status, 200, "{cleared:#}");
    restart_to_apply(&mut b).await;
    let result = receipt_result(&b, &request_digest);
    assert_eq!(result["outcome"], "refused", "{result:#}");
    assert_eq!(result["code"], "stage_not_found", "{result:#}");
    nothing_was_installed(&b);
    let (_, report) = get_signed(&b, "/offline-operations").await;
    assert!(report["queued"].as_array().unwrap().is_empty(), "{report}");
}

/// The first non-empty regular file under `root`, in name order.
fn first_regular_file(root: &Path) -> PathBuf {
    fn find(root: &Path) -> Option<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_dir() {
                if let Some(found) = find(&path) {
                    return Some(found);
                }
            } else if meta.file_type().is_file() && meta.len() > 0 {
                return Some(path);
            }
        }
        None
    }
    find(root).expect("the staged tree holds no regular file")
}

/// A puller whose signed `bundle_staging` policy member states 1-byte caps:
/// the member is rewritten and re-signed with the fixture's node key after the
/// profile's generation is written and before initialization is sealed, so the
/// node loads it through the real policy load.
async fn start_puller_with_tiny_staging_caps() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_core_knowledge_with_node_and_user_keys_and_plant(
        SigningKey::from_bytes(&[0x5d; 32]),
        SigningKey::from_bytes(&[0x5e; 32]),
        |state_path, _, fixture| {
            let path = state_path.join(".ai/node/policies/bundle_staging.yaml");
            assert!(path.is_file(), "the profile wrote no bundle_staging member");
            let body = serde_yaml::to_string(&json!({
                "schema": 1,
                "max_node_bytes": 1,
                "max_node_stages": 1,
                "max_key_bytes": 1,
                "max_key_stages": 1,
            }))?;
            std::fs::write(
                &path,
                lillux::signature::sign_content_at(
                    &body,
                    &fixture.node,
                    "#",
                    None,
                    common::fast_fixture::FAST_FIXTURE_TIME,
                ),
            )?;
            Ok(())
        },
    )
    .await
    .expect("start the pulling node with tiny staging caps")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_caps_a_node_stages_under_are_the_ones_its_signed_policy_states() {
    let (a, a_fixture) = start_publisher().await;
    let publication = publish(&a, &a_fixture).await;
    let pull_request = json!({
        "url": publication.url,
        "node": format!("fp:{}", publication.node_fingerprint),
        "expect": publication.record_hash,
    });

    // Under the policy's 1-byte caps the stage is refused with the typed cap
    // code, the message names the clear, and nothing is staged.
    let (tiny, _fixture) = start_puller_with_tiny_staging_caps().await;
    let (status, refused) = service(&tiny, "service:bundle/pull", pull_request.clone()).await;
    assert_eq!(status, 507, "{refused:#}");
    let text = refused.to_string();
    assert!(text.contains("bundle_stage_node_bytes_cap"), "{refused:#}");
    assert!(text.contains("ryeos bundle stage-clear"), "{refused:#}");
    let listed = listing(&tiny).await;
    assert!(
        listed["staged"].as_array().unwrap().is_empty(),
        "{listed:#}"
    );
    let area = tiny.state_path.join(".ai/state/bundle-staging");
    let held: Vec<String> = std::fs::read_dir(&area)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(held.is_empty(), "a refused stage left {held:?}");

    // The same stage passes under the shipped caps (2 GiB / 64, 512 MiB / 16).
    let (shipped, _fixture) = start_puller().await;
    let (status, staged) = service(&shipped, "service:bundle/pull", pull_request).await;
    assert_eq!(status, 200, "{staged:#}");
    assert_eq!(staged["result"]["status"], "staged", "{staged:#}");
}
