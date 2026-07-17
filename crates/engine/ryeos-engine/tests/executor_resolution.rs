//! Tests for executor resolution from CAS manifests.

use std::collections::HashMap;
use std::sync::Mutex;

use ryeos_engine::executor_resolution::*;
use serde_json::{json, Value};

/// In-memory CAS fixture for tests.
struct FakeCas {
    objects: Mutex<HashMap<String, Value>>,
}

impl FakeCas {
    fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
        }
    }

    fn store_object(&self, value: Value) -> String {
        let hash = lillux::cas::sha256_hex(lillux::cas::canonical_json(&value).unwrap().as_bytes());
        self.objects.lock().unwrap().insert(hash.clone(), value);
        hash
    }

    fn get_object(&self, hash: &str) -> Option<Value> {
        self.objects.lock().unwrap().get(hash).cloned()
    }

    fn get_object_fn(&self) -> impl Fn(&str) -> std::result::Result<Option<Value>, String> + '_ {
        move |hash| Ok(self.get_object(hash))
    }
}

fn make_item_source_value(blob_hash: &str, mode: u32) -> Value {
    json!({
        "kind": "item_source",
        "item_ref": "bin/x86_64-unknown-linux-gnu/directive-runtime",
        "content_blob_hash": blob_hash,
        "mode": mode,
        "integrity": format!("sha256:{blob_hash}"),
        "signature_info": null,
    })
}

#[test]
fn resolves_from_manifest_for_host_triple() {
    let cas = FakeCas::new();

    let item_source = make_item_source_value("aa".repeat(32).as_str(), 0o755);
    let is_hash = cas.store_object(item_source.clone());

    let mut manifest_hashes = HashMap::new();
    manifest_hashes.insert(
        "bin/x86_64-unknown-linux-gnu/directive-runtime".to_string(),
        is_hash.clone(),
    );

    let resolved = resolve_native_executor(
        &manifest_hashes,
        "native:directive-runtime",
        "x86_64-unknown-linux-gnu",
        cas.get_object_fn(),
    )
    .expect("resolution should succeed");

    assert_eq!(resolved.blob_hash, "aa".repeat(32));
    assert_eq!(resolved.mode, 0o755);
}

#[test]
fn missing_for_host_triple_errors() {
    let cas = FakeCas::new();
    let manifest_hashes = HashMap::new();

    let err = resolve_native_executor(
        &manifest_hashes,
        "native:directive-runtime",
        "x86_64-unknown-linux-gnu",
        cas.get_object_fn(),
    )
    .unwrap_err();

    let msg = format!("{err}");
    assert!(
        msg.contains("not found in manifest"),
        "expected NotInManifest error, got: {msg}"
    );
    assert!(
        msg.contains("x86_64-unknown-linux-gnu"),
        "error should mention host triple, got: {msg}"
    );
}

#[test]
fn missing_for_host_triple_lists_available_triples() {
    // Manifest ships the binary for two OTHER triples but not the host.
    // Error must enumerate them so the operator can see at a glance.
    let cas = FakeCas::new();
    let mut manifest_hashes = HashMap::new();
    manifest_hashes.insert(
        "bin/aarch64-apple-darwin/directive-runtime".to_string(),
        "11".repeat(32),
    );
    manifest_hashes.insert(
        "bin/x86_64-pc-windows-msvc/directive-runtime".to_string(),
        "22".repeat(32),
    );
    // Unrelated executor — must NOT pollute available_triples.
    manifest_hashes.insert(
        "bin/x86_64-unknown-linux-gnu/some-other-tool".to_string(),
        "33".repeat(32),
    );

    let err = resolve_native_executor(
        &manifest_hashes,
        "native:directive-runtime",
        "x86_64-unknown-linux-gnu",
        cas.get_object_fn(),
    )
    .unwrap_err();

    let msg = format!("{err}");
    assert!(msg.contains("aarch64-apple-darwin"), "got: {msg}");
    assert!(msg.contains("x86_64-pc-windows-msvc"), "got: {msg}");
    assert!(
        !msg.contains("some-other-tool"),
        "must not leak unrelated executors: {msg}"
    );
    assert!(
        msg.contains("rebuild"),
        "diagnostic should suggest rebuild action: {msg}"
    );
}

#[test]
fn missing_executor_entirely_says_so() {
    // Manifest ships nothing for this executor at all (no triple anywhere).
    let cas = FakeCas::new();
    let mut manifest_hashes = HashMap::new();
    manifest_hashes.insert(
        "bin/x86_64-unknown-linux-gnu/some-other-tool".to_string(),
        "11".repeat(32),
    );

    let err = resolve_native_executor(
        &manifest_hashes,
        "native:directive-runtime",
        "x86_64-unknown-linux-gnu",
        cas.get_object_fn(),
    )
    .unwrap_err();

    let msg = format!("{err}");
    assert!(
        msg.contains("ships no binaries for this executor at all"),
        "should say bundle ships zero binaries for this executor; got: {msg}"
    );
    assert!(msg.contains("rebuild"), "should suggest rebuild: {msg}");
}

