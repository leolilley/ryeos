//! Bounded-memory cold-seat replay into a private, unpublished core.
//! A page is never admission authority; only `finish` runs derived-state work.

use super::model::RyeOsCore;
use super::seat::{RetainedSeatEvent, SeatEvent, SeatEventKind, SeatReplayOutcome};
use std::collections::{BTreeMap, BTreeSet};

struct StagedTransferProof {
    frame: super::transfer_proof_frame::UiTransferProofFrame,
    lifecycle: ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
    seen: BTreeSet<String>,
}

fn positive_matches_cancellation(
    positive: &super::transfer_proof_frame::UiTransferProofFrame,
    negative: &super::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame,
) -> bool {
    let expected = negative
        .destination_inbound
        .iter()
        .chain(negative.destination_orphan_cancelled.iter())
        .collect::<Vec<_>>();
    positive.transfer == negative.transfer
        && positive.source_anchor == negative.source_anchor
        && positive.destination_anchor == negative.destination_anchor
        && expected.len() == positive.phases.len()
        && expected
            .iter()
            .zip(&positive.phases)
            .all(|(left, right)| *left == right)
}

#[derive(Default)]
pub struct ColdSeatReplay {
    last_sequence: Option<u64>,
    initialized: bool,
    transfer_proofs: BTreeMap<String, StagedTransferProof>,
    cancellation_proofs: BTreeMap<
        String,
        super::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame,
    >,
    root_cut: Option<super::transfer_proof_frame::UiTransferProofAnchor>,
    last_canonical: Option<(u64, String, u64)>,
    canonical_count: u64,
    freeze_operations: BTreeSet<String>,
    freeze_engine_positions: BTreeSet<u64>,
    freeze_receipts: BTreeMap<
        String,
        (
            ryeos_ui_presentation_contract::transfer::TransferEvidence,
            ryeos_ui_presentation_contract::seat_control::SeatAppendReceipt,
        ),
    >,
}

