//! End to end: an operator on one node pulls a bundle another node published.
//!
//! Node A installs the workflow bundles, freezes one and publishes it. Node B
//! is a separate daemon with its own node and operator keys, no remote
//! configured and no copy of the bundle. B's operator runs `bundle/pull` with
//! A's `/published` URL, A's node key fingerprint and the record hash. The
//! pull refuses a wrong node pin, a wrong record hash and a non-https URL
//! without moving a blob, then fetches, verifies and stages the bundle in B's
//! staging area. Nothing is installed or registered, the node still boots, and
//! bundle/list shows the stage as `staged`.

use ryeosd::test_support as common;

use common::DaemonHarness;
use common::fast_fixture::{
    FAST_FIXTURE_TIME, FastFixture, register_config_fixture_bundle, register_workflow_bundles,
};
use lillux::crypto::SigningKey;
use ryeos_engine::trust::PublicationRecord;
use serde_json::{Value, json};

/// The demo: a bundle with no command registration, so a fresh node whose policy
/// does not name it installs it with no operator step.
const BUNDLE: &str = "ryeos-docs";

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

/// A second node with its own node and operator keys and only the
/// Core-knowledge profile: it has never seen the published bundle and has no remote.
async fn start_puller() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_core_knowledge_with_node_and_user_keys(
        SigningKey::from_bytes(&[0x5b; 32]),
        SigningKey::from_bytes(&[0x5c; 32]),
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
    publish_bundle_on(a, fixture, BUNDLE).await
}

async fn publish_bundle_on(a: &DaemonHarness, fixture: &FastFixture, bundle: &str) -> Publication {
    let (status, frozen) = service(
        a,
        "service:publication/freeze",
        json!({ "bundle_name": bundle }),
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
    let (mut b, _b_fixture) = start_puller().await;
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

    // The right pins fetch, verify and stage the bundle. Nothing is installed
    // or registered: a live install cannot change the registrations the signed
    // init completion record pins.
    let journal = b.state_path.join(".ai/state/node-journal");
    let journal_before = dir_listing(&journal);
    let (status, staged) = pull(&b, &publication.url, &node_pin, &publication.record_hash).await;
    // The 200 answer is the pull thread's terminal write succeeding: the
    // pinned completion record was not touched.
    assert_eq!(status, 200, "{staged:#}");
    let result = &staged["result"];
    assert_eq!(result["status"], "staged", "{staged:#}");
    assert_eq!(result["installed"], false, "{staged:#}");
    assert_eq!(
        result["receipt"],
        "fetched and verified; staged; takes effect at the next start by a signed operation",
        "{staged:#}"
    );
    assert!(
        !staged.to_string().contains("installed to"),
        "the answer must not say installed: {staged:#}"
    );
    let digest = result["tree_digest"]
        .as_str()
        .expect("tree digest")
        .to_owned();
    assert!(
        !b.state_path.join(".ai/bundles").join(BUNDLE).exists(),
        "a staged pull put a bundle in the bundle root"
    );
    assert!(
        !b.state_path
            .join(".ai/node/bundles")
            .join(format!("{BUNDLE}.yaml"))
            .exists(),
        "a staged pull registered a bundle"
    );
    assert_eq!(
        dir_listing(&journal),
        journal_before,
        "a staged pull wrote to the offline ledger"
    );
    let stage_dir = b.state_path.join(".ai/state/bundle-staging").join(&digest);
    assert!(stage_dir.join("stage.record").is_file());
    assert!(stage_dir.join("tree").is_dir());
    assert!(stage_dir.join("publication.json").is_file());
    assert!(stage_dir.join("manifest.json").is_file());
    assert!(stage_dir.join("origin-public-key.json").is_file());

    // bundle/list shows it as staged and not installed, with what it declares.
    assert_staged_not_installed(&b, &digest).await;

    // The node still boots, the stage survives, and it is still not installed.
    b.restart().await.expect("the node boots with a stage held");
    assert_staged_not_installed(&b, &digest).await;

    // The same bytes pulled again are the same stage.
    let (status, again) = pull(&b, &publication.url, &node_pin, &publication.record_hash).await;
    assert_eq!(status, 200, "{again:#}");
    assert_eq!(again["result"]["tree_digest"], digest, "{again:#}");
    assert_eq!(again["result"]["already_staged"], true, "{again:#}");
}

fn dir_listing(dir: &std::path::Path) -> Vec<(String, u64)> {
    let mut entries: Vec<(String, u64)> = std::fs::read_dir(dir)
        .map(|read| {
            read.map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.metadata().unwrap().len(),
                )
            })
            .collect()
        })
        .unwrap_or_default();
    entries.sort();
    entries
}