#[test]
fn missing_blob_in_cas_errors() {
    let cas = FakeCas::new();

    let item_source = make_item_source_value("aa".repeat(32).as_str(), 0o755);
    let is_hash = cas.store_object(item_source);

    let mut manifest_hashes = HashMap::new();
    manifest_hashes.insert(
        "bin/x86_64-unknown-linux-gnu/directive-runtime".to_string(),
        is_hash.clone(),
    );

    // The item_source is in CAS but points at a blob that doesn't exist.
    // Resolution should still succeed — blob existence is checked
    // during materialization, not during resolution.
    let resolved = resolve_native_executor(
        &manifest_hashes,
        "native:directive-runtime",
        "x86_64-unknown-linux-gnu",
        cas.get_object_fn(),
    )
    .expect("resolution succeeds even if blob is missing");

    assert_eq!(resolved.blob_hash, "aa".repeat(32));
}

#[test]
fn missing_mode_errors() {
    let cas = FakeCas::new();

    // item_source without mode field
    let item_source = json!({
        "kind": "item_source",
        "item_ref": "bin/x86_64-unknown-linux-gnu/directive-runtime",
        "content_blob_hash": "aa".repeat(32),
        "integrity": format!("sha256:{}", "aa".repeat(32)),
        "signature_info": null,
    });
    let is_hash = cas.store_object(item_source);

    let mut manifest_hashes = HashMap::new();
    manifest_hashes.insert(
        "bin/x86_64-unknown-linux-gnu/directive-runtime".to_string(),
        is_hash,
    );

    let err = resolve_native_executor(
        &manifest_hashes,
        "native:directive-runtime",
        "x86_64-unknown-linux-gnu",
        cas.get_object_fn(),
    )
    .unwrap_err();

    let msg = format!("{err}");
    assert!(
        msg.contains("expected fields") && msg.contains("mode"),
        "expected strict missing-mode error, got: {msg}"
    );
}

#[test]
fn not_native_executor_errors() {
    let cas = FakeCas::new();

    let err = resolve_native_executor(
        &HashMap::new(),
        "tool:ryeos/core/bash/bash",
        "x86_64-unknown-linux-gnu",
        cas.get_object_fn(),
    )
    .unwrap_err();

    assert!(
        matches!(&err, ExecutorResolutionError::NotNativeExecutor),
        "expected NotNativeExecutor error, got: {err:?}"
    );
}

#[test]
fn empty_native_prefix_errors() {
    let cas = FakeCas::new();

    let err = resolve_native_executor(
        &HashMap::new(),
        "native:",
        "x86_64-unknown-linux-gnu",
        cas.get_object_fn(),
    )
    .unwrap_err();

    assert!(
        matches!(&err, ExecutorResolutionError::NotNativeExecutor),
        "expected NotNativeExecutor error for empty prefix, got: {err:?}"
    );
}

// ── Trust-root verification tests ───────────────────────────────

#[test]
fn signed_manifest_ref_requires_cryptographic_proof() {
    let key = lillux::crypto::SigningKey::from_bytes(&[41; 32]);
    let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
    let manifest_hash = "ab".repeat(32);
    let signed = lillux::signature::sign_content(
        &format!("{EXECUTOR_MANIFEST_REF_DOMAIN}\n{manifest_hash}\n"),
        &key,
        "#",
        None,
    );

    let verified = verify_signed_executor_manifest_ref(
        &signed,
        |candidate| (candidate == fingerprint.as_str()).then(|| key.verifying_key()),
        ryeos_engine::resolution::TrustClass::TrustedBundle,
    )
    .unwrap();

    assert_eq!(verified.manifest_hash, manifest_hash);
    assert_eq!(verified.signer_fingerprint, fingerprint);
}

#[test]
fn claimed_fingerprint_without_signed_ref_is_rejected() {
    let hash = "ab".repeat(32);
    let err = verify_signed_executor_manifest_ref(
        &format!("{hash}\n"),
        |_candidate| None,
        ryeos_engine::resolution::TrustClass::TrustedBundle,
    )
    .unwrap_err();

    assert!(matches!(
        err,
        ExecutorResolutionError::ManifestRefInvalid { .. }
    ));
}

#[test]
fn item_source_cas_substitution_is_rejected() {
    let cas = FakeCas::new();
    let item_source = make_item_source_value("aa".repeat(32).as_str(), 0o755);
    let actual_hash = cas.store_object(item_source);
    let mut manifest_hashes = HashMap::new();
    manifest_hashes.insert(
        "bin/x86_64-unknown-linux-gnu/directive-runtime".to_string(),
        "cc".repeat(32),
    );
    let substituted = cas.get_object(&actual_hash).unwrap();
    cas.objects
        .lock()
        .unwrap()
        .insert("cc".repeat(32), substituted);

    let err = resolve_native_executor(
        &manifest_hashes,
        "native:directive-runtime",
        "x86_64-unknown-linux-gnu",
        cas.get_object_fn(),
    )
    .unwrap_err();

    assert!(matches!(
        err,
        ExecutorResolutionError::CasObjectHashMismatch {
            object_kind: "ItemSource",
            ..
        }
    ));
}
