//! Typed transfer appends over the generic guarded-append primitives.
//!
//! Every writer here builds its event from a completed proof, checks the
//! deterministic shape and size of what it will publish before anything is
//! locked, then publishes through the store's mixed-cut append: the exact
//! frontier of the target, of its counterpart and of every further live seat
//! the read depended on, plus the fences of every retired seat, must be
//! unchanged under the publication locks. A proof is read testimony, never
//! permission: each writer rechecks the signed identity of both seats in its
//! admission, and the caller's own admission runs last, under the same locks.

use std::cell::Cell;

use anyhow::{Context, anyhow, bail};
use ryeos_app::state_store::{
    CutAppendResult, CutPairAppendResult, ExactSeat, MixedCutAdmission, MixedCutAppend,
    NewEventRecord, PersistedEventRecord, SeatCut, SeatPairAppend, StateStore,
};
use ryeos_state::objects::ThreadStatus;
use ryeos_state::objects::thread_event::{MAX_THREAD_EVENT_SERIALIZED_BYTES, ThreadEvent};
use ryeos_state::{AuthoritativeThreadEventReadAnchor, RetiredChainDependency};
use ryeos_ui_presentation_contract::{
    move_freeze_lifecycle::{
        MOVE_FREEZE_CANCELLED_EVENT, MoveFreezeCancellation, MoveFreezeCancellationWriter,
    },
    transfer::{
        TRANSFER_EVENT, TRANSFER_SCHEMA, TransferEventPayload, TransferEvidence, TransferIdentity,
        TransferPhase, TransferRecord, TransferSeat, from_event_value,
    },
    transfer_lifecycle::{
        TRANSFER_LIFECYCLE_EVENT, TransferLifecycleEventPayload, TransferLifecycleFence,
        verify_control_fence,
    },
    transfer_state::{TransferDestinationState, TransferSourceState},
};

use super::reader::{
    SettlementCounterpart, TransferSettlementCandidate, UiMoveFreezeCancellationProof,
    UiTransferFixedHistoryProof, UiTransferFixedSeatProof,
};

/// Authenticated facts about one seat, captured against one exact signed
/// anchor by the seat-transition owner. This is not a grant: the publication
/// still requires the running signed seat with this identity and the
/// unchanged anchor under the publication locks.
pub struct TransferSeatContext<'a> {
    pub anchor: &'a AuthoritativeThreadEventReadAnchor,
    pub owner: &'a str,
    pub surface: &'a str,
    pub client_ref: &'a str,
    pub presentation_id: &'a str,
    pub producer_incarnation: &'a str,
    pub revision: u64,
    pub next_engine_seq: u64,
    pub max_append_bytes: u64,
}

/// The two seats of one publication: the seat that gains the event and the
/// counterpart whose frontier must not move meanwhile.
#[derive(Clone, Copy)]
pub struct TransferSeatPair<'a> {
    pub target: &'a TransferSeatContext<'a>,
    pub counterpart: &'a TransferSeatContext<'a>,
}

/// The inputs of a destination acceptance, all from one completed read.
#[derive(Clone, Copy)]
pub struct AcceptanceInputs<'a> {
    pub transfer: &'a TransferIdentity,
    pub proof: &'a UiTransferFixedHistoryProof,
    pub preparation: &'a TransferEvidence,
    pub inbound_event: &'a ThreadEvent,
    pub prepared_event: &'a ThreadEvent,
    pub reservation: &'a serde_json::Value,
}

/// Every other chain the caller's read depended on, beyond the two seats of
/// the publication itself.
#[derive(Clone, Copy)]
pub struct TransferCutDependencies<'a> {
    pub current_anchors: &'a [AuthoritativeThreadEventReadAnchor],
    pub retired: &'a [RetiredChainDependency],
}

/// Outcome of a typed transfer publication. `Conflict` and `NotRunning` wrote
/// nothing. `Uncertain` means the caller's admission had already run when the
/// append failed: the batch may or may not be signed, so the caller must
/// reconcile from signed history and never treat absence or a fresh
/// allocation as permission to retry.
#[derive(Debug)]
pub enum TransferAppendResult {
    Committed(Vec<PersistedEventRecord>),
    Conflict,
    NotRunning,
    Uncertain { error: anyhow::Error },
}

/// The two source decisions admitted from a completed paired history. This
/// is deliberately not an arbitrary transfer-phase append capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferSourceDecision {
    Commit,
    Abort,
}

#[derive(Clone, Copy)]
enum SelectedPhase {
    Source(TransferSourceDecision),
    Activate,
}

/// Whether a seat event of this shape fits `bound` once the state layer has
/// assigned it positions, hashes and a timestamp, and the native event ceiling.
/// Unassigned fields take their longest rendering, so a fitting event cannot
/// grow past the bound on publication.
fn event_fits_bound(
    event: &NewEventRecord,
    chain_root_id: &str,
    thread_id: &str,
    bound: u64,
) -> anyhow::Result<bool> {
    let sized = ThreadEvent {
        schema: ryeos_state::objects::SCHEMA_VERSION,
        kind: "thread_event".to_string(),
        chain_root_id: chain_root_id.to_string(),
        chain_seq: u64::MAX,
        thread_id: thread_id.to_string(),
        thread_seq: u64::MAX,
        event_type: event.event_type.clone(),
        durability: ryeos_state::objects::EventDurability::Durable,
        // A sizing sentinel longer than any rendered timestamp, not a date.
        ts: "x".repeat(32),
        prev_chain_event_hash: Some("f".repeat(64)),
        prev_thread_event_hash: Some("f".repeat(64)),
        payload: event.payload.clone(),
    };
    let bytes = lillux::canonical_json(&sized.to_value())?.len();
    Ok(u64::try_from(bytes).is_ok_and(|bytes| bytes <= bound)
        && bytes <= MAX_THREAD_EVENT_SERIALIZED_BYTES)
}

