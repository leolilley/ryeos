//! End to end: an operator publishes an installed bundle and a stranger reads
//! it over the `/published` routes.
//!
//! A real daemon runs with the workflow bundles installed. The operator signs a
//! publication record with their own key (the steps `ryeos publish` takes after
//! it has frozen the bundle) and submits it. The routes are then read with no
//! credential at all: describe, list and every blob verify against the node
//! countersignature and the frozen manifest, a hash outside the closure is
//! refused identically whatever it is, and a retired or expired publication
//! answers with its own code. The capability report follows each change.

use std::collections::BTreeSet;

use ryeosd::test_support as common;

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use lillux::crypto::SigningKey;
use ryeos_engine::trust::{
    PublicationManifest, PublicationRecord, ServedPublication, compute_fingerprint,
};
use ryeos_node::capability_report::{CAPABILITY_REPORT_SCHEMA, SignedNodeCapabilityReport};
use serde_json::{Value, json};

const BUNDLE: &str = "knowledge";

async fn service(h: &DaemonHarness, service: &str, params: Value) -> (u16, Value) {
    let (status, body) = h
        .post_execute(service, ".", params)
        .await
        .expect("post /execute");
    (status.as_u16(), body)
}

async fn get(h: &DaemonHarness, path: &str) -> (u16, Vec<u8>) {
    let (status, _, body) = h
        .raw_request(reqwest::Method::GET, path)
        .await
        .expect("GET");
    (status.as_u16(), body)
}

async fn get_json(h: &DaemonHarness, path: &str) -> (u16, Value) {
    let (status, body) = get(h, path).await;
    let value = serde_json::from_slice(&body)
        .unwrap_or_else(|error| panic!("{path} answered non-JSON ({error}): {body:?}"));
    (status, value)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct Frozen {
    manifest_hash: String,
    node_fingerprint: String,
    version: String,
    read_bound: u64,
}

async fn freeze(h: &DaemonHarness) -> Frozen {
    let (status, frozen) = service(
        h,
        "service:publication/freeze",
        json!({ "bundle_name": BUNDLE }),
    )
    .await;
    assert_eq!(status, 200, "{frozen:#}");
    let result = &frozen["result"];
    Frozen {
        manifest_hash: result["manifest_hash"].as_str().expect("hash").to_owned(),
        node_fingerprint: result["node_fingerprint"].as_str().expect("fp").to_owned(),
        version: result["bundle_version"]
            .as_str()
            .expect("version")
            .to_owned(),
        read_bound: result["largest_file_bytes"]
            .as_u64()
            .unwrap()
            .max(result["manifest_bytes"].as_u64().unwrap())
            .max(ryeos_engine::trust::MIN_PUBLICATION_READ_BYTES),
    }
}

fn now_plus(secs: i64) -> u64 {
    lillux::time::unix_secs_now().saturating_add_signed(secs)
}

fn record(frozen: &Frozen, id_seed: u8, not_after_in_secs: u64) -> PublicationRecord {
    record_at(frozen, id_seed, now_plus(not_after_in_secs as i64))
}

fn record_at(frozen: &Frozen, id_seed: u8, not_after: u64) -> PublicationRecord {
    let now = lillux::time::unix_secs_now();
    PublicationRecord::new(
        &hex(&[id_seed; 16]),
        &hex(&rand::random::<[u8; 16]>()),
        &frozen.node_fingerprint,
        &frozen.manifest_hash,
        BUNDLE,
        &frozen.version,
        &lillux::time::iso8601_from_unix_secs(not_after),
        &lillux::time::iso8601_from_unix_secs(now),
        frozen.read_bound,
    )
}

async fn publish(h: &DaemonHarness, record: &PublicationRecord, key: &SigningKey) -> (u16, Value) {
    let signature = record.sign_operator(key).expect("sign");
    service(
        h,
        "service:publication/publish",
        json!({ "record": record, "signature": signature }),
    )
    .await
}

async fn start() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with(
        |state_path, _, fixture| register_workflow_bundles(state_path, fixture),
        |_| {},
    )
    .await
    .expect("start daemon with the workflow bundles")
}

