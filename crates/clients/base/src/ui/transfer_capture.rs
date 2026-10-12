//! Exact live-core capture for presentation transfer preparation.
//!
//! This is local-state testimony only. Canonical replay separately proves the
//! durable layout, binding and subject history before a preparation is signed.

use std::collections::BTreeSet;

use super::{
    composition_operation::UiCompositionRouteAssignment,
    model::{InputBufferKey, RyeOsCore},
    transfer_export::{
        UiTransferDraftExport, UiTransferExport, UiTransferExportContent, UiTransferExportMode,
        UiTransferMountExport,
    },
};
use crate::ids::RyeOsViewInstanceKey;

/// Resolve one typed local selection into a complete source-only inventory.
/// Every mutable value is then recaptured by the existing exact live owner;
/// no renderer-provided export, binding or source hash enters this path.
pub(crate) fn capture_selected_transfer_export(
    core: &RyeOsCore,
    mode: UiTransferExportMode,
    selection: &super::event::UiTransferSourceSelection,
    max_bytes: u64,
) -> Result<UiTransferExport, String> {
    use super::event::UiTransferSourceSelection as Source;
    use super::transfer_export::UiTransferSetExport;
    let presentation = core
        .presentation
        .as_ref()
        .ok_or("transfer source presentation absent")?;
    let replay = core.acknowledged_composition_replay()?;
    let mount = |instance: &RyeOsViewInstanceKey| -> Result<UiTransferMountExport, String> {
        let mounted = presentation
            .mount(instance)
            .ok_or("transfer source mount absent")?;
        let mut subject = core
            .selection_attachment_for_instance(instance)
            .ok_or("transfer source has no exact subject")?;
        if mode == UiTransferExportMode::Copy
            && matches!(
                &subject,
                super::attachment::SelectionAttachment::Pinned { .. }
            )
        {
            let binding = core
                .binding_for_instance(instance, mounted.view_ref())
                .ok_or("COPY source View binding absent")?;
            let facets = super::attachment::selection_dependencies(binding)
                .into_iter()
                .collect::<Vec<_>>();
            if facets.is_empty() {
                return Err("COPY pin has no portable subject contract".into());
            }
            // COPY is an explicit choice to leave the destination subject
            // unresolved. It never transports pinned values or their grant.
            subject = super::attachment::SelectionAttachment::RequiredSubject {
                input: format!("copy_subject_{}", instance.as_str()),
                facets,
            };
        }
        let mut frames = core
            .retained_lens_frames
            .get(instance)
            .cloned()
            .unwrap_or_default();
        if mode == UiTransferExportMode::Copy {
            for (index, frame) in frames.iter_mut().enumerate() {
                if matches!(
                    &frame.attachment,
                    super::attachment::SelectionAttachment::Pinned { .. }
                ) {
                    let descriptor = core
                        .binding_attachment(&frame.binding.binding_attachment_id)
                        .filter(|value| {
                            value.binding_generation == frame.binding.binding_generation
                                && value.binding_digest == frame.binding.binding_digest
                        })
                        .ok_or("COPY frame binding unavailable")?;
                    let binding = descriptor
                        .presentation
                        .views
                        .get(&frame.view_ref)
                        .ok_or("COPY frame View unavailable")?;
                    let binding: super::content::ViewBinding =
                        serde_json::from_value(binding.clone()).map_err(|error| {
                            format!("COPY frame View contract is invalid: {error}")
                        })?;
                    let facets = super::attachment::selection_dependencies(&binding)
                        .into_iter()
                        .collect::<Vec<_>>();
                    if facets.is_empty() {
                        return Err("COPY frame pin has no portable subject contract".into());
                    }
                    frame.attachment = super::attachment::SelectionAttachment::RequiredSubject {
                        input: format!("copy_frame_{}_{}", instance.as_str(), index),
                        facets,
                    };
                }
                frame.input_route = None;
            }
        }
        Ok(UiTransferMountExport {
            source_instance: instance.clone(),
            view_ref: mounted.view_ref().to_owned(),
            binding: mounted.binding().clone(),
            local: if mode == UiTransferExportMode::Move {
                mounted.local().clone()
            } else {
                crate::view_set::ViewLocalState::None
            },
            subject,
            route: UiCompositionRouteAssignment::Authored,
            current_label: core.retained_lens_labels.get(instance).cloned().flatten(),
            drafts: Vec::new(),
            lens_frames: frames,
        })
    };
    let set = |surface_id: &crate::ids::SurfaceInstanceId,
               view_set: &crate::view_set::ViewSet|
     -> Result<UiTransferSetExport, String> {
        let handle = replay
            .template_ids
            .get(&view_set.id)
            .ok_or("source set has no canonical template identity")?
            .clone();
        let order = view_set
            .root
            .as_ref()
            .map(crate::layout::LayoutTree::tile_ids)
            .unwrap_or_default();
        let focused_view = order.iter().position(|tile| *tile == view_set.focused_tile);
        let mounts = order
            .iter()
            .map(|tile_id| {
                let instance = &view_set
                    .tiles
                    .get(tile_id)
                    .ok_or("source layout tile absent")?
                    .instance_key;
                mount(instance)
            })
            .collect::<Result<Vec<_>, String>>()?;
        let binding = replay
            .insertion_bindings
            .get(&view_set.id)
            .ok_or("source set insertion binding absent")?
            .clone();
        Ok(UiTransferSetExport {
            handle: handle.clone(),
            source_view_set_id: view_set.id,
            source_owner: crate::ids::UiViewMountOwner::Surface {
                presentation_id: presentation.id().clone(),
                surface_instance_id: surface_id.clone(),
            },
            binding,
            seed: super::layout_preferences::capture_view_set(view_set, handle)?,
            focused_view,
            mounts,
        })
    };
    let content = match selection {
        Source::MountedView { instance_key } => {
            let (_, view_set) = presentation
                .view_set_for_instance(instance_key)
                .ok_or("selected View is not a Surface tile")?;
            UiTransferExportContent::SurfaceMount {
                source_view_set_id: view_set.id,
                mount: mount(instance_key)?,
            }
        }
        Source::ViewSet {
            surface_instance_id,
            view_set_id,
        } => {
            let surface = presentation
                .surfaces()
                .get(surface_instance_id)
                .ok_or("source Surface absent")?;
            let view_set = surface
                .view_sets()
                .iter()
                .find(|set| set.id == *view_set_id)
                .ok_or("source ViewSet absent")?;
            let selected = set(surface_instance_id, view_set)?;
            UiTransferExportContent::ViewSets {
                active_set: (surface.active_view_set().map(|set| set.id) == Some(*view_set_id))
                    .then(|| selected.handle.clone()),
                sets: vec![selected],
            }
        }
        Source::SurfaceViewSets {
            surface_instance_id,
        } => {
            let surface = presentation
                .surfaces()
                .get(surface_instance_id)
                .ok_or("source Surface absent")?;
            let sets = surface
                .view_sets()
                .iter()
                .map(|set_ref| set(surface_instance_id, set_ref))
                .collect::<Result<Vec<_>, String>>()?;
            let active_set = surface.active_view_set().and_then(|active| {
                sets.iter()
                    .find(|set| set.source_view_set_id == active.id)
                    .map(|set| set.handle.clone())
            });
            UiTransferExportContent::ViewSets { active_set, sets }
        }
        Source::SharedDock { edge } => {
            let name = match edge {
                super::model::RyeOsDockEdge::Top => "top",
                super::model::RyeOsDockEdge::Bottom => "bottom",
                super::model::RyeOsDockEdge::Left => "left",
                super::model::RyeOsDockEdge::Right => "right",
            };
            let dock = presentation
                .docks()
                .get(name)
                .ok_or("source shared dock absent")?;
            UiTransferExportContent::SharedDock {
                edge: *edge,
                visible: dock.visible,
                size: dock.size,
                mount: mount(dock.view().instance_key())?,
            }
        }
    };
    let requested = UiTransferExport {
        schema: super::transfer_export::UI_TRANSFER_EXPORT_SCHEMA,
        mode,
        content,
    };
    capture_transfer_export_from_core(core, &requested, max_bytes)
}

