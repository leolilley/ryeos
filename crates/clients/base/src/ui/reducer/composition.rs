//! The existing seat prefix is the sole structural replay base. This module
//! installs its checked projection into the one canonical Core composition;
//! it is neither another application model nor an admission/grant owner.

use std::collections::BTreeSet;

use super::model::{InputBufferKey, RyeOsCore};
use crate::ui::composition_replay::UiCompositionReplay;
use crate::ui::seat::SeatEventKind;

impl RyeOsCore {
    /// Fence an exact MOVE source or the old addressed route of an unsettled
    /// replacement or lens operation.
    /// Its local completion is still write-ahead, not publication or release.
    pub(crate) fn route_write_fenced_by_composition(
        &self,
        instance: &crate::ids::RyeOsViewInstanceKey,
    ) -> bool {
        if self.move_freeze_restricts_instance(instance) {
            return true;
        }
        let Ok(replay) = self.acknowledged_composition_replay() else {
            return true;
        };
        let Some(presentation) = self.presentation.as_ref() else {
            return true;
        };
        let affects =
            |operation: &crate::ui::admission_operation::UiPreparedPresentationOperation| {
                let Some(request) = operation.composition() else {
                    return false;
                };
                let (tab, tile) = match &request.mutation {
                    crate::ui::composition_operation::CompositionMutation::LensPush {
                        tab_id,
                        tile_id,
                        ..
                    }
                    | crate::ui::composition_operation::CompositionMutation::LensPop {
                        tab_id,
                        tile_id,
                    }
                    | crate::ui::composition_operation::CompositionMutation::ReplaceView {
                        tab_id,
                        tile_id,
                        ..
                    }
                    | crate::ui::composition_operation::CompositionMutation::ReplaceFollowingView {
                        tab_id,
                        tile_id,
                        ..
                    } => (*tab_id, *tile_id),
                    crate::ui::composition_operation::CompositionMutation::MoveViewAcrossSurfaces {
                        source,
                        ..
                    } => (source.view_set_id, source.tile_id),
                    _ => return false,
                };
                if &request.presentation_id != presentation.id()
                    || operation.root_binding != *presentation.root_binding()
                {
                    return true;
                }
                let Some(surface) = request.owner.surface_instance_id() else {
                    return true;
                };
                let Some(mount) = presentation
                    .view_set(surface, tab)
                    .and_then(|set| set.tiles.get(&tile))
                else {
                    return true;
                };
                &mount.instance_key == instance
            };
        replay.pending.values().any(affects)
            || self
                .seat
                .events()
                .get(self.presentation_operations.acknowledged_prefix_len..)
                .is_none_or(|events| {
                    events.iter().any(|event| match &event.kind {
                        crate::ui::seat::SeatEventKind::PresentationOperationPrepared(
                            operation,
                        ) => affects(operation),
                        _ => false,
                    })
                })
    }
    pub(crate) fn composition_write_available(&self) -> bool {
        !self.presentation_operations.replay_blocked
            && self
                .acknowledged_composition_replay()
                .is_ok_and(|projection| projection.pending.is_empty())
            && self
                .seat
                .events()
                .get(self.presentation_operations.acknowledged_prefix_len..)
                .is_some_and(|events| {
                    events.iter().all(|event| {
                        matches!(&event.kind,
                    SeatEventKind::Facet { key, .. } if key != crate::ui::seat::KEY_ACTIVE_SURFACE)
                    })
                })
    }
    /// Capture a closed local edit once. No executable effect, provisional
    /// layout mutation or browser-provided allocation is involved.
    pub(crate) fn prepare_composition_mutation(
        &mut self,
        mutation: crate::ui::composition_operation::CompositionMutation,
    ) -> Vec<crate::ui::effect::RyeOsEffect> {
        let Some(surface) = self.active_surface() else {
            return Vec::new();
        };
        let owner = crate::ui::composition_operation::CompositionOwner::Surface {
            surface_instance_id: surface.id().clone(),
            binding: surface.binding().clone(),
        };
        self.prepare_composition_owned(mutation, owner)
    }

