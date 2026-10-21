//! The seat composition's retention pieces.
//!
//! The state and application layers retire fenced chains and reap expired
//! seat leases without knowing what a seat is. This module supplies, together,
//! everything that needs that meaning: the event payload contracts of the
//! transfer testimony, the validator that judges each retained evidence event,
//! the qualifier that decides which terminal seats may retire and what they
//! keep, and the settler that closes a claimed seat's transfers before the
//! seat finalizes.
//!
//! A seat retires only when every transfer it took part in is completely
//! settled on both seats. It then keeps every transfer event it holds toward a
//! live counterpart as retained evidence, so the transfer stays readable
//! through the seat's retirement fence. A transfer whose counterpart seat has
//! already retired needs no evidence here: that counterpart's closure holds
//! the testimony, and nothing can fold the transfer again once both seats are
//! gone.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock, Weak};

use anyhow::{Context, anyhow, bail};
use ryeos_api::maintenance::RetentionComposition;
use ryeos_app::retention_exclusion::RetentionExclusion;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{FencedChainRetirementPlan, StateStore};
use ryeos_client_base::ui::presentation_initialization::{
    PRESENTATION_INITIALIZED_EVENT, UiPresentationInitializationRecord,
};
use ryeos_state::object_closure::{
    EventPayloadContractRegistration, RegisteredObjectEdge, RegisteredObjectExpectation,
    RegisteredObjectLinks,
};
use ryeos_state::objects::thread_event::ThreadEvent;
use ryeos_state::state_db::RetirementEvidenceRef;
use ryeos_ui_presentation_contract::{
    move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT,
    transfer::{TRANSFER_EVENT, TransferEvidence, TransferIdentity},
    transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT,
};
use serde_json::Value;

use super::appends::{
    MoveFreezeCancellationAppendResult, StoreReapingClaims, TransferAppendResult,
    append_reaping_freeze_cancellation, append_transfer_settlement,
};
use super::discovery::{discover_seat_transfer_identities, seat_initialization_event_hash};
use super::historical::{
    HistoryEndpoint, TransferClosure, prepare_settlement_from_cut, read_transfer_cut,
    seat_transfer_evidence,
};
use super::reader::{
    READ_PAGE_BYTES, READ_PAGE_EVENTS, SettlementPreparation, TransferSettlementSide, begin_read,
    decode_transfer_phase, prepare_write, read_page,
};
use super::retained::is_released_seat;

// ---------------------------------------------------------------------------
// Event contracts and the evidence validator
// ---------------------------------------------------------------------------

fn thread_event_edges(links: Vec<&TransferEvidence>) -> RegisteredObjectLinks {
    RegisteredObjectLinks {
        object_edges: links
            .into_iter()
            .map(|evidence| RegisteredObjectEdge {
                hash: evidence.event_hash.clone(),
                expected: RegisteredObjectExpectation::Kind("thread_event"),
            })
            .collect(),
        blob_hashes: Vec::new(),
        large_object_hashes: Vec::new(),
    }
}

fn validate_transfer(value: &Value) -> anyhow::Result<()> {
    ryeos_ui_presentation_contract::transfer::from_event_value(value)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn transfer_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let event = ryeos_ui_presentation_contract::transfer::from_event_value(value)?;
    Ok(thread_event_edges(event.payload.evidence_links()))
}

fn validate_lifecycle(value: &Value) -> anyhow::Result<()> {
    ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(value)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn lifecycle_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let event = ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(value)?;
    Ok(thread_event_edges(event.evidence_links()))
}

fn validate_freeze_cancellation(value: &Value) -> anyhow::Result<()> {
    ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(value)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn freeze_cancellation_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let record = ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(value)?;
    Ok(thread_event_edges(record.evidence_links()))
}