/// Capture the exact renderer-local state for an already selected closed
/// inventory. The requested value supplies only its scope and stable handles;
/// every mutable mount value is replaced from `core`.
pub(crate) fn capture_transfer_export_from_core(
    core: &RyeOsCore,
    requested: &UiTransferExport,
    max_bytes: u64,
) -> Result<UiTransferExport, String> {
    requested.validate(max_bytes)?;
    let mut captured = requested.clone();
    let instances = requested
        .mounts()
        .into_iter()
        .map(|mount| mount.source_instance.clone())
        .collect::<BTreeSet<_>>();
    let set_ids = match &requested.content {
        UiTransferExportContent::ViewSets { sets, .. } => sets
            .iter()
            .map(|set| set.source_view_set_id)
            .collect::<BTreeSet<_>>(),
        UiTransferExportContent::SurfaceMount {
            source_view_set_id, ..
        } => BTreeSet::from([*source_view_set_id]),
        UiTransferExportContent::SharedDock { .. } => BTreeSet::new(),
    };
    if core
        .pending_effects
        .values()
        .any(|effect| pending_effect_belongs_to(effect, &instances, &set_ids, requested))
    {
        return Err("transfer source has a pending local effect".into());
    }
    match &mut captured.content {
        UiTransferExportContent::ViewSets { active_set, sets } => {
            for set in sets.iter_mut() {
                let view_set = core
                    .view_set(set.source_view_set_id)
                    .ok_or("transfer source set is not mounted")?;
                set.seed =
                    super::layout_preferences::capture_view_set(view_set, set.seed.id.clone())?;
                let order = view_set
                    .root
                    .as_ref()
                    .map(crate::layout::LayoutTree::tile_ids)
                    .unwrap_or_default();
                set.focused_view = order.iter().position(|tile| *tile == view_set.focused_tile);
                if view_set.root.is_some() && set.focused_view.is_none() {
                    return Err("transfer source focus is outside its mounted layout".into());
                }
                if order.len() != set.mounts.len() {
                    return Err("transfer source mount inventory is incomplete".into());
                }
                for (mount, tile_id) in set.mounts.iter_mut().zip(order) {
                    let tile = view_set
                        .tiles
                        .get(&tile_id)
                        .ok_or("transfer source layout names an absent tile")?;
                    if mount.source_instance != tile.instance_key {
                        return Err(
                            "transfer source mount order differs from its live layout".into()
                        );
                    }
                    capture_mount(core, captured.mode, mount)?;
                }
            }
            let first = sets
                .first()
                .ok_or("transfer source set inventory is empty")?;
            let crate::ids::UiViewMountOwner::Surface {
                surface_instance_id,
                ..
            } = &first.source_owner
            else {
                return Err("transfer source set has no Surface owner".into());
            };
            *active_set = core
                .presentation
                .as_ref()
                .and_then(|presentation| presentation.surfaces().get(surface_instance_id))
                .and_then(|surface| surface.active_view_set())
                .and_then(|active| {
                    sets.iter()
                        .find(|set| set.source_view_set_id == active.id)
                        .map(|set| set.handle.clone())
                });
        }
        UiTransferExportContent::SurfaceMount { mount, .. } => {
            capture_mount(core, captured.mode, mount)?;
        }
        UiTransferExportContent::SharedDock {
            edge,
            visible,
            size,
            mount,
        } => {
            let edge = match edge {
                super::model::RyeOsDockEdge::Top => "top",
                super::model::RyeOsDockEdge::Bottom => "bottom",
                super::model::RyeOsDockEdge::Left => "left",
                super::model::RyeOsDockEdge::Right => "right",
            };
            let dock = core
                .presentation
                .as_ref()
                .and_then(|presentation| presentation.docks().get(edge))
                .ok_or("transfer source shared dock is absent")?;
            if dock.view().instance_key() != &mount.source_instance {
                return Err("transfer source shared dock changed instance".into());
            }
            *visible = dock.visible;
            *size = dock.size;
            capture_mount(core, captured.mode, mount)?;
        }
    }
    captured.validate(max_bytes)?;
    Ok(captured)
}

