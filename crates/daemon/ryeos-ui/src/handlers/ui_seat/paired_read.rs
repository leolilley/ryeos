//! Completed paired transfer cuts for session-level replay.
//!
//! A cut here is read testimony: the typed transfer layer scanned every seat
//! the requested transfers name at exactly the captured heads and fences. The
//! browser session compares it with its own retained memberships and conditions
//! any publication on every anchor and retired fence the cut names. Nothing in
//! this module grants a writer, a producer or a membership.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_client_base::ui::binding::UiPresentationInitialization;
use ryeos_client_base::ui::composition_replay::UiCompositionReplay;
use ryeos_state::{AuthoritativeThreadEventReadAnchor, RetiredChainDependency};
use ryeos_ui_presentation_contract::{
    move_freeze_lifecycle::MoveFreezeCancellation,
    transfer::{TransferEvidence, TransferIdentity, TransferRecord, TransferSeat},
    transfer_state::TransferLifecycleState,
};

use crate::browser_session::transfer_reservation::{
    CompletedPairedCut, CompletedRetiredFrontier, CompletedRetiredRoot,
};
use crate::browser_session::{BrowserSession, PresentationMembership};
use crate::transfer_store::discovery::{
    discover_retired_seat_transfer_identities, discover_seat_transfer_identities,
};
use crate::transfer_store::historical::{
    HistoryEndpoint, UiHistoricalTransferCutProof, read_historical_transfer_cut,
};
use crate::transfer_store::reader::UiHistoricalReceiptedFreezeWitness;
use crate::transfer_store::retained::{RetiredSeatEvidence, capture_retired_seat};

fn touches_seat(transfer: &TransferIdentity, presentation_id: &str, chain_root_id: &str) -> bool {
    [&transfer.source, &transfer.destination]
        .into_iter()
        .any(|seat| seat.presentation_id == presentation_id && seat.chain_root_id == chain_root_id)
}

/// The exact retained outcome of a cancelled MOVE freeze, with the receipted
/// ordinary events it cancels.
pub(super) struct CancellationFrame<'a> {
    pub cancellation: &'a MoveFreezeCancellation,
    pub preparation: &'a UiHistoricalReceiptedFreezeWitness,
    pub completion: Option<&'a UiHistoricalReceiptedFreezeWitness>,
}

/// A completed cut together with the session memberships of every seat it
/// names. Roots are exact seat roots, so a seat is identified by its root.
pub(crate) struct FixedTransferCut {
    proof: UiHistoricalTransferCutProof,
    memberships: BTreeMap<String, PresentationMembership>,
}

impl FixedTransferCut {
    /// Bind a completed proof to the session memberships of its seats. A seat
    /// the session does not hold under its exact coordinates refuses.
    fn new(
        state: &AppState,
        session: &BrowserSession,
        proof: UiHistoricalTransferCutProof,
    ) -> Result<Self> {
        if !proof.is_for_store(state.state_store.as_ref()) {
            bail!("paired cut belongs to another state owner");
        }
        let mut memberships = BTreeMap::new();
        for transfer in proof.identities().values() {
            for seat in [&transfer.source, &transfer.destination] {
                let membership = session_membership(session, seat)?;
                if let Some(prior) =
                    memberships.insert(seat.presentation_id.clone(), membership.clone())
                    && prior != *membership
                {
                    bail!("paired cut names one presentation with two memberships");
                }
            }
        }
        Ok(Self { proof, memberships })
    }

    /// Every live frontier the cut depends on.
    pub(crate) fn current_anchors(&self) -> Vec<AuthoritativeThreadEventReadAnchor> {
        self.proof.current_anchors()
    }

    /// Every retired fence the cut depends on.
    pub(crate) fn retired_dependencies(&self) -> Vec<RetiredChainDependency> {
        self.proof.retired_dependencies()
    }

