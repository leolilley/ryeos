//! Product-side reading of one protected, retained Q Worker transcript.
//!
//! The manifest and chunk checks prove exact bytes and their input coordinate.
//! They do not interpret Worker success or establish a qualification claim.

use anyhow::{Context as _, Result, ensure};
use lillux::time::MonotonicDeadline;
use ryeos_runtime::callback_uds::UdsRuntimeClient;
use ryeos_state::objects::{
    QualificationWorkerTranscriptChunkResponse, QualificationWorkerTranscriptChunkSelection,
    QualificationWorkerTranscriptReference, QualificationWorkerTranscriptStream,
    canonical_value_digest,
};
use serde::Deserialize;

use crate::consumer_record::ConsumerInputRecord;

pub struct VerifiedConsumerWorkerTranscript {
    pub reference: QualificationWorkerTranscriptReference,
    pub stream: QualificationWorkerTranscriptStream,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum ChunkObservationResponse {
    TranscriptChunk {
        chunk: QualificationWorkerTranscriptChunkResponse,
    },
}

/// Fetch only the ordered chunks committed by the protected reference. The
/// caller must supply its original retained record and non-renewable deadline.
/// A missing/malformed response fails this read; it never authorizes START,
/// replacement bytes, a different manifest, or another provider operation.
pub async fn read_consumer_worker_transcript(
    client: &UdsRuntimeClient,
    original: &ConsumerInputRecord,
    reference: QualificationWorkerTranscriptReference,
    deadline: MonotonicDeadline,
) -> Result<VerifiedConsumerWorkerTranscript> {
    ensure!(
        !deadline.has_elapsed(),
        "Q transcript caller deadline expired"
    );
    validate_reference_for_record(&reference, original)?;
    let coordinate = serde_json::to_value(&original.coordinate)?;
    let mut chunks = Vec::with_capacity(reference.manifest.chunks.len());
    for index in 0..reference.manifest.chunks.len() {
        ensure!(
            !deadline.has_elapsed(),
            "Q transcript caller deadline expired before chunk read"
        );
        let selection = QualificationWorkerTranscriptChunkSelection {
            manifest_hash: reference.manifest_hash.clone(),
            chunk_index: u32::try_from(index)?,
        };
        selection.validate()?;
        let response = client
            .consumer_verification_transcript_chunk(
                &original.coordinate.accepted_root_id,
                &reference.manifest.qualification_operation_id,
                coordinate.clone(),
                serde_json::to_value(&selection)?,
                deadline,
            )
            .await
            .context("protected Q transcript chunk read refused")?;
        ensure!(
            !deadline.has_elapsed(),
            "Q transcript chunk read exceeded caller deadline"
        );
        let ChunkObservationResponse::TranscriptChunk { chunk } = serde_json::from_value(response)
            .context("decode protected Q transcript chunk envelope")?;
        // The shared bounded decoder checks closed fields, canonical base64,
        // selected index, manifest hash, declared blob hash and exact size.
        chunks.push(chunk.verify_for(&reference, &selection)?);
    }
    ensure!(
        !deadline.has_elapsed(),
        "Q transcript verification exceeded caller deadline"
    );
    let stream = reference.manifest.verify_chunks(&chunks)?;
    let configuration = original.scripted_configuration()?;
    let request = original
        .purpose
        .policy_source
        .policy
        .producer_scenarios
        .get(&original.coordinate.scenario_id)
        .and_then(|scenario| scenario.consumer_worker_request.as_ref())
        .context("Q transcript has no original signed Worker request")?;
    crate::consumer_worker_semantics::check_fixture_capture(
        &configuration.qualification_worker,
        request.fixture.as_ref(),
        reference.manifest.fixture_capture.as_ref(),
        stream.fixture_content.as_ref(),
        &reference.manifest.channel_binding_digest,
    )?;
    ensure!(
        !deadline.has_elapsed(),
        "Q transcript verification exceeded caller deadline"
    );
    Ok(VerifiedConsumerWorkerTranscript { reference, stream })
}

pub(crate) fn validate_reference_for_record(
    reference: &QualificationWorkerTranscriptReference,
    original: &ConsumerInputRecord,
) -> Result<()> {
    original.validate()?;
    reference.validate()?;
    let configuration = original.scripted_configuration()?;
    let inputs = original.consumer_invocation()?;
    let manifest = &reference.manifest;
    // Required bounded projection from the exact death-settled launch process.
    // Authentication comes from this protected read and the daemon's immutable
    // manifest/source equality, not from a caller-supplied environment ID.
    manifest.provider_configuration.validate()?;
    original
        .purpose
        .require_consumer_invocation(&manifest.qualification_operation_id, &original.coordinate)?;
    let placement = ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
        &original.coordinate,
        &inputs.qualification_operation_id,
    )?;
    ensure!(
        manifest.accepted_root_id == original.coordinate.accepted_root_id
            && manifest.accepted_capsule_hash == original.coordinate.accepted_capsule_hash
            && manifest.qualification_operation_id == inputs.qualification_operation_id
            && manifest.owner_principal == original.purpose.owner_fingerprint
            && manifest.placement_thread_id == placement,
        "Q transcript changed its protected root, owner or qualification coordinate"
    );
    let request = original
        .purpose
        .policy_source
        .policy
        .producer_scenarios
        .get(&original.coordinate.scenario_id)
        .context("Q transcript has no original signed consumer scenario")?
        .consumer_worker_request
        .as_ref()
        .context("Q transcript scenario has no signed Worker Request")?;
    request.validate()?;
    configuration
        .qualification_worker
        .expected_fixture_content(request.fixture.as_ref())?;
    ensure!(
        request.fixture.is_some() == manifest.fixture_capture.is_some(),
        "Q transcript fixture presence differs from the original signed request"
    );
    let binding = manifest
        .controller_canary
        .as_ref()
        .map(|evidence| &evidence.binding);
    let actual_body = request.bound_controller_canary_body(binding)?;
    if let Some(binding) = binding {
        ensure!(
            binding.provider_declaration_id
                == manifest.provider_configuration.provider_declaration_id
                && binding.configuration_adapter_artifact_hash
                    == manifest
                        .provider_configuration
                        .configuration_adapter_artifact_hash
                && binding.configuration_adapter_artifact_bytes
                    == manifest
                        .provider_configuration
                        .configuration_adapter_artifact_bytes,
            "Q canary request adapter differs from the authenticated process configuration adapter"
        );
    }
    ensure!(
        manifest.request_digest == canonical_value_digest(&actual_body)?
            && manifest.setup_request_digest == canonical_value_digest(&request.session_setup)?,
        "Q transcript changed the original signed setup or turn Request"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chunk_envelope_cannot_introduce_claims_or_another_observation_shape() {
        let response = json!({
            "status": "transcript_chunk",
            "chunk": {
                "manifest_hash": "a".repeat(64), "chunk_index": 0,
                "blob_hash": "b".repeat(64), "size_bytes": 1, "bytes_b64": "YQ==",
            },
        });
        assert!(serde_json::from_value::<ChunkObservationResponse>(response.clone()).is_ok());
        let mut claim = response.clone();
        claim["qualified"] = true.into();
        assert!(serde_json::from_value::<ChunkObservationResponse>(claim).is_err());
        let mut chunk_claim = response.clone();
        chunk_claim["chunk"]["qualified"] = true.into();
        assert!(serde_json::from_value::<ChunkObservationResponse>(chunk_claim).is_err());
        let mut different = response;
        different["status"] = "observed".into();
        assert!(serde_json::from_value::<ChunkObservationResponse>(different).is_err());
    }
}
