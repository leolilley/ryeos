//! Fixed-cut reads of seat histories and the proofs they produce.
//!
//! A proof here is read testimony about one exact signed cut: it authenticates
//! the seat's control, receipt and initialization ancestry and folds the
//! transfer phases the cut retains. It is never admission. A writer must
//! condition its append on every anchor the proof names and recheck its own
//! mutable admission under the publication locks.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use ryeos_app::state_store::StateStore;
use ryeos_state::objects::thread_event::ThreadEvent;
use ryeos_ui_presentation_contract::{
    identity::UiSeatAppendIdentity,
    initialization::UiPresentationInitializationBound,
    seat_control::{
        SEAT_APPEND_RECEIPT_SCHEMA, SEAT_PRODUCER_EVENT, SeatAppendReceipt,
        SeatAppendSemanticEvent, UiSeatControlPredecessor, UiSeatProducer, UiSeatProducerState,
        authenticate_control_transition,
    },
    transfer::{
        TRANSFER_EVENT, TransferEvidence, TransferIdentity, TransferPhase, TransferRecord,
        TransferSeat,
    },
    transfer_lifecycle::{
        TRANSFER_LIFECYCLE_EVENT, TransferLifecycleEventPayload, verify_latest_control,
    },
    transfer_proof::TransferTestimony,
    transfer_state::TransferLifecycleState,
};

/// Most native events one seat append receipt may cover. The writer and every
/// reader of retained seat history share this protocol maximum.
pub const MAX_SEAT_RECEIPT_EVENTS: usize = 128;

/// Bytes of receipt-covered events one reader may hold while a receipt
/// interval is pending: the protocol maximum at the native event ceiling.
pub(super) const MAX_PENDING_RECEIPT_BYTES: u64 = MAX_SEAT_RECEIPT_EVENTS as u64
    * ryeos_state::objects::thread_event::MAX_THREAD_EVENT_SERIALIZED_BYTES as u64;

/// Events one page of a complete fixed read covers.
pub const READ_PAGE_EVENTS: u64 = 64;
/// Bytes one page of a complete fixed read covers.
pub const READ_PAGE_BYTES: u64 = 2 * 1024 * 1024;

pub(super) fn charge_pending_receipt_bytes(current: u64, event_bytes: u64) -> anyhow::Result<u64> {
    let total = current
        .checked_add(event_bytes)
        .ok_or_else(|| anyhow!("seat receipt interval byte count overflow"))?;
    if total > MAX_PENDING_RECEIPT_BYTES {
        bail!("seat receipt interval exceeds its protocol byte maximum");
    }
    Ok(total)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferSettlementSide {
    Source,
    Destination,
}

/// Every candidate additionally requires authenticated complete current-phase
/// proof, the exact owning seat's existing durable producer fence, system writer
/// attribution and an atomic conditional append. These are required checks, not
/// caller-supplied assertions that the checks already happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferSettlementRequirement {
    /// Cancel a destination-only inbound commitment only while exact source
    /// and destination heads are excluded and the complete authenticated
    /// source history proves no Prepared for this operation.
    SerializeOrphanCancellationAndSourcePreparation,
    /// A source abort must not race a commit or late destination acceptance.
    /// An incomplete scan with no Accepted is never a no-reservation proof.
    SerializeSourceDecisionAndDestinationAcceptance,
    /// Retain, Release or Observe only the exact already-established testimony.
    ExactCurrentPhaseAndOwningSeatFence,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TransferSettlementPlan {
    Candidate {
        record: TransferRecord,
        requirement: TransferSettlementRequirement,
    },
    /// Preserve the exact prior outcome; in particular do not relabel Activated
    /// as Retained after session loss. This is not a retirement clearance.
    AlreadyRecorded(TransferEvidence),
    /// A source seat with no Prepared has no transfer obligation to settle.
    AwaitingSourcePreparation,
    AwaitingSourceDecision,
    AwaitingDestinationSettlement,
    /// No destination acceptance appears in this caller-supplied fold. Source
    /// release still requires a complete authenticated no-reservation proof,
    /// serialized against late acceptance. Never manufacture Released/Observed.
    NoAcceptedReservation {
        source_abort: Option<TransferEvidence>,
    },
}

/// Opaque, in-memory progress of one two-seat settlement read. Its evidence is
/// reconstructed on any later attempt; this is not a durable registry.
pub struct UiTransferSettlementRead {
    store: Arc<StateStore>,
    transfer: TransferIdentity,
    side: TransferSettlementSide,
    source: UiTransferFixedSeatRead,
    destination: UiTransferFixedSeatRead,
    poisoned: bool,
}

/// What a completed settlement read selected for one side of one transfer.
pub enum SettlementPreparation {
    FreezeCancellation(UiMoveFreezeCancellationProof),
    Candidate(TransferSettlementCandidate),
    Deferred(TransferSettlementPlan),
}

/// A fully authenticated settlement the owning seat may publish under its
/// durable reaping claim. The anchors support storage consistency checks, not
/// admission: the writer rechecks both under the publication locks.
pub struct TransferSettlementCandidate {
    pub(super) store: Arc<StateStore>,
    pub(super) record: TransferRecord,
    pub(super) latest_control: TransferEvidence,
    pub(super) closed_control: bool,
    pub(super) owner_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    pub(super) counterpart: SettlementCounterpart,
    pub(super) original_append_bound: u64,
}

/// How the counterpart seat stands in a settlement's cut.
#[derive(Clone, Debug)]
pub enum SettlementCounterpart {
    /// A live seat at an exact signed frontier.
    Live(ryeos_state::AuthoritativeThreadEventReadAnchor),
    /// A retired seat, by the fence its retirement published.
    Retired(ryeos_state::RetiredChainDependency),
}

impl TransferSettlementCandidate {
    pub fn belongs_to(&self, store: &StateStore) -> bool {
        std::ptr::eq(self.store.as_ref(), store)
    }
    pub fn record(&self) -> &TransferRecord {
        &self.record
    }
    pub fn latest_control(&self) -> &TransferEvidence {
        &self.latest_control
    }
    pub fn closed_control(&self) -> bool {
        self.closed_control
    }
    pub fn owner_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.owner_anchor
    }
    pub fn counterpart(&self) -> &SettlementCounterpart {
        &self.counterpart
    }
    pub fn original_append_bound(&self) -> u64 {
        self.original_append_bound
    }
}

pub fn begin_read(
    store: Arc<StateStore>,
    transfer: &TransferIdentity,
    side: TransferSettlementSide,
) -> anyhow::Result<UiTransferSettlementRead> {
    if transfer.source.chain_root_id == transfer.destination.chain_root_id
        || transfer.source.thread_id == transfer.destination.thread_id
        || transfer.source.presentation_id == transfer.destination.presentation_id
    {
        bail!("transfer settlement requires two distinct retained seats");
    }
    let capture = |seat: &TransferSeat| {
        store
            .capture_thread_event_read_anchor(&seat.chain_root_id, &seat.thread_id)?
            .ok_or_else(|| anyhow!("transfer settlement requires both retained signed heads"))
    };
    // Capture BOTH cuts before reading either history. The shared scanner
    // authenticates ordinary freeze receipts in the same bounded page owner.
    let source_anchor = capture(&transfer.source)?;
    let destination_anchor = capture(&transfer.destination)?;
    let source = begin_fixed_seat_read(
        store.clone(),
        source_anchor,
        &transfer.source,
        std::slice::from_ref(transfer),
    )?;
    let destination = begin_fixed_seat_read(
        store.clone(),
        destination_anchor,
        &transfer.destination,
        std::slice::from_ref(transfer),
    )?;
    Ok(UiTransferSettlementRead {
        store,
        transfer: transfer.clone(),
        side,
        source,
        destination,
        poisoned: false,
    })
}

pub fn read_page(
    read: &mut UiTransferSettlementRead,
    max_events: u64,
    max_bytes: u64,
) -> anyhow::Result<bool> {
    if read.poisoned {
        bail!("transfer settlement read previously failed");
    }
    let result = (|| {
        let history = if !read.source.history.complete {
            &mut read.source
        } else if !read.destination.history.complete {
            &mut read.destination
        } else {
            return Ok(true);
        };
        read_fixed_seat_page(history, max_events, max_bytes)?;
        Ok(read.source.history.complete && read.destination.history.complete)
    })();
    if result.is_err() {
        read.poisoned = true;
    }
    result
}

pub fn prepare_write(read: UiTransferSettlementRead) -> anyhow::Result<SettlementPreparation> {
    require_complete_settlement_read(
        read.poisoned,
        read.source.history.complete,
        read.destination.history.complete,
    )?;
    let source = finish_fixed_seat_read(read.source)?;
    let destination = finish_fixed_seat_read(read.destination)?;
    let source_operation = source
        .requested
        .get(&read.transfer.operation_id)
        .ok_or_else(|| anyhow!("selected source coverage absent"))?;
    if read.side == TransferSettlementSide::Source
        && source_operation.phases.is_empty()
        && source_operation.freeze_preparation.is_some()
    {
        return Ok(SettlementPreparation::FreezeCancellation(
            assemble_move_freeze_cancellation_proof(&source, &destination, &read.transfer)?,
        ));
    }
    let destination_operation = destination
        .requested
        .get(&read.transfer.operation_id)
        .ok_or_else(|| anyhow!("selected destination coverage absent"))?;
    let state = fold_transfer_phase_slices(
        &read.transfer,
        &source_operation.phases,
        &destination_operation.phases,
    )?;
    let plan = select_settlement(&state, read.side).map_err(anyhow::Error::msg)?;
    let TransferSettlementPlan::Candidate { record, .. } = &plan else {
        return Ok(SettlementPreparation::Deferred(plan));
    };
    // Complete signed histories and two exact publication anchors make only a
    // candidate. The writer rechecks the durable reaping claim and both heads
    // under the publication locks; a concurrent destination Accepted moves its
    // head and conflicts a source Abort.
    let (owner, counterpart) = match read.side {
        TransferSettlementSide::Source => (&source.history, &destination.history),
        TransferSettlementSide::Destination => (&destination.history, &source.history),
    };
    let (fence, control) = owner
        .latest_control
        .as_ref()
        .ok_or_else(|| anyhow!("transfer settlement seat has no retained producer control"))?;
    Ok(SettlementPreparation::Candidate(
        TransferSettlementCandidate {
            store: read.store,
            record: record.clone(),
            latest_control: fence.clone(),
            closed_control: control.state == UiSeatProducerState::Closed,
            owner_anchor: owner.anchor.clone(),
            counterpart: SettlementCounterpart::Live(counterpart.anchor.clone()),
            original_append_bound: owner
                .original_append_bound
                .ok_or_else(|| anyhow!("seat initialization bound is absent"))?,
        },
    ))
}

pub(super) struct SeatHistory<A = ryeos_state::AuthoritativeThreadEventReadAnchor> {
    pub(super) seat: TransferSeat,
    pub(super) anchor: A,
    pub(super) progress: Option<ryeos_state::AuthoritativeThreadEventReadProgress>,
    pub(super) complete: bool,
    pub(super) previous_chain_seq: Option<u64>,
    pub(super) previous_thread_seq: Option<u64>,
    pub(super) latest_control: Option<(TransferEvidence, UiSeatProducer)>,
    pub(super) newer_control: Option<UiSeatProducer>,
    pub(super) frontier: Option<u64>,
    pub(super) interval_producer: Option<String>,
    pub(super) pending_receipt: Option<(SeatAppendReceipt, Vec<ThreadEvent>)>,
    pub(super) pending_receipt_bytes: u64,
    pub(super) pending_receipt_evidence: Option<TransferEvidence>,
    pub(super) validated_receipt: Option<(TransferEvidence, SeatAppendReceipt)>,
    pub(super) validated_receipt_events: Vec<ThreadEvent>,
    pending_cancellation_control:
        Option<ryeos_ui_presentation_contract::transfer_lifecycle::TransferLifecycleFence>,
    pub(super) initialization_seen: bool,
    pub(super) original_append_bound: Option<u64>,
    pub(super) initialized_root_binding: Option<serde_json::Value>,
    pub(super) initialized_payload: Option<serde_json::Value>,
    #[cfg(test)]
    pub(super) phases: Vec<RetainedPhase>,
}

#[derive(Clone)]
pub(super) struct RetainedPhase {
    pub(super) evidence: TransferEvidence,
    pub(super) record: TransferRecord,
    pub(super) lifecycle: Option<TransferLifecycleEventPayload>,
}

/// Shared closed-vocabulary decoding for positive discovery and authenticated
/// reconstruction. Foreign activity is not a phase; unknown owned versions
/// are never an absence/no-op result.
pub(super) fn decode_transfer_phase(
    event: &ThreadEvent,
) -> anyhow::Result<Option<(TransferRecord, Option<TransferLifecycleEventPayload>)>> {
    if !ryeos_ui_presentation_contract::transfer::is_transfer_testimony(&event.event_type) {
        return Ok(None);
    }
    let value = event.to_value();
    let phase = match event.event_type.as_str() {
        TRANSFER_EVENT => (
            ryeos_ui_presentation_contract::transfer::from_event_value(&value)
                .map_err(anyhow::Error::msg)?
                .payload,
            None,
        ),
        TRANSFER_LIFECYCLE_EVENT => {
            let payload =
                ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(&value)
                    .map_err(anyhow::Error::msg)?;
            (payload.payload.clone(), Some(payload))
        }
        _ => bail!("transfer history encountered unsupported testimony"),
    };
    Ok(Some(phase))
}

impl RetainedPhase {
    pub(super) fn new(
        evidence: TransferEvidence,
        record: TransferRecord,
        lifecycle: Option<TransferLifecycleEventPayload>,
    ) -> Self {
        Self {
            evidence,
            record,
            lifecycle,
        }
    }

    pub(super) fn testimony(&self) -> TransferTestimony<'_> {
        TransferTestimony {
            evidence: &self.evidence,
            record: &self.record,
        }
    }
}

