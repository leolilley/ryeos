//! Fixed-anchor, bounded forward proof of one seat's signed history.
//!
//! Projection rows are locators only. Each row is reloaded from canonical CAS,
//! linked to the previously verified event, and checked against one signed tip.
//! There is deliberately no client-supplied cursor or omitted-event allowlist.
//! Only `Complete` proves the whole interval; a pending step proves nothing.

use std::collections::BTreeMap;
use std::sync::Arc;

use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::PersistedEventRecord;
use ryeos_client_base::ui::binding::UiPresentationInitialization;
use ryeos_client_base::ui::composition_replay::UiCompositionReplay;
use ryeos_client_base::ui::presentation_initialization::PRESENTATION_INITIALIZED_EVENT;
use ryeos_client_base::ui::seat::retained_seat_event;
use ryeos_state::AuthoritativeThreadEventReadAnchor;
use ryeos_ui_presentation_contract::seat_control::{
    UiSeatControlPredecessor, UiSeatProducer, UiSeatProducerState, authenticate_control_transition,
};
use ryeos_ui_presentation_contract::transfer::TransferEvidence;

use super::super::paired_read::FixedTransferCut;
use super::super::{
    MAX_SEAT_APPEND_EVENTS, SEAT_APPEND_RECEIPT_EVENT, SEAT_EVENT_PREFIX, SEAT_PRODUCER_EVENT,
    SeatAppendReceipt,
};
use super::{invalid, persisted, validate_replay_initialization};
use crate::browser_session::transfer_reservation::CompletedPairedCut;

/// One bounded projection/CAS work slice never exceeds the signed replay
/// frame policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplayFrameBounds {
    events: usize,
    bytes: usize,
}

fn replay_frame_bounds(state: &AppState) -> Result<ReplayFrameBounds, HandlerError> {
    let policy = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(ReplayFrameBounds {
        events: usize::try_from(policy.seat.max_replay_frame_events)
            .map_err(|_| invalid("signed replay frame event bound overflows"))?,
        bytes: usize::try_from(policy.seat.max_replay_frame_bytes)
            .map_err(|_| invalid("signed replay frame byte bound overflows"))?,
    })
}

/// Only `Complete` proves the full interval to the signed tip.
pub(crate) enum FixedAnchorReplayStep {
    /// One bounded slice was folded; the interval is not yet proven.
    Pending,
    Complete {
        tip_hash: String,
        thread_seq: u64,
    },
}

#[derive(Clone)]
pub(crate) struct FixedAnchorReplay {
    anchor: AuthoritativeThreadEventReadAnchor,
    bounds: ReplayFrameBounds,
    presentation_id: String,
    chain_root_id: String,
    thread_id: String,
    tip_hash: String,
    tip_thread_seq: u64,
    tip_chain_seq: i64,
    after_chain_seq: Option<i64>,
    previous_hash: Option<String>,
    previous_thread_seq: u64,
    initialization_seen: bool,
    control_predecessor: Option<UiSeatControlPredecessor>,
    control: Option<UiSeatProducer>,
    next_engine_seq: u64,
    initialization: Option<UiPresentationInitialization>,
    composition: Option<UiCompositionReplay>,
    pending_producer_events: Vec<PersistedEventRecord>,
    reached_tip: bool,
    paired_cut: Option<Arc<FixedTransferCut>>,
    paired_seen: BTreeMap<String, u64>,
}

impl FixedAnchorReplay {
    /// Begin at one captured signed head. A signed head without an event is
    /// not a valid initialized seat and cannot silently become an empty
    /// replay. `cut` is the completed paired cut the seat's transfer
    /// testimony is folded against; a seat with transfer testimony and no cut
    /// does not complete.
    pub(crate) fn begin_at_anchor(
        state: &AppState,
        presentation_id: &str,
        anchor: AuthoritativeThreadEventReadAnchor,
        cut: Option<Arc<FixedTransferCut>>,
    ) -> Result<Self, HandlerError> {
        let chain_root_id = anchor.chain_root_id().to_owned();
        let thread_id = anchor.thread_id().to_owned();
        if let Some(cut) = &cut
            && !cut.covers_seat_anchor(presentation_id, &chain_root_id, &thread_id, &anchor)
        {
            return Err(invalid("paired replay cut differs from original root seat"));
        }
        let (tip_hash, tip_thread_seq) = anchor
            .tip()
            .map(|(hash, sequence)| (hash.to_owned(), sequence))
            .ok_or_else(|| invalid("seat replay has no signed event tip"))?;
        let tip = state
            .state_store
            .retained_thread_event_by_hash(&tip_hash)
            .map_err(|error| invalid(error.to_string()))?
            .ok_or_else(|| invalid("seat replay signed tip is absent from CAS"))?;
        if tip.chain_root_id != chain_root_id
            || tip.thread_id != thread_id
            || tip.thread_seq != tip_thread_seq
        {
            return Err(invalid("seat replay tip differs from signed thread anchor"));
        }
        let tip_chain_seq = i64::try_from(tip.chain_seq)
            .map_err(|_| invalid("seat replay tip chain sequence overflows"))?;
        Ok(Self {
            anchor,
            bounds: replay_frame_bounds(state)?,
            presentation_id: presentation_id.to_owned(),
            chain_root_id,
            thread_id,
            tip_hash,
            tip_thread_seq,
            tip_chain_seq,
            after_chain_seq: None,
            previous_hash: None,
            previous_thread_seq: 0,
            initialization_seen: false,
            control_predecessor: None,
            control: None,
            next_engine_seq: 0,
            initialization: None,
            composition: None,
            pending_producer_events: Vec::new(),
            reached_tip: false,
            paired_cut: cut,
            paired_seen: BTreeMap::new(),
        })
    }