/// The event payload contracts of the transfer testimony. Only the current
/// lifecycle version is registered: an earlier version's selector lives in
/// the reserved namespace, so it is refused as an event without an installed
/// decoder.
pub fn event_contracts() -> Vec<EventPayloadContractRegistration> {
    vec![
        EventPayloadContractRegistration {
            event_type: MOVE_FREEZE_CANCELLED_EVENT,
            validate: validate_freeze_cancellation,
            links: freeze_cancellation_links,
        },
        EventPayloadContractRegistration {
            event_type: TRANSFER_EVENT,
            validate: validate_transfer,
            links: transfer_links,
        },
        EventPayloadContractRegistration {
            event_type: TRANSFER_LIFECYCLE_EVENT,
            validate: validate_lifecycle,
            links: lifecycle_links,
        },
    ]
}

fn validate_initialization(event: &ThreadEvent) -> anyhow::Result<()> {
    let record: UiPresentationInitializationRecord =
        serde_json::from_value(event.payload.clone()).context("decode seat initialization")?;
    record.validate().map_err(anyhow::Error::msg)
}

/// Judge one retained evidence event. Retained evidence is the signed
/// initialization of a seat root or transfer testimony of one, decoded
/// exactly: anything else in a seat's closure would be a retained event
/// nothing can interpret.
pub fn validate_retained_evidence(event: &ThreadEvent) -> anyhow::Result<()> {
    if event.chain_root_id != event.thread_id {
        bail!("retained seat evidence is not on a seat root");
    }
    if event.event_type == PRESENTATION_INITIALIZED_EVENT {
        return validate_initialization(event);
    }
    if event.event_type == MOVE_FREEZE_CANCELLED_EVENT {
        return validate_freeze_cancellation(&event.to_value());
    }
    match decode_transfer_phase(event)? {
        Some(_) => Ok(()),
        None => bail!("retained seat evidence is not transfer testimony"),
    }
}

// ---------------------------------------------------------------------------
// Qualification
// ---------------------------------------------------------------------------

