//! Transient staging for destination transfer attachment batches.
//!
//! Durable acceptance remains the seat journal owner. Staging allocates no
//! session state. The conditional append callback reserves the exact hidden
//! batch before publishing Accepted; generic resolve/detach cannot expose it.

use std::sync::Arc;

use ryeos_app::state_store::StateStore;
use ryeos_state::{AuthoritativeThreadEventReadAnchor, RetiredChainDependency};

use ryeos_client_base::ui::{
    binding::UiBindingAttachmentCoordinate, composition_replay::UiCompositionReplay,
    transfer_export::UiTransferExport, transfer_reservation::UiTransferReservation,
};
use ryeos_ui_presentation_contract::{
    transfer::{TransferEvidence, TransferIdentity, TransferRecord},
    transfer_state::TransferLifecycleState,
};

use super::{
    AdmittedBindingAttachment, AttachmentStoreError, BindingAttachmentCandidate,
    BindingAttachmentCoordinate, BrowserSession, BrowserSessionStore, build_attachment,
    exact_attachment, exact_presentation_attachment, exact_shared_presentation_root, live_session,
    require_shared_presentation_root,
};

/// What the session needs to know of a completed paired transfer cut. The seat
/// core owns the cut; the session only compares it with its own retained
/// membership and reads the authenticated phases it released.
pub trait CompletedPairedCut {
    /// Every dependency of the cut stays excluded by the supplied exclusion.
    fn publication_cut_covered_by(
        &self,
        anchors: &[AuthoritativeThreadEventReadAnchor],
        retired: &[RetiredChainDependency],
    ) -> bool;

    /// In-memory equality of the cut's memberships with the session's.
    fn publication_memberships_match(&self, session: &BrowserSession) -> bool;

    /// The cut fixed exactly this live seat frontier for this presentation.
    fn covers_seat_anchor(
        &self,
        presentation_id: &str,
        chain_root_id: &str,
        thread_id: &str,
        anchor: &AuthoritativeThreadEventReadAnchor,
    ) -> bool;

    /// The cut fixed exactly this retired seat fence for this presentation.
    fn covers_retired_seat(
        &self,
        presentation_id: &str,
        chain_root_id: &str,
        dependency: &RetiredChainDependency,
    ) -> bool;

    /// Visit only phases authenticated by this cut.
    fn for_each_authenticated_phase(
        &self,
        visit: &mut dyn FnMut(
            &TransferEvidence,
            &TransferRecord,
            &TransferLifecycleState,
        ) -> anyhow::Result<()>,
    ) -> anyhow::Result<()>;
}

/// A retired seat's replay folded to its completed composition frontier.
pub struct CompletedRetiredFrontier {
    pub replay: UiCompositionReplay,
    pub transfer_testimony_seen: bool,
}

/// What the session needs to know of a retired seat's completed replay.
pub trait CompletedRetiredRoot {
    fn completed_paired_cut(&self) -> Option<&dyn CompletedPairedCut>;

    /// The replay was read through exactly this fence.
    fn matches_completed_retired_dependency(&self, dependency: &RetiredChainDependency) -> bool;

    /// Transfers the root's retained evidence names whose every other seat is
    /// retired too. Retirement qualified each as completely settled, so no
    /// seat is left to activate a hidden reservation one of them still holds.
    fn settled_retired_transfers(&self) -> &[TransferIdentity];

    fn completed_composition_frontier(
        &self,
        initialization: &ryeos_client_base::ui::binding::UiPresentationInitialization,
    ) -> anyhow::Result<CompletedRetiredFrontier>;
}

/// One shared-cut entry per live root fold.
pub type CompletedLiveRoot<'a> = (
    &'a super::PresentationMembership,
    &'a UiCompositionReplay,
    &'a dyn CompletedPairedCut,
);

/// One shared-cut entry per retired root fold.
pub type CompletedRetiredRootEntry<'a> = (
    &'a super::PresentationMembership,
    &'a dyn CompletedRetiredRoot,
    &'a RetiredChainDependency,
);

fn with_session_cut<T>(
    store: &StateStore,
    anchors: &[AuthoritativeThreadEventReadAnchor],
    retired: &[RetiredChainDependency],
    action: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    if retired.is_empty() {
        store.with_thread_event_read_anchors(anchors, action)
    } else {
        store.with_thread_event_read_mixed_cut(anchors, retired, action)
    }
}

#[derive(Debug, Clone)]
struct StagedTransferAttachment {
    source: BindingAttachmentCoordinate,
    destination: Arc<AdmittedBindingAttachment>,
}

/// Call-local batch. It is neither durable transfer state nor a second
/// reservation registry and must be committed only after exact acceptance.
#[derive(Debug, Clone)]
pub struct StagedTransferAttachmentBatch {
    transfer: TransferIdentity,
    generation_start: u64,
    next_generation: u64,
    attachments: Vec<StagedTransferAttachment>,
}

/// Path/policy validation was performed on these exact pinned attachment
/// identities outside the signed-chain exclusion. Publication still compares
/// the complete batch and current session ownership under its own lock.
pub struct PrevalidatedTransferAttachmentBatch(StagedTransferAttachmentBatch);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenTransferReservationOutcome {
    Created,
    ExistingExact,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActivatedTransferReservationMembership {
    transfer: TransferIdentity,
    reservation: UiTransferReservation,
    coordinates: Vec<BindingAttachmentCoordinate>,
    insertion_ownership: Vec<(
        ryeos_client_base::ids::SurfaceInstanceId,
        ryeos_client_base::ids::ViewSetId,
        BindingAttachmentCoordinate,
    )>,
    mount_ownership: Vec<(
        ryeos_client_base::ids::RyeOsViewInstanceKey,
        BindingAttachmentCoordinate,
    )>,
}

/// Exact predecessor removed by one canonical composition transition.
/// The coordinate prevents a retry for an old mount from retiring a newer
/// binding installed at the same stable view-instance key.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct ActivatedTransferMountPredecessor {
    pub view_instance_key: ryeos_client_base::ids::RyeOsViewInstanceKey,
    pub binding: BindingAttachmentCoordinate,
}

impl ActivatedTransferReservationMembership {
    /// Only activated, still-owned coordinates may be advertised at bootstrap.
    /// Accepted but unpublished batches remain outside this membership.
    pub fn current_coordinates(&self) -> impl Iterator<Item = &BindingAttachmentCoordinate> {
        self.coordinates.iter()
    }

    pub(super) fn owns_coordinate(&self, coordinate: &BindingAttachmentCoordinate) -> bool {
        self.coordinates.iter().any(|owned| owned == coordinate)
    }

    fn targets_surface(&self, surface_instance_id: &str) -> bool {
        use ryeos_client_base::ui::transfer_reservation::UiTransferDestinationPlacement;
        match &self.reservation.placement {
            UiTransferDestinationPlacement::Tabs { manifest, .. } => {
                manifest.surface_instance_id.as_str() == surface_instance_id
            }
            UiTransferDestinationPlacement::View {
                surface_instance_id: target,
                ..
            } => target.as_str() == surface_instance_id,
            UiTransferDestinationPlacement::SharedDock { .. } => false,
        }
    }

    #[cfg(test)]
    fn retire_mounts(
        &mut self,
        removed: &[ActivatedTransferMountPredecessor],
    ) -> Vec<BindingAttachmentCoordinate> {
        let candidates = self
            .mount_ownership
            .iter()
            .filter(|(instance, coordinate)| {
                removed.iter().any(|predecessor| {
                    &predecessor.view_instance_key == instance && &predecessor.binding == coordinate
                })
            })
            .map(|(_, coordinate)| coordinate.clone())
            .collect::<Vec<_>>();
        self.mount_ownership.retain(|(instance, coordinate)| {
            !removed.iter().any(|predecessor| {
                &predecessor.view_instance_key == instance && &predecessor.binding == coordinate
            })
        });
        let mut retired = Vec::new();
        for coordinate in candidates {
            if !self
                .insertion_ownership
                .iter()
                .any(|(_, _, insertion)| insertion == &coordinate)
                && !self
                    .mount_ownership
                    .iter()
                    .any(|(_, retained)| retained == &coordinate)
            {
                self.coordinates.retain(|owned| owned != &coordinate);
                if !retired.contains(&coordinate) {
                    retired.push(coordinate);
                }
            }
        }
        retired
    }