    /// One bounded projection/CAS work slice.
    pub(crate) fn next_step(
        &mut self,
        state: &AppState,
    ) -> Result<FixedAnchorReplayStep, HandlerError> {
        if self.reached_tip {
            if self.paired_cut.as_ref().is_some_and(|cut| {
                !cut.completed_root_positions(
                    &self.presentation_id,
                    &self.chain_root_id,
                    &self.paired_seen,
                )
            }) {
                return Err(invalid(
                    "paired replay did not consume its exact root testimony",
                ));
            }
            if self.anchor.chain_root_id() != self.chain_root_id
                || self.anchor.thread_id() != self.thread_id
                || self.anchor.tip() != Some((self.tip_hash.as_str(), self.tip_thread_seq))
            {
                return Err(invalid(
                    "seat replay completion differs from fixed signed anchor",
                ));
            }
            return Ok(FixedAnchorReplayStep::Complete {
                tip_hash: self.tip_hash.clone(),
                thread_seq: self.tip_thread_seq,
            });
        }
        let page = state
            .state_store
            .replay_events_through(
                &self.chain_root_id,
                Some(&self.thread_id),
                self.after_chain_seq,
                self.tip_chain_seq,
                self.bounds.events,
                self.bounds.bytes,
            )
            .map_err(|error| invalid(error.to_string()))?;
        if page.events.is_empty() {
            return Err(invalid(
                "seat replay projection made no progress before signed tip",
            ));
        }
        for locator in page.events {
            let hash = locator
                .event_hash
                .as_deref()
                .ok_or_else(|| invalid("seat replay locator lacks a CAS hash"))?;
            let event = state
                .state_store
                .retained_thread_event_by_hash(hash)
                .map_err(|error| invalid(error.to_string()))?
                .ok_or_else(|| invalid("seat replay locator CAS object is absent"))?;
            let record = persisted(hash.to_owned(), event)?;
            let expected_seq = self
                .previous_thread_seq
                .checked_add(1)
                .ok_or_else(|| invalid("seat replay thread sequence exhausted"))?;
            if !fixed_link_matches(
                &record,
                &locator,
                &self.chain_root_id,
                &self.thread_id,
                self.previous_hash.as_deref(),
                expected_seq,
                self.after_chain_seq,
            ) {
                // Empty no-op allowlist: even an apparently harmless omitted
                // event is not silently accepted as a signed-history gap.
                return Err(invalid(
                    "seat replay locator omits or changes signed thread history",
                ));
            }
            self.apply_record(record)?;
            self.previous_hash = Some(hash.to_owned());
            self.previous_thread_seq = expected_seq;
            self.after_chain_seq = Some(locator.chain_seq);
            if hash == self.tip_hash && expected_seq == self.tip_thread_seq {
                self.reached_tip = true;
                break;
            }
        }
        if self.reached_tip {
            if !self.initialization_seen || !self.pending_producer_events.is_empty() {
                return Err(invalid(
                    "seat replay ends without initialized, receipted history",
                ));
            }
        } else if !page.has_more {
            return Err(invalid("seat replay locators end before the signed tip"));
        }
        Ok(FixedAnchorReplayStep::Pending)
    }

    /// The composition once this exact root consumed every scheduled paired
    /// position and its complete receipted interval, under exactly the
    /// original initialization. A one-seat replay cannot be used as paired
    /// ownership testimony.
    pub(crate) fn completed_paired_projection(
        &self,
        initial: &UiPresentationInitialization,
    ) -> Result<&UiCompositionReplay, HandlerError> {
        if !self.reached_tip
            || !self.pending_producer_events.is_empty()
            || !self.paired_cut.as_ref().is_some_and(|cut| {
                cut.completed_root_positions(
                    &self.presentation_id,
                    &self.chain_root_id,
                    &self.paired_seen,
                )
            })
            || self.anchor.tip() != Some((self.tip_hash.as_str(), self.tip_thread_seq))
            || self.initialization.as_ref() != Some(initial)
            || self.after_chain_seq != Some(self.tip_chain_seq)
            || self.previous_hash.as_deref() != Some(self.tip_hash.as_str())
            || self.previous_thread_seq != self.tip_thread_seq
        {
            return Err(invalid(
                "paired projection did not finish its original signed cut",
            ));
        }
        self.composition
            .as_ref()
            .ok_or_else(|| invalid("paired projection has no composition"))
    }

