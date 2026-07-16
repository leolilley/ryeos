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
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::resolution::TrustClass;
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

fn with_empty_launch_contract(body: &str, serves: &str) -> String {
    format!(
        "{body}launch_contract:\n  primary_allowed_kinds: [{serves}]\n  primary_allowed_spaces: [bundle]\n  primary_allowed_trust: [trusted_bundle]\n  ref_bindings: {{}}\n  preparation:\n    kind: none\n  config_inputs: {{}}\n  secret_policy:\n    max_requirements: 0\n    allowed_names: []\n  required_runtime_data: []\n  runtime_facts: {{}}\n"
    )
}

/// Build a `KindRegistry` containing a single executable `directive`
/// kind whose execution block delegates to the runtime registry. Used
/// by parser tests so the post-parse `RuntimeServesUnknownKind` /
/// `RuntimeServesKindNoExecution` cross-checks (ε.2) accept the
/// fixture runtime YAMLs that all serve `directive`.
fn directive_kind_registry(bundle_root: &Path) -> KindRegistry {
    let kinds_dir = bundle_root.join(".ai/node/engine/kinds/directive");
    fs::create_dir_all(&kinds_dir).unwrap();
    let schema_body = r##"category: "engine/kinds/directive"
version: "1.0.0"
location:
  directory: directives
resolution: []
effective_trust:
  include_references: false
execution:
  delegate:
    via: runtime_registry
  thread_profile:
    name: directive_run
    root_executable: true
    supports_interrupt: false
    supports_continuation: false
formats:
  - extensions: [".yaml"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: "#"
composer: handler:ryeos/core/identity
composed_value_contract:
  root_type: mapping
  required: {}
metadata:
  rules: {}
"##;
    let signed = lillux::signature::sign_content(schema_body, &signing_key(), "#", None);
    fs::write(kinds_dir.join("directive.kind-schema.yaml"), signed).unwrap();
    let kinds_search = bundle_root.join(".ai/node/engine/kinds");
    KindRegistry::load_base(&[kinds_search], &trust_store()).unwrap()
}

const FULL_RUNTIME_YAML: &str = "\
kind: runtime
serves: directive
default: true
binary_ref: bin/{host_triple}/directive_runner
abi_version: v1
required_caps:
  - ryeos.read.directive.*
launch_contract:
  primary_allowed_kinds: [directive]
  primary_allowed_spaces: [bundle, project]
  primary_allowed_trust: [trusted_bundle, trusted_project]
  ref_bindings:
    context:
      required: false
      allowed_kinds: [directive]
      allowed_spaces: [bundle, project]
      allowed_trust: [trusted_bundle, trusted_project]
  preparation:
    kind: none
  config_inputs: {}
  secret_policy:
    max_requirements: 0
    allowed_names: []
  required_runtime_data: []
  runtime_facts: {}
description: Default directive runtime
";

const MINIMAL_RUNTIME_YAML: &str = "\
kind: runtime
serves: directive
binary_ref: bin/test-triple/runner
abi_version: v1
launch_contract:
  primary_allowed_kinds: [directive]
  primary_allowed_spaces: [bundle]
  primary_allowed_trust: [trusted_bundle]
  ref_bindings: {}
  preparation:
    kind: none
  config_inputs: {}
  secret_policy:
    max_requirements: 0
    allowed_names: []
  required_runtime_data: []
  runtime_facts: {}
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
    let kinds = directive_kind_registry(&bundle);
    let registry = RuntimeRegistry::build_from_bundles(
        &[(bundle.clone(), TrustClass::TrustedBundle)],
        &trust_store(),
        &kinds,
    )?;
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
    assert_eq!(
        yaml.required_caps,
        vec!["ryeos.read.directive.*".to_string()]
    );
    assert_eq!(
        yaml.launch_contract.primary_allowed_kinds,
        vec!["directive".to_string()]
    );
    assert!(yaml.launch_contract.ref_bindings.contains_key("context"));
    assert_eq!(
        yaml.description.as_deref(),
        Some("Default directive runtime")
    );
}

#[test]
fn parse_runtime_yaml_missing_serves_rejected() {
    let body = with_empty_launch_contract(
        "\
kind: runtime
binary_ref: bin/test-triple/x
abi_version: v1
",
        "directive",
    );
    let err = parse_via_registry(&body).unwrap_err();
    assert!(
        matches!(err, EngineError::RuntimeYamlInvalid { .. }),
        "expected RuntimeYamlInvalid, got: {err:?}"
    );
}