    pub(crate) fn prepare_subject_operation(
        &mut self,
        mount: crate::ids::RyeOsViewInstanceKey,
        attachment: crate::ui::attachment::SelectionAttachment,
    ) -> Vec<crate::ui::effect::RyeOsEffect> {
        use crate::ui::presentation_state::MountRef;
        let Some(presentation) = self.presentation.as_ref() else {
            return Vec::new();
        };
        let Some(mounted) = presentation
            .mount(&mount)
            .filter(|mounted| mounted.visible())
        else {
            return Vec::new();
        };
        let owner = match mounted {
            MountRef::SharedDock { .. } | MountRef::SharedDialog { .. } => {
                crate::ui::composition_operation::CompositionOwner::Shared {
                    binding: presentation.root_binding().clone(),
                }
            }
            MountRef::SurfaceDialog { surface_id, .. }
            | MountRef::SurfaceTile { surface_id, .. } => {
                let Some(surface) = presentation.surfaces().get(surface_id) else {
                    return Vec::new();
                };
                crate::ui::composition_operation::CompositionOwner::Surface {
                    surface_instance_id: surface_id.clone(),
                    binding: surface.binding().clone(),
                }
            }
        };
        self.prepare_composition_owned(
            crate::ui::composition_operation::CompositionMutation::SetSubject { mount, attachment },
            owner,
        )
    }

    pub(crate) fn prepare_composition_owned(
        &mut self,
        mutation: crate::ui::composition_operation::CompositionMutation,
        owner: crate::ui::composition_operation::CompositionOwner,
    ) -> Vec<crate::ui::effect::RyeOsEffect> {
        use crate::ui::admission_operation::{
            UiPreparedPresentationOperation, UiPreparedPresentationOperationBody,
        };
        let attempt = (|| -> Result<_, String> {
            if self.presentation_operations.replay_blocked {
                return Err("presentation replay is blocked".into());
            }
            let producer = self
                .presentation_operations
                .producer
                .clone()
                .ok_or("presentation writer unavailable")?;
            if !self.producer_matches_membership(&producer) {
                return Err("presentation writer is stale".into());
            }
            let projection = self.acknowledged_composition_replay()?;
            if !self.composition_write_available() {
                return Err("another structural operation awaits exact acknowledgement".into());
            }
            let presentation = self.presentation.as_ref().ok_or("presentation absent")?;
            let root_binding = presentation.root_binding().clone();
            let bounds = self
                .binding_attachment(&root_binding.binding_attachment_id)
                .ok_or("presentation root descriptor absent")?
                .binding_request_bounds;
            let request = crate::ui::composition_operation::UiCompositionRequest {
                schema: 1,
                presentation_id: presentation.id().clone(),
                operation_id: String::new(),
                expected_revision: projection.revision,
                expected_next_mount_id: projection.next_mount_id,
                owner,
                mutation,
            };
            if self.any_move_freeze(|freeze| {
                // Exact mount-only mutations must not fence another dock or
                // View merely because it shares the same admitted owner.
                match &request.mutation {
                    crate::ui::composition_operation::CompositionMutation::SetSubject { mount, .. }
                    | crate::ui::composition_operation::CompositionMutation::FocusView { mount } =>
                        freeze.instances.contains(mount),
                    _ => crate::ui::transfer_export::pending_request_affects_export(
                        &request,
                        &freeze.instances.iter().cloned().collect(),
                        &freeze.view_sets.iter().copied().collect(),
                        &freeze.export,
                    ),
                }
            }) {
                return Err("composition edit crosses exact MOVE freeze inventory".into());
            }
            let prepared_engine_seq = self
                .presentation_operations
                .reserved_engine_seq(self.seat.events().len())
                .ok_or("authenticated producer engine frontier unavailable or exhausted")?;
            let mut operation = UiPreparedPresentationOperation {
                schema: 2,
                operation_id: String::new(),
                producer,
                prepared_engine_seq,
                root_binding,
                request_bounds: bounds,
                body: UiPreparedPresentationOperationBody::Composition { request },
            };
            operation.assign_identity().map_err(str::to_owned)?;
            operation.validate().map_err(str::to_owned)?;
            // The full completion and append envelope are checked BEFORE
            // write-ahead preparation. A retry retains this exact identity.
            projection.expected_composition_completion(&operation, bounds.max_request_bytes)?;
            if !crate::ui::admission_operation::single_seat_append_fits(
                &operation.producer,
                crate::ui::admission_operation::PREPARED_EVENT,
                &operation,
                bounds.max_seat_append_bytes,
            ) {
                return Err("composition preparation exceeds admitted append bounds".into());
            }
            Ok(operation)
        })();
        match attempt {
            Ok(operation) => {
                self.seat
                    .append(SeatEventKind::PresentationOperationPrepared(operation));
                self.bump_generation();
            }
            Err(reason) => self.notice(
                format!("Composition edit not prepared: {reason}"),
                crate::ui::view_model::RyeOsTone::Warn,
            ),
        }
        Vec::new()
    }

