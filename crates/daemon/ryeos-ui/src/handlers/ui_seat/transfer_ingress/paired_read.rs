//! Private grouped app cuts for requested transfers, not the client replay lane.
//! Each reached seat is authorized before its discovery capture or CAS access.
//! All memberships are reauthorized and all cuts captured before proof-cache
//! lookup or proof scanning. One seat proof scan covers its whole
//! frozen operation scope; no producer, grant or live authority is constructed.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

use anyhow::{Result, anyhow, bail};
use ryeos_app::{
    handler_error::HandlerError,
    state::AppState,
    thread_lifecycle::presentation_transfer::{
        UiTransferFixedHistoryProof, UiTransferFixedSeatProof, assemble_fixed_history_proof,
        read_signed_seat_transfer_identity_page,
    },
};
use ryeos_ui_presentation_contract::transfer::{
    TRANSFER_SCHEMA, TransferIdentity, TransferPhase, TransferRecord, TransferSeat,
};

use super::{BrowserSession, PresentationMembership, get_ui_state};
use ryeos_state::AuthoritativeThreadEventReadAnchor;

type SeatKey = (String, String, String);

struct ScheduleCancellation(Arc<std::sync::atomic::AtomicBool>);

impl Drop for ScheduleCancellation {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

mod phase_schedule;

fn seat_key(seat: &TransferSeat) -> SeatKey {
    (
        seat.presentation_id.clone(),
        seat.chain_root_id.clone(),
        seat.thread_id.clone(),
    )
}

fn transfer_touches_seat(
    transfer: &TransferIdentity,
    presentation: &str,
    root: &str,
    thread: &str,
) -> bool {
    [&transfer.source, &transfer.destination]
        .into_iter()
        .any(|seat| {
            seat.presentation_id == presentation
                && seat.chain_root_id == root
                && seat.thread_id == thread
        })
}

struct SeatGroup {
    // Only stable coordinates identify the group. The representative origin
    // is NOT substituted for any operation's full immutable TransferIdentity.
    seat: TransferSeat,
    requested: Vec<TransferIdentity>,
}

struct FixedTransferPlan {
    operations: BTreeMap<String, TransferIdentity>,
    groups: BTreeMap<SeatKey, SeatGroup>,
}

fn plan_fixed_transfer_cut(requested: &[TransferIdentity]) -> Result<FixedTransferPlan> {
    if requested.is_empty() {
        bail!("fixed transfer cut requires requested operations");
    }
    let mut operations: BTreeMap<String, TransferIdentity> = BTreeMap::new();
    for transfer in requested {
        TransferRecord {
            schema: TRANSFER_SCHEMA,
            transfer: transfer.clone(),
            phase: TransferPhase::InboundCommitment,
        }
        .validate_shape()
        .map_err(anyhow::Error::msg)?;
        if let Some(prior) = operations.get(&transfer.operation_id) {
            if prior != transfer {
                bail!("fixed transfer cut has conflicting operation identities");
            }
        } else {
            operations.insert(transfer.operation_id.clone(), transfer.clone());
        }
    }
    let mut groups: BTreeMap<SeatKey, SeatGroup> = BTreeMap::new();
    let mut presentation_seats: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut seat_presentations: BTreeMap<(String, String), String> = BTreeMap::new();
    for transfer in operations.values() {
        for seat in [&transfer.source, &transfer.destination] {
            let coordinates = (seat.chain_root_id.clone(), seat.thread_id.clone());
            if presentation_seats
                .get(&seat.presentation_id)
                .is_some_and(|prior| prior != &coordinates)
                || seat_presentations
                    .get(&coordinates)
                    .is_some_and(|prior| prior != &seat.presentation_id)
            {
                bail!("fixed transfer cut changed a stable presentation seat association");
            }
            presentation_seats.insert(seat.presentation_id.clone(), coordinates.clone());
            seat_presentations.insert(coordinates, seat.presentation_id.clone());
            groups
                .entry(seat_key(seat))
                .or_insert_with(|| SeatGroup {
                    seat: seat.clone(),
                    requested: Vec::new(),
                })
                .requested
                .push(transfer.clone());
        }
    }
    Ok(FixedTransferPlan { operations, groups })
}

struct CompletedSeatCut {
    proof: Arc<UiTransferFixedSeatProof>,
    membership: PresentationMembership,
}

/// Only a fully completed grouped cut reaches the caller. Extraction grants
/// no current membership, composition/event-time witness or writer authority;
/// those checks remain in the existing admission owner.
pub(crate) struct FixedTransferCut {
    historical: Option<CompletedHistoricalCut>,
    phase_schedule: Vec<phase_schedule::ScheduledTransferPhase>,
    cancellations: BTreeMap<String, CompletedCancellationCut>,
    operations: BTreeMap<
        String,
        (
            UiTransferFixedHistoryProof,
            PresentationMembership,
            PresentationMembership,
        ),
    >,
}

struct CompletedHistoricalCut {
    cancellation_frames: BTreeMap<String, ryeos_client_base::ui::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame>,
    proof: Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiHistoricalTransferCutProof>,
    memberships: BTreeMap<SeatKey, PresentationMembership>,
    endpoints: BTreeMap<
        String,
        ryeos_app::thread_lifecycle::presentation_transfer::UiTransferHistoryEndpoint,
    >,
}

struct CompletedCancellationCut {
    frame: ryeos_client_base::ui::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame,
    proof: Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof>,
    source_prefix: Arc<super::super::VerifiedTransferSeatPrefix>,
    source: PresentationMembership,
    destination: PresentationMembership,
}

/// Source-test admission metadata only; phase authority remains the completed
/// same-store signed app proof. This never installs browser session grants.
#[cfg(test)]
pub(crate) fn test_cut_from_completed_positive_proof(
    proof: UiTransferFixedHistoryProof,
    source: ryeos_client_base::ui::binding::UiPresentationInitialization,
    destination: ryeos_client_base::ui::binding::UiPresentationInitialization,
) -> Arc<FixedTransferCut> {
    let member = |initial: ryeos_client_base::ui::binding::UiPresentationInitialization,
                  seat: &TransferSeat| {
        assert_eq!(initial.presentation_id, seat.presentation_id);
        assert!(initial.surfaces.is_empty());
        let digest = initial.digest().unwrap();
        PresentationMembership {
            presentation_id: initial.presentation_id.clone(),
            creation_request_id: initial.creation_request_id.clone(),
            root_coordinate: crate::browser_session::BindingAttachmentCoordinate {
                binding_attachment_id: initial.root_binding.binding_attachment_id.clone(),
                binding_generation: initial.root_binding.binding_generation,
                binding_digest: initial.root_binding.binding_digest.clone(),
            },
            thread_id: Some(seat.thread_id.clone()),
            chain_root_id: Some(seat.chain_root_id.clone()),
            surfaces: Default::default(),
            activated_transfer_reservations: Default::default(),
            next_mount_id: initial.next_mount_id,
            initialization: initial,
            initialization_digest: digest,
        }
    };
    let transfer = proof.lifecycle().transfer().clone();
    let source = member(source, &transfer.source);
    let destination = member(destination, &transfer.destination);
    let phase_schedule = phase_schedule::schedule_cut([&proof], || Ok(())).unwrap();
    Arc::new(FixedTransferCut {
        historical: None,
        phase_schedule,
        cancellations: BTreeMap::new(),
        operations: BTreeMap::from([(transfer.operation_id, (proof, source, destination))]),
    })
}

impl FixedTransferCut {
    pub(crate) fn publication_cut(
        &self,
        state: &AppState,
        session: &BrowserSession,
    ) -> Result<(
        Vec<AuthoritativeThreadEventReadAnchor>,
        Vec<ryeos_state::RetiredSeatPositivePrefix>,
    )> {
        if let Some(dependencies) = self.historical_publication_dependencies(state, session)? {
            return Ok(dependencies);
        }
        Ok((self.publication_anchors(state, session)?, Vec::new()))
    }
    pub(crate) fn historical_publication_dependencies(
        &self,
        state: &AppState,
        session: &BrowserSession,
    ) -> Result<
        Option<(
            Vec<AuthoritativeThreadEventReadAnchor>,
            Vec<ryeos_state::RetiredSeatPositivePrefix>,
        )>,
    > {
        let Some(historical) = &self.historical else {
            return Ok(None);
        };
        if !historical.proof.is_for_store(state.state_store.as_ref()) {
            bail!("historical publication has another completed state owner");
        }
        for (key, membership) in &historical.memberships {
            if authorize_root(state, session, key)? != *membership {
                bail!("historical dependency changed its original authenticated membership");
            }
        }
        Ok(Some((
            historical.proof.current_anchors().to_vec(),
            historical.proof.retired_prefixes().to_vec(),
        )))
    }
    /// Historical dependency endpoints for observation wakes only. Rechecking
    /// these heads cannot publish phases or establish current pair authority.
    pub(crate) fn live_dependency_anchors(
        &self,
        state: &AppState,
        session: &BrowserSession,
    ) -> Result<Vec<AuthoritativeThreadEventReadAnchor>> {
        if let Some(historical) = &self.historical {
            for (key, membership) in &historical.memberships {
                let current = authorize_root(state, session, key)?;
                if current.root_coordinate != membership.root_coordinate
                    || current.initialization != membership.initialization
                {
                    bail!("historical observation dependency changed immutable membership");
                }
            }
            // Permanent retired identities cannot advance. Mutable heads are
            // wake targets only; publication retains the mixed full inventory.
            return Ok(historical.proof.current_anchors().to_vec());
        }
        let mut anchors = BTreeMap::new();
        let dependencies = self
            .operations
            .values()
            .flat_map(|(proof, source, destination)| {
                [
                    (source, proof.source_anchor()),
                    (destination, proof.destination_anchor()),
                ]
            })
            .chain(self.cancellations.values().flat_map(|cut| {
                [
                    (&cut.source, cut.proof.source_anchor()),
                    (&cut.destination, cut.proof.destination_anchor()),
                ]
            }));
        for (membership, anchor) in dependencies {
            let (_, current) = super::super::require_presentation_seat_for_read(
                state,
                session,
                &membership.presentation_id,
                Some(anchor.thread_id()),
                Some(anchor.chain_root_id()),
            )?;
            if current.root_coordinate != membership.root_coordinate
                || current.initialization != membership.initialization
            {
                bail!("live paired dependency changed its immutable membership");
            }
            let key = (
                anchor.chain_root_id().to_owned(),
                anchor.thread_id().to_owned(),
            );
            if anchors
                .insert(key, anchor.clone())
                .is_some_and(|prior| prior != *anchor)
            {
                bail!("live paired dependency has conflicting fixed endpoints");
            }
        }
        Ok(anchors.into_values().collect())
    }

    /// Every dependency in this completed cut must remain excluded through
    /// publication. Additional session-root cuts are allowed, but a caller
    /// cannot substitute a singleton root exclusion for paired authority.
    pub(crate) fn publication_cut_covered_by(
        &self,
        anchors: &[AuthoritativeThreadEventReadAnchor],
        retired: &[ryeos_state::RetiredSeatPositivePrefix],
    ) -> bool {
        let Some(historical) = &self.historical else {
            return self.publication_anchors_covered_by(anchors);
        };
        if historical.proof.identities().is_empty() {
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
        let mut markers = BTreeMap::new();
        for prefix in retired {
            if current.contains_key(&(prefix.chain_root_id(), prefix.chain_root_id())) {
                return false;
            }
            if markers.insert(prefix.chain_root_id(), prefix).is_some_and(
                |previous: &ryeos_state::RetiredSeatPositivePrefix| !previous.same_cut(prefix),
            ) {
                return false;
            }
        }
        historical.proof.current_anchors().iter().all(|required| {
            current
                .get(&(required.chain_root_id(), required.thread_id()))
                .is_some_and(|actual| *actual == required)
        }) && historical.proof.retired_prefixes().iter().all(|required| {
            markers
                .get(required.chain_root_id())
                .is_some_and(|actual| actual.same_cut(required))
        })
    }

    pub(crate) fn publication_anchors_covered_by(
        &self,
        anchors: &[AuthoritativeThreadEventReadAnchor],
    ) -> bool {
        if self.operations.is_empty() && self.cancellations.is_empty() {
            return false;
        }
        let mut indexed = BTreeMap::new();
        for anchor in anchors {
            let key = (anchor.chain_root_id(), anchor.thread_id());
            if indexed
                .insert(key, anchor)
                .is_some_and(|prior| prior != anchor)
            {
                return false;
            }
        }
        let covered = |required: &AuthoritativeThreadEventReadAnchor| {
            indexed
                .get(&(required.chain_root_id(), required.thread_id()))
                .is_some_and(|actual| *actual == required)
        };
        self.operations.values().all(|(proof, _, _)| {
            [proof.source_anchor(), proof.destination_anchor()]
                .into_iter()
                .all(covered)
        }) && self.cancellations.values().all(|cut| {
            [cut.proof.source_anchor(), cut.proof.destination_anchor()]
                .into_iter()
                .all(covered)
        })
    }

    /// Reauthorize the complete paired scope before taking the ordered-chain
    /// read exclusion. No history or cache read occurs in its callback.
    pub(crate) fn publication_anchors(
        &self,
        state: &AppState,
        session: &BrowserSession,
    ) -> Result<Vec<AuthoritativeThreadEventReadAnchor>> {
        if self.historical.is_some() {
            bail!("historical cut requires mixed current and retired publication exclusions");
        }
        let mut anchors = BTreeMap::new();
        for (proof, source, destination) in self.operations.values() {
            for (membership, anchor) in [
                (source, proof.source_anchor()),
                (destination, proof.destination_anchor()),
            ] {
                let (_, current) = super::super::require_presentation_seat_for_read(
                    state,
                    session,
                    &membership.presentation_id,
                    Some(anchor.thread_id()),
                    Some(anchor.chain_root_id()),
                )?;
                if &current != membership {
                    bail!("paired history membership changed before publication");
                }
                let key = (
                    anchor.chain_root_id().to_owned(),
                    anchor.thread_id().to_owned(),
                );
                if let Some(prior) = anchors.insert(key, anchor.clone()) {
                    if prior != *anchor {
                        bail!("paired history cut has conflicting seat anchors");
                    }
                }
            }
        }
        for cancellation in self.cancellations.values() {
            for (membership, anchor) in [
                (&cancellation.source, cancellation.proof.source_anchor()),
                (
                    &cancellation.destination,
                    cancellation.proof.destination_anchor(),
                ),
            ] {
                let (_, current) = super::super::require_presentation_seat_for_read(
                    state,
                    session,
                    &membership.presentation_id,
                    Some(anchor.thread_id()),
                    Some(anchor.chain_root_id()),
                )?;
                if &current != membership {
                    bail!("negative paired history membership changed before publication");
                }
                let key = (
                    anchor.chain_root_id().to_owned(),
                    anchor.thread_id().to_owned(),
                );
                if let Some(prior) = anchors.insert(key, anchor.clone()) {
                    if prior != *anchor {
                        bail!("negative paired history has conflicting exact anchors");
                    }
                }
            }
        }
        Ok(anchors.into_values().collect())
    }

    /// In-memory equality only, suitable while the completed cut's chain
    /// locks are held. The caller also holds the seat-transition exclusion.
    pub(crate) fn publication_memberships_match(&self, session: &BrowserSession) -> bool {
        if let Some(historical) = &self.historical {
            return historical.memberships.values().all(|membership| {
                session.presentations.get(&membership.presentation_id) == Some(membership)
            });
        }
        self.operations.values().all(|(_, source, destination)| {
            [source, destination].into_iter().all(|membership| {
                session.presentations.get(&membership.presentation_id) == Some(membership)
            })
        }) && self.cancellations.values().all(|cancellation| {
            [&cancellation.source, &cancellation.destination]
                .into_iter()
                .all(|membership| {
                    session.presentations.get(&membership.presentation_id) == Some(membership)
                })
        })
    }

    pub(crate) fn seat_cancellation_ids<'a>(
        &'a self,
        presentation_id: &'a str,
        chain_root_id: &'a str,
        thread_id: &'a str,
    ) -> impl Iterator<Item = &'a str> {
        self.cancellations
            .iter()
            .filter_map(move |(id, cut)| {
                transfer_touches_seat(
                    cut.proof.transfer(),
                    presentation_id,
                    chain_root_id,
                    thread_id,
                )
                .then_some(id.as_str())
            })
            .chain(self.historical.iter().flat_map(move |cut| {
                cut.proof
                    .identities()
                    .iter()
                    .filter_map(move |(id, transfer)| {
                        (cut.proof.cancellation(id).is_some()
                            && transfer_touches_seat(
                                transfer,
                                presentation_id,
                                chain_root_id,
                                thread_id,
                            ))
                        .then_some(id.as_str())
                    })
            }))
    }