#[test]
fn parse_runtime_yaml_missing_binary_ref_rejected() {
    let body = with_empty_launch_contract(
        "\
kind: runtime
serves: directive
abi_version: v1
",
        "directive",
    );
    let err = parse_via_registry(&body).unwrap_err();
    assert!(
        matches!(err, EngineError::RuntimeYamlInvalid { .. }),
        "expected RuntimeYamlInvalid, got: {err:?}"
    );
}

#[test]
fn parse_runtime_yaml_missing_abi_version_rejected() {
    let body = with_empty_launch_contract(
        "\
kind: runtime
serves: directive
binary_ref: bin/test-triple/x
",
        "directive",
    );
    let err = parse_via_registry(&body).unwrap_err();
    assert!(
        matches!(err, EngineError::RuntimeYamlInvalid { .. }),
        "expected RuntimeYamlInvalid, got: {err:?}"
    );
}

#[test]
fn parse_runtime_yaml_wrong_kind_rejected() {
    let body = with_empty_launch_contract(
        "\
kind: tool
serves: directive
binary_ref: bin/test-triple/x
abi_version: v1
",
        "directive",
    );
    let err = parse_via_registry(&body).unwrap_err();
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
    let registry = RuntimeRegistry::build_from_bundles(
        &[(bundle.clone(), TrustClass::TrustedBundle)],
        &trust_store(),
        &directive_kind_registry(&bundle),
    )
    .unwrap();
    assert_eq!(registry.all().count(), 0);
    let err = registry.lookup_for("directive").unwrap_err();
    assert!(matches!(err, EngineError::NoRuntimeFor { .. }));
}

#[test]
fn registry_one_runtime_serving_kind_returned() {
    let bundle = tempdir();
    write_signed_runtime(&bundle, "default", MINIMAL_RUNTIME_YAML);
    let registry = RuntimeRegistry::build_from_bundles(
        &[(bundle.clone(), TrustClass::TrustedBundle)],
        &trust_store(),
        &directive_kind_registry(&bundle),
    )
    .unwrap();

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
        &with_empty_launch_contract(
            "\
kind: runtime
serves: directive
default: true
binary_ref: bin/test-triple/fast
abi_version: v1
",
            "directive",
        ),
    );
    write_signed_runtime(
        &bundle,
        "slow",
        &with_empty_launch_contract(
            "\
kind: runtime
serves: directive
binary_ref: bin/test-triple/slow
abi_version: v1
",
            "directive",
        ),
    );

    let registry = RuntimeRegistry::build_from_bundles(
        &[(bundle.clone(), TrustClass::TrustedBundle)],
        &trust_store(),
        &directive_kind_registry(&bundle),
    )
    .unwrap();
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
        &with_empty_launch_contract(
            "\
kind: runtime
serves: directive
default: true
binary_ref: bin/test-triple/a
abi_version: v1
",
            "directive",
        ),
    );
    write_signed_runtime(
        &bundle,
        "b",
        &with_empty_launch_contract(
            "\
kind: runtime
serves: directive
default: true
binary_ref: bin/test-triple/b
abi_version: v1
",
            "directive",
        ),
    );

    let err = RuntimeRegistry::build_from_bundles(
        &[(bundle.clone(), TrustClass::TrustedBundle)],
        &trust_store(),
        &directive_kind_registry(&bundle),
    )
    .unwrap_err();
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
        &with_empty_launch_contract(
            "\
kind: runtime
serves: directive
binary_ref: bin/test-triple/a
abi_version: v1
",
            "directive",
        ),
    );
    write_signed_runtime(
        &bundle,
        "b",
        &with_empty_launch_contract(
            "\
kind: runtime
serves: directive
binary_ref: bin/test-triple/b
abi_version: v1
",
            "directive",
        ),
    );

    let registry = RuntimeRegistry::build_from_bundles(
        &[(bundle.clone(), TrustClass::TrustedBundle)],
        &trust_store(),
        &directive_kind_registry(&bundle),
    )
    .unwrap();
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
    let registry = RuntimeRegistry::build_from_bundles(
        &[(bundle.clone(), TrustClass::TrustedBundle)],
        &trust_store(),
        &directive_kind_registry(&bundle),
    )
    .unwrap();

    let canonical = CanonicalRef::parse("runtime:default").unwrap();
    let rt = registry.lookup_by_ref(&canonical).expect("lookup hit");
    assert_eq!(rt.yaml.binary_ref, "bin/test-triple/runner");

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

    let err = RuntimeRegistry::build_from_bundles(
        &[(bundle.clone(), TrustClass::TrustedBundle)],
        &trust_store(),
        &directive_kind_registry(&bundle),
    )
    .unwrap_err();
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