/// Refuse an export whose mutable local inventory differs from the live core.
pub(crate) fn validate_transfer_export_against_core(
    core: &RyeOsCore,
    export: &UiTransferExport,
    max_bytes: u64,
) -> Result<(), String> {
    let captured = capture_transfer_export_from_core(core, export, max_bytes)?;
    if &captured != export {
        return Err("transfer export differs from exact live core state".into());
    }
    Ok(())
}

fn capture_mount(
    core: &RyeOsCore,
    mode: UiTransferExportMode,
    mount: &mut UiTransferMountExport,
) -> Result<(), String> {
    let requested_subject = mount.subject.clone();
    let canonical = core
        .presentation
        .as_ref()
        .and_then(|presentation| presentation.mount(&mount.source_instance))
        .ok_or("transfer source mount is not mounted")?;
    if canonical.view_ref() != mount.view_ref || canonical.binding() != &mount.binding {
        return Err("transfer source mount binding differs from the live core".into());
    }
    mount.local = if mode == UiTransferExportMode::Copy {
        crate::view_set::ViewLocalState::None
    } else {
        canonical.local().clone()
    };
    mount.route = if mode == UiTransferExportMode::Copy {
        UiCompositionRouteAssignment::Authored
    } else {
        route(core, &mount.source_instance)
    };
    mount.current_label = core
        .retained_lens_labels
        .get(&mount.source_instance)
        .cloned()
        .flatten();
    mount.subject = match mode {
        UiTransferExportMode::Move => core
            .selection_attachment_for_instance(&mount.source_instance)
            .ok_or("transfer source mount has no exact subject")?,
        UiTransferExportMode::Copy => copy_subject(core, mount, &requested_subject)?,
    };
    let retained_frames = core
        .retained_lens_frames
        .get(&mount.source_instance)
        .cloned()
        .unwrap_or_default();
    mount.lens_frames = if mode == UiTransferExportMode::Copy {
        if retained_frames.len() != mount.lens_frames.len() {
            return Err("copy must declare every retained return frame".into());
        }
        retained_frames
            .iter()
            .zip(&mount.lens_frames)
            .map(|(source, requested)| {
                if source.binding != requested.binding
                    || source.view_ref != requested.view_ref
                    || source.label != requested.label
                    || requested.input_route.is_some()
                {
                    return Err(
                        "copy return frame differs from its source or carries a route".into(),
                    );
                }
                if matches!(
                    &source.attachment,
                    super::attachment::SelectionAttachment::Pinned { .. }
                ) {
                    let descriptor = core
                        .binding_attachment(&source.binding.binding_attachment_id)
                        .filter(|value| {
                            value.binding_generation == source.binding.binding_generation
                                && value.binding_digest == source.binding.binding_digest
                        })
                        .ok_or("copy return frame binding is unavailable")?;
                    let binding = descriptor
                        .presentation
                        .views
                        .get(&source.view_ref)
                        .ok_or("copy return frame View is unavailable")?;
                    let super::attachment::SelectionAttachment::RequiredSubject { facets, .. } =
                        &requested.attachment
                    else {
                        return Err("copy return frame pin requires a portable subject".into());
                    };
                    let binding: super::content::ViewBinding =
                        serde_json::from_value(binding.clone()).map_err(|error| {
                            format!("copy return frame View contract is invalid: {error}")
                        })?;
                    let dependencies = super::attachment::selection_dependencies(&binding);
                    if dependencies.is_empty() || dependencies != facets.iter().cloned().collect() {
                        return Err(
                            "copy return frame portable subject differs from its binding".into(),
                        );
                    }
                } else if source.attachment != requested.attachment {
                    return Err("copy return frame subject differs from its source".into());
                }
                Ok(requested.clone())
            })
            .collect::<Result<Vec<_>, String>>()?
    } else {
        retained_frames
    };
    mount.drafts = if mode == UiTransferExportMode::Copy {
        Vec::new()
    } else {
        exact_drafts(core, mount)?
    };
    Ok(())
}

