//! Exact historical MOVE freeze testimony. A signed preparation may be
//! accepted only through this typed proof: the two ordinary events it links
//! are the receipted witnesses a completed source cut authenticated, never
//! renderer-supplied export bytes.

use anyhow::Result;
use ryeos_app::handler_error::HandlerError;
use ryeos_client_base::ui::{
    seat::{RetainedSeatEvent, SeatEventKind, retained_seat_event_with_evidence},
    transfer_export::{UiTransferExport, UiTransferExportMode},
};
use ryeos_ui_presentation_contract::{
    move_freeze_lifecycle::MoveFreezeCancellationEvidence,
    transfer::{MoveFreezeEvidence, TransferEvidence, TransferIdentity, TransferPhase},
};

use super::AuthenticatedSeat;
use crate::transfer_store::reader::{
    UiHistoricalReceiptedFreezeWitness, UiTransferFixedHistoryProof, UiTransferFixedSeatProof,
};

fn conflict(message: &'static str) -> anyhow::Error {
    HandlerError::Conflict(message.into()).into()
}

/// A receipted ordinary freeze event, decoded and attributed to the receipt
/// that covered it.
struct FreezeEvent {
    retained: RetainedSeatEvent,
    receipt_producer_incarnation: String,
    receipt_first_engine_seq: u64,
    receipt_last_engine_seq: u64,
}

fn freeze_event(
    proof: &UiTransferFixedSeatProof,
    transfer: &TransferIdentity,
    link: &TransferEvidence,
) -> Result<Option<FreezeEvent>> {
    let Some(witness) = proof.freeze_witness(transfer, link)? else {
        return Ok(None);
    };
    decode(witness, transfer).map(Some)
}

fn decode(
    witness: &UiHistoricalReceiptedFreezeWitness,
    transfer: &TransferIdentity,
) -> Result<FreezeEvent> {
    let mut row = witness.event.to_value();
    row["event_hash"] = serde_json::json!(witness.evidence.event_hash);
    let retained = retained_seat_event_with_evidence(&row)
        .map_err(anyhow::Error::msg)?
        .ok_or_else(|| conflict("MOVE freeze witness is not an ordinary typed Core event"))?;
    if retained.evidence != witness.evidence
        || retained.chain_root_id != transfer.source.chain_root_id
        || retained.thread_id != transfer.source.thread_id
    {
        return Err(conflict("MOVE freeze witness changes the exact source"));
    }
    Ok(FreezeEvent {
        retained,
        receipt_producer_incarnation: witness.receipt.producer_incarnation.clone(),
        receipt_first_engine_seq: witness.receipt.first_engine_seq,
        receipt_last_engine_seq: witness.receipt.last_engine_seq,
    })
}

/// A retained MOVE freeze, judged against the completed source cut and the
/// paired history that holds its signed preparation. `source_cut` must be the
/// completed read of `source`'s exact anchor.
pub(super) fn verify_retained_move_freeze(
    source: &AuthenticatedSeat,
    source_cut: &UiTransferFixedSeatProof,
    history: &UiTransferFixedHistoryProof,
    transfer: &TransferIdentity,
    export: &UiTransferExport,
    links: &MoveFreezeEvidence,
) -> Result<()> {
    let source_prepared = history
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
        || history.source_anchor() != &source.anchor
        || history.lifecycle().transfer() != transfer
        || source_prepared.evidence.chain_seq <= links.completion.chain_seq
    {
        return Err(conflict(
            "MOVE freeze differs from exact source cut or event order",
        ));
    }
    let retained = verify_source_move_freeze(source, source_cut, transfer, links)?;
    if &retained != export {
        return Err(conflict(
            "MOVE freeze export differs from signed preparation",
        ));
    }
    Ok(())
}

/// Typed freeze validation before any transfer Prepared exists.
pub(super) fn verify_source_move_freeze(
    source: &AuthenticatedSeat,
    source_cut: &UiTransferFixedSeatProof,
    transfer: &TransferIdentity,
    links: &MoveFreezeEvidence,
) -> Result<UiTransferExport> {
    verify_source_freeze_events(
        source,
        source_cut,
        transfer,
        &links.preparation,
        Some(&links.completion),
    )
}

/// The freeze a cancellation (pending or retained) names, judged the same way
/// with an optional completion.
pub(super) fn verify_source_cancellation_freeze(
    source: &AuthenticatedSeat,
    source_cut: &UiTransferFixedSeatProof,
    transfer: &TransferIdentity,
    links: &MoveFreezeCancellationEvidence,
) -> Result<UiTransferExport> {
    verify_source_freeze_events(
        source,
        source_cut,
        transfer,
        &links.preparation,
        links.completion.as_ref(),
    )
}