    fn retain_current_edges(
        &mut self,
        mut mount_live: impl FnMut(
            &ryeos_client_base::ids::RyeOsViewInstanceKey,
            &BindingAttachmentCoordinate,
        ) -> bool,
        mut insertion_live: impl FnMut(
            &ryeos_client_base::ids::SurfaceInstanceId,
            ryeos_client_base::ids::ViewSetId,
            &BindingAttachmentCoordinate,
        ) -> bool,
    ) -> usize {
        let mut retired = 0;
        self.mount_ownership.retain(|(instance, coordinate)| {
            let keep = mount_live(instance, coordinate);
            retired += usize::from(!keep);
            keep
        });
        self.insertion_ownership
            .retain(|(surface, set, coordinate)| {
                let keep = insertion_live(surface, *set, coordinate);
                retired += usize::from(!keep);
                keep
            });
        let mounts = &self.mount_ownership;
        let insertions = &self.insertion_ownership;
        self.coordinates.retain(|coordinate| {
            mounts.iter().any(|(_, owned)| owned == coordinate)
                || insertions.iter().any(|(_, _, owned)| owned == coordinate)
        });
        retired
    }
}

pub(super) fn pending_attachment_count(session: &BrowserSession) -> usize {
    session
        .pending_transfer_attachments
        .values()
        .try_fold(0usize, |count, batch| {
            count.checked_add(batch.attachments.len())
        })
        .unwrap_or(usize::MAX)
}

pub(super) fn occupied_attachment_count(session: &BrowserSession) -> usize {
    session
        .attachments
        .len()
        .saturating_add(pending_attachment_count(session))
}

pub(super) fn pending_attachments(
    session: &BrowserSession,
) -> impl Iterator<Item = &Arc<AdmittedBindingAttachment>> {
    session
        .pending_transfer_attachments
        .values()
        .flat_map(|batch| batch.attachments.iter().map(|entry| &entry.destination))
}

impl StagedTransferAttachmentBatch {
    /// Retained private admissions for current-authority revalidation only.
    /// Iteration does not publish a binding or grant invocation authority.
    pub fn destination_admissions(&self) -> impl Iterator<Item = &Arc<AdmittedBindingAttachment>> {
        self.attachments.iter().map(|entry| &entry.destination)
    }
    /// Project signed, renderer-neutral metadata only. The private pinned
    /// admission remains in this batch and is checked again at activation.
    pub fn destination_descriptors(
        &self,
        source: &UiCompositionReplay,
    ) -> Result<Vec<ryeos_client_base::ui::binding::UiBindingAttachment>, AttachmentStoreError>
    {
        self.attachments
            .iter()
            .map(|entry| {
                let origin = source
                    .binding_descriptors()
                    .get(&entry.source.binding_attachment_id)
                    .filter(|descriptor| {
                        descriptor.binding_generation == entry.source.binding_generation
                            && descriptor.binding_digest == entry.source.binding_digest
                    })
                    .ok_or(AttachmentStoreError::PresentationConflict)?;
                let descriptor = entry
                    .destination
                    .public_descriptor(origin.binding_request_bounds);
                descriptor
                    .validate()
                    .map_err(|_| AttachmentStoreError::PresentationConflict)?;
                Ok(descriptor)
            })
            .collect()
    }

    pub fn destination_binding(
        &self,
        source: &UiBindingAttachmentCoordinate,
    ) -> Option<UiBindingAttachmentCoordinate> {
        let source = private_coordinate(source);
        self.attachments
            .iter()
            .find(|entry| entry.source == source)
            .map(|entry| public_coordinate(&entry.destination.coordinate()))
    }
}

impl BrowserSessionStore {
    /// Reuse an exact pending in-process batch on retry, or allocate a fresh,
    /// call-local attachment for every distinct source binding in export
    /// order. No counter or attachment map is mutated here.
    pub fn stage_fresh_transfer_attachment_batch(
        &self,
        session_id: &str,
        transfer: &TransferIdentity,
        export: &UiTransferExport,
        max_attachments: usize,
        current_policy_digest: &str,
    ) -> Result<StagedTransferAttachmentBatch, AttachmentStoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        if let Some(pending) = session
            .pending_transfer_attachments
            .get(&transfer.operation_id)
        {
            if pending.transfer != *transfer {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            // A retry may reuse only the complete in-process batch. Match the
            // export's distinct source closure in order, not merely a subset
            // of coordinates found in the pending reservation.
            let mut sources = Vec::new();
            for source in export_source_bindings(export) {
                let source = private_coordinate(source);
                if !sources.contains(&source) {
                    sources.push(source);
                }
            }
            if sources.is_empty()
                || sources.len() != pending.attachments.len()
                || sources
                    .iter()
                    .zip(&pending.attachments)
                    .any(|(source, entry)| source != &entry.source)
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            for entry in &pending.attachments {
                let retained = exact_presentation_attachment(
                    session,
                    &transfer.source.presentation_id,
                    &entry.source,
                    current_policy_digest,
                )?;
                validate_retained_attachment(&retained, current_policy_digest)?;
                validate_retained_attachment(&entry.destination, current_policy_digest)?;
                if !same_retained_authority(&retained, &entry.destination) {
                    return Err(AttachmentStoreError::PresentationConflict);
                }
            }
            return Ok(pending.clone());
        }
        let mut generation = session.next_attachment_generation;
        let generation_start = generation;
        let mut attachments: Vec<StagedTransferAttachment> = Vec::new();
        for source in export_source_bindings(export) {
            let source = private_coordinate(source);
            if attachments.iter().any(|entry| entry.source == source) {
                continue;
            }
            let retained = exact_presentation_attachment(
                session,
                &transfer.source.presentation_id,
                &source,
                current_policy_digest,
            )?;
            validate_retained_attachment(&retained, current_policy_digest)?;
            let destination = Arc::new(build_attachment(
                uuid::Uuid::new_v4().to_string(),
                generation,
                candidate_from(&retained),
                &session.principal_id,
                current_policy_digest,
            )?);
            generation = generation
                .checked_add(1)
                .ok_or(AttachmentStoreError::Capacity)?;
            attachments.push(StagedTransferAttachment {
                source,
                destination,
            });
        }
        if attachments.is_empty()
            || occupied_attachment_count(session)
                .checked_add(attachments.len())
                .is_none_or(|count| count > max_attachments)
        {
            return Err(AttachmentStoreError::Capacity);
        }
        Ok(StagedTransferAttachmentBatch {
            transfer: transfer.clone(),
            generation_start,
            next_generation: generation,
            attachments,
        })
    }

    /// Recover a call-local handle only from an exact in-process hidden batch
    /// or an already activated complete live destination batch. Signed
    /// Accepted coordinates alone cannot recreate process-local admission.
    pub fn stage_retained_transfer_attachment_batch(
        &self,
        session_id: &str,
        transfer: &TransferIdentity,
        export: &UiTransferExport,
        reservation: &UiTransferReservation,
        max_record_bytes: u64,
        current_policy_digest: &str,
    ) -> Result<StagedTransferAttachmentBatch, AttachmentStoreError> {
        self.stage_retained_transfer_attachment_batch_inner(
            session_id,
            transfer,
            export,
            reservation,
            max_record_bytes,
            current_policy_digest,
            true,
        )
    }

    /// A completed source decision transfers the admitted hidden batch's
    /// provenance; it must not borrow a source mount that commit has retired.
    /// The ingress separately revalidates every retained destination admission
    /// against the current engine, policy and project/grant owner before locks.
    pub fn stage_activation_transfer_attachment_batch(
        &self,
        session_id: &str,
        proof: &crate::transfer_store::reader::UiTransferFixedHistoryProof,
        export: &UiTransferExport,
        reservation: &UiTransferReservation,
        max_record_bytes: u64,
        current_policy_digest: &str,
    ) -> Result<StagedTransferAttachmentBatch, AttachmentStoreError> {
        if proof.lifecycle().source_state()
            != ryeos_ui_presentation_contract::transfer_state::TransferSourceState::Committed
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        self.stage_retained_transfer_attachment_batch_inner(
            session_id,
            proof.lifecycle().transfer(),
            export,
            reservation,
            max_record_bytes,
            current_policy_digest,
            false,
        )
    }