/// Only the complete CAS reader can construct this candidate. Its original
/// anchors support storage consistency checks, not domain fence admission.
/// In particular authenticated Closed control does not establish session expiry
/// or prove its original writer admission.
impl<A> SeatHistory<A> {
    pub(super) fn fresh(seat: TransferSeat, anchor: A) -> Self {
        Self {
            seat,
            anchor,
            progress: None,
            complete: false,
            previous_chain_seq: None,
            previous_thread_seq: None,
            latest_control: None,
            newer_control: None,
            frontier: None,
            interval_producer: None,
            pending_receipt: None,
            pending_receipt_bytes: 0,
            pending_receipt_evidence: None,
            validated_receipt: None,
            validated_receipt_events: Vec::new(),
            pending_cancellation_control: None,
            initialization_seen: false,
            original_append_bound: None,
            initialized_root_binding: None,
            initialized_payload: None,
            #[cfg(test)]
            phases: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn push(
        &mut self,
        transfer: &TransferIdentity,
        hash: String,
        event: ThreadEvent,
    ) -> anyhow::Result<()> {
        if let Some(phase) = self.validate_seat_event(hash, event)? {
            self.route_operation_phase(transfer, phase)?;
        }
        Ok(())
    }

    /// Advance the exact-seat control/receipt/initialization reconstruction
    /// independently of operation selection. A decoded phase is provisional:
    /// the fixed CAS scan must still finish and verify its lifecycle fences.
    /// No phase slots or operation-specific identity checks happen here.
    pub(super) fn validate_seat_event(
        &mut self,
        hash: String,
        event: ThreadEvent,
    ) -> anyhow::Result<Option<RetainedPhase>> {
        if event.chain_root_id != self.seat.chain_root_id
            || event.thread_id != self.seat.thread_id
            || self
                .previous_chain_seq
                .is_some_and(|previous| event.chain_seq >= previous)
            || self
                .previous_thread_seq
                .is_some_and(|previous| event.thread_seq.checked_add(1) != Some(previous))
        {
            bail!("transfer history changed exact seat or adjacent thread order");
        }
        if self.initialization_seen
            && (event.event_type.starts_with("seat.")
                || event.event_type.starts_with("ui_seat.")
                || event.event_type == TRANSFER_EVENT
                || event.event_type == TRANSFER_LIFECYCLE_EVENT
                || event
                    .event_type
                    .starts_with("contract.ryeos_ui.move_freeze_cancelled"))
        {
            bail!("signed seat activity precedes its initialization");
        }
        let newer_chain_seq = self.previous_chain_seq;
        self.validated_receipt_events.clear();
        self.previous_chain_seq = Some(event.chain_seq);
        self.previous_thread_seq = Some(event.thread_seq);
        let evidence = TransferEvidence {
            event_hash: hash,
            chain_seq: event.chain_seq,
        };
        if let Some((receipt, events)) = self.pending_receipt.as_mut() {
            if !event.event_type.starts_with("seat.")
                || events.len() >= MAX_SEAT_RECEIPT_EVENTS
                || newer_chain_seq != event.chain_seq.checked_add(1)
            {
                bail!("seat receipt interval is interrupted or oversized");
            }
            let bytes = lillux::canonical_json(&event.to_value())?.len() as u64;
            self.pending_receipt_bytes =
                charge_pending_receipt_bytes(self.pending_receipt_bytes, bytes)?;
            events.push(event);
            if events.len() == receipt.event_count {
                self.finish_receipt()?;
            }
            return Ok(None);
        }
        if event.event_type == "ui_seat.append_receipt.v2" {
            let receipt: SeatAppendReceipt = serde_json::from_value(event.payload)
                .context("decode CAS-authenticated seat append receipt")?;
            if receipt.schema_version != SEAT_APPEND_RECEIPT_SCHEMA
                || receipt.event_count == 0
                || receipt.event_count > MAX_SEAT_RECEIPT_EVENTS
            {
                bail!("invalid seat append receipt version or bound");
            }
            let identity = UiSeatAppendIdentity {
                presentation_id: self.seat.presentation_id.clone(),
                chain_root_id: self.seat.chain_root_id.clone(),
                thread_id: self.seat.thread_id.clone(),
                producer_incarnation: receipt.producer_incarnation.clone(),
                first_engine_seq: receipt.first_engine_seq,
                last_engine_seq: receipt.last_engine_seq,
                event_count: receipt.event_count as u64,
                payload_digest: receipt.payload_digest.clone(),
            };
            if identity.derive_identity().map_err(anyhow::Error::msg)? != receipt.operation_id {
                bail!("seat append receipt identity differs from its exact seat interval");
            }
            self.pending_receipt_evidence = Some(evidence);
            self.pending_receipt = Some((receipt, Vec::with_capacity(MAX_SEAT_RECEIPT_EVENTS)));
            self.pending_receipt_bytes = 0;
            return Ok(None);
        }
        if event.event_type.starts_with("seat.")
            || event.event_type.starts_with("ui_seat.append_receipt.")
        {
            bail!("unreceipted or unsupported predecessor seat activity");
        }
        if event.event_type == SEAT_PRODUCER_EVENT {
            let control: UiSeatProducer = serde_json::from_value(event.payload)
                .context("decode CAS-authenticated seat producer control")?;
            control.validate().map_err(anyhow::Error::msg)?;
            control
                .validate_subject(&self.seat.chain_root_id, &self.seat.thread_id)
                .map_err(anyhow::Error::msg)?;
            if let Some(fence) = self.pending_cancellation_control.take() {
                ryeos_ui_presentation_contract::transfer_lifecycle::verify_control_fence(
                    &self.seat, &fence, &evidence, &control,
                )
                .map_err(anyhow::Error::msg)?;
            }
            if self.latest_control.is_none() {
                if self.frontier.is_some() && self.frontier != Some(control.next_engine_seq) {
                    bail!("newest producer control disagrees with retained receipt frontier");
                }
                self.latest_control = Some((evidence.clone(), control.clone()));
            } else if let Some(newer) = self.newer_control.as_ref() {
                if self.frontier != Some(control.next_engine_seq) {
                    bail!("control transition breaks reconstructed engine frontier");
                }
                let predecessor = UiSeatControlPredecessor::Control {
                    event_hash: evidence.event_hash.clone(),
                };
                authenticate_control_transition(
                    newer,
                    Some(&control),
                    &predecessor,
                    &self.seat.presentation_id,
                    &self.seat.chain_root_id,
                    &self.seat.thread_id,
                    newer.next_engine_seq,
                )
                .map_err(anyhow::Error::msg)?;
            }
            if let Some(producer) = self.interval_producer.take()
                && (control.state != UiSeatProducerState::Active
                    || control.producer_incarnation != producer)
            {
                bail!("receipt interval has no owning active producer");
            }
            self.frontier = Some(control.next_engine_seq);
            self.newer_control = Some(control);
            return Ok(None);
        }
        if event.event_type.starts_with("ui_seat.producer.") {
            bail!("unsupported predecessor producer-control history");
        }
        if event.event_type == "ui_seat.presentation_initialized.v1" {
            if self.initialization_seen {
                bail!("duplicate signed seat initialization");
            }
            // This lifecycle reader proves the exact signed predecessor,
            // presentation subject and original root descriptor append bound.
            // Full composition validation remains with the UI seat owner.
            let initial = UiPresentationInitializationBound::from_payload(
                &event.payload,
                &self.seat.presentation_id,
            )
            .map_err(anyhow::Error::msg)?;
            self.original_append_bound = Some(initial.max_seat_append_bytes);
            self.initialized_payload = Some(
                event
                    .payload
                    .get("initialization")
                    .cloned()
                    .ok_or_else(|| anyhow!("original signed initialization absent"))?,
            );
            self.initialized_root_binding = Some(
                event
                    .payload
                    .pointer("/initialization/root_binding")
                    .cloned()
                    .ok_or_else(|| anyhow!("signed initialization root binding absent"))?,
            );
            let newer = self
                .newer_control
                .as_ref()
                .ok_or_else(|| anyhow!("seat history has no producer control"))?;
            let predecessor = UiSeatControlPredecessor::Initialization {
                event_hash: evidence.event_hash,
            };
            authenticate_control_transition(
                newer,
                None,
                &predecessor,
                &self.seat.presentation_id,
                &self.seat.chain_root_id,
                &self.seat.thread_id,
                newer.next_engine_seq,
            )
            .map_err(anyhow::Error::msg)?;
            if self.frontier != Some(0) {
                bail!("initial producer frontier is not zero");
            }
            self.initialization_seen = true;
            return Ok(None);
        }
        if event
            .event_type
            .starts_with("contract.ryeos_ui.move_freeze_cancelled")
        {
            let cancellation =
                ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(
                    &event.to_value(),
                )
                .map_err(anyhow::Error::msg)?;
            if !same_stable_seat(&cancellation.transfer.source, &self.seat) {
                bail!("freeze cancellation belongs to another stable source seat");
            }
            self.authenticate_cancellation_witness(&cancellation)?;
            return Ok(None);
        }
        let Some((record, lifecycle)) = decode_transfer_phase(&event)? else {
            return Ok(None);
        };
        if !same_stable_seat(record.event_owner(), &self.seat) {
            bail!("transfer testimony belongs to another stable seat");
        }
        Ok(Some(RetainedPhase {
            evidence,
            record,
            lifecycle,
        }))
    }

    pub(super) fn authenticate_cancellation_witness(
        &mut self,
        cancellation: &ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
    ) -> anyhow::Result<()> {
        use ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellationWriter;
        match &cancellation.writer {
            MoveFreezeCancellationWriter::CurrentProducer {
                producer_incarnation,
                next_engine_seq,
            } => {
                if self
                    .frontier
                    .is_some_and(|frontier| frontier != *next_engine_seq)
                    || self
                        .interval_producer
                        .as_ref()
                        .is_some_and(|incarnation| incarnation != producer_incarnation)
                {
                    bail!("cancellation producer witness contradicts its event-time frontier");
                }
                // Nonadvancing pending constraints. Older validated receipts
                // and an ACTIVE historical control must independently close
                // these claims before any completed proof is issued.
                self.frontier = Some(*next_engine_seq);
                self.interval_producer = Some(producer_incarnation.clone());
            }
            MoveFreezeCancellationWriter::NodeLifecycle { fence } => {
                if self
                    .pending_cancellation_control
                    .as_ref()
                    .is_some_and(|prior| prior != fence)
                {
                    bail!("cancellations contradict the same historical control interval");
                }
                self.pending_cancellation_control = Some(fence.clone());
            }
        }
        Ok(())
    }

    /// Retain only the selected operation under its existing bounded phase
    /// policy. Shared decoding checks the outer phase-owner thread/root;
    /// only this routing step binds the full selected two-seat identity,
    /// including presentation, producer incarnation and origin revision.
    #[cfg(test)]
    pub(super) fn route_operation_phase(
        &mut self,
        transfer: &TransferIdentity,
        phase: RetainedPhase,
    ) -> anyhow::Result<()> {
        route_phase_into_slots(transfer, &self.seat, &mut self.phases, phase)
    }

    pub(super) fn finish_receipt(&mut self) -> anyhow::Result<()> {
        let (receipt, mut events) = self
            .pending_receipt
            .take()
            .ok_or_else(|| anyhow!("missing seat receipt"))?;
        self.pending_receipt_bytes = 0;
        events.reverse();
        let semantic = events
            .iter()
            .map(|event| {
                let engine_seq = event
                    .payload
                    .get("seq")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| anyhow!("seat event lacks engine sequence"))?;
                let payload = event
                    .payload
                    .get("payload")
                    .ok_or_else(|| anyhow!("seat event lacks semantic payload"))?;
                Ok(SeatAppendSemanticEvent {
                    engine_seq,
                    event_type: &event.event_type,
                    payload,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        receipt
            .validate_semantic_interval(&semantic, MAX_SEAT_RECEIPT_EVENTS)
            .map_err(anyhow::Error::msg)?;
        if self.frontier.is_some() && self.frontier != receipt.last_engine_seq.checked_add(1) {
            bail!("receipt interval breaks authenticated producer frontier");
        }
        if self
            .interval_producer
            .as_ref()
            .is_some_and(|prior| prior != &receipt.producer_incarnation)
        {
            bail!("receipt intervals switch producer without control");
        }
        self.validated_receipt = Some((
            self.pending_receipt_evidence
                .take()
                .ok_or_else(|| anyhow!("validated receipt has no exact signed evidence"))?,
            receipt.clone(),
        ));
        self.interval_producer = Some(receipt.producer_incarnation);
        self.frontier = Some(receipt.first_engine_seq);
        self.validated_receipt_events = events;
        Ok(())
    }

    pub(super) fn finish(&self) -> anyhow::Result<()> {
        if self.pending_receipt.is_some()
            || self.pending_cancellation_control.is_some()
            || !self.initialization_seen
            || self.original_append_bound.is_none()
            || self.interval_producer.is_some()
        {
            bail!("seat history ends with incomplete receipt or control ancestry");
        }
        Ok(())
    }
}

pub(super) fn route_phase_into_slots(
    transfer: &TransferIdentity,
    owner: &TransferSeat,
    phases: &mut Vec<RetainedPhase>,
    phase: RetainedPhase,
) -> anyhow::Result<()> {
    let record = &phase.record;
    if record.transfer.operation_id != transfer.operation_id {
        return Ok(());
    }
    if record.transfer != *transfer || record.event_owner() != owner {
        bail!("transfer operation changed identity or exact phase owner");
    }
    if phases.iter().any(|prior| {
        std::mem::discriminant(&prior.record.phase) == std::mem::discriminant(&record.phase)
    }) {
        bail!("transfer history repeats a settled phase");
    }
    // Eight ordinary phases plus the inbound; OrphanCancelled is
    // exclusive with Prepared and all its successors.
    if phases.len() >= 9 {
        bail!("transfer history exceeds bounded phase slots");
    }
    phases.push(phase);
    Ok(())
}

pub(super) fn verify_phase_slice_fences(
    latest_control: Option<&(TransferEvidence, UiSeatProducer)>,
    phases: &[RetainedPhase],
) -> anyhow::Result<()> {
    for phase in phases {
        if let Some(lifecycle) = &phase.lifecycle {
            let (fence, control) = latest_control
                .ok_or_else(|| anyhow!("lifecycle transfer has no retained producer fence"))?;
            verify_latest_control(lifecycle, fence, control).map_err(anyhow::Error::msg)?;
            if fence.chain_seq >= phase.evidence.chain_seq {
                bail!("lifecycle transfer precedes its producer fence");
            }
        } else if latest_control.is_some_and(|(fence, control)| {
            control.state
                == ryeos_ui_presentation_contract::seat_control::UiSeatProducerState::Closed
                && phase.evidence.chain_seq > fence.chain_seq
        }) {
            bail!("browser transfer testimony follows its Closed producer fence");
        }
    }
    Ok(())
}

pub(super) fn phase_order(phase: &TransferPhase) -> u8 {
    match phase {
        TransferPhase::InboundCommitment => 0,
        TransferPhase::OrphanCancelled { .. } | TransferPhase::Prepared { .. } => 1,
        TransferPhase::Accepted { .. } => 2,
        TransferPhase::Committed { .. } | TransferPhase::Aborted { .. } => 3,
        TransferPhase::Activated { .. }
        | TransferPhase::Retained { .. }
        | TransferPhase::Released { .. } => 4,
        TransferPhase::SettlementObserved { .. } => 5,
    }
}

pub(super) fn require_complete_settlement_read(
    poisoned: bool,
    source: bool,
    destination: bool,
) -> anyhow::Result<()> {
    if poisoned || !source || !destination {
        bail!("transfer settlement requires both complete authenticated histories");
    }
    Ok(())
}

pub(super) fn fold_transfer_phase_slices(
    transfer: &TransferIdentity,
    source: &[RetainedPhase],
    destination: &[RetainedPhase],
) -> anyhow::Result<TransferLifecycleState> {
    let mut phases: Vec<_> = source.iter().chain(destination).collect();
    // Topological consistency order only, never global publication order.
    // Prepared links the exact destination inbound commitment. Typed Commit
    // evidence authenticates the exact Accepted object, but Abort links only
    // Prepared: sorting cannot prove an Accepted did not arrive
    // after Abort. Source Abort therefore needs both fixed heads unchanged at
    // publication under the ordered two-chain locks.
    phases.sort_by_key(|phase| phase_order(&phase.record.phase));
    let first = phases
        .first()
        .ok_or_else(|| anyhow!("transfer inbound commitment is absent"))?;
    let mut state = TransferLifecycleState::begin(first.testimony()).map_err(anyhow::Error::msg)?;
    let mut last_source = None;
    let mut last_destination = None;
    for phase in phases {
        let previous = if phase.record.event_owner() == &transfer.source {
            &mut last_source
        } else {
            &mut last_destination
        };
        if previous.is_some_and(|previous| phase.evidence.chain_seq <= previous) {
            bail!("transfer phase order contradicts its owning chain");
        }
        *previous = Some(phase.evidence.chain_seq);
        state.fold(phase.testimony()).map_err(anyhow::Error::msg)?;
    }
    Ok(state)
}

/// Complete, immutable, two-seat CAS proof at the exact two captured signed
/// heads. This is read-only and carries no live session, producer, reservation
/// or publication authority. A writer must condition on BOTH returned anchors
/// and independently recheck its mutable domain admission before publication.
pub struct UiTransferFixedHistoryProof {
    store: Arc<StateStore>,
    source_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    destination_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    lifecycle: TransferLifecycleState,
}

/// Completed paired absence plus exact receipted freeze witnesses. This is
/// read testimony only: publication must recheck both anchors and independently
/// admit the current producer or durable lifecycle fence under existing locks.
pub struct UiMoveFreezeCancellationProof {
    store: Arc<StateStore>,
    source_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    destination_anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    transfer: TransferIdentity,
    freeze: ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellationEvidence,
    retained: Option<(
        TransferEvidence,
        ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
    )>,
    destination_inbound: Option<(TransferEvidence, TransferRecord)>,
    destination_orphan_cancelled: Option<(TransferEvidence, TransferRecord)>,
    original_append_bound: u64,
    latest_control: (TransferEvidence, UiSeatProducer),
    next_engine_seq: u64,
}

impl UiMoveFreezeCancellationProof {
    /// Positive orphan decision from the completed exact destination prefix.
    /// It never substitutes for the paired source-no-Prepared coverage.
    pub fn destination_orphan_cancelled(&self) -> Option<(&TransferEvidence, &TransferRecord)> {
        self.destination_orphan_cancelled
            .as_ref()
            .map(|(evidence, record)| (evidence, record))
    }
    pub fn original_append_bound(&self) -> u64 {
        self.original_append_bound
    }
    pub fn latest_control(&self) -> (&TransferEvidence, &UiSeatProducer) {
        (&self.latest_control.0, &self.latest_control.1)
    }
    pub fn next_engine_seq(&self) -> u64 {
        self.next_engine_seq
    }
    pub fn belongs_to(&self, store: &StateStore) -> bool {
        std::ptr::eq(self.store.as_ref(), store)
    }
    pub fn source_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.source_anchor
    }
    pub fn destination_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.destination_anchor
    }
    pub fn transfer(&self) -> &TransferIdentity {
        &self.transfer
    }
    pub fn freeze(
        &self,
    ) -> &ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellationEvidence
    {
        &self.freeze
    }
    pub fn retained_cancellation(
        &self,
    ) -> Option<(
        &TransferEvidence,
        &ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
    )> {
        self.retained
            .as_ref()
            .map(|(evidence, record)| (evidence, record))
    }
    pub fn destination_inbound(&self) -> Option<(&TransferEvidence, &TransferRecord)> {
        self.destination_inbound
            .as_ref()
            .map(|(evidence, record)| (evidence, record))
    }
}

/// An ordinary freeze event of a source seat together with the receipt that
/// covers it, retained so a replay can re-verify the export.
#[derive(Clone)]
pub struct UiHistoricalReceiptedFreezeWitness {
    pub evidence: TransferEvidence,
    pub event: ThreadEvent,
    pub receipt_evidence: TransferEvidence,
    pub receipt: SeatAppendReceipt,
}

pub(super) struct RequestedSeatOperation {
    pub(super) freeze_witnesses: BTreeMap<String, UiHistoricalReceiptedFreezeWitness>,
    pub(super) transfer: TransferIdentity,
    pub(super) owner: TransferSeat,
    pub(super) phases: Vec<RetainedPhase>,
    pub(super) freeze_preparation: Option<(
        TransferEvidence,
        ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezePreparationCommitment,
    )>,
    pub(super) freeze_completion: Option<(TransferEvidence, serde_json::Value)>,
    pub(super) cancellation: Option<(
        TransferEvidence,
        ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
    )>,
}

pub(super) fn same_stable_seat(left: &TransferSeat, right: &TransferSeat) -> bool {
    left.presentation_id == right.presentation_id
        && left.chain_root_id == right.chain_root_id
        && left.thread_id == right.thread_id
}

pub(super) fn freeze_requested_seat_operations(
    seat: &TransferSeat,
    requested: &[TransferIdentity],
) -> anyhow::Result<BTreeMap<String, RequestedSeatOperation>> {
    let mut operations: BTreeMap<String, RequestedSeatOperation> = BTreeMap::new();
    for transfer in requested {
        TransferRecord {
            schema: ryeos_ui_presentation_contract::transfer::TRANSFER_SCHEMA,
            transfer: transfer.clone(),
            phase: TransferPhase::InboundCommitment,
        }
        .validate_shape()
        .map_err(anyhow::Error::msg)?;
        let owner = if same_stable_seat(seat, &transfer.source) {
            &transfer.source
        } else if same_stable_seat(seat, &transfer.destination) {
            &transfer.destination
        } else {
            bail!("requested transfer does not belong to the exact fixed seat");
        };
        if let Some(prior) = operations.get(&transfer.operation_id) {
            if prior.transfer != *transfer {
                bail!("requested transfer operation has conflicting identities");
            }
            continue;
        }
        operations.insert(
            transfer.operation_id.clone(),
            RequestedSeatOperation {
                freeze_witnesses: BTreeMap::new(),
                transfer: transfer.clone(),
                owner: owner.clone(),
                phases: Vec::new(),
                freeze_preparation: None,
                freeze_completion: None,
                cancellation: None,
            },
        );
    }
    Ok(operations)
}

pub(super) fn route_requested_seat_phase(
    requested: &mut BTreeMap<String, RequestedSeatOperation>,
    phase: RetainedPhase,
) -> anyhow::Result<()> {
    if let Some(operation) = requested.get_mut(&phase.record.transfer.operation_id) {
        route_phase_into_slots(
            &operation.transfer,
            &operation.owner,
            &mut operation.phases,
            phase,
        )?;
    }
    Ok(())
}

pub(super) fn route_receipted_freeze_event(
    requested: &mut BTreeMap<String, RequestedSeatOperation>,
    event: &ThreadEvent,
    receipt_producer: &str,
    receipt: &(TransferEvidence, SeatAppendReceipt),
) -> anyhow::Result<()> {
    use ryeos_ui_presentation_contract::move_freeze_lifecycle::{
        preparation_commitment, validate_completion_commitment,
    };
    let semantic = event
        .payload
        .get("payload")
        .ok_or_else(|| anyhow!("receipted producer semantic payload absent"))?;
    let evidence = || -> anyhow::Result<TransferEvidence> {
        Ok(TransferEvidence {
            event_hash: lillux::sha256_hex(lillux::canonical_json(&event.to_value())?.as_bytes()),
            chain_seq: event.chain_seq,
        })
    };
    if event.event_type == "seat.presentation_operation.prepared" {
        let Some(commitment) = preparation_commitment(semantic).map_err(anyhow::Error::msg)? else {
            return Ok(());
        };
        let Some(operation) = requested.get_mut(&commitment.transfer.operation_id) else {
            return Ok(());
        };
        if commitment.transfer != operation.transfer
            || operation.owner != operation.transfer.source
            || commitment.transfer.source.producer_incarnation != receipt_producer
            || event.payload.get("seq").and_then(serde_json::Value::as_u64)
                != Some(commitment.prepared_engine_seq)
            || operation.freeze_preparation.is_some()
        {
            bail!("receipted freeze preparation conflicts with exact selected source");
        }
        if let Some((_, completion)) = &operation.freeze_completion {
            validate_completion_commitment(completion, &commitment).map_err(anyhow::Error::msg)?;
        }
        let proof_evidence = evidence()?;
        operation.freeze_witnesses.insert(
            proof_evidence.event_hash.clone(),
            UiHistoricalReceiptedFreezeWitness {
                evidence: proof_evidence.clone(),
                event: event.clone(),
                receipt_evidence: receipt.0.clone(),
                receipt: receipt.1.clone(),
            },
        );
        operation.freeze_preparation = Some((proof_evidence, commitment));
    } else if event.event_type == "seat.presentation_operation.completed" {
        let Some(id) = semantic
            .pointer("/result/transfer_operation_id")
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(());
        };
        let Some(operation) = requested.get_mut(id) else {
            return Ok(());
        };
        if operation.owner != operation.transfer.source || operation.freeze_completion.is_some() {
            bail!("receipted freeze completion conflicts with selected source");
        }
        if let Some((_, preparation)) = &operation.freeze_preparation {
            validate_completion_commitment(semantic, preparation).map_err(anyhow::Error::msg)?;
        }
        let proof_evidence = evidence()?;
        operation.freeze_witnesses.insert(
            proof_evidence.event_hash.clone(),
            UiHistoricalReceiptedFreezeWitness {
                evidence: proof_evidence.clone(),
                event: event.clone(),
                receipt_evidence: receipt.0.clone(),
                receipt: receipt.1.clone(),
            },
        );
        operation.freeze_completion = Some((proof_evidence, semantic.clone()));
    }
    Ok(())
}

pub(super) fn consume_selected_seat_event<A>(
    history: &mut SeatHistory<A>,
    requested: &mut BTreeMap<String, RequestedSeatOperation>,
    hash: String,
    event: ThreadEvent,
) -> anyhow::Result<()> {
    if event.event_type
        == ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT
    {
        let cancellation = ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(
            &event.to_value(),
        )
        .map_err(anyhow::Error::msg)?;
        if let Some(operation) = requested.get_mut(&cancellation.transfer.operation_id) {
            if operation.transfer != cancellation.transfer
                || operation.owner != operation.transfer.source
                || operation.cancellation.is_some()
            {
                bail!("cancellation conflicts with exact requested source identity");
            }
            operation.cancellation = Some((
                TransferEvidence {
                    event_hash: hash.clone(),
                    chain_seq: event.chain_seq,
                },
                cancellation,
            ));
        }
    }
    if let Some(phase) = history.validate_seat_event(hash, event)? {
        route_requested_seat_phase(requested, phase)?;
    }
    for receipted in std::mem::take(&mut history.validated_receipt_events) {
        route_receipted_freeze_event(
            requested,
            &receipted,
            history
                .interval_producer
                .as_deref()
                .ok_or_else(|| anyhow!("validated receipt producer absent"))?,
            history
                .validated_receipt
                .as_ref()
                .ok_or_else(|| anyhow!("validated receipt attribution absent"))?,
        )?;
    }
    Ok(())
}

pub(super) fn verify_selected_freeze(operation: &RequestedSeatOperation) -> anyhow::Result<()> {
    use ryeos_ui_presentation_contract::move_freeze_lifecycle::validate_completion_commitment;
    match (&operation.freeze_preparation, &operation.freeze_completion) {
        (Some((preparation_evidence, preparation)), Some((completion_evidence, completion))) => {
            validate_completion_commitment(completion, preparation).map_err(anyhow::Error::msg)?;
            if preparation_evidence.chain_seq >= completion_evidence.chain_seq {
                bail!("freeze completion precedes exact preparation");
            }
            if let Some((evidence, cancellation)) = &operation.cancellation
                && (cancellation.freeze.preparation != *preparation_evidence
                    || cancellation.freeze.completion.as_ref() != Some(completion_evidence)
                    || evidence.chain_seq <= completion_evidence.chain_seq
                    || operation
                        .phases
                        .iter()
                        .any(|phase| matches!(phase.record.phase, TransferPhase::Prepared { .. })))
            {
                bail!("freeze cancellation conflicts with authenticated obligation or Prepared");
            }
        }
        (None, None) if operation.cancellation.is_none() => {}
        (Some((preparation_evidence, _)), None) => {
            if let Some((evidence, cancellation)) = &operation.cancellation
                && (cancellation.freeze.preparation != *preparation_evidence
                    || cancellation.freeze.completion.is_some()
                    || evidence.chain_seq <= preparation_evidence.chain_seq
                    || operation
                        .phases
                        .iter()
                        .any(|phase| matches!(phase.record.phase, TransferPhase::Prepared { .. })))
            {
                bail!(
                    "pending freeze cancellation conflicts with authenticated obligation or Prepared"
                );
            }
        }
        _ => bail!("freeze completion or cancellation has no exact receipted preparation"),
    }
    for phase in &operation.phases {
        if let TransferPhase::Prepared {
            freeze: Some(links),
            ..
        } = &phase.record.phase
            && (operation
                .freeze_preparation
                .as_ref()
                .map(|(evidence, _)| evidence)
                != Some(&links.preparation)
                || operation
                    .freeze_completion
                    .as_ref()
                    .map(|(evidence, _)| evidence)
                    != Some(&links.completion)
                || links.completion.chain_seq >= phase.evidence.chain_seq)
        {
            bail!("MOVE Prepared has no exact earlier receipted freeze witnesses");
        }
    }
    Ok(())
}

/// Read one supplied fixed signed seat cut once for a frozen set of requested
/// transfer identities. Private state is process-local, never a registry or
/// admission token. Memory is O(requested operations), with at most nine phase
/// slots per operation plus one protocol-bounded receipt; resident lifetime is
/// not bounded by this API. No arbitrary operation cap or eviction is applied.
pub struct UiTransferFixedSeatRead {
    store: Arc<StateStore>,
    history: SeatHistory,
    requested: BTreeMap<String, RequestedSeatOperation>,
    poisoned: bool,
    endpoint: Option<PendingFixedSeatEndpoint>,
    predecessor: Option<Arc<UiTransferFixedSeatProof>>,
    joined_ancestry: bool,
}

impl UiTransferFixedSeatRead {
    /// Scheduling metadata only: this distinguishes an attempted suffix join
    /// from a cold scan, not completed coverage or admission authority.
    pub fn is_suffix_read(&self) -> bool {
        self.predecessor.is_some()
    }
}

pub(super) struct PendingFixedSeatEndpoint {
    pub(super) tip: TransferEvidence,
    pub(super) thread_seq: u64,
    pub(super) next_engine_seq: Option<u64>,
}

pub(super) struct FixedSeatEndpoint {
    pub(super) tip: TransferEvidence,
    pub(super) thread_seq: u64,
    pub(super) next_engine_seq: u64,
}

/// Opaque completed coverage of exactly the identities requested at begin.
/// Missing coverage is not absence evidence. Control and initialization prove
/// ancestry/final-control fences, not composition revision, event-time witness,
/// session expiry or live producer/publication authority.
pub struct UiTransferFixedSeatProof {
    store: Arc<StateStore>,
    history: SeatHistory,
    requested: BTreeMap<String, RequestedSeatOperation>,
    endpoint: FixedSeatEndpoint,
    joined_ancestry: bool,
}

impl UiTransferFixedSeatProof {
    /// Classify exact selected source testimony from this completed same-store
    /// cut before a caller attempts positive lifecycle assembly. An inbound
    /// destination phase cannot hide a retained source cancellation.
    pub fn selected_retained_cancellation(
        &self,
        store: &StateStore,
        transfer: &TransferIdentity,
    ) -> anyhow::Result<bool> {
        if !std::ptr::eq(self.store.as_ref(), store)
            || !same_stable_seat(&self.history.seat, &transfer.source)
        {
            bail!("cancellation classification requires exact completed source cut");
        }
        Ok(
            covered_seat_operation(&self.history.seat, &self.requested, transfer)?
                .cancellation
                .is_some(),
        )
    }

    /// Completed exact selected source cut, checked by the pair writer before
    /// it rechecks the identical anchor under publication locks. A cancellation
    /// remains permanent even after producer replacement or unrelated edits.
    pub fn validate_preparation_source(
        &self,
        store: &StateStore,
        anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        transfer: &TransferIdentity,
    ) -> anyhow::Result<()> {
        if !std::ptr::eq(self.store.as_ref(), store)
            || &self.history.anchor != anchor
            || !same_stable_seat(&self.history.seat, &transfer.source)
        {
            bail!("preparation requires completed same-store exact source cut");
        }
        let operation = covered_seat_operation(&self.history.seat, &self.requested, transfer)?;
        if operation.cancellation.is_some() {
            bail!("transfer preparation permanently refused by retained freeze cancellation");
        }
        if !operation.phases.is_empty() {
            bail!("transfer preparation repeats an existing source phase");
        }
        Ok(())
    }

    /// Derive mandatory MOVE links only from this completed receipted source
    /// cut. The UI owner still verifies the full typed Core export at its
    /// original event position; this method does not admit renderer contents.
    pub fn preparation_freeze_links(
        &self,
        store: &StateStore,
        anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        transfer: &TransferIdentity,
    ) -> anyhow::Result<ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence> {
        self.validate_preparation_source(store, anchor, transfer)?;
        let operation = covered_seat_operation(&self.history.seat, &self.requested, transfer)?;
        verify_selected_freeze(operation)?;
        let (preparation, _) = operation
            .freeze_preparation
            .as_ref()
            .ok_or_else(|| anyhow!("MOVE preparation has no receipted freeze preparation"))?;
        let (completion, _) = operation
            .freeze_completion
            .as_ref()
            .ok_or_else(|| anyhow!("MOVE preparation has no receipted freeze completion"))?;
        Ok(
            ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence {
                preparation: preparation.clone(),
                completion: completion.clone(),
            },
        )
    }

    /// The exact signed anchor this completed cut was read at.
    pub fn anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.history.anchor
    }

    /// The receipted ordinary event this completed source cut authenticated
    /// for the requested transfer, by its exact evidence.
    pub fn freeze_witness(
        &self,
        transfer: &TransferIdentity,
        evidence: &TransferEvidence,
    ) -> anyhow::Result<Option<&UiHistoricalReceiptedFreezeWitness>> {
        let operation = covered_seat_operation(&self.history.seat, &self.requested, transfer)?;
        Ok(operation
            .freeze_witnesses
            .get(&evidence.event_hash)
            .filter(|witness| witness.evidence == *evidence))
    }

    pub fn validate_preparation_destination(
        &self,
        store: &StateStore,
        anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        transfer: &TransferIdentity,
    ) -> anyhow::Result<()> {
        if !std::ptr::eq(self.store.as_ref(), store)
            || &self.history.anchor != anchor
            || !same_stable_seat(&self.history.seat, &transfer.destination)
        {
            bail!("preparation requires completed same-store exact destination cut");
        }
        let operation = covered_seat_operation(&self.history.seat, &self.requested, transfer)?;
        if !operation.phases.is_empty() || operation.cancellation.is_some() {
            bail!("transfer preparation repeats an existing destination phase");
        }
        Ok(())
    }
    pub(super) fn has_completed_ancestry(&self) -> bool {
        self.history.complete
            && (self.joined_ancestry || self.history.initialization_seen)
            && self.history.original_append_bound.is_some()
    }
}

pub fn begin_fixed_seat_read(
    store: Arc<StateStore>,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    seat: &TransferSeat,
    requested: &[TransferIdentity],
) -> anyhow::Result<UiTransferFixedSeatRead> {
    // TransferSeat's revision/incarnation are deliberately NOT context here:
    // each operation retains its own immutable origin in requested coverage.
    if uuid::Uuid::parse_str(&seat.presentation_id)
        .ok()
        .is_none_or(|id| id.to_string() != seat.presentation_id)
        || [&seat.chain_root_id, &seat.thread_id]
            .into_iter()
            .any(|id| id.is_empty() || id.trim() != id || id.chars().any(char::is_control))
        || anchor.chain_root_id() != seat.chain_root_id
        || anchor.thread_id() != seat.thread_id
    {
        bail!("fixed seat context differs from its exact signed anchor");
    }
    let requested = freeze_requested_seat_operations(seat, requested)?;
    Ok(UiTransferFixedSeatRead {
        store,
        history: SeatHistory::fresh(seat.clone(), anchor),
        requested,
        poisoned: false,
        endpoint: None,
        predecessor: None,
        joined_ancestry: false,
    })
}

/// Continue the supplied newer cut only when one completed predecessor covers
/// every request. Missing scope or an ineligible predecessor starts a cold
/// read of this SAME supplied anchor; divergence during an attempted join is
/// an error, never a silent new-head recapture or a suffix-absence result.
pub fn begin_fixed_seat_read_from_proof(
    store: Arc<StateStore>,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    seat: &TransferSeat,
    requested: &[TransferIdentity],
    proof: Arc<UiTransferFixedSeatProof>,
) -> anyhow::Result<UiTransferFixedSeatRead> {
    let mut read = begin_fixed_seat_read(store, anchor, seat, requested)?;
    let advancing = read
        .history
        .anchor
        .tip()
        .is_some_and(|(_, seq)| seq > proof.endpoint.thread_seq);
    if !Arc::ptr_eq(&read.store, &proof.store)
        || !proof.has_completed_ancestry()
        || !same_stable_seat(seat, &proof.history.seat)
        || !advancing
        || read.requested.values().any(|operation| {
            covered_seat_operation(seat, &proof.requested, &operation.transfer).is_err()
        })
    {
        return Ok(read);
    }
    // Preserve the single predecessor's broad coverage, even if this caller
    // requested a subset. Do not union independently read proofs.
    let broad = proof
        .requested
        .values()
        .map(|operation| operation.transfer.clone())
        .collect::<Vec<_>>();
    read.requested = freeze_requested_seat_operations(seat, &broad)?;
    read.predecessor = Some(proof);
    Ok(read)
}

pub(super) fn finish_fixed_seat_join<A, B>(
    history: &mut SeatHistory<A>,
    cached: &SeatHistory<B>,
    endpoint: &FixedSeatEndpoint,
    hash: &str,
    event: &ThreadEvent,
) -> anyhow::Result<()> {
    if event.chain_root_id != history.seat.chain_root_id
        || event.thread_id != history.seat.thread_id
        || !same_stable_seat(&history.seat, &cached.seat)
        || hash != endpoint.tip.event_hash
        || event.thread_seq != endpoint.thread_seq
        || event.chain_seq != endpoint.tip.chain_seq
        || history.previous_thread_seq != event.thread_seq.checked_add(1)
        || history
            .previous_chain_seq
            .is_none_or(|newer| newer <= event.chain_seq)
    {
        bail!("fixed seat suffix does not join its exact completed endpoint");
    }
    if history.pending_receipt.is_some() || history.pending_receipt_bytes != 0 {
        bail!("fixed seat suffix joins inside an incomplete receipt interval");
    }
    if history.initialization_seen {
        bail!("fixed seat suffix contains a new initialization");
    }
    if history
        .frontier
        .is_some_and(|frontier| frontier != endpoint.next_engine_seq)
    {
        bail!("fixed seat suffix frontier differs from completed predecessor");
    }
    let (fence, control) = cached
        .latest_control
        .as_ref()
        .ok_or_else(|| anyhow!("completed fixed seat predecessor has no producer control"))?;
    if let Some(pending) = history.pending_cancellation_control.take() {
        ryeos_ui_presentation_contract::transfer_lifecycle::verify_control_fence(
            &history.seat,
            &pending,
            fence,
            control,
        )
        .map_err(anyhow::Error::msg)?;
    }
    if history
        .interval_producer
        .as_ref()
        .is_some_and(|incarnation| {
            control.state != UiSeatProducerState::Active
                || incarnation != &control.producer_incarnation
        })
    {
        bail!("fixed seat suffix receipt has no matching predecessor producer");
    }
    if let Some(newer) = &history.newer_control {
        authenticate_control_transition(
            newer,
            Some(control),
            &UiSeatControlPredecessor::Control {
                event_hash: fence.event_hash.clone(),
            },
            &history.seat.presentation_id,
            &history.seat.chain_root_id,
            &history.seat.thread_id,
            newer.next_engine_seq,
        )
        .map_err(anyhow::Error::msg)?;
    }
    let append_bound = cached
        .original_append_bound
        .ok_or_else(|| anyhow!("completed fixed seat predecessor has no initialization bound"))?;
    // Inherit authenticated ancestry only AFTER the exact join and frontier,
    // producer and control-predecessor checks. Do not fake genesis folding.
    if history.latest_control.is_none() {
        history.latest_control = cached.latest_control.clone();
    }
    history.original_append_bound = Some(append_bound);
    history.initialized_root_binding = cached.initialized_root_binding.clone();
    Ok(())
}

pub fn read_fixed_seat_page(
    read: &mut UiTransferFixedSeatRead,
    max_events: u64,
    max_bytes: u64,
) -> anyhow::Result<bool> {
    if read.poisoned {
        bail!("transfer settlement read previously failed");
    }
    let result = (|| {
        if read.history.complete {
            return Ok(true);
        }
        let page = read.store.read_thread_event_page(
            &read.history.anchor,
            read.history.progress.as_ref(),
            max_events,
            max_bytes,
        )?;
        if page.events.is_empty() && page.progress.is_some() {
            bail!("transfer settlement history made no progress");
        }
        for (hash, event) in page.events {
            if read.endpoint.is_none() {
                if read.history.anchor.tip() != Some((hash.as_str(), event.thread_seq)) {
                    bail!("fixed seat read does not begin at its supplied signed endpoint");
                }
                read.endpoint = Some(PendingFixedSeatEndpoint {
                    tip: TransferEvidence {
                        event_hash: hash.clone(),
                        chain_seq: event.chain_seq,
                    },
                    thread_seq: event.thread_seq,
                    next_engine_seq: None,
                });
            }
            if let Some(cached) = &read.predecessor
                && (event.thread_seq <= cached.endpoint.thread_seq
                    || hash == cached.endpoint.tip.event_hash)
            {
                // The cached tip is a joining witness, not a new event.
                // Never fold it again or continue into its older history.
                finish_fixed_seat_join(
                    &mut read.history,
                    &cached.history,
                    &cached.endpoint,
                    &hash,
                    &event,
                )?;
                for operation in cached.requested.values() {
                    for phase in &operation.phases {
                        route_requested_seat_phase(&mut read.requested, phase.clone())?;
                    }
                    let target = read
                        .requested
                        .get_mut(&operation.transfer.operation_id)
                        .expect("cached broad coverage");
                    if target.freeze_preparation.is_some() && operation.freeze_preparation.is_some()
                        || target.freeze_completion.is_some()
                            && operation.freeze_completion.is_some()
                        || target.cancellation.is_some() && operation.cancellation.is_some()
                    {
                        bail!("suffix duplicates retained freeze testimony");
                    }
                    if target.freeze_preparation.is_none() {
                        target.freeze_preparation = operation.freeze_preparation.clone();
                    }
                    if target.freeze_completion.is_none() {
                        target.freeze_completion = operation.freeze_completion.clone();
                    }
                    if target.cancellation.is_none() {
                        target.cancellation = operation.cancellation.clone();
                    }
                }
                let endpoint = read.endpoint.as_mut().expect("captured supplied endpoint");
                if endpoint.next_engine_seq.is_none() {
                    endpoint.next_engine_seq = Some(cached.endpoint.next_engine_seq);
                }
                read.joined_ancestry = true;
                read.history.complete = true;
                read.history.progress = None;
                return Ok(true);
            }
            // The first RECEIPT frontier is trusted only when its complete
            // interval passes semantic validation. It can precede the newest
            // control in reverse history, whose original frontier is older.
            let pending_head_frontier = read
                .history
                .pending_receipt
                .as_ref()
                .map(|(receipt, _)| receipt.last_engine_seq.checked_add(1));
            consume_selected_seat_event(&mut read.history, &mut read.requested, hash, event)?;
            let endpoint = read.endpoint.as_mut().expect("captured supplied endpoint");
            if endpoint.next_engine_seq.is_none() && read.history.pending_receipt.is_none() {
                if let Some(frontier) = pending_head_frontier {
                    endpoint.next_engine_seq = Some(frontier.ok_or_else(|| {
                        anyhow!("fixed seat endpoint engine frontier is exhausted")
                    })?);
                } else if let Some((_, control)) = &read.history.latest_control {
                    endpoint.next_engine_seq = Some(control.next_engine_seq);
                }
            }
        }
        read.history.complete = page.progress.is_none();
        if read.history.complete {
            if read.predecessor.is_some() {
                bail!("fixed seat suffix reached genesis without its completed endpoint");
            }
            read.history.finish()?;
        }
        read.history.progress = page.progress;
        Ok(read.history.complete)
    })();
    if result.is_err() {
        read.poisoned = true;
    }
    result
}

pub fn finish_fixed_seat_read(
    read: UiTransferFixedSeatRead,
) -> anyhow::Result<UiTransferFixedSeatProof> {
    require_complete_settlement_read(read.poisoned, read.history.complete, true)?;
    if read.predecessor.is_some() {
        if !read.joined_ancestry || read.history.pending_receipt.is_some() {
            bail!("fixed seat suffix has no complete authenticated join");
        }
    } else {
        read.history.finish()?;
    }
    for operation in read.requested.values() {
        verify_phase_slice_fences(read.history.latest_control.as_ref(), &operation.phases)?;
        verify_selected_freeze(operation)?;
        if let Some((_, preparation)) = &operation.freeze_preparation
            && read.history.initialized_root_binding.as_ref() != Some(&preparation.root_binding)
        {
            bail!("receipted freeze root differs from authenticated initialization");
        }
    }
    let endpoint = read
        .endpoint
        .ok_or_else(|| anyhow!("completed fixed seat has no signed endpoint"))?;
    let endpoint = FixedSeatEndpoint {
        tip: endpoint.tip,
        thread_seq: endpoint.thread_seq,
        next_engine_seq: endpoint
            .next_engine_seq
            .ok_or_else(|| anyhow!("completed fixed seat has no authenticated head frontier"))?,
    };
    Ok(UiTransferFixedSeatProof {
        store: read.store,
        history: read.history,
        requested: read.requested,
        endpoint,
        joined_ancestry: read.joined_ancestry,
    })
}

/// Reuse completed coverage only at the identical signed cut and state owner.
/// A miss is not absence, suffix coverage or live authority. A hit retains the
/// whole prior proof rather than narrowing or unioning its frozen requests.
pub fn reuse_fixed_seat_proof(
    store: &StateStore,
    anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    seat: &TransferSeat,
    requested: &[TransferIdentity],
    proof: &Arc<UiTransferFixedSeatProof>,
) -> anyhow::Result<Option<Arc<UiTransferFixedSeatProof>>> {
    // Validate even on a cache miss: malformed or conflicting requests must
    // not be reinterpreted as a harmless lack of cached coverage.
    let requested = freeze_requested_seat_operations(seat, requested)?;
    if !std::ptr::eq(proof.store.as_ref(), store)
        || !proof.has_completed_ancestry()
        || proof.history.anchor != *anchor
        || !same_stable_seat(&proof.history.seat, seat)
    {
        return Ok(None);
    }
    for operation in requested.values() {
        if covered_seat_operation(seat, &proof.requested, &operation.transfer).is_err() {
            return Ok(None);
        }
    }
    Ok(Some(Arc::clone(proof)))
}

pub(super) fn covered_seat_operation<'a>(
    seat: &TransferSeat,
    requested: &'a BTreeMap<String, RequestedSeatOperation>,
    transfer: &TransferIdentity,
) -> anyhow::Result<&'a RequestedSeatOperation> {
    let operation = requested
        .get(&transfer.operation_id)
        .ok_or_else(|| anyhow!("fixed seat proof did not request this transfer identity"))?;
    if operation.transfer != *transfer || !same_stable_seat(seat, &operation.owner) {
        bail!("fixed seat proof coverage differs from the exact transfer identity");
    }
    Ok(operation)
}