async fn assert_staged_not_installed(b: &DaemonHarness, digest: &str) {
    let (status, listed) = service(b, "service:bundle/list", json!({})).await;
    assert_eq!(status, 200, "{listed:#}");
    assert!(
        !listed["result"]["bundles"]
            .as_array()
            .expect("bundles")
            .iter()
            .any(|entry| entry["name"] == BUNDLE),
        "a staged bundle is listed as installed: {listed:#}"
    );
    let staged = listed["result"]["staged"].as_array().expect("staged");
    let entry = staged
        .iter()
        .find(|entry| entry["name"] == BUNDLE)
        .unwrap_or_else(|| panic!("the stage is not listed: {listed:#}"));
    assert_eq!(entry["status"], "staged", "{listed:#}");
    assert_eq!(entry["tree_digest"], digest, "{listed:#}");
    assert!(entry["declared"]["provides_kinds"].is_array(), "{listed:#}");
    assert!(entry.get("lane").is_none(), "{listed:#}");
}

/// The unpinned publisher: A holds a bundle whose manifest is signed by a key
/// A pins and B does not.
const UNPINNED_BUNDLE: &str = "unpinned-demo";

fn stranger_key() -> SigningKey {
    SigningKey::from_bytes(&[0x77; 32])
}

async fn start_publisher_with_stranger_bundle() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            let stranger = stranger_key();
            common::pin_signer(
                state_path,
                &stranger.verifying_key(),
                ryeos_engine::trust::SignerRole::Publisher,
                ryeos_engine::trust::SignerScope::everywhere(),
            );
            register_config_fixture_bundle(state_path, UNPINNED_BUNDLE, fixture, |_| Ok(()))?;
            // The fixture signs the manifest with the shared fixture publisher;
            // sign it with the stranger instead.
            let manifest = state_path
                .join(".ai/bundles")
                .join(UNPINNED_BUNDLE)
                .join(".ai/manifest.yaml");
            let body =
                lillux::signature::strip_signature_lines(&std::fs::read_to_string(&manifest)?);
            std::fs::write(
                &manifest,
                lillux::signature::sign_content_at(&body, &stranger, "#", None, FAST_FIXTURE_TIME),
            )?;
            Ok(())
        },
        |_| {},
    )
    .await
    .expect("start the publishing node")
}

fn signer_files(h: &DaemonHarness) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(ryeos_engine::trust::signers_dir(&h.state_path))
        .expect("read signer registry")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publication_signed_by_a_publisher_the_puller_does_not_trust_is_refused_and_changes_nothing()
 {
    let (a, a_fixture) = start_publisher_with_stranger_bundle().await;
    let (b, _b_fixture) = start_puller().await;
    let publication = publish_bundle_on(&a, &a_fixture, UNPINNED_BUNDLE).await;
    let node_pin = format!("fp:{}", publication.node_fingerprint);
    let signers_before = signer_files(&b);

    let (status, refused) = pull(&b, &publication.url, &node_pin, &publication.record_hash).await;
    assert_eq!(status, 422, "{refused:#}");
    let text = refused.to_string();
    assert!(
        text.contains("publication_publisher_unpinned"),
        "{refused:#}"
    );
    let stranger_fp = lillux::signature::compute_fingerprint(&stranger_key().verifying_key());
    assert!(text.contains(&stranger_fp), "{refused:#}");
    assert!(
        text.contains("ryeos trust pin --from <PUBLISHER_TRUST.toml> --role publisher"),
        "{refused:#}"
    );

    // Nothing was installed and the pull added no trust.
    assert!(
        !b.state_path
            .join(".ai/bundles")
            .join(UNPINNED_BUNDLE)
            .exists(),
        "a refused pull left a bundle behind"
    );
    assert!(
        !b.state_path
            .join(".ai/node/bundles")
            .join(format!("{UNPINNED_BUNDLE}.yaml"))
            .exists(),
        "a refused pull registered a bundle"
    );
    assert_eq!(
        signer_files(&b),
        signers_before,
        "the pull changed B's trust"
    );
}
