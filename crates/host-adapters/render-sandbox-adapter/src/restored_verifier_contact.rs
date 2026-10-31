//! One-shot restored-guest verifier contact over the bounded Render Sandbox
//! token/proxy transport. This is distinct from supervisor activation.

use std::io::Read as _;

use anyhow::{Result, ensure};
use base64::Engine as _;
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::MonotonicDeadline;
use ryeos_external_execution_contract::canonical_json;
use ryeos_external_execution_contract::restored_runtime_measurement::{
    CONSUMER_VERIFIER_REMOTE_NAME, ConsumerVerifierAdapterObservation,
    RESTORATION_VERIFIER_REMOTE_NAME, RemoteVerificationPurpose,
    RestoredVerifierAdapterObservation, RestoredVerifierAdapterRequest,
    RestoredVerifierAdapterResponse,
};
use ryeos_http_transport::RequestBodySource;

use ryeos_external_execution_contract::UncertaintyClass;

use crate::Settings;
use crate::activation_contact::RenderContact;
use crate::contact_failure::{classified, failure, response_detail};
use crate::provider_spec::ProviderSpec;
use crate::proxy_route::ProxyOperation;

const MAX_UPLOAD_RESPONSE_BYTES: u64 = 16 * 1024;
const MAX_OWNER_RUN_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_OWNER_COMMAND_BYTES: usize = 8192;
const MAX_CONSUMER_COMMAND_BYTES: usize = 9 * 1024;

/// Exact provider command framing only. Selection and challenge validation
/// remain at the shared contract; this grants no contact or qualification.
pub(crate) fn verifier_command(request: &RestoredVerifierAdapterRequest) -> Result<String> {
    request.validate()?;
    let (name, flag, challenge, command_bound) = match &request.intent.purpose {
        RemoteVerificationPurpose::OwnerMeasurement { .. } => (
            RESTORATION_VERIFIER_REMOTE_NAME,
            "--challenge-b64",
            canonical_json(request.intent.owner_challenge()?)?,
            MAX_OWNER_COMMAND_BYTES,
        ),
        RemoteVerificationPurpose::ConsumerRuntime { .. } => (
            CONSUMER_VERIFIER_REMOTE_NAME,
            "--consumer-challenge-b64",
            request.consumer_challenge()?.canonical_bytes()?,
            MAX_CONSUMER_COMMAND_BYTES,
        ),
    };
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge);
    let command = format!(
        "{}/{name} {flag} {encoded}",
        request.intent.remote_upload_directory()?
    );
    ensure!(
        command.len() <= command_bound,
        "restored verifier command exceeds bound"
    );
    Ok(command)
}

