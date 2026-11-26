//! A peer node reads other nodes' service answers with a key that is not the
//! operator's, so each answer reaches it shaped by the item's tier. A field
//! the peer's client requires but the tier does not name vanishes, and the
//! client then fails (or, worse, reads a default). Each case below takes an
//! answer as the server emits it, shapes it for the class the peer is, and
//! checks that every leaf the client reads survived and that no free text
//! came through.

use std::path::{Path, PathBuf};

use ryeos_node_runtime::disclosure::{DisclosureClass, ServiceDisclosure, shape_result};
use serde_json::{Value, json};

fn item(service: &str) -> Value {
    let root: PathBuf =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../bundles/core/.ai/services");
    let text = std::fs::read_to_string(root.join(format!("{service}.yaml")))
        .unwrap_or_else(|error| panic!("{service}: {error}"));
    serde_yaml::from_str(&text).unwrap_or_else(|error| panic!("{service}: {error}"))
}

fn shaped(service: &str, class: DisclosureClass, answer: Value) -> Value {
    let body = item(service);
    let tier = ServiceDisclosure::parse(&body["disclosure"])
        .unwrap_or_else(|error| panic!("{service}: {error}"));
    shape_result(class, Some(&tier), answer, tier.bounds())
        .unwrap_or_else(|refusal| panic!("{service}: {refusal:?}"))
}

/// Every pointer in `read` must survive shaping for `class`; every pointer in
/// `withheld` must not.
fn check(service: &str, class: DisclosureClass, answer: Value, read: &[&str], withheld: &[&str]) {
    let out = shaped(service, class, answer);
    for pointer in read {
        assert!(
            out.pointer(pointer).is_some_and(|value| !value.is_null()),
            "{service} ({class:?}): a client reads {pointer} but the tier drops it: {out}"
        );
    }
    for pointer in withheld {
        assert!(
            out.pointer(pointer).is_none(),
            "{service} ({class:?}): {pointer} is free text and must not reach this class: {out}"
        );
    }
}

const HASH: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn the_public_key_document_a_peer_pins_survives_for_the_public_and_for_principals() {
    let answer = json!({
        "principal_id": "fp:x", "fingerprint": "x", "signing_key": "ed25519:k",
        "vault_fingerprint": HASH, "site_id": "site:s", "version": "1",
    });
    for class in [DisclosureClass::Public, DisclosureClass::Principal] {
        check(
            "identity/public_key",
            class,
            answer.clone(),
            &[
                "/principal_id",
                "/fingerprint",
                "/signing_key",
                "/vault_fingerprint",
                "/site_id",
            ],
            &[],
        );
    }
}

#[test]
fn authorize_key_keeps_what_the_client_reads_and_withholds_the_label() {
    let answer = json!({
        "fingerprint": "f", "scopes": ["a"], "granted_by": "g", "created_at": "t",
        "expires_at": null, "label": "free text",
    });
    check(
        "identity/authorize-key",
        DisclosureClass::Principal,
        answer,
        &["/fingerprint", "/scopes"],
        &["/label"],
    );
}

#[test]
fn a_malformed_closure_entry_keeps_its_hash_and_withholds_its_reason() {
    let describe = json!({ "malformed_objects": [{ "hash": HASH, "reason": "path /x" }] });
    check(
        "objects/closure/describe",
        DisclosureClass::Principal,
        describe,
        &["/malformed_objects/0/hash"],
        &["/malformed_objects/0/reason"],
    );
}

#[test]
fn a_thread_read_by_a_peer_carries_the_launch_hashes_the_workflow_binds() {
    let answer = json!({ "thread": {
        "thread_id": "T-1", "chain_root_id": "T-1", "status": "completed",
        "admitted_launch_capsule_hash": HASH,
        "project_authority": {
            "kind": "pinned_generation", "base_snapshot_hash": HASH,
            "display_path": "/home/someone/project",
        },
    }});
    check(
        "threads/get",
        DisclosureClass::Principal,
        answer,
        &[
            "/thread/thread_id",
            "/thread/admitted_launch_capsule_hash",
            "/thread/project_authority/base_snapshot_hash",
        ],
        &[
            "/thread/project_authority/display_path",
            "/thread/project_authority/kind",
        ],
    );
}

#[test]
fn sync_job_records_keep_their_hash_lists_and_withhold_errors_and_results() {
    let job = json!({
        "job_id": "j", "state": "done", "roots": [HASH], "heads": [HASH],
        "uploaded_hashes": [HASH], "fetched_hashes": [HASH],
        "peer": "remote-a", "last_error": "boom", "result": { "x": 1 },
    });
    check(
        "sync/jobs/list",
        DisclosureClass::Principal,
        json!({ "jobs": [job.clone()] }),
        &[
            "/jobs/0/roots/0",
            "/jobs/0/heads/0",
            "/jobs/0/uploaded_hashes/0",
            "/jobs/0/fetched_hashes/0",
        ],
        &["/jobs/0/peer", "/jobs/0/last_error", "/jobs/0/result"],
    );
    check(
        "sync/jobs/inspect",
        DisclosureClass::Principal,
        json!({ "job": job, "attempts": [] }),
        &[
            "/job/roots/0",
            "/job/heads/0",
            "/job/uploaded_hashes/0",
            "/job/fetched_hashes/0",
        ],
        &["/job/peer", "/job/last_error", "/job/result"],
    );
}

#[test]
fn the_offline_signer_report_gives_a_non_operator_the_closed_code_only() {
    let answer = json!({ "signer": {
        "fingerprint": "f", "local_operator": false, "grant_digest": null,
        "grant_error": "cannot read /home/x/identity", "grant_error_code": "grant_unverifiable",
    }});
    check(
        "offline-operations/facts",
        DisclosureClass::Principal,
        answer,
        &["/signer/grant_error_code"],
        &["/signer/grant_error"],
    );
}

#[test]
fn the_handoff_answer_carries_the_stage_timings_the_qualification_reads() {
    let measurements = json!({
        "schema": "s", "checkpoint_load_ms": 1, "closure_verification_ms": 2,
        "target_prepare_ms": 3, "source_publication_ms": 4, "target_adoption_ms": 5,
        "event_replay_ms": 6, "project_materialization_ms": 7,
        "worker_attach_recovery_ms": 8, "adoption_path": "attached_before_import",
        "total_handoff_ms": 9,
    });
    let reads: Vec<String> = measurements
        .as_object()
        .unwrap()
        .keys()
        .map(|key| format!("/qualification_measurements/{key}"))
        .collect();
    let reads: Vec<&str> = reads.iter().map(String::as_str).collect();
    check(
        "worker-executions/handoff",
        DisclosureClass::Principal,
        json!({ "operation_id": "o", "qualification_measurements": measurements }),
        &reads,
        &[],
    );
}
