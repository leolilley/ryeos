//! One-shot restored-guest verifier contact over the bounded Render Sandbox
//! token/proxy transport. This is distinct from supervisor activation.

use std::io::Read as _;

use anyhow::{Result, ensure};
use base64::Engine as _;
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::MonotonicDeadline;
use ryeos_external_execution_contract::canonical_json;
use ryeos_external_execution_contract::restored_runtime_measurement::{
    CONSUMER_VERIFIER_REMOTE_NAME, RESTORATION_VERIFIER_REMOTE_DIRECTORY,
    RESTORATION_VERIFIER_REMOTE_NAME, RemoteVerificationPurpose,
    RestoredVerifierAdapterObservation, RestoredVerifierAdapterRequest,
    RestoredVerifierAdapterResponse,
};
use ryeos_http_transport::RequestBodySource;

use crate::Settings;
use crate::activation_contact::RenderContact;
use crate::provider_spec::ProviderSpec;
use crate::proxy_route::ProxyOperation;

const MAX_UPLOAD_RESPONSE_BYTES: u64 = 16 * 1024;
const MAX_RUN_RESPONSE_BYTES: u64 = 64 * 1024;

/// Exact provider command framing only. Selection and challenge validation
/// remain at the shared contract; this grants no contact or qualification.
pub(crate) fn verifier_command(request: &RestoredVerifierAdapterRequest) -> Result<String> {
    request.validate()?;
    let (name, flag, challenge) = match &request.intent.purpose {
        RemoteVerificationPurpose::OwnerMeasurement { .. } => (
            RESTORATION_VERIFIER_REMOTE_NAME,
            "--challenge-b64",
            canonical_json(request.intent.owner_challenge()?)?,
        ),
        RemoteVerificationPurpose::ConsumerRuntime { .. } => (
            CONSUMER_VERIFIER_REMOTE_NAME,
            "--consumer-challenge-b64",
            canonical_json(&request.consumer_challenge()?)?,
        ),
    };
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge);
    let command = format!(
        "{}/{name} {flag} {encoded}",
        request.intent.remote_upload_directory()?
    );
    ensure!(
        command.len() <= 8192,
        "restored verifier command exceeds bound"
    );
    Ok(command)
}

/// The caller has already preflighted the signed qualifier, exact sealed tar
/// and finite routes. No failure here permits a second invocation. A complete
/// stream is merely an observation until the daemon joins execution evidence.
#[allow(clippy::too_many_arguments)]
pub(crate) fn first_contact(
    network: &NetworkContext,
    provider_spec: &ProviderSpec,
    settings: &Settings,
    request: &RestoredVerifierAdapterRequest,
    upload: &lillux::InheritedDescriptorAuthority,
    deadline: MonotonicDeadline,
    cancellation: &NetworkCancellation,
) -> Result<RestoredVerifierAdapterResponse> {
    request.validate()?;
    // Consumer request decoding is not activation of a guest verifier mode.
    // Refuse this lane before token minting/upload until its native protocol
    // and settlement/evidence join are installed.
    request.intent.owner_challenge()?;
    // Resolve the entire finite command before any provider contact.
    let command = verifier_command(request)?;
    let contact = RenderContact::new(
        network,
        provider_spec,
        settings,
        &request.occurrence.occurrence_id,
        deadline,
        cancellation,
    )?;
    let upload_operation = ProxyOperation::UploadFile {
        remote_path: RESTORATION_VERIFIER_REMOTE_DIRECTORY,
    };
    let upload_token = contact.mint(upload_operation, None)?;
    let upload_execution_id = upload_token.execution_id.clone();
    let upload_response = contact.send_proxy(
        upload_token,
        upload_operation,
        RequestBodySource::from_inherited_regular_file(
            upload.clone(),
            request.upload_bytes,
            request.upload_sha256.clone(),
        ),
        "application/x-tar",
        "application/json",
        MAX_UPLOAD_RESPONSE_BYTES,
    )?;
    ensure!(
        (200..300).contains(&upload_response.status),
        "restored verifier tar upload did not succeed"
    );
    let mut upload_body = Vec::new();
    upload_response
        .body
        .take(MAX_UPLOAD_RESPONSE_BYTES + 1)
        .read_to_end(&mut upload_body)?;
    ensure!(
        upload_body.len() as u64 <= MAX_UPLOAD_RESPONSE_BYTES,
        "restored verifier upload response exceeds bound"
    );

    let run_operation = ProxyOperation::RunStream;
    let run_token = contact.mint(run_operation, Some(&command))?;
    let run_execution_id = run_token.execution_id.clone();
    let run_body = canonical_json(&serde_json::json!({ "command": command }))?;
    let run_response = contact.send_proxy(
        run_token,
        run_operation,
        RequestBodySource::from_bytes(run_body),
        "application/json",
        "text/event-stream",
        MAX_RUN_RESPONSE_BYTES,
    )?;
    ensure!(
        run_response.status == 200
            && run_response
                .headers
                .iter()
                .filter(|header| header.name.eq_ignore_ascii_case("content-type"))
                .count()
                == 1
            && run_response.headers.iter().any(|header| {
                header.name.eq_ignore_ascii_case("content-type")
                    && header.value() == b"text/event-stream"
            }),
        "restored verifier run did not return an exact event stream"
    );
    let mut stream = Vec::new();
    run_response
        .body
        .take(MAX_RUN_RESPONSE_BYTES + 1)
        .read_to_end(&mut stream)?;
    ensure!(
        stream.len() as u64 <= MAX_RUN_RESPONSE_BYTES,
        "restored verifier stream exceeds bound"
    );
    let parsed = crate::snapshot_qualification::parse_restored_verifier_stream(
        &stream,
        request.intent.owner_challenge()?,
        &request.source_intent,
        &request.locator,
        &request.readiness,
    )?;
    let result = RestoredVerifierAdapterResponse::Observed {
        observation: Box::new(RestoredVerifierAdapterObservation {
            schema: 1,
            operation_id: request.intent.operation_id.clone(),
            occurrence_id: request.occurrence.occurrence_id.clone(),
            upload_token_execution_id: upload_execution_id,
            run_token_execution_id: run_execution_id,
            upload_response_sha256: lillux::sha256_hex(&upload_body),
            run_stream_sha256: parsed.response_sha256,
            measurement: parsed.measurement,
            contact_deadline_exceeded: false,
        }),
    };
    result.validate_for(request)?;
    Ok(result)
}
