//! Whole-transfer reads across every seat a requested set of transfers names.
//!
//! A cut captures every endpoint before scanning any one: a live seat as an
//! exact signed anchor, a retired seat as its fence and retained evidence.
//! Each live seat is then scanned once for all requested operations. Nothing
//! here is admission: a publication conditions on the anchors and fences the
//! cut returns and rechecks its own mutable admission under the locks.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use ryeos_app::state_store::StateStore;
use ryeos_state::{AuthoritativeThreadEventReadAnchor, RetiredChainDependency};
use ryeos_ui_presentation_contract::{
    move_freeze_lifecycle::MoveFreezeCancellation,
    seat_control::{UiSeatProducer, UiSeatProducerState},
    transfer::{TransferEvidence, TransferIdentity, TransferPhase, TransferRecord, TransferSeat},
    transfer_state::TransferLifecycleState,
};

use super::reader::{
    READ_PAGE_BYTES, READ_PAGE_EVENTS, RequestedSeatOperation, SeatHistory, SettlementCounterpart,
    SettlementPreparation, TransferSettlementCandidate, TransferSettlementPlan,
    TransferSettlementSide, UiHistoricalReceiptedFreezeWitness,
    completed_negative_terminal_decision, consume_selected_seat_event, covered_seat_operation,
    fold_transfer_phase_slices, freeze_requested_seat_operations, same_stable_seat,
    select_settlement, validate_negative_destination_phases, verify_phase_slice_fences,
    verify_selected_freeze,
};
use super::retained::{capture_retired_seat, retired_seat_operations};

/// Where one seat's testimony comes from in a cut.
#[derive(Clone, Debug)]
pub enum HistoryEndpoint {
    /// A live seat at an exact signed frontier.
    Current(AuthoritativeThreadEventReadAnchor),
    /// A retired seat, through its fence and retained evidence.
    Retired(RetiredChainDependency),
}

impl HistoryEndpoint {
    pub fn chain_root_id(&self) -> &str {
        match self {
            Self::Current(anchor) => anchor.chain_root_id(),
            Self::Retired(dependency) => &dependency.chain_root_id,
        }
    }

    pub fn current_anchor(&self) -> Option<&AuthoritativeThreadEventReadAnchor> {
        match self {
            Self::Current(anchor) => Some(anchor),
            Self::Retired(_) => None,
        }
    }
}

/// One seat of a cut with everything requested of it.
pub(super) struct EndpointRead {
    pub(super) seat: TransferSeat,
    pub(super) endpoint: HistoryEndpoint,
    pub(super) operations: BTreeMap<String, RequestedSeatOperation>,
    /// Present for live endpoints only: a retired endpoint keeps no history
    /// to authenticate, so it has no initialization, control or append bound
    /// to report.
    pub(super) live: Option<LiveSeatFacts>,
}

/// What a complete live scan authenticated about the seat itself.
pub(super) struct LiveSeatFacts {
    pub(super) initialization: serde_json::Value,
    pub(super) latest_control: (TransferEvidence, UiSeatProducer),
    pub(super) original_append_bound: u64,
}

/// A completed cut over every seat the requested transfers name.
pub struct TransferCutRead {
    store: Arc<StateStore>,
    local: HistoryEndpoint,
    identities: BTreeMap<String, TransferIdentity>,
    pub(super) endpoints: BTreeMap<String, EndpointRead>,
}

struct SeatGroup {
    seat: TransferSeat,
    requests: Vec<TransferIdentity>,
}

fn group_requests(
    requested: &[TransferIdentity],
) -> anyhow::Result<(
    BTreeMap<String, TransferIdentity>,
    BTreeMap<String, SeatGroup>,
)> {
    let mut groups = BTreeMap::<String, SeatGroup>::new();
    let mut presentations = BTreeMap::new();
    let mut identities = BTreeMap::new();
    for transfer in requested {
        TransferRecord {
            schema: ryeos_ui_presentation_contract::transfer::TRANSFER_SCHEMA,
            transfer: transfer.clone(),
            phase: TransferPhase::InboundCommitment,
        }
        .validate_shape()
        .map_err(anyhow::Error::msg)?;
        if let Some(prior) = identities.insert(transfer.operation_id.clone(), transfer.clone()) {
            if prior != *transfer {
                bail!("transfer cut has conflicting operation identities");
            }
            continue;
        }
        for seat in [&transfer.source, &transfer.destination] {
            if seat.chain_root_id != seat.thread_id {
                bail!("transfer cut requires root seats");
            }
            if let Some(prior) =
                presentations.insert(seat.presentation_id.clone(), seat.chain_root_id.clone())
                && prior != seat.chain_root_id
            {
                bail!("transfer cut presentation changes root");
            }
            let group = groups
                .entry(seat.chain_root_id.clone())
                .or_insert_with(|| SeatGroup {
                    seat: seat.clone(),
                    requests: Vec::new(),
                });
            if !same_stable_seat(&group.seat, seat) {
                bail!("transfer cut root changes stable seat ownership");
            }
            group.requests.push(transfer.clone());
        }
    }
    Ok((identities, groups))
}

