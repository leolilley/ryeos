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
use common::fast_fixture::{
    FAST_FIXTURE_TIME, FastFixture, register_config_fixture_bundle, register_workflow_bundles,
};
use lillux::crypto::SigningKey;
use ryeos_engine::trust::{PublicationManifest, PublicationRecord};
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
    publish_bundle_with_id(a, fixture, bundle, 0x41).await
}

/// Publish `bundle` under a record id built from `id_byte`, so one node can
/// hold several publications at once.
async fn publish_bundle_with_id(
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

    // The registration carries the publication-lane marker with the pins the
    // pull used.
    let publisher_fp = manifest
        .publisher_fingerprint
        .strip_prefix("fp:")
        .unwrap_or(&manifest.publisher_fingerprint)
        .to_owned();
    let registration = std::fs::read_to_string(
        b.state_path
            .join(".ai/node/bundles")
            .join(format!("{BUNDLE}.yaml")),
    )
    .expect("read the registration");
    for needle in [
        "lane:",
        "name: publication",
        publisher_fp.as_str(),
        publication.record_hash.as_str(),
        publication.node_fingerprint.as_str(),
        manifest.bundle_version.as_str(),
    ] {
        assert!(registration.contains(needle), "{needle}: {registration}");
    }

    // `bundle.list` shows the lane for this bundle only.
    let (status, listed) = service(&b, "service:bundle/list", json!({})).await;
    assert_eq!(status, 200, "{listed:#}");
    let bundles = listed["result"]["bundles"].as_array().expect("bundles");
    let entry = bundles
        .iter()
        .find(|entry| entry["name"] == BUNDLE)
        .expect("the pulled bundle is listed");
    assert_eq!(entry["lane"]["name"], "publication", "{entry:#}");
    assert_eq!(entry["lane"]["publisher_fingerprint"], json!(publisher_fp));
    assert_eq!(entry["lane"]["record_hash"], json!(publication.record_hash));
    assert_eq!(
        entry["lane"]["node_fingerprint"],
        json!(publication.node_fingerprint)
    );
    assert!(
        bundles
            .iter()
            .filter(|entry| entry["name"] != BUNDLE)
            .all(|entry| entry.get("lane").is_none()),
        "{listed:#}"
    );

    // The result carries the receipt, with the real fingerprints.
    let receipt = result["receipt"].as_str().expect("receipt");
    assert!(
        receipt.starts_with("Installed from a publication, not a release. Publisher fp:"),
        "{receipt}"
    );
    assert!(
        receipt.contains(&format!("Publisher fp:{publisher_fp} signed these bytes")),
        "{receipt}"
    );
    assert!(
        receipt.contains(&format!(
            "node fp:{} served them.",
            publication.node_fingerprint
        )),
        "{receipt}"
    );
    assert!(
        receipt.ends_with("nothing authorizes an exact set."),
        "{receipt}"
    );

    // The bundle is now installed, so a second pull moves nothing.
    let (status, again) = pull(&b, &publication.url, &node_pin, &publication.record_hash).await;
    assert_eq!(status, 409, "{again:#}");
    assert_eq!(
        again["error"]["code"], "bundle_already_installed",
        "{again:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pull_never_takes_a_name_a_native_install_holds() {
    let (a, a_fixture) = start_publisher().await;
    let (b, _b_fixture) = start_puller().await;

    // The puller's `knowledge` bundle is a native install; a publication of the
    // same name does not take it.
    let knowledge = publish_bundle_with_id(&a, &a_fixture, "knowledge", 0x43).await;
    let registration = b.state_path.join(".ai/node/bundles/knowledge.yaml");
    let before = std::fs::read(&registration).expect("native registration");
    let (status, refused) = pull(
        &b,
        &knowledge.url,
        &format!("fp:{}", knowledge.node_fingerprint),
        &knowledge.record_hash,
    )
    .await;
    assert_eq!(status, 409, "{refused:#}");
    assert_eq!(
        refused["error"]["code"], "bundle_name_held_by_native_lane",
        "{refused:#}"
    );
    assert_eq!(
        std::fs::read(&registration).expect("native registration"),
        before,
        "the refused pull touched the native registration"
    );
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

/// Synthetic bundles that each carry one thing a publication-lane bundle may
/// not: (bundle name, file path, file bytes, expected typed code).
const LANE_REFUSALS: &[(&str, &str, &[u8], &str)] = &[
    (
        "lane-service",
        ".ai/services/x/s.yaml",
        b"x",
        "publication_lane_kind_refused",
    ),
    (
        "lane-config",
        ".ai/config/x/c.yaml",
        b"x",
        "publication_lane_kind_refused",
    ),
    (
        "lane-node",
        ".ai/node/x/n.yaml",
        b"x",
        "publication_lane_kind_refused",
    ),
    (
        "lane-route",
        ".ai/routes/x/r.yaml",
        b"x",
        "publication_lane_kind_refused",
    ),
    (
        "lane-bin",
        "bin/tool",
        b"x",
        "bundle_lane_native_executable",
    ),
    (
        "lane-handler",
        ".ai/handlers/x/h.yaml",
        b"x",
        "bundle_lane_node_extension",
    ),
    (
        "lane-stray",
        "README.md",
        b"x",
        "publication_lane_stray_file",
    ),
    (
        "lane-elf",
        ".ai/knowledge/x/note.md",
        b"\x7fELF\x02\x01\x01",
        "publication_lane_executable_image",
    ),
];

async fn start_publisher_with_lane_refusals() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            for (bundle, path, bytes, _) in LANE_REFUSALS {
                register_config_fixture_bundle(state_path, bundle, fixture, |root| {
                    let file = root.join(path);
                    std::fs::create_dir_all(file.parent().expect("a parent directory"))?;
                    std::fs::write(file, bytes)?;
                    Ok(())
                })?;
            }
            Ok(())
        },
        |_| {},
    )
    .await
    .expect("start the publishing node")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_that_carries_more_than_items_is_refused_with_its_typed_code_and_nothing_is_written()
 {
    let (a, a_fixture) = start_publisher_with_lane_refusals().await;
    let (b, _b_fixture) = start_puller().await;
    for (index, (bundle, _, _, code)) in LANE_REFUSALS.iter().enumerate() {
        let publication = publish_bundle_with_id(&a, &a_fixture, bundle, 0x50 + index as u8).await;
        let (status, refused) = pull(
            &b,
            &publication.url,
            &format!("fp:{}", publication.node_fingerprint),
            &publication.record_hash,
        )
        .await;
        assert_eq!(status, 422, "{bundle}: {refused:#}");
        assert_eq!(refused["error"]["code"], *code, "{bundle}: {refused:#}");
        assert!(
            refused
                .to_string()
                .contains("cannot be lifted on this build"),
            "{bundle}: {refused:#}"
        );
        assert!(
            !b.state_path.join(".ai/bundles").join(bundle).exists(),
            "{bundle}: a refused pull left a bundle behind"
        );
        assert!(
            !b.state_path
                .join(".ai/node/bundles")
                .join(format!("{bundle}.yaml"))
                .exists(),
            "{bundle}: a refused pull left a registration behind"
        );
    }
}

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

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_of_a_tool_and_a_knowledge_item_installs_in_the_lane() {
    let (a, a_fixture) = start_publisher_with_items_bundle().await;
    let (b, _b_fixture) = start_puller().await;
    let publication = publish_bundle_with_id(&a, &a_fixture, ITEMS_BUNDLE, 0x60).await;
    let (status, result) = pull(
        &b,
        &publication.url,
        &format!("fp:{}", publication.node_fingerprint),
        &publication.record_hash,
    )
    .await;
    assert_eq!(status, 200, "{result:#}");
    let installed = b.state_path.join(".ai/bundles").join(ITEMS_BUNDLE);
    assert!(installed.join(".ai/tools/rye/web/fetch.yaml").is_file());
    assert!(installed.join(".ai/knowledge/lane/demo/note.md").is_file());
    let registration = std::fs::read_to_string(
        b.state_path
            .join(".ai/node/bundles")
            .join(format!("{ITEMS_BUNDLE}.yaml")),
    )
    .expect("the registration");
    assert!(registration.contains("lane:"), "{registration}");
    assert!(registration.contains("publication"), "{registration}");
}
