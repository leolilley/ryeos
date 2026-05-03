//! Golden tests for canonical JSON serialization and hashing.
//!
//! These tests verify that Rust rye-state produces identical canonical JSON
//! and SHA256 hashes as the Python rye/cas implementation.
//!
//! To generate/update test vectors:
//! 1. Run: cargo test --test golden -- --nocapture --ignored generate_vectors
//! 2. Copy the printed JSON to .tmp/test-vectors/*.json
//! 3. Verify with Python: pytest ryeos/rye/cas/test_golden.py

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

fn test_vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(".tmp/test-vectors")
}

#[test]
fn golden_thread_event_canonical_json() {
    let vectors_path = test_vectors_dir().join("thread_event_vectors.json");
    assert!(
        vectors_path.exists(),
        "Golden test vectors not found at {}. Vectors must be committed to .tmp/test-vectors/",
        vectors_path.display()
    );

    let data = fs::read_to_string(&vectors_path)
        .expect("failed to read thread_event_vectors.json");
    let vectors: Value = serde_json::from_str(&data)
        .expect("failed to parse thread_event_vectors.json");

    let cases = vectors["cases"]
        .as_array()
        .expect("cases must be an array");

    for case in cases {
        let name = case["name"]
            .as_str()
            .expect("case must have a name");
        let object = &case["object"];

        // Serialize to canonical JSON
        let canonical = lillux::canonical_json(object);
        let hash = lillux::sha256_hex(canonical.as_bytes());

        let expected = case["expected_hash"]
            .as_str()
            .expect("case must have expected_hash");
        assert!(
            !expected.is_empty(),
            "expected_hash must not be empty in test vector '{}'. Run generate_vectors.",
            name
        );
        assert_eq!(
            hash, expected,
            "Hash mismatch in '{}': Rust produced {} but vector expects {}",
            name, hash, expected
        );

        println!("TestCase: {}", name);
        println!("  Hash: {}", hash);

        // Verify the object is a valid thread_event shape
        assert_eq!(object["kind"], "thread_event", "Invalid kind in {}", name);
        assert_eq!(object["schema"], 1, "Invalid schema in {}", name);
    }
}

#[test]
fn golden_thread_snapshot_canonical_json() {
    let vectors_path = test_vectors_dir().join("thread_snapshot_vectors.json");
    assert!(
        vectors_path.exists(),
        "Golden test vectors not found at {}. Vectors must be committed to .tmp/test-vectors/",
        vectors_path.display()
    );

    let data = fs::read_to_string(&vectors_path)
        .expect("failed to read thread_snapshot_vectors.json");
    let vectors: Value = serde_json::from_str(&data)
        .expect("failed to parse thread_snapshot_vectors.json");

    let cases = vectors["cases"]
        .as_array()
        .expect("cases must be an array");

    for case in cases {
        let name = case["name"]
            .as_str()
            .expect("case must have a name");
        let object = &case["object"];

        // Serialize to canonical JSON
        let canonical = lillux::canonical_json(object);
        let hash = lillux::sha256_hex(canonical.as_bytes());

        let expected = case["expected_hash"]
            .as_str()
            .expect("case must have expected_hash");
        assert!(
            !expected.is_empty(),
            "expected_hash must not be empty in test vector '{}'. Run generate_vectors.",
            name
        );
        assert_eq!(
            hash, expected,
            "Hash mismatch in '{}': Rust produced {} but vector expects {}",
            name, hash, expected
        );

        println!("TestCase: {}", name);
        println!("  Hash: {}", hash);

        // Verify the object is a valid thread_snapshot shape
        assert_eq!(object["kind"], "thread_snapshot", "Invalid kind in {}", name);
        assert_eq!(object["schema"], 1, "Invalid schema in {}", name);
    }
}