    fn apply_record(&mut self, record: PersistedEventRecord) -> Result<(), HandlerError> {
        if matches!(
            record.event_type.as_str(),
            "thread_completed"
                | "thread_failed"
                | "thread_cancelled"
                | "thread_killed"
                | "thread_timed_out"
                | "thread_continued"
        ) {
            return self.apply_terminal(&record);
        }
        match record.event_type.as_str() {
            ryeos_state::event_types::THREAD_CREATED
                if !self.initialization_seen && self.previous_thread_seq == 0 =>
            {
                Ok(())
            }
            ryeos_state::event_types::THREAD_STARTED
                if !self.initialization_seen
                    && self.previous_thread_seq == 1
                    && self.pending_producer_events.is_empty() =>
            {
                // Signed root lifecycle position, not seat testimony. The
                // fixed locator/CAS link has already authenticated it.
                Ok(())
            }
            PRESENTATION_INITIALIZED_EVENT
                if !self.initialization_seen && self.pending_producer_events.is_empty() =>
            {
                validate_replay_initialization(&record, &self.presentation_id)?;
                let initial: ryeos_client_base::ui::presentation_initialization::UiPresentationInitializationRecord =
                    serde_json::from_value(record.payload.clone()).map_err(|error| {
                        invalid(format!("invalid authenticated replay initialization: {error}"))
                    })?;
                let event =
                    retained_seat_event(&serde_json::to_value(&record).map_err(|error| {
                        invalid(format!(
                            "encode authenticated replay initialization: {error}"
                        ))
                    })?)
                    .map_err(invalid)?
                    .ok_or_else(|| invalid("authenticated initialization is not a seat event"))?;
                super::super::composition::fold_private_composition_event(
                    &mut self.composition,
                    &initial.initialization,
                    &event,
                )
                .map_err(invalid)?;
                self.initialization = Some(initial.initialization);
                self.initialization_seen = true;
                self.control_predecessor = Some(UiSeatControlPredecessor::Initialization {
                    event_hash: record
                        .event_hash
                        .clone()
                        .ok_or_else(|| invalid("initialization has no CAS hash"))?,
                });
                Ok(())
            }
            SEAT_PRODUCER_EVENT
                if self.initialization_seen && self.pending_producer_events.is_empty() =>
            {
                let control: UiSeatProducer = serde_json::from_value(record.payload.clone())
                    .map_err(|error| {
                        invalid(format!("invalid authenticated replay control: {error}"))
                    })?;
                let predecessor = self
                    .control_predecessor
                    .as_ref()
                    .ok_or_else(|| invalid("seat replay control precedes initialization"))?;
                authenticate_control_transition(
                    &control,
                    self.control.as_ref(),
                    predecessor,
                    &self.presentation_id,
                    &self.chain_root_id,
                    &self.thread_id,
                    self.next_engine_seq,
                )
                .map_err(invalid)?;
                self.control_predecessor = Some(UiSeatControlPredecessor::Control {
                    event_hash: record
                        .event_hash
                        .clone()
                        .ok_or_else(|| invalid("control has no CAS hash"))?,
                });
                self.control = Some(control);
                Ok(())
            }
            SEAT_APPEND_RECEIPT_EVENT if self.initialization_seen => self.apply_receipt(record),
            kind if kind.starts_with(SEAT_EVENT_PREFIX) && self.initialization_seen => {
                if self.pending_producer_events.len() >= MAX_SEAT_APPEND_EVENTS {
                    return Err(invalid(
                        "seat replay producer interval exceeds append bound",
                    ));
                }
                self.pending_producer_events.push(record);
                Ok(())
            }
            ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT => {
                self.apply_cancellation(&record)
            }
            kind if ryeos_ui_presentation_contract::transfer::is_transfer_testimony(kind) => {
                self.apply_transfer(&record)
            }
            _ => Err(invalid(
                "seat replay includes an unsupported or misplaced event",
            )),
        }
    }

    /// A finalized seat's terminal lifecycle event is the exact signed tip,
    /// typed and carrying only the lease-expiry outcome. It is lifecycle
    /// testimony, not an ordinary seat operation or a producer capability.
    fn apply_terminal(&mut self, record: &PersistedEventRecord) -> Result<(), HandlerError> {
        let status: ryeos_state::objects::thread_snapshot::ThreadStatus = serde_json::from_value(
            record
                .payload
                .get("status")
                .cloned()
                .ok_or_else(|| invalid("terminal testimony lacks typed status"))?,
        )
        .map_err(|error| invalid(error.to_string()))?;
        if !matches!(
            status,
            ryeos_state::objects::thread_snapshot::ThreadStatus::Completed
                | ryeos_state::objects::thread_snapshot::ThreadStatus::Cancelled
                | ryeos_state::objects::thread_snapshot::ThreadStatus::Killed
        ) || record.event_type != format!("thread_{}", status.as_str())
            || record
                .payload
                .as_object()
                .is_none_or(|payload| payload.len() != 2)
            || record
                .payload
                .get("outcome_code")
                .and_then(serde_json::Value::as_str)
                != Some("seat_lease_expired")
        {
            return Err(invalid("terminal event type and typed status differ"));
        }
        if !self.initialization_seen
            || self.composition.is_none()
            || !self.pending_producer_events.is_empty()
            || record.event_hash.as_deref() != Some(self.tip_hash.as_str())
            || record.thread_seq != i64::try_from(self.tip_thread_seq).unwrap_or(-1)
            || record.chain_seq != self.tip_chain_seq
        {
            return Err(invalid("terminal testimony is not the exact seat tip"));
        }
        Ok(())
    }

    fn apply_receipt(&mut self, record: PersistedEventRecord) -> Result<(), HandlerError> {
        let receipt: SeatAppendReceipt = serde_json::from_value(record.payload.clone())
            .map_err(|error| invalid(format!("invalid authenticated replay receipt: {error}")))?;
        let expected_id = ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity {
            presentation_id: self.presentation_id.clone(),
            chain_root_id: self.chain_root_id.clone(),
            thread_id: self.thread_id.clone(),
            producer_incarnation: receipt.producer_incarnation.clone(),
            first_engine_seq: receipt.first_engine_seq,
            last_engine_seq: receipt.last_engine_seq,
            event_count: receipt.event_count as u64,
            payload_digest: receipt.payload_digest.clone(),
        }
        .derive_identity()
        .map_err(invalid)?;
        if receipt.schema_version
            != ryeos_ui_presentation_contract::seat_control::SEAT_APPEND_RECEIPT_SCHEMA
            || receipt.operation_id != expected_id
            || receipt.first_engine_seq != self.next_engine_seq
            || !self.control.as_ref().is_some_and(|control| {
                control.state == UiSeatProducerState::Active
                    && control.producer_incarnation == receipt.producer_incarnation
            })
        {
            return Err(invalid(
                "authenticated replay receipt breaks producer frontier",
            ));
        }
        super::super::append_ack::validate_projected_interval(
            &self.chain_root_id,
            &self.thread_id,
            &receipt,
            record.chain_seq,
            &self.pending_producer_events,
        )?;
        let initial = self
            .initialization
            .as_ref()
            .ok_or_else(|| invalid("seat replay has no composition initialization"))?;
        for producer_record in &self.pending_producer_events {
            let event =
                retained_seat_event(&serde_json::to_value(producer_record).map_err(|error| {
                    invalid(format!("encode authenticated replay event: {error}"))
                })?)
                .map_err(invalid)?
                .ok_or_else(|| invalid("receipted producer record is not a seat event"))?;
            super::super::composition::fold_private_receipted_composition_event(
                &mut self.composition,
                initial,
                &event,
                &self.thread_id,
                &receipt.producer_incarnation,
            )
            .map_err(invalid)?;
        }
        self.next_engine_seq = receipt
            .last_engine_seq
            .checked_add(1)
            .ok_or_else(|| invalid("seat replay engine frontier is exhausted"))?;
        self.pending_producer_events.clear();
        Ok(())
    }