    pub(crate) fn acknowledged_composition_replay(&self) -> Result<UiCompositionReplay, String> {
        let events = self
            .seat
            .events()
            .get(..self.presentation_operations.acknowledged_prefix_len)
            .ok_or("acknowledged seat prefix is outside its captured history")?;
        if let Some((len, replay)) = &self.acknowledged_composition_cache {
            if *len != self.presentation_operations.acknowledged_prefix_len {
                return Err("checked composition prefix does not match acknowledgement".into());
            }
            return Ok(replay.clone());
        }
        let Some(crate::ui::seat::SeatEvent {
            kind: SeatEventKind::PresentationInitialized(record),
            ..
        }) = events.first()
        else {
            return Err("composition requires acknowledged initialization first".into());
        };
        self.validate_replayed_initialization(&[])?;
        UiCompositionReplay::from_acknowledged_prefix(&record.initialization, events)
    }

    /// Advance a checked derivative before the acknowledged prefix becomes visible.
    /// The caller must only pass the exact next local events (after receipt validation).
    pub(crate) fn advance_acknowledged_composition_replay(
        &mut self,
        events: &[crate::ui::seat::SeatEvent],
    ) -> Result<(), String> {
        let old_len = self.presentation_operations.acknowledged_prefix_len;
        let mut replay = if let Some((len, replay)) = self.acknowledged_composition_cache.take() {
            if len != old_len {
                return Err("checked composition prefix does not match acknowledgement".into());
            }
            replay
        } else if old_len == 0 {
            let first = events.first().ok_or("composition initialization absent")?;
            let SeatEventKind::PresentationInitialized(record) = &first.kind else {
                return Err("composition initialization absent".into());
            };
            self.validate_replayed_initialization(std::slice::from_ref(first))?;
            let mut replay = UiCompositionReplay::begin(&record.initialization, first)?;
            for event in &events[1..] {
                replay = replay.apply_ordered_event_owned(event)?;
            }
            self.acknowledged_composition_cache = Some((events.len(), replay));
            return Ok(());
        } else {
            self.acknowledged_composition_replay()?
        };
        for event in events {
            replay = replay.apply_ordered_event_owned(event)?;
        }
        self.acknowledged_composition_cache = Some((old_len + events.len(), replay));
        Ok(())
    }

    /// Atomic publication after exact prefix acknowledgement or authoritative
    /// replay. A failed fold leaves the old presentation intact and blocks
    /// further effects; no synthetic selection or allocation events are made.
    pub(crate) fn reconcile_acknowledged_composition(
        &mut self,
    ) -> Result<BTreeSet<crate::ids::RyeOsViewInstanceKey>, String> {
        let replay = self.acknowledged_composition_replay()?;
        self.reconcile_acknowledged_composition_with_replay(replay)
    }

