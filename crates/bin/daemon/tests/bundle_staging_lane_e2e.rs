//! End to end: the publication lane at stage and at apply.
//!
//! A bundle pulled from a publication is audited by the lane's rules before it
//! is staged (a refusal stages nothing and writes nothing to the offline
//! ledger), and again on the install's own copy when a signed request applies
//! it, with the node's `publication_lane` policy as it stands then. The marker
//! is written into the node-signed registration only by that apply.
//!
//! The tests drive real daemons through stop and start. Revoking a lift after
//! staging is shown by moving a stage, whose signed record verifies under the
//! same node key, to a node whose policy does not name the bundle.
//!
//! The post-move digest recheck (`tree_changed_during_install`, with nothing
//! registered and no marker) is covered in `handlers/bundle_install.rs` by
//! `a_post_move_refusal_undoes_an_install_through_the_journal`; it covers a
//! change after the audit and does not test the audit itself. The missing
//! `publication_lane` section at apply (`lane_policy_unavailable`) cannot be
//! staged here, because a node without the section does not boot; it is
//! covered in `handlers/stage_lane.rs`.

use ryeosd::test_support as common;

use std::path::{Path, PathBuf};

use common::DaemonHarness;
use common::fast_fixture::{
    FAST_FIXTURE_TIME, FastFixture, register_config_fixture_bundle, register_workflow_bundles,
};
use lillux::crypto::SigningKey;
use ryeos_engine::roots::RuntimeRoot;
use ryeos_engine::trust::PublicationRecord;
use ryeos_node::offline_operation::bundle_apply::{
    BundleApplyStagedArguments, bundle_set_after, installed_bundle_names,
};
use ryeos_node::offline_operation::sign::{Delivery, SignDraft, sign_request_now};
use serde_json::{Value, json};

const OPERATION: &str = "node.bundle.apply-staged";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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

fn app_root(b: &DaemonHarness) -> &Path {
    &b.state_path
}

fn stage_dir(b: &DaemonHarness, digest: &str) -> PathBuf {
    b.state_path.join(".ai/state/bundle-staging").join(digest)
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

struct Publication {
    record_hash: String,
    node_fingerprint: String,
    url: String,
}

/// Freeze `bundle` on `a`, and publish it under a record id built from
/// `id_byte`, so one node can hold several publications.
async fn publish_bundle(
    a: &DaemonHarness,
    fixture: &FastFixture,
    bundle: &str,
    id_byte: u8,
) -> Publication {
    let (status, frozen) = service(
        a,
        "service:publication/freeze",
        json!({ "bundle_name": bundle }),
    )
    .await;
    assert_eq!(status, 200, "{bundle}: {frozen:#}");
    let result = &frozen["result"];
    let node_fingerprint = result["node_fingerprint"].as_str().unwrap().to_owned();
    let read_bound = result["largest_file_bytes"]
        .as_u64()
        .unwrap()
        .max(result["manifest_bytes"].as_u64().unwrap())
        .max(ryeos_engine::trust::MIN_PUBLICATION_READ_BYTES);
    let now = lillux::time::unix_secs_now();
    let record = PublicationRecord::new(
        &hex(&[id_byte; 16]),
        &hex(&rand::random::<[u8; 16]>()),
        &node_fingerprint,
        result["manifest_hash"].as_str().unwrap(),
        bundle,
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
    assert_eq!(status, 200, "{bundle}: {published:#}");
    Publication {
        url: format!("http://{}/published/{}", a.bind, record.id),
        record_hash: published["result"]["record_hash"]
            .as_str()
            .unwrap()
            .to_owned(),
        node_fingerprint,
    }
}

/// Pull `publication` onto `b`: the status and the answer.
async fn pull_raw(b: &DaemonHarness, publication: &Publication) -> (u16, Value) {
    service(
        b,
        "service:bundle/pull",
        json!({
            "url": publication.url,
            "node": format!("fp:{}", publication.node_fingerprint),
            "expect": publication.record_hash,
        }),
    )
    .await
}

/// Pull and expect a stage; returns its tree digest and the answer.
async fn pull(b: &DaemonHarness, publication: &Publication) -> (String, Value) {
    let (status, staged) = pull_raw(b, publication).await;
    assert_eq!(status, 200, "{staged:#}");
    assert_eq!(staged["result"]["status"], "staged", "{staged:#}");
    let digest = staged["result"]["tree_digest"]
        .as_str()
        .expect("tree digest")
        .to_owned();
    (digest, staged["result"].clone())
}

/// A node with the Core-knowledge profile and the default shipped policy,
/// under keys that other nodes in a test may share.
async fn start_puller() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_core_knowledge_with_node_and_user_keys(
        SigningKey::from_bytes(&[0x5b; 32]),
        SigningKey::from_bytes(&[0x5c; 32]),
    )
    .await
    .expect("start the pulling node")
}

/// A node whose signed `publication_lane` policy names `NAMED_BUNDLE`, under
/// the same keys as [`start_puller`].
async fn start_puller_naming_a_bundle() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_core_knowledge_with_node_and_user_keys_and_plant(
        SigningKey::from_bytes(&[0x5b; 32]),
        SigningKey::from_bytes(&[0x5c; 32]),
        |state_path, _, fixture| {
            let body = format!(
                "schema: 1\nadmit_activations:\n  - invoked\nliftable_facets:\n  - owns_namespaces\nnamed_bundles:\n  {NAMED_BUNDLE}:\n    kinds:\n      - config\n    facets:\n      - owns_namespaces\n"
            );
            std::fs::write(
                state_path.join(".ai/node/policies/publication_lane.yaml"),
                lillux::signature::sign_content_at(
                    &body,
                    &fixture.node,
                    "#",
                    None,
                    FAST_FIXTURE_TIME,
                ),
            )?;
            Ok(())
        },
    )
    .await
    .expect("start the pulling node naming a bundle")
}