impl ColdSeatReplay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Negative testimony has its own completed paired proof. It must never
    /// be converted to a positive lifecycle phase or inferred from missing rows.
    pub fn stage_move_freeze_cancellation_proof(
        &mut self,
        core: &RyeOsCore,
        frame: super::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame,
    ) -> Result<(), String> {
        frame.validate(16 * 1024 * 1024)?;
        let session = core.data.session.as_ref().ok_or("replay has no session")?;
        let (thread, root) = core.exact_retained_seat_coordinate()?;
        let presentation = core
            .presentation
            .as_ref()
            .ok_or("replay has no presentation")?
            .id();
        let anchor = [&frame.source_anchor, &frame.destination_anchor]
            .into_iter()
            .find(|anchor| {
                anchor.presentation_id == presentation.as_str()
                    && anchor.thread_id == thread
                    && anchor.chain_root_id == root
            })
            .ok_or("cancellation proof does not include the selected seat")?;
        if self.root_cut.as_ref().is_some_and(|cut| cut != anchor)
            || self.last_canonical.as_ref().is_some_and(|(seq, _, chain)| {
                *seq > anchor.thread_seq || *chain > anchor.chain_seq
            })
        {
            return Err("cancellation proof differs from the selected seat cut".into());
        }
        for member in [&frame.source_anchor, &frame.destination_anchor] {
            if !session.presentations.iter().any(|membership| {
                membership.presentation_id == member.presentation_id
                    && membership.thread_id.as_deref() == Some(member.thread_id.as_str())
                    && membership.chain_root_id.as_deref() == Some(member.chain_root_id.as_str())
            }) {
                return Err("cancellation proof has no exact paired session membership".into());
            }
        }
        if self
            .cancellation_proofs
            .contains_key(&frame.transfer.operation_id)
        {
            return Err("cold replay repeats a cancellation proof".into());
        }
        if self
            .transfer_proofs
            .get(&frame.transfer.operation_id)
            .is_some_and(|positive| !positive_matches_cancellation(&positive.frame, &frame))
        {
            return Err("cancellation proof conflicts with staged positive testimony".into());
        }
        self.root_cut = Some(anchor.clone());
        self.cancellation_proofs
            .insert(frame.transfer.operation_id.clone(), frame);
        Ok(())
    }

    /// Data from the authenticated fixed-cut transport only. Consistency is
    /// checked here; signatures and final current-head authorization belong to
    /// the existing server stream owner. Nothing is published by this method.
    pub fn stage_transfer_proof(
        &mut self,
        core: &RyeOsCore,
        frame: super::transfer_proof_frame::UiTransferProofFrame,
    ) -> Result<(), String> {
        let lifecycle = frame.validate(16 * 1024 * 1024)?;
        let session = core.data.session.as_ref().ok_or("replay has no session")?;
        let (thread, root) = core.exact_retained_seat_coordinate()?;
        let presentation = core
            .presentation
            .as_ref()
            .ok_or("replay has no presentation")?
            .id();
        let belongs = [&frame.source_anchor, &frame.destination_anchor]
            .iter()
            .any(|anchor| {
                anchor.presentation_id == presentation.as_str()
                    && anchor.thread_id == thread
                    && anchor.chain_root_id == root
            });
        if !belongs {
            return Err("transfer proof does not include the replayed seat".into());
        }
        let root_anchor = [&frame.source_anchor, &frame.destination_anchor]
            .into_iter()
            .find(|anchor| {
                anchor.presentation_id == presentation.as_str()
                    && anchor.thread_id == thread
                    && anchor.chain_root_id == root
            })
            .ok_or("transfer proof lost the selected root anchor")?;
        if self
            .root_cut
            .as_ref()
            .is_some_and(|prior| prior != root_anchor)
            || self
                .last_sequence
                .is_some_and(|sequence| sequence > root_anchor.chain_seq)
            || self
                .last_canonical
                .as_ref()
                .is_some_and(|(sequence, _, chain)| {
                    *sequence > root_anchor.thread_seq || *chain > root_anchor.chain_seq
                })
        {
            return Err("transfer proof changed or precedes the staged root cut".into());
        }
        for anchor in [&frame.source_anchor, &frame.destination_anchor] {
            if !session.presentations.iter().any(|membership| {
                membership.presentation_id == anchor.presentation_id
                    && membership.thread_id.as_deref() == Some(anchor.thread_id.as_str())
                    && membership.chain_root_id.as_deref() == Some(anchor.chain_root_id.as_str())
            }) {
                return Err("transfer proof seat has no exact session membership".into());
            }
        }
        let id = frame.transfer.operation_id.clone();
        if self.transfer_proofs.contains_key(&id) {
            return Err("cold replay repeats a transfer proof".into());
        }
        if self
            .cancellation_proofs
            .get(&id)
            .is_some_and(|negative| !positive_matches_cancellation(&frame, negative))
        {
            return Err("transfer proof conflicts with staged cancellation testimony".into());
        }
        self.root_cut = Some(root_anchor.clone());
        self.transfer_proofs.insert(
            id,
            StagedTransferProof {
                frame,
                lifecycle,
                seen: BTreeSet::new(),
            },
        );
        Ok(())
    }

    /// Partition canonical transfer rows before the ordinary retained decoder.
    /// They count in the transport cut but never become producer SeatEvents.
    pub fn append_authenticated_records(
        &mut self,
        core: &mut RyeOsCore,
        records: &[serde_json::Value],
    ) -> Result<(), String> {
        for row in records {
            let sequence = row
                .get("chain_seq")
                .and_then(serde_json::Value::as_u64)
                .filter(|sequence| *sequence > 0)
                .ok_or("canonical replay row has no chain position")?;
            let thread_sequence = row
                .get("thread_seq")
                .and_then(serde_json::Value::as_u64)
                .filter(|sequence| *sequence > 0)
                .ok_or("canonical replay row has no thread position")?;
            let hash = row
                .get("event_hash")
                .and_then(serde_json::Value::as_str)
                .filter(|hash| {
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
                .ok_or("canonical replay row has no event hash")?;
            let (thread, root) = core.exact_retained_seat_coordinate()?;
            if row.get("thread_id").and_then(serde_json::Value::as_str) != Some(thread.as_str())
                || row.get("chain_root_id").and_then(serde_json::Value::as_str)
                    != Some(root.as_str())
                || self
                    .last_canonical
                    .as_ref()
                    .is_some_and(|(previous, _, chain)| {
                        thread_sequence <= *previous || sequence <= *chain
                    })
                || self.root_cut.as_ref().is_some_and(|cut| {
                    thread_sequence > cut.thread_seq
                        || sequence > cut.chain_seq
                        || (thread_sequence == cut.thread_seq && hash != cut.event_hash)
                })
            {
                return Err("canonical replay row moved outside its exact root cut".into());
            }
            let kind = row
                .get("event_type")
                .and_then(serde_json::Value::as_str)
                .ok_or("replay record has no event type")?;
            if kind == ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT {
                self.append_cancellation_record(core, row)?;
            } else if kind == "ui_seat.append_receipt.v2" {
                let receipt: ryeos_ui_presentation_contract::seat_control::SeatAppendReceipt =
                    serde_json::from_value(row.get("payload").cloned().ok_or("receipt payload absent")?)
                        .map_err(|error| error.to_string())?;
                let covers_freeze = self.receipt_covers_freeze(
                    receipt.first_engine_seq, receipt.last_engine_seq,
                );
                // Retain only freeze-related receipts, never the whole canonical history.
                if covers_freeze
                    && self.freeze_receipts.insert(receipt.operation_id.clone(), (
                        ryeos_ui_presentation_contract::transfer::TransferEvidence { event_hash: hash.to_owned(), chain_seq: sequence }, receipt
                    )).is_some() {
                        return Err("replay repeats a freeze receipt identity".into());
                    }
            } else if ryeos_ui_presentation_contract::transfer::is_transfer_testimony(kind) {
                self.append_transfer_record(core, row, kind)?;
            } else if let Some(event) = super::seat::retained_seat_event_with_evidence(row)? {
                self.append_authenticated_batch(core, [event])?;
            }
            self.last_canonical = Some((thread_sequence, hash.to_owned(), sequence));
            self.canonical_count = self
                .canonical_count
                .checked_add(1)
                .ok_or("canonical replay event count overflowed")?;
        }
        Ok(())
    }

    fn receipt_covers_freeze(&self, first: u64, last: u64) -> bool {
        first <= last
            && self
                .freeze_engine_positions
                .range(first..=last)
                .next()
                .is_some()
    }

    /// Observe only successfully folded ordinary events. Receipt retention
    /// cannot depend on whether a later proof frame has already been staged.
    fn index_freeze_event(&mut self, event: &SeatEvent) {
        let relevant = match &event.kind {
            SeatEventKind::PresentationOperationPrepared(operation)
                if operation.move_freeze().is_some() =>
            {
                self.freeze_operations
                    .insert(operation.operation_id.clone());
                true
            }
            SeatEventKind::PresentationOperationCompleted(completed) => {
                self.freeze_operations.contains(&completed.operation_id)
            }
            _ => false,
        };
        if relevant && let Some(position) = event.producer_engine_seq {
            self.freeze_engine_positions.insert(position);
        }
    }

    fn append_cancellation_record(
        &mut self,
        core: &mut RyeOsCore,
        row: &serde_json::Value,
    ) -> Result<(), String> {
        let cancellation =
            ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(row)?;
        let frame = self
            .cancellation_proofs
            .get(&cancellation.transfer.operation_id)
            .ok_or("cancellation precedes its completed paired proof")?;
        let sequence = row
            .get("chain_seq")
            .and_then(serde_json::Value::as_u64)
            .ok_or("cancellation chain position absent")?;
        let hash = row
            .get("event_hash")
            .and_then(serde_json::Value::as_str)
            .ok_or("cancellation hash absent")?;
        if !self.initialized
            || frame.cancellation != cancellation
            || frame.cancellation_evidence.chain_seq != sequence
            || frame.cancellation_evidence.event_hash != hash
            || self.last_sequence.is_some_and(|prior| sequence <= prior)
        {
            return Err("cancellation differs from its exact staged testimony".into());
        }
        if let Some(positive) = self
            .transfer_proofs
            .get(&cancellation.transfer.operation_id)
            && !positive_matches_cancellation(&positive.frame, frame)
        {
            return Err("cancellation conflicts with staged positive testimony".into());
        }
        for witness in std::iter::once(&frame.preparation).chain(frame.completion.iter()) {
            if core.seat.evidence_at_sequence(witness.event.seq) != Some(&witness.evidence)
                || core.seat.event_at_sequence(witness.event.seq) != Some(&witness.event)
            {
                return Err(
                    "cancellation freeze witness differs from the canonical seat prefix".into(),
                );
            }
            let (evidence, receipt) = self
                .freeze_receipts
                .get(&witness.receipt_operation_id)
                .ok_or("cancellation freeze witness has no canonical receipt")?;
            if evidence != &witness.receipt_evidence
                || receipt.producer_incarnation != witness.receipt_producer_incarnation
                || receipt.first_engine_seq != witness.receipt_first_engine_seq
                || receipt.last_engine_seq != witness.receipt_last_engine_seq
            {
                return Err("cancellation receipt differs from its canonical interval".into());
            }
        }
        let (length, replay) = core
            .acknowledged_composition_cache
            .as_mut()
            .ok_or("cancellation has no checked composition prefix")?;
        if *length != core.presentation_operations.acknowledged_prefix_len {
            return Err("cancellation has an inconsistent acknowledged prefix".into());
        }
        replay.apply_authenticated_move_freeze_cancellation(
            &cancellation,
            &frame.preparation.event,
            frame.completion.as_ref().map(|witness| &witness.event),
            sequence,
        )?;
        self.cancellation_proofs
            .remove(&cancellation.transfer.operation_id);
        self.last_sequence = Some(sequence);
        Ok(())
    }

    fn append_transfer_record(
        &mut self,
        core: &mut RyeOsCore,
        row: &serde_json::Value,
        kind: &str,
    ) -> Result<(), String> {
        use ryeos_ui_presentation_contract::transfer::{TRANSFER_EVENT, TransferEvidence};
        let record = match kind {
            TRANSFER_EVENT => {
                ryeos_ui_presentation_contract::transfer::from_event_value(row)?.payload
            }
            ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT => {
                ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(row)?.payload
            }
            _ => return Err("unsupported transfer testimony family".into()),
        };
        let sequence = row
            .get("chain_seq")
            .and_then(serde_json::Value::as_u64)
            .ok_or("transfer row has no exact chain position")?;
        let hash = row
            .get("event_hash")
            .and_then(serde_json::Value::as_str)
            .ok_or("transfer row has no event hash")?;
        let (thread, root) = core.exact_retained_seat_coordinate()?;
        let owner = record.event_owner();
        if !self.initialized
            || self
                .last_sequence
                .is_some_and(|previous| sequence <= previous)
            || row.get("thread_id").and_then(serde_json::Value::as_str) != Some(thread.as_str())
            || row.get("chain_root_id").and_then(serde_json::Value::as_str) != Some(root.as_str())
            || owner.thread_id != thread
            || owner.chain_root_id != root
        {
            return Err("transfer row has invalid replay owner or position".into());
        }
        let proof = self
            .transfer_proofs
            .get_mut(&record.transfer.operation_id)
            .ok_or("transfer row precedes its paired proof")?;
        let evidence = TransferEvidence {
            event_hash: hash.to_owned(),
            chain_seq: sequence,
        };
        if !proof
            .frame
            .phases
            .iter()
            .any(|phase| phase.record == record && phase.evidence == evidence)
            || proof.seen.contains(hash)
        {
            return Err("transfer row differs from exact paired testimony".into());
        }
        let (length, replay) = core
            .acknowledged_composition_cache
            .as_mut()
            .ok_or("transfer row has no checked composition prefix")?;
        if *length != core.presentation_operations.acknowledged_prefix_len {
            return Err("transfer row has an inconsistent acknowledged prefix".into());
        }
        let root_id = replay
            .composition
            .root_binding()
            .binding_attachment_id
            .clone();
        let bound = replay
            .binding_descriptors()
            .get(&root_id)
            .ok_or("transfer row has no original root descriptor")?
            .binding_request_bounds
            .max_request_bytes;
        replay.apply_authenticated_transfer_phase(&proof.lifecycle, &record, &evidence, bound)?;
        proof.seen.insert(hash.to_owned());
        self.last_sequence = Some(sequence);
        Ok(())
    }

    /// Insert an authenticated transport batch into the candidate's one seat
    /// log without discarding the retained event coordinates. The caller drops
    /// the entire unpublished candidate on any error.
    pub fn append_authenticated_batch(
        &mut self,
        core: &mut RyeOsCore,
        events: impl IntoIterator<Item = RetainedSeatEvent>,
    ) -> Result<(), String> {
        let (thread_id, chain_root_id) = core.exact_retained_seat_coordinate()?;
        for retained in events {
            self.append_one(
                core,
                retained.event.clone(),
                Some((&thread_id, &chain_root_id, retained)),
            )?;
        }
        Ok(())
    }

    /// Pure reducer fixture path. Production replay always supplies retained
    /// evidence and an exact authenticated session membership.
    #[cfg(test)]
    pub fn append_batch(
        &mut self,
        core: &mut RyeOsCore,
        events: impl IntoIterator<Item = SeatEvent>,
    ) -> Result<(), String> {
        for event in events {
            self.append_one(core, event, None)?;
        }
        Ok(())
    }

    fn append_one(
        &mut self,
        core: &mut RyeOsCore,
        event: SeatEvent,
        retained: Option<(&str, &str, RetainedSeatEvent)>,
    ) -> Result<(), String> {
        if self.last_sequence.is_some_and(|last| event.seq <= last) {
            return Err("cold seat replay moved backwards".into());
        }
        if self
            .root_cut
            .as_ref()
            .is_some_and(|cut| event.seq > cut.chain_seq)
        {
            return Err("cold seat event exceeds its fixed paired cut".into());
        }
        match &event.kind {
            SeatEventKind::PresentationInitialized(_) if !self.initialized => {
                if !core.seat.events().is_empty() {
                    return Err("cold seat replay initialization is not first".into());
                }
                core.validate_replayed_initialization(std::slice::from_ref(&event))?;
                self.initialized = true;
            }
            SeatEventKind::PresentationInitialized(_) => {
                return Err("cold seat replay has duplicate initialization".into());
            }
            _ if !self.initialized => {
                return Err("cold seat replay precedes initialization".into());
            }
            _ => {}
        }
        let sequence = event.seq;
        let prefix = core.presentation_operations.acknowledged_prefix_len;
        let outcome = match retained {
            Some((thread_id, chain_root_id, retained)) => {
                core.seat
                    .append_retained(thread_id, chain_root_id, retained)?
            }
            None => core.seat.append_replayed(event),
        };
        match outcome {
            SeatReplayOutcome::Inserted { index } if index == prefix => {
                let accepted = core.seat.events()[index].clone();
                core.advance_acknowledged_composition_replay(std::slice::from_ref(&accepted))?;
                self.index_freeze_event(&accepted);
                core.presentation_operations
                    .fold_acknowledged(&core.seat.events()[index]);
                core.presentation_operations.acknowledged_prefix_len += 1;
            }
            _ => return Err("cold seat replay is not an exact append-only prefix".into()),
        }
        self.last_sequence = Some(sequence);
        Ok(())
    }

    /// Rebuild derived state once after the completed signed endpoint.
    pub fn finish(self, core: &mut RyeOsCore) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        if !self.transfer_proofs.is_empty() || !self.cancellation_proofs.is_empty() {
            return Err("paired replay requires its authenticated completion cut".into());
        }
        let effects = self.finish_checked(core)?;
        core.authenticated_transfer_proofs.clear();
        core.authenticated_cancellation_proofs.clear();
        core.authenticated_reproof_tip = None;
        Ok(effects)
    }

    pub fn finish_at_authenticated_tip(
        self,
        core: &mut RyeOsCore,
        tip_hash: &str,
        thread_seq: u64,
        event_count: u64,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        if self.canonical_count != event_count {
            return Err("replay completion differs from canonical event count".into());
        }
        if !self
            .last_canonical
            .as_ref()
            .is_some_and(|(sequence, hash, _)| *sequence == thread_seq && hash == tip_hash)
        {
            return Err("replay did not reach its authenticated canonical tip".into());
        }
        let (thread, root) = core.exact_retained_seat_coordinate()?;
        for frame in self.cancellation_proofs.values() {
            if frame.destination_anchor.thread_id != thread
                || frame.destination_anchor.chain_root_id != root
            {
                return Err("cancellation replay has unconsumed source testimony".into());
            }
            // The destination has no source cancellation row: it must replay
            // its real inbound and optional linked orphan phase under the
            // exact same paired cut, never a fabricated negative phase.
            match (
                &frame.destination_inbound,
                self.transfer_proofs.get(&frame.transfer.operation_id),
            ) {
                (Some(inbound), Some(positive))
                    if positive_matches_cancellation(&positive.frame, frame)
                        && positive.seen.contains(&inbound.evidence.event_hash)
                        && frame
                            .destination_orphan_cancelled
                            .as_ref()
                            .is_none_or(|orphan| {
                                positive.seen.contains(&orphan.evidence.event_hash)
                            }) => {}
                (None, None) if frame.destination_orphan_cancelled.is_none() => {}
                _ => {
                    return Err(
                        "cancellation destination differs from exact inbound testimony".into(),
                    );
                }
            }
        }
        if let Some(cut) = &self.root_cut
            && (cut.event_hash != tip_hash
                || cut.thread_seq != thread_seq
                || self.last_canonical.as_ref()
                    != Some(&(cut.thread_seq, cut.event_hash.clone(), cut.chain_seq)))
        {
            return Err("paired replay did not reach its exact canonical root tip".into());
        }
        for proof in self.transfer_proofs.values() {
            let anchor = [&proof.frame.source_anchor, &proof.frame.destination_anchor]
                .into_iter()
                .find(|anchor| anchor.thread_id == thread && anchor.chain_root_id == root)
                .ok_or("paired replay lost its root anchor")?;
            if anchor.event_hash != tip_hash || anchor.thread_seq != thread_seq {
                return Err("paired replay completion changed its fixed root cut".into());
            }
            let expected = proof
                .frame
                .phases
                .iter()
                .filter(|phase| {
                    phase.record.event_owner().thread_id == thread
                        && phase.record.event_owner().chain_root_id == root
                })
                .map(|phase| phase.evidence.event_hash.clone())
                .collect::<BTreeSet<_>>();
            if expected != proof.seen {
                return Err("paired replay has incomplete root testimony coverage".into());
            }
        }
        let effects = self.finish_checked(core)?;
        core.authenticated_transfer_proofs = self
            .transfer_proofs
            .into_iter()
            .map(|(id, proof)| (id, (proof.frame, proof.lifecycle)))
            .collect();
        core.authenticated_cancellation_proofs = self.cancellation_proofs;
        core.authenticated_reproof_tip = Some((tip_hash.to_owned(), thread_seq));
        Ok(effects)
    }

    fn finish_checked(
        &self,
        core: &mut RyeOsCore,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        if !self.initialized {
            return Err("presentation initialization was not replayed".into());
        }
        core.require_replayed_initialization()?;
        core.finalize_cold_seat_replay()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::model::BrowserViewport;
    use crate::ui::reducer::test_support;

    fn candidate() -> (RyeOsCore, SeatEvent) {
        let reference = test_support::core(test_support::session());
        let initial = reference.seat.events()[0].clone();
        let session = reference.data.session.clone().unwrap();
        let core = RyeOsCore::new(
            session,
            test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        (core, initial)
    }

    fn facet(seq: u64) -> SeatEvent {
        SeatEvent {
            seq,
            producer_engine_seq: None,
            kind: SeatEventKind::Facet {
                key: "test".into(),
                value: serde_json::json!(seq),
            },
        }
    }

    #[test]
    fn freeze_receipt_index_keeps_late_completion_before_proof_staging() {
        let mut replay = ColdSeatReplay::new();
        // Isolate indexing after the production fold has validated a freeze;
        // this synthetic completion does not claim admission or CAS evidence.
        replay.freeze_operations.insert("freeze-operation".into());
        replay.freeze_engine_positions.insert(4);
        let completion = |operation: &str, engine: u64| SeatEvent {
            seq: engine + 10,
            producer_engine_seq: Some(engine),
            kind: SeatEventKind::PresentationOperationCompleted(
                super::super::admission_operation::UiCompletedPresentationOperation {
                    operation_id: operation.into(),
                    result: serde_json::Value::Null,
                },
            ),
        };
        replay.index_freeze_event(&completion("unrelated-operation", 7));
        assert!(!replay.receipt_covers_freeze(7, 7));
        replay.index_freeze_event(&completion("freeze-operation", 9));
        assert!(replay.cancellation_proofs.is_empty());
        assert!(replay.receipt_covers_freeze(9, 9));
        assert!(replay.receipt_covers_freeze(3, 4));
        assert!(replay.receipt_covers_freeze(8, 10));
        assert!(!replay.receipt_covers_freeze(5, 8));
        assert!(!replay.receipt_covers_freeze(10, 9));
        assert!(!replay.receipt_covers_freeze(0, 0));
        assert_eq!(replay.freeze_engine_positions.len(), 2);
    }

    // Synthetic authenticated-transport testimony only. These values do not
    // claim a real CAS signature, engine admission, or two-seat publication.
    fn paired_candidate() -> (
        RyeOsCore,
        super::super::transfer_proof_frame::UiTransferProofFrame,
        Vec<serde_json::Value>,
    ) {
        use super::super::transfer_proof_frame::{
            UiTransferProofAnchor, UiTransferProofFrame, UiTransferProofPhase,
        };
        use ryeos_ui_presentation_contract::transfer::{
            TransferEvidence, TransferIdentity, TransferPhase, TransferRecord,
        };
        let (mut core, initial) = candidate();
        let source_id = "22222222-2222-4222-8222-222222222222";
        let session = core.data.session.as_mut().unwrap();
        let mut source = session.presentations[0].clone();
        source.presentation_id = source_id.into();
        source.thread_id = Some("source-seat".into());
        source.chain_root_id = Some("source-chain".into());
        session.presentations.push(source);
        let seat = |presentation: &str, thread: &str, root: &str| {
            ryeos_ui_presentation_contract::transfer::TransferSeat {
                presentation_id: presentation.into(),
                thread_id: thread.into(),
                chain_root_id: root.into(),
                producer_incarnation: "ab".repeat(32),
                revision: 0,
            }
        };
        let transfer = TransferIdentity {
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            source: seat(source_id, "source-seat", "source-chain"),
            destination: seat(
                test_support::FIXTURE_PRESENTATION_ID,
                "fixture-seat",
                "fixture-chain",
            ),
            export_digest: "cd".repeat(32),
        };
        // Native signed chain positions are one-based; local seat event
        // indexes are not native chain positions.
        let sequence = initial.seq + 2;
        let record = TransferRecord {
            schema: 1,
            transfer: transfer.clone(),
            phase: TransferPhase::InboundCommitment,
        };
        let frame = UiTransferProofFrame {
            schema: 1,
            transfer: transfer.clone(),
            source_anchor: UiTransferProofAnchor {
                presentation_id: source_id.into(),
                thread_id: "source-seat".into(),
                chain_root_id: "source-chain".into(),
                event_hash: "04".repeat(32),
                thread_seq: 1,
                chain_seq: 1,
            },
            destination_anchor: UiTransferProofAnchor {
                presentation_id: test_support::FIXTURE_PRESENTATION_ID.into(),
                thread_id: "fixture-seat".into(),
                chain_root_id: "fixture-chain".into(),
                event_hash: "03".repeat(32),
                thread_seq: 3,
                chain_seq: sequence + 1,
            },
            phases: vec![UiTransferProofPhase {
                evidence: TransferEvidence {
                    event_hash: "02".repeat(32),
                    chain_seq: sequence,
                },
                record: record.clone(),
            }],
        };
        let SeatEventKind::PresentationInitialized(initialization) = initial.kind else {
            unreachable!()
        };
        let rows = vec![
            serde_json::json!({"event_type":super::super::presentation_initialization::PRESENTATION_INITIALIZED_EVENT,"event_hash":"01".repeat(32),"chain_seq":initial.seq+1,"thread_seq":1,"thread_id":"fixture-seat","chain_root_id":"fixture-chain","payload":initialization}),
            serde_json::json!({"event_type":ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT,"event_hash":"02".repeat(32),"chain_seq":sequence,"thread_seq":2,"thread_id":"fixture-seat","chain_root_id":"fixture-chain","payload":{"seq":0,"operation_id":transfer.operation_id,"payload":record}}),
            serde_json::json!({"event_type":"thread_notice","event_hash":"03".repeat(32),"chain_seq":sequence+1,"thread_seq":3,"thread_id":"fixture-seat","chain_root_id":"fixture-chain","payload":{}}),
        ];
        frame.validate(65536).unwrap();
        (core, frame, rows)
    }

    // Exercises the destination coverage matcher only. These synthetic
    // values are injected after staging; they do not assert signed source
    // receipts, a complete app cut, or cancellation writer admission.
    fn synthetic_destination_cancellation(
        positive: &super::super::transfer_proof_frame::UiTransferProofFrame,
        inbound: bool,
    ) -> super::super::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame {
        use super::super::transfer_cancellation_proof_frame::{
            UiMoveFreezeCancellationProofFrame, UiReceiptedFreezeWitness,
        };
        use ryeos_ui_presentation_contract::{
            move_freeze_lifecycle::{
                MoveFreezeCancellation, MoveFreezeCancellationEvidence,
                MoveFreezeCancellationWriter,
            },
            transfer::TransferEvidence,
        };
        let preparation = TransferEvidence {
            event_hash: "aa".repeat(32),
            chain_seq: 1,
        };
        let freeze = MoveFreezeCancellationEvidence {
            preparation: preparation.clone(),
            completion: None,
        };
        let cancellation = MoveFreezeCancellation::new(
            positive.transfer.clone(),
            freeze,
            MoveFreezeCancellationWriter::CurrentProducer {
                producer_incarnation: "ab".repeat(32),
                next_engine_seq: 1,
            },
        )
        .unwrap();
        UiMoveFreezeCancellationProofFrame {
            schema: 1,
            transfer: positive.transfer.clone(),
            source_anchor: positive.source_anchor.clone(),
            destination_anchor: positive.destination_anchor.clone(),
            cancellation_evidence: TransferEvidence {
                event_hash: "bb".repeat(32),
                chain_seq: 2,
            },
            cancellation,
            destination_inbound: inbound.then(|| positive.phases[0].clone()),
            destination_orphan_cancelled: None,
            preparation: UiReceiptedFreezeWitness {
                evidence: preparation,
                event: SeatEvent {
                    seq: 1,
                    producer_engine_seq: Some(0),
                    kind: SeatEventKind::Facet {
                        key: "synthetic".into(),
                        value: serde_json::Value::Null,
                    },
                },
                receipt_evidence: TransferEvidence {
                    event_hash: "ac".repeat(32),
                    chain_seq: 2,
                },
                receipt_operation_id: "synthetic".into(),
                receipt_producer_incarnation: "ab".repeat(32),
                receipt_first_engine_seq: 0,
                receipt_last_engine_seq: 0,
            },
            completion: None,
        }
    }

    #[test]
    fn destination_negative_cut_requires_exact_real_inbound_when_present() {
        let (mut core, positive, rows) = paired_candidate();
        let negative = synthetic_destination_cancellation(&positive, true);
        let tip = positive.destination_anchor.clone();
        let mut replay = ColdSeatReplay::new();
        replay
            .stage_transfer_proof(&core, positive.clone())
            .unwrap();
        replay
            .cancellation_proofs
            .insert(positive.transfer.operation_id.clone(), negative);
        replay
            .append_authenticated_records(&mut core, &rows)
            .unwrap();
        replay
            .finish_at_authenticated_tip(&mut core, &tip.event_hash, tip.thread_seq, 3)
            .unwrap();

        let (mut core, positive, rows) = paired_candidate();
        let negative = synthetic_destination_cancellation(&positive, true);
        let tip = positive.destination_anchor.clone();
        let mut missing = ColdSeatReplay::new();
        missing
            .cancellation_proofs
            .insert(positive.transfer.operation_id.clone(), negative);
        let mut ordinary = rows;
        ordinary[1]["event_type"] = serde_json::json!("thread_notice");
        missing
            .append_authenticated_records(&mut core, &ordinary)
            .unwrap();
        assert!(
            missing
                .finish_at_authenticated_tip(&mut core, &tip.event_hash, tip.thread_seq, 3)
                .is_err()
        );
    }

    #[test]
    fn destination_negative_cut_with_absent_inbound_needs_no_positive_frame() {
        let (mut core, positive, mut rows) = paired_candidate();
        let negative = synthetic_destination_cancellation(&positive, false);
        let tip = positive.destination_anchor.clone();
        let mut replay = ColdSeatReplay::new();
        replay
            .cancellation_proofs
            .insert(positive.transfer.operation_id.clone(), negative);
        rows[1]["event_type"] = serde_json::json!("thread_notice");
        replay
            .append_authenticated_records(&mut core, &rows)
            .unwrap();
        replay
            .finish_at_authenticated_tip(&mut core, &tip.event_hash, tip.thread_seq, 3)
            .unwrap();
    }

    #[test]
    fn cancellation_never_accepts_a_conflicting_staged_positive_frame() {
        let (_, positive, _) = paired_candidate();
        let with_inbound = synthetic_destination_cancellation(&positive, true);
        assert!(positive_matches_cancellation(&positive, &with_inbound));
        let without_inbound = synthetic_destination_cancellation(&positive, false);
        assert!(!positive_matches_cancellation(&positive, &without_inbound));
        let mut changed = positive.clone();
        changed.source_anchor.event_hash = "ff".repeat(32);
        assert!(!positive_matches_cancellation(&changed, &with_inbound));
    }

    #[test]
    fn destination_orphan_requires_the_exact_second_positive_phase() {
        use super::super::transfer_proof_frame::UiTransferProofPhase;
        use ryeos_ui_presentation_contract::transfer::{
            TransferEvidence, TransferPhase, TransferRecord,
        };
        let (_, mut positive, _) = paired_candidate();
        let inbound = positive.phases[0].evidence.clone();
        let orphan = UiTransferProofPhase {
            evidence: TransferEvidence {
                event_hash: positive.destination_anchor.event_hash.clone(),
                chain_seq: positive.destination_anchor.chain_seq,
            },
            record: TransferRecord {
                schema: 1,
                transfer: positive.transfer.clone(),
                phase: TransferPhase::OrphanCancelled {
                    inbound: inbound.clone(),
                },
            },
        };
        positive.phases.push(orphan.clone());
        let mut negative = synthetic_destination_cancellation(&positive, true);
        negative.destination_orphan_cancelled = Some(orphan);
        assert!(positive_matches_cancellation(&positive, &negative));
        let mut incomplete = positive.clone();
        incomplete.phases.pop();
        assert!(!positive_matches_cancellation(&incomplete, &negative));
        let mut wrong = positive;
        wrong.phases[1].record.phase = TransferPhase::OrphanCancelled {
            inbound: TransferEvidence {
                event_hash: "ff".repeat(32),
                chain_seq: inbound.chain_seq,
            },
        };
        assert!(!positive_matches_cancellation(&wrong, &negative));
        let mut missing = serde_json::to_value(&negative).unwrap();
        missing
            .as_object_mut()
            .unwrap()
            .remove("destination_orphan_cancelled");
        assert!(
            serde_json::from_value::<
                super::super::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame,
            >(missing)
            .is_err()
        );
    }

    #[test]
    fn destination_orphan_cold_completion_consumes_both_real_phase_rows() {
        use super::super::transfer_proof_frame::UiTransferProofPhase;
        use ryeos_ui_presentation_contract::transfer::{
            TransferEvidence, TransferPhase, TransferRecord,
        };
        let (mut core, mut positive, mut rows) = paired_candidate();
        let inbound = positive.phases[0].evidence.clone();
        let orphan = UiTransferProofPhase {
            evidence: TransferEvidence {
                event_hash: positive.destination_anchor.event_hash.clone(),
                chain_seq: positive.destination_anchor.chain_seq,
            },
            record: TransferRecord {
                schema: 1,
                transfer: positive.transfer.clone(),
                phase: TransferPhase::OrphanCancelled { inbound },
            },
        };
        rows[2] = serde_json::json!({
            "event_type": ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT,
            "event_hash": orphan.evidence.event_hash.clone(),
            "chain_seq": orphan.evidence.chain_seq,
            "thread_seq": positive.destination_anchor.thread_seq,
            "thread_id": positive.destination_anchor.thread_id.clone(),
            "chain_root_id": positive.destination_anchor.chain_root_id.clone(),
            "payload": {"seq": 0, "operation_id": positive.transfer.operation_id.clone(),
                "payload": orphan.record.clone()},
        });
        positive.phases.push(orphan.clone());
        positive.validate(65536).unwrap();
        let mut negative = synthetic_destination_cancellation(&positive, true);
        negative.destination_orphan_cancelled = Some(orphan);
        let tip = positive.destination_anchor.clone();
        let mut replay = ColdSeatReplay::new();
        replay
            .stage_transfer_proof(&core, positive.clone())
            .unwrap();
        replay
            .cancellation_proofs
            .insert(positive.transfer.operation_id.clone(), negative);
        replay
            .append_authenticated_records(&mut core, &rows)
            .unwrap();
        replay
            .finish_at_authenticated_tip(&mut core, &tip.event_hash, tip.thread_seq, 3)
            .unwrap();
    }

    #[test]
    fn ordinary_canonical_replay_rejects_a_different_completion_tip() {
        for wrong_hash in [false, true] {
            let (mut core, _, rows) = paired_candidate();
            let mut replay = ColdSeatReplay::new();
            replay
                .append_authenticated_records(&mut core, &[rows[0].clone(), rows[2].clone()])
                .unwrap();
            let hash = if wrong_hash {
                "ff".repeat(32)
            } else {
                "03".repeat(32)
            };
            let sequence = if wrong_hash { 3 } else { 2 };
            assert!(
                replay
                    .finish_at_authenticated_tip(&mut core, &hash, sequence, 2)
                    .is_err()
            );
        }
    }

    #[test]
    fn paired_rows_count_transport_without_allocating_producer_seat_events() {
        let (mut core, frame, rows) = paired_candidate();
        let operation_id = frame.transfer.operation_id.clone();
        let tip = frame.destination_anchor.clone();
        let mut replay = ColdSeatReplay::new();
        replay.stage_transfer_proof(&core, frame).unwrap();
        assert!(core.authenticated_transfer_proofs.is_empty());
        replay
            .append_authenticated_records(&mut core, &rows)
            .unwrap();
        assert_eq!(core.seat.events().len(), 1);
        assert_eq!(core.presentation_operations.acknowledged_prefix_len, 1);
        assert_eq!(
            replay.transfer_proofs.values().next().unwrap().seen.len(),
            1
        );
        replay
            .finish_at_authenticated_tip(&mut core, &tip.event_hash, tip.thread_seq, 3)
            .unwrap();
        assert!(
            core.authenticated_transfer_proofs
                .contains_key(&operation_id)
        );
    }

    #[test]
    fn paired_projection_refuses_nonidentical_ordinary_replay_after_completion() {
        let (mut core, frame, rows) = paired_candidate();
        let tip = frame.destination_anchor.clone();
        let mut replay = ColdSeatReplay::new();
        replay.stage_transfer_proof(&core, frame).unwrap();
        replay
            .append_authenticated_records(&mut core, &rows)
            .unwrap();
        replay
            .finish_at_authenticated_tip(
                &mut core,
                &tip.event_hash,
                tip.thread_seq,
                rows.len() as u64,
            )
            .unwrap();
        core.presentation_operations.producer = None;
        let events = core.seat.events().to_vec();
        // The transfer row at this position is deliberately absent from the
        // producer log. Generic replay cannot fill that gap with an ordinary
        // facet and then rebuild a projection without its paired testimony.
        let effects = core.replay_seat_events([facet(2)]);
        assert!(effects.is_empty());
        assert!(core.presentation_operations.replay_blocked);
        assert!(core.authenticated_transfer_proofs.is_empty());
        assert_eq!(core.seat.events(), events.as_slice());
    }

    #[test]
    fn paired_completion_requires_exact_tip_and_every_root_phase() {
        for failure in [
            "tip",
            "count",
            "missing_phase",
            "ordinary_finish",
            "incomplete_tip",
        ] {
            let (mut core, frame, rows) = paired_candidate();
            let tip = frame.destination_anchor.clone();
            let mut replay = ColdSeatReplay::new();
            replay.stage_transfer_proof(&core, frame).unwrap();
            let selected = match failure {
                "missing_phase" => vec![rows[0].clone(), rows[2].clone()],
                "incomplete_tip" => rows[..2].to_vec(),
                _ => rows,
            };
            replay
                .append_authenticated_records(&mut core, &selected)
                .unwrap();
            let result = if failure == "ordinary_finish" {
                replay.finish(&mut core)
            } else {
                replay.finish_at_authenticated_tip(
                    &mut core,
                    if failure == "tip" {
                        "wrong-tip"
                    } else {
                        &tip.event_hash
                    },
                    tip.thread_seq,
                    selected.len() as u64 + u64::from(failure == "count"),
                )
            };
            assert!(result.is_err(), "{failure}");
        }
    }

    #[test]
    fn paired_cut_refuses_extra_rows_changed_scope_and_unstaged_testimony() {
        let (mut core, frame, rows) = paired_candidate();
        let mut replay = ColdSeatReplay::new();
        replay.stage_transfer_proof(&core, frame.clone()).unwrap();
        let mut changed = frame.clone();
        changed.transfer.operation_id = "44444444-4444-4444-8444-444444444444".into();
        changed.phases[0].record.transfer = changed.transfer.clone();
        changed.destination_anchor.event_hash = "ff".repeat(32);
        assert!(replay.stage_transfer_proof(&core, changed).is_err());
        replay
            .append_authenticated_records(&mut core, &rows)
            .unwrap();
        let count = core.seat.events().len();
        let extra = serde_json::json!({"event_type":"seat.facet","event_hash":"05".repeat(32),"chain_seq":frame.destination_anchor.chain_seq+1,"thread_seq":4,"thread_id":"fixture-seat","chain_root_id":"fixture-chain","payload":{"seq":1,"payload":{"key":"extra","value":true}}});
        assert!(
            replay
                .append_authenticated_records(&mut core, &[extra])
                .is_err()
        );
        assert_eq!(core.seat.events().len(), count);
        let (mut core, _, rows) = paired_candidate();
        let mut unstaged = ColdSeatReplay::new();
        unstaged
            .append_authenticated_records(&mut core, &rows[..1])
            .unwrap();
        assert!(
            unstaged
                .append_authenticated_records(&mut core, &rows[1..2])
                .is_err()
        );
    }

    #[test]
    fn authenticated_batch_retains_exact_evidence_and_rejects_foreign_seat() {
        let (mut core, initial) = candidate();
        let retained = RetainedSeatEvent {
            evidence: ryeos_ui_presentation_contract::transfer::TransferEvidence {
                event_hash: "ab".repeat(32),
                chain_seq: initial.seq,
            },
            event: initial.clone(),
            thread_id: "fixture-seat".into(),
            chain_root_id: "fixture-chain".into(),
        };
        let mut replay = ColdSeatReplay::new();
        replay
            .append_authenticated_batch(&mut core, [retained.clone()])
            .unwrap();
        assert_eq!(
            core.seat.evidence_at_sequence(initial.seq),
            Some(&retained.evidence)
        );

        let (mut wrong_core, _) = candidate();
        let mut wrong = retained;
        wrong.thread_id = "another-seat".into();
        assert!(
            ColdSeatReplay::new()
                .append_authenticated_batch(&mut wrong_core, [wrong])
                .is_err()
        );
        assert!(wrong_core.seat.events().is_empty());
    }

    #[test]
    fn live_retained_replay_rejects_a_changed_hash_without_lending_old_authority() {
        let (mut core, initial) = candidate();
        let retained = RetainedSeatEvent {
            evidence: ryeos_ui_presentation_contract::transfer::TransferEvidence {
                event_hash: "ab".repeat(32),
                chain_seq: initial.seq,
            },
            event: initial.clone(),
            thread_id: "fixture-seat".into(),
            chain_root_id: "fixture-chain".into(),
        };
        core.replay_authenticated_seat_events(vec![retained.clone()])
            .unwrap();
        assert_eq!(
            core.seat.evidence_at_sequence(initial.seq),
            Some(&retained.evidence)
        );
        let mut contradicted = retained;
        contradicted.evidence.event_hash = "cd".repeat(32);
        assert!(
            core.replay_authenticated_seat_events(vec![contradicted])
                .is_err()
        );
        assert!(core.presentation_operations.replay_blocked);
        assert_eq!(
            core.seat.evidence_at_sequence(initial.seq),
            Some(
                &ryeos_ui_presentation_contract::transfer::TransferEvidence {
                    event_hash: "ab".repeat(32),
                    chain_seq: initial.seq,
                }
            )
        );
    }

    #[test]
    fn live_retained_replay_cannot_attach_evidence_beyond_a_provisional_tail() {
        let (mut core, initial) = candidate();
        core.replay_authenticated_seat_events(vec![RetainedSeatEvent {
            evidence: ryeos_ui_presentation_contract::transfer::TransferEvidence {
                event_hash: "ab".repeat(32),
                chain_seq: initial.seq,
            },
            event: initial,
            thread_id: "fixture-seat".into(),
            chain_root_id: "fixture-chain".into(),
        }])
        .unwrap();
        core.seat
            .append_facet("local.provisional", serde_json::json!(true));
        let later = RetainedSeatEvent {
            evidence: ryeos_ui_presentation_contract::transfer::TransferEvidence {
                event_hash: "cd".repeat(32),
                chain_seq: 2,
            },
            event: facet(2),
            thread_id: "fixture-seat".into(),
            chain_root_id: "fixture-chain".into(),
        };
        assert!(core.replay_authenticated_seat_events(vec![later]).is_err());
        assert!(core.presentation_operations.replay_blocked);
        assert!(core.seat.evidence_at_sequence(2).is_none());
    }

    #[test]
    fn stages_one_prefix_without_reconciling_each_batch() {
        let (mut core, initial) = candidate();
        let mut replay = ColdSeatReplay::new();
        replay.append_batch(&mut core, [initial]).unwrap();
        let presentation_before = format!("{:?}", core.presentation);
        replay.append_batch(&mut core, [facet(2)]).unwrap();
        assert_eq!(core.seat.events().len(), 2);
        assert_eq!(format!("{:?}", core.presentation), presentation_before);
        assert_eq!(core.presentation_operations.acknowledged_prefix_len, 2);
        let effects = replay.finish(&mut core).unwrap();
        assert_eq!(
            effects.iter().map(|effect| effect.id).collect::<Vec<_>>(),
            core.pending_effects.keys().copied().collect::<Vec<_>>()
        );
        let startup = core.initial_effects_after_cold_replay(effects);
        assert_eq!(
            startup.iter().map(|effect| effect.id).collect::<Vec<_>>(),
            core.pending_effects.keys().copied().collect::<Vec<_>>()
        );
        assert_eq!(core.seat.fold().get("test"), Some(&serde_json::json!(2)));
    }

    #[test]
    fn refuses_a_cold_candidate_with_unpublished_platform_work() {
        let (mut core, initial) = candidate();
        let mut replay = ColdSeatReplay::new();
        replay.append_batch(&mut core, [initial]).unwrap();
        core.emit(super::super::effect::RyeOsEffectKind::SetLocationHash {
            hash: "#unpublished".into(),
        });
        assert_eq!(
            replay.finish(&mut core).unwrap_err(),
            "cold seat candidate already has pending platform effects"
        );
    }

    #[test]
    fn startup_merge_keeps_only_the_newest_fetch_for_each_source_key() {
        let session = test_support::session_with_surface(serde_json::json!({
            "name": "replay-fetch-fixture",
            "tiles": ["view:test/fetch"],
            "views": {
                "view:test/fetch": {
                    "widget": "rows",
                    "sources": {
                        "default": {
                            "ref": "service:test/source",
                            "params": {},
                            "collection": "rows"
                        }
                    }
                }
            }
        }));
        let reference = test_support::core(session);
        let initial = reference.seat.events()[0].clone();
        let session = reference.data.session.clone().unwrap();
        let mut core = RyeOsCore::new(
            session,
            test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        let mut replay = ColdSeatReplay::new();
        replay.append_batch(&mut core, [initial]).unwrap();
        let mut replay_effects = replay.finish(&mut core).unwrap();
        replay_effects.extend(core.initial_effects());
        // Exercise two real pending requests explicitly. Bootstrap emission is
        // idempotent and must not be assumed to generate a duplicate itself.
        let tile = core.active_view_set().unwrap().focused_tile;
        replay_effects.extend(core.emit_fetch_source(tile, "view:test/fetch"));
        replay_effects.extend(core.emit_fetch_source(tile, "view:test/fetch"));
        let premerge_keys = replay_effects
            .iter()
            .filter_map(|effect| match &effect.kind {
                super::super::effect::RyeOsEffectKind::FetchSource { tile_id, .. } => {
                    Some(tile_id.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            premerge_keys.len()
                > premerge_keys
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
            "fixture must contain duplicate pre-merge fetches"
        );
        let merged = core.initial_effects_after_cold_replay(replay_effects);
        let keys = merged
            .iter()
            .filter_map(|effect| match &effect.kind {
                super::super::effect::RyeOsEffectKind::FetchSource { tile_id, .. } => {
                    Some(tile_id.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !keys.is_empty(),
            "fixture must exercise a real source fetch"
        );
        assert_eq!(
            keys.len(),
            keys.iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        );
        assert_eq!(
            merged.iter().map(|effect| effect.id).collect::<Vec<_>>(),
            core.pending_effects.keys().copied().collect::<Vec<_>>()
        );
    }

    #[test]
    fn stale_later_fetch_cannot_discard_an_earlier_pending_fetch() {
        let session = test_support::session_with_surface(serde_json::json!({
            "name": "fetch-kind-fixture",
            "tiles": ["view:test/fetch"],
            "views": {
                "view:test/fetch": {
                    "widget": "rows",
                    "sources": {
                        "default": {
                            "ref": "service:test/source",
                            "params": {},
                            "collection": "rows"
                        }
                    }
                }
            }
        }));
        let mut source = test_support::core(session);
        let mut kind = source
            .initial_effects()
            .into_iter()
            .find_map(|effect| match effect.kind {
                kind @ super::super::effect::RyeOsEffectKind::FetchSource { .. } => Some(kind),
                _ => None,
            })
            .expect("fixture supplies a source fetch");
        if let super::super::effect::RyeOsEffectKind::FetchSource { tile_id, .. } = &mut kind {
            *tile_id = "synthetic-stale-later-source".into();
        }

        let mut core = test_support::core(test_support::session());
        let pending = core.emit(kind);
        let stale = super::super::effect::RyeOsEffect {
            id: pending.id + 1000,
            kind: pending.kind.clone(),
        };
        assert!(!core.pending_effects.contains_key(&stale.id));
        let merged = core.initial_effects_after_cold_replay(vec![pending.clone(), stale]);
        assert!(merged.iter().any(|effect| effect.id == pending.id));
        assert!(!merged.iter().any(|effect| effect.id == pending.id + 1000));
        assert!(core.pending_effects.contains_key(&pending.id));
    }

    #[test]
    fn rejects_preinitialization_and_duplicate_or_backward_prefix() {
        let (mut core, initial) = candidate();
        let mut replay = ColdSeatReplay::new();
        assert!(replay.append_batch(&mut core, [facet(1)]).is_err());
        assert!(core.seat.events().is_empty());
        replay.append_batch(&mut core, [initial.clone()]).unwrap();
        assert!(replay.append_batch(&mut core, [initial]).is_err());
        replay.append_batch(&mut core, [facet(3)]).unwrap();
        assert!(replay.append_batch(&mut core, [facet(2)]).is_err());
        assert_eq!(core.seat.events().len(), 2);
    }

    #[test]
    fn many_batches_match_one_shot_replay_at_completion() {
        let (mut candidate, initial) = candidate();
        let session = candidate.data.session.clone().unwrap();
        let mut one_shot = RyeOsCore::new(
            session,
            test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        let events = std::iter::once(initial)
            .chain((1..=256).map(facet))
            .collect::<Vec<_>>();
        let _ = one_shot.replay_seat_events(events.clone());
        one_shot.require_replayed_initialization().unwrap();

        let mut replay = ColdSeatReplay::new();
        for batch in events.chunks(4) {
            replay
                .append_batch(&mut candidate, batch.iter().cloned())
                .unwrap();
        }
        replay.finish(&mut candidate).unwrap();
        assert_eq!(candidate.seat.events(), one_shot.seat.events());
        assert_eq!(candidate.seat.fold(), one_shot.seat.fold());
        assert_eq!(
            format!("{:?}", candidate.presentation),
            format!("{:?}", one_shot.presentation)
        );
        assert_eq!(
            candidate.presentation_operations.acknowledged_prefix_len,
            one_shot.presentation_operations.acknowledged_prefix_len
        );
    }
}
