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

/// Completed protected acquisition, never launch/settlement authority. The
/// future interpreted caller consumes this owner BEFORE any whole-stream parse.
/// No public constructor, Clone or authentication flag exists.
pub(crate) struct ProtectedConsumerWorkerChunks {
    reference: QualificationWorkerTranscriptReference,
    chunks: Vec<Vec<u8>>,
    original_record_sha256: String,
    deadline: MonotonicDeadline,
}

fn require_original_handoff(
    original: &ConsumerInputRecord,
    expected_record_sha256: &str,
    deadline: MonotonicDeadline,
) -> Result<()> {
    ensure!(
        !deadline.has_elapsed(),
        "protected chunk handoff original deadline expired"
    );
    ensure!(
        lillux::sha256_hex(&original.canonical_bytes()?) == expected_record_sha256,
        "protected chunk handoff changed its original record"
    );
    ensure!(
        !deadline.has_elapsed(),
        "protected chunk record join exceeded original deadline"
    );
    Ok(())
}

impl ProtectedConsumerWorkerChunks {
    /// The host uses this SAME acquisition deadline, never an independently
    /// supplied or renewed child budget.
    pub(crate) fn original_deadline(&self) -> MonotonicDeadline {
        self.deadline
    }

    /// Consume the SAME raw vectors and reference; P performs ONE baseline
    /// parse. H3 must supply checker identity from its admitted program owner
    /// and execute synchronous preparation in its retained host transaction.
    pub(crate) fn prepare_checker_input(
        self,
        original: ConsumerInputRecord,
        checker_identity_digest: String,
    ) -> Result<crate::consumer_checker_projection::PreparedCheckerInput> {
        require_original_handoff(&original, &self.original_record_sha256, self.deadline)?;
        let prepared = crate::consumer_checker_projection::prepare_checker_projection(
            original,
            self.reference,
            crate::consumer_checker_projection::OriginalTranscriptChunks::from_original_bytes(
                self.chunks,
            ),
            checker_identity_digest,
        )?;
        ensure!(
            !self.deadline.has_elapsed(),
            "protected chunk preparation exceeded original deadline"
        );
        Ok(prepared)
    }
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
pub(crate) async fn read_consumer_worker_transcript_chunks(
    client: &UdsRuntimeClient,
    original: &ConsumerInputRecord,
    reference: QualificationWorkerTranscriptReference,
    deadline: MonotonicDeadline,
) -> Result<ProtectedConsumerWorkerChunks> {
    ensure!(
        !deadline.has_elapsed(),
        "Q transcript caller deadline expired"
    );
    validate_reference_for_record(&reference, original)?;
    let original_record_sha256 = lillux::sha256_hex(&original.canonical_bytes()?);
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
    Ok(ProtectedConsumerWorkerChunks {
        reference,
        chunks,
        original_record_sha256,
        deadline,
    })
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
    use base64::Engine as _;
    use serde_json::json;

    fn synthetic_reference(bytes: &[u8]) -> QualificationWorkerTranscriptReference {
        use ryeos_external_execution_contract::{
            ProviderCommandEnvironmentProjection, ProviderCommandEnvironmentTransport,
            ProviderConfigurationLaunchInput,
        };
        use ryeos_state::objects::{
            QUALIFICATION_WORKER_TRANSCRIPT_KIND, QUALIFICATION_WORKER_TRANSCRIPT_SCHEMA,
            QualificationWorkerTranscriptChunk, QualificationWorkerTranscriptManifest,
        };
        // Raw chunk response controls only. These bytes are not a parsed stream
        // or authenticated native testimony, and never qualify a real caller.
        let manifest = QualificationWorkerTranscriptManifest {
            kind: QUALIFICATION_WORKER_TRANSCRIPT_KIND.to_owned(),
            schema: QUALIFICATION_WORKER_TRANSCRIPT_SCHEMA,
            owner_principal: format!("fp:{}", "a".repeat(64)),
            accepted_root_id: "T-synthetic".to_owned(),
            accepted_capsule_hash: "b".repeat(64),
            placement_thread_id: format!("Q-{}", "c".repeat(64)),
            qualification_operation_id: "d".repeat(64),
            qualification_occurrence_id: "synthetic".to_owned(),
            channel_binding_digest: "e".repeat(64),
            process_incarnation_digest: "f".repeat(64),
            provider_configuration: ProviderConfigurationLaunchInput {
                schema: 1,
                configuration_sha256: "5".repeat(64),
                configuration_bytes: 100,
                command_environment: ProviderCommandEnvironmentProjection {
                    environment_id: "ryeos-external-candidate".to_owned(),
                    default_environment_id: "ryeos-external-candidate".to_owned(),
                    include_local: false,
                    transport: ProviderCommandEnvironmentTransport::CommandConnector,
                },
                provider_declaration_id: "synthetic".to_owned(),
                configuration_destination: "environments.toml".to_owned(),
                configuration_adapter_artifact_hash: "6".repeat(64),
                configuration_adapter_artifact_bytes: 100,
            },
            controller_canary: None,
            fixture_capture: None,
            setup_request_digest: "4".repeat(64),
            setup_final_response_digest: "7".repeat(64),
            setup_owner_send_settled_at_ms: 1,
            request_digest: "1".repeat(64),
            wire_max_bytes: 1024,
            turn_id: "turn".to_owned(),
            turn_start_digest: "2".repeat(64),
            turn_completion_digest: "3".repeat(64),
            owner_send_settled_at_ms: 1,
            delta_digest: None,
            delta_response_bytes: None,
            delta_wire_max_bytes: None,
            final_response_digest: "8".repeat(64),
            final_response_bytes: 2,
            batch_count: 0,
            through_sequence: 0,
            last_batch_digest: None,
            batch_canonical_bytes: 0,
            stream_sha256: lillux::sha256_hex(bytes),
            stream_bytes: bytes.len() as u64,
            chunks: vec![QualificationWorkerTranscriptChunk {
                blob_hash: lillux::sha256_hex(bytes),
                size_bytes: bytes.len() as u64,
            }],
        };
        let manifest_hash =
            canonical_value_digest(&serde_json::to_value(&manifest).unwrap()).unwrap();
        let reference = QualificationWorkerTranscriptReference {
            manifest_hash,
            manifest,
        };
        reference.validate().unwrap();
        reference
    }

    fn original_fixture() -> ConsumerInputRecord {
        serde_json::from_value(
            serde_json::to_value(crate::consumer_controller::tests::callback_record()).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn original_record_substitution_and_original_deadline_expiry_refuse_handoff() {
        let original = original_fixture();
        let digest = lillux::sha256_hex(&original.canonical_bytes().unwrap());
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(300));
        assert!(require_original_handoff(&original, &digest, deadline).is_ok());
        let mut changed = serde_json::to_value(&original).unwrap();
        changed["scenario_input"]["canary_value"] = json!("e".repeat(64));
        let changed: ConsumerInputRecord = serde_json::from_value(changed).unwrap();
        assert_ne!(
            lillux::sha256_hex(&changed.canonical_bytes().unwrap()),
            digest
        );
        assert!(require_original_handoff(&changed, &digest, deadline).is_err());
        assert!(
            require_original_handoff(
                &original,
                &digest,
                MonotonicDeadline::after(lillux::time::Duration::ZERO)
            )
            .is_err()
        );
    }

    #[test]
    fn raw_owner_moves_actual_bytes_and_expiry_refuses_before_projection() {
        let original = original_fixture();
        let bytes = vec![b'x'; 256];
        let reference = synthetic_reference(&bytes);
        let allocation = bytes.as_ptr();
        let owner = ProtectedConsumerWorkerChunks {
            reference,
            chunks: vec![bytes],
            original_record_sha256: lillux::sha256_hex(&original.canonical_bytes().unwrap()),
            deadline: MonotonicDeadline::after(lillux::time::Duration::ZERO),
        };
        assert_eq!(owner.chunks[0].as_ptr(), allocation);
        // Synthetic owner is test-only and not authenticated. Expiry must
        // refuse before P can parse these deliberately non-stream bytes.
        assert!(
            owner
                .prepare_checker_input(original, "a".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn chunk_response_cannot_change_selected_manifest_index_blob_size_or_bytes() {
        let bytes = vec![b'x'; 256];
        let reference = synthetic_reference(&bytes);
        let selection = QualificationWorkerTranscriptChunkSelection {
            manifest_hash: reference.manifest_hash.clone(),
            chunk_index: 0,
        };
        let response = QualificationWorkerTranscriptChunkResponse {
            manifest_hash: reference.manifest_hash.clone(),
            chunk_index: 0,
            blob_hash: lillux::sha256_hex(&bytes),
            size_bytes: bytes.len() as u64,
            bytes_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        };
        assert_eq!(response.verify_for(&reference, &selection).unwrap(), bytes);
        for mutation in ["manifest", "index", "blob", "size", "bytes"] {
            let mut changed = response.clone();
            match mutation {
                "manifest" => changed.manifest_hash = "0".repeat(64),
                "index" => changed.chunk_index = 1,
                "blob" => changed.blob_hash = "0".repeat(64),
                "size" => changed.size_bytes += 1,
                _ => {
                    changed.bytes_b64 =
                        base64::engine::general_purpose::STANDARD.encode(vec![b'y'; 256])
                }
            }
            assert!(
                changed.verify_for(&reference, &selection).is_err(),
                "{mutation}"
            );
        }
    }

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