/// Assemble one requested operation from two independently completed cuts.
/// Reuses their frozen coverage without rescanning either seat. This topology
/// proof alone grants no composition/event-time or live mutation authority.
pub fn assemble_fixed_history_proof(
    source: &UiTransferFixedSeatProof,
    destination: &UiTransferFixedSeatProof,
    transfer: &TransferIdentity,
) -> anyhow::Result<UiTransferFixedHistoryProof> {
    if !Arc::ptr_eq(&source.store, &destination.store) {
        bail!("fixed seat proofs belong to different state owners");
    }
    if !same_stable_seat(&source.history.seat, &transfer.source)
        || !same_stable_seat(&destination.history.seat, &transfer.destination)
    {
        bail!("fixed seat proofs differ from exact source and destination seats");
    }
    let source_operation =
        covered_seat_operation(&source.history.seat, &source.requested, transfer)?;
    let destination_operation =
        covered_seat_operation(&destination.history.seat, &destination.requested, transfer)?;
    let lifecycle = fold_transfer_phase_slices(
        transfer,
        &source_operation.phases,
        &destination_operation.phases,
    )?;
    Ok(UiTransferFixedHistoryProof {
        store: source.store.clone(),
        source_anchor: source.history.anchor.clone(),
        destination_anchor: destination.history.anchor.clone(),
        lifecycle,
    })
}