    fn apply_cancellation(&mut self, record: &PersistedEventRecord) -> Result<(), HandlerError> {
        if !self.pending_producer_events.is_empty() {
            return Err(invalid("cancellation interrupts an unreceipted interval"));
        }
        let cut = self
            .paired_cut
            .as_ref()
            .ok_or_else(|| invalid("MOVE freeze cancellation requires a paired replay cut"))?;
        let evidence = record_evidence(record, "paired cancellation")?;
        let value = serde_json::to_value(record).map_err(|error| invalid(error.to_string()))?;
        let cancellation =
            ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(&value)
                .map_err(invalid)?;
        let frame = cut
            .cancellation_at(&evidence)
            .ok_or_else(|| invalid("paired cancellation lacks exact retained outcome"))?;
        if *frame.cancellation != cancellation
            || cancellation.transfer.source.presentation_id != self.presentation_id
            || cancellation.transfer.source.chain_root_id != self.chain_root_id
            || cancellation.transfer.source.thread_id != self.thread_id
            || self.paired_seen.contains_key(&evidence.event_hash)
        {
            return Err(invalid(
                "paired cancellation differs from exact retained source outcome",
            ));
        }
        let preparation = freeze_seat_event(frame.preparation)?;
        let completion = frame.completion.map(freeze_seat_event).transpose()?;
        self.composition
            .as_mut()
            .ok_or_else(|| invalid("paired cancellation composition absent"))?
            .apply_authenticated_move_freeze_cancellation(
                &cancellation,
                &preparation,
                completion.as_ref(),
                evidence.chain_seq,
            )
            .map_err(invalid)?;
        self.paired_seen
            .insert(evidence.event_hash.clone(), evidence.chain_seq);
        Ok(())
    }

    fn apply_transfer(&mut self, record: &PersistedEventRecord) -> Result<(), HandlerError> {
        if !self.pending_producer_events.is_empty() {
            return Err(invalid("transfer interrupts an unreceipted producer batch"));
        }
        let value = serde_json::to_value(record)
            .map_err(|error| invalid(format!("encode authenticated transfer event: {error}")))?;
        let decoded = match record.event_type.as_str() {
            ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT => {
                let transfer = ryeos_ui_presentation_contract::transfer::from_event_value(&value)
                    .map_err(invalid)?;
                let control = self
                    .control
                    .as_ref()
                    .filter(|control| control.state == UiSeatProducerState::Active)
                    .ok_or_else(|| invalid("transfer has no active producer"))?;
                let revision = self
                    .composition
                    .as_ref()
                    .ok_or_else(|| invalid("transfer has no composition frontier"))?
                    .revision;
                transfer
                    .validate_current_witness(
                        self.next_engine_seq,
                        revision,
                        &self.presentation_id,
                        &control.producer_incarnation,
                    )
                    .map_err(invalid)?;
                transfer.payload
            }
            ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT => {
                ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(&value)
                    .map_err(invalid)?
                    .payload
            }
            _ => return Err(invalid("unsupported transfer testimony version")),
        };
        let cut = self
            .paired_cut
            .as_ref()
            .ok_or_else(|| invalid("transfer requires a complete two-seat replay proof"))?;
        let evidence = record_evidence(record, "paired transfer")?;
        let (authenticated, lifecycle) = cut
            .authenticated_phase_at(&self.chain_root_id, &evidence)
            .filter(|(authenticated, _)| *authenticated == &decoded)
            .ok_or_else(|| invalid("paired transfer differs from exact scheduled proof"))?;
        if self
            .paired_seen
            .insert(evidence.event_hash.clone(), evidence.chain_seq)
            .is_some()
        {
            return Err(invalid("paired transfer repeats an exact testimony"));
        }
        let root_attachment = &self
            .initialization
            .as_ref()
            .ok_or_else(|| invalid("paired transfer has no initialization"))?
            .root_binding
            .binding_attachment_id;
        let composition = self
            .composition
            .as_mut()
            .ok_or_else(|| invalid("paired transfer has no composition projection"))?;
        let bound = composition
            .binding_descriptors()
            .get(root_attachment)
            .ok_or_else(|| invalid("paired transfer has no root descriptor"))?
            .binding_request_bounds
            .max_request_bytes;
        composition
            .apply_authenticated_transfer_phase(lifecycle, authenticated, &evidence, bound)
            .map_err(invalid)
    }
}

