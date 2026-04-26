//! Integration tests for the runtime registry.
//!
//! Covers the YAML parser/validator (no trust store required) and the
//! end-to-end build path (signed YAML on disk → trust verify → group
//! by `serves`).

use std::fs;
use std::path::{Path, PathBuf};

use lillux::crypto::SigningKey;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::error::EngineError;
use ryeos_engine::runtime_registry::{RuntimeRegistry, RuntimeYaml};
use ryeos_engine::trust::{compute_fingerprint, TrustStore, TrustedSigner};

// ── Test helpers ─────────────────────────────────────────────────────

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[19u8; 32])
}

fn trust_store() -> TrustStore {
    let sk = signing_key();
    let vk = sk.verifying_key();
    let fp = compute_fingerprint(&vk);
    TrustStore::from_signers(vec![TrustedSigner {
        fingerprint: fp,
        verifying_key: vk,
        label: None,
    }])
}

fn tempdir() -> PathBuf {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rye_runtime_registry_test_{}_{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_signed_runtime(bundle_root: &Path, name: &str, body: &str) {
    let runtimes_dir = bundle_root.join(".ai").join("runtimes");
    fs::create_dir_all(&runtimes_dir).unwrap();
    let signed = lillux::signature::sign_content(body, &signing_key(), "#", None);
    fs::write(runtimes_dir.join(format!("{name}.yaml")), signed).unwrap();
}

const FULL_RUNTIME_YAML: &str = "\
kind: runtime
serves: directive
default: true
binary_ref: bin/{host_triple}/directive_runner
abi_version: v1
required_caps:
  - rye.read.directive.*
description: Default directive runtime
schema:
  envelope: LaunchEnvelope
  result: RuntimeResult
";

const MINIMAL_RUNTIME_YAML: &str = "\
kind: runtime
serves: directive
binary_ref: bin/runner
abi_version: v1
";

// ── Parser-only tests ────────────────────────────────────────────────

/// Use the registry to parse a raw YAML body. The registry doesn't
/// expose its internal parser publicly; instead we drive it through
/// `build_from_bundles` with a single signed file when the parser
/// path needs trust verification, and through direct `serde_yaml`
/// for purely-structural rejection cases (those don't need trust
/// because malformed YAML can't reach trust verification anyway).
///
/// For the parser/validator unit tests below we drive
/// `build_from_bundles` end-to-end on a one-file bundle so we
/// exercise exactly the same code path the engine uses.
fn parse_via_registry(body: &str) -> Result<RuntimeYaml, EngineError> {
    let bundle = tempdir();
    write_signed_runtime(&bundle, "rt", body);
    let registry = RuntimeRegistry::build_from_bundles(&[bundle], &trust_store())?;
    let rt = registry
        .all()
        .next()
        .expect("at least one runtime when build succeeded");
    Ok(rt.yaml.clone())
}

#[test]
fn parse_runtime_yaml_success() {
    let yaml = parse_via_registry(FULL_RUNTIME_YAML).expect("full YAML parses");
    assert_eq!(yaml.kind, "runtime");
    assert_eq!(yaml.serves, "directive");
    assert_eq!(yaml.default, Some(true));
    assert_eq!(yaml.binary_ref, "bin/{host_triple}/directive_runner");
    assert_eq!(yaml.abi_version, "v1");
    assert_eq!(yaml.required_caps, vec!["rye.read.directive.*".to_string()]);
    assert_eq!(yaml.description.as_deref(), Some("Default directive runtime"));
    let schema = yaml.schema.expect("schema present");
    assert_eq!(schema.envelope, "LaunchEnvelope");
    assert_eq!(schema.result, "RuntimeResult");
}

#[test]
fn parse_runtime_yaml_missing_serves_rejected() {
    let body = "\
kind: runtime
binary_ref: bin/x
abi_version: v1
";
    let err = parse_via_registry(body).unwrap_err();
    assert!(
        matches!(err, EngineError::RuntimeYamlInvalid { .. }),
        "expected RuntimeYamlInvalid, got: {err:?}"
    );
}

#[test]
fn parse_runtime_yaml_missing_binary_ref_rejected() {
    let body = "\
kind: runtime
serves: directive
abi_version: v1
";
    let err = parse_via_registry(body).unwrap_err();
    assert!(
        matches!(err, EngineError::RuntimeYamlInvalid { .. }),
        "expected RuntimeYamlInvalid, got: {err:?}"
    );
}

#[test]
fn parse_runtime_yaml_missing_abi_version_rejected() {
    let body = "\
kind: runtime
serves: directive
binary_ref: bin/x
";
    let err = parse_via_registry(body).unwrap_err();
    assert!(
        matches!(err, EngineError::RuntimeYamlInvalid { .. }),
        "expected RuntimeYamlInvalid, got: {err:?}"
    );
}

#[test]
fn parse_runtime_yaml_wrong_kind_rejected() {
    let body = "\
kind: tool
serves: directive
binary_ref: bin/x
abi_version: v1
";
    let err = parse_via_registry(body).unwrap_err();
    match err {
        EngineError::RuntimeYamlInvalid { reason, .. } => {
            assert!(
                reason.contains("expected `kind: runtime`"),
                "expected wrong-kind reason, got: {reason}"
            );
        }
        other => panic!("expected RuntimeYamlInvalid, got: {other:?}"),
    }
}

// ── Registry build / lookup tests ───────────────────────────────────

#[test]
fn registry_empty_when_no_bundles_have_runtimes() {
    let bundle = tempdir(); // no .ai/runtimes/ subdir
    let registry = RuntimeRegistry::build_from_bundles(&[bundle], &trust_store()).unwrap();
    assert_eq!(registry.all().count(), 0);
    let err = registry.lookup_for("directive").unwrap_err();
    assert!(matches!(err, EngineError::NoRuntimeFor { .. }));
}

#[test]
fn registry_one_runtime_serving_kind_returned() {
    let bundle = tempdir();
    write_signed_runtime(&bundle, "default", MINIMAL_RUNTIME_YAML);
    let registry =
        RuntimeRegistry::build_from_bundles(&[bundle], &trust_store()).unwrap();

    let rt = registry.lookup_for("directive").unwrap();
    assert_eq!(rt.canonical_ref.to_string(), "runtime:default");
    assert_eq!(rt.yaml.serves, "directive");
}

#[test]
fn registry_two_runtimes_one_default_returns_default() {
    let bundle = tempdir();
    write_signed_runtime(
        &bundle,
        "fast",
        "\
kind: runtime
serves: directive
default: true
binary_ref: bin/fast
abi_version: v1
",
    );
    write_signed_runtime(
        &bundle,
        "slow",
        "\
kind: runtime
serves: directive
binary_ref: bin/slow
abi_version: v1
",
    );

    let registry =
        RuntimeRegistry::build_from_bundles(&[bundle], &trust_store()).unwrap();
    let rt = registry.lookup_for("directive").unwrap();
    assert_eq!(rt.canonical_ref.to_string(), "runtime:fast");
    assert_eq!(rt.yaml.default, Some(true));
}

#[test]
fn registry_two_runtimes_both_default_fails_build() {
    let bundle = tempdir();
    write_signed_runtime(
        &bundle,
        "a",
        "\
kind: runtime
serves: directive
default: true
binary_ref: bin/a
abi_version: v1
",
    );
    write_signed_runtime(
        &bundle,
        "b",
        "\
kind: runtime
serves: directive
default: true
binary_ref: bin/b
abi_version: v1
",
    );

    let err = RuntimeRegistry::build_from_bundles(&[bundle], &trust_store()).unwrap_err();
    match err {
        EngineError::MultipleRuntimeDefaults { kind, defaults } => {
            assert_eq!(kind, "directive");
            assert_eq!(defaults.len(), 2);
        }
        other => panic!("expected MultipleRuntimeDefaults, got: {other:?}"),
    }
}

#[test]
fn registry_two_runtimes_neither_default_lookup_fails() {
    let bundle = tempdir();
    write_signed_runtime(
        &bundle,
        "a",
        "\
kind: runtime
serves: directive
binary_ref: bin/a
abi_version: v1
",
    );
    write_signed_runtime(
        &bundle,
        "b",
        "\
kind: runtime
serves: directive
binary_ref: bin/b
abi_version: v1
",
    );

    let registry =
        RuntimeRegistry::build_from_bundles(&[bundle], &trust_store()).unwrap();
    let err = registry.lookup_for("directive").unwrap_err();
    match err {
        EngineError::RuntimeDefaultRequired { kind, candidates } => {
            assert_eq!(kind, "directive");
            assert_eq!(candidates.len(), 2);
        }
        other => panic!("expected RuntimeDefaultRequired, got: {other:?}"),
    }
}

#[test]
fn registry_lookup_by_ref_returns_runtime() {
    let bundle = tempdir();
    write_signed_runtime(&bundle, "default", MINIMAL_RUNTIME_YAML);
    let registry =
        RuntimeRegistry::build_from_bundles(&[bundle], &trust_store()).unwrap();

    let canonical = CanonicalRef::parse("runtime:default").unwrap();
    let rt = registry.lookup_by_ref(&canonical).expect("lookup hit");
    assert_eq!(rt.yaml.binary_ref, "bin/runner");

    let missing = CanonicalRef::parse("runtime:nope").unwrap();
    assert!(registry.lookup_by_ref(&missing).is_none());
}

#[test]
fn registry_tampered_yaml_aborts_build() {
    let bundle = tempdir();
    write_signed_runtime(&bundle, "default", MINIMAL_RUNTIME_YAML);

    // Mutate one byte in the body without re-signing.
    let path = bundle.join(".ai/runtimes/default.yaml");
    let content = fs::read_to_string(&path).unwrap();
    // Append a comment line so the signed content hash no longer matches.
    let tampered = format!("{content}# tampered\n");
    fs::write(&path, tampered).unwrap();

    let err = RuntimeRegistry::build_from_bundles(&[bundle], &trust_store()).unwrap_err();
    match err {
        EngineError::RuntimeYamlInvalid { reason, .. } => {
            assert!(
                reason.contains("content hash mismatch"),
                "expected content hash mismatch, got: {reason}"
            );
        }
        other => panic!("expected RuntimeYamlInvalid, got: {other:?}"),
    }
}
