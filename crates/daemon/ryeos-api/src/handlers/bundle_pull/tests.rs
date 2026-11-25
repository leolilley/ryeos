use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use lillux::crypto::{Signer as _, SigningKey};
use ryeos_engine::trust::{
    ManifestEntry, PUBLICATION_MANIFEST_KIND, PUBLICATION_MANIFEST_SCHEMA, PublicationManifest,
    PublicationRecord, ServedPublication, operator_signature_digest,
};

use super::*;
use crate::handlers::remote_bundle_install::fetch_and_materialize_files;

const NOW: u64 = 1_800_000_000;
const ID: &str = "11111111111111111111111111111111";
const BODY: &[u8] = b"hello, bundle\n";

fn node_key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

fn node_fp() -> String {
    compute_fingerprint(&node_key().verifying_key())
}

fn manifest_for(bundle: &str, body: &[u8]) -> PublicationManifest {
    PublicationManifest {
        kind: PUBLICATION_MANIFEST_KIND.to_owned(),
        schema: PUBLICATION_MANIFEST_SCHEMA.to_owned(),
        bundle_name: bundle.to_owned(),
        bundle_manifest_digest: "b".repeat(64),
        publisher_fingerprint: "c".repeat(64),
        file_count: 1,
        total_bytes: body.len() as u64,
        entries: vec![ManifestEntry {
            path: "file.txt".to_owned(),
            hash: lillux::sha256_hex(body),
            size: body.len() as u64,
            mode: 0o644,
        }],
    }
}

fn manifest_hash(manifest: &PublicationManifest) -> String {
    let canonical = lillux::canonical_json(&serde_json::to_value(manifest).unwrap()).unwrap();
    lillux::sha256_hex(canonical.as_bytes())
}

struct Published {
    body: Value,
    pins: Pins,
}

fn countersign(key: &SigningKey, record: &PublicationRecord, operator_signature: &str) -> String {
    let text = format!(
        "ryeos-publication-node-v1\n{}\n{}\n{}",
        record.node_fingerprint,
        record.body_hash().unwrap(),
        operator_signature_digest(operator_signature)
    );
    base64::engine::general_purpose::STANDARD.encode(key.sign(text.as_bytes()).to_bytes())
}

fn published_with(
    record_bundle: &str,
    manifest: &PublicationManifest,
    not_after: u64,
    countersigner: &SigningKey,
) -> Published {
    let record = PublicationRecord::new(
        ID,
        "nonce",
        &node_fp(),
        &manifest_hash(manifest),
        record_bundle,
        &lillux::time::iso8601_from_unix_secs(not_after),
        &lillux::time::iso8601_from_unix_secs(NOW),
        1 << 20,
    );
    let operator_signature = "operator-signature";
    let served = ServedPublication {
        record_hash: record.body_hash().unwrap(),
        operator_signature_digest: operator_signature_digest(operator_signature),
        node_signature: countersign(countersigner, &record, operator_signature),
        record,
    };
    let mut body = serde_json::to_value(&served).unwrap();
    body["manifest"] = serde_json::to_value(manifest).unwrap();
    Published {
        pins: Pins {
            node: node_fp(),
            expect: served.record_hash,
        },
        body,
    }
}

fn published() -> Published {
    published_with("demo", &manifest_for("demo", BODY), NOW + 3600, &node_key())
}

fn verify(published: &Published, now: u64) -> Result<VerifiedPublication, PullRefusal> {
    verify_publication(
        &published.body,
        &node_key().verifying_key(),
        &published.pins,
        now,
    )
}

#[test]
fn an_honest_publication_verifies() {
    let published = published();
    let verified = verify(&published, NOW).expect("verifies");
    assert_eq!(verified.manifest.bundle_name, "demo");
}

#[test]
fn a_wrong_expect_is_a_hash_mismatch() {
    let mut published = published();
    published.pins.expect = "0".repeat(64);
    assert!(matches!(
        verify(&published, NOW),
        Err(PullRefusal::HashMismatch(_))
    ));
}

#[test]
fn a_tampered_record_is_a_hash_mismatch() {
    let mut published = published();
    published.body["record"]["bundle_name"] = json!("other");
    let refusal = verify(&published, NOW).unwrap_err();
    assert_eq!(refusal.code(), "publication_hash_mismatch");
}

#[test]
fn a_record_for_another_node_is_a_node_mismatch() {
    let mut published = published();
    published.pins.node = "d".repeat(64);
    assert_eq!(
        verify(&published, NOW).unwrap_err().code(),
        "publication_node_mismatch"
    );
}