    pub(crate) fn authenticated_cancellation_frame_at(
        &self,
        evidence: &ryeos_ui_presentation_contract::transfer::TransferEvidence,
    ) -> Option<&ryeos_client_base::ui::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame>{
        if let Some(historical) = &self.historical {
            historical
                .cancellation_frames
                .values()
                .find(|frame| &frame.cancellation_evidence == evidence)
        } else {
            self.cancellations
                .values()
                .find(|cut| &cut.frame.cancellation_evidence == evidence)
                .map(|cut| &cut.frame)
        }
    }

    pub(crate) fn cancellation_frame(
        &self,
        store: &ryeos_app::state_store::StateStore,
        operation_id: &str,
        max_bytes: u64,
    ) -> Result<ryeos_client_base::ui::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame>{
        if let Some(historical) = &self.historical {
            return historical_cancellation_proof_frame(store, historical, operation_id, max_bytes);
        }
        let cut = self
            .cancellations
            .get(operation_id)
            .ok_or_else(|| anyhow!("completed cut has no retained cancellation"))?;
        cancellation_proof_frame(store, &cut.proof, &cut.source_prefix, max_bytes)
    }

    /// Disclosure selects only operations on the requested seat. The full
    /// causal closure remains owned here and is still checked at publication.
    pub(crate) fn seat_operation_ids<'a>(
        &'a self,
        presentation_id: &'a str,
        chain_root_id: &'a str,
        thread_id: &'a str,
    ) -> impl Iterator<Item = &'a str> {
        self.operations
            .iter()
            .filter_map(move |(id, (proof, _, _))| {
                let transfer = proof.lifecycle().transfer();
                transfer_touches_seat(transfer, presentation_id, chain_root_id, thread_id)
                    .then_some(id.as_str())
            })
            .chain(self.historical.iter().flat_map(move |cut| {
                cut.proof
                    .identities()
                    .iter()
                    .filter_map(move |(id, transfer)| {
                        (cut.proof.cancellation(id).is_none()
                            && transfer_touches_seat(
                                transfer,
                                presentation_id,
                                chain_root_id,
                                thread_id,
                            ))
                        .then_some(id.as_str())
                    })
            }))
    }

    /// Extract one bounded operation only from this completed opaque app cut.
    /// Tip objects are looked up by the original signed anchor, never recaptured.
    /// This data frame does not publish the candidate or grant read/write access.
    pub(crate) fn proof_frame(
        &self,
        store: &ryeos_app::state_store::StateStore,
        operation_id: &str,
        max_bytes: u64,
    ) -> Result<ryeos_client_base::ui::transfer_proof_frame::UiTransferProofFrame> {
        use ryeos_client_base::ui::transfer_proof_frame::{
            UiTransferProofAnchor, UiTransferProofFrame, UiTransferProofPhase,
        };
        let lifecycle = if let Some(historical) = &self.historical {
            if !historical.proof.is_for_store(store) {
                bail!("historical frame has another state owner");
            }
            historical
                .proof
                .lifecycle(operation_id)
                .ok_or_else(|| anyhow!("historical cut has no requested lifecycle"))?
        } else {
            let proof = &self
                .operations
                .get(operation_id)
                .ok_or_else(|| anyhow!("completed cut has no requested operation"))?
                .0;
            if !proof.is_for_store(store) {
                bail!("transfer frame has another state owner");
            }
            proof.lifecycle()
        };
        let transfer = lifecycle.transfer();
        let extract_anchor = |seat: &TransferSeat,
                              anchor: &AuthoritativeThreadEventReadAnchor|
         -> Result<UiTransferProofAnchor> {
            if anchor.chain_root_id() != seat.chain_root_id || anchor.thread_id() != seat.thread_id
            {
                bail!("completed transfer proof has another anchored seat");
            }
            let (hash, thread_seq) = anchor
                .tip()
                .ok_or_else(|| anyhow!("completed transfer proof has no signed tip"))?;
            let event = store
                .retained_thread_event_by_hash(hash)?
                .ok_or_else(|| anyhow!("original transfer cut tip object is absent"))?;
            if event.chain_root_id != seat.chain_root_id
                || event.thread_id != seat.thread_id
                || event.thread_seq != thread_seq
                || ryeos_state::objects::thread_event::hash_event(&event)? != hash
            {
                bail!("transfer frame tip object differs from original exact signed anchor");
            }
            Ok(UiTransferProofAnchor {
                presentation_id: seat.presentation_id.clone(),
                chain_root_id: seat.chain_root_id.clone(),
                thread_id: seat.thread_id.clone(),
                event_hash: hash.into(),
                thread_seq,
                chain_seq: event.chain_seq,
            })
        };
        let mut phases = Vec::new();
        for testimony in [
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
        {
            phases.push(UiTransferProofPhase {
                evidence: testimony.evidence.clone(),
                record: testimony.record.clone(),
            });
        }
        let extract_endpoint = |seat: &TransferSeat| -> Result<UiTransferProofAnchor> {
            if let Some(historical) = &self.historical {
                use ryeos_app::thread_lifecycle::presentation_transfer::UiTransferHistoryEndpoint;
                match historical
                    .endpoints
                    .get(&seat.chain_root_id)
                    .ok_or_else(|| anyhow!("historical frame endpoint missing"))?
                {
                    UiTransferHistoryEndpoint::Current(anchor) => extract_anchor(seat, anchor),
                    UiTransferHistoryEndpoint::Retired(prefix) => {
                        let (hash, thread_seq, chain_seq) = prefix
                            .tip()
                            .ok_or_else(|| anyhow!("retired frame has no exact tip"))?;
                        let event = store
                            .retained_thread_event_by_hash(hash)?
                            .ok_or_else(|| anyhow!("retired frame tip object absent"))?;
                        if prefix.chain_root_id() != seat.chain_root_id
                            || event.chain_root_id != seat.chain_root_id
                            || event.thread_id != seat.thread_id
                            || event.thread_seq != thread_seq
                            || event.chain_seq != chain_seq
                            || ryeos_state::objects::thread_event::hash_event(&event)? != hash
                        {
                            bail!("retired frame tip differs from marker-bound native endpoint");
                        }
                        Ok(UiTransferProofAnchor {
                            presentation_id: seat.presentation_id.clone(),
                            chain_root_id: seat.chain_root_id.clone(),
                            thread_id: seat.thread_id.clone(),
                            event_hash: hash.into(),
                            thread_seq,
                            chain_seq,
                        })
                    }
                }
            } else {
                let proof = &self.operations.get(operation_id).expect("selected above").0;
                extract_anchor(
                    seat,
                    if seat == &transfer.source {
                        proof.source_anchor()
                    } else {
                        proof.destination_anchor()
                    },
                )
            }
        };
        let frame = UiTransferProofFrame {
            schema: 1,
            transfer: transfer.clone(),
            source_anchor: extract_endpoint(&transfer.source)?,
            destination_anchor: extract_endpoint(&transfer.destination)?,
            phases,
        };
        frame.validate(max_bytes).map_err(anyhow::Error::msg)?;
        Ok(frame)
    }

    pub(crate) fn covers_retired_seat_prefix(
        &self,
        presentation_id: &str,
        chain_root_id: &str,
        prefix: &ryeos_state::RetiredSeatPositivePrefix,
    ) -> bool {
        self.historical.as_ref().is_some_and(|historical| {
            prefix.chain_root_id() == chain_root_id
                && historical.proof.identities().values().any(|transfer| {
                    transfer_touches_seat(transfer, presentation_id, chain_root_id, chain_root_id)
                })
                && historical
                    .proof
                    .retired_prefixes()
                    .iter()
                    .any(|expected| expected.same_cut(prefix))
        })
    }

    pub(crate) fn covers_seat_anchor(
        &self,
        presentation_id: &str,
        chain_root_id: &str,
        thread_id: &str,
        anchor: &AuthoritativeThreadEventReadAnchor,
    ) -> bool {
        if let Some(historical) = &self.historical {
            return anchor.chain_root_id() == chain_root_id
                && anchor.thread_id() == thread_id
                && historical.proof.identities().values().any(|transfer| {
                    transfer_touches_seat(transfer, presentation_id, chain_root_id, thread_id)
                })
                && historical
                    .proof
                    .current_anchors()
                    .iter()
                    .any(|expected| expected == anchor);
        }
        let mut seen = false;
        for (proof, _, _) in self.operations.values() {
            let transfer = proof.lifecycle().transfer();
            for (seat, proven_anchor) in [
                (&transfer.source, proof.source_anchor()),
                (&transfer.destination, proof.destination_anchor()),
            ] {
                if seat.presentation_id == presentation_id
                    && seat.chain_root_id == chain_root_id
                    && seat.thread_id == thread_id
                {
                    seen = true;
                    if proven_anchor != anchor {
                        return false;
                    }
                }
            }
        }
        for cut in self.cancellations.values() {
            let transfer = cut.proof.transfer();
            for (seat, proven_anchor) in [
                (&transfer.source, cut.proof.source_anchor()),
                (&transfer.destination, cut.proof.destination_anchor()),
            ] {
                if seat.presentation_id == presentation_id
                    && seat.chain_root_id == chain_root_id
                    && seat.thread_id == thread_id
                {
                    seen = true;
                    if proven_anchor != anchor {
                        return false;
                    }
                }
            }
        }
        seen
    }

    pub(crate) fn completed_root_positions(
        &self,
        presentation_id: &str,
        chain_root_id: &str,
        thread_id: &str,
        seen: &BTreeMap<String, u64>,
    ) -> bool {
        let mut required = BTreeMap::new();
        for phase in &self.phase_schedule {
            let owner = phase.record.event_owner();
            if owner.presentation_id == presentation_id
                && owner.chain_root_id == chain_root_id
                && owner.thread_id == thread_id
            {
                required.insert(phase.evidence.event_hash.clone(), phase.evidence.chain_seq);
            }
        }
        for cut in self.cancellations.values() {
            let source = &cut.proof.transfer().source;
            if source.presentation_id == presentation_id
                && source.chain_root_id == chain_root_id
                && source.thread_id == thread_id
            {
                let Some((evidence, _)) = cut.proof.retained_cancellation() else {
                    return false;
                };
                required.insert(evidence.event_hash.clone(), evidence.chain_seq);
            }
        }
        if let Some(historical) = &self.historical {
            for transfer in historical.proof.identities().values() {
                if transfer.source.presentation_id == presentation_id
                    && transfer.source.chain_root_id == chain_root_id
                    && transfer.source.thread_id == thread_id
                {
                    if let Some((evidence, _)) =
                        historical.proof.cancellation(&transfer.operation_id)
                    {
                        required.insert(evidence.event_hash.clone(), evidence.chain_seq);
                    }
                }
            }
        }
        &required == seen
    }

    pub(crate) fn authenticated_phase_at(
        &self,
        evidence: &ryeos_ui_presentation_contract::transfer::TransferEvidence,
    ) -> Option<(
        &TransferRecord,
        &ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
    )> {
        let scheduled = self
            .phase_schedule
            .iter()
            .find(|phase| &phase.evidence == evidence)?;
        let lifecycle = if let Some(historical) = &self.historical {
            historical
                .proof
                .lifecycle(&scheduled.record.transfer.operation_id)?
        } else {
            self.operations
                .get(&scheduled.record.transfer.operation_id)?
                .0
                .lifecycle()
        };
        let matching = [
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
        .any(|testimony| testimony.evidence == evidence && testimony.record == &scheduled.record);
        matching.then_some((&scheduled.record, lifecycle))
    }

    /// Visit only phases obtained from this completed, same-store paired cut.
    /// The callback is a private projection consumer, not a writer or a
    /// browser-visible history stream. Keep the proof borrowed for the whole
    /// visit so a caller cannot substitute a record from another cut.
    pub(crate) fn for_each_authenticated_phase(
        &self,
        mut visit: impl FnMut(
            &ryeos_ui_presentation_contract::transfer::TransferEvidence,
            &TransferRecord,
            &ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
        ) -> Result<()>,
    ) -> Result<()> {
        for phase in &self.phase_schedule {
            let Some((record, proof)) = self.authenticated_phase_at(&phase.evidence) else {
                bail!("scheduled phase differs from completed paired testimony");
            };
            visit(&phase.evidence, record, proof)?;
        }
        Ok(())
    }

    pub(super) fn take_operation(
        &mut self,
        operation_id: &str,
    ) -> Result<(
        UiTransferFixedHistoryProof,
        PresentationMembership,
        PresentationMembership,
    )> {
        self.operations
            .remove(operation_id)
            .ok_or_else(|| anyhow!("fixed transfer cut has no remaining requested operation"))
    }
}

/// Extract only an already-retained negative outcome from the app owner's
/// completed same-store paired absence proof and the fixed source reader's
/// receipt-attributed ordinary witnesses. This is transport data, never a
/// transfer phase or an independently reusable absence grant.
fn historical_cancellation_proof_frame(
    store: &ryeos_app::state_store::StateStore,
    cut: &CompletedHistoricalCut,
    operation_id: &str,
    max_bytes: u64,
) -> Result<
    ryeos_client_base::ui::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame,
> {
    use ryeos_app::thread_lifecycle::presentation_transfer::UiTransferHistoryEndpoint;
    use ryeos_client_base::ui::{
        transfer_cancellation_proof_frame::{
            UiMoveFreezeCancellationProofFrame, UiReceiptedFreezeWitness,
        },
        transfer_proof_frame::{UiTransferProofAnchor, UiTransferProofPhase},
    };
    if !cut.proof.is_for_store(store) {
        bail!("historical cancellation has another state owner");
    }
    let (evidence, cancellation) = cut
        .proof
        .cancellation(operation_id)
        .ok_or_else(|| anyhow!("historical negative outcome absent"))?;
    let transfer = &cancellation.transfer;
    let anchor = |seat: &TransferSeat| -> Result<UiTransferProofAnchor> {
        let endpoint = cut
            .endpoints
            .get(&seat.chain_root_id)
            .ok_or_else(|| anyhow!("historical cancellation endpoint missing"))?;
        let (hash, thread_seq, expected_chain_seq) = match endpoint {
            UiTransferHistoryEndpoint::Current(anchor) => {
                if anchor.chain_root_id() != seat.chain_root_id
                    || anchor.thread_id() != seat.thread_id
                {
                    bail!("negative current endpoint differs");
                }
                let (hash, seq) = anchor
                    .tip()
                    .ok_or_else(|| anyhow!("negative current endpoint empty"))?;
                (hash, seq, None)
            }
            UiTransferHistoryEndpoint::Retired(prefix) => {
                if prefix.chain_root_id() != seat.chain_root_id {
                    bail!("negative retired endpoint differs");
                }
                let (hash, seq, chain) = prefix
                    .tip()
                    .ok_or_else(|| anyhow!("negative retired endpoint empty"))?;
                (hash, seq, Some(chain))
            }
        };
        let event = store
            .retained_thread_event_by_hash(hash)?
            .ok_or_else(|| anyhow!("negative historical tip absent"))?;
        if event.chain_root_id != seat.chain_root_id
            || event.thread_id != seat.thread_id
            || event.thread_seq != thread_seq
            || expected_chain_seq.is_some_and(|seq| seq != event.chain_seq)
            || ryeos_state::objects::thread_event::hash_event(&event)? != hash
        {
            bail!("negative historical tip differs");
        }
        Ok(UiTransferProofAnchor {
            presentation_id: seat.presentation_id.clone(),
            chain_root_id: seat.chain_root_id.clone(),
            thread_id: seat.thread_id.clone(),
            event_hash: hash.into(),
            thread_seq,
            chain_seq: event.chain_seq,
        })
    };
    let witness = |evidence: &ryeos_ui_presentation_contract::transfer::TransferEvidence| -> Result<UiReceiptedFreezeWitness> {
        let found = cut.proof.freeze_witness(evidence).ok_or_else(|| anyhow!("historical freeze receipt witness missing"))?;
        let mut row = found.event.to_value();
        row["event_hash"] = serde_json::json!(found.evidence.event_hash);
        let retained = ryeos_client_base::ui::seat::retained_seat_event_with_evidence(&row)
            .map_err(anyhow::Error::msg)?.ok_or_else(|| anyhow!("historical freeze witness is not an ordinary typed Core event"))?;
        if retained.evidence != found.evidence || retained.chain_root_id != transfer.source.chain_root_id
            || retained.thread_id != transfer.source.thread_id { bail!("historical freeze witness changes exact source"); }
        Ok(UiReceiptedFreezeWitness { evidence: found.evidence.clone(), event: retained.event,
            receipt_evidence: found.receipt_evidence.clone(), receipt_operation_id: found.receipt.operation_id.clone(),
            receipt_producer_incarnation: found.receipt.producer_incarnation.clone(),
            receipt_first_engine_seq: found.receipt.first_engine_seq, receipt_last_engine_seq: found.receipt.last_engine_seq })
    };
    let lifecycle = cut.proof.lifecycle(operation_id);
    let phase = |testimony: Option<
        ryeos_ui_presentation_contract::transfer_proof::TransferTestimony<'_>,
    >| {
        testimony.map(|value| UiTransferProofPhase {
            evidence: value.evidence.clone(),
            record: value.record.clone(),
        })
    };
    let frame = UiMoveFreezeCancellationProofFrame {
        schema: 1,
        transfer: transfer.clone(),
        source_anchor: anchor(&transfer.source)?,
        destination_anchor: anchor(&transfer.destination)?,
        cancellation_evidence: evidence.clone(),
        cancellation: cancellation.clone(),
        destination_inbound: phase(lifecycle.map(|state| state.inbound())),
        destination_orphan_cancelled: phase(lifecycle.and_then(|state| state.orphan_cancelled())),
        preparation: witness(&cancellation.freeze.preparation)?,
        completion: cancellation
            .freeze
            .completion
            .as_ref()
            .map(witness)
            .transpose()?,
    };
    frame.validate(max_bytes).map_err(anyhow::Error::msg)?;
    Ok(frame)
}

pub(crate) fn cancellation_proof_frame(
    store: &ryeos_app::state_store::StateStore,
    proof: &ryeos_app::thread_lifecycle::presentation_transfer::UiMoveFreezeCancellationProof,
    source: &super::super::VerifiedTransferSeatPrefix,
    max_bytes: u64,
) -> Result<
    ryeos_client_base::ui::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame,
> {
    use ryeos_client_base::ui::{
        transfer_cancellation_proof_frame::{
            UiMoveFreezeCancellationProofFrame, UiReceiptedFreezeWitness,
        },
        transfer_proof_frame::{UiTransferProofAnchor, UiTransferProofPhase},
    };
    if !proof.belongs_to(store) || source.anchor() != proof.source_anchor() {
        bail!("cancellation frame has another completed source owner");
    }
    let (cancellation_evidence, cancellation) = proof
        .retained_cancellation()
        .ok_or_else(|| anyhow!("cancellation frame has no retained negative outcome"))?;
    let witness = |evidence: &ryeos_ui_presentation_contract::transfer::TransferEvidence| -> Result<UiReceiptedFreezeWitness> {
        let found = source
            .freeze_event(evidence)
            .ok_or_else(|| anyhow!("cancellation frame lacks its exact receipted freeze witness"))?;
        Ok(UiReceiptedFreezeWitness {
            evidence: found.evidence.clone(),
            event: found.event.clone(),
            receipt_evidence: found.receipt_evidence.clone(),
            receipt_operation_id: found.receipt_operation_id.clone(),
            receipt_producer_incarnation: found.receipt_producer_incarnation.clone(),
            receipt_first_engine_seq: found.receipt_first_engine_seq,
            receipt_last_engine_seq: found.receipt_last_engine_seq,
        })
    };
    let anchor = |seat: &TransferSeat,
                  cut: &AuthoritativeThreadEventReadAnchor|
     -> Result<UiTransferProofAnchor> {
        if cut.chain_root_id() != seat.chain_root_id || cut.thread_id() != seat.thread_id {
            bail!("cancellation frame changed exact paired seat");
        }
        let (hash, thread_seq) = cut
            .tip()
            .ok_or_else(|| anyhow!("cancellation cut has no signed tip"))?;
        let event = store
            .retained_thread_event_by_hash(hash)?
            .ok_or_else(|| anyhow!("cancellation tip CAS object is absent"))?;
        if event.chain_root_id != seat.chain_root_id
            || event.thread_id != seat.thread_id
            || event.thread_seq != thread_seq
            || ryeos_state::objects::thread_event::hash_event(&event)? != hash
        {
            bail!("cancellation tip differs from exact signed anchor");
        }
        Ok(UiTransferProofAnchor {
            presentation_id: seat.presentation_id.clone(),
            chain_root_id: seat.chain_root_id.clone(),
            thread_id: seat.thread_id.clone(),
            event_hash: hash.into(),
            thread_seq,
            chain_seq: event.chain_seq,
        })
    };
    let frame = UiMoveFreezeCancellationProofFrame {
        schema: 1,
        transfer: proof.transfer().clone(),
        source_anchor: anchor(&proof.transfer().source, proof.source_anchor())?,
        destination_anchor: anchor(&proof.transfer().destination, proof.destination_anchor())?,
        cancellation_evidence: cancellation_evidence.clone(),
        cancellation: cancellation.clone(),
        destination_inbound: proof.destination_inbound().map(|(evidence, record)| {
            UiTransferProofPhase {
                evidence: evidence.clone(),
                record: record.clone(),
            }
        }),
        destination_orphan_cancelled: proof.destination_orphan_cancelled().map(
            |(evidence, record)| UiTransferProofPhase {
                evidence: evidence.clone(),
                record: record.clone(),
            },
        ),
        preparation: witness(&proof.freeze().preparation)?,
        completion: proof
            .freeze()
            .completion
            .as_ref()
            .map(witness)
            .transpose()?,
    };
    frame.validate(max_bytes).map_err(anyhow::Error::msg)?;
    Ok(frame)
}

pub(super) async fn read_fixed_transfer_cut(
    state: Arc<AppState>,
    session: &BrowserSession,
    requested: &[TransferIdentity],
) -> Result<FixedTransferCut> {
    let plan = plan_fixed_transfer_cut(requested)?;
    read_grouped_plan(state, session, plan, BTreeMap::new()).await
}

struct SuppliedRootCut {
    key: SeatKey,
    membership: PresentationMembership,
    anchor: AuthoritativeThreadEventReadAnchor,
}

fn validate_supplied_group(
    plan: &FixedTransferPlan,
    key: &SeatKey,
    anchor_root: &str,
    anchor_thread: &str,
) -> Result<()> {
    if !plan.groups.contains_key(key) || key.1 != anchor_root || key.2 != anchor_thread {
        bail!("discovered transfer cut changed its supplied root cut");
    }
    Ok(())
}

async fn read_grouped_plan(
    state: Arc<AppState>,
    session: &BrowserSession,
    plan: FixedTransferPlan,
    supplied: BTreeMap<SeatKey, SuppliedRootCut>,
) -> Result<FixedTransferCut> {
    for (key, root) in &supplied {
        if key != &root.key {
            bail!("discovered transfer cut changed supplied seat key");
        }
        validate_supplied_group(
            &plan,
            &root.key,
            root.anchor.chain_root_id(),
            root.anchor.thread_id(),
        )?;
    }
    let ui = get_ui_state(&state).ok_or_else(|| anyhow!("UiState not set"))?;
    let mut memberships = BTreeMap::new();
    // After root discovery, no grouped CAS or cache access until ALL discovered
    // session associations pass. Explicit-set callers have performed no CAS.
    for (key, group) in &plan.groups {
        let membership = ui
            .browser_sessions
            .require_presentation(
                &session.session_id,
                &group.seat.presentation_id,
                Some(&group.seat.thread_id),
                Some(&group.seat.chain_root_id),
                state.node_policy.generation_digest(),
            )
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
        let membership = if let Some(root) = supplied.get(key) {
            if root.membership.initialization != membership.initialization {
                bail!("discovered transfer root initialization changed");
            }
            root.membership.clone()
        } else {
            membership
        };
        memberships.insert(key.clone(), membership);
    }
    let mut anchors = BTreeMap::new();
    for (_, root) in supplied {
        anchors.insert(root.key, root.anchor);
    }
    // Capture ALL cuts before the first cache reuse or history read. One
    // capture per blocking task keeps the asynchronous work slices explicit.
    for (key, group) in &plan.groups {
        if anchors.contains_key(key) {
            continue;
        }
        let store = state.state_store.clone();
        let seat = group.seat.clone();
        let anchor = tokio::task::spawn_blocking(move || {
            store
                .capture_thread_event_read_anchor(&seat.chain_root_id, &seat.thread_id)?
                .ok_or_else(|| anyhow!("fixed transfer cut signed seat head is absent"))
        })
        .await??;
        anchors.insert(key.clone(), anchor);
    }
    let mut completed: BTreeMap<SeatKey, CompletedSeatCut> = BTreeMap::new();
    // Only one seat decoder is active at a time. Each gets all its frozen
    // requested identities, rather than rescanning it once per operation.
    for (key, group) in plan.groups {
        let membership = memberships
            .remove(&key)
            .ok_or_else(|| anyhow!("fixed transfer cut lost authorized membership"))?;
        let anchor = anchors
            .remove(&key)
            .ok_or_else(|| anyhow!("fixed transfer cut lost its captured anchor"))?;
        let proof = super::read_fixed_transfer_seat(
            state.clone(),
            group.seat,
            anchor,
            &membership.initialization,
            group.requested,
        )
        .await?;
        completed.insert(key, CompletedSeatCut { proof, membership });
    }
    let mut operations = BTreeMap::new();
    let mut cancellations = BTreeMap::new();
    for (operation_id, transfer) in plan.operations {
        let source = completed
            .get(&seat_key(&transfer.source))
            .ok_or_else(|| anyhow!("fixed transfer cut source is incomplete"))?;
        let destination = completed
            .get(&seat_key(&transfer.destination))
            .ok_or_else(|| anyhow!("fixed transfer cut destination is incomplete"))?;
        let source_proof = source.proof.clone();
        let destination_proof = destination.proof.clone();
        // Classify only through the completed exact source proof. A retained
        // cancellation must not be hidden by destination-only inbound phases.
        if source_proof.selected_retained_cancellation(state.state_store.as_ref(), &transfer)? {
            let negative = tokio::task::spawn_blocking({
                let transfer = transfer.clone();
                let source_proof = source_proof.clone();
                let destination_proof = destination_proof.clone();
                move || ryeos_app::thread_lifecycle::presentation_transfer::assemble_move_freeze_cancellation_proof(
                    &source_proof, &destination_proof, &transfer,
                )
            }).await??;
            if negative.retained_cancellation().is_none() {
                bail!("classified cancellation has no exact retained negative testimony");
            }
            let negative = Arc::new(negative);
            let digest = super::transfer_checkpoint_key(
                &transfer.source,
                &source.membership.initialization,
            )?
            .initialization_digest;
            let read_state = state.clone();
            let read_proof = negative.clone();
            let seat = transfer.source.clone();
            let read = tokio::task::spawn_blocking(move || {
                super::super::FixedAnchorReplay::begin_move_freeze_cancellation_admission(
                    &read_state,
                    &seat,
                    &digest,
                    read_proof,
                )
            })
            .await??;
            let prefix = super::finish_transfer_prefix(state.clone(), read).await?;
            let frame = cancellation_proof_frame(
                state.state_store.as_ref(),
                &negative,
                &prefix,
                4 * 1024 * 1024,
            )?;
            if negative.destination_inbound().is_some() {
                // Retain the real destination InboundCommitment and optional
                // exact linked OrphanCancelled in the positive schedule. The
                // negative app proof excluded every source phase and later
                // destination obligation at this completed paired cut.
                let positive = tokio::task::spawn_blocking({
                    let transfer = transfer.clone();
                    let source_proof = source_proof.clone();
                    let destination_proof = destination_proof.clone();
                    move || {
                        assemble_fixed_history_proof(&source_proof, &destination_proof, &transfer)
                    }
                })
                .await??;
                operations.insert(
                    operation_id.clone(),
                    (
                        positive,
                        source.membership.clone(),
                        destination.membership.clone(),
                    ),
                );
            }
            cancellations.insert(
                operation_id,
                CompletedCancellationCut {
                    frame,
                    proof: negative,
                    source_prefix: prefix,
                    source: source.membership.clone(),
                    destination: destination.membership.clone(),
                },
            );
            continue;
        }
        // Positive topology folding is bounded by one operation's slots.
        let proof = tokio::task::spawn_blocking(move || {
            assemble_fixed_history_proof(&source_proof, &destination_proof, &transfer)
        })
        .await??;
        operations.insert(
            operation_id,
            (
                proof,
                source.membership.clone(),
                destination.membership.clone(),
            ),
        );
    }
    // Aggregate graph work must not occupy the async executor. A disconnected
    // caller drops this guard, requesting cooperative cancellation of pure work.
    let cancellation = ScheduleCancellation(Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let cancelled = cancellation.0.clone();
    let result = tokio::task::spawn_blocking(move || {
        let phase_schedule =
            phase_schedule::schedule_cut(operations.values().map(|(proof, _, _)| proof), || {
                if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    bail!("fixed transfer schedule reader cancelled");
                }
                Ok(())
            })?;
        Ok(FixedTransferCut {
            historical: None,
            operations,
            cancellations,
            phase_schedule,
        })
    })
    .await?;
    drop(cancellation);
    result
}

/// Fixed-point positive dependency discovery at the captured seat cuts. The
/// internal operation superset is not browser publication or stream completion.
pub(crate) enum DiscoveredRootTransferCut {
    Empty {
        membership: PresentationMembership,
        anchor: AuthoritativeThreadEventReadAnchor,
    },
    Transfers {
        membership: PresentationMembership,
        anchor: AuthoritativeThreadEventReadAnchor,
        requested: Vec<TransferIdentity>,
        cut: FixedTransferCut,
    },
}

fn collect_root_identity(
    root: &SeatKey,
    operations: &mut BTreeMap<String, TransferIdentity>,
    transfer: TransferIdentity,
) -> Result<()> {
    TransferRecord {
        schema: TRANSFER_SCHEMA,
        transfer: transfer.clone(),
        phase: TransferPhase::InboundCommitment,
    }
    .validate_shape()
    .map_err(anyhow::Error::msg)?;
    let owner = [&transfer.source, &transfer.destination]
        .into_iter()
        .find(|seat| seat.chain_root_id == root.1 && seat.thread_id == root.2)
        .ok_or_else(|| anyhow!("discovered transfer does not belong to root"))?;
    if owner.presentation_id != root.0 {
        bail!("discovered transfer changed root presentation ownership");
    }
    if let Some(prior) = operations.get(&transfer.operation_id) {
        if prior != &transfer {
            bail!("discovered transfer has conflicting full operation identities");
        }
    } else {
        operations.insert(transfer.operation_id.clone(), transfer);
    }
    Ok(())
}

#[derive(Default)]
struct DependencyDiscovery {
    operations: BTreeMap<String, TransferIdentity>,
    presentation_seats: BTreeMap<String, (String, String)>,
    seat_presentations: BTreeMap<(String, String), String>,
    queued: BTreeSet<SeatKey>,
    work: VecDeque<SeatKey>,
}

impl DependencyDiscovery {
    fn queue_seat(&mut self, key: SeatKey) -> Result<()> {
        let coordinates = (key.1.clone(), key.2.clone());
        if key.1 != key.2
            || self
                .presentation_seats
                .get(&key.0)
                .is_some_and(|prior| prior != &coordinates)
            || self
                .seat_presentations
                .get(&coordinates)
                .is_some_and(|prior| prior != &key.0)
        {
            bail!("transfer dependency discovery changed a stable seat association");
        }
        self.presentation_seats
            .insert(key.0.clone(), coordinates.clone());
        self.seat_presentations.insert(coordinates, key.0.clone());
        if self.queued.insert(key.clone()) {
            self.work.push_back(key);
        }
        Ok(())
    }

    fn collect(&mut self, owner: &SeatKey, transfer: TransferIdentity) -> Result<()> {
        // Full shape, exact page ownership and operation conflicts are checked
        // before adding the endpoints to the attempt-local dependency worklist.
        let source = seat_key(&transfer.source);
        let destination = seat_key(&transfer.destination);
        collect_root_identity(owner, &mut self.operations, transfer)?;
        self.queue_seat(source)?;
        self.queue_seat(destination)?;
        Ok(())
    }
}

fn authorize_root(
    state: &AppState,
    session: &BrowserSession,
    key: &SeatKey,
) -> Result<PresentationMembership> {
    if key.1 != key.2 {
        bail!("transfer discovery requires exact root thread coordinates");
    }
    let ui = get_ui_state(state).ok_or_else(|| anyhow!("UiState not set"))?;
    Ok(ui
        .browser_sessions
        .require_presentation(
            &session.session_id,
            &key.0,
            Some(&key.2),
            Some(&key.1),
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?)
}

/// The replay handoff can supply its exact original cut; authorization still
/// precedes the first CAS access, and this path never recaptures that root.
pub(super) async fn read_root_transfer_cut_at_anchor(
    state: Arc<AppState>,
    session: &BrowserSession,
    presentation_id: &str,
    chain_root_id: &str,
    thread_id: &str,
    anchor: AuthoritativeThreadEventReadAnchor,
) -> Result<DiscoveredRootTransferCut> {
    let key = (
        presentation_id.to_owned(),
        chain_root_id.to_owned(),
        thread_id.to_owned(),
    );
    let membership = authorize_root(&state, session, &key)?;
    if anchor.chain_root_id() != key.1 || anchor.thread_id() != key.2 {
        bail!("transfer discovery supplied anchor changed root coordinates");
    }
    discover_root_cut(
        state,
        session,
        SuppliedRootCut {
            key,
            membership,
            anchor,
        },
    )
    .await
}

async fn read_historical_plan(
    state: Arc<AppState>,
    session: &BrowserSession,
    local_endpoint: &ryeos_app::thread_lifecycle::presentation_transfer::UiTransferHistoryEndpoint,
    plan: FixedTransferPlan,
    current: BTreeMap<SeatKey, SuppliedRootCut>,
    retired: BTreeMap<
        SeatKey,
        (
            PresentationMembership,
            ryeos_state::RetiredSeatPositivePrefix,
        ),
    >,
) -> Result<FixedTransferCut> {
    use ryeos_app::thread_lifecycle::presentation_transfer::{
        UiTransferHistoryEndpoint, begin_historical_transfer_cut_read_at_endpoint,
        finish_historical_transfer_cut_read, read_historical_transfer_cut_page,
    };
    let mut memberships = BTreeMap::new();
    let mut endpoints = BTreeMap::new();
    for (key, cut) in current {
        if authorize_root(&state, session, &key)? != cut.membership {
            bail!("historical discovery current membership changed");
        }
        endpoints.insert(
            key.1.clone(),
            UiTransferHistoryEndpoint::Current(cut.anchor),
        );
        memberships.insert(key, cut.membership);
    }
    for (key, (membership, prefix)) in retired {
        if authorize_root(&state, session, &key)? != membership {
            bail!("historical discovery retired association changed");
        }
        if endpoints
            .insert(key.1.clone(), UiTransferHistoryEndpoint::Retired(prefix))
            .is_some()
        {
            bail!("historical discovery mixed endpoint collision");
        }
        memberships.insert(key, membership);
    }
    let requested: Vec<_> = plan.operations.into_values().collect();
    let mut read = begin_historical_transfer_cut_read_at_endpoint(
        state.state_store.clone(),
        local_endpoint,
        &requested,
        endpoints.clone(),
    )?;
    loop {
        let (returned, complete) = tokio::task::spawn_blocking(move || {
            let complete = read_historical_transfer_cut_page(&mut read);
            (read, complete)
        })
        .await?;
        read = returned;
        if complete? {
            break;
        }
    }
    let proof = Arc::new(
        tokio::task::spawn_blocking(move || finish_historical_transfer_cut_read(read)).await??,
    );
    completed_historical_cut(state.state_store.as_ref(), proof, memberships, endpoints)
}

fn completed_historical_cut(
    store: &ryeos_app::state_store::StateStore,
    proof: Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiHistoricalTransferCutProof>,
    memberships: BTreeMap<SeatKey, PresentationMembership>,
    endpoints: BTreeMap<
        String,
        ryeos_app::thread_lifecycle::presentation_transfer::UiTransferHistoryEndpoint,
    >,
) -> Result<FixedTransferCut> {
    if !proof.is_for_store(store) {
        bail!("historical cut has another state owner");
    }
    for (key, membership) in &memberships {
        if !proof.initialization_matches(&key.1, &serde_json::to_value(&membership.initialization)?)
        {
            bail!("historical endpoint differs from original authenticated initialization");
        }
    }
    let phase_schedule = phase_schedule::schedule_historical_cut(&proof, || Ok(()))?;
    let mut historical = CompletedHistoricalCut {
        proof,
        memberships,
        endpoints,
        cancellation_frames: BTreeMap::new(),
    };
    for operation in historical.proof.identities().keys() {
        if historical.proof.cancellation(operation).is_some() {
            let frame = historical_cancellation_proof_frame(
                store,
                &historical,
                operation,
                4 * 1024 * 1024,
            )?;
            historical
                .cancellation_frames
                .insert(operation.clone(), frame);
        }
    }
    Ok(FixedTransferCut {
        historical: Some(historical),
        phase_schedule,
        operations: BTreeMap::new(),
        cancellations: BTreeMap::new(),
    })
}

#[cfg(test)]
pub(crate) fn test_cut_from_completed_historical_proof(
    store: &ryeos_app::state_store::StateStore,
    proof: Arc<ryeos_app::thread_lifecycle::presentation_transfer::UiHistoricalTransferCutProof>,
    memberships: Vec<PresentationMembership>,
) -> Result<Arc<FixedTransferCut>> {
    use ryeos_app::thread_lifecycle::presentation_transfer::UiTransferHistoryEndpoint;
    let mut endpoints = BTreeMap::new();
    for anchor in proof.current_anchors() {
        endpoints.insert(
            anchor.chain_root_id().to_owned(),
            UiTransferHistoryEndpoint::Current(anchor.clone()),
        );
    }
    for prefix in proof.retired_prefixes() {
        if endpoints
            .insert(
                prefix.chain_root_id().to_owned(),
                UiTransferHistoryEndpoint::Retired(prefix.clone()),
            )
            .is_some()
        {
            bail!("test completed proof mixes endpoint owners");
        }
    }
    let memberships = memberships
        .into_iter()
        .map(|membership| {
            let key = (
                membership.presentation_id.clone(),
                membership
                    .chain_root_id
                    .clone()
                    .ok_or_else(|| anyhow!("test member root absent"))?,
                membership
                    .thread_id
                    .clone()
                    .ok_or_else(|| anyhow!("test member thread absent"))?,
            );
            Ok((key, membership))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    if memberships.len() != endpoints.len() {
        bail!("test completed historical membership coverage differs");
    }
    Ok(Arc::new(completed_historical_cut(
        store,
        proof,
        memberships,
        endpoints,
    )?))
}

async fn discover_root_cut(
    state: Arc<AppState>,
    session: &BrowserSession,
    supplied: SuppliedRootCut,
) -> Result<DiscoveredRootTransferCut> {
    let membership = supplied.membership.clone();
    let anchor = supplied.anchor.clone();
    let key = supplied.key.clone();
    let (requested, cut) = discover_endpoint_cut(
        state,
        session,
        key,
        ryeos_app::thread_lifecycle::presentation_transfer::UiTransferHistoryEndpoint::Current(
            anchor.clone(),
        ),
        BTreeMap::from([(supplied.key.clone(), supplied)]),
        BTreeMap::new(),
    )
    .await?;
    match cut {
        None => Ok(DiscoveredRootTransferCut::Empty { membership, anchor }),
        Some(cut) => Ok(DiscoveredRootTransferCut::Transfers {
            membership,
            anchor,
            requested,
            cut,
        }),
    }
}

/// Read-only historical discovery. The caller supplies the original marker-bound
/// root; it cannot be replaced by a later live head or a synthetic live anchor.
pub(super) async fn read_root_transfer_cut_at_retired_prefix(
    state: Arc<AppState>,
    session: &BrowserSession,
    presentation_id: &str,
    prefix: ryeos_state::RetiredSeatPositivePrefix,
) -> Result<(
    PresentationMembership,
    Vec<TransferIdentity>,
    Option<FixedTransferCut>,
)> {
    let key = (
        presentation_id.to_owned(),
        prefix.chain_root_id().to_owned(),
        prefix.chain_root_id().to_owned(),
    );
    let membership = authorize_root(&state, session, &key)?;
    let endpoint =
        ryeos_app::thread_lifecycle::presentation_transfer::UiTransferHistoryEndpoint::Retired(
            prefix.clone(),
        );
    let (requested, cut) = discover_endpoint_cut(
        state,
        session,
        key.clone(),
        endpoint,
        BTreeMap::new(),
        BTreeMap::from([(key, (membership.clone(), prefix))]),
    )
    .await?;
    Ok((membership, requested, cut))
}

async fn discover_endpoint_cut(
    state: Arc<AppState>,
    session: &BrowserSession,
    key: SeatKey,
    local_endpoint: ryeos_app::thread_lifecycle::presentation_transfer::UiTransferHistoryEndpoint,
    mut supplied_cuts: BTreeMap<SeatKey, SuppliedRootCut>,
    mut retired_cuts: BTreeMap<
        SeatKey,
        (
            PresentationMembership,
            ryeos_state::RetiredSeatPositivePrefix,
        ),
    >,
) -> Result<(Vec<TransferIdentity>, Option<FixedTransferCut>)> {
    let mut discovery = DependencyDiscovery::default();
    discovery.queue_seat(key)?;
    while let Some(key) = discovery.work.pop_front() {
        if !supplied_cuts.contains_key(&key) && !retired_cuts.contains_key(&key) {
            // Each newly reached exact session association is authorized
            // before either capturing its head or accessing its CAS history.
            let membership = authorize_root(&state, session, &key)?;
            let store = state.state_store.clone();
            let root = key.1.clone();
            let thread = key.2.clone();
            let anchor = tokio::task::spawn_blocking(move || {
                store.capture_thread_event_read_anchor(&root, &thread)
            })
            .await??;
            if let Some(anchor) = anchor {
                supplied_cuts.insert(
                    key.clone(),
                    SuppliedRootCut {
                        key: key.clone(),
                        membership,
                        anchor,
                    },
                );
            } else {
                let store = state.state_store.clone();
                let root = key.1.clone();
                let prefix = tokio::task::spawn_blocking(move || {
                    let authority = store.pinned_state_authority()?;
                    let guard = authority.acquire_shared_guard()?;
                    authority.capture_retired_seat_positive_prefix(&root, &guard)?
                        .ok_or_else(|| anyhow!("dependency has neither current head nor marker-bound retired prefix"))
                }).await??;
                retired_cuts.insert(key.clone(), (membership, prefix));
            }
        }
        let seat_anchor = supplied_cuts.get(&key).map(|cut| cut.anchor.clone());
        let retired_prefix = retired_cuts.get(&key).map(|(_, prefix)| prefix.clone());
        let mut progress = None;
        loop {
            let store = state.state_store.clone();
            let root = key.1.clone();
            let anchor = seat_anchor.clone();
            let retired = retired_prefix.clone();
            let page = tokio::task::spawn_blocking(move || {
                if let Some(anchor) = anchor { read_signed_seat_transfer_identity_page(
                    &store,
                    &root,
                    &anchor,
                    progress.as_ref(),
                    64,
                    2 * 1024 * 1024,
                ) } else {
                    ryeos_app::thread_lifecycle::presentation_transfer::read_retired_seat_transfer_identity_page(
                        &store, retired.as_ref().ok_or_else(|| anyhow!("retired discovery endpoint missing"))?,
                        progress.as_ref(), 64, 2 * 1024 * 1024,
                    )
                }
            })
            .await??;
            for transfer in page.identities {
                discovery.collect(&key, transfer)?;
            }
            progress = page.progress;
            if progress.is_none() {
                break;
            }
        }
    }
    if discovery.operations.is_empty() {
        return Ok((Vec::new(), None));
    }
    // Freeze only after every reached seat exhausts opaque progress. This
    // superset stays internal; each seat gets one discovery walk and one shared
    // proof walk, independent of its operation count, at the very same cut.
    let requested: Vec<_> = discovery.operations.into_values().collect();
    let plan = plan_fixed_transfer_cut(&requested)?;
    if plan.groups.len() != supplied_cuts.len() + retired_cuts.len()
        || plan
            .groups
            .keys()
            .any(|key| !supplied_cuts.contains_key(key) && !retired_cuts.contains_key(key))
    {
        bail!("transfer dependency closure lacks a completed discovery cut");
    }
    let cut = if retired_cuts.is_empty() {
        read_grouped_plan(state, session, plan, supplied_cuts).await?
    } else {
        read_historical_plan(
            state,
            session,
            &local_endpoint,
            plan,
            supplied_cuts,
            retired_cuts,
        )
        .await?
    };
    Ok((requested, Some(cut)))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn retired_negative_cut_uses_real_terminal_marker_and_receipted_cancellation() {
        use ryeos_app::thread_lifecycle::presentation_transfer::{
            UiTransferHistoryEndpoint, begin_historical_transfer_cut_read_at_endpoints,
            finish_historical_transfer_cut_read, read_historical_transfer_cut_page,
        };
        use ryeos_client_base::ui::UiBindingRequestBounds;

        let (directory, state, old_proof, _prefix, projection, signed_surface, attachment) =
            super::super::super::authenticated_history::completed_cancellation_fixture_with_retention(
                ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 1 },
            );
        let mut state = state;
        let mut route = state.node_config.routes[0].clone();
        route.id = "ui/seat/append".into();
        route.path = "/ui/api/seat/append".into();
        route.methods = std::collections::HashSet::from(["POST".into()]);
        route.response.source = Some("service:ui/seat/append".into());
        route.response.source_config = serde_json::json!({});
        route.limits.body_bytes_max = 65536;
        std::sync::Arc::make_mut(&mut state.node_config)
            .routes
            .push(route);
        let state = std::sync::Arc::new(state);
        let transfer = old_proof.transfer().clone();
        let destination = &transfer.destination;
        state
            .state_store
            .touch_seat_lease(
                &destination.thread_id,
                "fp:test",
                "directive:test",
                "native:test",
            )
            .unwrap();
        assert!(
            state
                .state_store
                .claim_expired_seat_lease(&destination.thread_id, i64::MAX)
                .unwrap()
        );
        state.threads.set_scheduler_db(
            state.scheduler_db.clone(),
            state.scheduler_runtime_gate.clone(),
            directory.path().to_path_buf(),
        );
        let mut finalized = false;
        for _ in 0..256 {
            if state
                .threads
                .finalize_claimed_ui_seat_if_cleared(&destination.chain_root_id)
                .await
                .unwrap()
            {
                finalized = true;
                break;
            }
        }
        assert!(
            finalized,
            "actual lifecycle owner must finalize the destination"
        );
        let mut discovery = state
            .state_store
            .begin_terminal_seat_transfer_discovery(&destination.chain_root_id)
            .unwrap()
            .unwrap();
        while !state
            .state_store
            .read_terminal_seat_transfer_discovery_page(&mut discovery, 1, 1024 * 1024)
            .unwrap()
        {}
        let scope = state
            .state_store
            .finish_terminal_seat_transfer_discovery(discovery)
            .unwrap();
        let candidate = state
            .state_store
            .qualify_terminal_seat_transfer_retirement(scope, &[], &[])
            .unwrap();
        // The scheduler's pin-count callback is test-only bookkeeping. The
        // signed source receipt is real; the destination terminal marker and
        // mixed cut are produced and checked by their storage owners below.
        assert_eq!(
            state
                .state_store
                .retire_qualified_terminal_seat(&candidate, "2099-01-01T00:00:00Z", false, |_| Ok(
                    0
                ),)
                .unwrap()
                .unwrap()
                .retired_chains,
            1
        );
        assert!(
            state
                .state_store
                .capture_thread_event_read_anchor(
                    &destination.chain_root_id,
                    &destination.thread_id
                )
                .unwrap()
                .is_none()
        );
        let retired = {
            let authority = state.state_store.pinned_state_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            authority
                .capture_retired_seat_positive_prefix(&destination.chain_root_id, &guard)
                .unwrap()
                .unwrap()
        };
        let source_anchor = state
            .state_store
            .capture_thread_event_read_anchor(
                &transfer.source.chain_root_id,
                &transfer.source.thread_id,
            )
            .unwrap()
            .unwrap();
        let endpoints = std::collections::BTreeMap::from([
            (
                transfer.source.chain_root_id.clone(),
                UiTransferHistoryEndpoint::Current(source_anchor.clone()),
            ),
            (
                destination.chain_root_id.clone(),
                UiTransferHistoryEndpoint::Retired(retired.clone()),
            ),
        ]);
        let mut read = begin_historical_transfer_cut_read_at_endpoints(
            state.state_store.clone(),
            &source_anchor,
            std::slice::from_ref(&transfer),
            endpoints,
        )
        .unwrap();
        while !read_historical_transfer_cut_page(&mut read).unwrap() {}
        let historical = std::sync::Arc::new(finish_historical_transfer_cut_read(read).unwrap());
        assert!(historical.cancellation(&transfer.operation_id).is_some());
        assert_eq!(historical.retired_prefixes().len(), 1);

        let ui = crate::state::get_ui_state(&state).unwrap();
        let sessions = &ui.browser_sessions;
        let policy = state.node_policy.generation_digest().to_owned();
        let mut context = crate::browser_session::tests::test_shared_context();
        std::sync::Arc::make_mut(&mut context.compiled_binding)
            .binding
            .node_policy_generation_digest = policy.clone();
        let (session_id, token, _) = sessions.mint_token(context, 4, &policy).unwrap();
        sessions.consume_launch_token(&token).unwrap();
        let session = sessions.get_session(&session_id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        for seat in [&transfer.source, destination] {
            let generated = sessions
                .prepare_presentation(
                    &session_id,
                    &seat.presentation_id,
                    &root,
                    4,
                    &policy,
                    UiBindingRequestBounds {
                        max_request_bytes: 65536,
                        max_input_bytes: 65536,
                        max_seat_append_bytes: 65536,
                    },
                )
                .unwrap();
            crate::browser_session::tests::test_rekey_presentation(
                &sessions,
                &session_id,
                &generated.presentation_id,
                &seat.presentation_id,
            );
            sessions
                .bind_presentation_seat(
                    &session_id,
                    &seat.presentation_id,
                    &root,
                    &seat.thread_id,
                    &seat.chain_root_id,
                    &policy,
                )
                .unwrap();
        }
        let signed_source_initialization =
            super::super::super::authenticated_history::initialization_record(
                &state,
                &transfer.source.chain_root_id,
                &transfer.source.thread_id,
            )
            .unwrap()
            .unwrap();
        crate::browser_session::tests::test_install_signed_initialization(
            sessions,
            &session_id,
            &transfer.source.presentation_id,
            &signed_source_initialization,
        );
        let signed_destination_initialization = {
            let authority = state.state_store.pinned_state_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            let mut progress = None;
            let mut found = None;
            loop {
                let page = authority
                    .read_retired_seat_positive_prefix_page(
                        &retired,
                        progress.as_ref(),
                        256,
                        32 * 1024 * 1024,
                        &guard,
                    )
                    .unwrap();
                for (_, event) in page.events {
                    if event.event_type == ryeos_client_base::ui::presentation_initialization::PRESENTATION_INITIALIZED_EVENT {
                        found = Some(serde_json::from_value::<ryeos_client_base::ui::presentation_initialization::UiPresentationInitializationRecord>(event.payload).unwrap());
                    }
                }
                match page.progress {
                    Some(next) => progress = Some(next),
                    None => break,
                }
            }
            found.expect("retired destination has actual signed initialization")
        };
        crate::browser_session::tests::test_install_signed_initialization(
            sessions,
            &session_id,
            &destination.presentation_id,
            &signed_destination_initialization,
        );
        crate::browser_session::tests::test_seed_signed_surface(
            &sessions,
            &session_id,
            &transfer.source.presentation_id,
            &signed_surface,
            attachment,
        );
        let session = sessions.get_session(&session_id).unwrap();
        let memberships = [&transfer.source, destination]
            .into_iter()
            .map(|seat| session.presentations[&seat.presentation_id].clone())
            .collect::<Vec<_>>();
        let cut = super::test_cut_from_completed_historical_proof(
            &state.state_store,
            historical,
            memberships,
        )
        .unwrap();
        let frame = cut
            .cancellation_frame(&state.state_store, &transfer.operation_id, 4 * 1024 * 1024)
            .unwrap();
        assert_eq!(frame.transfer, transfer);
        assert_eq!(
            frame.destination_anchor.event_hash,
            retired.tip().unwrap().0
        );
        let (live, marked) = cut.publication_cut(&state, &session).unwrap();
        assert_eq!(live, vec![source_anchor.clone()]);
        assert_eq!(marked.len(), 1);
        assert!(marked[0].same_cut(&retired));
        assert!(cut.publication_cut_covered_by(&live, &marked));
        assert!(!cut.publication_cut_covered_by(&live, &[]));
        assert_eq!(
            state
                .state_store
                .with_thread_event_read_mixed_cut(
                    &transfer.source.chain_root_id,
                    &live,
                    &marked,
                    || Ok(7),
                )
                .unwrap(),
            Some(7)
        );
        let destination_membership = session.presentations[&destination.presentation_id].clone();
        let mut ordinary_retired =
            super::super::super::authenticated_history::FixedAnchorReplay::begin_at_retired_prefix(
                &state,
                &destination.presentation_id,
                retired.clone(),
                &destination_membership.initialization,
                None,
            )
            .unwrap();
        loop {
            match ordinary_retired.next_step(&state).unwrap() {
                super::super::super::authenticated_history::FixedAnchorReplayStep::Batch { .. } => {},
                super::super::super::authenticated_history::FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    panic!("ordinary retired destination invented transfer testimony")
                },
                super::super::super::authenticated_history::FixedAnchorReplayStep::Complete { .. } => break,
            }
        }
        assert!(ordinary_retired.matches_completed_retired_prefix(&retired));
        ordinary_retired
            .completed_composition_frontier(&destination_membership.initialization)
            .unwrap();
        let source_membership = session.presentations[&transfer.source.presentation_id].clone();
        let expected = session.presentations.values().cloned().collect::<Vec<_>>();
        let retired_root = [(&destination_membership, &ordinary_retired, &retired)];
        let before = sessions.get_session(&session_id).unwrap();
        let assert_unchanged = |actual: &crate::browser_session::BrowserSession| {
            assert_eq!(actual.presentations, before.presentations);
            assert_eq!(actual.root_attachment_id, before.root_attachment_id);
            assert_eq!(
                actual.next_attachment_generation,
                before.next_attachment_generation
            );
            assert_eq!(actual.next_surface_ordinal, before.next_surface_ordinal);
            assert_eq!(actual.attachments.len(), before.attachments.len());
            for (id, attachment) in &before.attachments {
                assert!(std::sync::Arc::ptr_eq(&actual.attachments[id], attachment));
            }
            assert_eq!(
                format!("{:?}", actual.pending_transfer_attachments),
                format!("{:?}", before.pending_transfer_attachments),
            );
        };
        assert!(
            sessions
                .with_reconciled_completed_roots_at_cut(
                    &state.state_store,
                    &live,
                    &marked,
                    &session_id,
                    &expected,
                    &policy,
                    &[],
                    &retired_root,
                    |_| Ok::<_, anyhow::Error>(None::<u64>),
                )
                .unwrap()
                .is_none()
        );
        assert_unchanged(&sessions.get_session(&session_id).unwrap());
        assert!(
            sessions
                .with_reconciled_completed_roots_at_cut(
                    &state.state_store,
                    &live,
                    &marked,
                    &session_id,
                    &expected,
                    "another-policy",
                    &[],
                    &retired_root,
                    |_| Ok(Some(1)),
                )
                .is_err()
        );
        assert_unchanged(&sessions.get_session(&session_id).unwrap());
        assert!(
            sessions
                .with_reconciled_completed_roots_at_cut(
                    &state.state_store,
                    &live,
                    &marked,
                    &session_id,
                    &expected[..1],
                    &policy,
                    &[],
                    &retired_root,
                    |_| Ok(Some(1)),
                )
                .is_err()
        );
        assert_unchanged(&sessions.get_session(&session_id).unwrap());
        assert!(
            sessions
                .with_reconciled_completed_roots_at_cut(
                    &state.state_store,
                    &live,
                    &marked,
                    &session_id,
                    &expected,
                    &policy,
                    &[],
                    &retired_root,
                    |_| Err::<Option<u64>, _>(anyhow::anyhow!("callback refused")),
                )
                .is_err()
        );
        assert_unchanged(&sessions.get_session(&session_id).unwrap());
        assert_eq!(
            sessions
                .with_reconciled_completed_roots_at_cut(
                    &state.state_store,
                    &live,
                    &marked,
                    &session_id,
                    &expected,
                    &policy,
                    &[],
                    &retired_root,
                    |_| Ok(Some(5)),
                )
                .unwrap(),
            Some(5)
        );
        assert!(
            sessions
                .with_reconciled_paired_roots_at_anchors(
                    &state.state_store,
                    &live,
                    &[],
                    &session_id,
                    &expected,
                    &policy,
                    &[(&source_membership, &projection, cut.as_ref())],
                    |_| Ok(Some(7)),
                )
                .is_err(),
            "omitted retired marker must not publish cleanup"
        );
        assert_eq!(
            sessions
                .with_reconciled_completed_roots_at_cut(
                    &state.state_store,
                    &live,
                    &marked,
                    &session_id,
                    &expected,
                    &policy,
                    &[(&source_membership, &projection, cut.as_ref())],
                    &retired_root,
                    |_| Ok(Some(7)),
                )
                .unwrap(),
            Some(7)
        );
        let mut retired_context = crate::browser_session::tests::test_shared_context();
        std::sync::Arc::make_mut(&mut retired_context.compiled_binding)
            .binding
            .node_policy_generation_digest = policy.clone();
        let (retired_session_id, retired_token, _) =
            sessions.mint_token(retired_context, 4, &policy).unwrap();
        sessions.consume_launch_token(&retired_token).unwrap();
        let retired_session = sessions.get_session(&retired_session_id).unwrap();
        let retired_root_coordinate =
            retired_session.attachments[&retired_session.root_attachment_id].coordinate();
        let generated = sessions
            .prepare_presentation(
                &retired_session_id,
                &destination.presentation_id,
                &retired_root_coordinate,
                4,
                &policy,
                UiBindingRequestBounds {
                    max_request_bytes: 65536,
                    max_input_bytes: 65536,
                    max_seat_append_bytes: 65536,
                },
            )
            .unwrap();
        crate::browser_session::tests::test_rekey_presentation(
            sessions,
            &retired_session_id,
            &generated.presentation_id,
            &destination.presentation_id,
        );
        sessions
            .bind_presentation_seat(
                &retired_session_id,
                &destination.presentation_id,
                &retired_root_coordinate,
                &destination.thread_id,
                &destination.chain_root_id,
                &policy,
            )
            .unwrap();
        crate::browser_session::tests::test_install_signed_initialization(
            sessions,
            &retired_session_id,
            &destination.presentation_id,
            &signed_destination_initialization,
        );
        let retired_session = sessions.get_session(&retired_session_id).unwrap();
        let only_retired = retired_session.presentations[&destination.presentation_id].clone();
        assert_eq!(
            sessions
                .with_reconciled_completed_roots_at_cut(
                    &state.state_store,
                    &[],
                    std::slice::from_ref(&retired),
                    &retired_session_id,
                    std::slice::from_ref(&only_retired),
                    &policy,
                    &[],
                    &[(&only_retired, &ordinary_retired, &retired)],
                    |_| Ok(Some(11)),
                )
                .unwrap(),
            Some(11)
        );
        let bootstrapped = crate::handlers::ui_session_current::handle(
            serde_json::json!({}),
            ryeos_app::handler_context::HandlerContext::new(
                format!("session:{retired_session_id}"),
                Vec::new(),
                false,
            ),
            state.clone(),
        )
        .await
        .unwrap();
        let bootstrapped: ryeos_client_base::ui::BrowserSession =
            serde_json::from_value(bootstrapped).unwrap();
        assert!(
            bootstrapped
                .presentations
                .iter()
                .any(|membership| membership.presentation_id == destination.presentation_id)
        );

        let mut replay = super::super::super::authenticated_history::FixedAnchorReplay::begin(
            &state,
            &transfer.source.presentation_id,
            &transfer.source.chain_root_id,
            &transfer.source.thread_id,
        )
        .unwrap();
        loop {
            match replay.next_step(&state).unwrap() {
                super::super::super::authenticated_history::FixedAnchorReplayStep::Batch { .. } => {},
                super::super::super::authenticated_history::FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    replay.attach_paired_cut(cut.clone()).unwrap();
                },
                super::super::super::authenticated_history::FixedAnchorReplayStep::Complete { .. } => break,
            }
        }
        assert!(replay.completed_paired_projection().is_ok());
        let advanced = state
            .state_store
            .append_events_if_anchor(
                &transfer.source.chain_root_id,
                &transfer.source.thread_id,
                &[ryeos_app::state_store::NewEventRecord {
                    event_type: "contract.test.retired-cut-head-advance.v1".into(),
                    storage_class: "indexed".into(),
                    payload: serde_json::json!({"advance": true}),
                }],
                &source_anchor,
            )
            .unwrap();
        assert!(matches!(
            advanced,
            ryeos_app::state_store::ConditionalEventAppendResult::Committed(_)
        ));
        assert!(
            state
                .state_store
                .with_thread_event_read_mixed_cut(
                    &transfer.source.chain_root_id,
                    &live,
                    &marked,
                    || Ok(13),
                )
                .unwrap()
                .is_none(),
            "an advanced live head must invalidate the exact mixed cut"
        );
        let after_publication = sessions.get_session(&session_id).unwrap();
        let after_expected = after_publication
            .presentations
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut entered = false;
        let refusal = sessions.with_reconciled_completed_roots_at_cut(
            &state.state_store,
            &live,
            &marked,
            &session_id,
            &after_expected,
            &policy,
            &[(&source_membership, &projection, cut.as_ref())],
            &retired_root,
            |_| {
                entered = true;
                Ok(Some(17))
            },
        );
        assert!(
            !entered,
            "a stale signed cut cannot invoke the publication callback"
        );
        assert!(refusal.is_err() || refusal.unwrap().is_none());
        let after_refusal = sessions.get_session(&session_id).unwrap();
        assert_eq!(after_refusal.presentations, after_publication.presentations);
        assert_eq!(
            after_refusal.next_attachment_generation,
            after_publication.next_attachment_generation
        );
        assert_eq!(
            after_refusal.next_surface_ordinal,
            after_publication.next_surface_ordinal
        );
        assert_eq!(
            after_refusal.attachments.len(),
            after_publication.attachments.len()
        );
        for (id, attachment) in &after_publication.attachments {
            assert!(std::sync::Arc::ptr_eq(
                &after_refusal.attachments[id],
                attachment
            ));
        }
        assert_eq!(
            format!("{:?}", after_refusal.pending_transfer_attachments),
            format!("{:?}", after_publication.pending_transfer_attachments)
        );
    }
    #[tokio::test]
    async fn two_retired_negative_endpoints_require_real_lifecycle_and_historical_cut() {
        use ryeos_app::thread_lifecycle::presentation_transfer::{
            UiTransferHistoryEndpoint, begin_historical_transfer_cut_read_at_endpoint,
            finish_historical_transfer_cut_read, read_historical_transfer_cut_page,
        };

        let (directory, state, original, _, _, signed_surface, attachment) =
            super::super::super::authenticated_history::completed_cancellation_fixture_with_retention(
                ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 1 },
            );
        let state = std::sync::Arc::new(state);
        // Session-current projects the admitted append bound from the node's
        // actual route table, including when both seats are already retired.
        let mut state = state;
        let state_mut = std::sync::Arc::get_mut(&mut state).unwrap();
        let mut config = state_mut.node_config.as_ref().clone();
        let mut route = config.routes[0].clone();
        route.id = "ui/seat/append".into();
        route.path = "/ui/api/seat/append".into();
        route.methods = std::collections::HashSet::from(["POST".into()]);
        route.response.source = Some("service:ui/seat/append".into());
        route.response.source_config = serde_json::json!({});
        route.limits.body_bytes_max = 65536;
        config.routes.push(route);
        state_mut.node_config = std::sync::Arc::new(config);
        let transfer = original.transfer().clone();
        state.threads.set_scheduler_db(
            state.scheduler_db.clone(),
            state.scheduler_runtime_gate.clone(),
            directory.path().to_path_buf(),
        );
        for seat in [&transfer.source, &transfer.destination] {
            state
                .state_store
                .touch_seat_lease(&seat.thread_id, "fp:test", "directive:test", "native:test")
                .unwrap();
            assert!(
                state
                    .state_store
                    .claim_expired_seat_lease(&seat.thread_id, i64::MAX,)
                    .unwrap()
            );
        }
        // Both terminal transitions are performed by the production clearance
        // owner while both signed heads still exist. Neither marker is minted
        // by a test helper or inferred from the absence of a current head.
        for seat in [&transfer.source, &transfer.destination] {
            let mut finalized = false;
            for _ in 0..256 {
                if state
                    .threads
                    .finalize_claimed_ui_seat_if_cleared(&seat.chain_root_id)
                    .await
                    .unwrap()
                {
                    finalized = true;
                    break;
                }
            }
            assert!(
                finalized,
                "production terminal clearance must finalize both seats"
            );
        }
        let mut retired = std::collections::BTreeMap::new();
        for _ in 0..256 {
            state
                .threads
                .advance_terminal_ui_seat_retirement("2099-01-01T00:00:00Z", false)
                .unwrap();
            let authority = state.state_store.pinned_state_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            for seat in [&transfer.source, &transfer.destination] {
                if let Some(prefix) = authority
                    .capture_retired_seat_positive_prefix(&seat.chain_root_id, &guard)
                    .unwrap()
                {
                    retired.insert(seat.chain_root_id.clone(), prefix);
                }
            }
            if retired.len() == 2 {
                break;
            }
        }
        assert_eq!(
            retired.len(),
            2,
            "both production retirement markers must remain available for historical replay"
        );
        for seat in [&transfer.source, &transfer.destination] {
            assert!(
                state
                    .state_store
                    .capture_thread_event_read_anchor(&seat.chain_root_id, &seat.thread_id,)
                    .unwrap()
                    .is_none()
            );
        }
        let local =
            UiTransferHistoryEndpoint::Retired(retired[&transfer.source.chain_root_id].clone());
        let endpoints = retired
            .iter()
            .map(|(root, prefix)| {
                (
                    root.clone(),
                    UiTransferHistoryEndpoint::Retired(prefix.clone()),
                )
            })
            .collect();
        let mut read = begin_historical_transfer_cut_read_at_endpoint(
            state.state_store.clone(),
            &local,
            std::slice::from_ref(&transfer),
            endpoints,
        )
        .unwrap();
        while !read_historical_transfer_cut_page(&mut read).unwrap() {}
        let completed = std::sync::Arc::new(finish_historical_transfer_cut_read(read).unwrap());
        assert!(completed.cancellation(&transfer.operation_id).is_some());
        assert!(completed.current_anchors().is_empty());
        assert_eq!(completed.retired_prefixes().len(), 2);
        let ui = crate::state::get_ui_state(&state).unwrap();
        let sessions = &ui.browser_sessions;
        let policy = state.node_policy.generation_digest().to_owned();
        let mut context = crate::browser_session::tests::test_shared_context();
        std::sync::Arc::make_mut(&mut context.compiled_binding)
            .binding
            .node_policy_generation_digest = policy.clone();
        let (session_id, token, _) = sessions.mint_token(context, 4, &policy).unwrap();
        sessions.consume_launch_token(&token).unwrap();
        let session = sessions.get_session(&session_id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        for seat in [&transfer.source, &transfer.destination] {
            let generated = sessions
                .prepare_presentation(
                    &session_id,
                    &seat.presentation_id,
                    &root,
                    4,
                    &policy,
                    ryeos_client_base::ui::UiBindingRequestBounds {
                        max_request_bytes: 65536,
                        max_input_bytes: 65536,
                        max_seat_append_bytes: 65536,
                    },
                )
                .unwrap();
            crate::browser_session::tests::test_rekey_presentation(
                sessions,
                &session_id,
                &generated.presentation_id,
                &seat.presentation_id,
            );
            sessions
                .bind_presentation_seat(
                    &session_id,
                    &seat.presentation_id,
                    &root,
                    &seat.thread_id,
                    &seat.chain_root_id,
                    &policy,
                )
                .unwrap();
            let initialization = {
                let authority = state.state_store.pinned_state_authority().unwrap();
                let guard = authority.acquire_shared_guard().unwrap();
                let prefix = &retired[&seat.chain_root_id];
                let mut progress = None;
                let mut found = None;
                loop {
                    let page = authority
                        .read_retired_seat_positive_prefix_page(
                            prefix,
                            progress.as_ref(),
                            256,
                            32 * 1024 * 1024,
                            &guard,
                        )
                        .unwrap();
                    for (_, event) in page.events {
                        if event.event_type == ryeos_client_base::ui::presentation_initialization::PRESENTATION_INITIALIZED_EVENT {
                            found = Some(serde_json::from_value::<ryeos_client_base::ui::presentation_initialization::UiPresentationInitializationRecord>(event.payload).unwrap());
                        }
                    }
                    match page.progress {
                        Some(next) => progress = Some(next),
                        None => break,
                    }
                }
                found.expect("retired seat has signed initialization")
            };
            crate::browser_session::tests::test_install_signed_initialization(
                sessions,
                &session_id,
                &seat.presentation_id,
                &initialization,
            );
        }
        crate::browser_session::tests::test_seed_signed_surface(
            sessions,
            &session_id,
            &transfer.source.presentation_id,
            &signed_surface,
            attachment,
        );
        let session = sessions.get_session(&session_id).unwrap();
        let memberships = [&transfer.source, &transfer.destination]
            .into_iter()
            .map(|seat| session.presentations[&seat.presentation_id].clone())
            .collect::<Vec<_>>();
        let cut = super::test_cut_from_completed_historical_proof(
            &state.state_store,
            completed,
            memberships.clone(),
        )
        .unwrap();
        let (live, marked) = cut.publication_cut(&state, &session).unwrap();
        assert!(live.is_empty());
        assert_eq!(marked.len(), 2);
        for (seat, membership) in [&transfer.source, &transfer.destination]
            .into_iter()
            .zip(&memberships)
        {
            let prefix = retired[&seat.chain_root_id].clone();
            let mut replay = super::super::super::authenticated_history::FixedAnchorReplay::begin_at_retired_prefix(
                &state, &seat.presentation_id, prefix.clone(),
                &membership.initialization, Some(cut.clone()),
            ).unwrap();
            loop {
                match replay.next_step(&state).unwrap() {
                    super::super::super::authenticated_history::FixedAnchorReplayStep::Batch { .. } => {},
                    super::super::super::authenticated_history::FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                        panic!("retired replay already owns its exact historical cut")
                    },
                    super::super::super::authenticated_history::FixedAnchorReplayStep::Complete { .. } => break,
                }
            }
            assert!(replay.matches_completed_retired_prefix(&prefix));
            replay
                .completed_composition_frontier(&membership.initialization)
                .unwrap();
        }
        let projected = crate::handlers::ui_session_current::handle(
            serde_json::json!({}),
            ryeos_app::handler_context::HandlerContext::new(
                format!("session:{session_id}"),
                Vec::new(),
                false,
            ),
            state.clone(),
        )
        .await
        .unwrap();
        let projected: ryeos_client_base::ui::BrowserSession =
            serde_json::from_value(projected).unwrap();
        assert_eq!(projected.presentations.len(), 2);
        let projected_source = projected
            .presentations
            .iter()
            .find(|membership| membership.presentation_id == transfer.source.presentation_id)
            .unwrap();
        assert!(
            !projected_source
                .surfaces
                .iter()
                .any(|surface| surface.surface_instance_id == signed_surface.surface_instance_id),
            "receipted source Surface closure must not be republished as current"
        );
        let stored = sessions.get_session(&session_id).unwrap();
        assert!(
            stored.presentations[&transfer.source.presentation_id].surfaces
                [&signed_surface.surface_instance_id]
                .closed,
            "paired retired bootstrap must commit the same authenticated cleanup"
        );
    }
    #[test]
    fn completed_negative_cut_publishes_signed_surface_closure_only_with_completion() {
        use ryeos_client_base::ui::UiBindingRequestBounds;

        let (_directory, state, proof, prefix, projection, signed_surface, attachment) =
            super::super::super::authenticated_history::completed_cancellation_fixture();
        let store = crate::browser_session::BrowserSessionStore::new();
        let policy = "22".repeat(32);
        let (session_id, token, _) = store
            .mint_token(
                crate::browser_session::tests::test_shared_context(),
                4,
                &policy,
            )
            .unwrap();
        store.consume_launch_token(&token).unwrap();
        let root_session = store.get_session(&session_id).unwrap();
        let root = root_session.attachments[&root_session.root_attachment_id].coordinate();
        for seat in [&proof.transfer().source, &proof.transfer().destination] {
            let generated = store
                .prepare_presentation(
                    &session_id,
                    &seat.presentation_id,
                    &root,
                    4,
                    &policy,
                    UiBindingRequestBounds {
                        max_request_bytes: 65536,
                        max_input_bytes: 65536,
                        max_seat_append_bytes: 65536,
                    },
                )
                .unwrap();
            crate::browser_session::tests::test_rekey_presentation(
                &store,
                &session_id,
                &generated.presentation_id,
                &seat.presentation_id,
            );
            store
                .bind_presentation_seat(
                    &session_id,
                    &seat.presentation_id,
                    &root,
                    &seat.thread_id,
                    &seat.chain_root_id,
                    &policy,
                )
                .unwrap();
        }
        crate::browser_session::tests::test_seed_signed_surface(
            &store,
            &session_id,
            &proof.transfer().source.presentation_id,
            &signed_surface,
            attachment,
        );
        let current = store.get_session(&session_id).unwrap();
        let source = current.presentations[&proof.transfer().source.presentation_id].clone();
        let destination =
            current.presentations[&proof.transfer().destination.presentation_id].clone();
        let expected = vec![source.clone(), destination.clone()];
        let anchors = vec![
            proof.source_anchor().clone(),
            proof.destination_anchor().clone(),
        ];
        let cut = super::FixedTransferCut {
            historical: None,
            phase_schedule: Vec::new(),
            cancellations: std::collections::BTreeMap::from([(
                proof.transfer().operation_id.clone(),
                super::CompletedCancellationCut {
                    frame: super::cancellation_proof_frame(
                        &state.state_store,
                        &proof,
                        &prefix,
                        4 * 1024 * 1024,
                    )
                    .unwrap(),
                    proof,
                    source_prefix: prefix,
                    source: source.clone(),
                    destination,
                },
            )]),
            operations: std::collections::BTreeMap::new(),
        };
        assert!(
            projection
                .closed_surfaces
                .contains_key(&signed_surface.surface_instance_id)
        );
        assert!(
            !current.presentations[&source.presentation_id].surfaces
                [&signed_surface.surface_instance_id]
                .closed
        );
        assert_eq!(
            store
                .with_reconciled_paired_roots_at_anchors(
                    &state.state_store,
                    &anchors,
                    &[],
                    &session_id,
                    &expected,
                    &policy,
                    &[(&source, &projection, &cut)],
                    |_| Ok(Some(42)),
                )
                .unwrap(),
            Some(42)
        );
        let published = store.get_session(&session_id).unwrap();
        assert!(
            published.presentations[&source.presentation_id].surfaces
                [&signed_surface.surface_instance_id]
                .closed
        );
        // The exact ordinary Surface attachment remains retained for
        // historical identity; closed membership removes its live use.
        assert!(
            published
                .attachments
                .contains_key(&signed_surface.binding.binding_attachment_id)
        );
    }

    #[test]
    fn completed_negative_cut_keeps_session_unchanged_on_failed_publication() {
        use ryeos_app::state_store::NewEventRecord;
        use ryeos_client_base::ui::UiBindingRequestBounds;

        let (_directory, state, proof, prefix, projection, signed_surface, surface_attachment) =
            super::super::super::authenticated_history::completed_cancellation_fixture();
        let store = crate::browser_session::BrowserSessionStore::new();
        let policy = "22".repeat(32);
        let (session_id, token, _) = store
            .mint_token(
                crate::browser_session::tests::test_shared_context(),
                4,
                &policy,
            )
            .unwrap();
        store.consume_launch_token(&token).unwrap();
        let session = store.get_session(&session_id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        let bounds = UiBindingRequestBounds {
            max_request_bytes: 65536,
            max_input_bytes: 65536,
            max_seat_append_bytes: 65536,
        };
        let bind = |seat: &ryeos_ui_presentation_contract::transfer::TransferSeat| {
            let generated = store
                .prepare_presentation(
                    &session_id,
                    &seat.presentation_id,
                    &root,
                    4,
                    &policy,
                    bounds,
                )
                .unwrap();
            crate::browser_session::tests::test_rekey_presentation(
                &store,
                &session_id,
                &generated.presentation_id,
                &seat.presentation_id,
            );
            store
                .bind_presentation_seat(
                    &session_id,
                    &seat.presentation_id,
                    &root,
                    &seat.thread_id,
                    &seat.chain_root_id,
                    &policy,
                )
                .unwrap()
        };
        bind(&proof.transfer().source);
        let destination = bind(&proof.transfer().destination);
        crate::browser_session::tests::test_seed_signed_surface(
            &store,
            &session_id,
            &proof.transfer().source.presentation_id,
            &signed_surface,
            surface_attachment,
        );
        let source = store.get_session(&session_id).unwrap().presentations
            [&proof.transfer().source.presentation_id]
            .clone();
        let expected = vec![source.clone(), destination.clone()];
        let anchors = vec![
            proof.source_anchor().clone(),
            proof.destination_anchor().clone(),
        ];
        let cut = super::FixedTransferCut {
            historical: None,
            phase_schedule: Vec::new(),
            cancellations: std::collections::BTreeMap::from([(
                proof.transfer().operation_id.clone(),
                super::CompletedCancellationCut {
                    frame: super::cancellation_proof_frame(
                        &state.state_store,
                        &proof,
                        &prefix,
                        4 * 1024 * 1024,
                    )
                    .unwrap(),
                    proof: proof.clone(),
                    source_prefix: prefix,
                    source: source.clone(),
                    destination,
                },
            )]),
            operations: std::collections::BTreeMap::new(),
        };
        let roots = [(&source, &projection, &cut)];
        let before = store.get_session(&session_id).unwrap();
        assert!(
            projection
                .closed_surfaces
                .contains_key(&signed_surface.surface_instance_id)
        );
        assert!(
            !before.presentations[&source.presentation_id].surfaces
                [&signed_surface.surface_instance_id]
                .closed
        );
        assert!(
            before
                .attachments
                .contains_key(&signed_surface.binding.binding_attachment_id)
        );

        // A full bounded channel must not commit the candidate.
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        sender.try_send(1).unwrap();
        assert!(
            store
                .with_reconciled_paired_roots_at_anchors(
                    &state.state_store,
                    &anchors,
                    &[],
                    &session_id,
                    &expected,
                    &policy,
                    &roots,
                    |_| Ok(sender.try_send(2).ok()),
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.get_session(&session_id).unwrap().presentations,
            before.presentations
        );

        assert!(
            store
                .with_reconciled_paired_roots_at_anchors::<()>(
                    &state.state_store,
                    &anchors,
                    &[],
                    &session_id,
                    &expected,
                    &policy,
                    &roots,
                    |_| anyhow::bail!("projection refused"),
                )
                .is_err()
        );
        assert_eq!(
            store.get_session(&session_id).unwrap().presentations,
            before.presentations
        );

        let mut called = false;
        assert!(
            store
                .with_reconciled_paired_roots_at_anchors(
                    &state.state_store,
                    &anchors,
                    &[],
                    &session_id,
                    &[source.clone(), source.clone()],
                    &policy,
                    &roots,
                    |_| {
                        called = true;
                        Ok(Some(()))
                    },
                )
                .is_err()
        );
        assert!(!called);
        assert_eq!(
            store.get_session(&session_id).unwrap().presentations,
            before.presentations
        );

        let extra = store
            .prepare_presentation(
                &session_id,
                "88888888-8888-4888-8888-888888888888",
                &root,
                4,
                &policy,
                bounds,
            )
            .unwrap();
        assert!(
            store
                .with_reconciled_paired_roots_at_anchors(
                    &state.state_store,
                    &anchors,
                    &[],
                    &session_id,
                    &expected,
                    &policy,
                    &roots,
                    |_| {
                        called = true;
                        Ok(Some(()))
                    },
                )
                .is_err()
        );
        assert!(!called);
        assert!(
            store
                .get_session(&session_id)
                .unwrap()
                .presentations
                .contains_key(&extra.presentation_id)
        );
        let expected_with_extra = store
            .get_session(&session_id)
            .unwrap()
            .presentations
            .values()
            .cloned()
            .collect::<Vec<_>>();

        state
            .state_store
            .append_events(
                &proof.transfer().destination.chain_root_id,
                &proof.transfer().destination.thread_id,
                &[NewEventRecord {
                    event_type: "test.signed_advance".into(),
                    storage_class: "indexed".into(),
                    payload: serde_json::json!({}),
                }],
            )
            .unwrap();
        assert!(
            store
                .with_reconciled_paired_roots_at_anchors(
                    &state.state_store,
                    &anchors,
                    &[],
                    &session_id,
                    &expected_with_extra,
                    &policy,
                    &roots,
                    |_| {
                        called = true;
                        Ok(Some(()))
                    },
                )
                .unwrap()
                .is_none()
        );
        assert!(!called);
        assert_eq!(
            store.get_session(&session_id).unwrap().presentations.len(),
            expected_with_extra.len()
        );
    }

    #[test]
    fn empty_cut_cannot_supply_publication_dependency_coverage() {
        let cut = super::FixedTransferCut {
            historical: None,
            phase_schedule: Vec::new(),
            cancellations: std::collections::BTreeMap::new(),
            operations: std::collections::BTreeMap::new(),
        };
        assert!(!cut.publication_anchors_covered_by(&[]));
    }

    #[test]
    fn dropping_schedule_reader_signals_its_worker() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let reader = super::ScheduleCancellation(flag.clone());
        assert!(!flag.load(Ordering::Acquire));
        drop(reader);
        assert!(flag.load(Ordering::Acquire));
    }

    use super::*;

    #[test]
    fn disclosure_selects_exact_root_operations_not_counterpart_only_closure() {
        let root = seat(1);
        let first = transfer(1, 1, 2);
        let counterpart_only = transfer(2, 2, 3);
        let touches = |identity: &TransferIdentity| {
            transfer_touches_seat(
                identity,
                &root.presentation_id,
                &root.chain_root_id,
                &root.thread_id,
            )
        };
        assert!(touches(&first));
        assert!(!touches(&counterpart_only));
        assert!(!transfer_touches_seat(
            &first,
            "another-presentation",
            &root.chain_root_id,
            &root.thread_id
        ));
        assert!(!transfer_touches_seat(
            &first,
            &root.presentation_id,
            "another-root",
            &root.thread_id
        ));
        assert!(!transfer_touches_seat(
            &first,
            &root.presentation_id,
            &root.chain_root_id,
            "another-thread"
        ));
        // Filtering disclosure does not narrow server proof scope.
        assert_eq!(
            plan_fixed_transfer_cut(&[first, counterpart_only])
                .unwrap()
                .operations
                .len(),
            2
        );
    }

    #[test]
    fn dependency_worklist_closes_multiple_hops_and_deduplicates_cycles() {
        let root = seat_key(&seat(1));
        let middle = seat_key(&seat(2));
        let last = seat_key(&seat(3));
        let first = transfer(1, 1, 2);
        let earlier = transfer(2, 2, 3);
        let cycle = transfer(3, 3, 1);
        let mut discovery = DependencyDiscovery::default();
        discovery.queue_seat(root.clone()).unwrap();
        assert_eq!(discovery.work.pop_front(), Some(root.clone()));
        discovery.collect(&root, first.clone()).unwrap();
        assert_eq!(discovery.work.pop_front(), Some(middle.clone()));
        discovery.collect(&middle, first.clone()).unwrap();
        discovery.collect(&middle, earlier.clone()).unwrap();
        assert_eq!(discovery.work.pop_front(), Some(last.clone()));
        discovery.collect(&last, earlier.clone()).unwrap();
        discovery.collect(&last, cycle.clone()).unwrap();
        assert!(discovery.work.is_empty());
        assert_eq!(discovery.queued.len(), 3);
        assert_eq!(discovery.operations.len(), 3);
        let requested: Vec<_> = discovery.operations.into_values().collect();
        let plan = plan_fixed_transfer_cut(&requested).unwrap();
        assert_eq!(plan.groups.len(), 3);
        assert_eq!(plan.groups[&root].requested, vec![first.clone(), cycle]);
        assert_eq!(plan.groups[&middle].requested, vec![first, earlier]);
    }

    #[test]
    fn dependency_discovery_refuses_reassociation_and_conflicting_shared_identity() {
        let root = seat_key(&seat(1));
        let middle = seat_key(&seat(2));
        let first = transfer(1, 1, 2);
        let mut discovery = DependencyDiscovery::default();
        discovery.queue_seat(root.clone()).unwrap();
        discovery.collect(&root, first.clone()).unwrap();
        let mut changed = first.clone();
        changed.destination.revision += 1;
        assert!(discovery.collect(&middle, changed).is_err());
        let mut other_presentation = seat_key(&seat(2));
        other_presentation.0 = seat(4).presentation_id;
        assert!(discovery.queue_seat(other_presentation).is_err());
        let mut other_coordinates = seat_key(&seat(2));
        other_coordinates.1 = seat(4).chain_root_id;
        other_coordinates.2 = seat(4).thread_id;
        assert!(discovery.queue_seat(other_coordinates).is_err());
        let mut not_root = seat_key(&seat(4));
        not_root.2 = "different-thread".into();
        assert!(discovery.queue_seat(not_root).is_err());
        assert_eq!(discovery.operations.len(), 1);
        assert_eq!(discovery.queued.len(), 2);
    }

    #[test]
    fn root_collector_deduplicates_full_identity_not_historical_origins() {
        let root = seat_key(&seat(1));
        let first = transfer(1, 1, 2);
        let mut second = transfer(2, 3, 1);
        second.destination.revision += 10;
        second.destination.producer_incarnation = "b".repeat(64);
        let mut operations = BTreeMap::new();
        collect_root_identity(&root, &mut operations, first.clone()).unwrap();
        collect_root_identity(&root, &mut operations, first.clone()).unwrap();
        collect_root_identity(&root, &mut operations, second.clone()).unwrap();
        assert_eq!(operations.len(), 2);
        assert_eq!(operations[&second.operation_id], second);
        let mut conflicting = first;
        conflicting.source.revision += 1;
        assert!(collect_root_identity(&root, &mut operations, conflicting).is_err());
    }

    #[test]
    fn root_collector_refuses_foreign_coordinates_owner_and_malformed_identity() {
        let root = seat_key(&seat(1));
        let mut operations = BTreeMap::new();
        assert!(collect_root_identity(&root, &mut operations, transfer(1, 2, 3)).is_err());
        let mut wrong_owner = transfer(1, 1, 2);
        wrong_owner.source.presentation_id = seat(3).presentation_id;
        assert!(collect_root_identity(&root, &mut operations, wrong_owner).is_err());
        let mut malformed = transfer(1, 1, 2);
        malformed.operation_id = "not-an-operation".into();
        assert!(collect_root_identity(&root, &mut operations, malformed).is_err());
        assert!(operations.is_empty());
    }

    #[test]
    fn supplied_root_cut_must_be_exact_group_and_exact_anchor_coordinates() {
        let plan = plan_fixed_transfer_cut(&[transfer(1, 1, 2), transfer(2, 1, 3)]).unwrap();
        let root = seat_key(&seat(1));
        validate_supplied_group(&plan, &root, &root.1, &root.2).unwrap();
        assert_eq!(plan.groups[&root].requested.len(), 2);
        assert!(validate_supplied_group(&plan, &root, "other-root", &root.2).is_err());
        assert!(validate_supplied_group(&plan, &root, &root.1, "other-thread").is_err());
        let mut wrong_presentation = root.clone();
        wrong_presentation.0 = seat(4).presentation_id;
        assert!(validate_supplied_group(&plan, &wrong_presentation, &root.1, &root.2).is_err());
        let absent = seat_key(&seat(4));
        assert!(validate_supplied_group(&plan, &absent, &absent.1, &absent.2).is_err());
    }

    fn seat(index: u64) -> TransferSeat {
        TransferSeat {
            presentation_id: format!("00000000-0000-4000-8000-{index:012x}"),
            chain_root_id: format!("T-seat-{index}"),
            thread_id: format!("T-seat-{index}"),
            producer_incarnation: format!("{index:064x}"),
            revision: index,
        }
    }

    fn transfer(index: u64, source: u64, destination: u64) -> TransferIdentity {
        TransferIdentity {
            operation_id: format!("10000000-0000-4000-8000-{index:012x}"),
            source: seat(source),
            destination: seat(destination),
            export_digest: "a".repeat(64),
        }
    }

    #[test]
    fn same_stable_pair_groups_full_operations_across_historical_origins() {
        let first = transfer(1, 1, 2);
        let mut second = transfer(2, 1, 2);
        second.source.revision += 10;
        second.destination.revision += 20;
        second.source.producer_incarnation = "b".repeat(64);
        second.destination.producer_incarnation = "c".repeat(64);
        let plan =
            plan_fixed_transfer_cut(&[first.clone(), second.clone(), first.clone()]).unwrap();
        assert_eq!(plan.operations.len(), 2);
        assert_eq!(plan.groups.len(), 2);
        for group in plan.groups.values() {
            assert_eq!(group.requested, vec![first.clone(), second.clone()]);
        }
        assert_eq!(plan.operations[&second.operation_id], second);
    }

    #[test]
    fn multiple_counterparts_share_one_root_group_with_complete_scope() {
        let first = transfer(1, 1, 2);
        let second = transfer(2, 1, 3);
        let plan = plan_fixed_transfer_cut(&[first.clone(), second.clone()]).unwrap();
        assert_eq!(plan.groups.len(), 3);
        assert_eq!(
            plan.groups[&seat_key(&first.source)].requested,
            vec![first.clone(), second.clone()]
        );
        assert_eq!(
            plan.groups[&seat_key(&first.destination)].requested,
            vec![first]
        );
        assert_eq!(
            plan.groups[&seat_key(&second.destination)].requested,
            vec![second]
        );
    }

    #[test]
    fn empty_malformed_or_conflicting_operation_requests_refuse() {
        assert!(plan_fixed_transfer_cut(&[]).is_err());
        let first = transfer(1, 1, 2);
        let mut conflicting = first.clone();
        conflicting.source.revision += 1;
        assert!(plan_fixed_transfer_cut(&[first.clone(), conflicting]).is_err());
        let mut malformed = first;
        malformed.export_digest = "not-a-digest".into();
        assert!(plan_fixed_transfer_cut(&[malformed]).is_err());
    }

    #[test]
    fn a_stable_seat_cannot_change_presentation_or_vice_versa() {
        let first = transfer(1, 1, 2);
        let mut presentation_changed = transfer(2, 1, 3);
        presentation_changed.source.presentation_id = seat(4).presentation_id;
        assert!(plan_fixed_transfer_cut(&[first.clone(), presentation_changed]).is_err());
        let mut coordinates_changed = transfer(2, 1, 3);
        coordinates_changed.source.chain_root_id = "T-new-root".into();
        coordinates_changed.source.thread_id = "T-new-seat".into();
        assert!(plan_fixed_transfer_cut(&[first, coordinates_changed]).is_err());
    }
}