pub(crate) fn completed_negative_terminal_decision(
    lifecycle: &TransferLifecycleState,
) -> Option<(TransferEvidence, TransferRecord)> {
    if lifecycle.accepted().is_some()
        || lifecycle.committed().is_some()
        || lifecycle.activated().is_some()
        || lifecycle.retained().is_some()
        || lifecycle.released().is_some()
    {
        return None;
    }
    if let Some(aborted) = lifecycle.aborted() {
        return Some((aborted.evidence.clone(), aborted.record.clone()));
    }
    if lifecycle.prepared().is_none()
        && let Some(orphan) = lifecycle.orphan_cancelled()
    {
        return Some((orphan.evidence.clone(), orphan.record.clone()));
    }
    None
}

pub(super) fn validate_negative_destination_phases(
    transfer: &TransferIdentity,
    phases: &[RetainedPhase],
) -> anyhow::Result<()> {
    if phases.iter().any(|phase| {
        !matches!(
            phase.record.phase,
            TransferPhase::InboundCommitment | TransferPhase::OrphanCancelled { .. }
        )
    }) {
        bail!("negative destination prefix has a preparation or accepted obligation");
    }
    if !phases.is_empty() {
        // Exact links/order/full identity remain checked by the existing fold;
        // merely naming an orphan phase is not negative admission.
        let lifecycle = fold_transfer_phase_slices(transfer, &[], phases)?;
        if lifecycle.prepared().is_some() || lifecycle.accepted().is_some() {
            bail!("negative destination prefix has conflicting lifecycle evidence");
        }
    }
    Ok(())
}

