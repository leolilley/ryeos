use base64::Engine as _;
use ryeos_external_execution::lifecycle_adapter::{
    LifecycleAdapterInvocation, run_lifecycle_adapter,
};
use ryeos_external_execution_contract::{
    LIFECYCLE_ADAPTER_PROTOCOL, LIFECYCLE_CREDENTIAL_FD_ENV, LIFECYCLE_SETTINGS_FD_ENV,
    LifecycleRuntimeProbeRequest, LifecycleRuntimeProbeResponse, LifecycleRuntimeProbeSource,
    canonical_json, from_json_slice_strict,
};

fn adapter() -> lillux::InheritedDescriptorAuthority {
    let path = std::path::Path::new(env!("CARGO_BIN_EXE_ryeos-render-sandbox-lifecycle-adapter"));
    let root = lillux::PinnedDirectory::open(path.parent().unwrap())
        .unwrap()
        .unwrap();
    root.open_pinned_regular(path.file_name().unwrap(), false)
        .unwrap()
        .unwrap()
        .into_inherited_descriptor_path()
        .unwrap()
}

fn invocation(
    with_credential: bool,
    request_materialized: bool,
    probe_materialized: bool,
) -> anyhow::Result<(LifecycleRuntimeProbeRequest, Vec<u8>)> {
    let adapter = adapter();
    let observation = adapter.regular_file_observation()?;
    let artifact_hash = adapter.digest_regular_file_stable_exact(&observation)?;
    let root = lillux::crypto::SigningKey::from_bytes(&[3; 32])
        .verifying_key()
        .to_bytes();
    let public_root = format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(root)
    );
    let settings = canonical_json(&serde_json::json!({
        "schema": 2,
        "owner_id": "owner",
        "plan": "starter",
        "region": "oregon",
        "snapshot_id": "snp-exact",
        "tls_roots_der_base64": ["AA=="]
    }))?;
    let captured_source =
        ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotSource::CapturedProduct {
            product_witness_hash: "1".repeat(64),
        };
    let materialized_source =
        ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotSource::BundleMaterialization {
            materialization_attestation_hash: "1".repeat(64),
            source_coordinate_digest: "2".repeat(64),
            materialization_binding_digest: "3".repeat(64),
        };
    let request = LifecycleRuntimeProbeRequest {
        schema: 2,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        adapter_id: "render-sandbox-early-access".into(),
        adapter_artifact_hash: artifact_hash,
        settings_digest: lillux::sha256_hex(&settings),
        binding_hash: "4".repeat(64),
        qualification_attestation_hash: "a".repeat(64),
        runtime_source: if request_materialized {
            materialized_source.clone()
        } else {
            captured_source.clone()
        },
        account: "account".into(),
        source: LifecycleRuntimeProbeSource {
            manifest_hash: "2".repeat(64),
            owner_executable_sha256: "5".repeat(64),
            controller_root_blob_sha256: lillux::sha256_hex(hex::encode(root).as_bytes()),
            controller_public_root: public_root.clone(),
        },
        probe_evidence: serde_json::json!({
            "schema": 6,
            "runtime_source": if probe_materialized {
                serde_json::to_value(&materialized_source)?
            } else {
                serde_json::to_value(&captured_source)?
            },
            "guest_runtime_manifest_hash": "2".repeat(64),
            "controller_public_root": public_root,
            "owner_id": "owner",
            "account": "account",
            "snapshot_id": "snp-exact",
            "restored_verifier_operation_id": "d".repeat(64),
            "restored_verifier_observation_hash": "e".repeat(64),
            "runtime_snapshot_locator": {
                "schema": ryeos_external_execution_contract::runtime_snapshot::RUNTIME_SNAPSHOT_RESULT_SCHEMA,
                "operation_id": "a".repeat(64),
                "intent_digest": "b".repeat(64),
                "source_occurrence_id": "sbx-source",
                "provider_group_id": "sbg-exact",
                "snapshot_id": "snp-exact",
                "provider_response_sha256": "c".repeat(64),
                "provider_creation_observation": {"schema": 1},
                "adapter_observation_sha256": lillux::sha256_hex(br#"{"schema":1}"#)
            },
            "snapshot_kind": "filesystem",
            "plan": "starter",
            "region": "oregon",
            "binding_hash": "4".repeat(64),
            "restored_tree_manifest_hash": "2".repeat(64),
            "installed_owner_hash": "5".repeat(64),
            "installed_controller_public_root": format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(root)
            ),
            "signed_import_mode": 0o600,
            "guest_package_mode": 0o600,
            "qualification_termination_operation_id": "f".repeat(64),
            "provider_terminal_observation_hash": "8".repeat(64)
        }),
    };
    let request_handle = lillux::sealed_memfd(c"probe-request", &request.canonical_bytes()?)
        .map_err(anyhow::Error::msg)?;
    let settings_handle =
        lillux::sealed_memfd(c"probe-settings", &settings).map_err(anyhow::Error::msg)?;
    let mut env = vec![(
        LIFECYCLE_SETTINGS_FD_ENV.into(),
        settings_handle
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?
            .to_string(),
    )];
    if with_credential {
        env.push((LIFECYCLE_CREDENTIAL_FD_ENV.into(), "99".into()));
    }
    let result = run_lifecycle_adapter(
        &adapter,
        LifecycleAdapterInvocation::VerifyRuntimeProbe,
        &request_handle,
        vec![settings_handle],
        env,
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(15)),
    );
    Ok((request, result?.bytes))
}

#[test]
fn sealed_probe_invokes_exact_adapter_without_contact_authority() {
    let (request, bytes) = invocation(false, false, false).unwrap();
    let response: LifecycleRuntimeProbeResponse = from_json_slice_strict(&bytes, 4096).unwrap();
    response.validate_for(&request).unwrap();
    let (materialized_request, materialized_bytes) = invocation(false, true, true).unwrap();
    let materialized_response: LifecycleRuntimeProbeResponse =
        from_json_slice_strict(&materialized_bytes, 4096).unwrap();
    materialized_response
        .validate_for(&materialized_request)
        .unwrap();
    assert!(invocation(true, false, false).is_err());
    assert!(invocation(false, false, true).is_err());
    assert!(invocation(false, true, false).is_err());
}