fn route(core: &RyeOsCore, instance: &RyeOsViewInstanceKey) -> UiCompositionRouteAssignment {
    let facet = super::seat::input_route_facet_key(instance);
    let pending = core
        .seat
        .events()
        .get(core.presentation_operations.acknowledged_prefix_len..)
        .unwrap_or_default()
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            super::seat::SeatEventKind::Facet { key, value } if key == &facet => {
                Some(value.clone())
            }
            _ => None,
        });
    pending
        // Presence owns the override even when the pending facet explicitly
        // clears it. Do not flatten this outer Option into authored fallback.
        .map(|value| if value.is_null() { None } else { Some(value) })
        .or_else(|| {
            core.retained_input_routes
                .get(instance)
                .map(|(_, value)| value.clone())
        })
        .map_or(UiCompositionRouteAssignment::Authored, |value| {
            UiCompositionRouteAssignment::Override { input_route: value }
        })
}

fn copy_subject(
    core: &RyeOsCore,
    mount: &UiTransferMountExport,
    requested: &super::attachment::SelectionAttachment,
) -> Result<super::attachment::SelectionAttachment, String> {
    use super::attachment::SelectionAttachment;
    match core.selection_attachment_for_instance(&mount.source_instance) {
        Some(SelectionAttachment::Pinned { .. }) => {
            let binding = core
                .binding_for_instance(&mount.source_instance, &mount.view_ref)
                .ok_or("copy source binding is unavailable")?;
            let facets = super::attachment::selection_dependencies(binding)
                .into_iter()
                .collect::<Vec<_>>();
            if facets.is_empty() {
                return Err("copy source pin has no portable subject contract".into());
            }
            let SelectionAttachment::RequiredSubject {
                input,
                facets: requested_facets,
            } = requested
            else {
                return Err("copy of a pinned source requires an explicit portable subject".into());
            };
            if input.is_empty()
                || facets.len() != requested_facets.len()
                || !requested_facets.iter().all(|facet| facets.contains(facet))
            {
                return Err("copy portable subject differs from the source binding".into());
            }
            Ok(requested.clone())
        }
        Some(subject) if &subject == requested => Ok(subject),
        Some(_) => Err("copy subject differs from the live source".into()),
        None => Err("copy source mount has no exact subject".into()),
    }
}