    pub(crate) fn reconcile_acknowledged_composition_with_replay(
        &mut self,
        mut replay: UiCompositionReplay,
    ) -> Result<BTreeSet<crate::ids::RyeOsViewInstanceKey>, String> {
        let cache_replay = self
            .acknowledged_composition_cache
            .is_none()
            .then(|| replay.clone());
        let previous = self.presentation.as_ref().ok_or("presentation absent")?;
        if previous.id() != replay.composition.id()
            || previous.root_binding() != replay.composition.root_binding()
        {
            return Err("causal projection belongs to another presentation root".into());
        }
        let focused_mount =
            |presentation: &crate::ui::presentation_state::PresentationComposition| {
                let set = presentation.active_view_set()?;
                set.tiles
                    .get(&set.focused_tile)
                    .map(|tile| tile.instance_key.clone())
            };
        let retained_focus = focused_mount(&replay.composition);
        let focus_scope =
            |presentation: &crate::ui::presentation_state::PresentationComposition| {
                let surface = presentation.active_surface()?;
                Some((
                    surface.id().clone(),
                    surface.active_view_set().map(|set| set.id),
                ))
            };
        let explicit_focus_change = self
            .presentation_operations
            .dirty
            .iter()
            .filter(|id| {
                replay.settled.contains(*id) && !self.presentation_operations.applied.contains(*id)
            })
            .any(|id| {
                let Some(event) = self
                    .presentation_operations
                    .prepared
                    .get(id)
                    .and_then(|seq| self.seat.event_at_sequence(*seq))
                else {
                    return false;
                };
                let SeatEventKind::PresentationOperationPrepared(operation) = &event.kind else {
                    return false;
                };
                operation.composition().is_some_and(|request| matches!(&request.mutation,
                    crate::ui::composition_operation::CompositionMutation::FocusView { .. }
                    | crate::ui::composition_operation::CompositionMutation::SelectGroup { .. }))
            });
        let focus_changed = explicit_focus_change
            || focused_mount(previous) != retained_focus
            || focus_scope(previous) != focus_scope(&replay.composition);
        // Motion belongs to successful local completion publication, not to
        // preparation or history hydration. Derive addresses from the two
        // checked compositions rather than predicting allocated tile IDs.
        let mut completed_motion = Vec::new();
        for id in &self.presentation_operations.dirty {
            if !replay.settled.contains(id) || self.presentation_operations.applied.contains(id) {
                continue;
            }
            let Some(SeatEventKind::PresentationOperationPrepared(operation)) = self
                .presentation_operations
                .prepared
                .get(id)
                .and_then(|seq| self.seat.event_at_sequence(*seq))
                .map(|event| &event.kind)
            else {
                continue;
            };
            let Some(request) = operation.composition() else {
                continue;
            };
            use crate::ui::composition_operation::{CompositionMutation, CompositionPlacement};
            use crate::ui::view_model::{RyeOsMotionEventVm, RyeOsSplitAxisVm};
            match &request.mutation {
                CompositionMutation::InsertView {
                    tab_id, placement, ..
                }
                | CompositionMutation::InsertPinnedView {
                    tab_id, placement, ..
                }
                | CompositionMutation::InsertFollowingView {
                    tab_id, placement, ..
                } => {
                    let old = previous
                        .view_sets()
                        .find(|(_, set)| set.id == *tab_id)
                        .map(|(_, set)| set);
                    let new = replay
                        .composition
                        .view_sets()
                        .find(|(_, set)| set.id == *tab_id)
                        .map(|(_, set)| set);
                    if let (Some(old), Some(new)) = (old, new) {
                        for (tile_id, _) in new
                            .tiles
                            .iter()
                            .filter(|(tile_id, _)| !old.tiles.contains_key(*tile_id))
                        {
                            if let CompositionPlacement::Split { target, axis, .. } = placement {
                                let event = RyeOsMotionEventVm::TileSplit {
                                    source_tile_id: target.0.to_string(),
                                    new_tile_id: tile_id.0.to_string(),
                                    axis: match axis {
                                        crate::layout::SplitAxis::Horizontal => {
                                            RyeOsSplitAxisVm::Horizontal
                                        }
                                        crate::layout::SplitAxis::Vertical => {
                                            RyeOsSplitAxisVm::Vertical
                                        }
                                    },
                                };
                                if !completed_motion.contains(&event) {
                                    completed_motion.push(event);
                                }
                            }
                            let event = RyeOsMotionEventVm::TileEnter {
                                tile_id: tile_id.0.to_string(),
                            };
                            if !completed_motion.contains(&event) {
                                completed_motion.push(event);
                            }
                        }
                    }
                }
                CompositionMutation::CloseView { tile_id, .. } => {
                    let existed = previous
                        .view_sets()
                        .any(|(_, set)| set.tiles.contains_key(tile_id));
                    let remains = replay
                        .composition
                        .view_sets()
                        .any(|(_, set)| set.tiles.contains_key(tile_id));
                    if existed && !remains {
                        let event = RyeOsMotionEventVm::TileExit {
                            tile_id: tile_id.0.to_string(),
                        };
                        if !completed_motion.contains(&event) {
                            completed_motion.push(event);
                        }
                    }
                }
                _ => {}
            }
        }
        let mut coordinates = BTreeSet::new();
        coordinates.insert(
            replay
                .composition
                .root_binding()
                .binding_attachment_id
                .clone(),
        );
        for surface in replay.composition.surfaces().values() {
            coordinates.insert(surface.binding().binding_attachment_id.clone());
        }
        for binding in replay.insertion_bindings.values() {
            coordinates.insert(binding.binding_attachment_id.clone());
        }
        for subject in replay.subjects.values() {
            coordinates.insert(subject.binding.binding_attachment_id.clone());
        }
        for frame in replay.lens_frames.values().flatten() {
            coordinates.insert(frame.binding.binding_attachment_id.clone());
        }
        let mut retained = Vec::new();
        let mut newly_registered = Vec::new();
        for id in coordinates {
            let descriptor = replay
                .binding_descriptors()
                .get(&id)
                .ok_or("live causal binding descriptor is absent")?;
            if self
                .binding_attachments
                .get(&id)
                .is_some_and(|current| &current.descriptor != descriptor)
            {
                return Err("causal descriptor contradicts a retained binding identity".into());
            }
            if !self.binding_attachments.contains_key(&id) {
                newly_registered.push(descriptor.clone());
            }
            retained.push((
                id,
                crate::ui::binding_context::RetainedUiBindingAttachment::from_descriptor(
                    descriptor.clone(),
                )
                .map_err(|error| error.to_string())?,
            ));
        }
        // Only the ACK owner may migrate transient instance ownership. Old
        // completed replay metadata is deliberately not a migration request.
        let mut mount_remaps = std::collections::BTreeMap::new();
        for id in &self.presentation_operations.dirty {
            if replay.settled.contains(id)
                && !self.presentation_operations.applied.contains(id)
                && let Some(remaps) = replay.completed_mount_remaps.get(id)
            {
                for (old, new) in remaps {
                    if mount_remaps.insert(old.clone(), new.clone()).is_some() {
                        return Err("completed mount remap has duplicate source".into());
                    }
                }
            }
        }
        let mut destinations = BTreeSet::new();
        for (old, new) in &mount_remaps {
            let source = previous.mount(old).ok_or("completed remap source absent")?;
            let destination = replay
                .composition
                .mount(new)
                .ok_or("completed remap destination absent")?;
            if old == new
                || !destinations.insert(new.clone())
                || previous.mount(new).is_some()
                || replay.composition.mount(old).is_some()
                || source.view_ref() != destination.view_ref()
                || source.binding() != destination.binding()
            {
                return Err("completed mount remap contradicts exact payload ownership".into());
            }
        }
        let mut remapped_buffers = Vec::new();
        for (encoded, buffer) in &self.input_buffers {
            let Some(mut key) = InputBufferKey::from_storage_key(encoded) else {
                continue;
            };
            let Some(destination) = mount_remaps.get(&key.view_instance_key) else {
                continue;
            };
            key.view_instance_key = destination.clone();
            let new_key = key.storage_key();
            if self.input_buffers.contains_key(&new_key) {
                return Err("completed mount remap collides with local draft".into());
            }
            remapped_buffers.push((encoded.clone(), new_key, buffer.clone()));
        }
        let mut preserved = replay
            .composition
            .docks()
            .values()
            .map(|mount| mount.view().instance_key().clone())
            .chain(
                replay
                    .composition
                    .dialogs()
                    .values()
                    .map(|mount| mount.view().instance_key().clone()),
            )
            .chain(replay.subjects.keys().cloned())
            .filter(|instance| {
                replay
                    .composition
                    .mount(instance)
                    .zip(previous.mount(instance))
                    .is_some_and(|(new, old)| {
                        new.view_ref() == old.view_ref() && new.binding() == old.binding()
                    })
            })
            .collect::<BTreeSet<_>>();
        replay.composition.preserve_transient_state_from(previous);
        for (old, new) in &mount_remaps {
            let local = previous
                .mount(old)
                .expect("remap checked above")
                .local()
                .clone();
            *replay
                .composition
                .mount_local_mut(new)
                .expect("remap checked above") = local;
            preserved.insert(new.clone());
        }
        // Transfer replay supplies data, not attachment authority. Check every
        // draft against the resulting admitted mount or its retained return
        // frame before mutating any live Core state.
        for (key, buffer) in &replay.transfer_drafts {
            let storage_key = key.storage_key();
            let matches_binding = replay
                .composition
                .mount(&key.view_instance_key)
                .is_some_and(|mount| {
                    InputBufferKey::storage_key_matches_view_binding(
                        &storage_key,
                        &key.view_instance_key,
                        mount.view_ref(),
                        mount.binding(),
                    )
                })
                || replay
                    .lens_frames
                    .get(&key.view_instance_key)
                    .is_some_and(|frames| {
                        frames.iter().any(|frame| {
                            InputBufferKey::storage_key_matches_view_binding(
                                &storage_key,
                                &key.view_instance_key,
                                &frame.view_ref,
                                &frame.binding,
                            )
                        })
                    });
            if replay.composition.mount(&key.view_instance_key).is_none()
                || !matches_binding
                || buffer.cursor > buffer.text.len()
                || !buffer.text.is_char_boundary(buffer.cursor)
            {
                return Err("transferred draft has no exact admitted destination binding".into());
            }
            let descriptor = replay
                .binding_descriptors()
                .values()
                .find(|descriptor| {
                    let coordinate = super::super::binding::UiBindingAttachmentCoordinate {
                        binding_attachment_id: descriptor.binding_attachment_id.clone(),
                        binding_generation: descriptor.binding_generation,
                        binding_digest: descriptor.binding_digest.clone(),
                    };
                    key.binding_scope == InputBufferKey::binding_scope(&coordinate)
                })
                .ok_or("transferred draft has no exact destination input budget")?;
            buffer.validate_retained_binding(key, descriptor)?;
        }
        let presentation_id = replay.composition.id().as_str();
        if self
            .data
            .session
            .as_ref()
            .and_then(|session| {
                session
                    .presentations
                    .iter()
                    .find(|membership| membership.presentation_id == presentation_id)
            })
            .is_none()
        {
            return Err("causal presentation has no authenticated membership".into());
        }
        let staged: &mut RyeOsCore = self;
        for (old, new, buffer) in remapped_buffers {
            staged.input_buffers.remove(&old);
            staged.input_buffers.insert(new, buffer);
        }
        for focus in [
            &mut staged.focused_input,
            &mut staged.field_query_editing,
            &mut staged.dialog_return_focus,
        ] {
            if let Some(new) = focus.as_ref().and_then(|old| mount_remaps.get(old)) {
                *focus = Some(new.clone());
            }
        }
        // Source effects retain their original request coordinates and epochs.
        // Never rewrite an in-flight result's identity or discard its outcome
        // as a side effect of changing a layout address.
        staged.binding_attachments.extend(retained);
        let next_subjects: std::collections::BTreeMap<_, _> = replay
            .subjects
            .into_iter()
            .map(|(key, subject)| (key, subject.attachment))
            .collect();
        let mut changed_subjects = staged
            .selection_attachments
            .keys()
            .chain(next_subjects.keys())
            .filter(|key| staged.selection_attachments.get(*key) != next_subjects.get(*key))
            .filter(|key| preserved.contains(*key))
            .cloned()
            .collect::<BTreeSet<_>>();
        changed_subjects.extend(
            staged
                .retained_input_routes
                .keys()
                .chain(replay.input_routes.keys())
                .filter(|key| {
                    staged.retained_input_routes.get(*key) != replay.input_routes.get(*key)
                })
                .filter(|key| replay.composition.mount(key).is_some())
                .cloned(),
        );
        staged.selection_attachments = next_subjects;
        staged.retained_work_selections = replay.work_selections;
        staged.retained_lens_frames = replay.lens_frames;
        staged.retained_input_routes = replay.input_routes;
        staged.retained_lens_labels = replay.lens_labels;
        staged.view_set_insertion_attachments = replay
            .insertion_bindings
            .into_iter()
            .map(|(id, coordinate)| (id, coordinate.binding_attachment_id))
            .collect();
        staged.view_set_template_ids = replay.template_ids;
        let current_draft_mounts = replay
            .composition
            .docks()
            .values()
            .map(|mount| mount.view().instance_key().clone())
            .chain(
                replay
                    .composition
                    .dialogs()
                    .values()
                    .map(|mount| mount.view().instance_key().clone()),
            )
            .chain(
                replay
                    .composition
                    .view_sets()
                    .flat_map(|(_, set)| set.tiles.values().map(|tile| tile.instance_key.clone())),
            )
            .collect::<Vec<_>>();
        staged.input_buffers.retain(|key, _| {
            current_draft_mounts.iter().any(|instance| {
                replay.composition.mount(instance).is_some_and(|mount| {
                    InputBufferKey::storage_key_matches_view_binding(
                        key,
                        instance,
                        mount.view_ref(),
                        mount.binding(),
                    )
                })
            }) || staged
                .retained_lens_frames
                .iter()
                .any(|(instance, frames)| {
                    frames.iter().any(|frame| {
                        InputBufferKey::storage_key_matches_view_binding(
                            key,
                            instance,
                            &frame.view_ref,
                            &frame.binding,
                        )
                    })
                })
        });
        // Replay can revisit activation on later ACKs. Preserve edits made
        // locally after the first hydration rather than resetting them to the
        // signed transfer snapshot on every reconciliation.
        for (key, buffer) in replay.transfer_drafts {
            staged
                .input_buffers
                .entry(key.storage_key())
                .or_insert(buffer);
        }
        // Switching a Surface or tab does not steal interaction focus from a
        // surviving shared mount. An explicit View/group focus edit still does.
        let preserves_shared_focus = |key: &crate::ids::RyeOsViewInstanceKey| {
            !explicit_focus_change
                && matches!(
                    key.mount_owner(),
                    Some(crate::ids::UiViewMountOwner::Shared { .. })
                )
        };
        staged.focused_input = staged.focused_input.take().filter(|key| {
            (!focus_changed || destinations.contains(key) || preserves_shared_focus(key))
                && preserved.contains(key)
                && replay
                    .composition
                    .mount(key)
                    .is_some_and(|mount| mount.visible())
        });
        staged.field_query_editing = staged.field_query_editing.take().filter(|key| {
            (!focus_changed || destinations.contains(key) || preserves_shared_focus(key))
                && preserved.contains(key)
                && replay
                    .composition
                    .mount(key)
                    .is_some_and(|mount| mount.visible())
        });
        let session = staged
            .data
            .session
            .as_mut()
            .expect("membership checked above");
        let membership = session
            .presentations
            .iter_mut()
            .find(|membership| membership.presentation_id == presentation_id)
            .expect("membership checked above");
        membership.surfaces = replay.admitted_surfaces.into_values().collect();
        membership.surfaces.sort_by_key(|surface| surface.ordinal);
        membership.next_mount_id = replay.next_mount_id;
        // Core and session attachment membership are updated together here
        // (and retired together by binding_lifecycle). Only a newly admitted
        // coordinate needs publication; rewalking all historical descriptors
        // on each ordinary edit is neither an authority check nor bounded.
        session.binding_attachments.extend(newly_registered);
        staged.presentation = Some(replay.composition);
        staged.normalize_mounted_local_states();
        for instance in &changed_subjects {
            staged.invalidate_view_sources(instance);
            staged.reset_field_replay_for_subject(instance);
        }
        if focus_changed
            && !staged
                .focused_input
                .as_ref()
                .is_some_and(|key| destinations.contains(key) || preserves_shared_focus(key))
        {
            staged.focused_input = retained_focus.filter(|key| {
                staged
                    .presentation
                    .as_ref()
                    .and_then(|p| p.mount(key))
                    .is_some_and(|mount| {
                        mount.visible() && staged.instance_declares_input(key, mount.view_ref())
                    })
            });
        }
        staged.bump_generation();
        staged.ui.motion.extend(completed_motion);
        if let Some(cache_replay) = cache_replay {
            self.acknowledged_composition_cache = Some((
                self.presentation_operations.acknowledged_prefix_len,
                cache_replay,
            ));
        }
        Ok(changed_subjects)
    }

