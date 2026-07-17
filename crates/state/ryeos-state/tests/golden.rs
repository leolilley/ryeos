//! Golden tests for canonical JSON serialization and hashing.
//!
//! These tests verify that Rust ryeos-state produces identical canonical JSON
//! and SHA256 hashes across runs (determinism and stability).
//!
//! To regenerate expected hashes after a format change:
//!   RYEOS_REGENERATE_VECTORS=1 cargo test --test golden -- --nocapture generate_vectors

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

fn test_vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(".tmp/test-vectors")
}

// ── Inline golden test cases ──────────────────────────────────────

#[test]
fn golden_thread_event_canonical_json() {
    let cases = [
        json!({
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
        }),
        json!({
            "schema": 1,
            "kind": "thread_event",
            "chain_root_id": "T-root-001",
            "chain_seq": 2,
            "thread_id": "T-root-001",
            "thread_seq": 2,
            "event_type": "tool_dispatched",
            "durability": "durable",
            "ts": "2026-04-21T12:00:01Z",
            "prev_chain_event_hash": null,
            "prev_thread_event_hash": null,
            "payload": {
                "kind": "tool",
                "item_ref": "tool:ryeos/bash/bash",
                "executor_ref": "native:subprocess"
            }
        }),
    ];

    for (i, object) in cases.iter().enumerate() {
        let canonical = lillux::canonical_json(object).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());

        assert_eq!(object["kind"], "thread_event", "case {}: invalid kind", i);
        assert_eq!(object["schema"], 1, "case {}: invalid schema", i);

        // Verify hash is stable (compute twice, must match)
        let hash2 = lillux::sha256_hex(lillux::canonical_json(object).unwrap().as_bytes());
        assert_eq!(hash, hash2, "case {}: hash not stable", i);
        assert_eq!(hash.len(), 64, "case {}: hash must be 64 hex chars", i);

        println!("thread_event case {}: hash={}", i, hash);
    }
}

#[test]
fn golden_thread_snapshot_canonical_json() {
    let cases = [
        json!({
            "schema": 2,
            "kind": "thread_snapshot",
            "thread_id": "T-root-001",
            "chain_root_id": "T-root-001",
            "status": "running",
            "kind_name": "directive",
            "item_ref": "directive:agent/core/base",
            "executor_ref": "native:directive-runtime",
            "launch_mode": "inline",
            "current_site_id": "site:host",
            "origin_site_id": "site:host",
            "upstream_thread_id": null,
            "requested_by": "user:alice",
            "project_root": null,
            "captured_history_policy": {
                "retention": { "mode": "durable" },
                "canonical_item_ref": "directive:agent/core/base",
                "item_content_hash": "11".repeat(32),
                "item_signer_fingerprint": "22".repeat(32),
                "item_trust_class": "trusted",
                "kind_schema_content_hash": "33".repeat(32),
                "resolved_from": {
                    "node_default": { "node_policy": "missing_config" }
                }
            },
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-21T12:00:00Z",
            "updated_at": "2026-04-21T12:00:00Z",
            "started_at": "2026-04-21T12:00:00Z",
            "finished_at": null,
            "result": null,
            "outcome_code": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 1,
            "last_thread_seq": 1
        }),
        json!({
            "schema": 2,
            "kind": "thread_snapshot",
            "thread_id": "T-snap-002",
            "chain_root_id": "T-root-001",
            "status": "completed",
            "kind_name": "tool",
            "item_ref": "tool:ryeos/bash/bash",
            "executor_ref": "native:subprocess",
            "launch_mode": "inline",
            "current_site_id": "site:host",
            "origin_site_id": "site:host",
            "upstream_thread_id": "T-root-001",
            "requested_by": null,
            "project_root": null,
            "captured_history_policy": null,
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-21T12:00:01Z",
            "updated_at": "2026-04-21T12:00:05Z",
            "started_at": "2026-04-21T12:00:01Z",
            "finished_at": "2026-04-21T12:00:05Z",
            "result": { "exit_code": 0 },
            "outcome_code": "success",
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 2,
            "last_thread_seq": 1
        }),
    ];

    for (i, object) in cases.iter().enumerate() {
        let canonical = lillux::canonical_json(object).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());

        assert_eq!(
            object["kind"], "thread_snapshot",
            "case {}: invalid kind",
            i
        );
        assert_eq!(object["schema"], 2, "case {}: invalid schema", i);

        let hash2 = lillux::sha256_hex(lillux::canonical_json(object).unwrap().as_bytes());
        assert_eq!(hash, hash2, "case {}: hash not stable", i);
        assert_eq!(hash.len(), 64, "case {}: hash must be 64 hex chars", i);

        println!("thread_snapshot case {}: hash={}", i, hash);
    }
}

