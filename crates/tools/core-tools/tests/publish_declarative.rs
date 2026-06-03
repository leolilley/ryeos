//! Integration test: `run_publish` supports declarative bundles with no `.ai/bin`.

use std::path::{Path, PathBuf};

use lillux::crypto::{DecodePrivateKey, SigningKey};

fn host_triple() -> String {
    let output = std::process::Command::new("rustc")
        .args(["-vV"])
        .output()
        .expect("rustc -vV");
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find(|line| line.starts_with("host:"))
        .expect("host triple in rustc -vV")
        .strip_prefix("host:")
        .unwrap()
        .trim()
        .to_string()
}

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

fn core_bundle() -> PathBuf {
    workspace_root().join("bundles").join("core")
}

fn load_dev_signing_key() -> SigningKey {
    let pem = std::fs::read_to_string(dev_key_path())
        .expect("dev signing key not found — run from workspace root");
    SigningKey::from_pkcs8_pem(&pem).expect("parse dev signing key")
}

fn create_declarative_bundle(root: &Path) -> PathBuf {
    let bundle = root.join("studio");
    let ai_dir = bundle.join(ryeos_engine::AI_DIR);
    std::fs::create_dir_all(&ai_dir).unwrap();
    std::fs::write(
        ai_dir.join("manifest.source.yaml"),
        r#"name: studio
version: "0.1.0"
description: "Declarative studio bundle test fixture"
requires_kinds: []
"#,
    )
    .unwrap();
    bundle
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

fn stage_core_handler_binaries(registry: &Path) {
    let triple = host_triple();
    let bin_dir = registry.join(ryeos_engine::AI_DIR).join("bin").join(triple);
    std::fs::create_dir_all(&bin_dir).unwrap();
    for name in [
        "rye-parser-yaml-document",
        "rye-parser-yaml-header-document",
        "rye-parser-regex-kv",
        "rye-composer-identity",
        "ryeos-core-tools",
    ] {
        let path = bin_dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho test {name}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
    }
}

fn prepare_core_registry(root: &Path, key: &SigningKey) -> PathBuf {
    let registry = root.join("core");
    copy_dir_recursive(&core_bundle(), &registry);
    stage_core_handler_binaries(&registry);
    ryeos_tools::actions::build_bundle::rebuild_bundle_manifest(&registry, key)
        .unwrap_or_else(|e| panic!("prepare core registry failed: {e:#}"));
    registry
}

fn run_publish_once(
    bundle_dir: &Path,
    registry_root: &Path,
    key: &SigningKey,
) -> ryeos_tools::actions::publish::PublishReport {
    let opts = ryeos_tools::actions::publish::PublishOptions {
        bundle_source: bundle_dir.to_path_buf(),
        registry_roots: vec![registry_root.to_path_buf()],
        signing_key: key.clone(),
        owner: "test".to_string(),
        emit_trust_doc: false,
    };
    ryeos_tools::actions::publish::run_publish(&opts)
        .unwrap_or_else(|e| panic!("publish failed: {e:#}"))
}

#[test]
fn declarative_publish_skips_binary_rebuild_and_signs_manifest() {
    if !core_bundle().join(ryeos_engine::AI_DIR).is_dir() {
        eprintln!("skipping: bundles/core not found");
        return;
    }
    let key = load_dev_signing_key();
    let tmp = tempfile::tempdir().unwrap();
    let bundle = create_declarative_bundle(tmp.path());
    let registry = prepare_core_registry(tmp.path(), &key);

    let report = run_publish_once(&bundle, &registry, &key);

    assert!(report.binary_rebuild_skipped);
    assert!(report.rebuild_report.is_none());
    assert!(report.binary_rebuild_skip_reason.is_some());
    assert!(report.sign_report.failed.is_empty());

    let manifest = bundle.join(ryeos_engine::AI_DIR).join("manifest.yaml");
    assert!(manifest.is_file(), "manifest.yaml should be generated");
    let manifest_content = std::fs::read_to_string(&manifest).unwrap();
    let first_line = manifest_content.lines().next().unwrap_or_default();
    assert!(
        lillux::signature::parse_signature_line(first_line, "#", None).is_some(),
        "manifest.yaml should have a valid RyeOS signature header"
    );

    assert!(
        !bundle.join(ryeos_engine::AI_DIR).join("objects").exists(),
        "publish should not create binary CAS objects for a declarative bundle"
    );
    assert!(
        !bundle.join(ryeos_engine::AI_DIR).join("refs").exists(),
        "publish should not create binary CAS refs for a declarative bundle"
    );
}

#[test]
fn direct_binary_rebuild_remains_strict_without_bin_directory() {
    let key = load_dev_signing_key();
    let tmp = tempfile::tempdir().unwrap();
    let bundle = create_declarative_bundle(tmp.path());

    let err = ryeos_tools::actions::build_bundle::rebuild_bundle_manifest(&bundle, &key)
        .expect_err("direct binary CAS rebuild should require .ai/bin");

    assert!(
        format!("{err:#}").contains("no .ai/bin directory"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn declarative_publish_is_idempotent_without_bin_directory() {
    if !core_bundle().join(ryeos_engine::AI_DIR).is_dir() {
        eprintln!("skipping: bundles/core not found");
        return;
    }
    let key = load_dev_signing_key();
    let tmp = tempfile::tempdir().unwrap();
    let bundle = create_declarative_bundle(tmp.path());
    let registry = prepare_core_registry(tmp.path(), &key);

    let _report1 = run_publish_once(&bundle, &registry, &key);
    let manifest = bundle.join(ryeos_engine::AI_DIR).join("manifest.yaml");
    let manifest1 = std::fs::read_to_string(&manifest).unwrap();

    let report2 = run_publish_once(&bundle, &registry, &key);
    let manifest2 = std::fs::read_to_string(&manifest).unwrap();

    assert_eq!(
        manifest1, manifest2,
        "second publish should leave manifest unchanged"
    );
    assert!(report2.binary_rebuild_skipped);
    assert!(report2.rebuild_report.is_none());
    assert!(!report2.manifest_changed);
    assert!(report2.sign_report.failed.is_empty());
}
