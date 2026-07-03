//! Integration test: `run_publish` supports declarative bundles with no `.ai/bin`.

use std::path::{Path, PathBuf};

use lillux::crypto::{DecodePrivateKey, SigningKey};
use ryeos_engine::trust::{compute_fingerprint, TrustStore, TrustedSigner};

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
    run_publish_once_with_trust(bundle_dir, registry_root, key, None)
}

fn run_publish_once_with_trust(
    bundle_dir: &Path,
    registry_root: &Path,
    key: &SigningKey,
    base_trust_store: Option<TrustStore>,
) -> ryeos_tools::actions::publish::PublishReport {
    let opts = ryeos_tools::actions::publish::PublishOptions {
        bundle_source: bundle_dir.to_path_buf(),
        registry_roots: vec![registry_root.to_path_buf()],
        signing_key: key.clone(),
        base_trust_store,
        owner: "test".to_string(),
        name: None,
        skip_unsignable: false,
        allow_namespace_mismatch: false,
        allow_uncovered_item_dirs: false,
        emit_trust_doc: false,
    };
    ryeos_tools::actions::publish::run_publish(&opts)
        .unwrap_or_else(|e| panic!("publish failed: {e:#}"))
}

fn trust_store_for_key(key: &SigningKey, label: &str) -> TrustStore {
    let verifying_key = key.verifying_key();
    TrustStore::from_signers(vec![TrustedSigner {
        fingerprint: compute_fingerprint(&verifying_key),
        verifying_key,
        label: Some(label.to_string()),
    }])
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
fn declarative_publish_requires_trust_for_registry_signed_by_different_key() {
    if !core_bundle().join(ryeos_engine::AI_DIR).is_dir() {
        eprintln!("skipping: bundles/core not found");
        return;
    }
    let registry_key = load_dev_signing_key();
    let author_key = SigningKey::from_bytes(&[99u8; 32]);
    let tmp = tempfile::tempdir().unwrap();
    let bundle = create_declarative_bundle(tmp.path());
    let registry = prepare_core_registry(tmp.path(), &registry_key);

    let err = ryeos_tools::actions::publish::run_publish(
        &ryeos_tools::actions::publish::PublishOptions {
            bundle_source: bundle.to_path_buf(),
            registry_roots: vec![registry.to_path_buf()],
            signing_key: author_key.clone(),
            base_trust_store: None,
            owner: "test".to_string(),
            name: None,
            skip_unsignable: false,
            allow_namespace_mismatch: false,
            allow_uncovered_item_dirs: false,
            emit_trust_doc: false,
        },
    )
    .expect_err("publish should reject untrusted dependency registry signer");
    let err = format!("{err:#}");
    assert!(err.contains("untrusted signer"), "unexpected error: {err}");

    let report = run_publish_once_with_trust(
        &bundle,
        &registry,
        &author_key,
        Some(trust_store_for_key(&registry_key, "registry")),
    );

    assert!(report.sign_report.failed.is_empty());
    assert_eq!(
        report.author_fingerprint,
        compute_fingerprint(&author_key.verifying_key()),
        "bundle source should be signed by the author key, not the registry key"
    );
    assert!(
        bundle
            .join(ryeos_engine::AI_DIR)
            .join("manifest.yaml")
            .is_file(),
        "manifest.yaml should be generated when dependency registry trust is supplied"
    );
}

/// A publish must never sweep node runtime state into the sign report. The
/// `node` kind's directory is `.ai/node`, which also holds `.ai/node/schedules`
/// and `.ai/node/routes` — runtime-owned prefixes a running daemon writes. The
/// runtime-owned floor excludes them before extension checks, so they never
/// become items and never raise namespace warnings.
#[test]
fn publish_excludes_runtime_owned_node_paths_from_sign_report() {
    if !core_bundle().join(ryeos_engine::AI_DIR).is_dir() {
        eprintln!("skipping: bundles/core not found");
        return;
    }
    let key = load_dev_signing_key();
    let tmp = tempfile::tempdir().unwrap();
    let bundle = create_declarative_bundle(tmp.path());
    let registry = prepare_core_registry(tmp.path(), &key);

    // Seed node runtime state that the `node` kind walk would otherwise sweep.
    let ai = bundle.join(ryeos_engine::AI_DIR);
    for (rel, body) in [
        ("node/schedules/nightly.yaml", "version: \"1\"\n"),
        ("node/routes/inbound.yaml", "version: \"1\"\n"),
        ("node/bundles/core.yaml", "version: \"1\"\n"),
    ] {
        let path = ai.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
    }

    let report = run_publish_once(&bundle, &registry, &key);

    let mut refs: Vec<&str> = Vec::new();
    for outcome in report
        .sign_report
        .validated
        .iter()
        .chain(report.sign_report.signed.iter())
        .chain(report.sign_report.failed.iter())
    {
        refs.push(outcome.item_ref.as_str());
    }
    for r in &refs {
        assert!(
            !r.contains("schedules") && !r.contains("routes") && !r.contains("bundles"),
            "runtime-owned node path leaked into sign report as `{r}`"
        );
    }
    assert!(
        report.sign_report.warnings.is_empty(),
        "runtime-owned exclusion should leave the namespace lint clean, got: {:?}",
        report.sign_report.warnings
    );
    // The runtime YAMLs must remain unsigned on disk — a bundle publish must
    // not author node runtime state.
    let scheduled = std::fs::read_to_string(ai.join("node/schedules/nightly.yaml")).unwrap();
    assert!(
        lillux::signature::parse_signature_line(scheduled.lines().next().unwrap_or_default(), "#", None)
            .is_none(),
        "node runtime state must not be signed by publish"
    );
}

/// A populated item directory whose kind is not in the loaded registry roots
/// must hard-fail the publish instead of silently skipping those items. The
/// `knowledge` kind lives in standard, so a bundle with `.ai/knowledge/` items
/// published against the core registry alone is exactly this case. Opting into
/// `allow_uncovered_item_dirs` lets a deliberately partial publish proceed.
#[test]
fn publish_hard_fails_on_uncovered_item_dir() {
    if !core_bundle().join(ryeos_engine::AI_DIR).is_dir() {
        eprintln!("skipping: bundles/core not found");
        return;
    }
    let key = load_dev_signing_key();
    let tmp = tempfile::tempdir().unwrap();
    let bundle = create_declarative_bundle(tmp.path());
    let registry = prepare_core_registry(tmp.path(), &key);

    let kdir = bundle
        .join(ryeos_engine::AI_DIR)
        .join("knowledge")
        .join("app");
    std::fs::create_dir_all(&kdir).unwrap();
    std::fs::write(kdir.join("notes.md"), "# notes\n").unwrap();

    let base_opts = |allow_uncovered: bool| ryeos_tools::actions::publish::PublishOptions {
        bundle_source: bundle.to_path_buf(),
        registry_roots: vec![registry.to_path_buf()],
        signing_key: key.clone(),
        base_trust_store: None,
        owner: "test".to_string(),
        name: None,
        skip_unsignable: false,
        allow_namespace_mismatch: false,
        allow_uncovered_item_dirs: allow_uncovered,
        emit_trust_doc: false,
    };

    let err = ryeos_tools::actions::publish::run_publish(&base_opts(false))
        .expect_err("uncovered knowledge/ dir must hard-fail the publish");
    let err = format!("{err:#}");
    assert!(
        err.contains("knowledge/") && err.contains("no registered kind"),
        "unexpected error: {err}"
    );

    // Opting out proceeds — the knowledge items are skipped, no error.
    ryeos_tools::actions::publish::run_publish(&base_opts(true))
        .expect("allow_uncovered_item_dirs should let a partial publish proceed");
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