fn indexed_event(event_type: &str, payload: serde_json::Value) -> NewEventRecord {
    NewEventRecord {
        event_type: event_type.to_string(),
        storage_class: "indexed".to_string(),
        payload,
    }
}

fn require_exact_seat_roots(
    anchor: &AuthoritativeThreadEventReadAnchor,
    seat: &TransferSeat,
) -> anyhow::Result<()> {
    if anchor.chain_root_id() != seat.chain_root_id
        || anchor.thread_id() != seat.thread_id
        || seat.chain_root_id != seat.thread_id
    {
        bail!("transfer publication requires an exact signed seat root");
    }
    Ok(())
}

/// The signed snapshot of `seat` is the running seat `context` describes.
fn snapshot_is_seat(
    snapshot: &ryeos_state::objects::ThreadSnapshot,
    seat: &TransferSeat,
    owner: &str,
    surface: &str,
    client_ref: &str,
    seat_kind: &str,
) -> bool {
    snapshot.thread_id == seat.thread_id
        && snapshot.chain_root_id == seat.chain_root_id
        && snapshot.kind_name == seat_kind
        && snapshot.status == ThreadStatus::Running
        && snapshot.requested_by.as_deref() == Some(owner)
        && snapshot.item_ref == surface
        && snapshot.executor_ref == client_ref
}

struct Publication<'a> {
    seat_kind: &'a str,
    target: &'a TransferSeatContext<'a>,
    target_seat: &'a TransferSeat,
    counterpart: &'a TransferSeatContext<'a>,
    counterpart_seat: &'a TransferSeat,
    events: Vec<NewEventRecord>,
    dependencies: TransferCutDependencies<'a>,
}

/// Publish one event on `target` while `counterpart` and the rest of the cut
/// stay exactly as read. Both seats must be the running seats their contexts
/// describe; `admit` is the caller's own admission and runs last.
fn publish_between_seats(
    store: &StateStore,
    publication: Publication<'_>,
    admit: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<TransferAppendResult> {
    let Publication {
        seat_kind,
        target,
        target_seat,
        counterpart,
        counterpart_seat,
        events,
        dependencies,
    } = publication;
    let mut anchors = vec![target.anchor.clone(), counterpart.anchor.clone()];
    anchors.extend(dependencies.current_anchors.iter().cloned());
    let admission_entered = Cell::new(false);
    let request = MixedCutAppend {
        chain_root_id: &target_seat.chain_root_id,
        thread_id: &target_seat.thread_id,
        events: &events,
        expected: target.anchor,
        counterpart: Some(counterpart.anchor),
        required_target_kind: seat_kind,
        current_anchors: &anchors,
        retired: dependencies.retired,
    };
    let outcome = store.append_events_if_mixed_cut(&request, |view: MixedCutAdmission<'_>| {
        let Some(other) = view.counterpart else {
            return Ok(false);
        };
        if !snapshot_is_seat(
            view.target,
            target_seat,
            target.owner,
            target.surface,
            target.client_ref,
            seat_kind,
        ) || !snapshot_is_seat(
            other,
            counterpart_seat,
            counterpart.owner,
            counterpart.surface,
            counterpart.client_ref,
            seat_kind,
        ) {
            return Ok(false);
        }
        admission_entered.set(true);
        admit()
    });
    Ok(match outcome {
        Ok(CutAppendResult::Committed(records)) => TransferAppendResult::Committed(records),
        Ok(CutAppendResult::Conflict) => TransferAppendResult::Conflict,
        Ok(CutAppendResult::NotRunning) => TransferAppendResult::NotRunning,
        Err(error) if admission_entered.get() => TransferAppendResult::Uncertain { error },
        Err(error) => return Err(error),
    })
}

fn wrapped_transfer_event(
    record: TransferRecord,
    context: &TransferSeatContext<'_>,
    seat: &TransferSeat,
) -> anyhow::Result<NewEventRecord> {
    record
        .validate_bounds(context.max_append_bytes)
        .map_err(anyhow::Error::msg)?;
    let wrapper = TransferEventPayload {
        seq: context.next_engine_seq,
        operation_id: record.transfer.operation_id.clone(),
        payload: record,
    };
    wrapper
        .validate_current_witness(
            context.next_engine_seq,
            context.revision,
            context.presentation_id,
            context.producer_incarnation,
        )
        .map_err(anyhow::Error::msg)?;
    let event = indexed_event(TRANSFER_EVENT, serde_json::to_value(wrapper)?);
    if !event_fits_bound(
        &event,
        &seat.chain_root_id,
        &seat.thread_id,
        context.max_append_bytes,
    )? {
        bail!("transfer phase exceeds the admitted seat append bound");
    }
    Ok(event)
}