/// Read every seat of `requested` for exactly the requested operations.
/// `local` is the cut the caller already holds for its own seat: it is used
/// as given and never recaptured. Every other endpoint is captured before any
/// live seat is scanned. A seat with neither a signed head nor a retirement
/// fence has no authenticated endpoint and refuses the whole cut.
pub fn read_transfer_cut(
    store: &Arc<StateStore>,
    local: &HistoryEndpoint,
    requested: &[TransferIdentity],
) -> anyhow::Result<TransferCutRead> {
    let (identities, groups) = group_requests(requested)?;
    let mut captured: BTreeMap<String, (SeatGroup, HistoryEndpoint, Option<_>)> = BTreeMap::new();
    for (root, group) in groups {
        let (endpoint, retired) = if root == local.chain_root_id() {
            match local {
                HistoryEndpoint::Current(anchor) => {
                    (HistoryEndpoint::Current(anchor.clone()), None)
                }
                HistoryEndpoint::Retired(_) => {
                    let evidence = capture_retired_seat(store, &root)?
                        .ok_or_else(|| anyhow!("local retired seat has no fence"))?;
                    if evidence.dependency()
                        != match local {
                            HistoryEndpoint::Retired(dependency) => dependency,
                            HistoryEndpoint::Current(_) => unreachable!("matched above"),
                        }
                    {
                        bail!("local retired seat closure changed");
                    }
                    (
                        HistoryEndpoint::Retired(evidence.dependency().clone()),
                        Some(evidence),
                    )
                }
            }
        } else if let Some(anchor) = store.capture_thread_event_read_anchor(&root, &root)? {
            (HistoryEndpoint::Current(anchor), None)
        } else {
            let evidence = capture_retired_seat(store, &root)?.ok_or_else(|| {
                anyhow!("transfer cut seat {root} has neither a signed head nor a retirement fence")
            })?;
            (
                HistoryEndpoint::Retired(evidence.dependency().clone()),
                Some(evidence),
            )
        };
        captured.insert(root, (group, endpoint, retired));
    }
    let mut endpoints = BTreeMap::new();
    for (root, (group, endpoint, retired)) in captured {
        let read = match (&endpoint, retired) {
            (HistoryEndpoint::Current(anchor), _) => {
                scan_current(store, &group.seat, anchor, &group.requests)?
            }
            (HistoryEndpoint::Retired(_), Some(evidence)) => (
                retired_seat_operations(&group.seat, &evidence, &group.requests)?,
                None,
            ),
            (HistoryEndpoint::Retired(_), None) => {
                bail!("retired endpoint has no captured evidence")
            }
        };
        endpoints.insert(
            root,
            EndpointRead {
                seat: group.seat,
                endpoint,
                operations: read.0,
                live: read.1,
            },
        );
    }
    Ok(TransferCutRead {
        store: store.clone(),
        local: local.clone(),
        identities,
        endpoints,
    })
}

/// Scan one live seat to genesis once for every requested operation, and
/// verify what the complete history proves about each.
fn scan_current(
    store: &StateStore,
    seat: &TransferSeat,
    anchor: &AuthoritativeThreadEventReadAnchor,
    requested: &[TransferIdentity],
) -> anyhow::Result<(
    BTreeMap<String, RequestedSeatOperation>,
    Option<LiveSeatFacts>,
)> {
    if anchor.chain_root_id() != seat.chain_root_id || anchor.thread_id() != seat.thread_id {
        bail!("transfer cut anchor differs from its exact seat");
    }
    let mut operations = freeze_requested_seat_operations(seat, requested)?;
    let mut history = SeatHistory::fresh(seat.clone(), anchor.clone());
    let mut progress = None;
    loop {
        let page = store.read_thread_event_page(
            anchor,
            progress.as_ref(),
            READ_PAGE_EVENTS,
            READ_PAGE_BYTES,
        )?;
        if page.events.is_empty() && page.progress.is_some() {
            bail!("transfer cut page made no progress");
        }
        for (hash, event) in page.events {
            if history.previous_thread_seq.is_none()
                && anchor.tip() != Some((hash.as_str(), event.thread_seq))
            {
                bail!("transfer cut scan differs from its authenticated endpoint");
            }
            consume_selected_seat_event(&mut history, &mut operations, hash, event)?;
        }
        progress = page.progress;
        if progress.is_none() {
            history.complete = true;
            break;
        }
    }
    history.finish()?;
    for operation in operations.values() {
        verify_phase_slice_fences(history.latest_control.as_ref(), &operation.phases)?;
        verify_selected_freeze(operation)?;
        if let Some((_, preparation)) = &operation.freeze_preparation
            && history.initialized_root_binding.as_ref() != Some(&preparation.root_binding)
        {
            bail!("transfer cut freeze root differs from original initialization");
        }
    }
    let facts = LiveSeatFacts {
        initialization: history
            .initialized_payload
            .clone()
            .ok_or_else(|| anyhow!("transfer cut seat lacks its original initialization"))?,
        latest_control: history
            .latest_control
            .clone()
            .ok_or_else(|| anyhow!("transfer cut seat has no authenticated producer control"))?,
        original_append_bound: history
            .original_append_bound
            .ok_or_else(|| anyhow!("transfer cut seat has no authenticated append bound"))?,
    };
    Ok((operations, Some(facts)))
}