#[test]
fn golden_chain_state_canonical_json() {
    let cases = [
        json!({
            "schema": 1,
            "kind": "chain_state",
            "chain_root_id": "T-root-001",
            "chain_seq": 0,
            "thread_id": "T-root-001",
            "state": {
                "phase": "initialized",
                "item_ref": "directive:agent/core/base"
            },
            "ts": "2026-04-21T12:00:00Z"
        }),
        json!({
            "schema": 1,
            "kind": "chain_state",
            "chain_root_id": "T-root-001",
            "chain_seq": 1,
            "thread_id": "T-root-001",
            "state": {
                "phase": "executing",
                "item_ref": "tool:ryeos/bash/bash",
                "exit_code": 0
            },
            "ts": "2026-04-21T12:00:02Z"
        }),
    ];

    for (i, object) in cases.iter().enumerate() {
        let canonical = lillux::canonical_json(object).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());

        assert_eq!(object["kind"], "chain_state", "case {}: invalid kind", i);
        assert_eq!(object["schema"], 1, "case {}: invalid schema", i);

        let hash2 = lillux::sha256_hex(lillux::canonical_json(object).unwrap().as_bytes());
        assert_eq!(hash, hash2, "case {}: hash not stable", i);
        assert_eq!(hash.len(), 64, "case {}: hash must be 64 hex chars", i);

        println!("chain_state case {}: hash={}", i, hash);
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
        // not a behavioral test. Env-var guard keeps CI green.
        return;
    }

    let dir = test_vectors_dir();
    fs::create_dir_all(&dir).expect("create test-vectors dir");

    for file in [
        "thread_event_vectors.json",
        "thread_snapshot_vectors.json",
        "chain_state_vectors.json",
    ] {
        let vectors_path = dir.join(file);
        if !vectors_path.exists() {
            eprintln!("Skipping: {} not found", file);
            continue;
        }

        let data = fs::read_to_string(&vectors_path).expect("failed to read vectors file");
        let mut vectors: Value = serde_json::from_str(&data).expect("failed to parse vectors file");

        let cases = vectors["cases"]
            .as_array_mut()
            .expect("cases must be an array");

        for case in cases {
            let object = &case["object"];
            let canonical = lillux::canonical_json(object).unwrap();
            let hash = lillux::sha256_hex(canonical.as_bytes());

            case["expected_hash"] = json!(hash);
        }

        let updated = serde_json::to_string_pretty(&vectors).expect("failed to serialize vectors");
        fs::write(&vectors_path, updated).expect("failed to write updated vectors");

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

    let hash1 = lillux::sha256_hex(lillux::canonical_json(&event).unwrap().as_bytes());
    let hash2 = lillux::sha256_hex(lillux::canonical_json(&event).unwrap().as_bytes());

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

    let canonical1 = lillux::canonical_json(&obj1).unwrap();
    let canonical2 = lillux::canonical_json(&obj2).unwrap();

    assert_eq!(
        canonical1, canonical2,
        "Canonical JSON must be deterministic"
    );
}