/// Publish one destination Accepted event after a complete two-seat phase
/// proof at exactly these anchors. Unrelated intervening events are
/// permitted before the proof; after it, any change to either seat conflicts.
/// The seat owner independently rechecks the affected source inventory,
/// destination placement, current producer and hidden reservation in
/// `admit_reservation`, which runs under the publication locks.
pub fn append_transfer_accepted(
    store: &StateStore,
    seat_kind: &str,
    inputs: &AcceptanceInputs<'_>,
    seats: TransferSeatPair<'_>,
    dependencies: TransferCutDependencies<'_>,
    admit_reservation: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<TransferAppendResult> {
    let AcceptanceInputs {
        transfer,
        proof,
        preparation,
        inbound_event,
        prepared_event,
        reservation,
    } = *inputs;
    let (destination, source) = (seats.target, seats.counterpart);
    if !proof.belongs_to(store)
        || proof.source_anchor() != source.anchor
        || proof.destination_anchor() != destination.anchor
        || proof.lifecycle().transfer() != transfer
        || proof.lifecycle().source_state() != TransferSourceState::Frozen
        || proof.lifecycle().destination_state() != TransferDestinationState::Unreserved
        || proof
            .lifecycle()
            .prepared()
            .is_none_or(|retained| retained.evidence != preparation)
    {
        bail!("acceptance requires complete current two-seat phase proof");
    }
    let inbound = from_event_value(&inbound_event.to_value()).map_err(anyhow::Error::msg)?;
    let prepared = from_event_value(&prepared_event.to_value()).map_err(anyhow::Error::msg)?;
    let TransferPhase::Prepared {
        inbound: inbound_link,
        ..
    } = &prepared.payload.phase
    else {
        bail!("acceptance requires exact source Prepared testimony");
    };
    require_exact_seat_roots(destination.anchor, &transfer.destination)?;
    require_exact_seat_roots(source.anchor, &transfer.source)?;
    if destination.presentation_id != transfer.destination.presentation_id
        || source.presentation_id != transfer.source.presentation_id
        || source.revision < transfer.source.revision
        || inbound.payload.transfer != *transfer
        || inbound.operation_id != transfer.operation_id
        || !matches!(inbound.payload.phase, TransferPhase::InboundCommitment)
        || inbound_link.chain_seq != inbound_event.chain_seq
        || ryeos_state::objects::thread_event::hash_event(inbound_event)? != inbound_link.event_hash
        || *proof.lifecycle().inbound().evidence != *inbound_link
        || prepared_event.chain_seq != preparation.chain_seq
        || ryeos_state::objects::thread_event::hash_event(prepared_event)? != preparation.event_hash
        || prepared_event.chain_root_id != transfer.source.chain_root_id
        || prepared_event.thread_id != transfer.source.thread_id
        || prepared.payload.transfer != *transfer
        || prepared.operation_id != transfer.operation_id
    {
        bail!("acceptance differs from authenticated retained phase evidence");
    }
    prepared
        .payload
        .validate_shape()
        .map_err(anyhow::Error::msg)?;
    let import_digest =
        ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(reservation)
            .map_err(anyhow::Error::msg)?;
    let accepted = TransferRecord {
        schema: TRANSFER_SCHEMA,
        transfer: transfer.clone(),
        phase: TransferPhase::Accepted {
            preparation: preparation.clone(),
            reservation: reservation.clone(),
            import_digest,
        },
    };
    let event = wrapped_transfer_event(accepted, destination, &transfer.destination)?;
    publish_between_seats(
        store,
        Publication {
            seat_kind,
            target: destination,
            target_seat: &transfer.destination,
            counterpart: source,
            counterpart_seat: &transfer.source,
            events: vec![event],
            dependencies,
        },
        admit_reservation,
    )
}

/// Publish only the source decision justified by this completed paired
/// history. `seats.target` is the source. The seat owner checks the affected
/// inventory and frozen state in `admit_phase`, under the publication locks.
pub fn append_transfer_source_decision(
    store: &StateStore,
    seat_kind: &str,
    proof: &UiTransferFixedHistoryProof,
    decision: TransferSourceDecision,
    seats: TransferSeatPair<'_>,
    dependencies: TransferCutDependencies<'_>,
    admit_phase: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<TransferAppendResult> {
    append_selected_phase(
        store,
        seat_kind,
        proof,
        SelectedPhase::Source(decision),
        seats,
        dependencies,
        admit_phase,
    )
}

/// Publish the destination's Activated event after the source committed.
/// `seats.target` is the destination.
pub fn append_transfer_activated(
    store: &StateStore,
    seat_kind: &str,
    proof: &UiTransferFixedHistoryProof,
    seats: TransferSeatPair<'_>,
    dependencies: TransferCutDependencies<'_>,
    admit_phase: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<TransferAppendResult> {
    append_selected_phase(
        store,
        seat_kind,
        proof,
        SelectedPhase::Activate,
        seats,
        dependencies,
        admit_phase,
    )
}

fn append_selected_phase(
    store: &StateStore,
    seat_kind: &str,
    proof: &UiTransferFixedHistoryProof,
    selected: SelectedPhase,
    seats: TransferSeatPair<'_>,
    dependencies: TransferCutDependencies<'_>,
    admit_phase: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<TransferAppendResult> {
    let (target, counterpart) = (seats.target, seats.counterpart);
    let lifecycle = proof.lifecycle();
    let transfer = lifecycle.transfer();
    let (expected_target, expected_counterpart, phase) = match selected {
        SelectedPhase::Source(TransferSourceDecision::Commit) => {
            if lifecycle.source_state() != TransferSourceState::Frozen
                || lifecycle.destination_state() != TransferDestinationState::Reserved
            {
                bail!("source commit requires retained accepted reservation");
            }
            let preparation = lifecycle
                .prepared()
                .ok_or_else(|| anyhow!("missing preparation"))?;
            let acceptance = lifecycle
                .accepted()
                .ok_or_else(|| anyhow!("missing acceptance"))?;
            let TransferPhase::Accepted { import_digest, .. } = &acceptance.record.phase else {
                bail!("retained acceptance has wrong phase");
            };
            (
                &transfer.source,
                &transfer.destination,
                TransferPhase::Committed {
                    preparation: (*preparation.evidence).clone(),
                    acceptance: (*acceptance.evidence).clone(),
                    import_digest: import_digest.clone(),
                },
            )
        }
        SelectedPhase::Source(TransferSourceDecision::Abort) => {
            if lifecycle.source_state() != TransferSourceState::Frozen
                || !matches!(
                    lifecycle.destination_state(),
                    TransferDestinationState::Unreserved | TransferDestinationState::Reserved
                )
            {
                bail!("source abort requires an undecided retained preparation");
            }
            let preparation = lifecycle
                .prepared()
                .ok_or_else(|| anyhow!("missing preparation"))?;
            (
                &transfer.source,
                &transfer.destination,
                TransferPhase::Aborted {
                    preparation: (*preparation.evidence).clone(),
                },
            )
        }
        SelectedPhase::Activate => {
            if lifecycle.source_state() != TransferSourceState::Committed
                || lifecycle.destination_state() != TransferDestinationState::Reserved
            {
                bail!("activation requires retained commit and reservation");
            }
            let commit = lifecycle
                .committed()
                .ok_or_else(|| anyhow!("missing commit"))?;
            let TransferPhase::Committed { import_digest, .. } = &commit.record.phase else {
                bail!("retained commit has wrong phase");
            };
            (
                &transfer.destination,
                &transfer.source,
                TransferPhase::Activated {
                    commit: (*commit.evidence).clone(),
                    import_digest: import_digest.clone(),
                },
            )
        }
    };
    let (target_anchor, counterpart_anchor) = match selected {
        SelectedPhase::Source(_) => (proof.source_anchor(), proof.destination_anchor()),
        SelectedPhase::Activate => (proof.destination_anchor(), proof.source_anchor()),
    };
    if !proof.belongs_to(store)
        || target.anchor != target_anchor
        || counterpart.anchor != counterpart_anchor
        || target.presentation_id != expected_target.presentation_id
        || counterpart.presentation_id != expected_counterpart.presentation_id
        || target.revision < expected_target.revision
        || counterpart.revision < expected_counterpart.revision
    {
        bail!("selected phase differs from completed paired seat proof");
    }
    require_exact_seat_roots(target.anchor, expected_target)?;
    require_exact_seat_roots(counterpart.anchor, expected_counterpart)?;
    let record = TransferRecord {
        schema: TRANSFER_SCHEMA,
        transfer: transfer.clone(),
        phase,
    };
    let event = wrapped_transfer_event(record, target, expected_target)?;
    publish_between_seats(
        store,
        Publication {
            seat_kind,
            target,
            target_seat: expected_target,
            counterpart,
            counterpart_seat: expected_counterpart,
            events: vec![event],
            dependencies,
        },
        admit_phase,
    )
}

/// Observation of one Accepted successor after an attempt. `Unchanged` is not
/// an absence proof: the capture is not held through a retry decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferAcceptedReadback {
    Observed { acceptance: TransferEvidence },
    Unchanged,
    Ambiguous,
}

/// Fixed-predecessor observation of one Accepted successor. An unchanged or
/// ambiguous result never permits a second reservation or append without a
/// new ordered admission.
pub fn read_transfer_accepted_successor(
    store: &StateStore,
    transfer: &TransferIdentity,
    preparation: &TransferEvidence,
    import_digest: &str,
    destination: &TransferSeatContext<'_>,
) -> anyhow::Result<TransferAcceptedReadback> {
    if destination.anchor.chain_root_id() != transfer.destination.chain_root_id
        || destination.anchor.thread_id() != transfer.destination.thread_id
    {
        bail!("acceptance readback names another exact destination seat");
    }
    let Some(current) = store.capture_thread_event_read_anchor(
        destination.anchor.chain_root_id(),
        destination.anchor.thread_id(),
    )?
    else {
        return Ok(TransferAcceptedReadback::Ambiguous);
    };
    if current == *destination.anchor {
        return Ok(TransferAcceptedReadback::Unchanged);
    }
    let (Some((previous_hash, previous_seq)), Some((current_hash, current_seq))) =
        (destination.anchor.tip(), current.tip())
    else {
        return Ok(TransferAcceptedReadback::Ambiguous);
    };
    if previous_seq.checked_add(1) != Some(current_seq) {
        return Ok(TransferAcceptedReadback::Ambiguous);
    }
    let page = store.read_thread_event_page(&current, None, 1, destination.max_append_bytes)?;
    let Some((hash, event)) = page.events.first() else {
        return Ok(TransferAcceptedReadback::Ambiguous);
    };
    if hash != current_hash
        || event.thread_seq != current_seq
        || event.prev_thread_event_hash.as_deref() != Some(previous_hash)
    {
        return Ok(TransferAcceptedReadback::Ambiguous);
    }
    let Ok(wrapper) = from_event_value(&event.to_value()) else {
        return Ok(TransferAcceptedReadback::Ambiguous);
    };
    if wrapper.seq != destination.next_engine_seq
        || wrapper.operation_id != transfer.operation_id
        || wrapper.payload.transfer != *transfer
    {
        return Ok(TransferAcceptedReadback::Ambiguous);
    }
    if !matches!(
        &wrapper.payload.phase,
        TransferPhase::Accepted {
            preparation: linked,
            import_digest: admitted,
            ..
        } if linked == preparation && admitted == import_digest
    ) {
        return Ok(TransferAcceptedReadback::Ambiguous);
    }
    Ok(TransferAcceptedReadback::Observed {
        acceptance: TransferEvidence {
            event_hash: hash.clone(),
            chain_seq: event.chain_seq,
        },
    })
}

/// Outcome of a freeze cancellation publication. `AlreadyRecorded` is a
/// signed readback, distinct from a newly published cancellation; neither
/// releases renderer state without full typed replay validation.
#[derive(Debug)]
pub enum MoveFreezeCancellationAppendResult {
    AlreadyRecorded(TransferEvidence),
    Committed(Vec<PersistedEventRecord>),
    Conflict,
    NotRunning,
}

fn cancellation_event(
    record: &MoveFreezeCancellation,
    seat: &TransferSeat,
    bound: u64,
) -> anyhow::Result<NewEventRecord> {
    record.validate_bounds(bound).map_err(anyhow::Error::msg)?;
    let event = indexed_event(MOVE_FREEZE_CANCELLED_EVENT, serde_json::to_value(record)?);
    if !event_fits_bound(&event, &seat.chain_root_id, &seat.thread_id, bound)? {
        bail!("freeze cancellation exceeds the authenticated source append bound");
    }
    Ok(event)
}

/// Source-owned negative commitment published by the live source producer.
/// `seats.target` is the source.
/// This does not prove the nested renderer export or release local state.
pub fn append_move_freeze_cancellation(
    store: &StateStore,
    seat_kind: &str,
    proof: &UiMoveFreezeCancellationProof,
    seats: TransferSeatPair<'_>,
    dependencies: TransferCutDependencies<'_>,
    admit_source_session: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<MoveFreezeCancellationAppendResult> {
    let (source, destination) = (seats.target, seats.counterpart);
    let transfer = proof.transfer();
    if !proof.belongs_to(store)
        || proof.source_anchor() != source.anchor
        || proof.destination_anchor() != destination.anchor
        || source.presentation_id != transfer.source.presentation_id
    {
        bail!(
            "freeze cancellation requires completed same-store exact source and counterpart cuts"
        );
    }
    require_exact_seat_roots(source.anchor, &transfer.source)?;
    require_exact_seat_roots(destination.anchor, &transfer.destination)?;
    if let Some((evidence, _)) = proof.retained_cancellation() {
        return Ok(MoveFreezeCancellationAppendResult::AlreadyRecorded(
            evidence.clone(),
        ));
    }
    let (_, control) = proof.latest_control();
    if control.state != ryeos_ui_presentation_contract::seat_control::UiSeatProducerState::Active
        || control.producer_incarnation != source.producer_incarnation
        || proof.next_engine_seq() != source.next_engine_seq
    {
        bail!("freeze cancellation current producer differs from the authenticated source cut");
    }
    let bound = proof.original_append_bound().min(source.max_append_bytes);
    let record = MoveFreezeCancellation::new(
        transfer.clone(),
        proof.freeze().clone(),
        MoveFreezeCancellationWriter::CurrentProducer {
            producer_incarnation: source.producer_incarnation.into(),
            next_engine_seq: source.next_engine_seq,
        },
    )
    .map_err(anyhow::Error::msg)?;
    let event = cancellation_event(&record, &transfer.source, bound)?;
    let outcome = publish_between_seats(
        store,
        Publication {
            seat_kind,
            target: source,
            target_seat: &transfer.source,
            counterpart: destination,
            counterpart_seat: &transfer.destination,
            events: vec![event],
            dependencies,
        },
        admit_source_session,
    )?;
    cancellation_outcome(outcome)
}

fn cancellation_outcome(
    outcome: TransferAppendResult,
) -> anyhow::Result<MoveFreezeCancellationAppendResult> {
    match outcome {
        TransferAppendResult::Committed(records) => {
            Ok(MoveFreezeCancellationAppendResult::Committed(records))
        }
        TransferAppendResult::Conflict => Ok(MoveFreezeCancellationAppendResult::Conflict),
        TransferAppendResult::NotRunning => Ok(MoveFreezeCancellationAppendResult::NotRunning),
        TransferAppendResult::Uncertain { error } => Err(error.context(
            "freeze cancellation append is uncertain; reconcile from signed source history",
        )),
    }
}

/// Which seats are known to be under a durable reaping claim, decided by the
/// caller from the claims it holds the retention exclusion over.
pub trait ReapingClaims {
    /// Whether `seat_root_id` is claimed for reaping, or already terminal:
    /// either way no browser authority can append to it any more.
    fn is_claimed_or_terminal(&self, seat_root_id: &str) -> anyhow::Result<bool>;
}

/// Claims read through the store.
pub struct StoreReapingClaims<'a> {
    store: &'a StateStore,
}

impl<'a> StoreReapingClaims<'a> {
    pub fn new(store: &'a StateStore) -> Self {
        Self { store }
    }
}

/// Seat leases examined per slice while looking for a claim. Mechanical read
/// granularity only.
const CLAIM_SLICE: usize = 64;

impl ReapingClaims for StoreReapingClaims<'_> {
    fn is_claimed_or_terminal(&self, seat_root_id: &str) -> anyhow::Result<bool> {
        let Some(detail) = self.store.get_thread(seat_root_id)? else {
            // A retired seat has no thread row left and can never append
            // again. A released closure leaves the settlement undecided: the
            // caller keeps its claim and offers the transfer again later.
            return match super::retained::capture_retired_seat(self.store, seat_root_id) {
                Ok(retired) => Ok(retired.is_some()),
                Err(error) if super::retained::is_released_seat(&error) => Ok(false),
                Err(error) => Err(error),
            };
        };
        if detail.status != "running" {
            return Ok(true);
        }
        let Some(cycle_max_id) = self.store.max_seat_lease_id()? else {
            return Ok(false);
        };
        let mut after: Option<String> = None;
        loop {
            let page = self.store.claimed_seat_leases_slice(
                after.as_deref(),
                &cycle_max_id,
                CLAIM_SLICE,
            )?;
            if page.iter().any(|claimed| claimed == seat_root_id) {
                return Ok(true);
            }
            if page.len() < CLAIM_SLICE {
                return Ok(false);
            }
            after = page.last().cloned();
        }
    }
}

/// Publish the cancellation of an expired source's unfinished MOVE freeze
/// under the source's durable reaping claim. The proof is a completed paired
/// cut; the writer rechecks both heads under the publication locks.
pub fn append_reaping_freeze_cancellation(
    store: &StateStore,
    seat_kind: &str,
    claims: &impl ReapingClaims,
    proof: &UiMoveFreezeCancellationProof,
) -> anyhow::Result<MoveFreezeCancellationAppendResult> {
    if !proof.belongs_to(store) {
        bail!("freeze cancellation proof belongs to another state owner");
    }
    if let Some((evidence, _)) = proof.retained_cancellation() {
        return Ok(MoveFreezeCancellationAppendResult::AlreadyRecorded(
            evidence.clone(),
        ));
    }
    let transfer = proof.transfer();
    require_exact_seat_roots(proof.source_anchor(), &transfer.source)?;
    require_exact_seat_roots(proof.destination_anchor(), &transfer.destination)?;
    if !claims.is_claimed_or_terminal(&transfer.source.thread_id)? {
        return Ok(MoveFreezeCancellationAppendResult::Conflict);
    }
    let bound = proof.original_append_bound();
    let (evidence, control) = proof.latest_control();
    let fence = lifecycle_fence(
        control.state == ryeos_ui_presentation_contract::seat_control::UiSeatProducerState::Closed,
        evidence,
    );
    verify_control_fence(&transfer.source, &fence, evidence, control)
        .map_err(anyhow::Error::msg)?;
    let record = MoveFreezeCancellation::new(
        transfer.clone(),
        proof.freeze().clone(),
        MoveFreezeCancellationWriter::NodeLifecycle { fence },
    )
    .map_err(anyhow::Error::msg)?;
    let event = cancellation_event(&record, &transfer.source, bound)?;
    let outcome = publish_node_authored(
        store,
        seat_kind,
        NodeAuthoredWrite {
            owner: &transfer.source,
            owner_anchor: proof.source_anchor(),
            counterpart: NodeCounterpart::Live {
                seat: &transfer.destination,
                anchor: proof.destination_anchor(),
            },
            events: vec![event],
        },
    )?;
    cancellation_outcome(outcome)
}

fn lifecycle_fence(
    closed_control: bool,
    latest_control: &TransferEvidence,
) -> TransferLifecycleFence {
    if closed_control {
        TransferLifecycleFence::ClosedControl {
            latest_control: latest_control.clone(),
        }
    } else {
        TransferLifecycleFence::LeaseReaping {
            latest_control: latest_control.clone(),
        }
    }
}

/// The counterpart of a node-authored publication.
enum NodeCounterpart<'a> {
    Live {
        seat: &'a TransferSeat,
        anchor: &'a AuthoritativeThreadEventReadAnchor,
    },
    Retired(&'a RetiredChainDependency),
}

struct NodeAuthoredWrite<'a> {
    owner: &'a TransferSeat,
    owner_anchor: &'a AuthoritativeThreadEventReadAnchor,
    counterpart: NodeCounterpart<'a>,
    events: Vec<NewEventRecord>,
}

/// Publish a node-authored event on a claimed seat. The seat's identity is
/// whatever its own signed snapshot carries: the node writes on behalf of the
/// claim, not of a browser producer, so there is no producer context to
/// match. The owner's head must be unchanged, and so must the counterpart's:
/// its head when it is live, its fence when it is retired. Live seats must be
/// running seats of the stated kind.
fn publish_node_authored(
    store: &StateStore,
    seat_kind: &str,
    write: NodeAuthoredWrite<'_>,
) -> anyhow::Result<TransferAppendResult> {
    let NodeAuthoredWrite {
        owner,
        owner_anchor,
        counterpart,
        events,
    } = write;
    let mut anchors = vec![owner_anchor.clone()];
    let mut retired = Vec::new();
    let mut live_counterpart = None;
    match &counterpart {
        NodeCounterpart::Live { seat, anchor } => {
            anchors.push((*anchor).clone());
            live_counterpart = Some((*seat, *anchor));
        }
        NodeCounterpart::Retired(dependency) => retired.push((*dependency).clone()),
    }
    let request = MixedCutAppend {
        chain_root_id: &owner.chain_root_id,
        thread_id: &owner.thread_id,
        events: &events,
        expected: owner_anchor,
        counterpart: live_counterpart.map(|(_, anchor)| anchor),
        required_target_kind: seat_kind,
        current_anchors: &anchors,
        retired: &retired,
    };
    let outcome = store.append_events_if_mixed_cut(&request, |view| {
        if view.target.thread_id != owner.thread_id
            || view.target.chain_root_id != owner.chain_root_id
            || view.target.kind_name != seat_kind
            || view.target.status != ThreadStatus::Running
        {
            return Ok(false);
        }
        Ok(match (live_counterpart, view.counterpart) {
            (Some((seat, _)), Some(other)) => {
                other.thread_id == seat.thread_id
                    && other.chain_root_id == seat.chain_root_id
                    && other.kind_name == seat_kind
            }
            (None, None) => true,
            _ => false,
        })
    })?;
    Ok(match outcome {
        CutAppendResult::Committed(records) => TransferAppendResult::Committed(records),
        CutAppendResult::Conflict => TransferAppendResult::Conflict,
        CutAppendResult::NotRunning => TransferAppendResult::NotRunning,
    })
}

/// Publish a lifecycle settlement candidate under the owner's durable reaping
/// claim. The candidate is a completed paired read; the event's fence commits
/// to the exact newest control of the owner. An orphan cancellation
/// additionally requires its source to be claimed or terminal: only then can
/// no Prepared still appear after the complete source history was read.
pub fn append_transfer_settlement(
    store: &StateStore,
    seat_kind: &str,
    claims: &impl ReapingClaims,
    candidate: &TransferSettlementCandidate,
) -> anyhow::Result<TransferAppendResult> {
    if !candidate.belongs_to(store) {
        bail!("transfer settlement candidate belongs to another state owner");
    }
    let record = candidate.record();
    let owner = record.event_owner();
    let counterpart = if owner == &record.transfer.source {
        &record.transfer.destination
    } else {
        &record.transfer.source
    };
    if owner.chain_root_id != owner.thread_id
        || candidate.owner_anchor().chain_root_id() != owner.chain_root_id
        || candidate.owner_anchor().thread_id() != owner.thread_id
    {
        bail!("transfer settlement candidate does not own its exact seat");
    }
    let counterpart_cut = match candidate.counterpart() {
        SettlementCounterpart::Live(anchor) => {
            if anchor.chain_root_id() != counterpart.chain_root_id
                || anchor.thread_id() != counterpart.thread_id
            {
                bail!("transfer settlement candidate does not name its exact counterpart");
            }
            NodeCounterpart::Live {
                seat: counterpart,
                anchor,
            }
        }
        SettlementCounterpart::Retired(dependency) => {
            if dependency.chain_root_id != counterpart.chain_root_id {
                bail!("transfer settlement candidate does not name its exact counterpart");
            }
            NodeCounterpart::Retired(dependency)
        }
    };
    if matches!(record.phase, TransferPhase::Aborted { .. }) && owner != &record.transfer.source {
        bail!("source Abort candidate does not own its exact source seat");
    }
    if matches!(record.phase, TransferPhase::OrphanCancelled { .. }) {
        if owner != &record.transfer.destination {
            bail!("orphan cancellation must be destination-owned");
        }
        if !claims.is_claimed_or_terminal(&counterpart.thread_id)? {
            return Ok(TransferAppendResult::Conflict);
        }
    }
    if !claims.is_claimed_or_terminal(&owner.thread_id)? {
        return Ok(TransferAppendResult::Conflict);
    }
    let append_bound = candidate.original_append_bound();
    if append_bound == 0 {
        bail!("lifecycle settlement append bound is zero");
    }
    let fence = lifecycle_fence(candidate.closed_control(), candidate.latest_control());
    let payload =
        TransferLifecycleEventPayload::new(record.clone(), fence).map_err(anyhow::Error::msg)?;
    payload
        .validate_bounds(append_bound)
        .map_err(anyhow::Error::msg)?;
    let event = indexed_event(TRANSFER_LIFECYCLE_EVENT, serde_json::to_value(payload)?);
    if !event_fits_bound(&event, &owner.chain_root_id, &owner.thread_id, append_bound)? {
        bail!("lifecycle settlement exceeds the seat append bound or the native event ceiling");
    }
    publish_node_authored(
        store,
        seat_kind,
        NodeAuthoredWrite {
            owner,
            owner_anchor: candidate.owner_anchor(),
            counterpart: counterpart_cut,
            events: vec![event],
        },
    )
    .context("publish lifecycle settlement")
}

/// The inputs of the destination-first preparation pair, all from completed
/// reads of the two seats at exactly the anchors their contexts name.
#[derive(Clone, Copy)]
pub struct PreparationInputs<'a> {
    pub transfer: &'a TransferIdentity,
    /// The closed source export, already validated against the source's
    /// verified composition by the caller.
    pub export: &'a serde_json::Value,
    pub source_proof: &'a UiTransferFixedSeatProof,
    pub destination_proof: &'a UiTransferFixedSeatProof,
}