/// The caller has already preflighted the signed qualifier, exact sealed tar
/// and finite routes. No failure here permits a second invocation. A complete
/// stream is merely an observation until the daemon joins execution evidence.
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
    let upload_directory = request.intent.remote_upload_directory()?;
    let run_response_bound = match &request.intent.purpose {
        RemoteVerificationPurpose::OwnerMeasurement { .. } => MAX_OWNER_RUN_RESPONSE_BYTES,
        RemoteVerificationPurpose::ConsumerRuntime { .. } => {
            u64::try_from(crate::snapshot_qualification::MAX_CONSUMER_RUN_STREAM_BYTES)?
        }
    };
    // Resolve the entire finite command before any provider contact.
    let command = verifier_command(request)?;
    let contact = RenderContact::new(
        network,
        provider_spec,
        settings,
        &request.occurrence.occurrence_id,
        deadline,
        cancellation,
    )
    .map_err(|error| classified(error, UncertaintyClass::Validation, "contact setup failed"))?;
    let upload_operation = ProxyOperation::UploadFile {
        remote_path: &upload_directory,
    };
    let upload_token = contact.mint(upload_operation, None).map_err(|error| {
        classified(
            error,
            UncertaintyClass::TokenMint,
            "upload token mint failed",
        )
    })?;
    let upload_execution_id = upload_token.execution_id.clone();
    let upload_response = contact
        .send_proxy(
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
        )
        .map_err(|error| classified(error, UncertaintyClass::Transport, "tar upload failed"))?;
    if !(200..300).contains(&upload_response.status) {
        let detail = response_detail(upload_response.status, upload_response.body);
        return Err(failure(
            UncertaintyClass::UploadStatus,
            &format!("tar upload {detail}"),
        ));
    }
    let mut upload_body = Vec::new();
    upload_response
        .body
        .take(MAX_UPLOAD_RESPONSE_BYTES + 1)
        .read_to_end(&mut upload_body)
        .map_err(|error| read_failure(&error, "upload response"))?;
    if upload_body.len() as u64 > MAX_UPLOAD_RESPONSE_BYTES {
        return Err(failure(
            UncertaintyClass::StreamBound,
            "upload response exceeds its bound",
        ));
    }

    let run_operation = ProxyOperation::RunStream;
    let run_token = contact
        .mint(run_operation, Some(&command))
        .map_err(|error| classified(error, UncertaintyClass::TokenMint, "run token mint failed"))?;
    let run_execution_id = run_token.execution_id.clone();
    let run_body = canonical_json(&serde_json::json!({ "command": command }))?;
    let run_response = contact
        .send_proxy(
            run_token,
            run_operation,
            RequestBodySource::from_bytes(run_body),
            "application/json",
            "text/event-stream",
            run_response_bound,
        )
        .map_err(|error| classified(error, UncertaintyClass::Transport, "run request failed"))?;
    if run_response.status != 200 {
        let detail = response_detail(run_response.status, run_response.body);
        return Err(failure(
            UncertaintyClass::RunStatus,
            &format!("run {detail}"),
        ));
    }
    let exact_event_stream = run_response
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("content-type"))
        .count()
        == 1
        && run_response.headers.iter().any(|header| {
            header.name.eq_ignore_ascii_case("content-type")
                && header.value() == b"text/event-stream"
        });
    if !exact_event_stream {
        return Err(failure(
            UncertaintyClass::ContentType,
            "run status 200 without exactly one text/event-stream content type",
        ));
    }
    let mut stream = Vec::new();
    run_response
        .body
        .take(run_response_bound + 1)
        .read_to_end(&mut stream)
        .map_err(|error| read_failure(&error, "run stream"))?;
    if stream.len() as u64 > run_response_bound {
        return Err(failure(
            UncertaintyClass::StreamBound,
            "run stream exceeds its bound",
        ));
    }
    let result = match &request.intent.purpose {
        RemoteVerificationPurpose::OwnerMeasurement { .. } => {
            let parsed = crate::snapshot_qualification::parse_restored_verifier_stream(
                &stream,
                request.intent.owner_challenge()?,
                &request.source_intent,
                &request.locator,
                &request.readiness,
            )
            .map_err(|error| {
                classified(
                    error,
                    UncertaintyClass::Parse,
                    "owner verifier stream did not parse",
                )
            })?;
            RestoredVerifierAdapterResponse::Observed {
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
            }
        }
        RemoteVerificationPurpose::ConsumerRuntime { .. } => {
            let challenge = request.consumer_challenge()?;
            let parsed = crate::snapshot_qualification::parse_consumer_verifier_stream(
                &stream,
                &challenge,
                deadline,
                cancellation,
            )
            .map_err(|error| {
                classified(
                    error,
                    UncertaintyClass::Parse,
                    "consumer verifier stream did not parse",
                )
            })?;
            RestoredVerifierAdapterResponse::ConsumerObserved {
                observation: Box::new(ConsumerVerifierAdapterObservation {
                    schema: 1,
                    operation_id: request.intent.operation_id.clone(),
                    occurrence_id: request.occurrence.occurrence_id.clone(),
                    verifier_artifact_hash: request.intent.verifier_artifact_hash.clone(),
                    challenge_digest: request.intent.consumer_challenge_digest()?,
                    upload_token_execution_id: upload_execution_id,
                    run_token_execution_id: run_execution_id,
                    upload_response_sha256: lillux::sha256_hex(&upload_body),
                    run_stream_sha256: parsed.response_sha256,
                    evidence_sha256: parsed.evidence_sha256,
                    evidence_bytes: parsed.evidence_bytes,
                    contact_deadline_exceeded: false,
                }),
                evidence: parsed.evidence,
            }
        }
    };
    result.validate_for(request).map_err(|error| {
        classified(
            error,
            UncertaintyClass::Validation,
            "verifier response failed validation",
        )
    })?;
    Ok(result)
}

/// A body read that failed: a timeout stays a timeout, a transport-refused
/// body is a bound or framing refusal, anything else is transport.
fn read_failure(error: &std::io::Error, what: &str) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::TimedOut {
        failure(UncertaintyClass::Timeout, &format!("{what} read timed out"))
    } else if ryeos_http_transport::is_response_refusal(error) {
        failure(
            UncertaintyClass::StreamBound,
            &format!("{what} refused by the transport"),
        )
    } else {
        failure(UncertaintyClass::Transport, &format!("{what} read failed"))
    }
}