fn pending_effect_belongs_to(
    effect: &super::effect::RyeOsEffectKind,
    instances: &BTreeSet<RyeOsViewInstanceKey>,
    set_ids: &BTreeSet<crate::ids::ViewSetId>,
    export: &UiTransferExport,
) -> bool {
    match effect {
        super::effect::RyeOsEffectKind::PrepareTransfer {
            export: pending, ..
        } => {
            pending
                .mounts()
                .into_iter()
                .any(|mount| instances.contains(&mount.source_instance))
                || pending.sets().is_some_and(|sets| {
                    sets.iter()
                        .any(|set| set_ids.contains(&set.source_view_set_id))
                })
        }
        // A selected phase contact can commit/remove source inventory or
        // activate destination inventory while its outcome is uncertain.
        // Without a fresh completed paired cut, no different local export can
        // prove noninterference; conservatively fence capture until rebootstrap.
        super::effect::RyeOsEffectKind::ContactTransfer { .. } => true,
        super::effect::RyeOsEffectKind::FetchSource { tile_id, .. } => {
            super::source_key::RyeOsSourceInstanceKey::decode(tile_id)
                .is_some_and(|key| instances.iter().any(|instance| key.belongs_to(instance)))
        }
        super::effect::RyeOsEffectKind::InvokeBinding {
            invocation_origin,
            input_origin,
            ..
        } => {
            invocation_origin
                .as_ref()
                .is_some_and(|origin| instances.contains(origin))
                || input_origin
                    .as_ref()
                    .is_some_and(|origin| instances.contains(&origin.buffer.view_instance_key))
        }
        super::effect::RyeOsEffectKind::InvokePresentationOperation { operation } => {
            operation
                .admission()
                .is_some_and(|admission| instances.contains(&admission.invocation_origin))
                || operation.composition().is_some_and(|request| {
                    super::transfer_export::pending_request_affects_export(
                        request, instances, set_ids, export,
                    )
                })
        }
        super::effect::RyeOsEffectKind::ReleaseBindingAttachment {
            binding_attachment_id,
            binding_generation,
            binding_digest,
        } => {
            let matches = |binding: &super::binding::UiBindingAttachmentCoordinate| {
                binding.binding_attachment_id == *binding_attachment_id
                    && binding.binding_generation == *binding_generation
                    && binding.binding_digest == *binding_digest
            };
            export.mounts().into_iter().any(|mount| {
                matches(&mount.binding)
                    || mount
                        .lens_frames
                        .iter()
                        .any(|frame| matches(&frame.binding))
            }) || export
                .sets()
                .is_some_and(|sets| sets.iter().any(|set| matches(&set.binding)))
        }
        // Replacement changes the presentation's authority as a whole. A
        // captured source snapshot cannot pretend this contact is unrelated.
        super::effect::RyeOsEffectKind::ReplaceSession { .. } => true,
        _ => false,
    }
}

fn exact_drafts(
    core: &RyeOsCore,
    mount: &UiTransferMountExport,
) -> Result<Vec<UiTransferDraftExport>, String> {
    let allowed_scopes = mount.draft_scopes();
    let mut drafts = Vec::new();
    let mut keys = BTreeSet::new();
    for (storage_key, state) in &core.input_buffers {
        if !InputBufferKey::storage_key_belongs_to(storage_key, &mount.source_instance) {
            continue;
        }
        let key = InputBufferKey::from_storage_key(storage_key)
            .ok_or("transfer source draft key is malformed")?;
        if key.view_instance_key != mount.source_instance
            || !allowed_scopes.contains(&(key.view_ref.clone(), key.binding_scope.clone()))
        {
            return Err("transfer source contains a draft outside its exact mount scope".into());
        }
        if key.storage_key() != *storage_key || !keys.insert(key.clone()) {
            return Err("transfer source draft key is noncanonical or repeated".into());
        }
        drafts.push(UiTransferDraftExport {
            key,
            state: state.clone(),
        });
    }
    drafts.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(drafts)
}

#[cfg(test)]
mod tests {
    #[test]
    fn route_capture_preserves_explicit_clear_over_authored_or_retained_route() {
        let (mut core, _, key) = fixture(UiTransferExportMode::Move);
        let instance = &key.view_instance_key;
        core.retained_input_routes.remove(instance);
        core.presentation_operations.acknowledged_prefix_len = core.seat.events().len();
        assert!(matches!(
            route(&core, instance),
            UiCompositionRouteAssignment::Authored
        ));
        core.retained_input_routes
            .insert(instance.clone(), (0, None));
        assert!(matches!(
            route(&core, instance),
            UiCompositionRouteAssignment::Override { input_route: None }
        ));
        core.retained_input_routes.insert(
            instance.clone(),
            (0, Some(serde_json::json!({"thread":"retained"}))),
        );
        core.seat.append_facet(
            super::super::seat::input_route_facet_key(instance),
            serde_json::Value::Null,
        );
        assert!(matches!(
            route(&core, instance),
            UiCompositionRouteAssignment::Override { input_route: None }
        ));
    }

    use super::*;
    use crate::ids::UiViewMountOwner;
    use crate::ui::attachment::SelectionAttachment;
    use crate::ui::binding::UiBindingAttachmentCoordinate;
    use crate::ui::effect::RyeOsEffectKind;
    use crate::ui::reducer::test_support;
    use crate::view_set::ViewLocalState;

    const MAX_BYTES: u64 = 64 * 1024;

