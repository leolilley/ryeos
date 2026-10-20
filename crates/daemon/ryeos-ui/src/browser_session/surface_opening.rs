//! Atomic all-tab admission in the existing presentation membership owner.
use super::*;
use ryeos_client_base::surface::opening::{SurfaceOpenPlacement, SurfaceOpenRequest};
use ryeos_client_base::ui::{
    binding::{
        UiBindingRequestBounds, UiSurfaceTabsAdmissionKind, UiSurfaceTabsAdmissionTransition,
    },
    composition_replay::UiCompositionReplay,
};

#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceTabsOpeningReceipt {
    pub result: UiSurfaceTabsAdmissionTransition,
    pub current_coordinates: Vec<BindingAttachmentCoordinate>,
}

pub(super) fn reconcile_tab_openings(
    session: &mut BrowserSession,
    presentation_id: &str,
    replay: &UiCompositionReplay,
) -> Result<usize, AttachmentStoreError> {
    let member = session
        .presentations
        .get_mut(presentation_id)
        .ok_or(AttachmentStoreError::PresentationInvalid)?;
    let mut used = Vec::new();
    for (_, set) in replay.composition.view_sets() {
        if let Some(binding) = replay.insertion_bindings.get(&set.id) {
            used.push(binding.clone());
        }
        for tile in set.tiles.values() {
            if let Some(mount) = replay.composition.mount(&tile.instance_key) {
                used.push(mount.binding().clone());
            }
        }
    }
    for dock in replay.composition.docks().values() {
        used.push(dock.view().binding().clone());
    }
    for dialog in replay.composition.dialogs().values() {
        used.push(dialog.view().binding().clone());
    }
    for frames in replay.lens_frames.values() {
        used.extend(frames.iter().map(|frame| frame.binding.clone()));
    }
    let mut removed = Vec::new();
    for surface in member.surfaces.values_mut() {
        for (id, receipt) in &mut surface.tab_openings {
            if !replay.settled.contains(id) && !surface.closed {
                continue;
            }
            receipt.current_coordinates.retain(|coordinate| {
                let keep = used
                    .iter()
                    .any(|binding| *binding == public_coordinate(coordinate));
                if !keep {
                    removed.push(coordinate.clone());
                }
                keep
            });
        }
    }
    let count = removed.len();
    for coordinate in removed {
        super::transfer_reservation::remove_attachment_if_unowned(session, &coordinate);
    }
    Ok(count)
}