pub fn assemble_move_freeze_cancellation_proof(
    source: &UiTransferFixedSeatProof,
    destination: &UiTransferFixedSeatProof,
    transfer: &TransferIdentity,
) -> anyhow::Result<UiMoveFreezeCancellationProof> {
    if !Arc::ptr_eq(&source.store, &destination.store)
        || !same_stable_seat(&source.history.seat, &transfer.source)
        || !same_stable_seat(&destination.history.seat, &transfer.destination)
    {
        bail!("freeze cancellation requires same-store completed exact paired cuts");
    }
    let source_operation =
        covered_seat_operation(&source.history.seat, &source.requested, transfer)?;
    let destination_operation =
        covered_seat_operation(&destination.history.seat, &destination.requested, transfer)?;
    if !source_operation.phases.is_empty() {
        bail!("freeze cancellation has an existing transfer preparation or destination obligation");
    }
    validate_negative_destination_phases(transfer, &destination_operation.phases)?;
    let preparation = source_operation
        .freeze_preparation
        .as_ref()
        .ok_or_else(|| anyhow!("freeze cancellation has no receipted source preparation"))?;
    let completion = source_operation.freeze_completion.as_ref();
    verify_selected_freeze(source_operation)?;
    Ok(UiMoveFreezeCancellationProof {
        store: source.store.clone(),
        source_anchor: source.history.anchor.clone(),
        destination_anchor: destination.history.anchor.clone(),
        transfer: transfer.clone(),
        freeze:
            ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellationEvidence {
                preparation: preparation.0.clone(),
                completion: completion.map(|value| value.0.clone()),
            },
        retained: source_operation.cancellation.clone(),
        destination_inbound: destination_operation
            .phases
            .iter()
            .find(|phase| matches!(phase.record.phase, TransferPhase::InboundCommitment))
            .map(|phase| (phase.evidence.clone(), phase.record.clone())),
        destination_orphan_cancelled: destination_operation
            .phases
            .iter()
            .find(|phase| matches!(phase.record.phase, TransferPhase::OrphanCancelled { .. }))
            .map(|phase| (phase.evidence.clone(), phase.record.clone())),
        original_append_bound: source
            .history
            .original_append_bound
            .ok_or_else(|| anyhow!("completed source has no authenticated append bound"))?,
        latest_control: source
            .history
            .latest_control
            .clone()
            .ok_or_else(|| anyhow!("completed source has no authenticated producer control"))?,
        next_engine_seq: source.endpoint.next_engine_seq,
    })
}

/// Opaque, owner-budgeted two-seat read for browser transfer admission. A
/// partial page is never a proof, including when it contains no matching
/// phase. The reader is poisoned by any failed page and can only be finished
/// after both exact fixed histories reach genesis.
pub struct UiTransferFixedHistoryRead {
    transfer: TransferIdentity,
    source: UiTransferFixedSeatRead,
    destination: UiTransferFixedSeatRead,
    poisoned: bool,
}

pub fn begin_fixed_history_read(
    store: Arc<StateStore>,
    transfer: &TransferIdentity,
) -> anyhow::Result<UiTransferFixedHistoryRead> {
    if transfer.source.chain_root_id == transfer.destination.chain_root_id
        || transfer.source.thread_id == transfer.destination.thread_id
        || transfer.source.presentation_id == transfer.destination.presentation_id
    {
        bail!("transfer settlement requires two distinct retained seats");
    }
    // Capture BOTH cuts before either scanner consumes a page.
    let source_anchor = store
        .capture_thread_event_read_anchor(
            &transfer.source.chain_root_id,
            &transfer.source.thread_id,
        )?
        .ok_or_else(|| anyhow!("transfer settlement requires both retained signed heads"))?;
    let destination_anchor = store
        .capture_thread_event_read_anchor(
            &transfer.destination.chain_root_id,
            &transfer.destination.thread_id,
        )?
        .ok_or_else(|| anyhow!("transfer settlement requires both retained signed heads"))?;
    let source = begin_fixed_seat_read(
        store.clone(),
        source_anchor,
        &transfer.source,
        std::slice::from_ref(transfer),
    )?;
    let destination = begin_fixed_seat_read(
        store,
        destination_anchor,
        &transfer.destination,
        std::slice::from_ref(transfer),
    )?;
    Ok(UiTransferFixedHistoryRead {
        transfer: transfer.clone(),
        source,
        destination,
        poisoned: false,
    })
}

pub fn read_fixed_history_page(
    read: &mut UiTransferFixedHistoryRead,
    max_events: u64,
    max_bytes: u64,
) -> anyhow::Result<bool> {
    if read.poisoned {
        bail!("transfer settlement read previously failed");
    }
    // One call reads one seat only, preserving the caller's page work budget.
    let result = if !read.source.history.complete {
        read_fixed_seat_page(&mut read.source, max_events, max_bytes)
    } else {
        read_fixed_seat_page(&mut read.destination, max_events, max_bytes)
    };
    match result {
        Ok(_) => Ok(read.source.history.complete && read.destination.history.complete),
        Err(error) => {
            read.poisoned = true;
            Err(error)
        }
    }
}

pub fn finish_fixed_history_read(
    read: UiTransferFixedHistoryRead,
) -> anyhow::Result<UiTransferFixedHistoryProof> {
    require_complete_settlement_read(
        read.poisoned,
        read.source.history.complete,
        read.destination.history.complete,
    )?;
    let source = finish_fixed_seat_read(read.source)?;
    let destination = finish_fixed_seat_read(read.destination)?;
    assemble_fixed_history_proof(&source, &destination, &read.transfer)
}

impl UiTransferFixedHistoryProof {
    /// Read-only owner identity, not admission or publication authority.
    pub fn is_for_store(&self, store: &StateStore) -> bool {
        std::ptr::eq(self.store.as_ref(), store)
    }

    pub(crate) fn belongs_to(&self, store: &StateStore) -> bool {
        self.is_for_store(store)
    }

    pub fn source_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.source_anchor
    }

    pub fn destination_anchor(&self) -> &ryeos_state::AuthoritativeThreadEventReadAnchor {
        &self.destination_anchor
    }

    pub fn lifecycle(&self) -> &TransferLifecycleState {
        &self.lifecycle
    }
}