/// Decide whether the terminal seat `root` may retire, and what it keeps.
/// `None` leaves the seat for a later pass: some transfer it took part in is
/// not completely settled, or its history is not exhaustively readable. A
/// plan always keeps the seat's signed initialization, and lists, for every
/// transfer whose counterpart seat is still live, each transfer event the seat
/// holds, with that counterpart. Seats whose preparation rests on this seat's
/// retained prefix are not listed here: the state layer adds every evidence
/// obligation holder to the retained counterparts, so this seat's closure
/// stays unreleased until each of them is fenced.
pub fn qualify_seat_retirement(
    store: &Arc<StateStore>,
    root: &str,
) -> anyhow::Result<Option<FencedChainRetirementPlan>> {
    let Some(anchor) = store.capture_thread_event_read_anchor(root, root)? else {
        return Ok(None);
    };
    let mut plan = FencedChainRetirementPlan::default();
    if let Some(event_hash) = seat_initialization_event_hash(store, root, &anchor)? {
        plan.evidence.push(RetirementEvidenceRef {
            counterpart_root: root.to_string(),
            event_hash,
        });
    }
    let identities = discover_seat_transfer_identities(store, root, &anchor)?;
    if identities.is_empty() {
        return Ok(Some(plan));
    }
    let requested: Vec<TransferIdentity> = identities.values().cloned().collect();
    let cut = match read_transfer_cut(store, &HistoryEndpoint::Current(anchor), &requested) {
        Ok(cut) => cut,
        Err(error) if is_released_seat(&error) => {
            tracing::debug!(%root, "seat retirement deferred: a counterpart closure was released");
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    for transfer in &requested {
        match cut.closure(transfer)? {
            TransferClosure::Closed => {}
            TransferClosure::Open(reason) => {
                tracing::debug!(%root, operation = %transfer.operation_id, reason, "seat retirement deferred");
                return Ok(None);
            }
        }
    }
    let mut retained = BTreeSet::new();
    for transfer in &requested {
        let counterpart_root = if transfer.source.chain_root_id == root {
            &transfer.destination.chain_root_id
        } else {
            &transfer.source.chain_root_id
        };
        let counterpart_is_live = cut
            .endpoint_is_live(counterpart_root)
            .ok_or_else(|| anyhow!("transfer cut omits the counterpart of a closed transfer"))?;
        if !counterpart_is_live {
            continue;
        }
        plan.counterpart_roots.insert(counterpart_root.clone());
        for evidence in seat_transfer_evidence(&cut, root, &transfer.operation_id)? {
            if retained.insert(evidence.event_hash.clone()) {
                plan.evidence.push(RetirementEvidenceRef {
                    counterpart_root: counterpart_root.clone(),
                    event_hash: evidence.event_hash,
                });
            }
        }
    }
    Ok(Some(plan))
}

// ---------------------------------------------------------------------------
// Settlement of claimed seats
// ---------------------------------------------------------------------------

/// Close every transfer the claimed seat `root` took part in, as far as the
/// seat's own authority reaches. `true` means every transfer is completely
/// settled on both seats and the seat may finalize; `false` leaves the claim
/// for a later pass (a counterpart still owes its side, a publication
/// conflicted, or a history is not exhaustively readable).
pub fn settle_claimed_seat_transfers(
    store: &Arc<StateStore>,
    seat_kind: &str,
    root: &str,
) -> anyhow::Result<bool> {
    let claims = StoreReapingClaims::new(store);
    let Some(anchor) = store.capture_thread_event_read_anchor(root, root)? else {
        return Ok(false);
    };
    let identities = discover_seat_transfer_identities(store, root, &anchor)?;
    for transfer in identities.values() {
        let side = if transfer.source.chain_root_id == root {
            TransferSettlementSide::Source
        } else if transfer.destination.chain_root_id == root {
            TransferSettlementSide::Destination
        } else {
            bail!("seat history names a transfer between two other seats");
        };
        loop {
            match settle_one_step(store, seat_kind, &claims, transfer, side, root)? {
                Step::Published => continue,
                Step::Settled => break,
                Step::Later => return Ok(false),
            }
        }
        let anchor = store
            .capture_thread_event_read_anchor(root, root)?
            .ok_or_else(|| anyhow!("claimed seat lost its signed head"))?;
        let cut = read_transfer_cut(
            store,
            &HistoryEndpoint::Current(anchor),
            std::slice::from_ref(transfer),
        )?;
        if cut.closure(transfer)? != TransferClosure::Closed {
            return Ok(false);
        }
    }
    Ok(true)
}

enum Step {
    /// One settlement was published; the transfer must be read again.
    Published,
    /// Nothing more this seat can publish for the transfer.
    Settled,
    /// A publication did not take effect; try again on a later pass.
    Later,
}

fn settle_one_step(
    store: &Arc<StateStore>,
    seat_kind: &str,
    claims: &StoreReapingClaims<'_>,
    transfer: &TransferIdentity,
    side: TransferSettlementSide,
    root: &str,
) -> anyhow::Result<Step> {
    let counterpart_root = match side {
        TransferSettlementSide::Source => &transfer.destination.chain_root_id,
        TransferSettlementSide::Destination => &transfer.source.chain_root_id,
    };
    let counterpart_live = store
        .capture_thread_event_read_anchor(counterpart_root, counterpart_root)?
        .is_some();
    let preparation = if counterpart_live {
        let mut read = begin_read(store.clone(), transfer, side)?;
        while !read_page(&mut read, READ_PAGE_EVENTS, READ_PAGE_BYTES)? {}
        prepare_write(read)?
    } else {
        let anchor = store
            .capture_thread_event_read_anchor(root, root)?
            .ok_or_else(|| anyhow!("claimed seat lost its signed head"))?;
        let cut = match read_transfer_cut(
            store,
            &HistoryEndpoint::Current(anchor),
            std::slice::from_ref(transfer),
        ) {
            Ok(cut) => cut,
            Err(error) if is_released_seat(&error) => return Ok(Step::Later),
            Err(error) => return Err(error),
        };
        prepare_settlement_from_cut(&cut, transfer, side)?
    };
    match preparation {
        SettlementPreparation::Deferred(_) => Ok(Step::Settled),
        SettlementPreparation::FreezeCancellation(proof) => Ok(
            match append_reaping_freeze_cancellation(store, seat_kind, claims, &proof)? {
                MoveFreezeCancellationAppendResult::Committed(_) => Step::Published,
                MoveFreezeCancellationAppendResult::AlreadyRecorded(_) => Step::Settled,
                MoveFreezeCancellationAppendResult::Conflict
                | MoveFreezeCancellationAppendResult::NotRunning => Step::Later,
            },
        ),
        SettlementPreparation::Candidate(candidate) => Ok(
            match append_transfer_settlement(store, seat_kind, claims, &candidate)? {
                TransferAppendResult::Committed(_) => Step::Published,
                TransferAppendResult::Conflict | TransferAppendResult::NotRunning => Step::Later,
                TransferAppendResult::Uncertain { error } => {
                    return Err(error.context("lifecycle settlement outcome is uncertain"));
                }
            },
        ),
    }
}

/// Settle the leases already claimed for reaping. For each claimed seat: a
/// missing or already terminal thread only has its lease released; a running
/// seat has its transfers settled and, once every transfer is completely
/// settled, is finalized and its lease released. A seat with an obligation
/// that cannot be discharged yet stays claimed and is offered again by a
/// later pass. Returns how many seats were finalized.
pub fn settle_claimed_seats(
    state: &AppState,
    exclusion: &RetentionExclusion<'_>,
    claimed: &[String],
) -> anyhow::Result<usize> {
    exclusion.require_exclusive()?;
    let mut finalized = 0usize;
    for root in claimed {
        match settle_claimed_seat(state, root) {
            Ok(true) => finalized += 1,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%root, %error, "claimed seat settlement retained for retry");
            }
        }
    }
    Ok(finalized)
}

