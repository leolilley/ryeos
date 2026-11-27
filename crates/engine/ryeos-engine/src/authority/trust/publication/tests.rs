use lillux::crypto::SigningKey;

use super::*;
use crate::authority::trust::{SignerRecord, SignerScope, compute_fingerprint};

const NOW: u64 = 1_800_000_000;
const DAY: u64 = 86_400;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn fp(key: &SigningKey) -> String {
    compute_fingerprint(&key.verifying_key())
}

fn signer(key: &SigningKey, role: SignerRole, status: SignerStatus) -> SignerRecord {
    let scope = match role {
        SignerRole::Operator => SignerScope::operator_default(),
        _ => SignerScope::authoring_default(),
    };
    let mut record = SignerRecord::new(&key.verifying_key(), "test", role, scope, status);
    record.event = "e".repeat(64);
    record
}

struct Node {
    root: tempfile::TempDir,
    node: SigningKey,
    operator: SigningKey,
}

impl Node {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            node: key(1),
            operator: key(2),
        }
    }

    fn trust_with(&self, status: SignerStatus) -> TrustStore {
        TrustStore::from_registry(
            self.node.verifying_key(),
            vec![signer(&self.operator, SignerRole::Operator, status)],
        )
        .unwrap()
        .with_succession_chain(&[])
    }

    fn trust(&self) -> TrustStore {
        self.trust_with(SignerStatus::Active)
    }

    fn record(&self, id_digit: char, not_after_secs: u64) -> PublicationRecord {
        PublicationRecord::new(
            &id_digit.to_string().repeat(32),
            "nonce",
            &fp(&self.node),
            &"a".repeat(64),
            "fixture",
            "1.2.3",
            &lillux::time::iso8601_from_unix_secs(not_after_secs),
            &lillux::time::iso8601_from_unix_secs(NOW),
            1 << 20,
        )
    }

    fn publish(&self, record: PublicationRecord) -> Result<StoredPublication, PublicationRefusal> {
        let signature = record.sign_operator(&self.operator).unwrap();
        publish_record(
            self.root.path(),
            &self.node,
            &self.trust(),
            &fp(&self.operator),
            record,
            signature,
            90,
            NOW,
        )
    }

    fn state(&self, id: &str) -> PublicationState {
        load_publication(self.root.path(), &self.node.verifying_key(), id).unwrap()
    }
}

#[test]
fn a_record_signed_by_the_active_operator_is_countersigned_and_served_without_the_operator_signature()
 {
    let node = Node::new();
    let stored = node.publish(node.record('1', NOW + DAY)).unwrap();
    let served = stored.served().unwrap();
    served.verify(&node.node.verifying_key()).unwrap();
    let text = serde_json::to_string(&served).unwrap();
    assert!(!text.contains(&stored.operator_signature), "{text}");
    assert!(!text.contains(&fp(&node.operator)), "{text}");
    // The countersignature binds the operator signature digest.
    let mut swapped = served.clone();
    swapped.operator_signature_digest = operator_signature_digest("another");
    assert_eq!(
        swapped.verify(&node.node.verifying_key()),
        Err(PublicationRefusal::BadSignature)
    );
    // Another node key does not verify it.
    assert!(served.verify(&key(9).verifying_key()).is_err());
}

#[test]
fn a_record_signed_by_a_non_operator_or_with_a_bad_signature_is_refused() {
    let node = Node::new();
    let record = node.record('2', NOW + DAY);
    let other = key(7);
    let signature = record.sign_operator(&other).unwrap();
    let refused = publish_record(
        node.root.path(),
        &node.node,
        &node.trust(),
        &fp(&other),
        record.clone(),
        signature,
        90,
        NOW,
    );
    assert_eq!(refused, Err(PublicationRefusal::SignerNotOperator));
    // The operator's name on another key's signature.
    let forged = record.sign_operator(&other).unwrap();
    let refused = publish_record(
        node.root.path(),
        &node.node,
        &node.trust(),
        &fp(&node.operator),
        record.clone(),
        forged,
        90,
        NOW,
    );
    assert_eq!(refused, Err(PublicationRefusal::BadSignature));
    // A retired operator key is not active.
    let signature = record.sign_operator(&node.operator).unwrap();
    let refused = publish_record(
        node.root.path(),
        &node.node,
        &node.trust_with(SignerStatus::Retired),
        &fp(&node.operator),
        record,
        signature,
        90,
        NOW,
    );
    assert_eq!(refused, Err(PublicationRefusal::SignerNotOperator));
}