/// Queue `arguments`, restart, and return the receipt's result.
async fn apply(b: &mut DaemonHarness, arguments: &BundleApplyStagedArguments) -> Value {
    let request_digest = queue_apply(b, arguments).await;
    restart_to_apply(b).await;
    receipt_result(b, &request_digest)
}

/// How many receipts the node's offline ledger holds.
fn receipt_count(b: &DaemonHarness) -> usize {
    let node = ryeos_node_runtime::identity::NodeIdentity::load(
        &RuntimeRoot::new(app_root(b).to_path_buf()).node_signing_key_path(),
    )
    .expect("node identity");
    ryeos_node::offline_operation::read_receipts(app_root(b), &node)
        .map(|read| read.receipts.len())
        .unwrap_or(0)
}

fn registration_path(b: &DaemonHarness, name: &str) -> PathBuf {
    b.state_path
        .join(".ai/node/bundles")
        .join(format!("{name}.yaml"))
}

fn registration_text(b: &DaemonHarness, name: &str) -> String {
    std::fs::read_to_string(registration_path(b, name)).expect("the registration")
}

fn staging_area_entries(b: &DaemonHarness) -> Vec<String> {
    std::fs::read_dir(b.state_path.join(".ai/state/bundle-staging"))
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Run a stopped-node service (`ryeosd run-service`) against `b`'s state.
fn run_stopped_service(b: &DaemonHarness, service: &str, params: &str) -> std::process::Output {
    std::process::Command::new(common::ryeosd_binary())
        .env("RUST_MIN_STACK", "4194304")
        .arg("--app-root")
        .arg(&b.state_path)
        .arg("--uds-path")
        .arg(&b.uds_path)
        .arg("run-service")
        .arg(service)
        .arg("--params")
        .arg(params)
        .env("HOSTNAME", "testhost")
        .env("RYEOS_APP_ROOT", &b.state_path)
        .env("HOME", b.user_space.path())
        .output()
        .expect("run ryeosd run-service")
}

fn copy_all(from: &Path, to: &Path) {
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .expect("run cp");
    assert!(status.success(), "cp {} {}", from.display(), to.display());
}

// ── Publishers ──────────────────────────────────────────────────────────

/// A bundle of items only: one tool and one knowledge item, signed by the
/// fixture publisher, its manifest declaring the two kinds it uses.
const ITEMS_BUNDLE: &str = "lane-items";

async fn start_publisher_with_items_bundle() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
            let tool_source =
                std::fs::read_to_string(repo.join("bundles/web/.ai/tools/rye/web/fetch.yaml"))?;
            let sign_at = |body: &str, prefix: &str, suffix: Option<&str>| {
                lillux::signature::sign_content_at(
                    body,
                    &fixture.publisher,
                    prefix,
                    suffix,
                    FAST_FIXTURE_TIME,
                )
            };
            register_config_fixture_bundle(state_path, ITEMS_BUNDLE, fixture, |root| {
                let tool = root.join(".ai/tools/rye/web/fetch.yaml");
                std::fs::create_dir_all(tool.parent().expect("a parent directory"))?;
                std::fs::write(
                    tool,
                    sign_at(
                        &lillux::signature::strip_signature_lines(&tool_source),
                        "#",
                        None,
                    ),
                )?;
                let note = root.join(".ai/knowledge/lane/demo/note.md");
                std::fs::create_dir_all(note.parent().expect("a parent directory"))?;
                std::fs::write(
                    note,
                    sign_at(
                        "---\ncategory: lane/demo\ntags: [demo]\nversion: \"1.0.0\"\ndescription: >\n  A note carried by a publication-lane bundle.\n---\n\n# Demo note\n",
                        "<!--",
                        Some("-->"),
                    ),
                )?;
                Ok(())
            })?;
            let manifest = state_path
                .join(".ai/bundles")
                .join(ITEMS_BUNDLE)
                .join(".ai/manifest.yaml");
            std::fs::write(
                &manifest,
                sign_at(
                    &format!(
                        "name: {ITEMS_BUNDLE}\nversion: 1.0.0\ndescription: items only\nprovides_kinds: []\nrequires_kinds:\n  - tool\n  - knowledge\nuses_kinds: []\n"
                    ),
                    "#",
                    None,
                ),
            )?;
            Ok(())
        },
        |_| {},
    )
    .await
    .expect("start the publishing node")
}

