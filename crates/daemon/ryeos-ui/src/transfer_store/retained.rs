//! The retained evidence of retired seats.
//!
//! A retired seat keeps no history of its own: what survives is the closure
//! its retirement recorded, the coordinates of the retained evidence events,
//! each judged by the composition's evidence validator when the closure was
//! stored. Reading it back therefore trusts retirement-time qualification for
//! the control and receipt ancestry a live read re-derives, and checks only
//! what the retained events themselves state: exact owner, exact decoding and
//! the transfer phase order. The seat's signed initialization is always
//! retained, so a reader can compare it with the admission the session holds
//! instead of trusting the session. The retired seat is covered by the fence this
//! module captures: a publication that depends on it names that fence, and
//! the state layer re-proves it under the chain locks.

use std::collections::BTreeMap;

use anyhow::{Context, anyhow, bail};
use ryeos_app::state_store::StateStore;
use ryeos_client_base::ui::presentation_initialization::{
    PRESENTATION_INITIALIZED_EVENT, UiPresentationInitializationRecord,
};
use ryeos_state::RetiredChainDependency;
use ryeos_state::RetiredChainFence;
use ryeos_state::objects::thread_event::ThreadEvent;
use ryeos_ui_presentation_contract::{
    move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT,
    transfer::{TransferEvidence, TransferIdentity, TransferSeat},
};

use super::reader::{
    RequestedSeatOperation, decode_transfer_phase, freeze_requested_seat_operations,
    route_requested_seat_phase,
};

/// A retired seat's retained evidence, read through its fence.
pub struct RetiredSeatEvidence {
    dependency: RetiredChainDependency,
    initialization: Option<UiPresentationInitializationRecord>,
    owner_events: Vec<(TransferEvidence, ThreadEvent)>,
}

impl RetiredSeatEvidence {
    /// The fence the seat's retirement published. A publication that depends
    /// on this evidence names it in its cut.
    pub fn dependency(&self) -> &RetiredChainDependency {
        &self.dependency
    }

    pub fn chain_root_id(&self) -> &str {
        &self.dependency.chain_root_id
    }

    /// The seat's signed initialization as its retirement retained it. A seat
    /// that retired without ever being initialized retained none.
    pub fn initialization(&self) -> Option<&UiPresentationInitializationRecord> {
        self.initialization.as_ref()
    }

    /// Retained transfer testimony of the retired seat's own chain, oldest
    /// first.
    pub fn owner_events(&self) -> &[(TransferEvidence, ThreadEvent)] {
        &self.owner_events
    }
}

/// Capture the fence and retained evidence of a retired seat. `None` means no
/// fence exists for the root: absence of a fence proves nothing about the
/// chain. A released closure is refused: its retained evidence can no longer
/// prove anything.
pub fn capture_retired_seat(
    store: &StateStore,
    chain_root_id: &str,
) -> anyhow::Result<Option<RetiredSeatEvidence>> {
    let authority = store.pinned_state_authority()?;
    let manifest_hash = match authority
        .require_recovery()?
        .retired_chain_fence(chain_root_id)?
    {
        RetiredChainFence::Absent => return Ok(None),
        RetiredChainFence::Retired { manifest_hash } => manifest_hash,
        RetiredChainFence::Released { .. } => {
            bail!("retired seat {chain_root_id} had its closure released")
        }
    };
    let closure = ryeos_state::read_retired_chain_closure(
        &authority.cas_store()?,
        chain_root_id,
        &manifest_hash,
    )
    .with_context(|| format!("read the retained closure of retired seat {chain_root_id}"))?;
    let mut owner_events = Vec::new();
    let mut initialization = None;
    for coordinate in &closure.retained_evidence {
        let event = store
            .retained_thread_event_by_hash(&coordinate.event_hash)?
            .ok_or_else(|| anyhow!("retained evidence event of {chain_root_id} is not retained"))?;
        if event.chain_seq != coordinate.chain_seq {
            bail!("retained evidence event differs from its recorded coordinate");
        }
        if event.chain_root_id == chain_root_id
            && event.event_type == PRESENTATION_INITIALIZED_EVENT
        {
            let record: UiPresentationInitializationRecord =
                serde_json::from_value(event.payload.clone())
                    .context("decode the retained seat initialization")?;
            record.validate().map_err(anyhow::Error::msg)?;
            if initialization.replace(record).is_some() {
                bail!("retired seat {chain_root_id} retained two initializations");
            }
        } else if event.chain_root_id == chain_root_id {
            owner_events.push((
                TransferEvidence {
                    event_hash: coordinate.event_hash.clone(),
                    chain_seq: event.chain_seq,
                },
                event,
            ));
        }
    }
    owner_events.sort_by_key(|(evidence, _)| evidence.chain_seq);
    if owner_events
        .windows(2)
        .any(|pair| pair[0].0.chain_seq == pair[1].0.chain_seq)
    {
        bail!("retained evidence repeats one chain position");
    }
    Ok(Some(RetiredSeatEvidence {
        dependency: RetiredChainDependency {
            chain_root_id: chain_root_id.to_string(),
            manifest_hash,
        },
        initialization,
        owner_events,
    }))
}

/// The requested operations of a retired seat, from its retained evidence
/// alone. An operation with no retained testimony has an empty phase list,
/// which is absence only because retirement retained every transfer event the
/// seat held toward a live counterpart.
pub(super) fn retired_seat_operations(
    seat: &TransferSeat,
    evidence: &RetiredSeatEvidence,
    requested: &[TransferIdentity],
) -> anyhow::Result<BTreeMap<String, RequestedSeatOperation>> {
    if evidence.chain_root_id() != seat.chain_root_id {
        bail!("retired evidence belongs to another seat");
    }
    let mut operations = freeze_requested_seat_operations(seat, requested)?;
    for (coordinate, event) in evidence.owner_events() {
        if event.event_type == MOVE_FREEZE_CANCELLED_EVENT {
            let cancellation =
                ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(
                    &event.to_value(),
                )
                .map_err(anyhow::Error::msg)?;
            if let Some(operation) = operations.get_mut(&cancellation.transfer.operation_id) {
                if operation.transfer != cancellation.transfer
                    || operation.owner != operation.transfer.source
                    || operation.cancellation.is_some()
                {
                    bail!("retained cancellation conflicts with the requested source identity");
                }
                operation.cancellation = Some((coordinate.clone(), cancellation));
            }
            continue;
        }
        let Some((record, lifecycle)) = decode_transfer_phase(event)? else {
            bail!("retired seat evidence holds non-transfer testimony");
        };
        if record.event_owner().chain_root_id != seat.chain_root_id
            || record.event_owner().thread_id != seat.thread_id
            || record.event_owner().presentation_id != seat.presentation_id
        {
            bail!("retained transfer testimony belongs to another stable seat");
        }
        route_requested_seat_phase(
            &mut operations,
            super::reader::RetainedPhase::new(coordinate.clone(), record, lifecycle),
        )?;
    }
    Ok(operations)
}