impl TransferCutRead {
    pub fn is_for_store(&self, store: &StateStore) -> bool {
        std::ptr::eq(self.store.as_ref(), store)
    }

    pub fn local_endpoint(&self) -> &HistoryEndpoint {
        &self.local
    }

    pub fn identities(&self) -> &BTreeMap<String, TransferIdentity> {
        &self.identities
    }

    /// Every live frontier of the cut, local seat first when it is live.
    pub fn current_anchors(&self) -> Vec<AuthoritativeThreadEventReadAnchor> {
        let local_root = self.local.chain_root_id();
        let mut anchors: Vec<_> = self
            .endpoints
            .get(local_root)
            .and_then(|read| read.endpoint.current_anchor())
            .cloned()
            .into_iter()
            .collect();
        anchors.extend(
            self.endpoints
                .iter()
                .filter(|(root, _)| root.as_str() != local_root)
                .filter_map(|(_, read)| read.endpoint.current_anchor().cloned()),
        );
        anchors
    }

    /// Every retired seat of the cut, by the fence it was read through.
    pub fn retired_dependencies(&self) -> Vec<RetiredChainDependency> {
        self.endpoints
            .values()
            .filter_map(|read| match &read.endpoint {
                HistoryEndpoint::Retired(dependency) => Some(dependency.clone()),
                HistoryEndpoint::Current(_) => None,
            })
            .collect()
    }

    /// Whether the seat `root` was read as a live endpoint (`Some(true)`), as
    /// a retired one (`Some(false)`), or is not part of the cut.
    pub fn endpoint_is_live(&self, root: &str) -> Option<bool> {
        self.endpoints
            .get(root)
            .map(|read| matches!(read.endpoint, HistoryEndpoint::Current(_)))
    }

    /// Whether the transfer is completely settled on both seats.
    pub fn closure(&self, transfer: &TransferIdentity) -> anyhow::Result<TransferClosure> {
        let (source, destination) = self.operations_for(transfer)?;
        let source_is_live = matches!(
            self.endpoints
                .get(&transfer.source.chain_root_id)
                .map(|read| &read.endpoint),
            Some(HistoryEndpoint::Current(_))
        );
        judge_closure(source, source_is_live, destination, transfer)
    }

    /// The two seats' coverage of one requested transfer.
    pub(super) fn operations_for(
        &self,
        transfer: &TransferIdentity,
    ) -> anyhow::Result<(&RequestedSeatOperation, &RequestedSeatOperation)> {
        let source = self
            .endpoints
            .get(&transfer.source.chain_root_id)
            .ok_or_else(|| anyhow!("transfer cut source endpoint is absent"))?;
        let destination = self
            .endpoints
            .get(&transfer.destination.chain_root_id)
            .ok_or_else(|| anyhow!("transfer cut destination endpoint is absent"))?;
        Ok((
            covered_seat_operation(&source.seat, &source.operations, transfer)?,
            covered_seat_operation(&destination.seat, &destination.operations, transfer)?,
        ))
    }
}

/// Completed historical testimony about a set of transfers. Mutable
/// publication still needs current membership authorization and a cut over
/// `current_anchors` and `retired_dependencies`.
pub struct UiHistoricalTransferCutProof {
    cut: TransferCutRead,
    initializations: BTreeMap<String, serde_json::Value>,
    freeze_witnesses: BTreeMap<String, UiHistoricalReceiptedFreezeWitness>,
    lifecycles: BTreeMap<String, TransferLifecycleState>,
    cancellations: BTreeMap<String, (TransferEvidence, MoveFreezeCancellation)>,
}