/// Files of a synthetic bundle: (path, bytes).
type LaneFiles = &'static [(&'static str, &'static [u8])];

/// Synthetic bundles that each carry one thing a publication-lane bundle may
/// not: (bundle name, kinds its manifest declares, files, expected typed code).
const LANE_REFUSALS: &[(&str, &[&str], LaneFiles, &str)] = &[
    (
        "lane-service",
        &["service"],
        &[(
            ".ai/services/lane/demo.yaml",
            b"kind: service\nendpoint: lane.demo\navailability: local\nlocal_execute: tool:lane/demo\nrequired_caps: []\ndescription: \"a service item\"\ndisclosure: {}\n",
        )],
        "publication_lane_kind_refused",
    ),
    (
        "lane-config",
        &["config"],
        &[(".ai/config/x/c.yaml", b"x")],
        "publication_lane_kind_refused",
    ),
    (
        "lane-bin",
        &["config"],
        &[("bin/tool", b"x")],
        "publication_lane_never_admitted",
    ),
    (
        "lane-stray",
        &["config"],
        &[("README.md", b"x")],
        "publication_lane_stray_file",
    ),
    (
        "lane-elf",
        &["knowledge"],
        &[(".ai/knowledge/x/note.md", b"\x7fELF\x02\x01\x01")],
        "publication_lane_executable_image",
    ),
];

async fn start_publisher_with_lane_refusals() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            for (bundle, kinds, files, _) in LANE_REFUSALS {
                register_config_fixture_bundle(state_path, bundle, fixture, |root| {
                    for (path, bytes) in *files {
                        let file = root.join(path);
                        std::fs::create_dir_all(file.parent().expect("a parent directory"))?;
                        // Service items are signed by the fixture publisher so
                        // the publishing node verifies them like any other.
                        let bytes = if path.starts_with(".ai/services/") {
                            lillux::signature::sign_content_at(
                                std::str::from_utf8(bytes).expect("utf-8 service item"),
                                &fixture.publisher,
                                "#",
                                None,
                                FAST_FIXTURE_TIME,
                            )
                            .into_bytes()
                        } else {
                            bytes.to_vec()
                        };
                        std::fs::write(file, bytes)?;
                    }
                    Ok(())
                })?;
                let requires: String = kinds.iter().map(|kind| format!("  - {kind}\n")).collect();
                let manifest = state_path
                    .join(".ai/bundles")
                    .join(bundle)
                    .join(".ai/manifest.yaml");
                std::fs::write(
                    &manifest,
                    lillux::signature::sign_content_at(
                        &format!(
                            "name: {bundle}\nversion: 1.0.0\ndescription: lane refusal\nprovides_kinds: []\nrequires_kinds:\n{requires}uses_kinds: []\n"
                        ),
                        &fixture.publisher,
                        "#",
                        None,
                        FAST_FIXTURE_TIME,
                    ),
                )?;
            }
            Ok(())
        },
        |_| {},
    )
    .await
    .expect("start the publishing node")
}