fn settle_claimed_seat(state: &AppState, root: &str) -> anyhow::Result<bool> {
    let store = &state.state_store;
    let Some(detail) = store.get_thread(root)? else {
        store.remove_seat_lease(root)?;
        return Ok(false);
    };
    if detail.status != "running" {
        store.remove_terminal_seat_lease_if_proved(root)?;
        return Ok(false);
    }
    if !settle_claimed_seat_transfers(store, &detail.kind, root)
        .with_context(|| format!("settle transfers of claimed seat {root}"))?
    {
        return Ok(false);
    }
    state
        .threads
        .finalize_thread(&ryeos_app::thread_lifecycle::ThreadFinalizeParams {
            thread_id: root.to_string(),
            status: "completed".to_string(),
            outcome_code: Some("seat_lease_expired".to_string()),
            result: None,
            error: None,
            metadata: None,
            artifacts: Vec::new(),
            final_cost: None,
            summary_json: None,
        })?;
    if !store.remove_terminal_seat_lease_if_proved(root)? {
        bail!("finalized seat {root} is not proved terminal in signed authority");
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// The composition
// ---------------------------------------------------------------------------

/// The store the qualifier reads. The qualifier hook receives only a chain
/// root, so the composition root binds the store once, after the store is
/// open. The binding is weak: it never keeps the store open past its owner.
static RETENTION_STORE: OnceLock<Weak<StateStore>> = OnceLock::new();

/// Bind the store the retirement qualifier reads. Binding happens once per
/// process; binding another store is refused.
pub fn bind_retention_store(store: &Arc<StateStore>) -> anyhow::Result<()> {
    let bound = RETENTION_STORE.get_or_init(|| Arc::downgrade(store));
    anyhow::ensure!(
        Weak::ptr_eq(bound, &Arc::downgrade(store)),
        "a different store is already bound to the retention qualifier"
    );
    Ok(())
}

fn bound_store() -> anyhow::Result<Arc<StateStore>> {
    RETENTION_STORE
        .get()
        .ok_or_else(|| anyhow!("the retention qualifier has no bound store"))?
        .upgrade()
        .ok_or_else(|| anyhow!("the store bound to the retention qualifier is closed"))
}

/// Everything the seat composition contributes to retention, for the one
/// injection point the daemon root calls.
pub fn retention_composition() -> RetentionComposition {
    RetentionComposition {
        event_contracts: event_contracts(),
        evidence_validator: validate_retained_evidence,
        qualify: Arc::new(|root| qualify_seat_retirement(&bound_store()?, root)),
        settle_claimed_seats: Arc::new(settle_claimed_seats),
    }
}