fn seat_of<'a>(seat: &'a TransferSeat, context: &'a TransferSeatContext<'a>) -> ExactSeat<'a> {
    ExactSeat {
        root_id: &seat.chain_root_id,
        owner: context.owner,
        item_ref: context.surface,
        executor_ref: context.client_ref,
    }
}

/// Publish the destination's InboundCommitment and then the source's Prepared
/// event, which links the committed inbound, under one cut. `seats.target` is
/// the destination. Both seats must be absent from each other's history for
/// this operation, and a MOVE carries the freeze links its completed source
/// cut authenticated. Two signed heads cannot move atomically: every result
/// other than `Committed`, `Conflict` and `NotRunning` is partial or uncertain
/// and must be reconciled from signed history before a retry.
pub fn append_transfer_preparation_pair(
    store: &StateStore,
    seat_kind: &str,
    inputs: &PreparationInputs<'_>,
    seats: TransferSeatPair<'_>,
    dependencies: TransferCutDependencies<'_>,
    admit_session: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<CutPairAppendResult> {
    let PreparationInputs {
        transfer,
        export,
        source_proof,
        destination_proof,
    } = *inputs;
    let (destination, source) = (seats.target, seats.counterpart);
    source_proof.validate_preparation_source(store, source.anchor, transfer)?;
    destination_proof.validate_preparation_destination(store, destination.anchor, transfer)?;
    require_exact_seat_roots(destination.anchor, &transfer.destination)?;
    require_exact_seat_roots(source.anchor, &transfer.source)?;
    if destination.presentation_id != transfer.destination.presentation_id
        || source.presentation_id != transfer.source.presentation_id
    {
        bail!("transfer preparation seat contexts differ from the exact transfer seats");
    }
    let inbound = TransferRecord {
        schema: TRANSFER_SCHEMA,
        transfer: transfer.clone(),
        phase: TransferPhase::InboundCommitment,
    };
    let inbound_event = wrapped_transfer_event(inbound, destination, &transfer.destination)?;
    let export_digest = ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(export)
        .map_err(anyhow::Error::msg)?;
    if export_digest != transfer.export_digest {
        bail!("transfer source export differs from exact commitment");
    }
    let freeze = match export.get("mode").and_then(serde_json::Value::as_str) {
        Some("copy") => None,
        Some("move") => {
            Some(source_proof.preparation_freeze_links(store, source.anchor, transfer)?)
        }
        _ => bail!("transfer preparation has no current export mode"),
    };
    let prepared_record = |inbound: TransferEvidence| TransferRecord {
        schema: TRANSFER_SCHEMA,
        transfer: transfer.clone(),
        phase: TransferPhase::Prepared {
            freeze: freeze.clone(),
            inbound,
            export: export.clone(),
        },
    };
    // Deterministic source shape and size failures surface before the
    // destination becomes visible; only the inbound coordinate is replaced by
    // the real one after the signed destination write.
    wrapped_transfer_event(
        prepared_record(TransferEvidence {
            event_hash: "f".repeat(64),
            chain_seq: u64::MAX,
        }),
        source,
        &transfer.source,
    )?;
    let destination_seat = seat_of(&transfer.destination, destination);
    let source_seat = seat_of(&transfer.source, source);
    let mut anchors = vec![destination.anchor.clone(), source.anchor.clone()];
    anchors.extend(dependencies.current_anchors.iter().cloned());
    let cut = SeatCut {
        expected: destination.anchor,
        anchors: &anchors,
        retired: dependencies.retired,
    };
    let request = SeatPairAppend {
        first: &destination_seat,
        second: &source_seat,
        second_anchor: source.anchor,
        first_events: std::slice::from_ref(&inbound_event),
        first_rests_on_second: true,
        cut: &cut,
    };
    store.append_seat_pair_if_mixed_cut(
        &request,
        |view| {
            if !snapshot_is_seat(
                view.first,
                &transfer.destination,
                destination.owner,
                destination.surface,
                destination.client_ref,
                seat_kind,
            ) || !snapshot_is_seat(
                view.second,
                &transfer.source,
                source.owner,
                source.surface,
                source.client_ref,
                seat_kind,
            ) {
                return Ok(false);
            }
            admit_session()
        },
        |committed| {
            let [record] = committed else {
                bail!("transfer inbound commitment published another event count");
            };
            let event = wrapped_transfer_event(
                prepared_record(TransferEvidence {
                    event_hash: record.event_hash.clone().ok_or_else(|| {
                        anyhow!("committed destination inbound has no signed hash")
                    })?,
                    chain_seq: u64::try_from(record.chain_seq)
                        .context("committed destination inbound position")?,
                }),
                source,
                &transfer.source,
            )?;
            Ok(vec![event])
        },
    )
}

/// Observation of the preparation pair after an uncertain attempt. The two
/// heads are read separately, so an unchanged head is not a simultaneous
/// absence proof and never permits a retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferPreparationReadback {
    BothUnchanged,
    DestinationObservedOnly {
        inbound: TransferEvidence,
    },
    LinkedPairObserved {
        inbound: TransferEvidence,
        prepared: TransferEvidence,
    },
    Ambiguous,
}