fn verify_source_freeze_events(
    source: &AuthenticatedSeat,
    source_cut: &UiTransferFixedSeatProof,
    transfer: &TransferIdentity,
    preparation_link: &TransferEvidence,
    completion_link: Option<&TransferEvidence>,
) -> Result<UiTransferExport> {
    if source_cut.anchor() != &source.anchor
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
    let preparation = freeze_event(source_cut, transfer, preparation_link)?
        .ok_or_else(|| conflict("MOVE freeze preparation lacks receipted source evidence"))?;
    let SeatEventKind::PresentationOperationPrepared(operation) = &preparation.retained.event.kind
    else {
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
    let producer_seq = preparation.retained.event.producer_engine_seq;
    if request.transfer != *transfer
        || request.export.mode != UiTransferExportMode::Move
        || request.root_binding != source.membership.initialization.root_binding
        || request.transfer.source.revision != transfer.source.revision
        || operation.producer.presentation_id != transfer.source.presentation_id
        || operation.producer.thread_id != transfer.source.thread_id
        || operation.producer.producer_incarnation != transfer.source.producer_incarnation
        || preparation.receipt_producer_incarnation != operation.producer.producer_incarnation
        || producer_seq != Some(operation.prepared_engine_seq)
        || producer_seq.is_none_or(|seq| {
            seq < preparation.receipt_first_engine_seq || seq > preparation.receipt_last_engine_seq
        })
    {
        return Err(conflict(
            "MOVE freeze differs from exact transfer or original producer",
        ));
    }
    let Some(completion_link) = completion_link else {
        // The completed paired cancellation proof, not this absence branch
        // alone, establishes that no completion exists at this cut.
        return Ok(request.export.clone());
    };
    let completion = freeze_event(source_cut, transfer, completion_link)?
        .ok_or_else(|| conflict("MOVE freeze completion lacks receipted source evidence"))?;
    let SeatEventKind::PresentationOperationCompleted(completed) = &completion.retained.event.kind
    else {
        return Err(conflict(
            "MOVE freeze completion link names another seat event",
        ));
    };
    completed
        .validate_for(operation)
        .map_err(|_| conflict("MOVE freeze completion differs from original preparation"))?;
    if completion
        .retained
        .event
        .producer_engine_seq
        .is_none_or(|seq| {
            seq < completion.receipt_first_engine_seq || seq > completion.receipt_last_engine_seq
        })
    {
        return Err(conflict(
            "MOVE freeze completion lacks exact producer receipt position",
        ));
    }
    Ok(request.export.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::objects::{EventDurability, SCHEMA_VERSION, thread_event::ThreadEvent};
    use ryeos_ui_presentation_contract::{
        seat_control::{SEAT_APPEND_RECEIPT_SCHEMA, SeatAppendReceipt},
        transfer::TransferSeat,
    };

    const SOURCE_ROOT: &str = "T-source";

    fn transfer() -> TransferIdentity {
        let seat = |presentation: &str, root: &str| TransferSeat {
            presentation_id: presentation.into(),
            thread_id: root.into(),
            chain_root_id: root.into(),
            producer_incarnation: "a".repeat(64),
            revision: 0,
        };
        TransferIdentity {
            operation_id: "44444444-4444-4444-8444-444444444444".into(),
            source: seat("11111111-1111-4111-8111-111111111111", SOURCE_ROOT),
            destination: seat("22222222-2222-4222-8222-222222222222", "T-destination"),
            export_digest: "b".repeat(64),
        }
    }

    fn witness(
        event_type: &str,
        thread: &str,
        chain_seq: u64,
    ) -> UiHistoricalReceiptedFreezeWitness {
        UiHistoricalReceiptedFreezeWitness {
            evidence: TransferEvidence {
                event_hash: "c".repeat(64),
                chain_seq,
            },
            event: ThreadEvent {
                schema: SCHEMA_VERSION,
                kind: "thread_event".into(),
                chain_root_id: thread.into(),
                chain_seq,
                thread_id: thread.into(),
                thread_seq: chain_seq,
                event_type: event_type.into(),
                durability: EventDurability::Durable,
                ts: "2026-10-21T00:00:00Z".into(),
                prev_chain_event_hash: None,
                prev_thread_event_hash: None,
                payload: serde_json::json!({"seq": 0, "payload": {"key": "k", "value": 1}}),
            },
            receipt_evidence: TransferEvidence {
                event_hash: "d".repeat(64),
                chain_seq: chain_seq + 1,
            },
            receipt: SeatAppendReceipt {
                schema_version: SEAT_APPEND_RECEIPT_SCHEMA.into(),
                producer_incarnation: "a".repeat(64),
                operation_id: "55555555-5555-4555-8555-555555555555".into(),
                first_engine_seq: 0,
                last_engine_seq: 0,
                event_count: 1,
                payload_digest: "e".repeat(64),
            },
        }
    }

    #[test]
    fn a_witness_is_decoded_only_as_an_ordinary_typed_event_of_the_exact_source() {
        let decoded = decode(&witness("seat.facet", SOURCE_ROOT, 4), &transfer()).unwrap();
        assert_eq!(decoded.retained.thread_id, SOURCE_ROOT);
        assert_eq!(decoded.receipt_first_engine_seq, 0);
        assert_eq!(decoded.receipt_last_engine_seq, 0);
    }

    #[test]
    fn a_witness_that_is_not_an_ordinary_seat_event_is_refused() {
        let error = decode(&witness("thread_started", SOURCE_ROOT, 4), &transfer())
            .err()
            .expect("lifecycle testimony is not a freeze witness");
        assert!(
            format!("{error}").contains("ordinary typed Core event"),
            "{error}"
        );
        // Transfer testimony belongs to the paired lane, never to a freeze.
        assert!(
            decode(
                &witness(
                    ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT,
                    SOURCE_ROOT,
                    4
                ),
                &transfer()
            )
            .is_err()
        );
    }

    #[test]
    fn a_witness_of_another_seat_or_position_is_refused() {
        let foreign = decode(&witness("seat.facet", "T-destination", 4), &transfer())
            .err()
            .expect("the destination's event is not a source freeze witness");
        assert!(format!("{foreign}").contains("exact source"), "{foreign}");
        let mut moved = witness("seat.facet", SOURCE_ROOT, 4);
        moved.evidence.chain_seq = 9;
        assert!(decode(&moved, &transfer()).is_err());
    }
}