#[test]
fn a_countersignature_from_another_key_is_invalid() {
    let impostor = SigningKey::from_bytes(&[9; 32]);
    let published = published_with("demo", &manifest_for("demo", BODY), NOW + 3600, &impostor);
    assert_eq!(
        verify(&published, NOW).unwrap_err(),
        PullRefusal::SignatureInvalid
    );
}

#[test]
fn a_publication_past_not_after_is_expired() {
    let published = published();
    assert_eq!(
        verify(&published, NOW + 7200).unwrap_err(),
        PullRefusal::Expired
    );
}

#[test]
fn a_manifest_that_does_not_hash_to_the_record_is_refused() {
    let mut published = published();
    published.body["manifest"]["entries"][0]["hash"] = json!("e".repeat(64));
    assert_eq!(
        verify(&published, NOW).unwrap_err().code(),
        "publication_manifest_mismatch"
    );
}

#[test]
fn a_manifest_for_another_bundle_than_the_record_is_refused() {
    let published = published_with(
        "demo",
        &manifest_for("elsewhere", BODY),
        NOW + 3600,
        &node_key(),
    );
    assert_eq!(
        verify(&published, NOW).unwrap_err().code(),
        "publication_manifest_mismatch"
    );
}

fn key_doc(fingerprint: &str, version: &str) -> PublicKeyDoc {
    PublicKeyDoc {
        fingerprint: fingerprint.to_owned(),
        signing_key: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(node_key().verifying_key().as_bytes())
        ),
        version: version.to_owned(),
    }
}

#[test]
fn the_public_key_must_be_the_pinned_node_on_the_same_series() {
    let fp = node_fp();
    assert!(check_public_key(&key_doc(&fp, "0.6.3"), &fp, "0.6.9").is_ok());
    assert!(check_public_key(&key_doc(&format!("fp:{fp}"), "0.6.3"), &fp, "0.6.9").is_ok());
    assert_eq!(
        check_public_key(&key_doc(&fp, "0.6.3"), &"d".repeat(64), "0.6.3")
            .unwrap_err()
            .code(),
        "publication_node_mismatch"
    );
    // A key that does not hash to the fingerprint it is offered under.
    let other = "d".repeat(64);
    assert_eq!(
        check_public_key(&key_doc(&other, "0.6.3"), &other, "0.6.3")
            .unwrap_err()
            .code(),
        "publication_node_mismatch"
    );
    for remote in ["0.7.0", "1.6.3", "garbage", ""] {
        assert_eq!(
            check_public_key(&key_doc(&fp, remote), &fp, "0.6.3")
                .unwrap_err()
                .code(),
            "publication_version_mismatch",
            "{remote}"
        );
    }
}

#[test]
fn only_https_or_a_loopback_address_is_pulled_from() {
    let path = format!("/published/{ID}");
    for url in [
        format!("http://example.com{path}"),
        format!("http://10.0.0.5{path}"),
        format!("ftp://example.com{path}"),
    ] {
        assert_eq!(
            parse_target(&url).unwrap_err().code(),
            "publication_insecure_url",
            "{url}"
        );
    }
    for url in [
        format!("https://example.com:8443{path}"),
        format!("http://127.0.0.1:9000{path}"),
        format!("http://localhost{path}/"),
        format!("http://[::1]:9000{path}"),
    ] {
        let target = parse_target(&url).unwrap_or_else(|error| panic!("{url}: {error}"));
        assert_eq!(target.id, ID);
    }
    assert_eq!(
        parse_target("https://example.com:8443/published/11111111111111111111111111111111")
            .unwrap()
            .origin,
        "https://example.com:8443"
    );
    for url in [
        format!("https://user:pw@example.com{path}"),
        format!("https://example.com{path}?x=1"),
        "https://example.com/published/short".to_owned(),
        "https://example.com/other".to_owned(),
    ] {
        assert_eq!(
            parse_target(&url).unwrap_err().code(),
            "publication_malformed",
            "{url}"
        );
    }
}

#[test]
fn pins_are_checked_for_shape() {
    let fp = node_fp();
    assert!(Pins::parse(&format!("fp:{fp}"), &"a".repeat(64)).is_ok());
    assert!(Pins::parse("fp:abc", &"a".repeat(64)).is_err());
    assert!(Pins::parse(&fp, "nothex").is_err());
}

#[test]
fn an_untrusted_publisher_is_refused_with_the_pin_command() {
    let trust = TrustStore::from_registry(node_key().verifying_key(), vec![]).unwrap();
    let mut manifest = manifest_for("demo", BODY);
    let refusal = check_publisher_pinned(&trust, &manifest).unwrap_err();
    assert_eq!(refusal.code(), "publication_publisher_unpinned");
    assert!(refusal.to_string().contains("ryeos trust pin"), "{refusal}");
    assert!(refusal.to_string().contains(&"c".repeat(64)), "{refusal}");
    manifest.publisher_fingerprint = node_fp();
    assert!(check_publisher_pinned(&trust, &manifest).is_ok());
}