const NAMED_BUNDLE: &str = "lane-named";
const TWIN_BUNDLE: &str = "lane-twin";

/// A bundle of one inert config item, declaring the namespace it owns. The
/// two names carry the same bytes, so only the node's policy tells them apart.
async fn start_publisher_with_named_and_twin() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            for bundle in [NAMED_BUNDLE, TWIN_BUNDLE] {
                register_config_fixture_bundle(state_path, bundle, fixture, |root| {
                    let item = root.join(format!(".ai/config/lane-demo-{bundle}/demo.yaml"));
                    std::fs::create_dir_all(item.parent().expect("a parent directory"))?;
                    std::fs::write(
                        item,
                        lillux::signature::sign_content_at(
                            &format!("schema: 1\ncategory: lane-demo-{bundle}\nnote: an inert config item\n"),
                            &fixture.publisher,
                            "#",
                            None,
                            FAST_FIXTURE_TIME,
                        ),
                    )?;
                    Ok(())
                })?;
                let manifest = state_path
                    .join(".ai/bundles")
                    .join(bundle)
                    .join(".ai/manifest.yaml");
                std::fs::write(
                    &manifest,
                    lillux::signature::sign_content_at(
                        &format!(
                            "name: {bundle}\nversion: 1.0.0\ndescription: lane lift\nprovides_kinds: []\nrequires_kinds:\n  - config\nuses_kinds: []\nowns_namespaces:\n  - lane-demo-{bundle}\n"
                        ),
                        &fixture.publisher,
                        "#",
                        None,
                        FAST_FIXTURE_TIME,
                    ),
                )?;
            }
            Ok(())
        },
        |_| {},
    )
    .await
    .expect("start the publishing node")
}