#[test]
fn not_after_must_be_future_and_within_the_node_and_code_maximum() {
    let node = Node::new();
    assert_eq!(
        node.publish(node.record('3', NOW - 1)),
        Err(PublicationRefusal::Expired)
    );
    assert_eq!(
        node.publish(node.record('3', NOW + 91 * DAY)),
        Err(PublicationRefusal::NotAfterTooFar { max_days: 90 })
    );
    // Node data above the code maximum is clamped.
    let record = node.record('4', NOW + 181 * DAY);
    let signature = record.sign_operator(&node.operator).unwrap();
    let refused = publish_record(
        node.root.path(),
        &node.node,
        &node.trust(),
        &fp(&node.operator),
        record,
        signature,
        10_000,
        NOW,
    );
    assert_eq!(
        refused,
        Err(PublicationRefusal::NotAfterTooFar {
            max_days: PUBLICATION_HARD_MAX_LIFETIME_DAYS
        })
    );
    assert!(node.publish(node.record('5', NOW + 90 * DAY)).is_ok());
}

#[test]
fn a_publication_expires_at_not_after() {
    let node = Node::new();
    let stored = node.publish(node.record('6', NOW + DAY)).unwrap();
    let state = node.state(&stored.record.id);
    assert!(state.live(NOW + DAY - 1).is_ok());
    assert_eq!(state.live(NOW + DAY), Err(PublicationRefusal::Expired));
}

#[test]
fn the_tombstone_survives_a_put_back_record() {
    let node = Node::new();
    let stored = node.publish(node.record('7', NOW + DAY)).unwrap();
    let id = stored.record.id.clone();
    let (tombstone, written) = retire_publication(
        node.root.path(),
        &node.node,
        &id,
        PublicationReason::Operator,
        NOW + 10,
    )
    .unwrap();
    assert!(written);
    // Retiring twice keeps the first.
    let (again, written) = retire_publication(
        node.root.path(),
        &node.node,
        &id,
        PublicationReason::Expired,
        NOW + 20,
    )
    .unwrap();
    assert!(!written);
    assert_eq!(again, tombstone);

    // Put the record file back byte for byte: the tombstone still wins.
    let path = publications_dir(node.root.path()).join(format!("{id}.json"));
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, bytes).unwrap();
    let state = node.state(&id);
    assert_eq!(
        state.live(NOW + 30),
        Err(PublicationRefusal::Retired {
            retired_at: tombstone.retired_at.clone(),
            reason: PublicationReason::Operator,
        })
    );
    // The same id cannot be published again.
    assert_eq!(
        node.publish(node.record('7', NOW + DAY)),
        Err(PublicationRefusal::AlreadyExists)
    );
    // The ledger agrees.
    let ledger = PublicationLedger::load(node.root.path(), &node.node.verifying_key()).unwrap();
    assert_eq!(ledger.tombstones.len(), 1);
    assert!(ledger.state_of(&id).live(NOW + 30).is_err());
}

#[test]
fn an_edited_or_foreign_signed_record_does_not_load() {
    let node = Node::new();
    let stored = node.publish(node.record('8', NOW + DAY)).unwrap();
    let id = stored.record.id.clone();
    let path = publications_dir(node.root.path()).join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("fixture", "swapped")).unwrap();
    assert!(load_publication(node.root.path(), &node.node.verifying_key(), &id).is_err());
    // Signed by another key.
    assert!(load_publication(node.root.path(), &key(9).verifying_key(), &id).is_err());
}

#[test]
fn the_read_time_check_follows_the_operator_key_status() {
    let node = Node::new();
    let stored = node.publish(node.record('9', NOW + DAY)).unwrap();
    stored.check_operator(&node.trust()).unwrap();
    assert_eq!(
        stored.check_operator(&node.trust_with(SignerStatus::Retired)),
        Err(PublicationRefusal::SignerNotOperator)
    );
    assert_eq!(
        stored.check_operator(&node.trust_with(SignerStatus::Revoked)),
        Err(PublicationRefusal::SignerNotOperator)
    );
}

#[test]
fn an_unknown_or_malformed_id_is_not_found() {
    let node = Node::new();
    assert_eq!(
        node.state(&"f".repeat(32)).live(NOW),
        Err(PublicationRefusal::NotFound)
    );
    assert_eq!(
        node.state("../etc").live(NOW),
        Err(PublicationRefusal::NotFound)
    );
}

