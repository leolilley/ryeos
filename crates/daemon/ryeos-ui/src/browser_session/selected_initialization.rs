//! Selected document admission stages one complete child under the existing
//! session lock. It never claims a producer or publishes configured defaults.
use super::*;
use ryeos_client_base::surface::opening::{SurfaceOpenPlacement, SurfaceOpenRequest};
use ryeos_client_base::ui::presentation_initialization::{
    UiPresentationInitialSelection, UiPresentationInitialization,
};

impl PresentationMembership {
    pub fn matches_prepared_document(
        &self,
        request_id: &str,
        child: &ryeos_client_base::ui::binding::UiPreparedPresentationDocument,
    ) -> bool {
        self.creation_request_id == request_id
            && self.presentation_id == child.presentation_id
            && self.thread_id.as_deref() == Some(child.thread_id.as_str())
            && self.chain_root_id.as_deref() == Some(child.chain_root_id.as_str())
            && public_coordinate(&self.root_coordinate) == child.root_binding
            && self.initialization == child.initialization
            && child.initialization.digest().ok().as_deref()
                == Some(self.initialization_digest.as_str())
    }
}

impl BrowserSessionStore {
    pub fn prepare_selected_presentation(
        &self,
        session_id: &str,
        opening: &SurfaceOpenRequest,
        origin: BindingAttachmentCandidate,
        tabs: Vec<BindingAttachmentCandidate>,
        admission: Option<&super::admission::SavedSurfaceAdmission>,
        limits: &PresentationLimits,
        policy: &str,
        bounds: ryeos_client_base::ui::UiBindingRequestBounds,
        validate_publication: impl FnOnce(
            &PresentationMembership,
            &[Arc<AdmittedBindingAttachment>],
        ) -> Result<(), AttachmentStoreError>,
    ) -> Result<PresentationMembership, AttachmentStoreError> {
        opening
            .validate()
            .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
        let SurfaceOpenPlacement::NewPresentation { root_binding } = &opening.placement else {
            return Err(AttachmentStoreError::PresentationInvalid);
        };
        let coordinate = BindingAttachmentCoordinate {
            binding_attachment_id: root_binding.binding_attachment_id.clone(),
            binding_generation: root_binding.binding_generation,
            binding_digest: root_binding.binding_digest.clone(),
        };
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        let root = require_shared_presentation_root(session, &coordinate, policy)?;
        let same_generation = |candidate: &BindingAttachmentCandidate| {
            candidate
                .compiled_binding
                .binding
                .request_engine_generation_identity
                == root
                    .compiled_binding
                    .binding
                    .request_engine_generation_identity
        };
        if !same_generation(&origin) || !tabs.iter().all(same_generation) {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        if let Some(proof) = admission {
            if !proof.matches_origin(&session.principal_id, &origin)
                || tabs.len() != proof.receipt().tab_count()
                || tabs.iter().enumerate().any(|(index, candidate)| {
                    !proof.matches_tab(index, &session.principal_id, candidate)
                })
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
        } else if !tabs.is_empty()
            || origin.registered_project_id != root.registered_project_id
            || origin.compiled_binding.binding.project_root
                != root.compiled_binding.binding.project_root
            || match (&origin.project_authority, &root.project_authority) {
                (None, None) => false,
                (Some(left), Some(right)) => !Arc::ptr_eq(left, right),
                _ => true,
            }
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        let mut generation = session.next_attachment_generation;
        let mut staged = Vec::with_capacity(1 + tabs.len());
        for candidate in std::iter::once(origin).chain(tabs) {
            staged.push(Arc::new(build_attachment(
                uuid::Uuid::new_v4().to_string(),
                generation,
                candidate,
                &session.principal_id,
                policy,
            )?));
            generation = generation
                .checked_add(1)
                .ok_or(AttachmentStoreError::Capacity)?;
        }
        let origin = &staged[0];
        let receipt = admission
            .map(|proof| {
                let request = proof.receipt();
                Ok::<_, AttachmentStoreError>(SavedSurfaceReceipt {
                    provenance: request.provenance.clone(),
                    title: request.title.clone(),
                    origin: request.origin.clone(),
                    active_view_set_id: request.active_view_set_id.clone(),
                    body: request.admitted_body(
                        &staged[1..]
                            .iter()
                            .map(|entry| entry.coordinate())
                            .collect::<Vec<_>>(),
                    )?,
                })
            })
            .transpose()?;
        if let Some(existing) = session
            .presentations
            .values()
            .find(|entry| entry.creation_request_id == opening.creation_request_id)
        {
            if existing.root_coordinate != coordinate
                || existing.initialization.selection
                    != (UiPresentationInitialSelection::SelectedSurface {
                        opening: opening.clone(),
                    })
                || existing.surfaces.len() != 1
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            let surface = existing.surfaces.values().next().unwrap();
            let old = std::iter::once(&surface.coordinate)
                .chain(
                    surface
                        .saved_resume
                        .as_ref()
                        .into_iter()
                        .flat_map(|receipt| receipt.coordinates()),
                )
                .map(|coordinate| exact_attachment(session, coordinate))
                .collect::<Result<Vec<_>, _>>()?;
            if surface.closed
                || old.len() != staged.len()
                || old.iter().zip(&staged).any(|(left, right)| {
                    left.compiled_binding.binding != right.compiled_binding.binding
                        || left.presentation != right.presentation
                        || left.registered_project_id != right.registered_project_id
                })
                || match (&surface.saved_resume, admission) {
                    (Some(retained), Some(proof)) => !retained.matches_request(proof.receipt()),
                    (None, None) => false,
                    _ => true,
                }
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            for descriptor in &old {
                validate_retained_project_path(descriptor)?;
            }
            let mut descriptors = vec![root];
            descriptors.extend(old);
            validate_publication(existing, &descriptors)?;
            return Ok(existing.clone());
        }
        if limits.max_surface_instances == 0
            || session.presentations.len() >= limits.max_presentations
            || transfer_reservation::occupied_attachment_count(session)
                .checked_add(staged.len())
                .is_none_or(|count| count > limits.max_attachments)
        {
            return Err(AttachmentStoreError::Capacity);
        }
        let title = receipt
            .as_ref()
            .map(|receipt| receipt.title.clone())
            .unwrap_or_else(|| {
                origin
                    .presentation
                    .definition
                    .surface()
                    .map(|surface| surface.display_title().to_string())
                    .unwrap_or_default()
            });
        if title.trim().is_empty()
            || title.len() > limits.max_label_bytes
            || title.chars().any(char::is_control)
        {
            return Err(AttachmentStoreError::PresentationInvalid);
        }
        let presentation_id = uuid::Uuid::new_v4().to_string();
        let surface_id = uuid::Uuid::new_v4().to_string();
        let composition = receipt.as_ref().map(SavedSurfaceReceipt::public_descriptor);
        let tab_descriptors = staged[1..]
            .iter()
            .map(|entry| entry.public_descriptor(bounds))
            .collect::<Vec<_>>();
        let (seeds, active) = ryeos_client_base::ui::binding::validate_surface_open_selection(
            &opening.selection,
            composition.as_ref(),
            &origin.public_descriptor(bounds),
            &tab_descriptors,
        )
        .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
        let manifest =
            allocate_mount_manifest(&presentation_id, &surface_id, 1, seeds, active.as_deref())?;
        let surface = SurfaceMembership {
            surface_instance_id: surface_id.clone(),
            creation_request_id: opening.creation_request_id.clone(),
            ordinal: session.next_surface_ordinal,
            title,
            coordinate: origin.coordinate(),
            mount_manifest: manifest.clone(),
            initial_seed: None,
            closed: false,
            particular_resume: None,
            saved_resume: receipt,
            tab_openings: BTreeMap::new(),
        };
        let mut descriptors = vec![root];
        descriptors.extend(staged.iter().cloned());
        descriptors
            .sort_by(|left, right| left.binding_attachment_id.cmp(&right.binding_attachment_id));
        let initialization = UiPresentationInitialization {
            schema:
                ryeos_ui_presentation_contract::initialization::PRESENTATION_INITIALIZATION_SCHEMA,
            selection: UiPresentationInitialSelection::SelectedSurface {
                opening: opening.clone(),
            },
            presentation_id: presentation_id.clone(),
            creation_request_id: opening.creation_request_id.clone(),
            root_binding: root_binding.clone(),
            next_mount_id: manifest.next_mount_id,
            surfaces: vec![surface.public_descriptor()],
            binding_attachments: descriptors
                .iter()
                .map(|entry| entry.public_descriptor(bounds))
                .collect(),
        };
        initialization
            .validate()
            .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
        let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
        let membership = PresentationMembership {
            presentation_id: presentation_id.clone(),
            creation_request_id: opening.creation_request_id.clone(),
            root_coordinate: coordinate,
            thread_id: Some(thread_id.clone()),
            chain_root_id: Some(thread_id),
            surfaces: HashMap::from([(surface_id, surface)]),
            activated_transfer_reservations: HashMap::new(),
            next_mount_id: manifest.next_mount_id,
            initialization_digest: initialization
                .digest()
                .map_err(|_| AttachmentStoreError::PresentationInvalid)?,
            initialization,
        };
        let next_ordinal = session
            .next_surface_ordinal
            .checked_add(1)
            .ok_or(AttachmentStoreError::Capacity)?;
        validate_publication(&membership, &descriptors)?;
        for descriptor in staged {
            session
                .attachments
                .insert(descriptor.binding_attachment_id.clone(), descriptor);
        }
        session.next_attachment_generation = generation;
        session.next_surface_ordinal = next_ordinal;
        session
            .presentations
            .insert(presentation_id, membership.clone());
        Ok(membership)
    }
}
