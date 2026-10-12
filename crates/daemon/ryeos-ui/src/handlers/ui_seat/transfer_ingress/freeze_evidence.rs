//! Exact historical MOVE freeze testimony. Existing signed preparation may be
//! accepted only through this typed proof; fresh MOVE preparation remains sealed.

use anyhow::Result;
use ryeos_app::handler_error::HandlerError;
use ryeos_client_base::ui::{
    seat::SeatEventKind,
    transfer_export::{UiTransferExport, UiTransferExportMode},
};
use ryeos_ui_presentation_contract::transfer::{
    MoveFreezeEvidence, TransferIdentity, TransferPhase,
};

use super::{super::VerifiedTransferSeatPrefix, AuthenticatedSeat, TransferAdmissionProof};

fn conflict(message: &'static str) -> anyhow::Error {
    HandlerError::Conflict(message.into()).into()
}

/// Consumes only a *completed* fixed-anchor prefix with the two ordinary
/// events explicitly requested at begin. Its reader already validated each
/// whole producer interval, receipt, original preparation provenance and
/// event-position composition fold before publishing these witnesses.
pub(super) fn verify_retained_move_freeze(
    source: &AuthenticatedSeat,
    prefix: &VerifiedTransferSeatPrefix,
    proof: &TransferAdmissionProof,
    transfer: &TransferIdentity,
    export: &UiTransferExport,
    links: &MoveFreezeEvidence,
) -> Result<()> {
    let source_prepared = proof
        .history
        .lifecycle()
        .prepared()
        .ok_or_else(|| conflict("MOVE freeze requires completed source preparation proof"))?;
    let TransferPhase::Prepared {
        freeze: Some(retained_links),
        export: retained_export,
        ..
    } = &source_prepared.record.phase
    else {
        return Err(conflict(
            "MOVE preparation lacks its exact signed freeze links",
        ));
    };
    if retained_links != links || retained_export != &serde_json::to_value(export)? {
        return Err(conflict(
            "MOVE freeze evidence differs from signed source preparation",
        ));
    }
    if export.mode != UiTransferExportMode::Move
        || proof.history.source_anchor() != &source.anchor
        || proof.history.lifecycle().transfer() != transfer
        || source_prepared.evidence.chain_seq <= links.completion.chain_seq
    {
        return Err(conflict(
            "MOVE freeze differs from exact source cut or event order",
        ));
    }
    let retained = verify_source_move_freeze(source, prefix, transfer, links)?;
    if &retained != export {
        return Err(conflict(
            "MOVE freeze export differs from signed preparation",
        ));
    }
    Ok(())
}

/// Typed freeze validation before any transfer Prepared exists. Cancellation
/// uses this exact completed source prefix together with the app owner's
/// separate paired absence proof; neither proof substitutes for the other.
pub(super) fn verify_source_move_freeze(
    source: &AuthenticatedSeat,
    prefix: &VerifiedTransferSeatPrefix,
    transfer: &TransferIdentity,
    links: &MoveFreezeEvidence,
) -> Result<UiTransferExport> {
    verify_source_freeze_events(
        source,
        prefix,
        transfer,
        &links.preparation,
        Some(&links.completion),
    )
}

pub(super) fn verify_source_cancellation_freeze(
    source: &AuthenticatedSeat,
    prefix: &VerifiedTransferSeatPrefix,
    transfer: &TransferIdentity,
    links: &ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellationEvidence,
) -> Result<UiTransferExport> {
    verify_source_freeze_events(
        source,
        prefix,
        transfer,
        &links.preparation,
        links.completion.as_ref(),
    )
}

fn verify_source_freeze_events(
    source: &AuthenticatedSeat,
    prefix: &VerifiedTransferSeatPrefix,
    transfer: &TransferIdentity,
    preparation_link: &ryeos_ui_presentation_contract::transfer::TransferEvidence,
    completion_link: Option<&ryeos_ui_presentation_contract::transfer::TransferEvidence>,
) -> Result<UiTransferExport> {
    if prefix.anchor() != &source.anchor
        || completion_link.is_some_and(|completion| {
            preparation_link.chain_seq >= completion.chain_seq
                || preparation_link.event_hash == completion.event_hash
        })
        || source.verified.replay.revision < transfer.source.revision
    {
        return Err(conflict(
            "MOVE freeze differs from exact source cut or event order",
        ));
    }
    // Reconfirm the signed prefix is paired with this exact current
    // initialization, not another membership with the same seat coordinates.
    prefix.history(&source.membership.initialization)?;
    let preparation = prefix
        .freeze_event(preparation_link)
        .ok_or_else(|| conflict("MOVE freeze preparation lacks receipted source evidence"))?;
    let SeatEventKind::PresentationOperationPrepared(operation) = &preparation.event.kind else {
        return Err(conflict(
            "MOVE freeze preparation link names another seat event",
        ));
    };
    operation
        .validate()
        .map_err(|_| conflict("MOVE freeze preparation is invalid"))?;
    let request = operation
        .move_freeze()
        .ok_or_else(|| conflict("MOVE freeze preparation has another operation kind"))?;
    if request.transfer != *transfer
        || request.export.mode != UiTransferExportMode::Move
        || request.root_binding != source.membership.initialization.root_binding
        || request.transfer.source.revision != transfer.source.revision
        || operation.producer.presentation_id != transfer.source.presentation_id
        || operation.producer.thread_id != transfer.source.thread_id
        || operation.producer.producer_incarnation != transfer.source.producer_incarnation
        || preparation.receipt_producer_incarnation != operation.producer.producer_incarnation
        || preparation.event.producer_engine_seq != Some(operation.prepared_engine_seq)
        || preparation.event.producer_engine_seq.is_none_or(|seq| {
            seq < preparation.receipt_first_engine_seq || seq > preparation.receipt_last_engine_seq
        })
    {
        return Err(conflict(
            "MOVE freeze differs from exact transfer or original producer",
        ));
    }
    let Some(completion_link) = completion_link else {
        // The opaque completed paired cancellation proof, not this absence
        // branch alone, establishes that no completion exists at this cut.
        return Ok(request.export.clone());
    };
    let completion = prefix
        .freeze_event(completion_link)
        .ok_or_else(|| conflict("MOVE freeze completion lacks receipted source evidence"))?;
    let SeatEventKind::PresentationOperationCompleted(completed) = &completion.event.kind else {
        return Err(conflict(
            "MOVE freeze completion link names another seat event",
        ));
    };
    completed
        .validate_for(operation)
        .map_err(|_| conflict("MOVE freeze completion differs from original preparation"))?;
    if completion.event.producer_engine_seq.is_none_or(|seq| {
        seq < completion.receipt_first_engine_seq || seq > completion.receipt_last_engine_seq
    }) {
        return Err(conflict(
            "MOVE freeze completion lacks exact producer receipt position",
        ));
    }
    Ok(request.export.clone())
}