/// A runtime registry validates executable kind membership without imposing a
/// built-in protocol ref. The signed kind schema remains the protocol authority.
#[test]
fn runtime_serves_kind_uses_schema_declared_protocol() {
    let bundle = tempdir();

    // Write a kind schema whose protocol is intentionally not a built-in
    // runtime descriptor. Registry construction must not rewrite that choice.
    let kinds_dir = bundle.join(".ai/node/engine/kinds/fake_kind");
    fs::create_dir_all(&kinds_dir).unwrap();
    let schema_body = r##"category: "engine/kinds/fake_kind"
version: "1.0.0"
location:
  directory: fake_items
resolution: []
effective_trust:
  include_references: false
execution:
  terminator:
    kind: subprocess
    protocol: protocol:ryeos/core/opaque
  thread_profile:
    name: fake_run
    root_executable: true
    supports_interrupt: false
    supports_continuation: false
formats:
  - extensions: [".yaml"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: "#"
composer: handler:ryeos/core/identity
composed_value_contract:
  root_type: mapping
  required: {}
metadata:
  rules: {}
"##;
    let sk = signing_key();
    let vk = sk.verifying_key();
    let fp = compute_fingerprint(&vk);
    let signed = lillux::signature::sign_content(schema_body, &sk, "#", None);
    fs::write(kinds_dir.join("fake_kind.kind-schema.yaml"), signed).unwrap();

    // Trust the test signing key for kind-schema loading.
    let kinds_ts = TrustStore::from_signers(vec![TrustedSigner {
        fingerprint: fp,
        verifying_key: vk,
        label: None,
    }]);
    let kinds_search = bundle.join(".ai/node/engine/kinds");
    let kinds = KindRegistry::load_base(&[kinds_search], &kinds_ts).unwrap();

    // Write a runtime YAML that serves "fake_kind".
    write_signed_runtime(
        &bundle,
        "wrong-protocol-rt",
        &with_empty_launch_contract(
            r#"kind: runtime
serves: fake_kind
binary_ref: bin/x86_64-unknown-linux-gnu/fake-binary
abi_version: "v1"
"#,
            "fake_kind",
        ),
    );

    let registry = RuntimeRegistry::build_from_bundles(
        &[(bundle.clone(), TrustClass::TrustedBundle)],
        &trust_store(),
        &kinds,
    )
    .unwrap();
    assert!(registry.lookup_for("fake_kind").is_ok_and(|runtime| runtime
        .canonical_ref
        .to_string()
        .contains("wrong-protocol-rt")));
}

/// ε: runtime YAML whose `serves` kind has no execution block at all
/// must fail boot — a runtime that serves a non-executable kind is a
/// catalog mistake the daemon refuses to silently carry forward.
#[test]
fn runtime_serves_kind_without_execution_fails() {
    let bundle = tempdir();

    // Write a kind schema for "no_exec_kind" with no execution block.
    let kinds_dir = bundle.join(".ai/node/engine/kinds/no_exec_kind");
    fs::create_dir_all(&kinds_dir).unwrap();
    let schema_body = r##"category: "engine/kinds/no_exec_kind"
version: "1.0.0"
location:
  directory: no_exec_items
effective_trust:
  include_references: false
resolution: []
formats:
  - extensions: [".yaml"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: "#"
composer: handler:ryeos/core/identity
composed_value_contract:
  root_type: mapping
  required: {}
metadata:
  rules: {}
"##;
    let sk = signing_key();
    let vk = sk.verifying_key();
    let fp = compute_fingerprint(&vk);
    let signed = lillux::signature::sign_content(schema_body, &sk, "#", None);
    fs::write(kinds_dir.join("no_exec_kind.kind-schema.yaml"), signed).unwrap();

    let kinds_ts = TrustStore::from_signers(vec![TrustedSigner {
        fingerprint: fp,
        verifying_key: vk,
        label: None,
    }]);
    let kinds_search = bundle.join(".ai/node/engine/kinds");
    let kinds = KindRegistry::load_base(&[kinds_search], &kinds_ts).unwrap();

    write_signed_runtime(
        &bundle,
        "no-exec-rt",
        &with_empty_launch_contract(
            r#"kind: runtime
serves: no_exec_kind
binary_ref: bin/x86_64-unknown-linux-gnu/fake-binary
abi_version: "v1"
"#,
            "no_exec_kind",
        ),
    );

    let err = RuntimeRegistry::build_from_bundles(
        &[(bundle.clone(), TrustClass::TrustedBundle)],
        &trust_store(),
        &kinds,
    )
    .unwrap_err();
    match err {
        EngineError::RuntimeServesKindNoExecution { kind, runtime } => {
            assert_eq!(kind, "no_exec_kind");
            assert!(runtime.contains("no-exec-rt"));
        }
        other => panic!("expected RuntimeServesKindNoExecution, got: {other:?}"),
    }
}
