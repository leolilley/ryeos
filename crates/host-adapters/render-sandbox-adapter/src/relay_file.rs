//! Exact sealed relay file effect. The daemon owns the durable claim and this
//! adapter reports only provider transport; no channel meaning is inferred.

use anyhow::{Result, ensure};
use base64::Engine as _;
use lillux::network::NetworkCancellation;
use ryeos_external_execution_contract::relay_file::{
    LIFECYCLE_RELAY_UPLOAD_FD_ENV, MAX_RELAY_REQUEST_BYTES, MAX_RELAY_RESPONSE_BYTES,
    RelayFileAdapterRequest, RelayFileAdapterResponse, RelayFileEffect, RelayFileKind,
};
use ryeos_external_execution_contract::{
    LIFECYCLE_BOOTSTRAP_FD_ENV, LIFECYCLE_PROVIDER_SPEC_FD_ENV, LIFECYCLE_PROVIDER_SPEC_SHA256_ENV,
    LIFECYCLE_REQUEST_FD_ENV, LIFECYCLE_SETTINGS_FD_ENV, MAX_LIFECYCLE_PROVIDER_SPEC_BYTES,
    from_json_slice_strict,
};
use ryeos_state::external_execution::transport::{
    ExternalSupervisorBootstrap, MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
};

use super::{
    MAX_SETTINGS_BYTES, Settings, network_context_from_captured_inputs, operation_deadline,
    parse_provider_spec_bytes, read_sealed_env, request_deadline, validate_settings,
    write_response_bounded,
};
use crate::relay_contact::RenderRelayFileContact;

fn validate_bootstrap(
    request: &RelayFileAdapterRequest,
    bootstrap: &ExternalSupervisorBootstrap,
) -> Result<()> {
    bootstrap.validate()?;
    let relay = bootstrap.controller.relay()?;
    let expected_deadline = if request.kind == RelayFileKind::Attach {
        bootstrap.attachment_deadline_ms
    } else {
        bootstrap
            .attachment_deadline_ms
            .checked_add(i64::from(bootstrap.execution_timeout_seconds) * 1_000)
            .and_then(|ms| {
                ms.checked_add(i64::from(bootstrap.post_execution_timeout_seconds) * 1_000)
            })
            .ok_or_else(|| anyhow::anyhow!("relay exchange deadline overflow"))?
    };
    ensure!(
        (request.kind != RelayFileKind::Exchange || request.sequence <= relay.maximum_frames)
            && request.placement_thread_id == bootstrap.placement_thread_id
            && request.occurrence_id == bootstrap.occurrence_id
            && request.allocation_request_digest == bootstrap.allocation_request_digest
            && request.bootstrap_digest == bootstrap.digest()?
            && request.binding_hash == bootstrap.execution_binding_hash
            && request.mailbox_root == bootstrap.relay_mailbox_root()?
            && request.maximum_file_bytes == relay.maximum_file_bytes
            && request.deadline_ms == expected_deadline,
        "relay file request differs from its original sealed bootstrap"
    );
    // Request validation derives the sole permissible filename from this
    // mailbox root, direction and sequence. No caller path is used directly.
    request.validate()
}

pub(super) fn operate() -> Result<()> {
    let enclosing_deadline = operation_deadline()?;
    let cancellation = NetworkCancellation::default();
    let _signal_cancellation = super::SignalCancellation::install(cancellation.clone())?;
    let request_bytes = read_sealed_env(LIFECYCLE_REQUEST_FD_ENV, MAX_RELAY_REQUEST_BYTES)?;
    let request: RelayFileAdapterRequest =
        from_json_slice_strict(&request_bytes, MAX_RELAY_REQUEST_BYTES)?;
    ensure!(
        request.canonical_bytes()? == request_bytes,
        "relay file request is noncanonical"
    );
    let bootstrap_bytes = read_sealed_env(
        LIFECYCLE_BOOTSTRAP_FD_ENV,
        MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
    )?;
    let bootstrap: ExternalSupervisorBootstrap =
        from_json_slice_strict(&bootstrap_bytes, MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES)?;
    ensure!(
        bootstrap.canonical_bytes()? == bootstrap_bytes,
        "relay original bootstrap is noncanonical"
    );
    validate_bootstrap(&request, &bootstrap)?;
    let deadline = request_deadline(request.deadline_ms, enclosing_deadline)?;
    let provider_spec_bytes = read_sealed_env(
        LIFECYCLE_PROVIDER_SPEC_FD_ENV,
        usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )?;
    let provider_spec_sha256 = std::env::var(LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)?;
    let schema_digest = lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json"));
    let (provider_spec, _) =
        parse_provider_spec_bytes(&provider_spec_bytes, &provider_spec_sha256, &schema_digest)?;
    let settings_bytes = read_sealed_env(LIFECYCLE_SETTINGS_FD_ENV, MAX_SETTINGS_BYTES)?;
    ensure!(
        lillux::sha256_hex(&settings_bytes) == request.settings_digest,
        "relay settings changed their signed identity"
    );
    let settings_value: serde_json::Value =
        from_json_slice_strict(&settings_bytes, MAX_SETTINGS_BYTES)?;
    ensure!(
        lillux::canonical_json(&settings_value)?.as_bytes() == settings_bytes,
        "relay settings are noncanonical"
    );
    let settings: Settings = serde_json::from_value(settings_value)?;
    validate_settings(&settings)?;
    ensure!(
        settings.owner_id == bootstrap.controller.relay()?.owner_workspace_id,
        "relay Render owner differs from original signed workspace"
    );
    let upload_bytes = match request.effect {
        RelayFileEffect::UploadOnce => {
            let bytes = read_sealed_env(
                LIFECYCLE_RELAY_UPLOAD_FD_ENV,
                request.maximum_file_bytes as usize,
            )?;
            ensure!(
                request.expected_bytes == Some(bytes.len() as u64)
                    && request.expected_sha256.as_deref()
                        == Some(lillux::sha256_hex(&bytes).as_str()),
                "relay upload changed its committed byte identity"
            );
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            ensure!(
                lillux::canonical_json(&value)?.as_bytes() == bytes,
                "relay upload is not canonical JSON"
            );
            Some(bytes)
        }
        RelayFileEffect::Download => {
            ensure!(
                std::env::var_os(LIFECYCLE_RELAY_UPLOAD_FD_ENV).is_none(),
                "relay download received upload authority"
            );
            None
        }
    };
    let network = network_context_from_captured_inputs()?;
    let mut contact = RenderRelayFileContact::new(
        &network,
        &provider_spec,
        &settings,
        &request.occurrence_id,
        &request.remote_path,
        deadline,
        &cancellation,
    )?;
    let request_digest = lillux::sha256_hex(&request_bytes);
    let response = match request.effect {
        RelayFileEffect::UploadOnce => {
            let bytes = upload_bytes
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("relay upload lost verified bytes"))?;
            contact.upload_once(
                bytes,
                request.expected_sha256.as_deref().unwrap_or_default(),
            )?;
            RelayFileAdapterResponse::UploadTransportObserved { request_digest }
        }
        RelayFileEffect::Download => {
            let bytes = contact.download_bounded(request.maximum_file_bytes)?;
            RelayFileAdapterResponse::Downloaded {
                request_digest,
                bytes_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                sha256: lillux::sha256_hex(&bytes),
                bytes: bytes.len() as u64,
            }
        }
    };
    response.validate_for(&request, &lillux::sha256_hex(&request_bytes))?;
    write_response_bounded(&response, MAX_RELAY_RESPONSE_BYTES - 1)
}