// ── Tests ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_publication_stage_applies_in_the_lane_and_only_the_apply_writes_the_marker() {
    let (a, a_fixture) = start_publisher_with_items_bundle().await;
    let (mut b, _b_fixture) = start_puller().await;
    let publication = publish_bundle(&a, &a_fixture, ITEMS_BUNDLE, 0x60).await;
    let (digest, staged) = pull(&b, &publication).await;

    // The stage answer carries a labelled prediction; nothing is registered
    // and the signed record states no lift and no lane.
    assert_eq!(
        staged["predicted_lifts"]["note"], "prediction; policy is re-read at apply",
        "{staged:#}"
    );
    let record_text = std::fs::read_to_string(stage_dir(&b, &digest).join("stage.record"))
        .expect("the stage record");
    assert!(!record_text.contains("predicted_lifts"), "{record_text}");
    let listed = listing(&b).await;
    let entry = staged_entry(&listed, ITEMS_BUNDLE).expect("the stage is listed");
    assert!(entry.get("declared").is_some(), "{entry:#}");
    assert!(entry.get("lane").is_none(), "{entry:#}");
    assert!(!is_installed(&listed, ITEMS_BUNDLE));

    let args = arguments(&b, ITEMS_BUNDLE, &digest);
    let result = apply(&mut b, &args).await;
    assert_eq!(result["outcome"], "applied", "{result:#}");
    assert_eq!(result["lane"]["lifted"]["invoked"], true, "{result:#}");
    let receipt = result["lane"]["receipt"].as_str().expect("lane receipt");
    assert!(
        receipt.contains("From a publication, not a release"),
        "{receipt}"
    );
    assert!(receipt.contains("not checked when a stage is applied"));

    // The node-signed registration carries the marker, and bundle/list shows
    // it with the publisher and record the stage's evidence names.
    let registration = registration_text(&b, ITEMS_BUNDLE);
    assert!(registration.contains("lane:"), "{registration}");
    let listed = listing(&b).await;
    assert!(is_installed(&listed, ITEMS_BUNDLE), "{listed:#}");
    let installed = listed["bundles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == ITEMS_BUNDLE)
        .unwrap();
    assert_eq!(installed["lane"]["name"], "publication", "{installed:#}");
    assert_eq!(
        installed["lane"]["record_hash"], publication.record_hash,
        "{installed:#}"
    );
    assert_eq!(
        installed["lane"]["node_fingerprint"], publication.node_fingerprint,
        "{installed:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_that_carries_more_than_invoked_items_stages_nothing_and_writes_nothing() {
    let (a, a_fixture) = start_publisher_with_lane_refusals().await;
    let (b, _b_fixture) = start_puller().await;
    let receipts_before = receipt_count(&b);
    for (index, (bundle, _, _, code)) in LANE_REFUSALS.iter().enumerate() {
        let publication = publish_bundle(&a, &a_fixture, bundle, 0x50 + index as u8).await;
        let (status, refused) = pull_raw(&b, &publication).await;
        assert_eq!(status, 422, "{bundle}: {refused:#}");
        assert!(
            refused.to_string().contains(code),
            "{bundle}: expected {code}: {refused:#}"
        );
        assert!(
            staging_area_entries(&b).is_empty(),
            "{bundle}: a refused stage left {:?}",
            staging_area_entries(&b)
        );
        let listed = listing(&b).await;
        assert!(staged_entry(&listed, bundle).is_none(), "{listed:#}");
        assert!(!b.state_path.join(".ai/bundles").join(bundle).exists());
    }
    // A refused stage appends nothing to the node-signed offline ledger.
    assert_eq!(receipt_count(&b), receipts_before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_named_bundle_is_lifted_by_the_policy_and_its_unnamed_twin_stages_nothing() {
    let (a, a_fixture) = start_publisher_with_named_and_twin().await;
    let (mut b, _b_fixture) = start_puller_naming_a_bundle().await;

    let twin = publish_bundle(&a, &a_fixture, TWIN_BUNDLE, 0x70).await;
    let (status, refused) = pull_raw(&b, &twin).await;
    assert_eq!(status, 422, "{refused:#}");
    assert!(
        refused
            .to_string()
            .contains("publication_lane_facet_not_admitted"),
        "{refused:#}"
    );
    assert!(staging_area_entries(&b).is_empty());

    let named = publish_bundle(&a, &a_fixture, NAMED_BUNDLE, 0x71).await;
    let (digest, staged) = pull(&b, &named).await;
    assert_eq!(staged["predicted_lifts"]["kinds"], json!(["config"]));
    assert_eq!(
        staged["predicted_lifts"]["facets"],
        json!(["owns_namespaces"])
    );
    let args = arguments(&b, NAMED_BUNDLE, &digest);
    let result = apply(&mut b, &args).await;
    assert_eq!(result["outcome"], "applied", "{result:#}");
    assert_eq!(result["lane"]["lifted"]["kinds"], json!(["config"]));
    assert_eq!(
        result["lane"]["lifted"]["facets"],
        json!(["owns_namespaces"])
    );
    assert!(
        b.state_path
            .join(".ai/bundles")
            .join(NAMED_BUNDLE)
            .join(format!(".ai/config/lane-demo-{NAMED_BUNDLE}/demo.yaml"))
            .is_file()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lift_the_policy_no_longer_names_is_refused_at_apply_and_shown_to_the_operator() {
    let (a, a_fixture) = start_publisher_with_named_and_twin().await;
    let (named_node, _f1) = start_puller_naming_a_bundle().await;
    let named = publish_bundle(&a, &a_fixture, NAMED_BUNDLE, 0x73).await;
    let (digest, _) = pull(&named_node, &named).await;

    // The same node key and operator, a policy that does not name the bundle:
    // the stage's signed record verifies there, and apply reads the policy as
    // it stands on that node.
    let (mut b, _f2) = start_puller().await;
    let area = b.state_path.join(".ai/state/bundle-staging");
    lillux::create_directory_exact_mode(&area, 0o700).expect("create the staging area");
    copy_all(&stage_dir(&named_node, &digest), &area);
    let listed = listing(&b).await;
    assert!(staged_entry(&listed, NAMED_BUNDLE).is_some(), "{listed:#}");

    let receipts_before = receipt_count(&b);
    let args = arguments(&b, NAMED_BUNDLE, &digest);
    let result = apply(&mut b, &args).await;
    assert_eq!(result["outcome"], "refused", "{result:#}");
    assert_eq!(result["code"], "lane_refused", "{result:#}");
    assert!(
        result
            .to_string()
            .contains("publication_lane_facet_not_admitted"),
        "{result:#}"
    );
    assert_eq!(receipt_count(&b), receipts_before + 1);
    assert!(!b.state_path.join(".ai/bundles").join(NAMED_BUNDLE).exists());

    // The operator sees the stage as apply_refused with the closed code.
    let listed = listing(&b).await;
    let entry = staged_entry(&listed, NAMED_BUNDLE).expect("the stage stays");
    assert_eq!(entry["status"], "apply_refused", "{entry:#}");
    assert_eq!(entry["apply_refusal"]["code"], "lane_refused", "{entry:#}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stage_whose_record_names_another_publication_than_its_evidence_is_refused() {
    let (a, a_fixture) = start_publisher_with_items_bundle().await;
    let (mut b, _b_fixture) = start_puller().await;
    let publication = publish_bundle(&a, &a_fixture, ITEMS_BUNDLE, 0x74).await;
    let (digest, _) = pull(&b, &publication).await;

    // Re-commit the same tree and evidence under a record, signed by the
    // node, that claims a different publication record hash.
    let node = ryeos_node_runtime::identity::NodeIdentity::load(
        &RuntimeRoot::new(app_root(&b).to_path_buf()).node_signing_key_path(),
    )
    .expect("node identity");
    let area = ryeos_node::bundle_staging::StagingArea::open(app_root(&b)).expect("open the area");
    let verified = area
        .read(&digest, &node.signing_key().verifying_key())
        .expect("read the genuine stage");
    let mut record = verified.record.clone();
    record.source.publication_record_hash = Some("0".repeat(64));
    let evidence: Vec<(String, Vec<u8>)> = verified
        .evidence
        .iter()
        .map(|(name, bytes)| (name.clone(), bytes.clone()))
        .collect();
    // Keep the genuine tree's bytes and modes, remove the genuine stage, and
    // commit the same tree under the altered record.
    let scratch = tempfile::tempdir().expect("scratch directory");
    let kept = scratch.path().join("tree");
    copy_all(&verified.tree(), &kept);
    area.remove(&digest).expect("remove the genuine stage");
    let temp = area.begin().expect("begin a stage");
    copy_all(&kept.join("."), &temp.tree());
    lillux::set_tree_directory_modes(&temp.tree()).expect("directory modes");
    let caps = ryeos_node::bundle_staging::StagingCaps::from_policy(
        &ryeos_node_runtime::node_policy::sections::bundle_staging::BundleStagingPolicy {
            schema: ryeos_node_runtime::node_policy::sections::bundle_staging::CURRENT_SCHEMA,
            max_node_bytes: 2 * 1024 * 1024 * 1024,
            max_node_stages: 64,
            max_key_bytes: 512 * 1024 * 1024,
            max_key_stages: 16,
        },
    );
    area.commit(temp, record, &evidence, &caps, node.signing_key())
        .expect("commit the altered stage");
    drop(area);

    let args = arguments(&b, ITEMS_BUNDLE, &digest);
    let result = apply(&mut b, &args).await;
    assert_eq!(result["outcome"], "refused", "{result:#}");
    assert_eq!(result["code"], "lane_evidence_invalid", "{result:#}");
    assert!(!b.state_path.join(".ai/bundles").join(ITEMS_BUNDLE).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_laned_bundle_whose_tree_changed_after_apply_is_skipped_and_the_node_still_boots() {
    let (a, a_fixture) = start_publisher_with_items_bundle().await;
    let (mut b, _b_fixture) = start_puller().await;
    let publication = publish_bundle(&a, &a_fixture, ITEMS_BUNDLE, 0x75).await;
    let (digest, _) = pull(&b, &publication).await;
    let args = arguments(&b, ITEMS_BUNDLE, &digest);
    let result = apply(&mut b, &args).await;
    assert_eq!(result["outcome"], "applied", "{result:#}");
    let registration_before = registration_text(&b, ITEMS_BUNDLE);

    // Something writes a native executable directory into the installed tree.
    let installed_tree = b.state_path.join(".ai/bundles").join(ITEMS_BUNDLE);
    std::fs::create_dir_all(installed_tree.join("bin")).expect("create bin");
    std::fs::write(installed_tree.join("bin/tool"), b"x").expect("write bin/tool");

    b.restart()
        .await
        .expect("the node boots with a skipped laned bundle");

    // The bundle is not loaded and the rest are; its registration is still
    // there, unchanged, and the sealed registration set did not stop the boot.
    let listed = listing(&b).await;
    assert!(!is_installed(&listed, ITEMS_BUNDLE), "{listed:#}");
    assert!(is_installed(&listed, "core"), "{listed:#}");
    assert_eq!(registration_text(&b, ITEMS_BUNDLE), registration_before);

    // The loader reports why, and the guards still see it as registered and
    // laned: it cannot be taken over by a native install of the same name.
    let trust = ryeos_engine::trust::TrustStore::load_node_registry(&b.state_path)
        .expect("load the node trust registry");
    let loader = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
        app_root: &b.state_path,
        trust_store: &trust,
    };
    let (_, skipped) = loader
        .load_bundle_section_reporting_skipped()
        .expect("load the bundle section");
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(skipped[0].name, ITEMS_BUNDLE);
    assert_eq!(skipped[0].code, "publication_lane_never_admitted");
    let registered = loader.load_registrations().expect("the registrations");
    assert!(
        registered
            .iter()
            .any(|record| record.name == ITEMS_BUNDLE && record.lane.is_some())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skipped_laned_bundle_is_removed_through_bundle_remove_and_the_node_boots_again() {
    let (a, a_fixture) = start_publisher_with_items_bundle().await;
    let (mut b, _b_fixture) = start_puller().await;
    let publication = publish_bundle(&a, &a_fixture, ITEMS_BUNDLE, 0x76).await;
    let (digest, _) = pull(&b, &publication).await;
    let args = arguments(&b, ITEMS_BUNDLE, &digest);
    let result = apply(&mut b, &args).await;
    assert_eq!(result["outcome"], "applied", "{result:#}");

    // The installed tree gains a native executable directory, and the node
    // restarts: the bundle is registered, laned, and not loaded.
    let installed_tree = b.state_path.join(".ai/bundles").join(ITEMS_BUNDLE);
    std::fs::create_dir_all(installed_tree.join("bin")).expect("create bin");
    std::fs::write(installed_tree.join("bin/tool"), b"x").expect("write bin/tool");
    b.restart().await.expect("the node boots with it skipped");
    assert!(!is_installed(&listing(&b).await, ITEMS_BUNDLE));
    assert!(registration_path(&b, ITEMS_BUNDLE).is_file());

    // The only way out is the stopped-node removal, which names the bundle,
    // passes prospective admission without it, and reseals the registrations.
    b.stop_gracefully().await.expect("stop the node");
    let removed = run_stopped_service(
        &b,
        "service:bundle/remove",
        &format!("{{\"name\":\"{ITEMS_BUNDLE}\"}}"),
    );
    assert!(
        removed.status.success(),
        "bundle.remove failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&removed.stdout),
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!installed_tree.exists(), "the tree should be gone");
    assert!(
        !registration_path(&b, ITEMS_BUNDLE).exists(),
        "the registration should be gone"
    );

    // The registrations were resealed, so the node boots, with the rest loaded.
    b.respawn_with(|_| {}).await.expect("the node boots again");
    let listed = listing(&b).await;
    assert!(!is_installed(&listed, ITEMS_BUNDLE), "{listed:#}");
    assert!(is_installed(&listed, "core"), "{listed:#}");
    let trust = ryeos_engine::trust::TrustStore::load_node_registry(&b.state_path)
        .expect("load the node trust registry");
    let (_, skipped) = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
        app_root: &b.state_path,
        trust_store: &trust,
    }
    .load_bundle_section_reporting_skipped()
    .expect("load the bundle section");
    assert!(skipped.is_empty(), "{skipped:?}");
}
