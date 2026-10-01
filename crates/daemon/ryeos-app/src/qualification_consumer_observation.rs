//! Bounded readback of the exact completed Q conversation.
//! The journal owns transport/settlement corroboration; the signed independent
//! verifier interprets the Worker language. These reads grant no fresh contact.

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;
use ryeos_state::objects::{
    MAX_QUALIFICATION_WORKER_TRANSCRIPT_CHUNK_BYTES, QualificationWorkerTranscriptChunkResponse,
    QualificationWorkerTranscriptChunkSelection, QualificationWorkerTranscriptManifest,
    QualificationWorkerTranscriptReference,
};

pub(crate) fn read_retained_reference(
    state: &crate::state::AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    owner_principal: &str,
) -> Result<QualificationWorkerTranscriptReference> {
    coordinate.validate()?;
    let retained = state
        .state_store
        .qualification_historical_transport_evidence(
            coordinate,
            qualification_operation_id,
            owner_principal,
        )?;
    // CAS traversal stays outside the callback mutex. The caller reauthenticates
    // the current bearer before returning any protected bytes to its verifier.
    authority.ensure_guard(guard)?;
    let value = authority
        .cas_store()?
        .get_object_bounded(&retained.transcript_manifest_hash, 1024 * 1024)?
        .context("Q retained transcript manifest is absent")?;
    let manifest: QualificationWorkerTranscriptManifest = serde_json::from_value(value)?;
    let reference = QualificationWorkerTranscriptReference {
        manifest_hash: retained.transcript_manifest_hash,
        manifest,
    };
    reference.validate()?;
    let manifest = &reference.manifest;
    ensure!(
        manifest.owner_principal == owner_principal
            && manifest.accepted_root_id == coordinate.accepted_root_id
            && manifest.accepted_capsule_hash == coordinate.accepted_capsule_hash
            && manifest.placement_thread_id == retained.placement_thread_id
            && manifest.qualification_operation_id == qualification_operation_id
            && manifest.qualification_occurrence_id
                == retained.owner.qualification_occurrence.occurrence_id
            && manifest.channel_binding_digest == retained.channel_binding_digest
            && manifest.request_digest == retained.request_digest
            && manifest.final_response_digest == retained.final_response_digest
            && manifest.turn_id == retained.turn_id
            && manifest.turn_start_digest == retained.turn_start_digest
            && manifest.turn_completion_digest == retained.turn_completion_digest,
        "Q CAS transcript differs from its exact settled command and transport owner"
    );
    authority.ensure_guard(guard)?;
    Ok(reference)
}

pub(crate) fn read_selected_chunk(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    reference: &QualificationWorkerTranscriptReference,
    selection: &QualificationWorkerTranscriptChunkSelection,
) -> Result<QualificationWorkerTranscriptChunkResponse> {
    reference.validate()?;
    selection.validate()?;
    ensure!(
        selection.manifest_hash == reference.manifest_hash,
        "Q chunk request substituted another transcript manifest"
    );
    let declared = reference
        .manifest
        .chunks
        .get(usize::try_from(selection.chunk_index)?)
        .context("Q chunk request is outside the exact retained manifest")?;
    authority.ensure_guard(guard)?;
    let bytes = authority
        .cas_store()?
        .get_blob_bounded(
            &declared.blob_hash,
            MAX_QUALIFICATION_WORKER_TRANSCRIPT_CHUNK_BYTES,
        )?
        .context("Q selected retained transcript chunk is absent")?;
    ensure!(
        bytes.len() as u64 == declared.size_bytes
            && lillux::sha256_hex(&bytes) == declared.blob_hash,
        "Q retained chunk bytes differ from the exact journal-owned manifest"
    );
    authority.ensure_guard(guard)?;
    Ok(QualificationWorkerTranscriptChunkResponse {
        manifest_hash: reference.manifest_hash.clone(),
        chunk_index: selection.chunk_index,
        blob_hash: declared.blob_hash.clone(),
        size_bytes: declared.size_bytes,
        bytes_b64: STANDARD.encode(&bytes),
    })
}