    #[test]
    fn pending_phase_contact_fences_source_capture_until_observation() {
        use ryeos_ui_presentation_contract::transfer::{TransferIdentity, TransferSeat};
        let (_, requested, _) = fixture(UiTransferExportMode::Copy);
        let transfer = TransferIdentity {
            operation_id: "11111111-1111-4111-8111-111111111111".into(),
            source: TransferSeat {
                presentation_id: "22222222-2222-4222-8222-222222222222".into(),
                thread_id: "source".into(),
                chain_root_id: "source".into(),
                producer_incarnation: "a".repeat(64),
                revision: 0,
            },
            destination: TransferSeat {
                presentation_id: "33333333-3333-4333-8333-333333333333".into(),
                thread_id: "destination".into(),
                chain_root_id: "destination".into(),
                producer_incarnation: "b".repeat(64),
                revision: 0,
            },
            export_digest: "c".repeat(64),
        };
        let effect = RyeOsEffectKind::ContactTransfer {
            request: super::super::effect::UiTransferContactRequest::Commit { transfer },
            request_bounds: super::super::binding::UiBindingRequestBounds {
                max_request_bytes: MAX_BYTES,
                max_input_bytes: MAX_BYTES,
                max_seat_append_bytes: MAX_BYTES,
            },
        };
        assert!(pending_effect_belongs_to(
            &effect,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &requested,
        ));
    }

    #[test]
    fn draft_storage_parser_is_owned_and_canonical() {
        let (_, _, key) = fixture(UiTransferExportMode::Move);
        assert_eq!(
            InputBufferKey::from_storage_key(&key.storage_key()),
            Some(key.clone())
        );
        assert!(
            InputBufferKey::from_storage_key(&format!("{}\u{1f}", key.storage_key())).is_none()
        );
        let mut bad = key.clone();
        bad.binding_scope = "AB".repeat(32);
        assert!(InputBufferKey::from_storage_key(&bad.storage_key()).is_none());
        bad = key;
        bad.target_scope = Some("chain:retained".into());
        assert_eq!(
            InputBufferKey::from_storage_key(&bad.storage_key()),
            Some(bad)
        );
    }

    fn fixture(mode: UiTransferExportMode) -> (RyeOsCore, UiTransferExport, InputBufferKey) {
        let mut core = test_support::core(test_support::session_with_surface(serde_json::json!({
            "name":"Transfer subject fixture", "tiles":["view:test/subject"],
            "views":{"view:test/subject":{"widget":"rows",
                "input":{"id":"query"},
                "sources":{"default":{"ref":"service:test/read","params":{"work":"@facet:selection.work"}}}
            }}
        })));
        let (set_id, instance, binding, view_ref, local, seed) = {
            let surface = core.active_surface().unwrap();
            let set = surface.active_view_set().unwrap();
            let tile_id = set.root.as_ref().unwrap().tile_ids()[0];
            let tile = &set.tiles[&tile_id];
            let instance = tile.instance_key.clone();
            let binding = core
                .presentation
                .as_ref()
                .unwrap()
                .mount(&instance)
                .unwrap()
                .binding()
                .clone();
            (
                set.id,
                instance,
                binding,
                tile.view.view_ref.clone(),
                tile.local.clone(),
                super::super::layout_preferences::capture_view_set(set, "set".into()).unwrap(),
            )
        };
        core.selection_attachments.insert(
            instance.clone(),
            SelectionAttachment::RequiredSubject {
                input: "subject".into(),
                facets: vec!["selection.work".into()],
            },
        );
        assert_eq!(
            core.selection_attachment_for_instance(&instance),
            Some(SelectionAttachment::RequiredSubject {
                input: "subject".into(),
                facets: vec!["selection.work".into()],
            }),
            "fixture must mount a selection-dependent source: {:?}",
            core.binding_for_instance(&instance, &view_ref)
        );
        let draft_key = InputBufferKey::from_binding_coordinate(
            instance.clone(),
            view_ref.clone(),
            "query",
            &binding,
        );
        core.input_buffers.insert(
            draft_key.storage_key(),
            super::super::model::RyeOsInputState {
                text: "draft".into(),
                cursor: 5,
                filter_field: 0,
            },
        );
        let owner = instance.mount_owner().unwrap();
        let UiViewMountOwner::Surface { .. } = &owner else {
            panic!("fixture mount is Surface-owned")
        };
        let insertion_id = core.insertion_attachment_id(set_id).unwrap().to_owned();
        let insertion = core.binding_attachment(&insertion_id).unwrap();
        let insertion = UiBindingAttachmentCoordinate {
            binding_attachment_id: insertion.binding_attachment_id.clone(),
            binding_generation: insertion.binding_generation,
            binding_digest: insertion.binding_digest.clone(),
        };
        let requested = UiTransferExport {
            schema: super::super::transfer_export::UI_TRANSFER_EXPORT_SCHEMA,
            mode,
            content: UiTransferExportContent::ViewSets {
                active_set: Some("set".into()),
                sets: vec![super::super::transfer_export::UiTransferSetExport {
                    handle: "set".into(),
                    source_view_set_id: set_id,
                    source_owner: owner,
                    binding: insertion,
                    seed,
                    focused_view: Some(0),
                    mounts: vec![UiTransferMountExport {
                        source_instance: instance,
                        view_ref,
                        binding,
                        local: if mode == UiTransferExportMode::Copy {
                            ViewLocalState::None
                        } else {
                            local
                        },
                        subject: SelectionAttachment::RequiredSubject {
                            input: "subject".into(),
                            facets: vec!["selection.work".into()],
                        },
                        route: UiCompositionRouteAssignment::Authored,
                        current_label: None,
                        drafts: Vec::new(),
                        lens_frames: Vec::new(),
                    }],
                }],
            },
        };
        (core, requested, draft_key)
    }