    /// Whether the replay of `chain_root_id` consumed exactly the transfer
    /// positions this cut scheduled for it, no more and no fewer.
    pub(crate) fn completed_root_positions(
        &self,
        presentation_id: &str,
        chain_root_id: &str,
        seen: &BTreeMap<String, u64>,
    ) -> bool {
        let mut required = BTreeMap::new();
        for (evidence, record) in self.proof.seat_phases(chain_root_id) {
            if record.event_owner().presentation_id == presentation_id {
                required.insert(evidence.event_hash.clone(), evidence.chain_seq);
            }
        }
        for transfer in self.proof.identities().values() {
            if transfer.source.presentation_id == presentation_id
                && transfer.source.chain_root_id == chain_root_id
                && let Some((evidence, _)) = self.proof.cancellation(&transfer.operation_id)
            {
                required.insert(evidence.event_hash.clone(), evidence.chain_seq);
            }
        }
        &required == seen
    }

    /// The scheduled phase at exactly this position of `chain_root_id`, with
    /// the completed lifecycle that contains it.
    pub(crate) fn authenticated_phase_at(
        &self,
        chain_root_id: &str,
        evidence: &TransferEvidence,
    ) -> Option<(&TransferRecord, &TransferLifecycleState)> {
        let (_, record) = self
            .proof
            .seat_phases(chain_root_id)
            .into_iter()
            .find(|(scheduled, _)| *scheduled == evidence)?;
        let lifecycle = self.proof.lifecycle(&record.transfer.operation_id)?;
        let present = [
            Some(lifecycle.inbound()),
            lifecycle.orphan_cancelled(),
            lifecycle.prepared(),
            lifecycle.accepted(),
            lifecycle.committed(),
            lifecycle.aborted(),
            lifecycle.activated(),
            lifecycle.retained(),
            lifecycle.released(),
            lifecycle.settlement_observed(),
        ]
        .into_iter()
        .flatten()
        .any(|testimony| testimony.evidence == evidence && testimony.record == record);
        present.then_some((record, lifecycle))
    }

    pub(super) fn cancellation_at(
        &self,
        evidence: &TransferEvidence,
    ) -> Option<CancellationFrame<'_>> {
        self.proof.identities().keys().find_map(|operation_id| {
            let (retained, cancellation) = self.proof.cancellation(operation_id)?;
            if retained != evidence {
                return None;
            }
            let preparation = self
                .proof
                .freeze_witness(&cancellation.freeze.preparation)?;
            let completion = match &cancellation.freeze.completion {
                Some(completion) => Some(self.proof.freeze_witness(completion)?),
                None => None,
            };
            Some(CancellationFrame {
                cancellation,
                preparation,
                completion,
            })
        })
    }
}

fn session_membership<'a>(
    session: &'a BrowserSession,
    seat: &TransferSeat,
) -> Result<&'a PresentationMembership> {
    session
        .presentations
        .get(&seat.presentation_id)
        .filter(|membership| {
            membership.chain_root_id.as_deref() == Some(seat.chain_root_id.as_str())
                && membership.thread_id.as_deref() == Some(seat.thread_id.as_str())
        })
        .ok_or_else(|| anyhow!("paired cut names a seat the session does not hold"))
}

impl CompletedPairedCut for FixedTransferCut {
    fn publication_cut_covered_by(
        &self,
        anchors: &[AuthoritativeThreadEventReadAnchor],
        retired: &[RetiredChainDependency],
    ) -> bool {
        if self.proof.identities().is_empty() {
            return false;
        }
        let mut current = BTreeMap::new();
        for anchor in anchors {
            if current
                .insert((anchor.chain_root_id(), anchor.thread_id()), anchor)
                .is_some_and(|previous| previous != anchor)
            {
                return false;
            }
        }
        let mut fences = BTreeMap::new();
        for dependency in retired {
            if current.contains_key(&(
                dependency.chain_root_id.as_str(),
                dependency.chain_root_id.as_str(),
            )) {
                return false;
            }
            if fences
                .insert(dependency.chain_root_id.as_str(), dependency)
                .is_some_and(|previous| previous != dependency)
            {
                return false;
            }
        }
        self.proof.current_anchors().iter().all(|required| {
            current
                .get(&(required.chain_root_id(), required.thread_id()))
                .is_some_and(|actual| *actual == required)
        }) && self.proof.retired_dependencies().iter().all(|required| {
            fences
                .get(required.chain_root_id.as_str())
                .is_some_and(|actual| *actual == required)
        })
    }