fn manifest_bytes(entries: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    let entries: Vec<ManifestEntry> = entries
        .iter()
        .map(|(path, data)| ManifestEntry {
            path: (*path).to_owned(),
            hash: lillux::sha256_hex(data),
            size: data.len() as u64,
            mode: 0o644,
        })
        .collect();
    let manifest = PublicationManifest {
        kind: PUBLICATION_MANIFEST_KIND.to_owned(),
        schema: PUBLICATION_MANIFEST_SCHEMA.to_owned(),
        bundle_name: "fixture".to_owned(),
        bundle_version: "1.2.3".to_owned(),
        bundle_manifest_digest: "b".repeat(64),
        publisher_fingerprint: "c".repeat(64),
        file_count: entries.len() as u64,
        total_bytes: entries.iter().map(|entry| entry.size).sum(),
        entries,
    };
    let canonical = lillux::canonical_json(&serde_json::to_value(&manifest).unwrap()).unwrap();
    (
        lillux::sha256_hex(canonical.as_bytes()),
        canonical.into_bytes(),
    )
}

#[test]
fn the_closure_is_the_manifest_hash_plus_every_entry_hash_and_nothing_else() {
    let (hash, bytes) = manifest_bytes(&[("a.txt", b"alpha"), ("dir/b.txt", b"beta")]);
    let (_, closure) = PublicationManifest::from_object_bytes(&hash, &bytes).unwrap();
    assert_eq!(closure.hash_count(), 3);
    assert!(closure.contains(&hash));
    assert!(closure.is_manifest(&hash));
    assert!(closure.contains(&lillux::sha256_hex(b"alpha")));
    assert_eq!(closure.blob_size(&lillux::sha256_hex(b"beta")), Some(4));
    assert!(!closure.contains(&lillux::sha256_hex(b"gamma")));
    assert!(!closure.contains("not a hash"));
    assert!(!closure.contains(""));
}

#[test]
fn a_record_and_a_manifest_need_an_identifier_version() {
    let node = Node::new();
    let mut record = node.record('1', NOW + 3600);
    record.validate_shape().unwrap();
    for good in ["1.2.3", "1.0.0-rc.1+build.5", "v_2", &"v".repeat(64)] {
        record.version = good.to_owned();
        record
            .validate_shape()
            .unwrap_or_else(|_| panic!("{good:?}"));
    }
    for bad in [
        "",
        "has space",
        "line\nbreak",
        "<b>",
        "a\"b",
        "a'b",
        "a/b",
        "é",
        &"v".repeat(65),
    ] {
        record.version = bad.to_owned();
        assert!(record.validate_shape().is_err(), "{bad:?}");
    }
    let (_, bytes) = manifest_bytes(&[("a.txt", b"alpha")]);
    let mut manifest: PublicationManifest = serde_json::from_slice(&bytes).unwrap();
    manifest.validate_shape().unwrap();
    for bad in ["", "<b>", "a b"] {
        manifest.bundle_version = bad.to_owned();
        assert!(manifest.validate_shape().is_err(), "{bad:?}");
    }
    manifest.bundle_version = "1.0.0-rc.1+build.5".to_owned();
    manifest.validate_shape().unwrap();
}

#[test]
fn a_manifest_whose_bytes_do_not_match_its_hash_or_shape_is_refused() {
    let (hash, bytes) = manifest_bytes(&[("a.txt", b"alpha")]);
    let mut tampered = bytes.clone();
    tampered.push(b' ');
    assert!(PublicationManifest::from_object_bytes(&hash, &tampered).is_err());
    let (hash, bytes) = manifest_bytes(&[("../escape", b"alpha")]);
    assert!(PublicationManifest::from_object_bytes(&hash, &bytes).is_err());
    let (hash, bytes) = manifest_bytes(&[("a", b"x"), ("a", b"y")]);
    assert!(PublicationManifest::from_object_bytes(&hash, &bytes).is_err());
}

#[test]
fn only_live_publications_pin_their_manifest() {
    let node = Node::new();
    let with_manifest = |digit: char, not_after: u64, manifest: char| {
        let mut record = node.record(digit, not_after);
        record.root.manifest_hash = manifest.to_string().repeat(64);
        node.publish(record).unwrap().record.id
    };
    let _live = with_manifest('a', NOW + DAY, '1');
    let retired = with_manifest('b', NOW + DAY, '2');
    let _soon = with_manifest('c', NOW + 10, '3');
    retire_publication(
        node.root.path(),
        &node.node,
        &retired,
        PublicationReason::Operator,
        NOW + 1,
    )
    .unwrap();
    let ledger = PublicationLedger::load(node.root.path(), &node.node.verifying_key()).unwrap();
    assert_eq!(
        ledger.pinned_manifest_hashes(NOW + 5),
        vec!["1".repeat(64), "3".repeat(64)]
    );
    // Once a publication passes not_after, its manifest is no longer pinned.
    assert_eq!(
        ledger.pinned_manifest_hashes(NOW + 10),
        vec!["1".repeat(64)]
    );
}