    #[test]
    fn capture_refuses_exact_binding_release_but_not_another_generation() {
        let (mut core, requested, _) = fixture(UiTransferExportMode::Move);
        let binding = requested.mounts()[0].binding.clone();
        core.pending_effects.insert(
            1,
            super::super::effect::RyeOsEffectKind::ReleaseBindingAttachment {
                binding_attachment_id: binding.binding_attachment_id.clone(),
                binding_generation: binding.binding_generation,
                binding_digest: binding.binding_digest.clone(),
            },
        );
        assert!(
            capture_transfer_export_from_core(&core, &requested, MAX_BYTES)
                .unwrap_err()
                .contains("pending local effect")
        );
        core.pending_effects.insert(
            1,
            super::super::effect::RyeOsEffectKind::ReleaseBindingAttachment {
                binding_attachment_id: binding.binding_attachment_id,
                binding_generation: binding.binding_generation + 1,
                binding_digest: binding.binding_digest,
            },
        );
        assert!(capture_transfer_export_from_core(&core, &requested, MAX_BYTES).is_ok());
    }

    #[test]
    fn capture_refuses_session_replacement() {
        let (mut core, requested, _) = fixture(UiTransferExportMode::Move);
        core.pending_effects.insert(
            1,
            super::super::effect::RyeOsEffectKind::ReplaceSession {
                session_id: "successor".into(),
                launch_url: "/ui".into(),
            },
        );
        assert!(
            capture_transfer_export_from_core(&core, &requested, MAX_BYTES)
                .unwrap_err()
                .contains("pending local effect")
        );
    }

    #[test]
    fn pending_admission_is_scoped_to_its_exact_exported_origin() {
        use super::super::{admission_operation::*, binding::*, effect::RyeOsEffectKind};
        let (core, requested, _) = fixture(UiTransferExportMode::Move);
        let mount = requested.mounts()[0];
        let binding = mount.binding.clone();
        // Classifier fixture only: this record is never signed or dispatched.
        let mut operation = UiPreparedPresentationOperation {
            schema: 2,
            operation_id: "fixture".into(),
            prepared_engine_seq: 0,
            producer: UiPresentationProducer {
                presentation_id: "fixture".into(),
                thread_id: "fixture".into(),
                producer_incarnation: "a".repeat(64),
            },
            root_binding: binding.clone(),
            request_bounds: UiBindingRequestBounds {
                max_request_bytes: MAX_BYTES,
                max_input_bytes: MAX_BYTES,
                max_seat_append_bytes: MAX_BYTES,
            },
            body: UiPreparedPresentationOperationBody::Admission(UiPreparedAdmissionOperation {
                purpose: UiPresentationOperationKind::OpenSurface,
                surface_open_target: None,
                particular_selection: None,
                invocation_origin: mount.source_instance.clone(),
                success_notice: None,
                request: UiBindingRequest {
                    binding_attachment_id: binding.binding_attachment_id,
                    binding_generation: binding.binding_generation,
                    binding_digest: binding.binding_digest,
                    coordinate: UiBindingCoordinate::Affordance {
                        view_ref: mount.view_ref.clone(),
                        affordance_id: "open".into(),
                    },
                    payload: UiBindingPayload::Selection {
                        record: serde_json::json!({}),
                    },
                },
            }),
        };
        let instances = BTreeSet::from([mount.source_instance.clone()]);
        assert!(pending_effect_belongs_to(
            &RyeOsEffectKind::InvokePresentationOperation {
                operation: operation.clone()
            },
            &instances,
            &BTreeSet::new(),
            &requested,
        ));
        let UiPreparedPresentationOperationBody::Admission(admission) = &mut operation.body else {
            unreachable!()
        };
        admission.invocation_origin = RyeOsViewInstanceKey::surface_tile(
            core.presentation.as_ref().unwrap().id(),
            core.active_surface().unwrap().id(),
            crate::ids::TileId::new(999),
        );
        assert!(!pending_effect_belongs_to(
            &RyeOsEffectKind::InvokePresentationOperation { operation },
            &instances,
            &BTreeSet::new(),
            &requested,
        ));
    }