#[test]
fn golden_chain_state_canonical_json() {
    let vectors_path = test_vectors_dir().join("chain_state_vectors.json");
    assert!(
        vectors_path.exists(),
        "Golden test vectors not found at {}. Vectors must be committed to .tmp/test-vectors/",
        vectors_path.display()
    );

    let data = fs::read_to_string(&vectors_path)
        .expect("failed to read chain_state_vectors.json");
    let vectors: Value = serde_json::from_str(&data)
        .expect("failed to parse chain_state_vectors.json");

    let cases = vectors["cases"]
        .as_array()
        .expect("cases must be an array");

    for case in cases {
        let name = case["name"]
            .as_str()
            .expect("case must have a name");
        let object = &case["object"];

        // Serialize to canonical JSON
        let canonical = lillux::canonical_json(object);
        let hash = lillux::sha256_hex(canonical.as_bytes());

        let expected = case["expected_hash"]
            .as_str()
            .expect("case must have expected_hash");
        assert!(
            !expected.is_empty(),
            "expected_hash must not be empty in test vector '{}'. Run generate_vectors.",
            name
        );
        assert_eq!(
            hash, expected,
            "Hash mismatch in '{}': Rust produced {} but vector expects {}",
            name, hash, expected
        );

        println!("TestCase: {}", name);
        println!("  Hash: {}", hash);

        // Verify the object is a valid chain_state shape
        assert_eq!(object["kind"], "chain_state", "Invalid kind in {}", name);
        assert_eq!(object["schema"], 1, "Invalid schema in {}", name);
    }
}

/// Maintenance utility: regenerate test vector hashes from source objects.
///
/// **Not a behavioral test** — this rewrites `*_vectors.json` files in-place.
/// Only meant to run when updating canonical hashes after a format change.
///
/// ```bash
/// RYEOS_REGENERATE_VECTORS=1 cargo test --test golden -- --nocapture generate_vectors
/// ```
#[test]
fn generate_vectors() {
    if std::env::var("RYEOS_REGENERATE_VECTORS").is_err() {
        // No-op unless explicitly opted in — this is a maintenance tool,
        // not a behavioral test. Removing the old #[ignore] because
        // Wave-5 forbids it; env-var guard achieves the same safety.
        return;
    }
    println!("\nGenerating test vector hashes...\n");

    for file in ["thread_event_vectors.json", "thread_snapshot_vectors.json", "chain_state_vectors.json"] {
        let vectors_path = test_vectors_dir().join(file);
        if !vectors_path.exists() {
            eprintln!("Skipping: {} not found", file);
            continue;
        }

        let data = fs::read_to_string(&vectors_path)
            .expect("failed to read vectors file");
        let mut vectors: Value = serde_json::from_str(&data)
            .expect("failed to parse vectors file");

        let cases = vectors["cases"]
            .as_array_mut()
            .expect("cases must be an array");

        for case in cases {
            let object = &case["object"];
            let canonical = lillux::canonical_json(object);
            let hash = lillux::sha256_hex(canonical.as_bytes());

            case["expected_hash"] = json!(hash);
        }

        let updated = serde_json::to_string_pretty(&vectors)
            .expect("failed to serialize vectors");
        fs::write(&vectors_path, updated)
            .expect("failed to write updated vectors");

        println!("Updated: {}", vectors_path.display());
    }
}

#[test]
fn verify_thread_event_hash_stability() {
    // Verify that the same object produces the same hash every time
    let event = json!({
        "schema": 1,
        "kind": "thread_event",
        "chain_root_id": "T-root-001",
        "chain_seq": 1,
        "thread_id": "T-root-001",
        "thread_seq": 1,
        "event_type": "thread_created",
        "durability": "durable",
        "ts": "2026-04-21T12:00:00Z",
        "prev_chain_event_hash": null,
        "prev_thread_event_hash": null,
        "payload": {
            "kind": "agent",
            "item_ref": "directive:init",
            "executor_ref": "native:directive-runtime"
        }
    });

    let hash1 = lillux::sha256_hex(lillux::canonical_json(&event).as_bytes());
    let hash2 = lillux::sha256_hex(lillux::canonical_json(&event).as_bytes());

    assert_eq!(hash1, hash2, "Hash must be stable across calls");
    assert_eq!(hash1.len(), 64, "Hash must be 64 hex characters");
}

#[test]
fn verify_canonical_json_determinism() {
    // Verify that different serialization orders produce the same canonical JSON
    let obj1 = json!({
        "b": 2,
        "a": 1,
        "c": 3
    });

    let obj2 = json!({
        "a": 1,
        "b": 2,
        "c": 3
    });

    let canonical1 = lillux::canonical_json(&obj1);
    let canonical2 = lillux::canonical_json(&obj2);

    assert_eq!(canonical1, canonical2, "Canonical JSON must be deterministic");
}
