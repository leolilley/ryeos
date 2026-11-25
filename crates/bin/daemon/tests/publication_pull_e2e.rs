//! End to end: an operator on one node pulls a bundle another node published.
//!
//! Node A installs the workflow bundles, freezes one and publishes it. Node B
//! is a separate daemon with its own node and operator keys, no remote
//! configured and no copy of the bundle. B's operator runs `bundle/pull` with
//! A's `/published` URL, A's node key fingerprint and the record hash. The
//! pull refuses a wrong node pin, a wrong record hash and a non-https URL
//! without moving a blob, then installs the bundle through B's own admission:
//! every installed file hashes to its manifest entry.

use ryeosd::test_support as common;

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use lillux::crypto::SigningKey;
use ryeos_engine::trust::{PublicationManifest, PublicationRecord};
use serde_json::{Value, json};

const BUNDLE: &str = "knowledge";

async fn service(h: &DaemonHarness, service: &str, params: Value) -> (u16, Value) {
    let (status, body) = h
        .post_execute(service, ".", params)
        .await
        .expect("post /execute");
    (status.as_u16(), body)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn start_publisher() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with(
        |state_path, _, fixture| register_workflow_bundles(state_path, fixture),
        |_| {},
    )
    .await
    .expect("start the publishing node")
}

/// A second node with its own node and operator keys and only the core
/// bundle: it has never seen the published bundle and has no remote.
async fn start_puller() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with_node_and_user_keys(
        SigningKey::from_bytes(&[0x5b; 32]),
        SigningKey::from_bytes(&[0x5c; 32]),
        |_, _, _| Ok(()),
        |_| {},
    )
    .await
    .expect("start the pulling node")
}

struct Publication {
    id: String,
    record_hash: String,
    node_fingerprint: String,
    url: String,
}

async fn publish_on(a: &DaemonHarness, fixture: &FastFixture) -> Publication {
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
    let record_hash = published["result"]["record_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(record_hash, record.body_hash().unwrap());
    Publication {
        url: format!("http://{}/published/{}", a.bind, record.id),
        id: record.id,
        record_hash,
        node_fingerprint,
    }
}

async fn pull(b: &DaemonHarness, url: &str, node: &str, expect: &str) -> (u16, Value) {
    service(
        b,
        "service:bundle/pull",
        json!({ "url": url, "node": node, "expect": expect }),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_node_pulls_a_published_bundle_with_the_pins_and_only_with_the_pins() {
    let (a, a_fixture) = start_publisher().await;
    let (b, _b_fixture) = start_puller().await;
    let publication = publish_on(&a, &a_fixture).await;
    let node_pin = format!("fp:{}", publication.node_fingerprint);

    // Wrong pins and a non-https URL are refused before any blob moves.
    let (status, refused) = pull(
        &b,
        &publication.url,
        &format!("fp:{}", "d".repeat(64)),
        &publication.record_hash,
    )
    .await;
    assert_ne!(status, 200, "{refused:#}");
    assert!(
        refused.to_string().contains("publication_node_mismatch"),
        "{refused:#}"
    );
    let (status, refused) = pull(&b, &publication.url, &node_pin, &"0".repeat(64)).await;
    assert_ne!(status, 200, "{refused:#}");
    assert!(
        refused.to_string().contains("publication_hash_mismatch"),
        "{refused:#}"
    );
    let (status, refused) = pull(
        &b,
        &format!("http://example.com/published/{}", publication.id),
        &node_pin,
        &publication.record_hash,
    )
    .await;
    assert_ne!(status, 200, "{refused:#}");
    assert!(
        refused.to_string().contains("publication_insecure_url"),
        "{refused:#}"
    );
    // Nothing was installed by the refusals.
    assert!(
        !b.state_path.join(".ai/bundles").join(BUNDLE).exists(),
        "a refused pull left a bundle behind"
    );

    // The pins together install the bundle.
    let (status, pulled) = pull(&b, &publication.url, &node_pin, &publication.record_hash).await;
    assert_eq!(status, 200, "{pulled:#}");
    let result = &pulled["result"];
    assert_eq!(result["bundle_name"], BUNDLE, "{pulled:#}");
    assert_eq!(result["publication_id"], json!(publication.id));
    assert_eq!(result["journal"]["recorded"], true, "{pulled:#}");

    // Every file B installed hashes to the entry A's frozen manifest names.
    let described: Value = ryeos_http_egress::client_builder()
        .build()
        .expect("HTTP client")
        .get(&publication.url)
        .send()
        .await
        .expect("describe")
        .json()
        .await
        .expect("describe JSON");
    let manifest: PublicationManifest =
        serde_json::from_value(described["manifest"].clone()).expect("the manifest");
    let installed = std::path::PathBuf::from(result["path"].as_str().expect("installed path"));
    assert_eq!(result["files_installed"], json!(manifest.entries.len()));
    for entry in &manifest.entries {
        let bytes = std::fs::read(installed.join(&entry.path))
            .unwrap_or_else(|error| panic!("{}: {error}", entry.path));
        assert_eq!(lillux::sha256_hex(&bytes), entry.hash, "{}", entry.path);
        assert_eq!(bytes.len() as u64, entry.size, "{}", entry.path);
    }

    // The bundle is now installed, so a second pull moves nothing.
    let (status, again) = pull(&b, &publication.url, &node_pin, &publication.record_hash).await;
    assert_ne!(status, 200, "{again:#}");
}