    fn publication_memberships_match(&self, session: &BrowserSession) -> bool {
        self.memberships.values().all(|membership| {
            session.presentations.get(&membership.presentation_id) == Some(membership)
        })
    }

    fn covers_seat_anchor(
        &self,
        presentation_id: &str,
        chain_root_id: &str,
        thread_id: &str,
        anchor: &AuthoritativeThreadEventReadAnchor,
    ) -> bool {
        anchor.chain_root_id() == chain_root_id
            && anchor.thread_id() == thread_id
            && self
                .proof
                .identities()
                .values()
                .any(|transfer| touches_seat(transfer, presentation_id, chain_root_id))
            && self
                .proof
                .current_anchors()
                .iter()
                .any(|expected| expected == anchor)
    }

    fn covers_retired_seat(
        &self,
        presentation_id: &str,
        chain_root_id: &str,
        dependency: &RetiredChainDependency,
    ) -> bool {
        dependency.chain_root_id == chain_root_id
            && self
                .proof
                .identities()
                .values()
                .any(|transfer| touches_seat(transfer, presentation_id, chain_root_id))
            && self
                .proof
                .retired_dependencies()
                .iter()
                .any(|expected| expected == dependency)
    }

    fn for_each_authenticated_phase(
        &self,
        visit: &mut dyn FnMut(
            &TransferEvidence,
            &TransferRecord,
            &TransferLifecycleState,
        ) -> Result<()>,
    ) -> Result<()> {
        let roots = self
            .proof
            .identities()
            .values()
            .flat_map(|transfer| {
                [
                    transfer.source.chain_root_id.as_str(),
                    transfer.destination.chain_root_id.as_str(),
                ]
            })
            .collect::<std::collections::BTreeSet<_>>();
        for root in roots {
            for (evidence, _) in self.proof.seat_phases(root) {
                let Some((record, lifecycle)) = self.authenticated_phase_at(root, evidence) else {
                    bail!("scheduled phase differs from completed paired testimony");
                };
                visit(evidence, record, lifecycle)?;
            }
        }
        Ok(())
    }
}

/// Read the completed paired cut of one live transfer-bearing root at its
/// captured head. A seat whose history names no transfer has no cut.
pub(crate) async fn read_live_root_cut(
    state: Arc<AppState>,
    session: &BrowserSession,
    membership: &PresentationMembership,
    anchor: AuthoritativeThreadEventReadAnchor,
) -> Result<Arc<FixedTransferCut>> {
    let root = anchor.chain_root_id().to_owned();
    let task_state = state.clone();
    let proof = tokio::task::spawn_blocking(move || -> Result<_> {
        let identities =
            discover_seat_transfer_identities(&task_state.state_store, &root, &anchor)?;
        if identities.is_empty() {
            bail!("transfer-bearing seat names no transfer");
        }
        let requested = identities.into_values().collect::<Vec<_>>();
        read_historical_transfer_cut(
            &task_state.state_store,
            &HistoryEndpoint::Current(anchor),
            &requested,
        )
    })
    .await
    .map_err(|error| HandlerError::Internal(format!("paired cut reader failed: {error}")))?
    .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    let cut = FixedTransferCut::new(&state, session, proof)
        .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    if cut.memberships.get(&membership.presentation_id) != Some(membership) {
        return Err(HandlerError::Conflict("paired cut root membership changed".into()).into());
    }
    Ok(Arc::new(cut))
}

/// A retired seat as the session reads it: its retirement fence, its retained
/// transfer evidence and the paired cut that evidence names, if any.
pub(crate) struct RetiredRoot {
    evidence: RetiredSeatEvidence,
    cut: Option<FixedTransferCut>,
}