impl BrowserSessionStore {
    pub fn publish_surface_tabs(
        &self,
        session_id: &str,
        presentation_id: &str,
        root_coordinate: &BindingAttachmentCoordinate,
        opening: &SurfaceOpenRequest,
        replay: &UiCompositionReplay,
        origin: BindingAttachmentCandidate,
        tabs: Vec<BindingAttachmentCandidate>,
        admission: Option<&super::admission::SavedSurfaceAdmission>,
        limits: &PresentationLimits,
        bounds: UiBindingRequestBounds,
        policy: &str,
        validate_publication: impl FnOnce(
            &UiSurfaceTabsAdmissionTransition,
        ) -> Result<(), AttachmentStoreError>,
    ) -> Result<UiSurfaceTabsAdmissionTransition, AttachmentStoreError> {
        opening
            .validate()
            .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
        let SurfaceOpenPlacement::AppendTabs {
            destination_surface_instance_id,
            destination_binding,
            expected_composition_revision,
            expected_next_mount_id,
            insertion_index,
            ..
        } = &opening.placement
        else {
            return Err(AttachmentStoreError::PresentationInvalid);
        };
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        let root =
            exact_presentation_attachment(session, presentation_id, root_coordinate, policy)?;
        let member = session
            .presentations
            .get(presentation_id)
            .ok_or(AttachmentStoreError::PresentationInvalid)?;
        let surface = member
            .surfaces
            .get(destination_surface_instance_id.as_str())
            .filter(|surface| {
                !surface.closed && public_coordinate(&surface.coordinate) == *destination_binding
            })
            .ok_or(AttachmentStoreError::PresentationConflict)?;
        if member.root_coordinate != *root_coordinate
            || replay.composition.id().as_str() != presentation_id
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        let current_generation = &root
            .compiled_binding
            .binding
            .request_engine_generation_identity;
        let origin_matches = match admission {
            Some(admission) => admission.matches_origin(&session.principal_id, &origin),
            None => {
                origin.compiled_binding.binding.project_root
                    == root.compiled_binding.binding.project_root
                    && origin.registered_project_id == root.registered_project_id
                    && match (&origin.project_authority, &root.project_authority) {
                        (None, None) => true,
                        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                        _ => false,
                    }
            }
        };
        if !origin_matches
            || &origin
                .compiled_binding
                .binding
                .request_engine_generation_identity
                != current_generation
            || admission.is_none() && !tabs.is_empty()
            || admission.is_some_and(|admission| admission.receipt().tab_count() != tabs.len())
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        let mut generation = session.next_attachment_generation;
        let origin = Arc::new(build_attachment(
            uuid::Uuid::new_v4().to_string(),
            generation,
            origin,
            &session.principal_id,
            policy,
        )?);
        generation = generation
            .checked_add(1)
            .ok_or(AttachmentStoreError::Capacity)?;
        let mut admitted_tabs = Vec::with_capacity(tabs.len());
        for (index, candidate) in tabs.into_iter().enumerate() {
            if &candidate
                .compiled_binding
                .binding
                .request_engine_generation_identity
                != current_generation
                || !admission.is_some_and(|admission| {
                    admission.matches_tab(index, &session.principal_id, &candidate)
                })
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            let attachment = Arc::new(build_attachment(
                uuid::Uuid::new_v4().to_string(),
                generation,
                candidate,
                &session.principal_id,
                policy,
            )?);
            if attachment.presentation.definition.surface().is_none() {
                return Err(AttachmentStoreError::PresentationInvalid);
            }
            admitted_tabs.push(attachment);
            generation = generation
                .checked_add(1)
                .ok_or(AttachmentStoreError::Capacity)?;
        }
        // Unknown-outcome retries return the original complete receipt, never
        // allocate a second batch or borrow a now-focused destination.
        if let Some(retained) = surface.tab_openings.get(&opening.creation_request_id) {
            let retained = &retained.result;
            if &retained.opening != opening || retained.tab_attachments.len() != admitted_tabs.len()
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            for (descriptor, fresh) in std::iter::once((&retained.origin_attachment, &origin))
                .chain(retained.tab_attachments.iter().zip(&admitted_tabs))
            {
                let coordinate = BindingAttachmentCoordinate {
                    binding_attachment_id: descriptor.binding_attachment_id.clone(),
                    binding_generation: descriptor.binding_generation,
                    binding_digest: descriptor.binding_digest.clone(),
                };
                let old = exact_attachment(session, &coordinate)?;
                validate_retained_project_path(&old)?;
                if old.compiled_binding.binding != fresh.compiled_binding.binding
                    || old.presentation != fresh.presentation
                    || old.registered_project_id != fresh.registered_project_id
                {
                    return Err(AttachmentStoreError::PresentationConflict);
                }
            }
            retained
                .validate()
                .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
            validate_publication(retained)?;
            return Ok(retained.clone());
        }
        let current = replay
            .composition
            .surfaces()
            .get(destination_surface_instance_id)
            .filter(|surface| surface.binding() == destination_binding)
            .ok_or(AttachmentStoreError::PresentationConflict)?;
        if replay.revision != *expected_composition_revision
            || replay.next_mount_id != *expected_next_mount_id
            || member.next_mount_id != *expected_next_mount_id
            || *insertion_index > current.view_sets().len()
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        let composition = admission
            .map(|admission| {
                let receipt = admission.receipt();
                Ok::<_, AttachmentStoreError>(
                    SavedSurfaceReceipt {
                        provenance: receipt.provenance.clone(),
                        title: receipt.title.clone(),
                        origin: receipt.origin.clone(),
                        active_view_set_id: receipt.active_view_set_id.clone(),
                        body: receipt.admitted_body(
                            &admitted_tabs
                                .iter()
                                .map(|tab| tab.coordinate())
                                .collect::<Vec<_>>(),
                        )?,
                    }
                    .public_descriptor(),
                )
            })
            .transpose()?;
        let seeds = match &composition {
            Some(composition) => composition.mount_seeds(),
            None => origin
                .presentation
                .definition
                .surface()
                .ok_or(AttachmentStoreError::PresentationInvalid)?
                .to_mount_seeds()
                .map_err(|_| AttachmentStoreError::PresentationInvalid)?
                .into_iter()
                .map(|seed| (seed, public_coordinate(&origin.coordinate())))
                .collect(),
        };
        if seeds.is_empty()
            || current
                .view_sets()
                .len()
                .checked_add(seeds.len())
                .is_none_or(|count| count > ryeos_client_base::surface::view_sets::MAX_VIEW_SETS)
            || transfer_reservation::occupied_attachment_count(session)
                .checked_add(1 + admitted_tabs.len())
                .is_none_or(|count| count > limits.max_attachments)
        {
            return Err(AttachmentStoreError::Capacity);
        }
        let active = composition
            .as_ref()
            .map(|composition| composition.active_view_set_id.as_str())
            .or_else(|| seeds.first().map(|(seed, _)| seed.id.as_str()));
        let manifest = allocate_mount_manifest(
            presentation_id,
            destination_surface_instance_id.as_str(),
            *expected_next_mount_id,
            seeds.clone(),
            active,
        )?;
        let result = UiSurfaceTabsAdmissionTransition {
            kind: UiSurfaceTabsAdmissionKind::ImportSurfaceTabs,
            presentation_id: presentation_id.into(),
            creation_request_id: opening.creation_request_id.clone(),
            opening: opening.clone(),
            composition,
            origin_attachment: origin.public_descriptor(bounds),
            tab_attachments: admitted_tabs
                .iter()
                .map(|tab| tab.public_descriptor(bounds))
                .collect(),
            manifest,
        };
        result
            .validate()
            .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
        validate_publication(&result)?;
        session
            .attachments
            .insert(origin.binding_attachment_id.clone(), origin);
        for tab in admitted_tabs {
            session
                .attachments
                .insert(tab.binding_attachment_id.clone(), tab);
        }
        let member = session
            .presentations
            .get_mut(presentation_id)
            .expect("checked member");
        member.next_mount_id = result.manifest.next_mount_id;
        member
            .surfaces
            .get_mut(destination_surface_instance_id.as_str())
            .expect("checked destination")
            .tab_openings
            .insert(
                opening.creation_request_id.clone(),
                SurfaceTabsOpeningReceipt {
                    current_coordinates: std::iter::once(&result.origin_attachment)
                        .chain(&result.tab_attachments)
                        .map(|descriptor| BindingAttachmentCoordinate {
                            binding_attachment_id: descriptor.binding_attachment_id.clone(),
                            binding_generation: descriptor.binding_generation,
                            binding_digest: descriptor.binding_digest.clone(),
                        })
                        .collect(),
                    result: result.clone(),
                },
            );
        session.next_attachment_generation = generation;
        Ok(result)
    }
}
