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

fn invocation(with_credential: bool) -> anyhow::Result<(LifecycleRuntimeProbeRequest, Vec<u8>)> {
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
    let request = LifecycleRuntimeProbeRequest {
        schema: 1,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        adapter_id: "render-sandbox-early-access".into(),
        adapter_artifact_hash: artifact_hash,
        settings_digest: lillux::sha256_hex(&settings),
        binding_hash: "4".repeat(64),
        qualification_attestation_hash: "a".repeat(64),
        product_witness_hash: "1".repeat(64),
        account: "account".into(),
        source: LifecycleRuntimeProbeSource {
            manifest_hash: "2".repeat(64),
            owner_executable_sha256: "5".repeat(64),
            controller_root_blob_sha256: lillux::sha256_hex(hex::encode(root).as_bytes()),
            controller_public_root: public_root.clone(),
        },
        probe_evidence: serde_json::json!({
            "schema": 1,
            "product_witness_hash": "1".repeat(64),
            "guest_runtime_manifest_hash": "2".repeat(64),
            "controller_public_root": public_root,
            "owner_id": "owner",
            "account": "account",
            "snapshot_id": "snp-exact",
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
            "lost_stream_survival_evidence_hash": "6".repeat(64),
            "authenticated_ready_evidence_hash": "7".repeat(64),
            "whole_guest_termination_evidence_hash": "8".repeat(64),
            "writer_exclusion_evidence_hash": "9".repeat(64)
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
    let (request, bytes) = invocation(false).unwrap();
    let response: LifecycleRuntimeProbeResponse = from_json_slice_strict(&bytes, 4096).unwrap();
    response.validate_for(&request).unwrap();
    assert!(invocation(true).is_err());
}