impl RetiredRoot {
    pub(crate) fn cut(&self) -> Option<&FixedTransferCut> {
        self.cut.as_ref()
    }

    pub(crate) fn dependency(&self) -> &RetiredChainDependency {
        self.evidence.dependency()
    }
}

/// Read a retired root's cut from its retained evidence. A retired seat with
/// no retained transfer testimony has no cut, and neither has one whose every
/// transfer is between retired seats: nothing of such a transfer is left to
/// reconcile, and its counterpart kept no testimony to read it from.
pub(crate) async fn read_retired_root(
    state: Arc<AppState>,
    session: &BrowserSession,
    membership: &PresentationMembership,
    evidence: RetiredSeatEvidence,
) -> Result<RetiredRoot> {
    if membership.chain_root_id.as_deref() != Some(evidence.chain_root_id())
        || membership.thread_id.as_deref() != Some(evidence.chain_root_id())
    {
        return Err(
            HandlerError::Conflict("retired evidence belongs to another seat".into()).into(),
        );
    }
    if evidence.owner_events().is_empty() {
        return Ok(RetiredRoot {
            evidence,
            cut: None,
        });
    }
    let task_state = state.clone();
    let (evidence, proof) = tokio::task::spawn_blocking(move || -> Result<_> {
        let store = &task_state.state_store;
        let mut requested = Vec::new();
        for identity in discover_retired_seat_transfer_identities(&evidence)?.into_values() {
            if !counterparts_all_retired(store, evidence.chain_root_id(), &identity)? {
                requested.push(identity);
            }
        }
        if requested.is_empty() {
            return Ok((evidence, None));
        }
        let proof = read_historical_transfer_cut(
            store,
            &HistoryEndpoint::Retired(evidence.dependency().clone()),
            &requested,
        )?;
        Ok((evidence, Some(proof)))
    })
    .await
    .map_err(|error| HandlerError::Internal(format!("retired cut reader failed: {error}")))?
    .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    let Some(proof) = proof else {
        return Ok(RetiredRoot {
            evidence,
            cut: None,
        });
    };
    let cut = FixedTransferCut::new(&state, session, proof)
        .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    if cut.memberships.get(&membership.presentation_id) != Some(membership) {
        return Err(HandlerError::Conflict("retired cut root membership changed".into()).into());
    }
    Ok(RetiredRoot {
        evidence,
        cut: Some(cut),
    })
}

/// Whether every seat of the transfer other than `root` is a retired chain.
/// A seat with neither a head nor a fence is not retired: the cut refuses it.
fn counterparts_all_retired(
    store: &ryeos_app::state_store::StateStore,
    root: &str,
    transfer: &TransferIdentity,
) -> Result<bool> {
    for seat in [&transfer.source, &transfer.destination] {
        if seat.chain_root_id == root {
            continue;
        }
        if store
            .capture_thread_event_read_anchor(&seat.chain_root_id, &seat.thread_id)?
            .is_some()
            || capture_retired_seat(store, &seat.chain_root_id)?.is_none()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

impl CompletedRetiredRoot for RetiredRoot {
    fn completed_paired_cut(&self) -> Option<&dyn CompletedPairedCut> {
        self.cut.as_ref().map(|cut| cut as &dyn CompletedPairedCut)
    }

    fn matches_completed_retired_dependency(&self, dependency: &RetiredChainDependency) -> bool {
        self.evidence.dependency() == dependency
    }

    /// A retired seat keeps no composition, only its transfer evidence, so
    /// its frontier is the original initialization and nothing folded onto it.
    /// Transfer testimony is seen exactly when a paired cut was read for it.
    fn completed_composition_frontier(
        &self,
        initialization: &UiPresentationInitialization,
    ) -> Result<CompletedRetiredFrontier> {
        Ok(CompletedRetiredFrontier {
            replay: UiCompositionReplay::initialized(initialization)
                .map_err(|error| anyhow!(error))?,
            transfer_testimony_seen: self.cut.is_some(),
        })
    }
}