fn record_evidence(
    record: &PersistedEventRecord,
    what: &str,
) -> Result<TransferEvidence, HandlerError> {
    Ok(TransferEvidence {
        event_hash: record
            .event_hash
            .clone()
            .ok_or_else(|| invalid(format!("{what} has no CAS hash")))?,
        chain_seq: u64::try_from(record.chain_seq)
            .map_err(|_| invalid(format!("{what} position is invalid")))?,
    })
}

fn freeze_seat_event(
    witness: &crate::transfer_store::reader::UiHistoricalReceiptedFreezeWitness,
) -> Result<ryeos_client_base::ui::seat::SeatEvent, HandlerError> {
    let record = persisted(witness.evidence.event_hash.clone(), witness.event.clone())?;
    retained_seat_event(&serde_json::to_value(&record).map_err(|error| invalid(error.to_string()))?)
        .map_err(invalid)?
        .ok_or_else(|| invalid("receipted freeze witness is not a seat event"))
}

fn fixed_link_matches(
    record: &PersistedEventRecord,
    locator: &PersistedEventRecord,
    root: &str,
    thread: &str,
    previous_hash: Option<&str>,
    expected_thread_seq: u64,
    previous_chain_seq: Option<i64>,
) -> bool {
    record.chain_root_id == root
        && record.thread_id == thread
        && record.thread_seq == i64::try_from(expected_thread_seq).unwrap_or(-1)
        && record.prev_thread_event_hash.as_deref() == previous_hash
        && record.chain_seq == locator.chain_seq
        && record.thread_seq == locator.thread_seq
        && locator.chain_root_id == root
        && locator.thread_id == thread
        && previous_chain_seq.is_none_or(|previous| record.chain_seq > previous)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_ui_presentation_contract::seat_control::{UiSeatControlRequest, UiSeatProducerClaim};

    const PRESENTATION: &str = "11111111-1111-4111-8111-111111111111";

    fn seat_thread() -> ryeos_app::state_store::NewThreadRecord {
        let hash = "a".repeat(64);
        ryeos_app::state_store::NewThreadRecord {
            thread_id: "T-seat".into(),
            chain_root_id: "T-seat".into(),
            kind: "seat_session".into(),
            item_ref: "directive:test".into(),
            executor_ref: "native:test".into(),
            launch_mode: "wait".into(),
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            upstream_thread_id: None,
            requested_by: Some("fp:test".into()),
            project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
                canonical_item_ref: "directive:test".into(),
                item_content_hash: hash.clone(),
                item_signer_fingerprint: Some(hash.clone()),
                item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: hash,
                resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:
                        ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
                },
            }),
        }
    }

    fn initialization() -> UiPresentationInitialization {
        let initial: UiPresentationInitialization = serde_json::from_value(serde_json::json!({
            "schema":ryeos_ui_presentation_contract::initialization::PRESENTATION_INITIALIZATION_SCHEMA,
            "selection":{"kind":"configured_defaults"},"presentation_id":PRESENTATION,
            "creation_request_id":"22222222-2222-4222-8222-222222222222",
            "root_binding":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"ab".repeat(32)},
            "next_mount_id":1,"surfaces":[],"binding_attachments":[{
                "binding_attachment_id":"root","binding_generation":1,"binding_digest":"ab".repeat(32),
                "root_ref":"config:ryeos/test/presentation","root_generation":"cd".repeat(32),
                "project_path":null,"subject":{"kind":"projectless"},"posture":"observation_only",
                "binding_request_bounds":{"max_request_bytes":65536,"max_input_bytes":4096,"max_seat_append_bytes":131072},
                "presentation":{"definition":{"kind":"shared_presentation","config":{
                    "category":"ui","shared_presentation":{"schema":1,"title":"Replay fixture","launcher":"launcher",
                        "dialogs":[{"id":"launcher","title":"Launcher","view":"view:test/launcher"}],
                        "docks":{},"capabilities":{"sources":[],"affordances":[]}}}},
                    "views":{"view:test/launcher":{"widget":"rows"}}}
            }]
        })).unwrap();
        initial.validate().unwrap();
        initial
    }

    fn initialization_payload(initial: &UiPresentationInitialization) -> serde_json::Value {
        serde_json::json!({"operation_id":initial.creation_request_id,"initialization":initial})
    }

    fn record(hash: &str, sequence: i64, previous: Option<&str>) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: 0,
            event_hash: Some(hash.to_owned()),
            chain_root_id: "T-root".into(),
            chain_seq: sequence,
            thread_id: "T-seat".into(),
            thread_seq: sequence,
            event_type: "thread_created".into(),
            storage_class: "indexed".into(),
            ts: "2026-10-06T00:00:00Z".into(),
            prev_chain_event_hash: previous.map(str::to_owned),
            prev_thread_event_hash: previous.map(str::to_owned),
            payload: serde_json::json!({}),
        }
    }

    type Fixture = (
        tempfile::TempDir,
        ryeos_app::test_support::TestStateStore,
        AppState,
        FixedAnchorReplay,
    );

    /// A real signed seat thread supplies the anchor; events then enter the
    /// fold directly, so the fold tests exercise one rule at a time.
    fn replay_fixture() -> Fixture {
        let (directory, owner, state) = crate::test_state::build_test_state();
        state
            .state_store
            .create_thread_for_test(&seat_thread())
            .unwrap();
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor("T-seat", "T-seat")
            .unwrap()
            .unwrap();
        let replay =
            FixedAnchorReplay::begin_at_anchor(&state, PRESENTATION, anchor, None).unwrap();
        (directory, owner, state, replay)
    }

    fn initialized_replay() -> Fixture {
        let (directory, owner, state, mut replay) = replay_fixture();
        let initial = initialization();
        let mut initialization = record(&"a".repeat(64), 2, None);
        initialization.chain_root_id = "T-seat".into();
        initialization.event_type = PRESENTATION_INITIALIZED_EVENT.into();
        initialization.payload = initialization_payload(&initial);
        replay.apply_record(initialization).unwrap();
        let mut request = UiSeatControlRequest::Claim {
            schema: 2,
            operation_id: String::new(),
            presentation_id: PRESENTATION.into(),
            thread_id: "T-seat".into(),
            chain_root_id: "T-seat".into(),
            predecessor: replay.control_predecessor.clone().unwrap(),
            intent_nonce: "33333333-3333-4333-8333-333333333333".into(),
            claim: UiSeatProducerClaim::Fresh,
        };
        request.assign_identity().unwrap();
        let control = UiSeatProducer {
            schema_version: ryeos_ui_presentation_contract::seat_control::SEAT_PRODUCER_SCHEMA
                .into(),
            operation_id: request.operation_id().into(),
            request,
            claim: Some(UiSeatProducerClaim::Fresh),
            predecessor: None,
            producer_incarnation: "b".repeat(64),
            state: UiSeatProducerState::Active,
            next_engine_seq: 0,
        };
        let mut control_record = record(&"b".repeat(64), 3, Some(&"a".repeat(64)));
        control_record.chain_root_id = "T-seat".into();
        control_record.event_type = SEAT_PRODUCER_EVENT.into();
        control_record.payload = serde_json::to_value(control).unwrap();
        replay.apply_record(control_record).unwrap();
        (directory, owner, state, replay)
    }

    fn transfer_record(replay: &FixedAnchorReplay) -> PersistedEventRecord {
        let mut event = record(&"e".repeat(64), 7, Some(&"d".repeat(64)));
        event.chain_root_id = "T-seat".into();
        event.event_type = ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT.into();
        event.payload = serde_json::json!({"seq":replay.next_engine_seq,
            "operation_id":"44444444-4444-4444-8444-444444444444",
            "payload":{"schema":1,"transfer":{
                "operation_id":"44444444-4444-4444-8444-444444444444",
                "source":{"presentation_id":"55555555-5555-4555-8555-555555555555","thread_id":"T-other","chain_root_id":"T-other","producer_incarnation":"c".repeat(64),"revision":0},
                "destination":{"presentation_id":PRESENTATION,"thread_id":"T-seat","chain_root_id":"T-seat","producer_incarnation":replay.control.as_ref().unwrap().producer_incarnation,"revision":replay.composition.as_ref().unwrap().revision},
                "export_digest":"f".repeat(64)},"phase":{"kind":"inbound_commitment"}}});
        event
    }

    #[test]
    fn replay_frames_are_bounded_by_the_signed_policy() {
        let (_directory, _owner, state) = crate::test_state::build_test_state();
        assert_eq!(
            replay_frame_bounds(&state).unwrap(),
            ReplayFrameBounds {
                events: 64,
                bytes: 4_194_304,
            }
        );
    }

    #[test]
    fn a_signed_head_without_seat_history_never_completes() {
        let (_directory, _owner, state, mut replay) = replay_fixture();
        let mut guard = 0;
        let error = loop {
            guard += 1;
            assert!(guard < 8, "replay must terminate");
            match replay.next_step(&state) {
                Ok(FixedAnchorReplayStep::Pending) => {}
                Ok(FixedAnchorReplayStep::Complete { .. }) => {
                    panic!("a thread with no signed initialization cannot be a complete seat")
                }
                Err(error) => break error,
            }
        };
        assert!(
            format!("{error}").contains("initialized, receipted history"),
            "{error}"
        );
    }

    #[test]
    fn initialization_only_history_completes_but_is_not_paired_testimony() {
        let (_directory, _owner, state) = crate::test_state::build_test_state();
        state
            .state_store
            .create_thread_for_test(&seat_thread())
            .unwrap();
        let initial = initialization();
        let appended = state
            .state_store
            .append_events(
                "T-seat",
                "T-seat",
                &[ryeos_app::state_store::NewEventRecord {
                    event_type: PRESENTATION_INITIALIZED_EVENT.into(),
                    storage_class: "indexed".into(),
                    payload: initialization_payload(&initial),
                }],
            )
            .unwrap();
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor("T-seat", "T-seat")
            .unwrap()
            .unwrap();
        let mut replay =
            FixedAnchorReplay::begin_at_anchor(&state, PRESENTATION, anchor, None).unwrap();
        assert!(replay.completed_paired_projection(&initial).is_err());
        let tip = loop {
            match replay.next_step(&state).unwrap() {
                FixedAnchorReplayStep::Pending => {}
                FixedAnchorReplayStep::Complete {
                    tip_hash,
                    thread_seq,
                } => break (tip_hash, thread_seq),
            }
        };
        assert_eq!(tip.0, appended[0].event_hash.clone().unwrap());
        assert_eq!(
            replay.control_predecessor,
            Some(UiSeatControlPredecessor::Initialization {
                event_hash: appended[0].event_hash.clone().unwrap(),
            })
        );
        assert!(replay.control.is_none());
        assert!(
            replay.completed_paired_projection(&initial).is_err(),
            "a one-seat replay without a paired cut cannot authorize cleanup"
        );
    }

    #[test]
    fn exact_unpaired_transfer_is_refused() {
        let (_directory, _owner, _state, mut replay) = initialized_replay();
        let initial = initialization();
        assert!(replay.completed_paired_projection(&initial).is_err());
        let event = transfer_record(&replay);
        let error = replay.apply_record(event).unwrap_err();
        assert!(format!("{error}").contains("complete two-seat replay proof"));
        assert!(!replay.reached_tip);
        replay.reached_tip = true;
        assert!(
            replay.completed_paired_projection(&initial).is_err(),
            "a forged local tip without a complete paired cut cannot authorize cleanup"
        );
    }

    #[test]
    fn transfer_rejects_stale_sequence_revision_and_incarnation_before_pairing() {
        let (_directory, _owner, _state, replay) = initialized_replay();
        for field in ["sequence", "revision", "incarnation"] {
            let mut candidate = replay.clone();
            let mut event = transfer_record(&candidate);
            match field {
                "sequence" => event.payload["seq"] = serde_json::json!(1),
                "revision" => {
                    event.payload["payload"]["transfer"]["destination"]["revision"] =
                        serde_json::json!(1)
                }
                _ => {
                    event.payload["payload"]["transfer"]["destination"]["producer_incarnation"] =
                        serde_json::json!("c".repeat(64))
                }
            }
            let error = candidate.apply_record(event).unwrap_err();
            assert!(
                format!("{error}").contains("transfer witness differs"),
                "{field}: {error}"
            );
        }
    }

    #[test]
    fn lifecycle_versions_and_unknown_transfer_family_never_apply() {
        let (_directory, _owner, _state, replay) = initialized_replay();
        for kind in [
            ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT,
            "contract.ryeos_ui.presentation_transfer_lifecycle.v2",
            "contract.ryeos_ui.presentation_transfer.future",
        ] {
            let mut candidate = replay.clone();
            let mut event = transfer_record(&candidate);
            event.event_type = kind.into();
            assert!(candidate.apply_record(event).is_err(), "{kind}");
        }
        let mut candidate = replay.clone();
        let mut event = transfer_record(&candidate);
        event.event_type = "contract.ryeos_ui.presentation_transfer.future".into();
        let error = candidate.apply_record(event).unwrap_err();
        assert!(format!("{error}").contains("unsupported transfer testimony version"));
    }

    #[test]
    fn transfer_interrupting_an_unreceipted_interval_is_refused() {
        let (_directory, _owner, _state, mut replay) = initialized_replay();
        let mut facet = record(&"c".repeat(64), 4, Some(&"b".repeat(64)));
        facet.chain_root_id = "T-seat".into();
        facet.event_type = format!("{SEAT_EVENT_PREFIX}facet");
        replay.apply_record(facet).unwrap();
        assert_eq!(replay.pending_producer_events.len(), 1);
        let event = transfer_record(&replay);
        let error = replay.apply_record(event).unwrap_err();
        assert!(format!("{error}").contains("unreceipted producer batch"));
        assert_eq!(replay.pending_producer_events.len(), 1);
    }

    #[test]
    fn a_receipt_releases_a_structural_edit_before_the_transfer_witness_is_checked() {
        use ryeos_client_base::ui::admission_operation::{
            COMPLETED_EVENT, PREPARED_EVENT, UiPreparedPresentationOperation,
            UiPreparedPresentationOperationBody, UiSeatAppendIdentity,
        };
        use ryeos_client_base::ui::attachment::SelectionAttachment;
        use ryeos_client_base::ui::binding::UiPresentationProducer;
        use ryeos_client_base::ui::composition_operation::{
            CompositionMutation, CompositionOwner, UiCompositionRequest,
        };
        use sha2::{Digest, Sha256};
        let (_directory, _owner, _state, mut replay) = initialized_replay();
        let composition = replay.composition.as_ref().unwrap();
        let initial = replay.initialization.as_ref().unwrap();
        let mount = composition
            .composition
            .dialogs()
            .values()
            .next()
            .unwrap()
            .view()
            .instance_key()
            .clone();
        let attachment = SelectionAttachment::Pinned {
            values: std::collections::BTreeMap::new(),
            fingerprint: format!("{:x}", Sha256::digest(b"{}")),
        };
        let mut prepared = UiPreparedPresentationOperation {
            schema: 2,
            prepared_engine_seq: 0,
            operation_id: String::new(),
            producer: UiPresentationProducer {
                presentation_id: PRESENTATION.into(),
                thread_id: "T-seat".into(),
                producer_incarnation: replay
                    .control
                    .as_ref()
                    .unwrap()
                    .producer_incarnation
                    .clone(),
            },
            root_binding: initial.root_binding.clone(),
            request_bounds: initial.binding_attachments[0].binding_request_bounds,
            body: UiPreparedPresentationOperationBody::Composition {
                request: UiCompositionRequest {
                    schema: 1,
                    presentation_id: composition.composition.id().clone(),
                    operation_id: String::new(),
                    expected_revision: 0,
                    expected_next_mount_id: composition.next_mount_id,
                    owner: CompositionOwner::Shared {
                        binding: initial.root_binding.clone(),
                    },
                    mutation: CompositionMutation::SetSubject {
                        mount: mount.clone(),
                        attachment,
                    },
                },
            },
        };
        prepared.assign_identity().unwrap();
        let completed = composition
            .expected_composition_completion(&prepared, prepared.request_bounds.max_request_bytes)
            .unwrap();
        let semantic = serde_json::json!([
            {"engine_seq":0,"event_type":PREPARED_EVENT,"payload":prepared},
            {"engine_seq":1,"event_type":COMPLETED_EVENT,"payload":completed},
        ]);
        for (index, kind) in [PREPARED_EVENT, COMPLETED_EVENT].into_iter().enumerate() {
            let mut event = record(
                &format!("{:064x}", index + 10),
                4 + index as i64,
                Some(&format!("{:064x}", index + 9)),
            );
            event.chain_root_id = "T-seat".into();
            event.storage_class = "durable".into();
            event.event_type = kind.into();
            event.payload = serde_json::json!({"seq":index,"payload":semantic[index]["payload"]});
            replay.apply_record(event).unwrap();
        }
        assert_eq!(replay.next_engine_seq, 0);
        assert_eq!(replay.composition.as_ref().unwrap().revision, 0);
        let mut interrupted = replay.clone();
        let error = interrupted
            .apply_record(transfer_record(&replay))
            .unwrap_err();
        assert!(format!("{error}").contains("unreceipted producer batch"));
        let digest =
            ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(&semantic).unwrap();
        let identity = UiSeatAppendIdentity {
            presentation_id: PRESENTATION.into(),
            chain_root_id: "T-seat".into(),
            thread_id: "T-seat".into(),
            producer_incarnation: replay
                .control
                .as_ref()
                .unwrap()
                .producer_incarnation
                .clone(),
            first_engine_seq: 0,
            last_engine_seq: 1,
            event_count: 2,
            payload_digest: digest.clone(),
        };
        let receipt = SeatAppendReceipt {
            schema_version:
                ryeos_ui_presentation_contract::seat_control::SEAT_APPEND_RECEIPT_SCHEMA.into(),
            producer_incarnation: identity.producer_incarnation.clone(),
            operation_id: identity.derive_identity().unwrap(),
            first_engine_seq: 0,
            last_engine_seq: 1,
            event_count: 2,
            payload_digest: digest,
        };
        let mut receipt_record = record(&"d".repeat(64), 6, Some(&format!("{:064x}", 11)));
        receipt_record.chain_root_id = "T-seat".into();
        receipt_record.event_type = SEAT_APPEND_RECEIPT_EVENT.into();
        receipt_record.payload = serde_json::to_value(receipt).unwrap();
        replay.apply_record(receipt_record).unwrap();
        assert!(replay.pending_producer_events.is_empty());
        assert_eq!(replay.next_engine_seq, 2);
        assert_eq!(replay.composition.as_ref().unwrap().revision, 1);
        assert!(
            replay
                .composition
                .as_ref()
                .unwrap()
                .subjects
                .contains_key(&mount)
        );
        let mut stale = transfer_record(&replay);
        stale.payload["payload"]["transfer"]["destination"]["revision"] = serde_json::json!(0);
        let error = replay.clone().apply_record(stale).unwrap_err();
        assert!(format!("{error}").contains("transfer witness differs"));
        let error = replay.apply_record(transfer_record(&replay)).unwrap_err();
        assert!(format!("{error}").contains("complete two-seat replay proof"));
    }

    #[test]
    fn missing_projection_row_cannot_become_a_noop_gap() {
        let retained = record("b", 3, Some("a"));
        let locator = retained.clone();
        assert!(!fixed_link_matches(
            &retained,
            &locator,
            "T-root",
            "T-seat",
            Some("a"),
            2,
            Some(1),
        ));
    }

    #[test]
    fn projection_payload_and_event_type_are_not_membership() {
        let retained = record("b", 2, Some("a"));
        let mut locator = retained.clone();
        locator.payload = serde_json::json!({"forged": true});
        locator.event_type = "seat.facet".into();
        assert!(fixed_link_matches(
            &retained,
            &locator,
            "T-root",
            "T-seat",
            Some("a"),
            2,
            Some(1),
        ));
        locator.thread_seq = 3;
        assert!(!fixed_link_matches(
            &retained,
            &locator,
            "T-root",
            "T-seat",
            Some("a"),
            2,
            Some(1),
        ));
    }

    #[test]
    fn terminal_lifecycle_testimony_is_accepted_only_as_the_exact_typed_tip() {
        let (_directory, _owner, _state, replay) = initialized_replay();
        let tip = |replay: &FixedAnchorReplay| {
            let mut event = record(
                replay.tip_hash.as_str(),
                replay.tip_chain_seq,
                Some(&"b".repeat(64)),
            );
            event.chain_root_id = "T-seat".into();
            event.thread_seq = i64::try_from(replay.tip_thread_seq).unwrap();
            event.event_type = "thread_completed".into();
            event.payload =
                serde_json::json!({"status":"completed","outcome_code":"seat_lease_expired"});
            event
        };
        let mut accepted = replay.clone();
        accepted.apply_record(tip(&replay)).unwrap();

        let mut elsewhere = tip(&replay);
        elsewhere.event_hash = Some("f".repeat(64));
        assert!(replay.clone().apply_record(elsewhere).is_err());
        let mut mistyped = tip(&replay);
        mistyped.event_type = "thread_killed".into();
        assert!(replay.clone().apply_record(mistyped).is_err());
        let mut other_outcome = tip(&replay);
        other_outcome.payload["outcome_code"] = serde_json::json!("operator_stop");
        assert!(replay.clone().apply_record(other_outcome).is_err());
        let mut extra = tip(&replay);
        extra.payload["note"] = serde_json::json!("x");
        assert!(replay.clone().apply_record(extra).is_err());
        let mut failed = tip(&replay);
        failed.event_type = "thread_failed".into();
        failed.payload["status"] = serde_json::json!("failed");
        assert!(replay.clone().apply_record(failed).is_err());
    }

    #[test]
    fn a_duplicate_initialization_is_refused() {
        let (_directory, _owner, _state, mut replay) = initialized_replay();
        let initial = initialization();
        let mut duplicate = record(&"c".repeat(64), 4, Some(&"b".repeat(64)));
        duplicate.chain_root_id = "T-seat".into();
        duplicate.event_type = PRESENTATION_INITIALIZED_EVENT.into();
        duplicate.payload = initialization_payload(&initial);
        assert!(replay.apply_record(duplicate).is_err());
    }
}