enum PairSeatReadback {
    Absent,
    Exact {
        evidence: TransferEvidence,
        wrapper: TransferEventPayload,
    },
    Ambiguous,
}

fn read_pair_seat_tip(
    store: &StateStore,
    context: &TransferSeatContext<'_>,
    transfer: &TransferIdentity,
    destination: bool,
) -> anyhow::Result<PairSeatReadback> {
    let original = context.anchor;
    let Some(current) =
        store.capture_thread_event_read_anchor(original.chain_root_id(), original.thread_id())?
    else {
        return Ok(PairSeatReadback::Ambiguous);
    };
    if current == *original {
        return Ok(PairSeatReadback::Absent);
    }
    let (Some((previous_hash, previous_seq)), Some((current_hash, current_seq))) =
        (original.tip(), current.tip())
    else {
        return Ok(PairSeatReadback::Ambiguous);
    };
    if previous_seq.checked_add(1) != Some(current_seq) {
        return Ok(PairSeatReadback::Ambiguous);
    }
    let page = store.read_thread_event_page(&current, None, 1, context.max_append_bytes)?;
    let Some((hash, event)) = page.events.first() else {
        return Ok(PairSeatReadback::Ambiguous);
    };
    if hash != current_hash
        || event.thread_seq != current_seq
        || event.prev_thread_event_hash.as_deref() != Some(previous_hash)
    {
        return Ok(PairSeatReadback::Ambiguous);
    }
    let Ok(wrapper) = from_event_value(&event.to_value()) else {
        return Ok(PairSeatReadback::Ambiguous);
    };
    if wrapper.seq != context.next_engine_seq
        || wrapper.payload.transfer != *transfer
        || !matches!(
            (&wrapper.payload.phase, destination),
            (TransferPhase::InboundCommitment, true) | (TransferPhase::Prepared { .. }, false)
        )
    {
        return Ok(PairSeatReadback::Ambiguous);
    }
    Ok(PairSeatReadback::Exact {
        evidence: TransferEvidence {
            event_hash: hash.clone(),
            chain_seq: event.chain_seq,
        },
        wrapper,
    })
}