    #[test]
    fn copy_return_frames_keep_subject_contract_without_borrowing_route_or_drafts() {
        let (mut core, mut requested, _) = fixture(UiTransferExportMode::Copy);
        let mount = requested.mounts()[0];
        let frame = super::super::composition_lens::RetainedLensFrame {
            binding: mount.binding.clone(),
            view_ref: mount.view_ref.clone(),
            attachment: mount.subject.clone(),
            input_route: Some(serde_json::json!({"target":"old"})),
            label: Some("Return".into()),
        };
        core.retained_lens_frames
            .insert(mount.source_instance.clone(), vec![frame.clone()]);
        let UiTransferExportContent::ViewSets { sets, .. } = &mut requested.content else {
            unreachable!()
        };
        let mut portable = frame;
        portable.input_route = None;
        sets[0].mounts[0].lens_frames = vec![portable.clone()];
        let captured = capture_transfer_export_from_core(&core, &requested, MAX_BYTES).unwrap();
        assert_eq!(captured.mounts()[0].lens_frames, vec![portable]);
        assert!(captured.mounts()[0].drafts.is_empty());
        let UiTransferExportContent::ViewSets { sets, .. } = &mut requested.content else {
            unreachable!()
        };
        sets[0].mounts[0].lens_frames.clear();
        assert!(
            capture_transfer_export_from_core(&core, &requested, MAX_BYTES)
                .unwrap_err()
                .contains("every retained return frame")
        );
    }

    #[test]
    fn move_capture_requires_the_complete_exact_draft_inventory() {
        let (mut core, requested, key) = fixture(UiTransferExportMode::Move);
        let captured = capture_transfer_export_from_core(&core, &requested, MAX_BYTES).unwrap();
        assert_eq!(captured.mounts()[0].drafts.len(), 1);

        let mut missing = captured.clone();
        let UiTransferExportContent::ViewSets { sets, .. } = &mut missing.content else {
            unreachable!()
        };
        sets[0].mounts[0].drafts.clear();
        assert!(validate_transfer_export_against_core(&core, &missing, MAX_BYTES).is_err());

        core.input_buffers.get_mut(&key.storage_key()).unwrap().text = "changed".into();
        assert!(validate_transfer_export_against_core(&core, &captured, MAX_BYTES).is_err());
    }

    #[test]
    fn exact_drafts_includes_hidden_lens_scope_and_refuses_alien_scope() {
        let (mut core, requested, current_key) = fixture(UiTransferExportMode::Move);
        let mut mount = requested.mounts()[0].clone();
        let frame_binding = UiBindingAttachmentCoordinate {
            binding_attachment_id: "hidden-frame".into(),
            binding_generation: 2,
            binding_digest: "cd".repeat(32),
        };
        mount
            .lens_frames
            .push(super::super::composition_lens::RetainedLensFrame {
                binding: frame_binding.clone(),
                view_ref: "view:test/hidden".into(),
                attachment: SelectionAttachment::RequiredSubject {
                    input: "hidden-subject".into(),
                    facets: vec!["selection.work".into()],
                },
                input_route: None,
                label: None,
            });
        let hidden_key = InputBufferKey::from_binding_coordinate(
            mount.source_instance.clone(),
            "view:test/hidden",
            "query",
            &frame_binding,
        );
        core.input_buffers.insert(
            hidden_key.storage_key(),
            super::super::model::RyeOsInputState {
                text: "hidden".into(),
                cursor: 6,
                filter_field: 0,
            },
        );

        let drafts = exact_drafts(&core, &mount).unwrap();
        assert_eq!(drafts.len(), 2);
        assert!(drafts.iter().any(|draft| draft.key == current_key));
        assert!(drafts.iter().any(|draft| draft.key == hidden_key));

        let alien_binding = UiBindingAttachmentCoordinate {
            binding_attachment_id: "alien".into(),
            binding_generation: 3,
            binding_digest: "ab".repeat(32),
        };
        let alien_key = InputBufferKey::from_binding_coordinate(
            mount.source_instance.clone(),
            "view:test/hidden",
            "alien",
            &alien_binding,
        );
        core.input_buffers.insert(
            alien_key.storage_key(),
            super::super::model::RyeOsInputState::default(),
        );
        assert!(exact_drafts(&core, &mount).is_err());
    }

    #[test]
    fn copy_capture_drops_renderer_local_state_and_drafts() {
        let (core, requested, _) = fixture(UiTransferExportMode::Copy);
        let captured = capture_transfer_export_from_core(&core, &requested, MAX_BYTES).unwrap();
        let mount = captured.mounts()[0];
        assert_eq!(mount.local, ViewLocalState::None);
        assert!(mount.drafts.is_empty());
        assert_eq!(mount.route, UiCompositionRouteAssignment::Authored);
    }

    #[test]
    fn frozen_move_refuses_later_local_mutation() {
        let (mut core, requested, _) = fixture(UiTransferExportMode::Move);
        let captured = capture_transfer_export_from_core(&core, &requested, MAX_BYTES).unwrap();
        let instance = captured.mounts()[0].source_instance.clone();
        core.selection_attachments.insert(
            instance,
            SelectionAttachment::FollowViewSet {
                view_set_id: captured.sets().unwrap()[0].source_view_set_id,
            },
        );
        assert!(validate_transfer_export_against_core(&core, &captured, MAX_BYTES).is_err());
    }
}
