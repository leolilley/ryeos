//! Integration test: `run_publish` is idempotent.
//!
//! Two consecutive publishes of the same bundle with the same key and
//! no source content changes must produce zero file changes (zero git
//! diff). This is the acceptance contract for the incremental publish
//! design: every signing path checks body hash + signer fingerprint +
//! signature validity before writing.
//!
//! Prerequisites:
//!   - `bundles/core/` must exist and contain a publishable bundle
//!   - `.dev-keys/PUBLISHER_DEV.pem` must exist (dev signing key)
//!
//! This test is intentionally slow (~20-30s) because it exercises the
//! full publish pipeline including CAS rebuild and kind schema bootstrap.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lillux::crypto::{DecodePrivateKey, SigningKey};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("bundles").is_dir())
        .expect("workspace root with bundles/ directory")
        .to_path_buf()
}

fn dev_key_path() -> PathBuf {
    workspace_root().join(".dev-keys").join("PUBLISHER_DEV.pem")
}

fn source_bundle() -> PathBuf {
    workspace_root().join("bundles").join("core")
}

/// Snapshot every signable source file's SHA-256 hash under a directory.
///
/// Only includes files that the publish pipeline signs (YAML, MD, TOML
/// under `.ai/`). Excludes CAS-derived artifacts (objects, refs, bin
/// sidecars, manifest.source.yaml) because those are rebuilt from
/// scratch on every run and are not part of the idempotency contract.
fn snapshot_file_hashes(root: &Path) -> BTreeMap<String, String> {
    let mut hashes = BTreeMap::new();
    walk_files(root, &mut hashes, root);
    hashes
}

fn walk_files(dir: &Path, out: &mut BTreeMap<String, String>, base: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip CAS-derived directories — they are rebuilt from scratch
            // each run and are not part of the signing idempotency contract.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if matches!(name, "objects" | "refs" | "bin") {
                    continue;
                }
            }
            // Skip .git
            if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                continue;
            }
            walk_files(&path, out, base);
        } else if path.is_file() {
            // Skip manifest.source.yaml — it's a CAS rebuild artifact
            // (materialized from object refs, regenerated each run).
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name == "manifest.source.yaml" {
                    continue;
                }
            }
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let content = std::fs::read(&path).unwrap_or_default();
            let hash = lillux::sha256_hex(&content);
            out.insert(rel, hash);
        }
    }
}

fn load_dev_signing_key() -> SigningKey {
    let pem = std::fs::read_to_string(dev_key_path())
        .expect("dev signing key not found — run from workspace root");
    SigningKey::from_pkcs8_pem(&pem)
        .expect("parse dev signing key")
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path);
        } else {
            std::fs::copy(&src_path, &dst_path).unwrap();
        }
    }
}

fn run_publish_once(bundle_dir: &Path, key: &SigningKey) -> ryeos_tools::actions::publish::PublishReport {
    let opts = ryeos_tools::actions::publish::PublishOptions {
        bundle_source: bundle_dir.to_path_buf(),
        registry_root: bundle_dir.to_path_buf(), // core is its own registry
        signing_key: key.clone(),
        owner: "test".to_string(),
        emit_trust_doc: false,
    };
    ryeos_tools::actions::publish::run_publish(&opts)
        .unwrap_or_else(|e| panic!("publish failed: {e:#}"))
}

#[test]
fn publish_twice_produces_zero_diff() {
    let source = source_bundle();
    if !source.join(ryeos_engine::AI_DIR).is_dir() {
        eprintln!("skipping: bundles/core not found");
        return;
    }
    let key = load_dev_signing_key();

    // Create a temp copy to avoid mutating the source tree
    let tmp = tempfile::tempdir().unwrap();
    let bundle = tmp.path().join("core");
    copy_dir_recursive(&source, &bundle);

    // Run 1: initial publish
    let report1 = run_publish_once(&bundle, &key);

    // Snapshot all file hashes
    let hashes1 = snapshot_file_hashes(&bundle);

    // Run 2: re-publish with no content changes
    let report2 = run_publish_once(&bundle, &key);

    // Snapshot again
    let hashes2 = snapshot_file_hashes(&bundle);

    // Assert: no file bytes changed
    assert_eq!(
        hashes1, hashes2,
        "second publish changed file contents — publish is not idempotent"
    );

    // Assert: report shows zero signed items on second run
    assert_eq!(
        report2.bootstrap_signed, 0,
        "bootstrap_signed should be 0 on second run, got {}",
        report2.bootstrap_signed
    );
    assert!(
        report2.sign_report.signed.is_empty(),
        "sign_report.signed should be empty on second run, got: {:?}",
        report2.sign_report.signed
    );
    assert!(
        !report2.manifest_changed,
        "manifest_changed should be false on second run"
    );

    // First run should have signed some items
    assert!(
        report1.bootstrap_signed + report1.sign_report.signed.len() > 0,
        "first run should have signed items — if this fails the bundle may already be fully signed"
    );

    // Basic sanity: second run validated everything
    assert!(
        report2.bootstrap_validated + report2.sign_report.validated.len() > 0,
        "second run should have validated existing items"
    );
}