    fn stage_retained_transfer_attachment_batch_inner(
        &self,
        session_id: &str,
        transfer: &TransferIdentity,
        export: &UiTransferExport,
        reservation: &UiTransferReservation,
        max_record_bytes: u64,
        current_policy_digest: &str,
        require_source_admissions: bool,
    ) -> Result<StagedTransferAttachmentBatch, AttachmentStoreError> {
        reservation
            .validate_export(transfer, export, max_record_bytes)
            .map_err(|_| AttachmentStoreError::PresentationConflict)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        if let Some(pending) = session
            .pending_transfer_attachments
            .get(&transfer.operation_id)
        {
            validate_reservation_bindings(pending, export, reservation)?;
            validate_retained_batch(pending, current_policy_digest)?;
            if pending.transfer != *transfer {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            let destination = session
                .presentations
                .get(&transfer.destination.presentation_id)
                .ok_or(AttachmentStoreError::PresentationInvalid)?;
            exact_presentation_attachment(
                session,
                &transfer.destination.presentation_id,
                &destination.root_coordinate,
                current_policy_digest,
            )?;
            // Retained hidden memory is not itself a current browser grant.
            // A positive signed Accepted response must still have the exact
            // source admissions from which this batch was derived.
            if require_source_admissions {
                validate_retained_source_admissions(session, pending, current_policy_digest)?;
            }
            return Ok(pending.clone());
        }
        let pairs = reservation_binding_pairs(export, reservation)?;
        let mut attachments: Vec<StagedTransferAttachment> = Vec::new();
        let mut generation_start = None;
        let mut next_generation = None;
        let live_destinations = pairs
            .iter()
            .filter(|(_, destination)| {
                session
                    .attachments
                    .contains_key(&destination.binding_attachment_id)
            })
            .count();
        if live_destinations != pairs.len() {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        for (source_coordinate, destination_coordinate) in pairs {
            let source = private_coordinate(source_coordinate);
            let destination = private_coordinate(destination_coordinate);
            if let Some(existing) = attachments.iter().find(|entry| entry.source == source) {
                if existing.destination.coordinate() != destination {
                    return Err(AttachmentStoreError::PresentationConflict);
                }
                continue;
            }
            let expected_generation = match next_generation {
                Some(expected) => expected,
                None => {
                    generation_start = Some(destination.binding_generation);
                    destination.binding_generation
                }
            };
            if destination.binding_generation != expected_generation {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            if attachments
                .iter()
                .any(|entry| entry.destination.coordinate() == destination)
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            let attachment = exact_presentation_attachment(
                session,
                &transfer.destination.presentation_id,
                &destination,
                current_policy_digest,
            )?;
            validate_retained_attachment(&attachment, current_policy_digest)?;
            if attachment.coordinate() != destination {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            next_generation = Some(
                expected_generation
                    .checked_add(1)
                    .ok_or(AttachmentStoreError::Capacity)?,
            );
            attachments.push(StagedTransferAttachment {
                source,
                destination: attachment,
            });
        }
        Ok(StagedTransferAttachmentBatch {
            transfer: transfer.clone(),
            generation_start: generation_start.ok_or(AttachmentStoreError::PresentationInvalid)?,
            next_generation: next_generation.ok_or(AttachmentStoreError::PresentationInvalid)?,
            attachments,
        })
    }

    /// Test entry for the same hidden reservation owner. Production holds the
    /// complete session snapshot before entering StateStore and chain locks,
    /// then calls the in-session helper from Accepted's admission callback.
    #[cfg(test)]
    pub fn reserve_hidden_transfer_attachment_batch(
        &self,
        session_id: &str,
        staged: &StagedTransferAttachmentBatch,
        export: &UiTransferExport,
        reservation: &UiTransferReservation,
        max_attachments: usize,
        max_record_bytes: u64,
        current_policy_digest: &str,
    ) -> Result<HiddenTransferReservationOutcome, AttachmentStoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        Self::reserve_hidden_transfer_attachment_batch_in_session(
            session,
            staged,
            export,
            reservation,
            max_attachments,
            max_record_bytes,
            current_policy_digest,
        )
    }

    /// Called inside Accepted's admission callback with the session owner
    /// already held. Never reacquires that owner underneath StateStore locks.
    pub fn reserve_hidden_transfer_attachment_batch_in_session(
        session: &mut BrowserSession,
        staged: &StagedTransferAttachmentBatch,
        export: &UiTransferExport,
        reservation: &UiTransferReservation,
        max_attachments: usize,
        max_record_bytes: u64,
        current_policy_digest: &str,
    ) -> Result<HiddenTransferReservationOutcome, AttachmentStoreError> {
        if session.expires_at <= std::time::Instant::now() {
            return Err(AttachmentStoreError::SessionInvalid);
        }
        reservation
            .validate_export(&staged.transfer, export, max_record_bytes)
            .map_err(|_| AttachmentStoreError::PresentationConflict)?;
        validate_reservation_bindings(staged, export, reservation)?;

        // Recheck the destination root and policy inside the committing lock.
        let destination = session
            .presentations
            .get(&staged.transfer.destination.presentation_id)
            .ok_or(AttachmentStoreError::PresentationInvalid)?;
        exact_presentation_attachment(
            session,
            &staged.transfer.destination.presentation_id,
            &destination.root_coordinate,
            current_policy_digest,
        )?;
        // Staging was call-local. Source membership or its pinned authority
        // may have changed before the signed append's admission callback.
        // Recheck the exact source closure under this same session lock even
        // for an existing hidden batch. A pending batch is not signed proof
        // that an uncertain Accepted write committed; its retry must not
        // re-admit a now-revoked source. Recovery reads the signed history.
        for entry in &staged.attachments {
            let source = exact_presentation_attachment(
                session,
                &staged.transfer.source.presentation_id,
                &entry.source,
                current_policy_digest,
            )?;
            validate_retained_attachment(&source, current_policy_digest)?;
            if !same_retained_authority(&source, &entry.destination) {
                return Err(AttachmentStoreError::PresentationConflict);
            }
        }

        if let Some(pending) = session
            .pending_transfer_attachments
            .get(&staged.transfer.operation_id)
        {
            if !same_batch(pending, staged) {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            validate_retained_batch(pending, current_policy_digest)?;
            return Ok(HiddenTransferReservationOutcome::ExistingExact);
        }
        let live = staged
            .attachments
            .iter()
            .filter(|entry| {
                session
                    .attachments
                    .contains_key(&entry.destination.binding_attachment_id)
            })
            .count();
        // An already published destination cannot stand in for a hidden
        // reservation. Activation and Accepted admission are distinct phases.
        if live != 0 {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        if session.next_attachment_generation == staged.generation_start {
            validate_retained_batch(staged, current_policy_digest)?;
            if occupied_attachment_count(session)
                .checked_add(staged.attachments.len())
                .is_none_or(|count| count > max_attachments)
            {
                return Err(AttachmentStoreError::Capacity);
            }
            session.next_attachment_generation = staged.next_generation;
            session
                .pending_transfer_attachments
                .insert(staged.transfer.operation_id.clone(), staged.clone());
            return Ok(HiddenTransferReservationOutcome::Created);
        }
        // No missing-cache fallback: a later generation can belong to an
        // ordinary publisher. Only the retained exact batch above proves this
        // interval was consumed by this operation.
        Err(AttachmentStoreError::PresentationConflict)
    }

    /// Discard only the exact hidden batch created by this call after a
    /// definite pre-write Accepted conflict. Unlike durable-abort release,
    /// this requires the call-local batch identity; consumed generations are
    /// deliberately not rolled back.
    pub fn discard_new_hidden_transfer_attachment_batch(
        &self,
        session_id: &str,
        staged: &StagedTransferAttachmentBatch,
    ) -> Result<bool, AttachmentStoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        if session
            .presentations
            .get(&staged.transfer.destination.presentation_id)
            .and_then(|presentation| {
                presentation
                    .activated_transfer_reservations
                    .get(&staged.transfer.operation_id)
            })
            .is_some()
            || staged.attachments.iter().any(|entry| {
                session
                    .attachments
                    .contains_key(&entry.destination.binding_attachment_id)
            })
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        let Some(pending) = session
            .pending_transfer_attachments
            .get(&staged.transfer.operation_id)
        else {
            return Ok(false);
        };
        if !same_batch(pending, staged) {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        session
            .pending_transfer_attachments
            .remove(&staged.transfer.operation_id);
        Ok(true)
    }

    /// Atomically publish the exact previously reserved attachments and their
    /// destination membership only after durable activation.
    pub fn prevalidate_transfer_attachment_batch(
        &self,
        staged: &StagedTransferAttachmentBatch,
        export: &UiTransferExport,
        reservation: &UiTransferReservation,
        max_record_bytes: u64,
        current_policy_digest: &str,
    ) -> Result<PrevalidatedTransferAttachmentBatch, AttachmentStoreError> {
        reservation
            .validate_export(&staged.transfer, export, max_record_bytes)
            .map_err(|_| AttachmentStoreError::PresentationConflict)?;
        validate_reservation_bindings(staged, export, reservation)?;
        validate_retained_batch(staged, current_policy_digest)?;
        Ok(PrevalidatedTransferAttachmentBatch(staged.clone()))
    }

    pub fn activate_transfer_attachment_batch_at_mixed_cut(
        &self,
        store: &StateStore,
        anchors: &[AuthoritativeThreadEventReadAnchor],
        retired: &[RetiredChainDependency],
        expected_memberships: &[super::PresentationMembership],
        cut: &dyn CompletedPairedCut,
        session_id: &str,
        source_membership: &super::PresentationMembership,
        destination_membership: &super::PresentationMembership,
        prevalidated: &PrevalidatedTransferAttachmentBatch,
        export: &UiTransferExport,
        reservation: &UiTransferReservation,
        max_record_bytes: u64,
        max_attachments: usize,
        current_policy_digest: &str,
    ) -> anyhow::Result<Option<()>> {
        if !cut.publication_cut_covered_by(anchors, retired)
            || ![source_membership, destination_membership]
                .iter()
                .all(|membership| {
                    anchors
                        .iter()
                        .filter(|anchor| {
                            membership.thread_id.as_deref() == Some(anchor.thread_id())
                                && membership.chain_root_id.as_deref()
                                    == Some(anchor.chain_root_id())
                        })
                        .count()
                        == 1
                })
        {
            return Err(AttachmentStoreError::PresentationConflict.into());
        }
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        Self::validate_exact_presentation_snapshot(
            session,
            expected_memberships,
            current_policy_digest,
        )?;
        if !cut.publication_memberships_match(session) {
            return Err(AttachmentStoreError::PresentationConflict.into());
        }
        if source_membership.presentation_id == destination_membership.presentation_id
            || session
                .presentations
                .get(&source_membership.presentation_id)
                != Some(source_membership)
            || session
                .presentations
                .get(&destination_membership.presentation_id)
                != Some(destination_membership)
        {
            return Err(AttachmentStoreError::PresentationConflict.into());
        }
        for membership in [source_membership, destination_membership] {
            exact_presentation_attachment(
                session,
                &membership.presentation_id,
                &membership.root_coordinate,
                current_policy_digest,
            )?;
        }
        with_session_cut(store, anchors, retired, || {
            Self::activate_transfer_attachment_batch_in_session(
                session,
                prevalidated,
                export,
                reservation,
                max_record_bytes,
                max_attachments,
                current_policy_digest,
            )
            .map_err(anyhow::Error::new)
        })
    }

    fn activate_transfer_attachment_batch_in_session(
        session: &mut BrowserSession,
        prevalidated: &PrevalidatedTransferAttachmentBatch,
        export: &UiTransferExport,
        reservation: &UiTransferReservation,
        max_record_bytes: u64,
        max_attachments: usize,
        current_policy_digest: &str,
    ) -> Result<(), AttachmentStoreError> {
        if session.expires_at <= std::time::Instant::now() {
            return Err(AttachmentStoreError::SessionInvalid);
        }
        let staged = &prevalidated.0;
        reservation
            .validate_export(&staged.transfer, export, max_record_bytes)
            .map_err(|_| AttachmentStoreError::PresentationConflict)?;
        validate_reservation_bindings(staged, export, reservation)?;
        let destination = session
            .presentations
            .get(&staged.transfer.destination.presentation_id)
            .ok_or(AttachmentStoreError::PresentationInvalid)?;
        exact_presentation_attachment(
            session,
            &staged.transfer.destination.presentation_id,
            &destination.root_coordinate,
            current_policy_digest,
        )?;
        validate_destination_membership_target(session, &staged.transfer, reservation)?;
        let existing_membership = session
            .presentations
            .get(&staged.transfer.destination.presentation_id)
            .and_then(|presentation| {
                presentation
                    .activated_transfer_reservations
                    .get(&staged.transfer.operation_id)
            })
            .cloned();
        if let Some(existing) = &existing_membership
            && (existing.transfer != staged.transfer || existing.reservation != *reservation)
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        let pending = session
            .pending_transfer_attachments
            .get(&staged.transfer.operation_id)
            .cloned();
        if let Some(pending) = &pending {
            validate_retained_batch_identity(pending, current_policy_digest)?;
        }
        let retained = staged
            .attachments
            .iter()
            .map(|entry| {
                session
                    .attachments
                    .get(&entry.destination.binding_attachment_id)
                    .map(|current| {
                        current.coordinate() == entry.destination.coordinate()
                            && same_retained_authority(current, &entry.destination)
                    })
            })
            .collect::<Vec<_>>();
        if retained.iter().all(|entry| entry == &Some(true)) {
            if pending
                .as_ref()
                .is_some_and(|batch| !same_batch(batch, staged))
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            validate_live_batch_identity(session, staged, current_policy_digest)?;
            if existing_membership.is_none() {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            session
                .pending_transfer_attachments
                .remove(&staged.transfer.operation_id);
            return Ok(());
        }
        let Some(pending) = pending else {
            return Err(AttachmentStoreError::PresentationConflict);
        };
        if !same_batch(&pending, staged) || retained.iter().any(Option::is_some) {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        // The pending batch already occupies capacity. Publish every member
        // and remove the hidden owner in the same session lock.
        if occupied_attachment_count(session) > max_attachments {
            return Err(AttachmentStoreError::Capacity);
        }
        for entry in &pending.attachments {
            session.attachments.insert(
                entry.destination.binding_attachment_id.clone(),
                entry.destination.clone(),
            );
        }
        let coordinates = pending
            .attachments
            .iter()
            .map(|entry| entry.destination.coordinate())
            .collect::<Vec<_>>();
        let insertion_ownership = match &reservation.placement {
            ryeos_client_base::ui::transfer_reservation::UiTransferDestinationPlacement::Tabs {
                manifest,
                ..
            } => manifest
                .tabs
                .iter()
                .map(|tab| {
                    (
                        manifest.surface_instance_id.clone(),
                        tab.view_set_id,
                        private_coordinate(&tab.binding),
                    )
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let mount_ownership = reservation_mount_ownership(&reservation.mounts);
        session
            .presentations
            .get_mut(&staged.transfer.destination.presentation_id)
            .expect("validated destination presentation")
            .activated_transfer_reservations
            .insert(
                staged.transfer.operation_id.clone(),
                ActivatedTransferReservationMembership {
                    transfer: staged.transfer.clone(),
                    reservation: reservation.clone(),
                    coordinates,
                    insertion_ownership,
                    mount_ownership,
                },
            );
        session
            .pending_transfer_attachments
            .remove(&staged.transfer.operation_id);
        Ok(())
    }

    /// Reconcile activated attachment ownership against one authenticated
    /// current seat replay. The caller holds the existing seat-transition
    /// lock and reads the signed history before entering this session-map
    /// lock. Pending Accepted batches are deliberately outside this cache.
    pub fn reconcile_activated_transfer_ownership(
        &self,
        session_id: &str,
        presentation_id: &str,
        root_coordinate: &BindingAttachmentCoordinate,
        replay: &UiCompositionReplay,
    ) -> Result<usize, AttachmentStoreError> {
        if replay.composition.id().as_str() != presentation_id {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        reconcile_activated_transfer_ownership_in_session(
            session,
            presentation_id,
            root_coordinate,
            replay,
        )
    }

    /// One existing session owner and one ordered exclusion for all completed
    /// roots. The callback sees only the cleaned in-memory candidate; a full
    /// channel or failed projection leaves the original session untouched.
    pub fn with_reconciled_paired_roots_at_anchors<T>(
        &self,
        store: &StateStore,
        anchors: &[AuthoritativeThreadEventReadAnchor],
        retired: &[RetiredChainDependency],
        session_id: &str,
        expected: &[super::PresentationMembership],
        current_policy_digest: &str,
        roots: &[CompletedLiveRoot<'_>],
        action: impl FnOnce(&BrowserSession) -> anyhow::Result<Option<T>>,
    ) -> anyhow::Result<Option<T>> {
        self.with_reconciled_completed_roots_at_cut(
            store,
            anchors,
            retired,
            session_id,
            expected,
            current_policy_digest,
            roots,
            &[],
            action,
        )
    }

    /// Extend the same one-candidate session owner to complete retired-root
    /// folds. An ordinary retired root carries no fabricated transfer cut.
    pub fn with_reconciled_completed_roots_at_cut<T>(
        &self,
        store: &StateStore,
        anchors: &[AuthoritativeThreadEventReadAnchor],
        retired: &[RetiredChainDependency],
        session_id: &str,
        expected: &[super::PresentationMembership],
        current_policy_digest: &str,
        roots: &[CompletedLiveRoot<'_>],
        retired_roots: &[CompletedRetiredRootEntry<'_>],
        action: impl FnOnce(&BrowserSession) -> anyhow::Result<Option<T>>,
    ) -> anyhow::Result<Option<T>> {
        if (anchors.is_empty() && retired.is_empty())
            || (roots.is_empty() && retired_roots.is_empty())
        {
            return Err(AttachmentStoreError::PresentationConflict.into());
        }
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        if session.presentations.len() != expected.len()
            || expected
                .iter()
                .map(|membership| &membership.presentation_id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != expected.len()
            || expected.iter().any(|membership| {
                session.presentations.get(&membership.presentation_id) != Some(membership)
            })
            || roots.iter().any(|(membership, replay, cut)| {
                replay.composition.id().as_str() != membership.presentation_id
                    || session.presentations.get(&membership.presentation_id) != Some(*membership)
                    || !cut.publication_memberships_match(session)
                    || !cut.publication_cut_covered_by(anchors, retired)
                    || !anchors.iter().any(|anchor| {
                        membership.chain_root_id.as_deref() == Some(anchor.chain_root_id())
                            && membership.thread_id.as_deref() == Some(anchor.thread_id())
                            && cut.covers_seat_anchor(
                                &membership.presentation_id,
                                anchor.chain_root_id(),
                                anchor.thread_id(),
                                anchor,
                            )
                    })
            })
        {
            return Err(AttachmentStoreError::PresentationConflict.into());
        }
        let mut retired_projected = Vec::with_capacity(retired_roots.len());
        for (membership, root, dependency) in retired_roots {
            let cut = root.completed_paired_cut();
            if session.presentations.get(&membership.presentation_id) != Some(*membership)
                || membership.chain_root_id.as_deref() != Some(dependency.chain_root_id.as_str())
                || membership.thread_id.as_deref() != Some(dependency.chain_root_id.as_str())
                || !retired.iter().any(|included| included == *dependency)
                || !root.matches_completed_retired_dependency(dependency)
                || cut.is_some_and(|cut| {
                    !cut.publication_memberships_match(session)
                        || !cut.publication_cut_covered_by(anchors, retired)
                        || !cut.covers_retired_seat(
                            &membership.presentation_id,
                            &dependency.chain_root_id,
                            dependency,
                        )
                })
            {
                return Err(AttachmentStoreError::PresentationConflict.into());
            }
            let completed = root
                .completed_composition_frontier(&membership.initialization)
                .map_err(|_| AttachmentStoreError::PresentationConflict)?;
            if cut.is_none() && completed.transfer_testimony_seen {
                return Err(AttachmentStoreError::PresentationConflict.into());
            }
            retired_projected.push((*membership, completed.replay));
        }
        for membership in expected {
            require_shared_presentation_root(
                session,
                &membership.root_coordinate,
                current_policy_digest,
            )?;
        }
        let mut candidate = session.clone();
        for (membership, replay, cut) in roots {
            apply_paired_cleanup_to_candidate(&mut candidate, membership, replay)?;
            release_completed_negative_batches(&mut candidate, *cut)?;
        }
        for (membership, replay) in &retired_projected {
            apply_paired_cleanup_to_candidate(&mut candidate, membership, replay)?;
        }
        for (_, root, _) in retired_roots {
            if let Some(cut) = root.completed_paired_cut() {
                release_completed_negative_batches(&mut candidate, cut)?;
            }
            for transfer in root.settled_retired_transfers() {
                release_settled_hidden_batch(&mut candidate, transfer)?;
            }
        }
        let publish = || {
            let result = action(&candidate)?;
            if result.is_some() {
                *session = candidate;
            }
            Ok(result)
        };
        let selected = with_session_cut(store, anchors, retired, publish)?;
        Ok(selected.flatten())
    }
}

/// Called only on the private candidate behind complete paired cut validation.
/// The enclosing owner publishes it only while the exact signed heads/retired
/// prefixes still match. An abort alone never releases an Accepted batch.
fn release_completed_negative_batches(
    candidate: &mut BrowserSession,
    cut: &dyn CompletedPairedCut,
) -> anyhow::Result<()> {
    use ryeos_ui_presentation_contract::transfer::TransferPhase;
    cut.for_each_authenticated_phase(&mut |_, record, lifecycle| {
        if !matches!(record.phase, TransferPhase::Released { .. }) {
            return Ok(());
        }
        let Some(TransferPhase::Prepared { export, .. }) =
            lifecycle.prepared().map(|p| &p.record.phase)
        else {
            return Err(AttachmentStoreError::PresentationConflict.into());
        };
        let Some(TransferPhase::Accepted { reservation, .. }) =
            lifecycle.accepted().map(|p| &p.record.phase)
        else {
            return Err(AttachmentStoreError::PresentationConflict.into());
        };
        let export: UiTransferExport = serde_json::from_value(export.clone())?;
        let reservation: UiTransferReservation = serde_json::from_value(reservation.clone())?;
        release_exact_hidden_batch(candidate, &record.transfer, &export, &reservation)
    })
}

/// Drop the hidden reservation of a transfer whose seats are all retired. The
/// reservation must be the one this exact transfer made: another identity under
/// the same operation is a conflict. Activated mounts and frontiers stay.
fn release_settled_hidden_batch(
    candidate: &mut BrowserSession,
    transfer: &TransferIdentity,
) -> anyhow::Result<()> {
    match candidate
        .pending_transfer_attachments
        .get(&transfer.operation_id)
    {
        None => Ok(()),
        Some(pending) if pending.transfer != *transfer => {
            Err(AttachmentStoreError::PresentationConflict.into())
        }
        Some(_) => {
            candidate
                .pending_transfer_attachments
                .remove(&transfer.operation_id);
            Ok(())
        }
    }
}

// Mechanical candidate mutation only. The sole production caller above supplies
// the Released phase from a completed authenticated cut; this is not ingress.
fn release_exact_hidden_batch(
    candidate: &mut BrowserSession,
    transfer: &TransferIdentity,
    export: &UiTransferExport,
    reservation: &UiTransferReservation,
) -> anyhow::Result<()> {
    let Some(pending) = candidate
        .pending_transfer_attachments
        .get(&transfer.operation_id)
    else {
        return Ok(());
    };
    if pending.transfer != *transfer {
        return Err(AttachmentStoreError::PresentationConflict.into());
    }
    let membership = candidate
        .presentations
        .get(&transfer.destination.presentation_id)
        .ok_or(AttachmentStoreError::PresentationConflict)?;
    let bound = membership
        .initialization
        .binding_attachments
        .iter()
        .find(|attachment| {
            attachment.binding_attachment_id == membership.root_coordinate.binding_attachment_id
                && attachment.binding_generation == membership.root_coordinate.binding_generation
                && attachment.binding_digest == membership.root_coordinate.binding_digest
        })
        .ok_or(AttachmentStoreError::PresentationConflict)?
        .binding_request_bounds
        .max_request_bytes;
    reservation
        .validate_export(transfer, export, bound)
        .map_err(anyhow::Error::msg)?;
    validate_reservation_bindings(pending, export, reservation)?;
    // Remove only hidden reservation ownership. Activated mounts, pins, return
    // frames and allocation/generation frontiers remain untouched.
    candidate
        .pending_transfer_attachments
        .remove(&transfer.operation_id);
    Ok(())
}

fn apply_paired_cleanup_to_candidate(
    candidate: &mut BrowserSession,
    membership: &super::PresentationMembership,
    replay: &UiCompositionReplay,
) -> Result<(), AttachmentStoreError> {
    exact_shared_presentation_root(candidate, &membership.root_coordinate)?;
    for surface in membership
        .surfaces
        .values()
        .filter(|surface| !surface.closed)
    {
        let Some(closure) = replay.closed_surfaces.get(&surface.surface_instance_id) else {
            continue;
        };
        let coordinate = BindingAttachmentCoordinate {
            binding_attachment_id: closure.binding.binding_attachment_id.clone(),
            binding_generation: closure.binding.binding_generation,
            binding_digest: closure.binding.binding_digest.clone(),
        };
        if closure.presentation_id != membership.presentation_id
            || closure.surface_instance_id != surface.surface_instance_id
            || coordinate != surface.coordinate
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        exact_attachment(candidate, &coordinate)?;
        candidate
            .presentations
            .get_mut(&membership.presentation_id)
            .expect("validated paired presentation")
            .surfaces
            .get_mut(&surface.surface_instance_id)
            .expect("validated paired Surface")
            .closed = true;
        retire_surface_transfer_membership(
            candidate,
            &membership.presentation_id,
            &surface.surface_instance_id,
        );
    }
    reconcile_activated_transfer_ownership_in_session(
        candidate,
        &membership.presentation_id,
        &membership.root_coordinate,
        replay,
    )?;
    Ok(())
}

fn reconcile_activated_transfer_ownership_in_session(
    session: &mut BrowserSession,
    presentation_id: &str,
    root_coordinate: &BindingAttachmentCoordinate,
    replay: &UiCompositionReplay,
) -> Result<usize, AttachmentStoreError> {
    let membership = session
        .presentations
        .get(presentation_id)
        .filter(|membership| membership.root_coordinate == *root_coordinate)
        .ok_or(AttachmentStoreError::PresentationConflict)?;
    exact_attachment(session, &membership.root_coordinate)?;
    let presentation = session
        .presentations
        .get_mut(presentation_id)
        .expect("validated presentation membership");
    let mut candidates = Vec::new();
    let mut retired = 0usize;
    for ownership in presentation.activated_transfer_reservations.values_mut() {
        candidates.extend(ownership.coordinates.iter().cloned());
        retired += ownership.retain_current_edges(
            |instance, coordinate| {
                replay
                    .composition
                    .mount(instance)
                    .is_some_and(|mount| private_coordinate(mount.binding()) == *coordinate)
                    || replay.lens_frames.get(instance).is_some_and(|frames| {
                        frames
                            .iter()
                            .any(|frame| private_coordinate(&frame.binding) == *coordinate)
                    })
            },
            |surface, set, coordinate| {
                replay.composition.view_set(surface, set).is_some()
                    && replay
                        .insertion_bindings
                        .get(&set)
                        .is_some_and(|binding| private_coordinate(binding) == *coordinate)
            },
        );
    }
    presentation
        .activated_transfer_reservations
        .retain(|_, ownership| !ownership.coordinates.is_empty());
    for coordinate in candidates {
        remove_attachment_if_unowned(session, &coordinate);
    }
    retired += super::surface_opening::reconcile_tab_openings(session, presentation_id, replay)?;
    Ok(retired)
}

/// Associate the full return-frame closure with its mounted instance. Batch
/// retention alone cannot release these references when that mount retires.
/// Repeated exact binding references share one edge, matching staging dedup.
fn reservation_mount_ownership(
    mounts: &[ryeos_client_base::ui::transfer_reservation::UiTransferMountAllocation],
) -> Vec<(
    ryeos_client_base::ids::RyeOsViewInstanceKey,
    BindingAttachmentCoordinate,
)> {
    let mut ownership = Vec::new();
    for mount in mounts {
        for binding in std::iter::once(&mount.binding)
            .chain(mount.lens_frames.iter().map(|frame| &frame.binding))
        {
            let edge = (
                mount.destination_instance.clone(),
                private_coordinate(binding),
            );
            if !ownership.contains(&edge) {
                ownership.push(edge);
            }
        }
    }
    ownership
}

fn validate_destination_membership_target(
    session: &BrowserSession,
    transfer: &TransferIdentity,
    reservation: &UiTransferReservation,
) -> Result<(), AttachmentStoreError> {
    use ryeos_client_base::ui::transfer_reservation::UiTransferDestinationPlacement;
    let presentation = session
        .presentations
        .get(&transfer.destination.presentation_id)
        .ok_or(AttachmentStoreError::PresentationInvalid)?;
    let target = match &reservation.placement {
        UiTransferDestinationPlacement::Tabs { manifest, .. } => {
            Some(manifest.surface_instance_id.as_str())
        }
        UiTransferDestinationPlacement::View {
            surface_instance_id,
            ..
        } => Some(surface_instance_id.as_str()),
        UiTransferDestinationPlacement::SharedDock { .. } => None,
    };
    if target.is_some_and(|id| {
        presentation
            .surfaces
            .get(id)
            .is_none_or(|surface| surface.closed)
    }) {
        return Err(AttachmentStoreError::PresentationInvalid);
    }
    Ok(())
}

pub(super) fn retire_surface_transfer_membership(
    session: &mut BrowserSession,
    presentation_id: &str,
    surface_instance_id: &str,
) {
    let removed = {
        let Some(presentation) = session.presentations.get_mut(presentation_id) else {
            return;
        };
        let operation_ids = presentation
            .activated_transfer_reservations
            .iter()
            .filter(|(_, membership)| membership.targets_surface(surface_instance_id))
            .map(|(operation_id, _)| operation_id.clone())
            .collect::<Vec<_>>();
        operation_ids
            .into_iter()
            .filter_map(|operation_id| {
                presentation
                    .activated_transfer_reservations
                    .remove(&operation_id)
            })
            .collect::<Vec<_>>()
    };
    for coordinate in removed
        .into_iter()
        .flat_map(|membership| membership.coordinates)
    {
        remove_attachment_if_unowned(session, &coordinate);
    }
}

pub(super) fn remove_attachment_if_unowned(
    session: &mut BrowserSession,
    coordinate: &BindingAttachmentCoordinate,
) {
    let still_owned = session.presentations.values().any(|presentation| {
        presentation.root_coordinate == *coordinate
            || presentation
                .surfaces
                .values()
                .any(|surface| surface.owns_coordinate(coordinate))
            || presentation
                .activated_transfer_reservations
                .values()
                .any(|reservation| reservation.owns_coordinate(coordinate))
    });
    if !still_owned {
        session
            .attachments
            .remove(&coordinate.binding_attachment_id);
    }
}

fn same_batch(left: &StagedTransferAttachmentBatch, right: &StagedTransferAttachmentBatch) -> bool {
    left.transfer == right.transfer
        && left.generation_start == right.generation_start
        && left.next_generation == right.next_generation
        && left.attachments.len() == right.attachments.len()
        && left
            .attachments
            .iter()
            .zip(&right.attachments)
            .all(|(left, right)| {
                left.source == right.source
                    && left.destination.coordinate() == right.destination.coordinate()
                    && same_retained_authority(&left.destination, &right.destination)
            })
}

fn same_retained_authority(
    left: &AdmittedBindingAttachment,
    right: &AdmittedBindingAttachment,
) -> bool {
    left.compiled_binding == right.compiled_binding
        && left.presentation == right.presentation
        && left.registered_project_id == right.registered_project_id
        && left.root_ref == right.root_ref
        && left.project_query_identity == right.project_query_identity
        && match (&left.project_authority, &right.project_authority) {
            (None, None) => true,
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
}

fn validate_reservation_bindings(
    staged: &StagedTransferAttachmentBatch,
    export: &UiTransferExport,
    reservation: &UiTransferReservation,
) -> Result<(), AttachmentStoreError> {
    if reservation.mounts.len() != export.mounts().len() {
        return Err(AttachmentStoreError::PresentationConflict);
    }
    for (source, reserved) in export.mounts().into_iter().zip(&reservation.mounts) {
        if source.source_instance != reserved.source_instance
            || staged.destination_binding(&source.binding).as_ref() != Some(&reserved.binding)
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
    }
    if staged.attachments.iter().any(|entry| {
        reservation_binding_pairs(export, reservation).map_or(true, |pairs| {
            !pairs.iter().any(|(_, destination)| {
                *destination == &public_coordinate(&entry.destination.coordinate())
            })
        })
    }) {
        return Err(AttachmentStoreError::PresentationConflict);
    }
    for (source, destination) in reservation_binding_pairs(export, reservation)? {
        if staged.destination_binding(source).as_ref() != Some(destination) {
            return Err(AttachmentStoreError::PresentationConflict);
        }
    }
    Ok(())
}

fn export_source_bindings(export: &UiTransferExport) -> Vec<&UiBindingAttachmentCoordinate> {
    let mut bindings = Vec::new();
    if let Some(sets) = export.sets() {
        bindings.extend(sets.iter().map(|set| &set.binding));
    }
    for mount in export.mounts() {
        bindings.push(&mount.binding);
        bindings.extend(mount.lens_frames.iter().map(|frame| &frame.binding));
    }
    bindings
}

fn reservation_binding_pairs<'a>(
    export: &'a UiTransferExport,
    reservation: &'a UiTransferReservation,
) -> Result<
    Vec<(
        &'a UiBindingAttachmentCoordinate,
        &'a UiBindingAttachmentCoordinate,
    )>,
    AttachmentStoreError,
> {
    let mut pairs = Vec::new();
    if let Some(sets) = export.sets() {
        let ryeos_client_base::ui::transfer_reservation::UiTransferDestinationPlacement::Tabs {
            manifest,
            ..
        } = &reservation.placement
        else {
            return Err(AttachmentStoreError::PresentationConflict);
        };
        if sets.len() != manifest.tabs.len() {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        pairs.extend(
            sets.iter()
                .zip(&manifest.tabs)
                .map(|(set, tab)| (&set.binding, &tab.binding)),
        );
    }
    if export.mounts().len() != reservation.mounts.len() {
        return Err(AttachmentStoreError::PresentationConflict);
    }
    for (source, destination) in export.mounts().into_iter().zip(&reservation.mounts) {
        if source.lens_frames.len() != destination.lens_frames.len() {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        pairs.push((&source.binding, &destination.binding));
        pairs.extend(
            source
                .lens_frames
                .iter()
                .zip(&destination.lens_frames)
                .map(|(source, destination)| (&source.binding, &destination.binding)),
        );
    }
    Ok(pairs)
}

fn validate_retained_attachment(
    attachment: &AdmittedBindingAttachment,
    current_policy_digest: &str,
) -> Result<(), AttachmentStoreError> {
    if attachment
        .compiled_binding
        .binding
        .node_policy_generation_digest
        != current_policy_digest
    {
        return Err(AttachmentStoreError::PolicyStale);
    }
    super::validate_retained_project_path(attachment)
}

fn validate_retained_batch(
    batch: &StagedTransferAttachmentBatch,
    current_policy_digest: &str,
) -> Result<(), AttachmentStoreError> {
    for entry in &batch.attachments {
        validate_retained_attachment(&entry.destination, current_policy_digest)?;
    }
    Ok(())
}

fn validate_retained_source_admissions(
    session: &BrowserSession,
    batch: &StagedTransferAttachmentBatch,
    policy: &str,
) -> Result<(), AttachmentStoreError> {
    for entry in &batch.attachments {
        let source = exact_presentation_attachment(
            session,
            &batch.transfer.source.presentation_id,
            &entry.source,
            policy,
        )?;
        validate_retained_attachment(&source, policy)?;
        if !same_retained_authority(&source, &entry.destination) {
            return Err(AttachmentStoreError::PresentationConflict);
        }
    }
    Ok(())
}

fn validate_retained_batch_identity(
    batch: &StagedTransferAttachmentBatch,
    current_policy_digest: &str,
) -> Result<(), AttachmentStoreError> {
    if batch.attachments.iter().any(|entry| {
        entry
            .destination
            .compiled_binding
            .binding
            .node_policy_generation_digest
            != current_policy_digest
    }) {
        return Err(AttachmentStoreError::PolicyStale);
    }
    Ok(())
}

fn validate_live_batch_identity(
    session: &BrowserSession,
    batch: &StagedTransferAttachmentBatch,
    current_policy_digest: &str,
) -> Result<(), AttachmentStoreError> {
    for entry in &batch.attachments {
        let retained = exact_presentation_attachment(
            session,
            &batch.transfer.destination.presentation_id,
            &entry.destination.coordinate(),
            current_policy_digest,
        )?;
        if !same_retained_authority(&retained, &entry.destination) {
            return Err(AttachmentStoreError::PresentationConflict);
        }
    }
    Ok(())
}

fn candidate_from(source: &AdmittedBindingAttachment) -> BindingAttachmentCandidate {
    BindingAttachmentCandidate {
        registered_project_id: source.registered_project_id.clone(),
        compiled_binding: source.compiled_binding.clone(),
        presentation: source.presentation.clone(),
        project_authority: source.project_authority.clone(),
    }
}

fn private_coordinate(coordinate: &UiBindingAttachmentCoordinate) -> BindingAttachmentCoordinate {
    BindingAttachmentCoordinate {
        binding_attachment_id: coordinate.binding_attachment_id.clone(),
        binding_generation: coordinate.binding_generation,
        binding_digest: coordinate.binding_digest.clone(),
    }
}

fn public_coordinate(coordinate: &BindingAttachmentCoordinate) -> UiBindingAttachmentCoordinate {
    UiBindingAttachmentCoordinate {
        binding_attachment_id: coordinate.binding_attachment_id.clone(),
        binding_generation: coordinate.binding_generation,
        binding_digest: coordinate.binding_digest.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::ids::{PresentationId, SurfaceInstanceId};
    use ryeos_client_base::surface::view_sets::ViewSetSeedSpec;
    use ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest;
    use ryeos_client_base::ui::transfer_reservation::{
        UiTransferDestinationPlacement, UiTransferSetAllocation,
    };
    use ryeos_ui_presentation_contract::transfer::TransferSeat;

    fn staged_batch() -> StagedTransferAttachmentBatch {
        let context = super::super::tests::test_context();
        let attachment = Arc::new(
            build_attachment(
                "destination-binding".into(),
                7,
                BindingAttachmentCandidate {
                    registered_project_id: None,
                    compiled_binding: context.compiled_binding,
                    presentation: context.presentation,
                    project_authority: None,
                },
                "fp:test",
                &"22".repeat(32),
            )
            .unwrap(),
        );
        let seat = |id: &str| TransferSeat {
            presentation_id: id.into(),
            thread_id: id.into(),
            chain_root_id: id.into(),
            producer_incarnation: "a".repeat(64),
            revision: 0,
        };
        StagedTransferAttachmentBatch {
            transfer: TransferIdentity {
                operation_id: "11111111-1111-4111-8111-111111111111".into(),
                source: seat("22222222-2222-4222-8222-222222222222"),
                destination: seat("33333333-3333-4333-8333-333333333333"),
                export_digest: "b".repeat(64),
            },
            generation_start: 7,
            next_generation: 8,
            attachments: vec![StagedTransferAttachment {
                source: BindingAttachmentCoordinate {
                    binding_attachment_id: "source-binding".into(),
                    binding_generation: 3,
                    binding_digest: "c".repeat(64),
                },
                destination: attachment,
            }],
        }
    }

    #[test]
    fn exact_hidden_retry_does_not_accept_changed_retained_authority() {
        let original = staged_batch();
        assert!(same_batch(&original, &original.clone()));
        // A committed source can already be retired while the independently
        // admitted destination remains retained privately. This test isolates
        // the checks; it does not manufacture a completed commit proof.
        let store = BrowserSessionStore::new();
        let (id, token, _) = store
            .mint_token(super::super::tests::test_context(), 4, &"22".repeat(32))
            .unwrap();
        store.consume_launch_token(&token).unwrap();
        let session = store.get_session(&id).unwrap();
        assert!(
            validate_retained_source_admissions(&session, &original, &"22".repeat(32)).is_err()
        );
        validate_retained_batch(&original, &"22".repeat(32)).unwrap();
        assert!(validate_retained_batch(&original, &"44".repeat(32)).is_err());
        assert!(Arc::ptr_eq(
            original.destination_admissions().next().unwrap(),
            &original.attachments[0].destination
        ));
        let mut changed = original.clone();
        Arc::make_mut(&mut changed.attachments[0].destination).root_ref = "another-root".into();
        assert!(!same_batch(&original, &changed));
        let mut changed = original.clone();
        Arc::make_mut(&mut changed.attachments[0].destination).project_query_identity =
            Some("another-query".into());
        assert!(!same_batch(&original, &changed));
        let mut changed = original.clone();
        let binding = &mut Arc::make_mut(&mut changed.attachments[0].destination).compiled_binding;
        Arc::make_mut(binding).posture =
            crate::compiled_binding::EffectiveUiPosture::ObservationOnly;
        assert!(!same_batch(&original, &changed));
    }

    #[test]
    fn a_settled_retired_transfer_releases_the_hidden_reservation_it_made() {
        let batch = staged_batch();
        let store = BrowserSessionStore::new();
        let (id, token, _) = store
            .mint_token(super::super::tests::test_context(), 4, &"22".repeat(32))
            .unwrap();
        store.consume_launch_token(&token).unwrap();
        let mut candidate = store.get_session(&id).unwrap();
        let transfer = batch.transfer.clone();
        candidate
            .pending_transfer_attachments
            .insert(transfer.operation_id.clone(), batch);

        // Another identity under the same operation is not this reservation.
        let mut other = transfer.clone();
        other.export_digest = "d".repeat(64);
        assert!(release_settled_hidden_batch(&mut candidate, &other).is_err());
        assert!(
            candidate
                .pending_transfer_attachments
                .contains_key(&transfer.operation_id)
        );

        release_settled_hidden_batch(&mut candidate, &transfer).unwrap();
        assert!(candidate.pending_transfer_attachments.is_empty());
        // A transfer with no reservation left is already settled.
        release_settled_hidden_batch(&mut candidate, &transfer).unwrap();
    }

    #[test]
    fn mount_ownership_includes_distinct_lens_bindings_without_duplicate_edges() {
        use ryeos_client_base::ids::{RyeOsViewInstanceKey, TileId};
        use ryeos_client_base::ui::attachment::SelectionAttachment;
        use ryeos_client_base::ui::transfer_reservation::{
            UiTransferLensFrameAllocation, UiTransferMountAllocation,
        };
        let presentation = PresentationId::try_new("destination").unwrap();
        let surface = SurfaceInstanceId::try_new("surface").unwrap();
        let instance = RyeOsViewInstanceKey::surface_tile(&presentation, &surface, TileId::new(1));
        let subject = SelectionAttachment::RequiredSubject {
            input: "subject".into(),
            facets: vec!["selection.work".into()],
        };
        let binding = |id: &str| UiBindingAttachmentCoordinate {
            binding_attachment_id: id.into(),
            binding_generation: 1,
            binding_digest: "ab".repeat(32),
        };
        let mount = UiTransferMountAllocation {
            source_instance: instance.clone(),
            destination_instance: instance.clone(),
            binding: binding("current"),
            destination_subject: subject.clone(),
            lens_frames: vec![
                UiTransferLensFrameAllocation {
                    binding: binding("return"),
                    destination_subject: subject.clone(),
                },
                UiTransferLensFrameAllocation {
                    binding: binding("current"),
                    destination_subject: subject,
                },
            ],
        };
        assert_eq!(
            reservation_mount_ownership(&[mount]),
            vec![
                (instance.clone(), private_coordinate(&binding("current"))),
                (instance, private_coordinate(&binding("return"))),
            ]
        );
    }

    #[test]
    fn activated_membership_owns_exact_coordinates_and_targets_its_surface() {
        let coordinate = BindingAttachmentCoordinate {
            binding_attachment_id: "destination-binding".into(),
            binding_generation: 7,
            binding_digest: "d".repeat(64),
        };
        let public = public_coordinate(&coordinate);
        let presentation_id =
            PresentationId::try_new("22222222-2222-4222-8222-222222222222").unwrap();
        let surface_id = SurfaceInstanceId::try_new("destination-surface").unwrap();
        let seed: ViewSetSeedSpec = serde_json::from_value(serde_json::json!({
            "schema": 2, "id": "empty", "title": "Empty", "root": null
        }))
        .unwrap();
        let manifest = UiSurfaceMountManifest::allocate(
            presentation_id.clone(),
            surface_id.clone(),
            10,
            vec![(seed, public)],
            Some("empty"),
        )
        .unwrap();
        let destination_view_set_id = manifest.tabs[0].view_set_id;
        let descriptor = serde_json::from_value(serde_json::json!({
            "binding_attachment_id": coordinate.binding_attachment_id,
            "binding_generation": coordinate.binding_generation,
            "binding_digest": coordinate.binding_digest,
            "root_ref": "surface:test", "root_generation": "generation-one",
            "presentation": {"definition": {"kind": "surface", "surface": {"name": "test"}}, "views": {}},
            "project_path": null, "subject": {"kind":"projectless"},
            "posture": "observation_only",
            "binding_request_bounds": {"max_request_bytes": 4096, "max_input_bytes": 1024, "max_seat_append_bytes": 65536}
        })).unwrap();
        let reservation = UiTransferReservation {
            schema:
                ryeos_ui_presentation_contract::initialization::PRESENTATION_INITIALIZATION_SCHEMA,
            presentation_id,
            expected_revision: 4,
            allocation_start: 10,
            next_mount_id: manifest.next_mount_id,
            placement: UiTransferDestinationPlacement::Tabs {
                manifest,
                sets: vec![UiTransferSetAllocation {
                    source_handle: "empty".into(),
                    source_view_set_id: ryeos_client_base::ids::ViewSetId::new(2),
                    destination_view_set_id,
                    focused_tile_id: None,
                }],
                active_view_set_id: Some(destination_view_set_id),
            },
            mounts: vec![],
            descriptors: vec![descriptor],
        };
        let seat = |presentation_id: &str, thread_id: &str| TransferSeat {
            presentation_id: presentation_id.into(),
            thread_id: thread_id.into(),
            chain_root_id: thread_id.into(),
            producer_incarnation: "a".repeat(64),
            revision: 4,
        };
        let mount_coordinate = BindingAttachmentCoordinate {
            binding_attachment_id: "mounted-binding".into(),
            binding_generation: 8,
            binding_digest: "e".repeat(64),
        };
        let replacement_coordinate = BindingAttachmentCoordinate {
            binding_attachment_id: "replacement-binding".into(),
            binding_generation: 9,
            binding_digest: "f".repeat(64),
        };
        let mount_instance = ryeos_client_base::ids::RyeOsViewInstanceKey::shared_dock(
            &PresentationId::try_new("22222222-2222-4222-8222-222222222222").unwrap(),
            "left",
        )
        .unwrap();
        let sibling_mount_instance = ryeos_client_base::ids::RyeOsViewInstanceKey::shared_dock(
            &PresentationId::try_new("22222222-2222-4222-8222-222222222222").unwrap(),
            "right",
        )
        .unwrap();
        let mut membership = ActivatedTransferReservationMembership {
            transfer: TransferIdentity {
                operation_id: "11111111-1111-4111-8111-111111111111".into(),
                source: seat("11111111-1111-4111-8111-111111111111", "source-thread"),
                destination: seat("22222222-2222-4222-8222-222222222222", "destination-thread"),
                export_digest: "b".repeat(64),
            },
            reservation,
            coordinates: vec![
                coordinate.clone(),
                mount_coordinate.clone(),
                replacement_coordinate.clone(),
            ],
            insertion_ownership: vec![(
                ryeos_client_base::ids::SurfaceInstanceId::try_new(surface_id.as_str()).unwrap(),
                ryeos_client_base::ids::ViewSetId::new(1),
                coordinate.clone(),
            )],
            mount_ownership: vec![
                (mount_instance.clone(), mount_coordinate.clone()),
                (mount_instance.clone(), replacement_coordinate.clone()),
                (sibling_mount_instance.clone(), mount_coordinate.clone()),
            ],
        };
        assert!(membership.owns_coordinate(&coordinate));
        assert_eq!(
            membership
                .current_coordinates()
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                coordinate.clone(),
                mount_coordinate.clone(),
                replacement_coordinate.clone(),
            ]
        );
        assert!(membership.targets_surface(surface_id.as_str()));
        let mut other = coordinate.clone();
        other.binding_generation += 1;
        assert!(!membership.owns_coordinate(&other));
        let retired = membership.retire_mounts(&[ActivatedTransferMountPredecessor {
            view_instance_key: mount_instance,
            binding: mount_coordinate.clone(),
        }]);
        assert!(retired.is_empty());
        assert!(membership.owns_coordinate(&mount_coordinate));
        assert!(membership.owns_coordinate(&replacement_coordinate));
        let retired = membership.retire_mounts(&[ActivatedTransferMountPredecessor {
            view_instance_key: sibling_mount_instance,
            binding: mount_coordinate.clone(),
        }]);
        assert_eq!(retired, vec![mount_coordinate.clone()]);
        assert!(!membership.owns_coordinate(&mount_coordinate));
        assert!(membership.owns_coordinate(&replacement_coordinate));
        assert!(membership.owns_coordinate(&coordinate));

        let unchanged = membership.clone();
        assert_eq!(
            membership.retain_current_edges(
                |instance, binding| unchanged
                    .mount_ownership
                    .contains(&(instance.clone(), binding.clone())),
                |surface, set, binding| unchanged.insertion_ownership.contains(&(
                    surface.clone(),
                    set,
                    binding.clone()
                )),
            ),
            0
        );
        let mut reassociated_tab = membership.clone();
        assert_eq!(
            reassociated_tab.retain_current_edges(
                |_, _| true,
                |_, set, _| set == ryeos_client_base::ids::ViewSetId::new(2),
            ),
            1
        );
        assert!(!reassociated_tab.owns_coordinate(&coordinate));
        assert!(
            !reassociated_tab
                .current_coordinates()
                .any(|owned| owned == &coordinate)
        );
        assert!(reassociated_tab.owns_coordinate(&replacement_coordinate));
        // A surviving exact insertion edge keeps even an otherwise empty tab
        // attachment; replacing the mount coordinate retires only that mount.
        assert_eq!(
            membership.retain_current_edges(|_, _| false, |_, _, _| true),
            1
        );
        assert!(!membership.owns_coordinate(&replacement_coordinate));
        assert!(membership.owns_coordinate(&coordinate));
        assert_eq!(
            membership.retain_current_edges(|_, _| false, |_, _, _| false),
            1
        );
        assert!(!membership.owns_coordinate(&coordinate));
        assert_eq!(
            membership.retain_current_edges(|_, _| false, |_, _, _| false),
            0
        );
    }
}