fn report(h: &DaemonHarness) -> Value {
    let signed = SignedNodeCapabilityReport::read_verified(&h.state_path)
        .expect("the report verifies")
        .expect("a report is recorded");
    assert_eq!(signed.report.schema, CAPABILITY_REPORT_SCHEMA);
    serde_json::to_value(&signed.report.publications).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stranger_reads_a_published_bundle_and_only_that_bundle() {
    let (h, fixture) = start().await;
    let operator_fp = compute_fingerprint(&fixture.user.verifying_key());

    // Nothing is published yet, and the report says so.
    let (status, listed) = get_json(&h, "/published").await;
    assert_eq!(status, 200, "{listed:#}");
    assert_eq!(listed["publications"], json!([]));
    assert_eq!(report(&h)["live"], json!([]));

    let frozen = freeze(&h).await;
    let record = record(&frozen, 0x11, 3600);
    let id = record.id.clone();

    // Only the operator's signature publishes: another key's is refused.
    let (status, refused) = publish(&h, &record, &fixture.authoring).await;
    assert_eq!(status, 403, "{refused:#}");
    assert!(
        refused.to_string().contains("publication_bad_signature"),
        "{refused:#}"
    );
    // A past not_after and one beyond the node's maximum are refused.
    let (status, refused) =
        publish(&h, &record_at(&frozen, 0x12, now_plus(-10)), &fixture.user).await;
    assert_eq!(status, 400, "{refused:#}");
    let (status, refused) = publish(
        &h,
        &record_at(&frozen, 0x13, now_plus(91 * 86_400)),
        &fixture.user,
    )
    .await;
    assert_eq!(status, 400, "{refused:#}");
    assert!(
        refused
            .to_string()
            .contains("publication_not_after_too_far"),
        "{refused:#}"
    );
    // A record naming a manifest this node never froze is refused.
    let mut unfrozen = record.clone();
    unfrozen.root.manifest_hash = "d".repeat(64);
    let (status, _) = publish(&h, &unfrozen, &fixture.user).await;
    assert_ne!(status, 200);

    let (status, published) = publish(&h, &record, &fixture.user).await;
    assert_eq!(status, 200, "{published:#}");
    let record_hash = published["result"]["record_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(record_hash, record.body_hash().unwrap());
    assert_eq!(
        published["result"]["report"]["refreshed"], true,
        "{published:#}"
    );

    // The list names it, without any operator identity.
    let (status, listed) = get_json(&h, "/published").await;
    assert_eq!(status, 200, "{listed:#}");
    assert_eq!(listed["publications"][0]["id"], json!(id));
    assert!(!listed.to_string().contains(&operator_fp));

    // Describe: the body, its hash and the node countersignature verify under
    // the node key, and the operator's signature and fingerprint are absent.
    let (status, body) = get(&h, &format!("/published/{id}")).await;
    assert_eq!(status, 200);
    let text = String::from_utf8(body.clone()).unwrap();
    assert!(!text.contains(&operator_fp), "{text}");
    let described: Value = serde_json::from_slice(&body).unwrap();
    let manifest: PublicationManifest =
        serde_json::from_value(described["manifest"].clone()).expect("the manifest object");
    let mut served = described.clone();
    served.as_object_mut().unwrap().remove("manifest");
    let served: ServedPublication = serde_json::from_value(served).expect("served form");
    served
        .verify(&fixture.node.verifying_key())
        .expect("the node countersignature verifies under the node key");
    assert_eq!(served.record_hash, record_hash);
    assert!(
        served.verify(&fixture.user.verifying_key()).is_err(),
        "only the node key verifies the countersignature"
    );

    // Every blob of the closure is served and hashes to its name.
    let (status, manifest_blob) = get(
        &h,
        &format!("/published/{id}/blob/{}", frozen.manifest_hash),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(lillux::sha256_hex(&manifest_blob), frozen.manifest_hash);
    let mut seen = BTreeSet::new();
    for entry in &manifest.entries {
        if !seen.insert(entry.hash.clone()) {
            continue;
        }
        let (status, bytes) = get(&h, &format!("/published/{id}/blob/{}", entry.hash)).await;
        assert_eq!(status, 200, "{}", entry.path);
        assert_eq!(lillux::sha256_hex(&bytes), entry.hash, "{}", entry.path);
        assert_eq!(bytes.len() as u64, entry.size);
    }

    // A hash outside the closure is refused with one answer, whatever it is,
    // including the record's own body hash and hashes the store may hold.
    let outside: Vec<String> = vec![
        "e".repeat(64),
        lillux::sha256_hex(b"not in the closure"),
        record_hash.clone(),
        frozen.node_fingerprint.clone(),
        "not-a-hash".to_owned(),
        "A".repeat(64),
    ];
    let mut answers = BTreeSet::new();
    for hash in &outside {
        let (status, body) = get(&h, &format!("/published/{id}/blob/{hash}")).await;
        assert_eq!(status, 404, "{hash}");
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], "object_not_in_scope", "{hash}");
        answers.insert(body);
    }
    assert_eq!(answers.len(), 1, "out-of-scope answers differ: {answers:?}");

    // Unknown ids, other methods and other paths.
    let (status, value) = get_json(&h, &format!("/published/{}", "f".repeat(32))).await;
    assert_eq!(
        (status, &value["error"]["code"]),
        (404, &json!("publication_not_found"))
    );
    let (status, _, _) = h
        .raw_request(reqwest::Method::POST, &format!("/published/{id}"))
        .await
        .unwrap();
    assert_eq!(status.as_u16(), 405);
    let (status, _) = get(&h, &format!("/published/{id}/blob")).await;
    assert_eq!(status, 404);

    // The report lists it live.
    let live = report(&h)["live"].clone();
    assert_eq!(live[0]["id"], json!(id), "{live:#}");
    assert_eq!(live[0]["manifest_hash"], json!(frozen.manifest_hash));
    let facts = report(&h);
    assert_eq!(facts["per_address_limits"], false);
    assert!(facts["daily_byte_cap"].as_u64().unwrap() > 0);
    assert!(
        facts["node_key_rotation_note"]
            .as_str()
            .unwrap()
            .contains("existing publication links stop verifying; re-share them")
    );

    // Retire: the id answers publication_retired with a category, the list
    // drops it, and the report lists it as retired.
    let (status, listed) = service(&h, "service:publication/list", json!({})).await;
    assert_eq!(status, 200, "{listed:#}");
    assert_eq!(listed["result"]["publications"][0]["status"], "live");
    let (status, retired) = service(&h, "service:publication/retire", json!({ "id": id })).await;
    assert_eq!(status, 200, "{retired:#}");
    assert_eq!(retired["result"]["newly_retired"], true);
    let (status, gone) = get_json(&h, &format!("/published/{id}")).await;
    assert_eq!(status, 410, "{gone:#}");
    assert_eq!(gone["error"]["code"], "publication_retired");
    assert_eq!(gone["error"]["reason"], "operator");
    assert!(gone["error"]["retired_at"].is_string());
    let (status, gone) = get_json(
        &h,
        &format!("/published/{id}/blob/{}", frozen.manifest_hash),
    )
    .await;
    assert_eq!(
        (status, &gone["error"]["code"]),
        (410, &json!("publication_retired"))
    );
    let (_, listed) = get_json(&h, "/published").await;
    assert_eq!(listed["publications"], json!([]));
    let facts = report(&h);
    assert_eq!(facts["live"], json!([]));
    assert_eq!(facts["retired"][0]["id"], json!(id));
    assert_eq!(facts["retired"][0]["reason"], "operator");
    // The same id cannot be published again.
    let (status, _) = publish(&h, &record, &fixture.user).await;
    assert_eq!(status, 409);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publication_stops_being_served_after_not_after() {
    let (h, fixture) = start().await;
    let frozen = freeze(&h).await;
    let record = record(&frozen, 0x21, 3);
    let (status, published) = publish(&h, &record, &fixture.user).await;
    assert_eq!(status, 200, "{published:#}");
    let (status, _) = get(&h, &format!("/published/{}", record.id)).await;
    assert_eq!(status, 200);
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    let (status, gone) = get_json(&h, &format!("/published/{}", record.id)).await;
    assert_eq!(status, 410, "{gone:#}");
    assert_eq!(gone["error"]["code"], "publication_expired");
    let (_, listed) = get_json(&h, "/published").await;
    assert_eq!(listed["publications"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_version_is_the_frozen_manifests_and_a_mismatch_is_refused() {
    let (h, fixture) = start().await;
    let frozen = freeze(&h).await;
    let mut wrong = record(&frozen, 0x22, 3600);
    wrong.version = format!("{}-other", frozen.version);
    let (status, refused) = publish(&h, &wrong, &fixture.user).await;
    assert_eq!(status, 400, "{refused:#}");
    assert_eq!(
        refused["error"]["code"], "publication_malformed",
        "{refused:#}"
    );
    let right = record(&frozen, 0x23, 3600);
    let (status, published) = publish(&h, &right, &fixture.user).await;
    assert_eq!(status, 200, "{published:#}");
    assert_eq!(published["result"]["version"], frozen.version.as_str());
    let (_, listed) = get_json(&h, "/published").await;
    assert_eq!(
        listed["publications"][0]["version"],
        frozen.version.as_str()
    );
    let (_, described) = get_json(&h, &format!("/published/{}", right.id)).await;
    assert_eq!(described["record"]["version"], frozen.version.as_str());
    assert_eq!(
        described["manifest"]["bundle_version"],
        frozen.version.as_str()
    );
}

async fn gc(h: &DaemonHarness) -> Value {
    let (status, body) = service(
        h,
        "service:maintenance/gc",
        json!({ "dry_run": false, "compact": false }),
    )
    .await;
    assert_eq!(status, 200, "{body:#}");
    body["result"].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gc_pass_keeps_a_live_publication_and_reclaims_it_after_retire() {
    let (h, fixture) = start().await;
    let frozen = freeze(&h).await;
    let record = record(&frozen, 0x31, 3600);
    let id = record.id.clone();
    let (status, published) = publish(&h, &record, &fixture.user).await;
    assert_eq!(status, 200, "{published:#}");

    // The frozen blobs are referenced by nothing else: a sweep that does not
    // pin the publication would take them.
    let swept = gc(&h).await;
    let (status, body) = get(&h, &format!("/published/{id}")).await;
    assert_eq!(status, 200, "after GC {swept:#}");
    let described: Value = serde_json::from_slice(&body).unwrap();
    let manifest: PublicationManifest =
        serde_json::from_value(described["manifest"].clone()).unwrap();
    let (status, bytes) = get(
        &h,
        &format!("/published/{id}/blob/{}", frozen.manifest_hash),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(lillux::sha256_hex(&bytes), frozen.manifest_hash);
    for entry in &manifest.entries {
        let (status, bytes) = get(&h, &format!("/published/{id}/blob/{}", entry.hash)).await;
        assert_eq!(status, 200, "{} after GC {swept:#}", entry.path);
        assert_eq!(lillux::sha256_hex(&bytes), entry.hash);
    }

    // Retire, then put the record file back byte for byte: the tombstone
    // still wins.
    let record_file = ryeos_engine::roots::RuntimeRoot::new(h.state_path.clone())
        .state()
        .join("publications")
        .join(format!("{id}.json"));
    let kept = std::fs::read(&record_file).expect("the stored record");
    let (status, retired) = service(&h, "service:publication/retire", json!({ "id": id })).await;
    assert_eq!(status, 200, "{retired:#}");
    std::fs::remove_file(&record_file).unwrap();
    std::fs::write(&record_file, &kept).unwrap();
    let (status, gone) = get_json(&h, &format!("/published/{id}")).await;
    assert_eq!(status, 410, "{gone:#}");
    assert_eq!(gone["error"]["code"], "publication_retired");

    // With the publication retired, a sweep may reclaim the closure.
    let swept = gc(&h).await;
    assert!(
        swept["deleted_objects"].as_u64().unwrap() >= 1,
        "the retired publication's manifest is no longer pinned: {swept:#}"
    );
}

/// Rewrite the node-signed publication policy so the day's cap is `bytes`,
/// before the init completion seals the generation.
fn set_daily_cap(
    state_path: &std::path::Path,
    node: &SigningKey,
    bytes: u64,
) -> anyhow::Result<()> {
    let path = state_path.join(".ai/node/policies/publication.yaml");
    let raw = std::fs::read_to_string(&path)?;
    let body = lillux::signature::strip_signature_lines(&raw);
    let mut policy: serde_yaml::Value = serde_yaml::from_str(&body)?;
    policy["daily_bytes"] = bytes.into();
    let signed = lillux::signature::sign_content_at(
        &serde_yaml::to_string(&policy)?,
        node,
        "#",
        None,
        common::fast_fixture::FAST_FIXTURE_TIME,
    );
    std::fs::write(&path, signed)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daily_cap_refuses_reads_once_the_day_is_spent() {
    const CAP: u64 = 256 * 1024;
    let (h, fixture) = DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            register_workflow_bundles(state_path, fixture)?;
            set_daily_cap(state_path, &fixture.node, CAP)
        },
        |_| {},
    )
    .await
    .expect("start daemon with a small daily cap");
    let frozen = freeze(&h).await;
    let record = record(&frozen, 0x41, 3600);
    let id = record.id.clone();
    let (status, published) = publish(&h, &record, &fixture.user).await;
    assert_eq!(status, 200, "{published:#}");
    assert_eq!(report(&h)["daily_byte_cap"], json!(CAP));

    // Each read of the manifest reserves its size against the cap; the day
    // runs out within a bounded number of reads.
    let path = format!("/published/{id}/blob/{}", frozen.manifest_hash);
    let mut served = 0_u32;
    let refused = loop {
        let (status, headers, body) = h
            .raw_request(reqwest::Method::GET, &path)
            .await
            .expect("GET");
        if status.as_u16() == 200 {
            served += 1;
            assert!(served < 100_000, "the cap never ran out");
            continue;
        }
        break (status.as_u16(), headers, body);
    };
    assert!(served >= 1, "the first read fits under the cap");
    let (status, headers, body) = refused;
    assert_eq!(status, 429);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["code"], "publication_quota_exhausted");
    assert!(headers.contains_key(reqwest::header::RETRY_AFTER));
    // An out-of-scope hash still gets its one answer, and a spent day does
    // not turn it into a quota answer.
    let (status, value) = get_json(&h, &format!("/published/{id}/blob/{}", "e".repeat(64))).await;
    assert_eq!(
        (status, &value["error"]["code"]),
        (404, &json!("object_not_in_scope"))
    );
}