/// Fixed-predecessor observation of an uncertain preparation pair. A linked
/// pair is reported only when the source's Prepared names the observed
/// inbound and carries exactly the attempted COPY export.
pub fn read_transfer_preparation_pair(
    store: &StateStore,
    transfer: &TransferIdentity,
    export: &serde_json::Value,
    seats: TransferSeatPair<'_>,
) -> anyhow::Result<TransferPreparationReadback> {
    let (destination, source) = (seats.target, seats.counterpart);
    if destination.anchor.chain_root_id() != transfer.destination.chain_root_id
        || destination.anchor.thread_id() != transfer.destination.thread_id
        || source.anchor.chain_root_id() != transfer.source.chain_root_id
        || source.anchor.thread_id() != transfer.source.thread_id
    {
        bail!("transfer pair readback names another seat");
    }
    let inbound = read_pair_seat_tip(store, destination, transfer, true)?;
    let prepared = read_pair_seat_tip(store, source, transfer, false)?;
    Ok(match (inbound, prepared) {
        (PairSeatReadback::Absent, PairSeatReadback::Absent) => {
            TransferPreparationReadback::BothUnchanged
        }
        (PairSeatReadback::Exact { evidence, .. }, PairSeatReadback::Absent) => {
            TransferPreparationReadback::DestinationObservedOnly { inbound: evidence }
        }
        (
            PairSeatReadback::Exact {
                evidence: inbound, ..
            },
            PairSeatReadback::Exact {
                evidence: prepared,
                wrapper,
            },
        ) if matches!(
            &wrapper.payload.phase,
            TransferPhase::Prepared { inbound: link, export: actual, .. }
                if *link == inbound && actual == export
        ) =>
        {
            TransferPreparationReadback::LinkedPairObserved { inbound, prepared }
        }
        _ => TransferPreparationReadback::Ambiguous,
    })
}