// ── The blob source seam ────────────────────────────────────────────

/// A blob source that returns fixed bytes for every hash.
struct FakeSource {
    bytes: Vec<u8>,
}

impl BlobSource for FakeSource {
    async fn fetch_batch(&self, hashes: &[String]) -> anyhow::Result<HashMap<String, Vec<u8>>> {
        Ok(hashes
            .iter()
            .map(|hash| (hash.clone(), self.bytes.clone()))
            .collect())
    }

    async fn stream_blob(&self, _hash: &str, _sink: &mut std::fs::File) -> anyhow::Result<u64> {
        unreachable!("the fixture blobs are small")
    }
}

#[tokio::test]
async fn blobs_from_a_source_are_hashed_and_materialised() {
    let response = export_response(&manifest_for("demo", BODY));
    let dir = tempfile::tempdir().unwrap();
    let (files, bytes) = fetch_and_materialize_files(
        &FakeSource {
            bytes: BODY.to_vec(),
        },
        &response.entries,
        dir.path(),
    )
    .await
    .expect("materialises");
    assert_eq!((files, bytes), (1, BODY.len() as u64));
    assert_eq!(std::fs::read(dir.path().join("file.txt")).unwrap(), BODY);
}

#[tokio::test]
async fn a_tampered_blob_is_a_blob_mismatch() {
    let response = export_response(&manifest_for("demo", BODY));
    let dir = tempfile::tempdir().unwrap();
    let error = fetch_and_materialize_files(
        &FakeSource {
            bytes: b"tampered, bundle\n".to_vec(),
        },
        &response.entries,
        dir.path(),
    )
    .await
    .unwrap_err();
    let refused = classify(error);
    assert!(
        matches!(&refused, HandlerError::Structured { code, .. } if code == "publication_blob_mismatch"),
        "{refused:?}"
    );
}

// ── The public fetch ────────────────────────────────────────────────

/// Serve one canned response on loopback and record the request head.
fn serve_once(response: &'static str) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(String::new()));
    let recorded = seen.clone();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            head.extend_from_slice(&buffer[..read]);
        }
        *recorded.lock().unwrap() = String::from_utf8_lossy(&head).into_owned();
        stream.write_all(response.as_bytes()).unwrap();
    });
    (url, seen)
}

#[tokio::test]
async fn the_public_fetch_sends_no_identity() {
    let (url, seen) =
        serve_once("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let body = PublicHttp::new()
        .unwrap()
        .get_bounded(&format!("{url}/public-key"), 1024)
        .await
        .unwrap();
    assert_eq!(body.as_deref(), Some(&b"ok"[..]));
    let head = seen.lock().unwrap().clone();
    let names: Vec<String> = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split(':').next())
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    assert!(!names.is_empty(), "{head}");
    for name in &names {
        assert!(
            [
                "host",
                "accept",
                "accept-encoding",
                "user-agent",
                "connection"
            ]
            .contains(&name.as_str()),
            "unexpected request header `{name}` in:\n{head}"
        );
    }
    assert!(
        !head.to_ascii_lowercase().contains("cookie")
            && !head.to_ascii_lowercase().contains("authorization")
            && !head.to_ascii_lowercase().contains("x-ryeos"),
        "{head}"
    );
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let (url, seen) = serve_once(
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let refusal = PublicHttp::new()
        .unwrap()
        .get_bounded(&format!("{url}/published/{ID}"), 1024)
        .await
        .unwrap_err();
    assert_eq!(refusal.code(), "publication_fetch_failed");
    assert!(refusal.to_string().contains("302"), "{refusal}");
    assert!(!seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_response_longer_than_its_bound_is_cut_off() {
    let (url, _) =
        serve_once("HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world");
    let body = PublicHttp::new()
        .unwrap()
        .get_bounded(&format!("{url}/x"), 5)
        .await
        .unwrap();
    assert_eq!(body, None);
}

#[tokio::test]
async fn a_refusal_surfaces_the_nodes_typed_code() {
    let (url, _) = serve_once(
        "HTTP/1.1 410 Gone\r\nContent-Type: application/json\r\nContent-Length: 46\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"publication_expired\",\"x\":1}}",
    );
    let refusal = PublicHttp::new()
        .unwrap()
        .get_bounded(&format!("{url}/x"), 1024)
        .await
        .unwrap_err();
    assert!(refusal.to_string().contains("410"), "{refusal}");
    assert!(
        refusal.to_string().contains("publication_expired"),
        "{refusal}"
    );
}
