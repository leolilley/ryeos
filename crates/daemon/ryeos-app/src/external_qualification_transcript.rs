//! Portable structural bytes for one completed Q Worker conversation.
//!
//! This assembler consumes only journal-validated owner, command and batch
//! rows. It creates no qualification or semantic success permit. The caller
//! must first prove the post-send command marker and terminal process, then
//! stage the chunks and manifest in CAS before retaining their immutable
//! operator row. A pushed batch may have been queued before Request contact;
//! its receipt does not establish request-correlated semantic success.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;
use ryeos_state::objects::{
    MAX_QUALIFICATION_WORKER_TRANSCRIPT_BYTES, MAX_QUALIFICATION_WORKER_TRANSCRIPT_CHUNK_BYTES,
    QUALIFICATION_WORKER_TRANSCRIPT_KIND, QUALIFICATION_WORKER_TRANSCRIPT_SCHEMA,
    QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA, QualificationWorkerTranscriptChunk,
    QualificationWorkerTranscriptManifest, QualificationWorkerTranscriptStream,
};

use crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot;
use crate::process::ExecutionProcessIdentity;
use crate::runtime_db::external_execution::{
    QualificationWorkerCommandPhase, QualificationWorkerCommandRecord,
    QualificationWorkerObservationBatchRecord,
};
use crate::state::AppState;

/// Retain the exact death-settled local conversation while the Q allocation
/// still has its accepted owner and checked Bundle generation. The manifest
/// is structural evidence only; callers must separately prove its meaning and
/// the remote occurrence before making a qualification claim.
pub fn retain_terminal_qualification_worker_transcript(
    state: &AppState,
    placement: &str,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    binding_digest: &str,
    root: &PreparedExternalQualificationRoot,
    expected_process: &ExecutionProcessIdentity,
) -> Result<String> {
    crate::external_qualification_local::with_qualification_local_owner(
        state,
        placement,
        coordinate,
        binding_digest,
        root,
        |_| {
            state.engine.with_checked_bundle_generation(|generation| {
                root.current_local_worker_resolution(generation)?;
                crate::external_qualification_local::ensure_qualification_worker_terminal(
                    state,
                    placement,
                    expected_process,
                )?;
                crate::process::assert_reaped_process_group_absent(expected_process)?;
                let retained = state
                    .state_store
                    .retain_qualification_worker_transcript(placement)?;
                ensure!(
                    retained.placement_thread_id == placement,
                    "Q transcript changed its exact retained placement"
                );
                Ok(retained.manifest_hash)
            })
        },
    )
}

pub(crate) struct PreparedQualificationWorkerTranscript {
    pub manifest: QualificationWorkerTranscriptManifest,
    pub manifest_hash: String,
    pub chunks: Vec<Vec<u8>>,
}

