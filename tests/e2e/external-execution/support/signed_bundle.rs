//! Exact signed synthetic external-adapter bundle shared by protocol and daemon tests.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use lillux::crypto::SigningKey;

pub struct SyntheticExternalArtifacts<'a> {
    pub adapter: &'a Path,
    pub supervisor: &'a Path,
    pub launcher: &'a Path,
    pub connector: &'a Path,
    pub configuration: &'a Path,
}

pub fn install_signed_test_bundle(
    root: &Path,
    connector_new_group: bool,
    key: &SigningKey,
    artifacts: &SyntheticExternalArtifacts<'_>,
) -> (PathBuf, ryeos_engine::trust::TrustStore, PathBuf) {
    let bundle = root.join("synthetic-external");
    let ai = bundle.join(ryeos_engine::AI_DIR);
    let target = lillux::platform::current_binary_target().unwrap();
    let bin = ai.join("bin").join(target);
    std::fs::create_dir_all(&bin).unwrap();
    let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
    let cas = lillux::cas::CasStore::new(ai.join("objects"));
    let mut item_source_hashes = serde_json::Map::new();
    let executables = [
        (
            "ryeos-synthetic-external-lifecycle-adapter",
            artifacts.adapter,
        ),
        (
            "ryeos-synthetic-external-candidate-supervisor",
            artifacts.supervisor,
        ),
        (
            "ryeos-synthetic-external-candidate-launcher",
            artifacts.launcher,
        ),
        (
            "ryeos-synthetic-external-candidate-connector",
            artifacts.connector,
        ),
        (
            "ryeos-synthetic-codex-external-configuration",
            artifacts.configuration,
        ),
    ];
    let mut adapter_path = None;
    for (name, source) in executables {
        let bytes = std::fs::read(source).unwrap();
        let destination = bin.join(name);
        std::fs::write(&destination, &bytes).unwrap();
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755)).unwrap();
        if name == "ryeos-synthetic-external-lifecycle-adapter" {
            adapter_path = Some(destination.clone());
        }
        let content_blob_hash = cas.store_blob(&bytes).unwrap();
        let item_ref = format!("bin/{target}/{name}");
        let item_source = serde_json::json!({
            "content_blob_hash": content_blob_hash,
            "integrity": format!("sha256:{content_blob_hash}"),
            "item_ref": item_ref,
            "kind": "item_source",
            "mode": 0o755,
            "signature_info": null,
        });
        let item_source_hash = cas.store_object(&item_source).unwrap();
        let sidecar = lillux::signature::sign_content(
            &lillux::cas::canonical_json(&item_source).unwrap(),
            key,
            "#",
            None,
        );
        std::fs::write(
            destination.with_file_name(format!("{name}.item_source.json")),
            sidecar,
        )
        .unwrap();
        item_source_hashes.insert(item_ref, serde_json::Value::String(item_source_hash));
    }
    let provider_spec_bytes = br#"{"schema":1,"operations":[]}"#;
    let provider_spec_path = bundle.join("lifecycle/provider.json");
    std::fs::create_dir_all(provider_spec_path.parent().unwrap()).unwrap();
    std::fs::write(&provider_spec_path, provider_spec_bytes).unwrap();
    std::fs::set_permissions(
        &provider_spec_path,
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let provider_spec_sha256 = lillux::sha256_hex(provider_spec_bytes);
    let executor_manifest = serde_json::json!({
        "item_source_hashes": item_source_hashes,
        "kind": "source_manifest",
    });
    let executor_manifest_hash = cas.store_object(&executor_manifest).unwrap();
    let manifest_ref = ai.join("refs/bundles/manifest");
    std::fs::create_dir_all(manifest_ref.parent().unwrap()).unwrap();
    std::fs::write(
        manifest_ref,
        lillux::signature::sign_content(
            &format!(
                "{}\n{executor_manifest_hash}\n",
                ryeos_engine::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN
            ),
            key,
            "#",
            None,
        ),
    )
    .unwrap();
    let bundle_manifest = format!(
        r#"name: synthetic-external
version: '1.0'
description: test-only signed external lifecycle fixture
provides_kinds: []
requires_kinds: []
uses_kinds: []
external_lifecycle_adapters:
  - id: synthetic-local
    protocol: ryeos.external-execution.lifecycle-adapter.v2
    targets: [{target}]
    adapter: ryeos-synthetic-external-lifecycle-adapter
    supervisor: ryeos-synthetic-external-candidate-supervisor
    launcher: ryeos-synthetic-external-candidate-launcher
    provider_spec:
      path: lifecycle/provider.json
      sha256: {provider_spec_sha256}
    settings_schema_digest: '{}'
    capabilities:
      - exact_allocation_reconciliation
      - authoritative_no_occurrence
      - exact_activation_reconciliation
      - idempotent_termination
      - exact_terminal_observation
external_providers:
  - id: codex-hosted
    protocol: ryeos.external-execution.provider-configuration.v1
    targets: [{target}]
    connector: ryeos-synthetic-external-candidate-connector
    connector_process_group: {connector_process_group}
    configuration_adapter: ryeos-synthetic-codex-external-configuration
    configuration_destination: environments.toml
"#,
        "9".repeat(64),
        connector_process_group = if connector_new_group {
            "new"
        } else {
            "inherited"
        },
        provider_spec_sha256 = provider_spec_sha256,
    );
    std::fs::write(
        ai.join("manifest.yaml"),
        lillux::signature::sign_content(&bundle_manifest, key, "#", None),
    )
    .unwrap();
    let trust =
        ryeos_engine::trust::TrustStore::from_signers(vec![ryeos_engine::trust::TrustedSigner {
            fingerprint,
            verifying_key: key.verifying_key(),
            label: Some("synthetic-external-test".into()),
        }]);
    (bundle, trust, adapter_path.unwrap())
}
