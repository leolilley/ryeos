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

    fn store_object(&self, hash: &str, value: Value) {
        self.objects.lock().unwrap().insert(hash.to_string(), value);
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
        "item_ref": "bin/x86_64-unknown-linux-gnu/directive-runtime",
        "content_blob_hash": blob_hash,
        "mode": mode,
        "integrity": "sha256:deadbeef",
    })
}

fn make_signed_item_source_value(blob_hash: &str, mode: u32, fingerprint: &str) -> Value {
    json!({
        "item_ref": "bin/x86_64-unknown-linux-gnu/directive-runtime",
        "content_blob_hash": blob_hash,
        "mode": mode,
        "integrity": "sha256:deadbeef",
        "signature_info": {
            "fingerprint": fingerprint,
            "signature": "fake-sig",
        },
    })
}

#[test]
fn resolves_from_manifest_for_host_triple() {
    let cas = FakeCas::new();

    let item_source = make_item_source_value("aa".repeat(32).as_str(), 0o755);
    let is_hash = "cc".repeat(32);
    cas.store_object(&is_hash, item_source.clone());

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
fn missing_blob_in_cas_errors() {
    let cas = FakeCas::new();

    let item_source = make_item_source_value("aa".repeat(32).as_str(), 0o755);
    let is_hash = "cc".repeat(32);
    cas.store_object(&is_hash, item_source);

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
        "item_ref": "bin/x86_64-unknown-linux-gnu/directive-runtime",
        "content_blob_hash": "aa".repeat(32),
        "integrity": "sha256:deadbeef",
    });
    let is_hash = "cc".repeat(32);
    cas.store_object(&is_hash, item_source);

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
        msg.contains("no mode"),
        "expected MissingMode error, got: {msg}"
    );
}

#[test]
fn not_native_executor_errors() {
    let cas = FakeCas::new();

    let err = resolve_native_executor(
        &HashMap::new(),
        "tool:rye/core/bash/bash",
        "x86_64-unknown-linux-gnu",
        cas.get_object_fn(),
    )
    .unwrap_err();

    let msg = format!("{err}");
    assert!(
        msg.contains("not a native executor"),
        "expected NotNativeExecutor error, got: {msg}"
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

    let msg = format!("{err}");
    assert!(
        msg.contains("not a native executor"),
        "expected NotNativeExecutor error for empty prefix, got: {msg}"
    );
}

// ── Trust verification tests ────────────────────────────────────

#[test]
fn trusted_binary_passes() {
    let item_source = make_signed_item_source_value("aa".repeat(32).as_str(), 0o755, "trusted-fp");

    let (trust_class, fp) = verify_executor_trust(&item_source, |f| f == "trusted-fp");

    assert_eq!(trust_class, ryeos_engine::resolution::TrustClass::TrustedSystem);
    assert_eq!(fp.as_deref(), Some("trusted-fp"));
}

#[test]
fn untrusted_binary_with_unknown_signer() {
    let item_source = make_signed_item_source_value("aa".repeat(32).as_str(), 0o755, "unknown-fp");

    let (trust_class, fp) = verify_executor_trust(&item_source, |_f| false);

    assert_eq!(trust_class, ryeos_engine::resolution::TrustClass::UntrustedUserSpace);
    assert_eq!(fp.as_deref(), Some("unknown-fp"));
}

#[test]
fn unsigned_binary_is_unsigned() {
    let item_source = make_item_source_value("aa".repeat(32).as_str(), 0o755);

    let (trust_class, fp) = verify_executor_trust(&item_source, |_f| false);

    assert_eq!(trust_class, ryeos_engine::resolution::TrustClass::Unsigned);
    assert!(fp.is_none());
}

#[test]
fn empty_fingerprint_is_untrusted() {
    let item_source = json!({
        "item_ref": "bin/x86_64-unknown-linux-gnu/directive-runtime",
        "content_blob_hash": "aa".repeat(32),
        "mode": 0o755,
        "integrity": "sha256:deadbeef",
        "signature_info": {
            "fingerprint": "",
            "signature": "",
        },
    });

    let (trust_class, fp) = verify_executor_trust(&item_source, |_f| false);

    assert_eq!(trust_class, ryeos_engine::resolution::TrustClass::UntrustedUserSpace);
    assert!(fp.is_none());
}