impl UiHistoricalTransferCutProof {
    pub fn initialization_matches(&self, root: &str, initialization: &serde_json::Value) -> bool {
        self.initializations.get(root) == Some(initialization)
    }

    pub fn freeze_witness(
        &self,
        evidence: &TransferEvidence,
    ) -> Option<&UiHistoricalReceiptedFreezeWitness> {
        self.freeze_witnesses
            .get(&evidence.event_hash)
            .filter(|witness| witness.evidence == *evidence)
    }

    pub fn is_for_store(&self, store: &StateStore) -> bool {
        self.cut.is_for_store(store)
    }

    pub fn local_endpoint(&self) -> &HistoryEndpoint {
        self.cut.local_endpoint()
    }

    pub fn identities(&self) -> &BTreeMap<String, TransferIdentity> {
        self.cut.identities()
    }

    pub fn current_anchors(&self) -> Vec<AuthoritativeThreadEventReadAnchor> {
        self.cut.current_anchors()
    }

    pub fn retired_dependencies(&self) -> Vec<RetiredChainDependency> {
        self.cut.retired_dependencies()
    }

    pub fn lifecycle(&self, operation_id: &str) -> Option<&TransferLifecycleState> {
        self.lifecycles.get(operation_id)
    }

    pub fn cancellation(
        &self,
        operation_id: &str,
    ) -> Option<&(TransferEvidence, MoveFreezeCancellation)> {
        self.cancellations.get(operation_id)
    }
}