/// Assemble an exact canonical stream from immutable local evidence. The
/// StateStore writer rechecks these coordinates and the positive marker in
/// one DB transaction before binding the CAS object as a retained root.
pub(crate) fn prepare_qualification_worker_transcript(
    owner_principal: &str,
    command: &QualificationWorkerCommandRecord,
    batches: &[QualificationWorkerObservationBatchRecord],
) -> Result<PreparedQualificationWorkerTranscript> {
    ensure!(
        command.phase == QualificationWorkerCommandPhase::TurnCompleted
            && command.owner_send_settled_at_ms.is_some(),
        "Q transcript has no completed turn and successful owner send"
    );
    let turn_id = command
        .turn_id
        .clone()
        .context("Q transcript has no turn identity")?;
    let turn_start_digest = command
        .turn_start_digest
        .clone()
        .context("Q transcript has no turn start")?;
    let turn_completion_digest = command
        .turn_completion_digest
        .clone()
        .context("Q transcript has no turn completion")?;
    let final_response = command
        .final_response
        .clone()
        .context("Q transcript has no retained Final")?;
    let final_response_digest = command
        .final_response_digest
        .clone()
        .context("Q transcript has no Final digest")?;
    let wire_max_bytes = command
        .final_wire_max_bytes
        .context("Q transcript has no signed wire maximum")?;
    ensure!(
        command
            .delta_wire_max_bytes
            .is_none_or(|delta| delta == wire_max_bytes),
        "Q transcript Delta and Final changed signed wire maximum"
    );
    let mut batch_canonical_bytes = 0_u64;
    let mut batch_bodies = Vec::with_capacity(batches.len());
    for batch in batches {
        ensure!(
            batch.placement_thread_id == command.placement_thread_id
                && batch.process_incarnation_digest == command.process_incarnation_digest
                && batch.request_digest == command.request_digest
                && batch.wire_max_bytes == wire_max_bytes,
            "Q transcript batch changed its retained command or wire"
        );
        batch_canonical_bytes = batch_canonical_bytes
            .checked_add(u64::try_from(lillux::canonical_json(&batch.batch)?.len())?)
            .context("Q transcript batch byte count overflow")?;
        batch_bodies.push(batch.batch.clone());
    }
    let stream = QualificationWorkerTranscriptStream {
        schema: QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA.to_owned(),
        batches: batch_bodies,
        delta_response: command.delta_response.clone(),
        final_response: final_response.clone(),
    };
    let stream_bytes = lillux::canonical_json(&serde_json::to_value(&stream)?)?.into_bytes();
    ensure!(
        u64::try_from(stream_bytes.len())? <= MAX_QUALIFICATION_WORKER_TRANSCRIPT_BYTES,
        "Q transcript stream exceeds its absolute byte bound"
    );
    let chunk_size = usize::try_from(MAX_QUALIFICATION_WORKER_TRANSCRIPT_CHUNK_BYTES)?;
    let chunks = stream_bytes
        .chunks(chunk_size)
        .map(ToOwned::to_owned)
        .collect::<Vec<Vec<u8>>>();
    let final_bytes = lillux::canonical_json(&final_response)?;
    let delta_bytes = command
        .delta_response
        .as_ref()
        .map(lillux::canonical_json)
        .transpose()?;
    let manifest = QualificationWorkerTranscriptManifest {
        kind: QUALIFICATION_WORKER_TRANSCRIPT_KIND.to_owned(),
        schema: QUALIFICATION_WORKER_TRANSCRIPT_SCHEMA,
        owner_principal: owner_principal.to_owned(),
        accepted_root_id: command.accepted_root_id.clone(),
        accepted_capsule_hash: command.accepted_capsule_hash.clone(),
        placement_thread_id: command.placement_thread_id.clone(),
        qualification_operation_id: command.qualification_operation_id.clone(),
        qualification_occurrence_id: command.qualification_occurrence_id.clone(),
        channel_binding_digest: command.channel_binding_digest.clone(),
        process_incarnation_digest: command.process_incarnation_digest.clone(),
        request_digest: command.request_digest.clone(),
        owner_send_settled_at_ms: command
            .owner_send_settled_at_ms
            .context("Q transcript lost owner-send success marker")?,
        wire_max_bytes,
        turn_id,
        turn_start_digest,
        turn_completion_digest,
        delta_digest: command.delta_response_digest.clone(),
        delta_response_bytes: delta_bytes
            .as_ref()
            .map(|bytes| u64::try_from(bytes.len()))
            .transpose()?,
        delta_wire_max_bytes: command.delta_wire_max_bytes,
        final_response_digest,
        final_response_bytes: u64::try_from(final_bytes.len())?,
        batch_count: u64::try_from(batches.len())?,
        through_sequence: batches.last().map_or(0, |batch| batch.through_sequence),
        last_batch_digest: batches.last().map(|batch| batch.batch_digest.clone()),
        batch_canonical_bytes,
        stream_sha256: lillux::sha256_hex(&stream_bytes),
        stream_bytes: u64::try_from(stream_bytes.len())?,
        chunks: chunks
            .iter()
            .map(|chunk| -> Result<QualificationWorkerTranscriptChunk> {
                Ok(QualificationWorkerTranscriptChunk {
                    blob_hash: lillux::sha256_hex(chunk),
                    size_bytes: u64::try_from(chunk.len())?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    manifest.verify_chunks(&chunks)?;
    let manifest_hash = ryeos_state::objects::canonical_value_digest(&manifest.to_value()?)?;
    Ok(PreparedQualificationWorkerTranscript {
        manifest,
        manifest_hash,
        chunks,
    })
}