/// Read both seats of one transfer to genesis and assemble the proof. The page
/// sizes are mechanical read granularity, not a bound on any history.
pub fn read_complete_fixed_history(
    store: Arc<StateStore>,
    transfer: &TransferIdentity,
) -> anyhow::Result<UiTransferFixedHistoryProof> {
    let mut read = begin_fixed_history_read(store, transfer)?;
    while !read_fixed_history_page(&mut read, READ_PAGE_EVENTS, READ_PAGE_BYTES)? {}
    finish_fixed_history_read(read)
}

pub(super) fn select_settlement(
    state: &TransferLifecycleState,
    side: TransferSettlementSide,
) -> Result<TransferSettlementPlan, String> {
    use TransferSettlementPlan as Plan;
    use TransferSettlementRequirement as Requirement;

    let Some(prepared) = state.prepared() else {
        return match side {
            TransferSettlementSide::Source => Ok(Plan::AwaitingSourcePreparation),
            TransferSettlementSide::Destination => {
                if let Some(cancelled) = state.orphan_cancelled() {
                    Ok(Plan::AlreadyRecorded(cancelled.evidence.clone()))
                } else {
                    let record = TransferRecord {
                        schema: state.inbound().record.schema,
                        transfer: state.transfer().clone(),
                        phase: TransferPhase::OrphanCancelled {
                            inbound: state.inbound().evidence.clone(),
                        },
                    };
                    record.validate_shape()?;
                    Ok(Plan::Candidate {
                        record,
                        requirement: Requirement::SerializeOrphanCancellationAndSourcePreparation,
                    })
                }
            }
        };
    };

    let candidate = |phase, requirement| {
        let record = TransferRecord {
            schema: prepared.record.schema,
            transfer: state.transfer().clone(),
            phase,
        };
        record.validate_shape()?;
        Ok(Plan::Candidate {
            record,
            requirement,
        })
    };

    match side {
        TransferSettlementSide::Source => {
            if let Some(observed) = state.settlement_observed() {
                return Ok(Plan::AlreadyRecorded(observed.evidence.clone()));
            }
            if let Some(terminal) = state.activated().or(state.retained()).or(state.released()) {
                return candidate(
                    TransferPhase::SettlementObserved {
                        settlement: terminal.evidence.clone(),
                    },
                    Requirement::ExactCurrentPhaseAndOwningSeatFence,
                );
            }
            if state.committed().is_some() {
                return Ok(Plan::AwaitingDestinationSettlement);
            }
            if let Some(aborted) = state.aborted() {
                return if state.accepted().is_none() {
                    Ok(Plan::NoAcceptedReservation {
                        source_abort: Some(aborted.evidence.clone()),
                    })
                } else {
                    Ok(Plan::AwaitingDestinationSettlement)
                };
            }
            candidate(
                TransferPhase::Aborted {
                    preparation: prepared.evidence.clone(),
                },
                Requirement::SerializeSourceDecisionAndDestinationAcceptance,
            )
        }
        TransferSettlementSide::Destination => {
            if let Some(terminal) = state.activated().or(state.retained()).or(state.released()) {
                return Ok(Plan::AlreadyRecorded(terminal.evidence.clone()));
            }
            let Some(accepted) = state.accepted() else {
                return Ok(Plan::NoAcceptedReservation {
                    source_abort: state.aborted().map(|aborted| aborted.evidence.clone()),
                });
            };
            let TransferPhase::Accepted { import_digest, .. } = &accepted.record.phase else {
                return Err("transfer fold's acceptance has the wrong phase".into());
            };
            if let Some(committed) = state.committed() {
                return candidate(
                    TransferPhase::Retained {
                        commit: committed.evidence.clone(),
                        import_digest: import_digest.clone(),
                    },
                    Requirement::ExactCurrentPhaseAndOwningSeatFence,
                );
            }
            if let Some(aborted) = state.aborted() {
                return candidate(
                    TransferPhase::Released {
                        abort: aborted.evidence.clone(),
                        acceptance: accepted.evidence.clone(),
                        import_digest: import_digest.clone(),
                    },
                    Requirement::ExactCurrentPhaseAndOwningSeatFence,
                );
            }
            Ok(Plan::AwaitingSourceDecision)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer_store::discovery::signed_transfer_identity_for_seat;
    use ryeos_ui_presentation_contract::{
        payload_digest::seat_payload_digest,
        seat_control::{SEAT_PRODUCER_SCHEMA, UiSeatControlRequest, UiSeatProducerClaim},
        transfer::{TransferIdentity, TransferSeat},
        transfer_proof::TransferTestimony,
    };
    use serde_json::json;

    fn evidence(marker: &str, chain_seq: u64) -> TransferEvidence {
        TransferEvidence {
            event_hash: marker.repeat(32),
            chain_seq,
        }
    }

    fn prepared() -> TransferLifecycleState {
        let seat = |presentation_id: &str, side: &str| TransferSeat {
            presentation_id: presentation_id.into(),
            thread_id: format!("thread-{side}"),
            chain_root_id: format!("chain-{side}"),
            producer_incarnation: "ab".repeat(32),
            revision: 1,
        };
        let export = json!({"mode": "copy", "view": "retained"});
        let transfer = TransferIdentity {
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            source: seat("11111111-1111-4111-8111-111111111111", "source"),
            destination: seat("22222222-2222-4222-8222-222222222222", "destination"),
            export_digest: seat_payload_digest(&export).unwrap(),
        };
        let inbound = TransferRecord {
            schema: 1,
            transfer: transfer.clone(),
            phase: TransferPhase::InboundCommitment,
        };
        let mut state = TransferLifecycleState::begin(TransferTestimony {
            evidence: &evidence("ff", 1),
            record: &inbound,
        })
        .unwrap();
        let record = TransferRecord {
            schema: 1,
            transfer,
            phase: TransferPhase::Prepared {
                inbound: evidence("ff", 1),
                freeze: None,
                export,
            },
        };
        state
            .fold(TransferTestimony {
                evidence: &evidence("aa", 1),
                record: &record,
            })
            .unwrap();
        state
    }

    fn fold(state: &mut TransferLifecycleState, phase: TransferPhase, marker: &str, seq: u64) {
        let record = TransferRecord {
            schema: 1,
            transfer: state.transfer().clone(),
            phase,
        };
        state
            .fold(TransferTestimony {
                evidence: &evidence(marker, seq),
                record: &record,
            })
            .unwrap();
    }

    fn accepted() -> TransferLifecycleState {
        let mut state = prepared();
        let reservation = json!({"mount": 1});
        fold(
            &mut state,
            TransferPhase::Accepted {
                preparation: evidence("aa", 1),
                import_digest: seat_payload_digest(&reservation).unwrap(),
                reservation,
            },
            "bb",
            2,
        );
        state
    }

    #[test]
    fn destination_only_inbound_has_only_a_fenced_cancellation_candidate() {
        let prepared = prepared();
        let mut orphan = TransferLifecycleState::begin(prepared.inbound()).unwrap();
        assert_eq!(
            select_settlement(&orphan, TransferSettlementSide::Source).unwrap(),
            TransferSettlementPlan::AwaitingSourcePreparation
        );
        let TransferSettlementPlan::Candidate {
            record,
            requirement,
        } = select_settlement(&orphan, TransferSettlementSide::Destination).unwrap()
        else {
            panic!("destination orphan needs a fenced cancellation candidate")
        };
        assert_eq!(
            record.phase,
            TransferPhase::OrphanCancelled {
                inbound: evidence("ff", 1)
            }
        );
        assert_eq!(
            requirement,
            TransferSettlementRequirement::SerializeOrphanCancellationAndSourcePreparation
        );
        let mut wrong = prepared.prepared().unwrap().record.clone();
        let TransferPhase::Prepared { inbound, .. } = &mut wrong.phase else {
            unreachable!()
        };
        *inbound = evidence("ee", 1);
        assert!(
            orphan
                .fold(TransferTestimony {
                    evidence: &evidence("aa", 1),
                    record: &wrong,
                })
                .is_err()
        );
        assert!(matches!(
            select_settlement(&orphan, TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::Candidate { .. }
        ));

        orphan
            .fold(TransferTestimony {
                evidence: &evidence("dd", 2),
                record: &record,
            })
            .unwrap();
        assert_eq!(
            select_settlement(&orphan, TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::AlreadyRecorded(evidence("dd", 2))
        );
        assert!(orphan.fold(prepared.prepared().unwrap()).is_err());
        let (decision, retained) = completed_negative_terminal_decision(&orphan).unwrap();
        assert_eq!(decision, evidence("dd", 2));
        assert_eq!(retained, record);
    }

    #[test]
    fn linked_preparation_never_selects_orphan_cancellation() {
        let state = prepared();
        assert!(!matches!(
            select_settlement(&state, TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::Candidate {
                record: TransferRecord {
                    phase: TransferPhase::OrphanCancelled { .. },
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn orphan_cancellation_reader_requires_both_complete_signed_histories() {
        assert!(require_complete_settlement_read(false, true, true).is_ok());
        for (source, destination) in [(false, true), (true, false), (false, false)] {
            assert!(require_complete_settlement_read(false, source, destination).is_err());
        }
        assert!(require_complete_settlement_read(true, true, true).is_err());
    }

    #[test]
    fn prepared_without_its_signed_destination_inbound_cannot_begin_replay() {
        let state = prepared();
        assert!(TransferLifecycleState::begin(state.prepared().unwrap()).is_err());
        assert!(
            phase_order(&state.inbound().record.phase)
                < phase_order(&state.prepared().unwrap().record.phase)
        );
    }

    fn committed() -> TransferLifecycleState {
        let mut state = accepted();
        fold(
            &mut state,
            TransferPhase::Committed {
                preparation: evidence("aa", 1),
                acceptance: evidence("bb", 2),
                import_digest: seat_payload_digest(&json!({"mount": 1})).unwrap(),
            },
            "cc",
            3,
        );
        state
    }

    fn candidate(state: &TransferLifecycleState, side: TransferSettlementSide) -> TransferRecord {
        let TransferSettlementPlan::Candidate { record, .. } =
            select_settlement(state, side).unwrap()
        else {
            panic!("expected a provisional settlement candidate")
        };
        record
    }

    fn synthetic_history() -> (
        SeatHistory<()>,
        Vec<(String, ryeos_state::objects::thread_event::ThreadEvent)>,
    ) {
        use ryeos_state::objects::thread_event::NewEvent;
        let seat = prepared().transfer().source.clone();
        let hash = |marker: &str| marker.repeat(64);
        let make = |marker: &str, thread_seq, chain_seq, event_type: &str, payload| {
            (
                hash(marker),
                NewEvent::new(&seat.chain_root_id, &seat.thread_id, event_type)
                    .thread_seq(thread_seq)
                    .chain_seq(chain_seq)
                    .payload(payload)
                    .build(),
            )
        };
        let predecessor = UiSeatControlPredecessor::Initialization {
            event_hash: hash("a"),
        };
        let mut claim = UiSeatControlRequest::Claim {
            schema: 2,
            operation_id: String::new(),
            presentation_id: seat.presentation_id.clone(),
            thread_id: seat.thread_id.clone(),
            chain_root_id: seat.chain_root_id.clone(),
            predecessor,
            intent_nonce: "44444444-4444-4444-8444-444444444444".into(),
            claim: UiSeatProducerClaim::Fresh,
        };
        claim.assign_identity().unwrap();
        let incarnation = "b".repeat(64);
        let active = UiSeatProducer {
            schema_version: SEAT_PRODUCER_SCHEMA.into(),
            operation_id: claim.operation_id().into(),
            request: claim,
            claim: Some(UiSeatProducerClaim::Fresh),
            predecessor: None,
            producer_incarnation: incarnation.clone(),
            state: UiSeatProducerState::Active,
            next_engine_seq: 0,
        };
        let mut close = UiSeatControlRequest::Close {
            schema: 2,
            operation_id: String::new(),
            presentation_id: seat.presentation_id.clone(),
            thread_id: seat.thread_id.clone(),
            chain_root_id: seat.chain_root_id.clone(),
            predecessor: UiSeatControlPredecessor::Control {
                event_hash: hash("b"),
            },
            intent_nonce: "55555555-5555-4555-8555-555555555555".into(),
            producer_incarnation: incarnation.clone(),
        };
        close.assign_identity().unwrap();
        let closed = UiSeatProducer {
            schema_version: SEAT_PRODUCER_SCHEMA.into(),
            operation_id: close.operation_id().into(),
            request: close,
            claim: None,
            predecessor: Some(incarnation.clone()),
            producer_incarnation: incarnation.clone(),
            state: UiSeatProducerState::Closed,
            next_engine_seq: 1,
        };
        let semantic_payload = json!({"value": true});
        let semantic = [SeatAppendSemanticEvent {
            engine_seq: 0,
            event_type: "seat.facet",
            payload: &semantic_payload,
        }];
        let digest = seat_payload_digest(&serde_json::to_value(semantic).unwrap()).unwrap();
        let mut receipt = SeatAppendReceipt {
            schema_version: SEAT_APPEND_RECEIPT_SCHEMA.into(),
            producer_incarnation: incarnation.clone(),
            operation_id: String::new(),
            first_engine_seq: 0,
            last_engine_seq: 0,
            event_count: 1,
            payload_digest: digest.clone(),
        };
        receipt.operation_id = UiSeatAppendIdentity {
            presentation_id: seat.presentation_id.clone(),
            chain_root_id: seat.chain_root_id.clone(),
            thread_id: seat.thread_id.clone(),
            producer_incarnation: incarnation,
            first_engine_seq: 0,
            last_engine_seq: 0,
            event_count: 1,
            payload_digest: digest,
        }
        .derive_identity()
        .unwrap();
        let events = vec![
            make(
                "d",
                5,
                9,
                SEAT_PRODUCER_EVENT,
                serde_json::to_value(closed).unwrap(),
            ),
            make(
                "c",
                4,
                7,
                "ui_seat.append_receipt.v2",
                serde_json::to_value(receipt).unwrap(),
            ),
            make(
                "e",
                3,
                6,
                "seat.facet",
                json!({"seq":0,"payload":semantic_payload}),
            ),
            make(
                "b",
                2,
                5,
                SEAT_PRODUCER_EVENT,
                serde_json::to_value(active).unwrap(),
            ),
            make(
                "a",
                1,
                3,
                "ui_seat.presentation_initialized.v1",
                json!({
                    "operation_id":"66666666-6666-4666-8666-666666666666",
                    "initialization":{"schema":2,"selection":{"kind":"configured_defaults"},
                        "creation_request_id":"66666666-6666-4666-8666-666666666666",
                        "presentation_id":seat.presentation_id,
                        "root_binding":{"binding_attachment_id":"root","binding_generation":1,
                            "binding_digest":"a".repeat(64)},
                        "binding_attachments":[{"binding_attachment_id":"root",
                            "binding_generation":1,"binding_digest":"a".repeat(64),
                            "binding_request_bounds":{"max_request_bytes":4096,
                                "max_input_bytes":1024,"max_seat_append_bytes":65536}}]}
                }),
            ),
            make("f", 0, 1, "thread_created", json!({})),
        ];
        let history = SeatHistory {
            seat,
            anchor: (),
            progress: None,
            complete: false,
            previous_chain_seq: None,
            previous_thread_seq: None,
            latest_control: None,
            newer_control: None,
            frontier: None,
            interval_producer: None,
            pending_receipt: None,
            pending_receipt_bytes: 0,
            pending_receipt_evidence: None,
            validated_receipt: None,
            validated_receipt_events: Vec::new(),
            pending_cancellation_control: None,
            initialization_seen: false,
            original_append_bound: None,
            initialized_root_binding: None,
            initialized_payload: None,
            phases: Vec::new(),
        };
        (history, events)
    }

    #[test]
    fn signed_identity_locator_keeps_only_exact_phase_owner() {
        use ryeos_state::objects::thread_event::NewEvent;
        let state = prepared();
        let mut record = state.prepared().unwrap().record.clone();
        record.transfer.source.chain_root_id = record.transfer.source.thread_id.clone();
        let source = &record.transfer.source;
        let payload = ryeos_ui_presentation_contract::transfer::TransferEventPayload {
            seq: 0,
            operation_id: record.transfer.operation_id.clone(),
            payload: record.clone(),
        };
        let event = NewEvent::new(&source.chain_root_id, &source.thread_id, TRANSFER_EVENT)
            .thread_seq(1)
            .chain_seq(1)
            .payload(serde_json::to_value(payload).unwrap())
            .build();
        assert_eq!(
            signed_transfer_identity_for_seat(&source.thread_id, &event)
                .unwrap()
                .unwrap(),
            record.transfer
        );
        assert!(signed_transfer_identity_for_seat("another-seat", &event).is_err());
        let ordinary = NewEvent::new(&source.chain_root_id, &source.thread_id, "thread_created")
            .thread_seq(1)
            .chain_seq(1)
            .payload(json!({}))
            .build();
        assert!(
            signed_transfer_identity_for_seat(&source.thread_id, &ordinary)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn signed_identity_extractor_does_not_infer_absence_from_newer_event() {
        use ryeos_state::objects::thread_event::NewEvent;

        let mut record = prepared().prepared().unwrap().record.clone();
        record.transfer.source.chain_root_id = record.transfer.source.thread_id.clone();
        let seat = &record.transfer.source.thread_id;
        let ordinary = NewEvent::new(seat, seat, "seat.facet")
            .thread_seq(3)
            .chain_seq(3)
            .payload(json!({"value": true}))
            .build();
        let malformed = NewEvent::new(seat, seat, TRANSFER_EVENT)
            .thread_seq(2)
            .chain_seq(2)
            .payload(json!({"invalid": true}))
            .build();
        let transfer = NewEvent::new(seat, seat, TRANSFER_EVENT)
            .thread_seq(1)
            .chain_seq(1)
            .payload(
                serde_json::to_value(
                    ryeos_ui_presentation_contract::transfer::TransferEventPayload {
                        seq: 0,
                        operation_id: record.transfer.operation_id.clone(),
                        payload: record.clone(),
                    },
                )
                .unwrap(),
            )
            .build();

        // A newer event gives no negative proof. Malformed signed transfer
        // content is an error, not an empty locator result.
        assert!(
            signed_transfer_identity_for_seat(seat, &ordinary)
                .unwrap()
                .is_none()
        );
        assert!(signed_transfer_identity_for_seat(seat, &malformed).is_err());

        // An older positive is still discoverable; this locator never grants
        // settlement authority.
        let found = signed_transfer_identity_for_seat(seat, &transfer).unwrap();
        assert_eq!(found, Some(record.transfer));
    }

    #[test]
    fn signed_identity_locator_refuses_unknown_transfer_family_members() {
        use ryeos_state::objects::thread_event::NewEvent;
        let seat = "T-transfer-fixture";
        for kind in [
            format!("{TRANSFER_EVENT}.future"),
            format!("{TRANSFER_LIFECYCLE_EVENT}.future"),
            "contract.ryeos_ui.move_freeze_cancelled.v2".into(),
        ] {
            let event = NewEvent::new(seat, seat, &kind)
                .thread_seq(1)
                .chain_seq(1)
                .payload(json!({}))
                .build();
            let error = signed_transfer_identity_for_seat(seat, &event).unwrap_err();
            assert!(error.to_string().contains("unsupported testimony"));
        }
    }

    #[test]
    fn reverse_reader_cannot_skip_unknown_transfer_testimony() {
        use ryeos_state::objects::thread_event::NewEvent;
        let transfer = prepared().transfer().clone();
        for kind in [
            format!("{TRANSFER_EVENT}.future"),
            format!("{TRANSFER_LIFECYCLE_EVENT}.future"),
        ] {
            let (mut history, _) = synthetic_history();
            let event = NewEvent::new(&history.seat.chain_root_id, &history.seat.thread_id, &kind)
                .thread_seq(10)
                .chain_seq(10)
                .payload(json!({}))
                .build();
            let error = history.push(&transfer, "a".repeat(64), event).unwrap_err();
            assert!(error.to_string().contains("unsupported testimony"));
            assert!(history.phases.is_empty());
        }
    }

    #[test]
    fn cancellation_current_witness_is_closed_by_historical_receipts_and_control() {
        use ryeos_ui_presentation_contract::move_freeze_lifecycle::{
            MoveFreezeCancellation, MoveFreezeCancellationWriter,
        };
        let make = |incarnation: &str, next_engine_seq| {
            MoveFreezeCancellation::new(
                prepared().transfer().clone(),
                ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence {
                    preparation: evidence("01", 1),
                    completion: evidence("02", 2),
                }
                .into(),
                MoveFreezeCancellationWriter::CurrentProducer {
                    producer_incarnation: incarnation.repeat(64),
                    next_engine_seq,
                },
            )
            .unwrap()
        };
        let (mut history, events) = synthetic_history();
        history
            .push(
                prepared().transfer(),
                events[0].0.clone(),
                events[0].1.clone(),
            )
            .unwrap();
        assert!(
            history
                .authenticate_cancellation_witness(&make("b", 2))
                .is_err()
        );
        history
            .authenticate_cancellation_witness(&make("b", 1))
            .unwrap();
        for (hash, event) in events[1..].iter().cloned() {
            history.push(prepared().transfer(), hash, event).unwrap();
        }
        history.finish().unwrap();
        let (mut wrong, events) = synthetic_history();
        wrong
            .push(
                prepared().transfer(),
                events[0].0.clone(),
                events[0].1.clone(),
            )
            .unwrap();
        wrong
            .authenticate_cancellation_witness(&make("c", 1))
            .unwrap();
        assert!(
            events[1..]
                .iter()
                .cloned()
                .try_for_each(|(hash, event)| wrong.push(prepared().transfer(), hash, event))
                .is_err()
        );
    }

    #[test]
    fn selected_move_requires_receipted_freeze_not_a_hash_pair() {
        let state = prepared();
        let transfer = state.transfer().clone();
        let mut requested =
            freeze_requested_seat_operations(&transfer.source, std::slice::from_ref(&transfer))
                .unwrap();
        let operation = requested.get_mut(&transfer.operation_id).unwrap();
        let mut record = state.prepared().unwrap().record.clone();
        let TransferPhase::Prepared { freeze, .. } = &mut record.phase else {
            unreachable!()
        };
        *freeze = Some(
            ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence {
                preparation: evidence("01", 1),
                completion: evidence("02", 2),
            },
        );
        operation.phases.push(RetainedPhase {
            evidence: evidence("aa", 3),
            record,
            lifecycle: None,
        });
        assert!(
            verify_selected_freeze(operation)
                .unwrap_err()
                .to_string()
                .contains("receipted freeze witnesses")
        );
    }

    #[test]
    fn reverse_control_receipt_and_initialization_survive_page_split() {
        let (mut history, events) = synthetic_history();
        let transfer = prepared().transfer().clone();
        for (hash, event) in events[..2].iter().cloned() {
            history.push(&transfer, hash, event).unwrap();
        }
        assert!(history.pending_receipt.is_some());
        for (hash, event) in events[2..].iter().cloned() {
            history.push(&transfer, hash, event).unwrap();
        }
        history.finish().unwrap();
        assert_eq!(
            history.latest_control.as_ref().unwrap().1.state,
            UiSeatProducerState::Closed
        );
    }

    #[test]
    fn frozen_seat_requests_bind_full_identities_but_only_stable_context() {
        let transfer = prepared().transfer().clone();
        let mut second = transfer.clone();
        second.operation_id = "77777777-7777-4777-8777-777777777777".into();
        second.source.revision += 1;
        second.source.producer_incarnation = "c".repeat(64);
        let mut context = transfer.source.clone();
        context.revision += 100;
        context.producer_incarnation = "d".repeat(64);
        let mut requests = vec![transfer.clone(), second.clone(), transfer.clone()];
        let frozen = freeze_requested_seat_operations(&context, &requests).unwrap();
        assert_eq!(frozen.len(), 2);
        assert_eq!(frozen[&second.operation_id].owner, second.source);
        requests[0].export_digest = "e".repeat(64);
        assert!(covered_seat_operation(&context, &frozen, &transfer).is_ok());
        assert!(covered_seat_operation(&context, &frozen, &requests[0]).is_err());
        assert!(freeze_requested_seat_operations(&context, &requests).is_err());
        let mut wrong_context = context.clone();
        wrong_context.presentation_id = "88888888-8888-4888-8888-888888888888".into();
        assert!(
            freeze_requested_seat_operations(&wrong_context, std::slice::from_ref(&transfer))
                .is_err()
        );
        let empty = freeze_requested_seat_operations(&context, &[]).unwrap();
        assert!(covered_seat_operation(&context, &empty, &transfer).is_err());
        let mut malformed = transfer;
        malformed.destination.thread_id = malformed.source.thread_id.clone();
        assert!(freeze_requested_seat_operations(&context, &[malformed]).is_err());
    }

    fn synthetic_receipt_tip_join() -> (
        SeatHistory<()>,
        SeatHistory<()>,
        FixedSeatEndpoint,
        String,
        ryeos_state::objects::thread_event::ThreadEvent,
    ) {
        let (mut cached, events) = synthetic_history();
        // This complete cut ends at the receipt, AFTER its Active control's
        // original frontier. A newer Close must use the head frontier (1),
        // not the original control frontier (0), when authenticating the join.
        for (hash, event) in events[1..].iter().cloned() {
            assert!(cached.validate_seat_event(hash, event).unwrap().is_none());
        }
        cached.finish().unwrap();
        let (mut suffix, _) = synthetic_history();
        suffix
            .validate_seat_event(events[0].0.clone(), events[0].1.clone())
            .unwrap();
        let endpoint = FixedSeatEndpoint {
            tip: TransferEvidence {
                event_hash: events[1].0.clone(),
                chain_seq: events[1].1.chain_seq,
            },
            thread_seq: events[1].1.thread_seq,
            next_engine_seq: 1,
        };
        (
            cached,
            suffix,
            endpoint,
            events[1].0.clone(),
            events[1].1.clone(),
        )
    }

    #[test]
    fn suffix_join_authenticates_control_after_receipt_advanced_cached_frontier() {
        let (cached, mut suffix, endpoint, hash, event) = synthetic_receipt_tip_join();
        assert_eq!(cached.latest_control.as_ref().unwrap().1.next_engine_seq, 0);
        assert_eq!(endpoint.next_engine_seq, 1);
        assert!(!suffix.initialization_seen);
        finish_fixed_seat_join(&mut suffix, &cached, &endpoint, &hash, &event).unwrap();
        assert!(
            !suffix.initialization_seen,
            "a suffix must not impersonate genesis replay"
        );
        assert_eq!(
            suffix.latest_control.as_ref().unwrap().1.state,
            UiSeatProducerState::Closed
        );
        assert_eq!(suffix.original_append_bound, Some(65536));
        // The join receipt was not semantically folded a second time.
        assert!(suffix.pending_receipt.is_none());
        assert_eq!(suffix.frontier, Some(1));
        assert_eq!(suffix.previous_thread_seq, Some(5));
    }

    #[test]
    fn suffix_join_refuses_changed_endpoint_pending_receipt_frontier_and_producer() {
        for fault in [
            "hash",
            "thread",
            "chain",
            "adjacency",
            "pending",
            "frontier",
            "producer",
            "control",
        ] {
            let (mut cached, mut suffix, endpoint, mut hash, mut event) =
                synthetic_receipt_tip_join();
            match fault {
                "hash" => hash = "f".repeat(64),
                "thread" => event.thread_seq += 1,
                "chain" => event.chain_seq += 1,
                "adjacency" => suffix.previous_thread_seq = Some(6),
                "pending" => {
                    suffix.pending_receipt = Some((
                        serde_json::from_value(event.payload.clone()).unwrap(),
                        Vec::new(),
                    ))
                }
                "frontier" => suffix.frontier = Some(0),
                "producer" => suffix.interval_producer = Some("c".repeat(64)),
                _ => cached.latest_control.as_mut().unwrap().0.event_hash = "f".repeat(64),
            }
            assert!(
                finish_fixed_seat_join(&mut suffix, &cached, &endpoint, &hash, &event).is_err(),
                "{fault}"
            );
            assert!(
                suffix.original_append_bound.is_none(),
                "{fault} inherited ancestry before authentication"
            );
            assert!(!suffix.initialization_seen);
        }
    }

    #[test]
    fn shared_seat_validation_routes_each_requested_origin_without_rescanning() {
        use ryeos_state::objects::thread_event::NewEvent;
        let state = prepared();
        let first = state.transfer().clone();
        let mut second = first.clone();
        second.operation_id = "77777777-7777-4777-8777-777777777777".into();
        second.source.revision += 1;
        second.source.producer_incarnation = "c".repeat(64);
        let (mut history, _) = synthetic_history();
        let mut requested =
            freeze_requested_seat_operations(&history.seat, &[first.clone(), second.clone()])
                .unwrap();
        let first_record = state.prepared().unwrap().record.clone();
        let mut second_record = first_record.clone();
        second_record.transfer = second.clone();
        let mut unrequested = first_record.clone();
        unrequested.transfer.operation_id = "88888888-8888-4888-8888-888888888888".into();
        for (index, record) in [first_record.clone(), second_record, unrequested]
            .into_iter()
            .enumerate()
        {
            let seq = 13 - index as u64;
            let event = NewEvent::new(
                &history.seat.chain_root_id,
                &history.seat.thread_id,
                TRANSFER_EVENT,
            )
            .thread_seq(seq)
            .chain_seq(seq)
            .payload(json!({"seq":0,"operation_id":record.transfer.operation_id,"payload":record}))
            .build();
            let phase = history
                .validate_seat_event(format!("{seq:064x}"), event)
                .unwrap()
                .unwrap();
            route_requested_seat_phase(&mut requested, phase).unwrap();
        }
        assert_eq!(history.previous_thread_seq, Some(11));
        assert!(history.phases.is_empty());
        assert_eq!(requested.len(), 2);
        assert_eq!(requested[&first.operation_id].phases.len(), 1);
        assert_eq!(requested[&second.operation_id].phases.len(), 1);
        assert_eq!(
            requested[&second.operation_id].phases[0].record.transfer,
            second
        );
        let mut changed_origin = first_record;
        changed_origin.transfer.source.revision += 1;
        let phase = RetainedPhase {
            evidence: evidence("ee", 10),
            record: changed_origin,
            lifecycle: None,
        };
        assert!(route_requested_seat_phase(&mut requested, phase).is_err());
        assert_eq!(
            history.previous_thread_seq,
            Some(11),
            "routing never consumes ancestry"
        );
        // Partial synthetic ancestry is not a completed fixed-seat proof.
        assert!(history.finish().is_err());
    }

    #[test]
    fn common_seat_validation_refuses_foreign_presentation_before_operation_selection() {
        use ryeos_state::objects::thread_event::NewEvent;
        let (mut history, _) = synthetic_history();
        let mut record = prepared().prepared().unwrap().record.clone();
        record.transfer.operation_id = "88888888-8888-4888-8888-888888888888".into();
        record.transfer.source.presentation_id = "77777777-7777-4777-8777-777777777777".into();
        let event = NewEvent::new(
            &history.seat.chain_root_id,
            &history.seat.thread_id,
            TRANSFER_EVENT,
        )
        .thread_seq(10)
        .chain_seq(10)
        .payload(json!({"seq":0,"operation_id":record.transfer.operation_id,"payload":record}))
        .build();
        let error = history
            .validate_seat_event("c".repeat(64), event)
            .err()
            .expect("foreign presentation must refuse common validation");
        assert!(error.to_string().contains("another stable seat"));
        assert!(history.phases.is_empty());
    }

    #[test]
    fn common_validation_cannot_skip_unrequested_foreign_thread_or_root() {
        use ryeos_state::objects::thread_event::NewEvent;
        for change_root in [false, true] {
            let (mut history, _) = synthetic_history();
            let mut record = prepared().prepared().unwrap().record.clone();
            record.transfer.operation_id = "88888888-8888-4888-8888-888888888888".into();
            if change_root {
                record.transfer.source.chain_root_id = "T-other-root".into();
            } else {
                record.transfer.source.thread_id = "T-other-thread".into();
            }
            let event = NewEvent::new(
                &history.seat.chain_root_id,
                &history.seat.thread_id,
                TRANSFER_EVENT,
            )
            .thread_seq(10)
            .chain_seq(10)
            .payload(json!({"seq":0,"operation_id":record.transfer.operation_id,"payload":record}))
            .build();
            assert!(history.validate_seat_event("c".repeat(64), event).is_err());
            assert!(history.phases.is_empty());
        }
    }

    #[test]
    fn independent_seat_validation_reconstructs_ancestry_without_phase_routing() {
        let (mut history, events) = synthetic_history();
        for (index, (hash, event)) in events.into_iter().enumerate() {
            assert!(history.validate_seat_event(hash, event).unwrap().is_none());
            assert!(history.phases.is_empty());
            if index == 1 {
                assert!(history.pending_receipt.is_some());
            }
            if index == 2 {
                assert!(history.pending_receipt.is_none());
                assert_eq!(history.pending_receipt_bytes, 0);
            }
        }
        history.finish().unwrap();
        assert_eq!(history.frontier, Some(0));
        assert_eq!(history.original_append_bound, Some(65536));
        assert_eq!(
            history.latest_control.as_ref().unwrap().1.state,
            UiSeatProducerState::Closed
        );
    }

    #[test]
    fn decoded_phases_are_independent_of_selected_operation_and_bounded_routing() {
        use ryeos_state::objects::thread_event::NewEvent;
        let state = prepared();
        let transfer = state.transfer().clone();
        let (mut history, _) = synthetic_history();
        let selected = state.prepared().unwrap().record.clone();
        let mut unrelated = selected.clone();
        unrelated.transfer.operation_id = "77777777-7777-4777-8777-777777777777".into();
        // Same stable seat, different operation origin. The common validator
        // must not compare this origin to the selected transfer's TransferSeat.
        unrelated.transfer.source.revision += 1;
        unrelated.transfer.source.producer_incarnation = "c".repeat(64);
        let event = |record: &TransferRecord, seq| {
            NewEvent::new(
                &history.seat.chain_root_id,
                &history.seat.thread_id,
                TRANSFER_EVENT,
            )
            .thread_seq(seq)
            .chain_seq(seq)
            .payload(json!({"seq":0,"operation_id":record.transfer.operation_id,"payload":record}))
            .build()
        };
        let unrelated_event = event(&unrelated, 11);
        let selected_event = event(&selected, 10);
        let unrelated_phase = history
            .validate_seat_event("c".repeat(64), unrelated_event)
            .unwrap()
            .unwrap();
        let selected_phase = history
            .validate_seat_event("d".repeat(64), selected_event)
            .unwrap()
            .unwrap();
        assert_eq!(unrelated_phase.record, unrelated);
        assert_eq!(selected_phase.record, selected);
        assert!(history.phases.is_empty());
        let reconstructed_order = (history.previous_chain_seq, history.previous_thread_seq);
        history
            .route_operation_phase(&transfer, unrelated_phase)
            .unwrap();
        assert!(history.phases.is_empty());
        history
            .route_operation_phase(&transfer, selected_phase)
            .unwrap();
        assert_eq!(history.phases.len(), 1);
        assert_eq!(history.phases[0].evidence.event_hash, "d".repeat(64));
        assert_eq!(
            (history.previous_chain_seq, history.previous_thread_seq),
            reconstructed_order
        );
        // Routing alone keeps the existing duplicate-phase refusal. Neither
        // these provisional phases nor partial ancestry imply settlement.
        let duplicate = RetainedPhase {
            evidence: evidence("ee", 9),
            record: selected,
            lifecycle: None,
        };
        let error = history
            .route_operation_phase(&transfer, duplicate)
            .unwrap_err();
        assert!(error.to_string().contains("repeats a settled phase"));
    }

    #[test]
    fn reverse_reader_rejects_broken_receipt_adjacency_and_frontier() {
        let transfer = prepared().transfer().clone();
        let (mut history, mut events) = synthetic_history();
        events[2].1.chain_seq = 5;
        for (hash, event) in events[..2].iter().cloned() {
            history.push(&transfer, hash, event).unwrap();
        }
        assert!(
            history
                .push(&transfer, events[2].0.clone(), events[2].1.clone())
                .is_err()
        );
        let (mut history, mut events) = synthetic_history();
        events[0].1.payload["next_engine_seq"] = json!(2);
        assert!(
            history
                .push(&transfer, events[0].0.clone(), events[0].1.clone())
                .is_ok()
        );
        history
            .push(&transfer, events[1].0.clone(), events[1].1.clone())
            .unwrap();
        assert!(
            history
                .push(&transfer, events[2].0.clone(), events[2].1.clone())
                .is_err()
        );
    }

    #[test]
    fn reverse_reader_rejects_wrong_initialization_subject() {
        let (mut history, mut events) = synthetic_history();
        let transfer = prepared().transfer().clone();
        events[4].1.payload["initialization"]["presentation_id"] =
            json!("77777777-7777-4777-8777-777777777777");
        for (hash, event) in events[..4].iter().cloned() {
            history.push(&transfer, hash, event).unwrap();
        }
        assert!(
            history
                .push(&transfer, events[4].0.clone(), events[4].1.clone())
                .is_err()
        );
    }

    #[test]
    fn precommit_abort_explicitly_requires_late_acceptance_exclusion() {
        for state in [prepared(), accepted()] {
            let TransferSettlementPlan::Candidate {
                record,
                requirement,
            } = select_settlement(&state, TransferSettlementSide::Source).unwrap()
            else {
                panic!("expected provisional abort")
            };
            assert!(matches!(record.phase, TransferPhase::Aborted { .. }));
            assert_eq!(
                requirement,
                TransferSettlementRequirement::SerializeSourceDecisionAndDestinationAcceptance
            );
        }
        assert_eq!(
            select_settlement(&accepted(), TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::AwaitingSourceDecision
        );
    }

    #[test]
    fn abort_without_acceptance_never_manufactures_release_or_observation() {
        let mut state = prepared();
        fold(
            &mut state,
            TransferPhase::Aborted {
                preparation: evidence("aa", 1),
            },
            "cc",
            3,
        );
        let (decision, record) = completed_negative_terminal_decision(&state).unwrap();
        assert_eq!(decision, evidence("cc", 3));
        assert!(matches!(record.phase, TransferPhase::Aborted { .. }));
        assert!(completed_negative_terminal_decision(&accepted()).is_none());
        for side in [
            TransferSettlementSide::Source,
            TransferSettlementSide::Destination,
        ] {
            assert_eq!(
                select_settlement(&state, side).unwrap(),
                TransferSettlementPlan::NoAcceptedReservation {
                    source_abort: Some(evidence("cc", 3)),
                }
            );
        }
    }

    #[test]
    fn committed_data_is_retained_then_observed_without_live_activation() {
        let mut state = committed();
        assert_eq!(
            select_settlement(&state, TransferSettlementSide::Source).unwrap(),
            TransferSettlementPlan::AwaitingDestinationSettlement
        );
        let retained = candidate(&state, TransferSettlementSide::Destination);
        assert!(matches!(retained.phase, TransferPhase::Retained { .. }));
        fold(&mut state, retained.phase, "dd", 4);
        let observed = candidate(&state, TransferSettlementSide::Source);
        assert_eq!(
            observed.phase,
            TransferPhase::SettlementObserved {
                settlement: evidence("dd", 4),
            }
        );
        fold(&mut state, observed.phase, "ee", 5);
        assert_eq!(
            select_settlement(&state, TransferSettlementSide::Source).unwrap(),
            TransferSettlementPlan::AlreadyRecorded(evidence("ee", 5))
        );
        assert_eq!(
            select_settlement(&state, TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::AlreadyRecorded(evidence("dd", 4))
        );
    }

    #[test]
    fn accepted_abort_releases_exact_reservation_before_source_observation() {
        let mut state = accepted();
        fold(
            &mut state,
            TransferPhase::Aborted {
                preparation: evidence("aa", 1),
            },
            "cc",
            3,
        );
        assert_eq!(
            select_settlement(&state, TransferSettlementSide::Source).unwrap(),
            TransferSettlementPlan::AwaitingDestinationSettlement
        );
        let released = candidate(&state, TransferSettlementSide::Destination);
        assert_eq!(
            released.phase,
            TransferPhase::Released {
                abort: evidence("cc", 3),
                acceptance: evidence("bb", 2),
                import_digest: seat_payload_digest(&json!({"mount": 1})).unwrap(),
            }
        );
        fold(&mut state, released.phase, "dd", 4);
        assert_eq!(
            candidate(&state, TransferSettlementSide::Source).phase,
            TransferPhase::SettlementObserved {
                settlement: evidence("dd", 4),
            }
        );
    }

    #[test]
    fn historical_activation_is_preserved_after_session_loss() {
        let mut state = committed();
        fold(
            &mut state,
            TransferPhase::Activated {
                commit: evidence("cc", 3),
                import_digest: seat_payload_digest(&json!({"mount": 1})).unwrap(),
            },
            "dd",
            4,
        );
        assert_eq!(
            select_settlement(&state, TransferSettlementSide::Destination).unwrap(),
            TransferSettlementPlan::AlreadyRecorded(evidence("dd", 4))
        );
        assert_eq!(
            candidate(&state, TransferSettlementSide::Source).phase,
            TransferPhase::SettlementObserved {
                settlement: evidence("dd", 4),
            }
        );
    }

    #[test]
    fn pending_receipt_budget_accepts_the_full_native_protocol_interval() {
        let event = ryeos_state::objects::thread_event::MAX_THREAD_EVENT_SERIALIZED_BYTES as u64;
        let mut bytes = 0;
        for _ in 0..128 {
            bytes = charge_pending_receipt_bytes(bytes, event).unwrap();
        }
        assert_eq!(bytes, MAX_PENDING_RECEIPT_BYTES);
        assert!(charge_pending_receipt_bytes(bytes, 1).is_err());
        assert!(charge_pending_receipt_bytes(u64::MAX, 1).is_err());
    }

    #[test]
    fn incomplete_or_poisoned_histories_cannot_prepare_publication() {
        assert!(require_complete_settlement_read(false, true, true).is_ok());
        for (poisoned, source, destination) in [
            (false, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            assert!(require_complete_settlement_read(poisoned, source, destination).is_err());
        }
    }
}