/// Read a historical cut and fold every requested transfer's lifecycle.
pub fn read_historical_transfer_cut(
    store: &Arc<StateStore>,
    local: &HistoryEndpoint,
    requested: &[TransferIdentity],
) -> anyhow::Result<UiHistoricalTransferCutProof> {
    let cut = read_transfer_cut(store, local, requested)?;
    let mut lifecycles = BTreeMap::new();
    let mut cancellations = BTreeMap::new();
    let mut freeze_witnesses = BTreeMap::new();
    let initializations = cut
        .endpoints
        .iter()
        .filter_map(|(root, read)| {
            read.live
                .as_ref()
                .map(|live| (root.clone(), live.initialization.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    for transfer in cut.identities.values() {
        let (source, destination) = cut.operations_for(transfer)?;
        if !source.phases.is_empty() || !destination.phases.is_empty() {
            lifecycles.insert(
                transfer.operation_id.clone(),
                fold_transfer_phase_slices(transfer, &source.phases, &destination.phases)?,
            );
        }
        if let Some(cancellation) = &source.cancellation {
            for (hash, witness) in &source.freeze_witnesses {
                freeze_witnesses.insert(hash.clone(), witness.clone());
            }
            cancellations.insert(transfer.operation_id.clone(), cancellation.clone());
        }
    }
    Ok(UiHistoricalTransferCutProof {
        cut,
        initializations,
        freeze_witnesses,
        lifecycles,
        cancellations,
    })
}

/// Whether one transfer is completely settled on both seats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferClosure {
    /// Every obligation of the transfer is discharged: a seat touching only
    /// closed transfers may finalize and retire.
    Closed,
    /// An obligation remains; the reason is for diagnostics only.
    Open(&'static str),
}

/// Judge one requested transfer from its two completed seat coverages. A
/// transfer is closed when the source observed the destination's settlement,
/// when an abort was released, or when a negative decision (an abort with no
/// acceptance, an orphan cancellation, a retained freeze cancellation with
/// its destination disposed of) completed. Contradictory coverage is an
/// error, never an open transfer.
fn judge_closure(
    source: &RequestedSeatOperation,
    source_is_live: bool,
    destination: &RequestedSeatOperation,
    transfer: &TransferIdentity,
) -> anyhow::Result<TransferClosure> {
    let cancelled = if let Some((_, cancellation)) = &source.cancellation {
        if cancellation.transfer != *transfer
            || (source_is_live && source.freeze_preparation.is_none())
            || !source.phases.is_empty()
        {
            bail!("retained freeze cancellation conflicts with transfer phases");
        }
        validate_negative_destination_phases(transfer, &destination.phases)?;
        true
    } else {
        false
    };
    if cancelled && destination.phases.is_empty() {
        return Ok(TransferClosure::Closed);
    }
    let lifecycle = fold_transfer_phase_slices(transfer, &source.phases, &destination.phases)?;
    let negative = completed_negative_terminal_decision(&lifecycle);
    if cancelled && negative.is_none() {
        return Ok(TransferClosure::Open(
            "a cancelled freeze still has an undisposed destination inbound",
        ));
    }
    if !cancelled
        && lifecycle.settlement_observed().is_none()
        && !(lifecycle.aborted().is_some() && lifecycle.released().is_some())
        && negative.is_none()
    {
        return Ok(TransferClosure::Open(
            "the transfer lacks completed settlement on both seats",
        ));
    }
    if source_is_live
        && source.freeze_preparation.is_some()
        && !cancelled
        && !lifecycle.prepared().is_some_and(|prepared| {
            matches!(
                &prepared.record.phase,
                TransferPhase::Prepared {
                    freeze: Some(_),
                    ..
                }
            )
        })
    {
        bail!("an ordinary freeze has no matching MOVE preparation");
    }
    Ok(TransferClosure::Closed)
}

/// The events the seat `root` holds toward one requested transfer: every
/// phase and the freeze cancellation, oldest first. This is the evidence a
/// retirement keeps so the transfer stays readable through the seat's fence.
pub fn seat_transfer_evidence(
    cut: &TransferCutRead,
    root: &str,
    operation_id: &str,
) -> anyhow::Result<Vec<TransferEvidence>> {
    let read = cut
        .endpoints
        .get(root)
        .ok_or_else(|| anyhow!("transfer cut has no endpoint for seat {root}"))?;
    let operation = read
        .operations
        .get(operation_id)
        .ok_or_else(|| anyhow!("transfer cut seat {root} did not request {operation_id}"))?;
    let mut events = BTreeSet::new();
    for phase in &operation.phases {
        events.insert((phase.evidence.chain_seq, phase.evidence.event_hash.clone()));
    }
    if let Some((evidence, _)) = &operation.cancellation {
        events.insert((evidence.chain_seq, evidence.event_hash.clone()));
    }
    Ok(events
        .into_iter()
        .map(|(chain_seq, event_hash)| TransferEvidence {
            event_hash,
            chain_seq,
        })
        .collect())
}

/// Select the settlement of one side of one requested transfer from a
/// completed cut. The counterpart may be live or retired. A source with an
/// unfinished ordinary freeze is not selectable here: its cancellation needs
/// the receipted fixed proofs and goes through the fixed-read path.
pub fn prepare_settlement_from_cut(
    cut: &TransferCutRead,
    transfer: &TransferIdentity,
    side: TransferSettlementSide,
) -> anyhow::Result<SettlementPreparation> {
    let (source, destination) = cut.operations_for(transfer)?;
    if side == TransferSettlementSide::Source
        && source.phases.is_empty()
        && (source.freeze_preparation.is_some() || source.cancellation.is_some())
    {
        bail!("an unfinished or cancelled freeze is settled through fixed proofs");
    }
    let state = fold_transfer_phase_slices(transfer, &source.phases, &destination.phases)?;
    let plan = select_settlement(&state, side).map_err(anyhow::Error::msg)?;
    let TransferSettlementPlan::Candidate { record, .. } = &plan else {
        return Ok(SettlementPreparation::Deferred(plan));
    };
    let (owner_root, counterpart_root) = match side {
        TransferSettlementSide::Source => (
            &transfer.source.chain_root_id,
            &transfer.destination.chain_root_id,
        ),
        TransferSettlementSide::Destination => (
            &transfer.destination.chain_root_id,
            &transfer.source.chain_root_id,
        ),
    };
    let owner = cut
        .endpoints
        .get(owner_root)
        .ok_or_else(|| anyhow!("transfer cut owner endpoint is absent"))?;
    let counterpart = cut
        .endpoints
        .get(counterpart_root)
        .ok_or_else(|| anyhow!("transfer cut counterpart endpoint is absent"))?;
    let (HistoryEndpoint::Current(owner_anchor), Some(live)) = (&owner.endpoint, &owner.live)
    else {
        bail!("a retired seat cannot be settled");
    };
    let (fence, control) = &live.latest_control;
    Ok(SettlementPreparation::Candidate(
        TransferSettlementCandidate {
            store: cut.store.clone(),
            record: record.clone(),
            latest_control: fence.clone(),
            closed_control: control.state == UiSeatProducerState::Closed,
            owner_anchor: owner_anchor.clone(),
            counterpart: match &counterpart.endpoint {
                HistoryEndpoint::Current(anchor) => SettlementCounterpart::Live(anchor.clone()),
                HistoryEndpoint::Retired(dependency) => {
                    SettlementCounterpart::Retired(dependency.clone())
                }
            },
            original_append_bound: live.original_append_bound,
        },
    ))
}