    /// A settled subject replaces every channel's observation, including an
    /// already requested on-demand channel. No source work is emitted by the
    /// preparation or the first ACK; callers invoke this after publication.
    pub(crate) fn refresh_settled_subjects(
        &mut self,
        changed: &BTreeSet<crate::ids::RyeOsViewInstanceKey>,
    ) -> Vec<crate::ui::effect::RyeOsEffect> {
        let targets = changed
            .iter()
            .filter_map(|instance| {
                let mount = self.presentation.as_ref()?.mount(instance)?;
                mount
                    .visible()
                    .then(|| (instance.clone(), mount.view_ref().to_string()))
            })
            .collect::<Vec<_>>();
        targets
            .into_iter()
            .flat_map(|(instance, view_ref)| {
                let channels = self
                    .binding_for_instance(&instance, &view_ref)
                    .map(|binding| binding.sources.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                channels
                    .into_iter()
                    .flat_map(|channel| {
                        self.clear_field_expansions_for_channel(&instance, &channel);
                        self.refresh_source_channel(instance.clone(), &view_ref, &channel)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

#[cfg(test)]
mod motion_tests {
    use crate::ui::reducer::test_support::*;
    use crate::ui::view_model::RyeOsMotionEventVm;

    #[test]
    fn tile_motion_is_published_once_at_completion_ack_only() {
        let mut core = core(session_with_surface(serde_json::json!({
            "name":"motion", "tiles":["view:test/a"],
            "views":{"view:test/a":{"widget":"text"},"view:test/b":{"widget":"text"}}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core.ui.motion.clear();
        core.add_center_tile(crate::view_set::ViewSpec::bound("view:test/b"));
        assert!(core.ui.motion.is_empty());
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert!(core.ui.motion.is_empty());
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert_eq!(
            core.ui
                .motion
                .iter()
                .filter(|event| matches!(event, RyeOsMotionEventVm::TileEnter { .. }))
                .count(),
            1
        );
        assert_eq!(
            core.ui
                .motion
                .iter()
                .filter(|event| matches!(event, RyeOsMotionEventVm::TileSplit { .. }))
                .count(),
            1
        );
        let added = core.active_view_set().unwrap().focused_tile;
        core.ui.motion.clear();
        core.reconcile_acknowledged_composition().unwrap();
        assert!(core.ui.motion.is_empty());
        core.close_tile_or_empty(added);
        assert!(core.ui.motion.is_empty());
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert!(core.ui.motion.is_empty());
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert_eq!(
            core.ui
                .motion
                .iter()
                .filter(|event| matches!(event, RyeOsMotionEventVm::TileExit { .. }))
                .cloned()
                .collect::<Vec<_>>(),
            vec![RyeOsMotionEventVm::TileExit {
                tile_id: added.0.to_string()
            }]
        );
        core.ui.motion.clear();
        core.reconcile_acknowledged_composition().unwrap();
        assert!(core.ui.motion.is_empty());
    }
}
