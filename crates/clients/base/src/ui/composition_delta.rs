//! Pure atomic local delta staging. Current authority is checked by the caller.
use super::binding::UiBindingAttachment;
use super::composition_identity::UiSurfaceMountManifest;
use super::composition_operation::{CompositionMutation, UiCompositionRequest};
use super::presentation_state::PresentationComposition;

pub fn composition_split_guard(root: &crate::layout::LayoutTree) -> Result<String, String> {
    let value = serde_json::to_value(root).map_err(|error| error.to_string())?;
    super::seat_digest::seat_payload_digest(&value).map_err(|error| error.to_string())
}
fn validate_subject(
    descriptor: &UiBindingAttachment,
    view_ref: &str,
    subject: &SavedViewSelectionSource,
    operation_bound: u64,
) -> Result<(), String> {
    let raw = descriptor
        .presentation
        .views
        .get(view_ref)
        .ok_or("subject View absent from closure")?;
    let binding: super::content::ViewBinding =
        serde_json::from_value(raw.clone()).map_err(|error| error.to_string())?;
    super::composition_subjects::validate_portable_subject(
        subject,
        &binding,
        descriptor
            .binding_request_bounds
            .max_request_bytes
            .min(operation_bound),
    )
}
use crate::ids::{RyeOsViewInstanceKey, ViewSetId};
use crate::surface::view_sets::{SavedViewMountRef, SavedViewSelectionSource};
use std::collections::BTreeMap;

pub enum UiCompositionSubjectAssignment {
    Portable {
        resolved_follow_set: Option<ViewSetId>,
        instance_key: RyeOsViewInstanceKey,
        view_set_id: ViewSetId,
        source: SavedViewSelectionSource,
    },
    Exact {
        instance_key: RyeOsViewInstanceKey,
        attachment: super::attachment::SelectionAttachment,
    },
}

fn stage_subject_delta(
    current: &PresentationComposition,
    request: &UiCompositionRequest,
    revision: u64,
    next_mount_id: u64,
    descriptors: &BTreeMap<String, UiBindingAttachment>,
    max_request_bytes: u64,
    instance: &RyeOsViewInstanceKey,
    attachment: &super::attachment::SelectionAttachment,
) -> Result<UiCompositionDelta, String> {
    use super::composition_operation::CompositionOwner;
    use crate::ids::UiViewMountOwner;
    let mount = current.mount(instance).ok_or("subject mount is absent")?;
    match (&request.owner, instance.mount_owner()) {
        (
            CompositionOwner::Shared { binding },
            Some(UiViewMountOwner::Shared { presentation_id }),
        ) if &presentation_id == current.id()
            && binding == current.root_binding()
            && mount.binding() == binding => {}
        (
            CompositionOwner::Surface {
                surface_instance_id,
                binding,
            },
            Some(UiViewMountOwner::Surface {
                presentation_id,
                surface_instance_id: mounted_owner,
            }),
        ) if &presentation_id == current.id()
            && surface_instance_id == &mounted_owner
            && current
                .surfaces()
                .get(surface_instance_id)
                .is_some_and(|surface| surface.binding() == binding) => {}
        _ => return Err("subject mount differs from exact composition owner".into()),
    }
    let descriptor = descriptors
        .get(&mount.binding().binding_attachment_id)
        .ok_or("subject descriptor absent")?;
    descriptor.validate().map_err(str::to_owned)?;
    if descriptor.binding_generation != mount.binding().binding_generation
        || descriptor.binding_digest != mount.binding().binding_digest
    {
        return Err("subject descriptor coordinate differs".into());
    }
    let raw = descriptor
        .presentation
        .views
        .get(mount.view_ref())
        .ok_or("subject View absent from exact closure")?;
    let view: super::content::ViewBinding =
        serde_json::from_value(raw.clone()).map_err(|error| error.to_string())?;
    let dependencies = super::attachment::selection_dependencies(&view);
    let bound = max_request_bytes.min(descriptor.binding_request_bounds.max_request_bytes);
    if bound == 0
        || serde_json::to_vec(attachment)
            .map_err(|error| error.to_string())?
            .len() as u64
            > bound
    {
        return Err("subject attachment exceeds exact binding byte bound".into());
    }
    match attachment {
        super::attachment::SelectionAttachment::FollowViewSet { view_set_id } => {
            let owner = request
                .owner
                .surface_instance_id()
                .ok_or("shared views cannot follow a Surface selection")?;
            current
                .view_set(owner, *view_set_id)
                .ok_or("subject FollowViewSet is not in the same Surface")?;
        }
        super::attachment::SelectionAttachment::Pinned {
            values,
            fingerprint,
        } => {
            use sha2::{Digest, Sha256};
            if values
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                != dependencies
                || values.values().any(serde_json::Value::is_null)
                || *fingerprint
                    != format!(
                        "{:x}",
                        Sha256::digest(
                            serde_json::to_vec(values).map_err(|error| error.to_string())?
                        )
                    )
            {
                return Err("pinned subject dependencies or fingerprint differ".into());
            }
        }
        super::attachment::SelectionAttachment::RequiredSubject { input, facets } => {
            if input.trim().is_empty()
                || input.trim() != input
                || input.chars().any(char::is_control)
                || facets.is_empty()
                || facets.len() != dependencies.len()
                || facets
                    .iter()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    != dependencies
            {
                return Err("required subject differs from exact View dependencies".into());
            }
        }
    }
    let mut affected_bindings = vec![request.owner.binding().clone()];
    if !affected_bindings.contains(mount.binding()) {
        affected_bindings.push(mount.binding().clone());
    }
    Ok(UiCompositionDelta {
        mount_remaps: BTreeMap::new(),
        allocated_mounts: None,
        affected_bindings,
        composition: current.clone(),
        revision: revision
            .checked_add(1)
            .ok_or("composition revision exhausted")?,
        next_mount_id,
        allocated_manifest: None,
        subjects: vec![UiCompositionSubjectAssignment::Exact {
            instance_key: instance.clone(),
            attachment: attachment.clone(),
        }],
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::composition_operation::CompositionOwner;
    #[test]
    fn move_destination_must_identify_the_actual_placement() {
        use super::super::composition_operation::CompositionPlacement;
        use crate::ids::{TileId, ViewGroupId};
        use crate::layout::{LayoutTree, SplitAxis};
        let root = LayoutTree::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutTree::Group {
                group_id: ViewGroupId::new(1),
                label: None,
                tabs: vec![TileId::new(1), TileId::new(2)],
                active: TileId::new(1),
            }),
            second: Box::new(LayoutTree::Group {
                group_id: ViewGroupId::new(2),
                label: None,
                tabs: vec![TileId::new(3)],
                active: TileId::new(3),
            }),
        };
        let split = |target| CompositionPlacement::Split {
            target,
            axis: SplitAxis::Horizontal,
            before: false,
            ratio: 0.5,
        };
        assert!(validate_move_destination(&root, TileId::new(2), &split(TileId::new(3))).is_err());
        assert!(validate_move_destination(&root, TileId::new(2), &split(TileId::new(2))).is_ok());
        assert!(
            validate_move_destination(
                &root,
                TileId::new(2),
                &CompositionPlacement::GroupTab {
                    group_id: ViewGroupId::new(2),
                    before: None
                },
            )
            .is_err()
        );
        assert!(
            validate_move_destination(
                &root,
                TileId::new(2),
                &CompositionPlacement::GroupTab {
                    group_id: ViewGroupId::new(1),
                    before: Some(TileId::new(1))
                },
            )
            .is_ok()
        );
        assert!(
            validate_move_destination(&root, TileId::new(2), &CompositionPlacement::First).is_err()
        );
    }
    fn fixture() -> (
        PresentationComposition,
        UiCompositionRequest,
        BTreeMap<String, UiBindingAttachment>,
        u64,
    ) {
        let session = crate::ui::reducer::test_support::session();
        let member = &session.presentations[0];
        let descriptors = session
            .binding_attachments
            .iter()
            .map(|entry| (entry.binding_attachment_id.clone(), entry.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut composition = PresentationComposition::from_membership(
            member,
            &descriptors[&member.root_binding.binding_attachment_id],
        )
        .unwrap();
        for surface in &member.surfaces {
            composition
                .open_surface(
                    surface,
                    &descriptors[&surface.binding.binding_attachment_id],
                )
                .unwrap();
        }
        let surface = &member.surfaces[0];
        let request = UiCompositionRequest {
            schema: 1,
            presentation_id: composition.id().clone(),
            operation_id: "22222222-2222-4222-8222-222222222222".into(),
            expected_revision: 0,
            expected_next_mount_id: member.next_mount_id,
            owner: super::super::composition_operation::CompositionOwner::Surface {
                surface_instance_id: crate::ids::SurfaceInstanceId::try_new(
                    surface.surface_instance_id.clone(),
                )
                .unwrap(),
                binding: surface.binding.clone(),
            },
            mutation: CompositionMutation::RenameTab {
                tab_id: composition.active_view_set().unwrap().id,
                title: "Renamed".into(),
            },
        };
        (composition, request, descriptors, member.next_mount_id)
    }
    #[test]
    fn surface_rename_stages_only_title_and_one_revision_without_allocation() {
        let (composition, mut request, descriptors, frontier) = fixture();
        let before = format!("{composition:?}");
        let owner = request.owner.surface_instance_id().unwrap().clone();
        let original = &composition.surfaces()[&owner];
        request.mutation = CompositionMutation::RenameSurface {
            title: "Research".into(),
        };
        let delta =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        let renamed = &delta.composition.surfaces()[&owner];
        assert_eq!(renamed.title(), "Research");
        assert_eq!(renamed.id(), original.id());
        assert_eq!(renamed.ordinal(), original.ordinal());
        assert_eq!(renamed.binding(), original.binding());
        assert_eq!(
            format!("{:?}", renamed.view_sets()),
            format!("{:?}", original.view_sets())
        );
        assert_eq!(delta.revision, 1);
        assert_eq!(delta.next_mount_id, frontier);
        assert!(delta.allocated_manifest.is_none());
        assert!(delta.allocated_mounts.is_none());
        assert!(delta.mount_remaps.is_empty());
        let receipt = super::super::composition_completion::UiCompositionCompletion::from_delta(
            &request, &delta,
        )
        .unwrap();
        assert_eq!(receipt.next_mount_id, frontier);
        assert!(receipt.allocated_manifest.is_none());
        assert!(receipt.allocated_mounts.is_none());
        assert_eq!(format!("{composition:?}"), before);
        if let CompositionOwner::Surface { binding, .. } = &mut request.owner {
            binding.binding_generation += 1;
        }
        assert!(
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .is_err()
        );
        assert_eq!(format!("{composition:?}"), before);
    }

    #[test]
    fn cross_surface_stage_preserves_binding_and_refuses_changed_destination() {
        use super::super::composition_operation::{CompositionPlacement, CompositionTileTarget};
        let (mut composition, mut request, descriptors, frontier) = fixture();
        let session = crate::ui::reducer::test_support::session();
        let mut destination = session.presentations[0].surfaces[0].clone();
        destination.surface_instance_id = "55555555-5555-4555-8555-555555555555".into();
        destination.ordinal = 2;
        let descriptor = &descriptors[&destination.binding.binding_attachment_id];
        let seeds = descriptor
            .presentation
            .definition
            .surface()
            .unwrap()
            .to_mount_seeds()
            .unwrap();
        let active = seeds.first().map(|seed| seed.id.clone());
        destination.mount_manifest = UiSurfaceMountManifest::allocate(
            composition.id().clone(),
            crate::ids::SurfaceInstanceId::try_new(destination.surface_instance_id.clone())
                .unwrap(),
            frontier,
            seeds
                .into_iter()
                .map(|seed| (seed, destination.binding.clone()))
                .collect(),
            active.as_deref(),
        )
        .unwrap();
        let next = destination.mount_manifest.next_mount_id;
        composition.open_surface(&destination, descriptor).unwrap();
        let source_surface = request.owner.surface_instance_id().unwrap().clone();
        let destination_surface =
            crate::ids::SurfaceInstanceId::try_new(destination.surface_instance_id).unwrap();
        let source_set = composition.surfaces()[&source_surface]
            .active_view_set()
            .unwrap();
        let source_tile = source_set.focused_tile;
        let old_key = source_set.tiles[&source_tile].instance_key.clone();
        let old_binding = composition.mount(&old_key).unwrap().binding().clone();
        let target = composition.surfaces()[&destination_surface]
            .active_view_set()
            .unwrap();
        let target_tile = target.focused_tile;
        request.expected_next_mount_id = next;
        request.mutation = CompositionMutation::MoveViewAcrossSurfaces {
            source: CompositionTileTarget {
                surface_instance_id: source_surface.clone(),
                view_set_id: source_set.id,
                tile_id: source_tile,
            },
            destination: CompositionTileTarget {
                surface_instance_id: destination_surface,
                view_set_id: target.id,
                tile_id: target_tile,
            },
            destination_binding: destination.binding,
            placement: CompositionPlacement::Split {
                target: target_tile,
                axis: crate::layout::SplitAxis::Horizontal,
                before: false,
                ratio: 0.5,
            },
        };
        let before = format!("{composition:?}");
        let delta =
            stage_composition_delta(&composition, &request, 0, next, &descriptors, 65536).unwrap();
        assert_eq!(
            format!("{composition:?}"),
            before,
            "staging never changes the live composition"
        );
        let new_key = &delta.mount_remaps[&old_key];
        assert!(delta.composition.mount(&old_key).is_none());
        assert_eq!(
            delta.composition.mount(new_key).unwrap().binding(),
            &old_binding
        );
        assert_eq!(
            delta.composition.active_surface().unwrap().id(),
            composition.active_surface().unwrap().id(),
            "moving a View does not retarget the currently active Surface"
        );
        assert_eq!(
            delta.next_mount_id,
            next + 1,
            "only the new split group consumes an identity"
        );
        if let CompositionMutation::MoveViewAcrossSurfaces {
            destination_binding,
            ..
        } = &mut request.mutation
        {
            destination_binding.binding_generation += 1;
        }
        assert!(
            stage_composition_delta(&composition, &request, 0, next, &descriptors, 65536).is_err()
        );
        assert_eq!(format!("{composition:?}"), before);
    }

    #[test]
    fn allocated_view_placement_checks_actual_tab_without_mutating_it() {
        use super::super::composition_operation::CompositionPlacement;
        let (composition, _, _, frontier) = fixture();
        let set = composition.active_view_set().unwrap();
        let original = set.root.clone();
        let allocation = UiCompositionMountAllocation::allocate(frontier, 1, 0).unwrap();
        fn first_group(root: &crate::layout::LayoutTree) -> crate::ids::ViewGroupId {
            match root {
                crate::layout::LayoutTree::Group { group_id, .. } => *group_id,
                crate::layout::LayoutTree::Split { first, .. } => first_group(first),
            }
        }
        assert!(
            validate_allocated_view_placement(
                set,
                &allocation,
                &CompositionPlacement::GroupTab {
                    group_id: first_group(set.root.as_ref().unwrap()),
                    before: None,
                },
            )
            .is_ok()
        );
        assert!(
            validate_allocated_view_placement(set, &allocation, &CompositionPlacement::First)
                .is_err()
        );
        assert!(
            validate_allocated_view_placement(
                set,
                &allocation,
                &CompositionPlacement::GroupTab {
                    group_id: crate::ids::ViewGroupId::new(frontier - 1),
                    before: Some(crate::ids::TileId::new(frontier - 1)),
                },
            )
            .is_err()
        );
        assert_eq!(set.root, original);
    }
    #[test]
    fn live_following_placement_is_atomic_and_rejects_missing_target() {
        use super::super::attachment::SelectionAttachment;
        use super::super::composition_operation::{
            CompositionPlacement, UiCompositionRouteAssignment,
        };
        let (composition, mut request, descriptors, frontier) = fixture();
        let set = composition.active_view_set().unwrap();
        let tile = &set.tiles[&set.focused_tile];
        let binding = composition
            .mount(&tile.instance_key)
            .unwrap()
            .binding()
            .clone();
        let attachment = SelectionAttachment::FollowViewSet {
            view_set_id: set.id,
        };
        for replace in [false, true] {
            request.mutation = if replace {
                CompositionMutation::ReplaceFollowingView {
                    tab_id: set.id,
                    tile_id: set.focused_tile,
                    view_ref: tile.view.view_ref.clone(),
                    binding: binding.clone(),
                    attachment: attachment.clone(),
                    route_assignment: UiCompositionRouteAssignment::Authored,
                }
            } else {
                CompositionMutation::InsertFollowingView {
                    tab_id: set.id,
                    placement: CompositionPlacement::Split {
                        target: set.focused_tile,
                        axis: crate::layout::SplitAxis::Horizontal,
                        before: false,
                        ratio: 0.5,
                    },
                    view_ref: tile.view.view_ref.clone(),
                    binding: binding.clone(),
                    attachment: attachment.clone(),
                    route_assignment: UiCompositionRouteAssignment::Authored,
                }
            };
            let delta =
                stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                    .unwrap();
            assert!(
                matches!(&delta.subjects[..], [UiCompositionSubjectAssignment::Exact { attachment:actual,.. }] if actual == &attachment)
            );
            assert_eq!(delta.next_mount_id, frontier + if replace { 0 } else { 2 });
            let receipt =
                super::super::composition_completion::UiCompositionCompletion::from_delta(
                    &request, &delta,
                )
                .unwrap();
            assert_eq!(
                receipt.request_digest,
                super::super::composition_completion::UiCompositionCompletion::request_digest(
                    &request
                )
                .unwrap()
            );
            let mut invalid = request.clone();
            match &mut invalid.mutation {
                CompositionMutation::InsertFollowingView { attachment, .. }
                | CompositionMutation::ReplaceFollowingView { attachment, .. } => {
                    *attachment = SelectionAttachment::FollowViewSet {
                        view_set_id: crate::ids::ViewSetId::new(u64::MAX),
                    };
                }
                _ => unreachable!(),
            }
            assert!(
                stage_composition_delta(&composition, &invalid, 0, frontier, &descriptors, 65536)
                    .is_err()
            );
            assert_eq!(
                composition.active_view_set().unwrap().tile_ids(),
                set.tile_ids()
            );
        }
    }

    #[test]
    fn pinned_insertion_allocates_once_and_commits_original_request_digest() {
        use super::super::attachment::SelectionAttachment;
        use super::super::composition_operation::CompositionPlacement;
        let (composition, mut request, mut descriptors, frontier) = fixture();
        let set = composition.active_view_set().unwrap();
        let original_tiles = set.tile_ids();
        let tile = &set.tiles[&set.focused_tile];
        let binding = composition
            .mount(&tile.instance_key)
            .unwrap()
            .binding()
            .clone();
        let view_ref = tile.view.view_ref.clone();
        descriptors
            .get_mut(&binding.binding_attachment_id)
            .unwrap()
            .presentation
            .views
            .insert(
                view_ref.clone(),
                serde_json::json!({"facet":"selection.work"}),
            );
        let values = BTreeMap::from([(
            "selection.work".into(),
            serde_json::json!({"thread_id":"T-observed"}),
        )]);
        use sha2::{Digest, Sha256};
        let attachment = SelectionAttachment::Pinned {
            fingerprint: format!("{:x}", Sha256::digest(serde_json::to_vec(&values).unwrap())),
            values,
        };
        request.mutation = CompositionMutation::InsertPinnedView {
            tab_id: set.id,
            placement: CompositionPlacement::Split {
                target: set.focused_tile,
                axis: crate::layout::SplitAxis::Horizontal,
                before: false,
                ratio: 0.5,
            },
            view_ref: view_ref.clone(),
            binding: binding.clone(),
            attachment: attachment.clone(),
            route_assignment:
                crate::ui::composition_operation::UiCompositionRouteAssignment::Authored,
        };
        let first =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        let second =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        assert_eq!(first.allocated_mounts, second.allocated_mounts);
        assert_eq!(first.next_mount_id, frontier + 2);
        assert_eq!(first.revision, 1);
        assert_eq!(
            composition.active_view_set().unwrap().tile_ids(),
            original_tiles
        );
        assert!(
            matches!(&first.subjects[..], [UiCompositionSubjectAssignment::Exact { attachment: exact, .. }] if exact == &attachment)
        );
        let receipt = super::super::composition_completion::UiCompositionCompletion::from_delta(
            &request, &first,
        )
        .unwrap();
        assert_eq!(
            receipt.request_digest,
            super::super::composition_completion::UiCompositionCompletion::request_digest(&request)
                .unwrap()
        );
        let mut wrong = request.clone();
        if let CompositionMutation::InsertPinnedView { binding, .. } = &mut wrong.mutation {
            binding.binding_generation += 1;
        }
        assert!(
            stage_composition_delta(&composition, &wrong, 0, frontier, &descriptors, 65536)
                .is_err()
        );
        wrong = request.clone();
        if let CompositionMutation::InsertPinnedView {
            attachment: SelectionAttachment::Pinned { fingerprint, .. },
            ..
        } = &mut wrong.mutation
        {
            *fingerprint = "wrong".into();
        }
        assert!(
            stage_composition_delta(&composition, &wrong, 0, frontier, &descriptors, 65536)
                .is_err()
        );
        descriptors
            .get_mut(&binding.binding_attachment_id)
            .unwrap()
            .presentation
            .views
            .insert(view_ref, serde_json::json!({}));
        assert!(
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .is_err(),
            "no dependency is not a meaningful pin"
        );
    }

    #[test]
    fn exact_subject_changes_validate_pin_dependency_fingerprint_and_owner() {
        use super::super::attachment::SelectionAttachment;
        let (composition, mut request, mut descriptors, frontier) = fixture();
        let instance = composition
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let mount = composition.mount(&instance).unwrap();
        descriptors
            .get_mut(&mount.binding().binding_attachment_id)
            .unwrap()
            .presentation
            .views
            .insert(
                mount.view_ref().into(),
                serde_json::json!({"facet":"selection.work"}),
            );
        let values = BTreeMap::from([(
            "selection.work".into(),
            serde_json::json!({"thread_id":"T-work"}),
        )]);
        use sha2::{Digest, Sha256};
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&values).unwrap()));
        request.mutation = CompositionMutation::SetSubject {
            mount: instance.clone(),
            attachment: SelectionAttachment::Pinned {
                values: values.clone(),
                fingerprint,
            },
        };
        let delta =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        assert_eq!(delta.next_mount_id, frontier);
        assert!(
            matches!(&delta.subjects[0], UiCompositionSubjectAssignment::Exact { instance_key, .. } if instance_key == &instance)
        );
        request.mutation = CompositionMutation::SetSubject {
            mount: instance.clone(),
            attachment: SelectionAttachment::Pinned {
                values,
                fingerprint: "wrong".into(),
            },
        };
        assert!(
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .is_err()
        );
        request.mutation = CompositionMutation::SetSubject {
            mount: instance,
            attachment: SelectionAttachment::RequiredSubject {
                input: "work".into(),
                facets: vec!["selection.other".into()],
            },
        };
        assert!(
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .is_err()
        );
    }

    #[test]
    fn shared_subject_owner_cannot_borrow_surface_selection_or_layout() {
        use super::super::composition_operation::CompositionOwner;
        let (composition, mut request, descriptors, frontier) = fixture();
        let instance = composition
            .dialogs()
            .values()
            .next()
            .unwrap()
            .view()
            .instance_key()
            .clone();
        request.owner = CompositionOwner::Shared {
            binding: composition.root_binding().clone(),
        };
        request.mutation = CompositionMutation::SetSubject {
            mount: instance,
            attachment: super::super::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: composition.active_view_set().unwrap().id,
            },
        };
        assert!(
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .is_err()
        );
        request.mutation = CompositionMutation::RenameTab {
            tab_id: composition.active_view_set().unwrap().id,
            title: "No".into(),
        };
        assert!(request.validate(65536).is_err());
    }

    #[test]
    fn stale_wrong_owner_and_overflow_never_mutate_original() {
        let (composition, mut request, descriptors, frontier) = fixture();
        let original = composition.active_view_set().unwrap().title.clone();
        request.expected_revision = 1;
        assert!(
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .is_err()
        );
        request.expected_revision = 0;
        if let super::super::composition_operation::CompositionOwner::Surface { binding, .. } =
            &mut request.owner
        {
            binding.binding_generation += 1;
        }
        assert!(
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .is_err()
        );
        assert_eq!(composition.active_view_set().unwrap().title, original);
    }
    #[test]
    fn required_subject_matches_exact_dependency_and_bound() {
        let (_, request, mut descriptors, _) = fixture();
        let descriptor = descriptors
            .get_mut(&request.owner.binding().binding_attachment_id)
            .unwrap();
        let reference = descriptor.presentation.views.keys().next().unwrap().clone();
        descriptor.presentation.views.insert(
            reference.clone(),
            serde_json::json!({"facet":"selection.work"}),
        );
        let valid = SavedViewSelectionSource::RequiredSubject {
            input: "work".into(),
            facets: vec!["selection.work".into()],
        };
        assert!(validate_subject(descriptor, &reference, &valid, 65536).is_ok());
        let missing = SavedViewSelectionSource::RequiredSubject {
            input: "work".into(),
            facets: vec![],
        };
        assert!(validate_subject(descriptor, &reference, &missing, 65536).is_err());
        let wrong = SavedViewSelectionSource::RequiredSubject {
            input: "work".into(),
            facets: vec!["selection.project".into()],
        };
        assert!(validate_subject(descriptor, &reference, &wrong, 65536).is_err());
        assert!(validate_subject(descriptor, &reference, &valid, 1).is_err());
        descriptor.binding_request_bounds.max_request_bytes = 1;
        assert!(validate_subject(descriptor, &reference, &valid, 65536).is_err());
    }
    #[test]
    fn in_place_allocation_evidence_is_contiguous_and_preserves_tiles() {
        let valid = UiCompositionMountAllocation::allocate(20, 1, 2).unwrap();
        valid.validate().unwrap();
        let mut duplicate = valid.clone();
        duplicate.group_ids[0].0 = duplicate.tile_ids[0].0;
        assert!(duplicate.validate().is_err());
        assert!(UiCompositionMountAllocation::allocate(u64::MAX, 1, 0).is_err());
        let (composition, mut request, descriptors, frontier) = fixture();
        let tab = composition.active_view_set().unwrap();
        let original = tab.tile_ids();
        request.mutation = CompositionMutation::Arrange {
            tab_id: tab.id,
            axis: crate::layout::SplitAxis::Horizontal,
        };
        let first =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        let second =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        assert_eq!(first.allocated_mounts, second.allocated_mounts);
        assert_eq!(
            first
                .composition
                .view_set(request.owner.surface_instance_id().unwrap(), tab.id)
                .unwrap()
                .tile_ids(),
            original
        );
        assert_eq!(composition.active_view_set().unwrap().tile_ids(), original);
    }
    #[test]
    fn replacement_new_binding_is_authority_evidence_and_stale_triple_refuses() {
        let (composition, mut request, mut descriptors, frontier) = fixture();
        let tab = composition.active_view_set().unwrap();
        let tile = tab.focused_tile;
        let mut descriptor = descriptors[&request.owner.binding().binding_attachment_id].clone();
        descriptor.binding_attachment_id = "replacement".into();
        descriptor.binding_generation += 1;
        let reference = descriptor.presentation.views.keys().next().unwrap().clone();
        let binding = super::super::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: descriptor.binding_attachment_id.clone(),
            binding_generation: descriptor.binding_generation,
            binding_digest: descriptor.binding_digest.clone(),
        };
        descriptors.insert(descriptor.binding_attachment_id.clone(), descriptor);
        request.mutation = CompositionMutation::ReplaceView {
            tab_id: tab.id,
            tile_id: tile,
            view_ref: reference,
            binding: binding.clone(),
            subject: SavedViewSelectionSource::FollowOwnSet,
            route_assignment:
                crate::ui::composition_operation::UiCompositionRouteAssignment::Authored,
        };
        let delta =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        assert!(delta.affected_bindings.contains(&binding));
        assert_eq!(
            delta
                .affected_bindings
                .iter()
                .filter(|value| *value == &binding)
                .count(),
            1
        );
        descriptors
            .get_mut("replacement")
            .unwrap()
            .binding_generation += 1;
        assert!(
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .is_err()
        );
    }
    #[test]
    fn restored_nondefault_focus_uses_exact_manifest_and_preserves_other_group_selection() {
        let (composition, mut request, descriptors, frontier) = fixture();
        let original_id = composition.active_view_set().unwrap().id;
        let descriptor = &descriptors[&request.owner.binding().binding_attachment_id];
        let view_ref = descriptor.presentation.views.keys().next().unwrap();
        let template = serde_json::from_value(serde_json::json!({"id":"restore","name":"Restore",
            "composition":{"schema":2,"id":"restore","title":"Restore","root":{"type":"split","axis":"horizontal","ratio":0.5,
                "first":{"type":"group","views":[view_ref,view_ref],"active":1},
                "second":{"type":"group","views":[view_ref,view_ref],"active":0}}},
            "relationships":(0..4).map(|index|serde_json::json!({"mount":{"kind":"tile","index":index},"source":{"mode":"follow_own_set"}})).collect::<Vec<_>>()
        })).unwrap();
        request.mutation = CompositionMutation::RestoreTabs {
            tabs: vec![super::super::composition_operation::CompositionTabSeed {
                template,
                binding: request.owner.binding().clone(),
                focused_view: Some(3),
                fresh_subjects: BTreeMap::new(),
                followed_sets: BTreeMap::new(),
            }],
            active_seed_id: "restore".into(),
        };
        let staged =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        let manifest = staged.allocated_manifest.as_ref().unwrap();
        let set = staged.composition.active_view_set().unwrap();
        assert_eq!(set.focused_tile, manifest.tabs[0].tile_ids[3]);
        assert_eq!(
            set.root.as_ref().unwrap().active_tile_ids(),
            vec![manifest.tabs[0].tile_ids[1], manifest.tabs[0].tile_ids[3]]
        );
        assert_eq!(staged.revision, 1);
        assert_eq!(composition.active_view_set().unwrap().id, original_id);
        if let CompositionMutation::RestoreTabs { tabs, .. } = &mut request.mutation {
            tabs[0].focused_view = Some(4);
        }
        assert!(
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .is_err()
        );
    }

    #[test]
    fn inserted_tab_ids_and_unresolved_subjects_are_deterministic() {
        let (composition, mut request, descriptors, frontier) = fixture();
        let descriptor = &descriptors[&request.owner.binding().binding_attachment_id];
        let view_ref = descriptor.presentation.views.keys().next().unwrap().clone();
        let template=serde_json::from_value(serde_json::json!({"id":"new","name":"New","composition":{"schema":2,"id":"new","title":"New","root":{"type":"group","views":[view_ref],"active":0}},"relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"follow_own_set"}}]})).unwrap();
        request.mutation = CompositionMutation::InsertTab {
            index: 0,
            tab: super::super::composition_operation::CompositionTabSeed {
                template,
                binding: request.owner.binding().clone(),
                focused_view: None,
                fresh_subjects: BTreeMap::new(),
                followed_sets: BTreeMap::new(),
            },
            activate: true,
        };
        let first =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        let second =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        assert_eq!(first.allocated_manifest, second.allocated_manifest);
        assert_eq!(first.subjects.len(), 1);
        assert_eq!(first.revision, 1);
        assert!(first.next_mount_id > frontier);
        request.expected_next_mount_id = u64::MAX;
        assert!(
            stage_composition_delta(&composition, &request, 0, u64::MAX, &descriptors, 65536)
                .is_err()
        );
    }

    #[test]
    fn appended_collection_obeys_total_limit_and_rejects_late_descriptor_atomically() {
        let (composition, mut request, descriptors, frontier) = fixture();
        let before = composition.active_view_set().unwrap().id;
        let descriptor = &descriptors[&request.owner.binding().binding_attachment_id];
        let reference = descriptor.presentation.views.keys().next().unwrap();
        let make_tab = |index| {
            super::super::composition_operation::CompositionTabSeed {
            template:serde_json::from_value(serde_json::json!({"id":format!("tab-{index}"),"name":"Imported","composition":{"schema":2,"id":format!("tab-{index}"),"title":"Imported","root":{"type":"group","views":[reference],"active":0}},"relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"follow_own_set"}}]})).unwrap(),
            binding:request.owner.binding().clone(),fresh_subjects:BTreeMap::new(),followed_sets:BTreeMap::new(),focused_view:None,
        }
        };
        let tabs = (0..crate::surface::view_sets::MAX_VIEW_SETS)
            .map(make_tab)
            .collect::<Vec<_>>();
        request.mutation = CompositionMutation::AppendTabs {
            index: 1,
            tabs,
            active_seed_id: "tab-0".into(),
            activate: true,
        };
        assert!(
            stage_composition_delta(
                &composition,
                &request,
                0,
                frontier,
                &descriptors,
                1024 * 1024
            )
            .is_err(),
            "imported count alone is legal but existing plus imported exceeds bound"
        );
        let CompositionMutation::AppendTabs { tabs, .. } = &mut request.mutation else {
            unreachable!()
        };
        tabs.truncate(2);
        let accepted =
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .unwrap();
        assert_eq!(
            accepted
                .composition
                .active_surface()
                .unwrap()
                .view_sets()
                .len(),
            3
        );
        let CompositionMutation::AppendTabs { tabs, .. } = &mut request.mutation else {
            unreachable!()
        };
        tabs[1].binding.binding_generation += 1;
        assert!(
            stage_composition_delta(&composition, &request, 0, frontier, &descriptors, 65536)
                .is_err()
        );
        assert_eq!(composition.active_view_set().unwrap().id, before);
        assert_eq!(composition.active_surface().unwrap().view_sets().len(), 1);
    }
}
pub struct UiCompositionDelta {
    /// Deterministic relocation coordinates derived by atomic staging. They
    /// are applied only by the completed composition replay/ACK owner, never
    /// by preparation or transport callbacks.
    pub mount_remaps: BTreeMap<RyeOsViewInstanceKey, RyeOsViewInstanceKey>,
    pub allocated_mounts: Option<UiCompositionMountAllocation>,
    pub affected_bindings: Vec<super::binding::UiBindingAttachmentCoordinate>,
    pub composition: PresentationComposition,
    pub revision: u64,
    pub next_mount_id: u64,
    pub allocated_manifest: Option<UiSurfaceMountManifest>,
    pub subjects: Vec<UiCompositionSubjectAssignment>,
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiCompositionMountAllocation {
    pub schema: u32,
    pub allocation_start: u64,
    pub next_mount_id: u64,
    pub tile_ids: Vec<crate::ids::TileId>,
    pub group_ids: Vec<crate::ids::ViewGroupId>,
}
impl UiCompositionMountAllocation {
    pub fn allocate(start: u64, tiles: usize, groups: usize) -> Result<Self, String> {
        if start == 0
            || tiles
                .checked_add(groups)
                .is_none_or(|count| count > crate::layout::MAX_LAYOUT_TILES * 2)
        {
            return Err("invalid mount allocation shape".into());
        }
        let mut next = start;
        let mut take = || {
            let id = next;
            next = next.checked_add(1).ok_or("mount allocation exhausted")?;
            Ok::<u64, String>(id)
        };
        let tile_ids = (0..tiles)
            .map(|_| take().map(crate::ids::TileId::new))
            .collect::<Result<Vec<_>, _>>()?;
        let group_ids = (0..groups)
            .map(|_| take().map(crate::ids::ViewGroupId::new))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            schema: 1,
            allocation_start: start,
            next_mount_id: next,
            tile_ids,
            group_ids,
        })
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != 1
            || Self::allocate(
                self.allocation_start,
                self.tile_ids.len(),
                self.group_ids.len(),
            )? != *self
        {
            return Err("noncanonical mount allocation evidence".into());
        }
        Ok(())
    }
}
pub(crate) fn place(
    root: &mut Option<crate::layout::LayoutTree>,
    tile: crate::ids::TileId,
    placement: &super::composition_operation::CompositionPlacement,
    group: Option<crate::ids::ViewGroupId>,
) -> Result<(), String> {
    use super::composition_operation::CompositionPlacement;
    use crate::layout::LayoutTree;
    fn find_group(node: &mut LayoutTree, id: crate::ids::ViewGroupId) -> Option<&mut LayoutTree> {
        match node {
            LayoutTree::Group { group_id, .. } if *group_id == id => Some(node),
            LayoutTree::Split { first, second, .. } => {
                find_group(first, id).or_else(|| find_group(second, id))
            }
            _ => None,
        }
    }
    fn split(
        node: &mut LayoutTree,
        target: crate::ids::TileId,
        incoming: LayoutTree,
        axis: crate::layout::SplitAxis,
        before: bool,
        ratio: f32,
    ) -> bool {
        match node {
            LayoutTree::Group { tabs, .. } if tabs.contains(&target) => {
                let old = node.clone();
                let (first, second) = if before {
                    (incoming, old)
                } else {
                    (old, incoming)
                };
                *node = LayoutTree::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                };
                true
            }
            LayoutTree::Split { first, second, .. } => {
                if first.tile_ids().contains(&target) {
                    split(first, target, incoming, axis, before, ratio)
                } else {
                    split(second, target, incoming, axis, before, ratio)
                }
            }
            _ => false,
        }
    }
    match placement {
        CompositionPlacement::First => {
            if root.is_some() {
                return Err("First placement requires empty layout".into());
            }
            *root = Some(LayoutTree::Group {
                group_id: group.ok_or("new group identity absent")?,
                label: None,
                tabs: vec![tile],
                active: tile,
            });
        }
        CompositionPlacement::GroupTab { group_id, before } => {
            let node = find_group(root.as_mut().ok_or("layout empty")?, *group_id)
                .ok_or("target group absent")?;
            if let LayoutTree::Group { tabs, active, .. } = node {
                let index = match before {
                    Some(id) => tabs
                        .iter()
                        .position(|id_| id_ == id)
                        .ok_or("before tile absent")?,
                    None => tabs.len(),
                };
                tabs.insert(index, tile);
                *active = tile;
            }
        }
        CompositionPlacement::Split {
            target,
            axis,
            before,
            ratio,
        } => {
            let incoming = LayoutTree::Group {
                group_id: group.ok_or("new group identity absent")?,
                label: None,
                tabs: vec![tile],
                active: tile,
            };
            if !split(
                root.as_mut().ok_or("layout empty")?,
                *target,
                incoming,
                *axis,
                *before,
                *ratio,
            ) {
                return Err("split target absent".into());
            }
        }
    }
    root.as_ref()
        .ok_or("placement produced empty layout")?
        .validate()
        .map_err(str::to_owned)
}

/// Check an allocated insertion against the exact destination tab layout.
/// This stages no binding, descriptor, subject, or composition mutation.
pub(crate) fn validate_allocated_view_placement(
    set: &crate::view_set::ViewSet,
    allocation: &UiCompositionMountAllocation,
    placement: &super::composition_operation::CompositionPlacement,
) -> Result<(), String> {
    allocation.validate()?;
    if allocation.tile_ids.len() != 1
        || allocation.group_ids.len()
            != usize::from(!matches!(
                placement,
                super::composition_operation::CompositionPlacement::GroupTab { .. }
            ))
    {
        return Err("single-view placement has a noncanonical allocation shape".into());
    }
    let tile = allocation.tile_ids[0];
    if set.tiles.contains_key(&tile) {
        return Err("allocated tile already belongs to destination tab".into());
    }
    let mut root = set.root.clone();
    place(
        &mut root,
        tile,
        placement,
        allocation.group_ids.first().copied(),
    )
}

fn validate_move_destination(
    root: &crate::layout::LayoutTree,
    destination: crate::ids::TileId,
    placement: &super::composition_operation::CompositionPlacement,
) -> Result<(), String> {
    use super::composition_operation::CompositionPlacement;
    use crate::layout::LayoutTree;
    fn group_contains(
        node: &LayoutTree,
        group: crate::ids::ViewGroupId,
        tile: crate::ids::TileId,
    ) -> bool {
        match node {
            LayoutTree::Group { group_id, tabs, .. } => *group_id == group && tabs.contains(&tile),
            LayoutTree::Split { first, second, .. } => {
                group_contains(first, group, tile) || group_contains(second, group, tile)
            }
        }
    }
    match placement {
        CompositionPlacement::First => Err("move into a nonempty tab cannot use First".into()),
        CompositionPlacement::Split { target, .. } if *target == destination => Ok(()),
        CompositionPlacement::GroupTab { group_id, .. }
            if group_contains(root, *group_id, destination) =>
        {
            Ok(())
        }
        _ => Err("move placement differs from its exact destination".into()),
    }
}
fn stage_cross_surface_move(
    current: &PresentationComposition,
    request: &UiCompositionRequest,
    descriptors: &BTreeMap<String, UiBindingAttachment>,
) -> Result<UiCompositionDelta, String> {
    use super::composition_operation::{CompositionMutation, CompositionPlacement};
    use super::presentation_state::PresentationSurfaceViewSetUpdate;
    let CompositionMutation::MoveViewAcrossSurfaces {
        source,
        destination,
        destination_binding,
        placement,
    } = &request.mutation
    else {
        return Err("cross-Surface move request absent".into());
    };
    let source_owner = request
        .owner
        .surface_instance_id()
        .ok_or("source Surface owner absent")?;
    let source_surface = current
        .surfaces()
        .get(source_owner)
        .ok_or("source Surface absent")?;
    let destination_surface = current
        .surfaces()
        .get(&destination.surface_instance_id)
        .ok_or("destination Surface absent")?;
    if source_surface.binding() != request.owner.binding()
        || destination_surface.binding() != destination_binding
    {
        return Err("cross-Surface owner binding differs".into());
    }
    let mut affected_bindings = Vec::new();
    let mut check_binding =
        |binding: &super::binding::UiBindingAttachmentCoordinate| -> Result<(), String> {
            let descriptor = descriptors
                .get(&binding.binding_attachment_id)
                .ok_or("move descriptor absent")?;
            descriptor.validate().map_err(str::to_owned)?;
            if descriptor.binding_generation != binding.binding_generation
                || descriptor.binding_digest != binding.binding_digest
            {
                return Err("move descriptor coordinate differs".into());
            }
            if !affected_bindings.contains(binding) {
                affected_bindings.push(binding.clone());
            }
            Ok(())
        };
    check_binding(request.owner.binding())?;
    check_binding(destination_binding)?;
    let mut inventory = |surface: &super::presentation_state::SurfaceComposition| -> Result<
        BTreeMap<crate::ids::TileId, super::binding::UiBindingAttachmentCoordinate>,
        String,
    > {
        let mut bindings = BTreeMap::new();
        for tile in surface
            .view_sets()
            .iter()
            .flat_map(|set| set.tiles.values())
        {
            let mount = current
                .mount(&tile.instance_key)
                .ok_or("move inventory mount absent")?;
            check_binding(mount.binding())?;
            let id = tile
                .instance_key
                .surface_tile_id()
                .ok_or("move inventory tile coordinate absent")?;
            if bindings.insert(id, mount.binding().clone()).is_some() {
                return Err("move inventory duplicates tile".into());
            }
        }
        Ok(bindings)
    };
    let mut source_bindings = inventory(source_surface)?;
    let mut destination_bindings = inventory(destination_surface)?;
    let mut source_sets = source_surface.view_sets().to_vec();
    let mut destination_sets = destination_surface.view_sets().to_vec();
    let source_set = source_sets
        .iter_mut()
        .find(|set| set.id == source.view_set_id)
        .ok_or("source tab absent")?;
    let destination_set = destination_sets
        .iter_mut()
        .find(|set| set.id == destination.view_set_id)
        .ok_or("destination tab absent")?;
    if !source_set
        .root
        .as_ref()
        .is_some_and(|root| root.tile_ids().contains(&source.tile_id))
        || destination_set.tiles.contains_key(&source.tile_id)
    {
        return Err("cross-Surface source tile differs".into());
    }
    match destination_set.root.as_ref() {
        Some(root) if destination_set.tiles.contains_key(&destination.tile_id) => {
            validate_move_destination(root, destination.tile_id, placement)?
        }
        None if destination.tile_id == source.tile_id
            && matches!(placement, CompositionPlacement::First)
            && destination_set.tiles.is_empty() => {}
        _ => return Err("cross-Surface destination placement differs".into()),
    }
    let allocation = UiCompositionMountAllocation::allocate(
        request.expected_next_mount_id,
        0,
        usize::from(!matches!(placement, CompositionPlacement::GroupTab { .. })),
    )?;
    let mut moved = source_set
        .tiles
        .remove(&source.tile_id)
        .ok_or("source tile absent")?;
    let old_key = moved.instance_key.clone();
    let new_key = RyeOsViewInstanceKey::surface_tile(
        current.id(),
        &destination.surface_instance_id,
        source.tile_id,
    );
    moved.instance_key = new_key.clone();
    source_set.root = source_set
        .root
        .take()
        .ok_or("source layout absent")?
        .without_tile(source.tile_id);
    if source_set.maximized_tile == Some(source.tile_id) {
        source_set.maximized_tile = None;
    }
    if source_set.focused_tile == source.tile_id {
        if let Some(next) = source_set
            .root
            .as_ref()
            .and_then(|root| root.active_tile_ids().first().copied())
        {
            source_set.focus_tile(next);
        } else {
            source_set.focused_tile = crate::ids::TileId::new(0);
            source_set.focus_target = None;
        }
    }
    place(
        &mut destination_set.root,
        source.tile_id,
        placement,
        allocation.group_ids.first().copied(),
    )?;
    destination_set.tiles.insert(source.tile_id, moved);
    destination_set.maximized_tile = None;
    destination_set.focus_tile(source.tile_id);
    let moved_binding = source_bindings
        .remove(&source.tile_id)
        .ok_or("source binding absent")?;
    if destination_bindings
        .insert(source.tile_id, moved_binding)
        .is_some()
    {
        return Err("destination binding collision".into());
    }
    let mut staged = current.clone();
    staged.replace_two_surface_view_sets(
        PresentationSurfaceViewSetUpdate {
            surface_instance_id: source_owner.clone(),
            view_sets: source_sets,
            active_view_set: source_surface.active_view_set().map(|set| set.id),
            bindings: source_bindings,
        },
        PresentationSurfaceViewSetUpdate {
            surface_instance_id: destination.surface_instance_id.clone(),
            view_sets: destination_sets,
            active_view_set: Some(destination.view_set_id),
            bindings: destination_bindings,
        },
    )?;
    Ok(UiCompositionDelta {
        mount_remaps: BTreeMap::from([(old_key, new_key)]),
        allocated_mounts: Some(allocation.clone()),
        affected_bindings,
        composition: staged,
        revision: request
            .expected_revision
            .checked_add(1)
            .ok_or("revision exhausted")?,
        next_mount_id: allocation.next_mount_id,
        allocated_manifest: None,
        subjects: Vec::new(),
    })
}

pub fn stage_composition_delta(
    current: &PresentationComposition,
    request: &UiCompositionRequest,
    revision: u64,
    next_mount_id: u64,
    descriptors: &BTreeMap<String, UiBindingAttachment>,
    max_request_bytes: u64,
) -> Result<UiCompositionDelta, String> {
    request.validate(max_request_bytes)?;
    if current.id() != &request.presentation_id
        || request.expected_revision != revision
        || request.expected_next_mount_id != next_mount_id
    {
        return Err("stale composition owner/revision/frontier".into());
    }
    if matches!(
        &request.mutation,
        CompositionMutation::MoveViewAcrossSurfaces { .. }
    ) {
        return stage_cross_surface_move(current, request, descriptors);
    }
    if let CompositionMutation::InsertFollowingView {
        tab_id,
        view_ref,
        binding,
        attachment,
        route_assignment,
        ..
    }
    | CompositionMutation::ReplaceFollowingView {
        tab_id,
        view_ref,
        binding,
        attachment,
        route_assignment,
        ..
    } = &request.mutation
    {
        let owner = request
            .owner
            .surface_instance_id()
            .ok_or("following View requires Surface owner")?;
        let super::attachment::SelectionAttachment::FollowViewSet { view_set_id } = attachment
        else {
            return Err("following View requires exact live follower".into());
        };
        if current.view_set(owner, *view_set_id).is_none() {
            return Err("following View target is outside its Surface".into());
        }
        let mut allocation_request = request.clone();
        allocation_request.mutation = match &request.mutation {
            CompositionMutation::InsertFollowingView { placement, .. } => {
                CompositionMutation::InsertView {
                    tab_id: *tab_id,
                    placement: placement.clone(),
                    view_ref: view_ref.clone(),
                    binding: binding.clone(),
                    subject: SavedViewSelectionSource::FollowOwnSet,
                    route_assignment: route_assignment.clone(),
                }
            }
            CompositionMutation::ReplaceFollowingView { tile_id, .. } => {
                CompositionMutation::ReplaceView {
                    tab_id: *tab_id,
                    tile_id: *tile_id,
                    view_ref: view_ref.clone(),
                    binding: binding.clone(),
                    subject: SavedViewSelectionSource::FollowOwnSet,
                    route_assignment: route_assignment.clone(),
                }
            }
            _ => unreachable!(),
        };
        let mut delta = stage_composition_delta(
            current,
            &allocation_request,
            revision,
            next_mount_id,
            descriptors,
            max_request_bytes,
        )?;
        let tile = match &request.mutation {
            CompositionMutation::ReplaceFollowingView { tile_id, .. } => *tile_id,
            _ => *delta
                .allocated_mounts
                .as_ref()
                .and_then(|a| (a.tile_ids.len() == 1).then(|| &a.tile_ids[0]))
                .ok_or("following insertion allocation differs")?,
        };
        let instance = delta
            .composition
            .view_set(owner, *tab_id)
            .and_then(|set| set.tiles.get(&tile))
            .ok_or("following destination is absent")?
            .instance_key
            .clone();
        let validated = stage_subject_delta(
            &delta.composition,
            request,
            revision,
            delta.next_mount_id,
            descriptors,
            max_request_bytes,
            &instance,
            attachment,
        )?;
        delta.subjects = validated.subjects;
        for binding in validated.affected_bindings {
            if !delta.affected_bindings.contains(&binding) {
                delta.affected_bindings.push(binding);
            }
        }
        return Ok(delta);
    }
    if let CompositionMutation::InsertPinnedView {
        tab_id,
        placement,
        view_ref,
        binding,
        attachment,
        route_assignment,
    } = &request.mutation
    {
        let descriptor = descriptors
            .get(&binding.binding_attachment_id)
            .ok_or("pinned insertion descriptor absent")?;
        descriptor.validate().map_err(str::to_owned)?;
        if descriptor.binding_generation != binding.binding_generation
            || descriptor.binding_digest != binding.binding_digest
        {
            return Err("pinned insertion binding coordinate differs".into());
        }
        let raw = descriptor
            .presentation
            .views
            .get(view_ref)
            .ok_or("pinned insertion View outside exact closure")?;
        let view: super::content::ViewBinding =
            serde_json::from_value(raw.clone()).map_err(|error| error.to_string())?;
        let facets = super::attachment::selection_dependencies(&view)
            .into_iter()
            .collect::<Vec<_>>();
        // Allocation staging is private. Its temporary portable relationship
        // is replaced by the validated exact pin before this delta is returned.
        if facets.is_empty() {
            return Err("pinned insertion requires actual View selection dependencies".into());
        }
        let subject = SavedViewSelectionSource::RequiredSubject {
            input: "pinned_subject".into(),
            facets,
        };
        let mut allocation_request = request.clone();
        allocation_request.mutation = CompositionMutation::InsertView {
            tab_id: *tab_id,
            placement: placement.clone(),
            view_ref: view_ref.clone(),
            binding: binding.clone(),
            subject,
            route_assignment: route_assignment.clone(),
        };
        let mut delta = stage_composition_delta(
            current,
            &allocation_request,
            revision,
            next_mount_id,
            descriptors,
            max_request_bytes,
        )?;
        let owner = request
            .owner
            .surface_instance_id()
            .ok_or("pinned insertion requires Surface owner")?;
        let allocated = delta
            .allocated_mounts
            .as_ref()
            .ok_or("pinned insertion has no exact allocation")?;
        let tile_id = *allocated
            .tile_ids
            .first()
            .filter(|_| allocated.tile_ids.len() == 1)
            .ok_or("pinned insertion allocation differs")?;
        let instance = delta
            .composition
            .view_set(owner, *tab_id)
            .and_then(|set| set.tiles.get(&tile_id))
            .ok_or("pinned insertion mount absent")?
            .instance_key
            .clone();
        let validated = stage_subject_delta(
            &delta.composition,
            request,
            revision,
            delta.next_mount_id,
            descriptors,
            max_request_bytes,
            &instance,
            attachment,
        )?;
        delta.subjects = validated.subjects;
        for coordinate in validated.affected_bindings {
            if !delta.affected_bindings.contains(&coordinate) {
                delta.affected_bindings.push(coordinate);
            }
        }
        return Ok(delta);
    }
    if let CompositionMutation::SetSubject { mount, attachment } = &request.mutation {
        return stage_subject_delta(
            current,
            request,
            revision,
            next_mount_id,
            descriptors,
            max_request_bytes,
            mount,
            attachment,
        );
    }
    let owner = request
        .owner
        .surface_instance_id()
        .ok_or("layout edit requires Surface owner")?;
    if current
        .surfaces()
        .get(owner)
        .is_none_or(|surface| surface.binding() != request.owner.binding())
    {
        return Err("composition Surface owner differs".into());
    }
    let exact=|coordinate:&super::binding::UiBindingAttachmentCoordinate|->Result<&UiBindingAttachment,String>{
        let descriptor=descriptors.get(&coordinate.binding_attachment_id).ok_or("admitted descriptor is absent")?;
        descriptor.validate().map_err(str::to_owned)?;
        if descriptor.binding_generation!=coordinate.binding_generation||descriptor.binding_digest!=coordinate.binding_digest{return Err("admitted descriptor coordinate differs".into());}
        Ok(descriptor)
    };
    exact(request.owner.binding())?;
    let mut affected_bindings = vec![request.owner.binding().clone()];
    let requested_bindings = match &request.mutation {
        CompositionMutation::InsertTab { tab, .. } => vec![&tab.binding],
        CompositionMutation::RestoreTabs { tabs, .. }
        | CompositionMutation::AppendTabs { tabs, .. } => {
            tabs.iter().map(|tab| &tab.binding).collect()
        }
        CompositionMutation::InsertView { binding, .. }
        | CompositionMutation::ReplaceView { binding, .. }
        | CompositionMutation::LensPush { binding, .. } => vec![binding],
        _ => Vec::new(),
    };
    for binding in requested_bindings {
        exact(binding)?;
        if !affected_bindings.contains(binding) {
            affected_bindings.push(binding.clone());
        }
    }
    let mut affected_tiles = Vec::new();
    use CompositionMutation::*;
    let target_tab = match &request.mutation {
        RenameTab { tab_id, .. }
        | CloseTab { tab_id }
        | ReorderTab { tab_id, .. }
        | SelectTab { tab_id }
        | SplitRatio { tab_id, .. }
        | SelectGroup { tab_id, .. }
        | IsolateView { tab_id, .. }
        | Promote { tab_id, .. }
        | CloseView { tab_id, .. }
        | ReplaceView { tab_id, .. } => Some(*tab_id),
        InsertView { tab_id, .. } | Arrange { tab_id, .. } => Some(*tab_id),
        MoveView { source, .. } => Some(source.view_set_id),
        _ => None,
    };
    if let Some(id) = target_tab {
        let set = current
            .view_set(owner, id)
            .ok_or("affected tab is absent")?;
        match &request.mutation {
            CloseView { tile_id, .. }
            | ReplaceView { tile_id, .. }
            | Promote { tile_id, .. }
            | SelectGroup { tile_id, .. } => affected_tiles.push(
                set.tiles
                    .get(tile_id)
                    .ok_or("affected tile is absent")?
                    .instance_key
                    .clone(),
            ),
            _ => affected_tiles.extend(set.tiles.values().map(|tile| tile.instance_key.clone())),
        }
    }
    if let MoveView {
        source,
        destination,
        ..
    } = &request.mutation
        && source.view_set_id != destination.view_set_id
    {
        affected_tiles.extend(
            current
                .view_set(owner, destination.view_set_id)
                .ok_or("destination tab is absent")?
                .tiles
                .values()
                .map(|tile| tile.instance_key.clone()),
        );
    }
    if matches!(&request.mutation, RestoreTabs { .. }) {
        affected_tiles.extend(
            current.surfaces()[owner]
                .view_sets()
                .iter()
                .flat_map(|set| set.tiles.values())
                .map(|tile| tile.instance_key.clone()),
        );
    }
    if let FocusView { mount } = &request.mutation {
        affected_tiles.push(mount.clone());
    }
    for instance in affected_tiles {
        let binding = current
            .mount(&instance)
            .ok_or("affected mount absent")?
            .binding()
            .clone();
        exact(&binding)?;
        if !affected_bindings.contains(&binding) {
            affected_bindings.push(binding);
        }
    }
    let mut staged = current.clone();
    let mut subjects = Vec::new();
    let mut allocated_mounts = None;
    let allocated = request.allocate_tab_manifest(max_request_bytes)?;
    if let Some(manifest) = &allocated {
        let seed_tabs = match &request.mutation {
            CompositionMutation::InsertTab { tab, .. } => vec![tab],
            CompositionMutation::RestoreTabs { tabs, .. }
            | CompositionMutation::AppendTabs { tabs, .. } => tabs.iter().collect(),
            _ => unreachable!(),
        };
        let mut sets = Vec::new();
        let mut bindings = BTreeMap::new();
        for (index, tab) in seed_tabs.iter().enumerate() {
            let descriptor = exact(&tab.binding)?;
            let declaration = descriptor
                .presentation
                .definition
                .surface()
                .ok_or("tab seed requires an admitted Surface")?;
            let mut one = manifest.clone();
            let row = manifest.tabs[index].clone();
            one.allocation_start = row.view_set_id.0;
            one.next_mount_id = manifest
                .tabs
                .get(index + 1)
                .map_or(manifest.next_mount_id, |next| next.view_set_id.0);
            one.active_view_set_id = Some(row.view_set_id);
            one.tabs = vec![row];
            let mut set = one
                .instantiate_with_exact_ids(&declaration.tiling)?
                .remove(0);
            if let Some(index) = tab.focused_view {
                let tile = *one.tabs[0]
                    .tile_ids
                    .get(index)
                    .ok_or("tab focus mount absent")?;
                if !set.focus_tile(tile) {
                    return Err("tab focus mount unavailable".into());
                }
            }
            for tile in set.tiles.values() {
                if !descriptor
                    .presentation
                    .views
                    .contains_key(&tile.view.view_ref)
                {
                    return Err("requested View is outside its exact closure".into());
                }
            }
            for relationship in &tab.template.relationships {
                let resolved_follow_set =
                    if let SavedViewSelectionSource::FollowSet { saved_view_set_id } =
                        &relationship.source
                    {
                        let internal = matches!(
                            &request.mutation,
                            CompositionMutation::RestoreTabs { .. }
                                | CompositionMutation::AppendTabs { .. }
                        )
                        .then(|| {
                            seed_tabs
                                .iter()
                                .position(|candidate| candidate.template.id == *saved_view_set_id)
                        })
                        .flatten();
                        if let Some(target) = internal {
                            if tab.followed_sets.contains_key(saved_view_set_id) {
                                return Err(
                                    "internal follower cannot also select an external target"
                                        .into(),
                                );
                            }
                            Some(manifest.tabs[target].view_set_id)
                        } else {
                            if matches!(
                                &request.mutation,
                                CompositionMutation::RestoreTabs { .. }
                                    | CompositionMutation::AppendTabs { .. }
                            ) {
                                return Err("restoration cannot follow a tab it replaces".into());
                            }
                            let target = *tab
                                .followed_sets
                                .get(saved_view_set_id)
                                .ok_or("external FollowSet requires an exact causal target")?;
                            let target_set = current
                                .view_set(owner, target)
                                .ok_or("external follower target is not in this exact Surface")?;
                            for target_tile in target_set.tiles.values() {
                                let coordinate = current
                                    .mount(&target_tile.instance_key)
                                    .ok_or("follower target mount absent")?
                                    .binding();
                                exact(coordinate)?;
                                if !affected_bindings.contains(coordinate) {
                                    affected_bindings.push(coordinate.clone());
                                }
                            }
                            Some(target)
                        }
                    } else {
                        None
                    };
                let SavedViewMountRef::Tile { index } = &relationship.mount;
                let tile = *one.tabs[0]
                    .tile_ids
                    .get(*index)
                    .ok_or("subject slot is absent")?;
                validate_subject(
                    descriptor,
                    &set.tiles[&tile].view.view_ref,
                    &relationship.source,
                    max_request_bytes,
                )?;
                let instance_key = set.tiles[&tile].instance_key.clone();
                let supplied = if let SavedViewSelectionSource::RequiredSubject { input, .. } =
                    &relationship.source
                {
                    tab.fresh_subjects.get(input)
                } else {
                    None
                };
                if let Some(values) = supplied {
                    use sha2::{Digest, Sha256};
                    subjects.push(UiCompositionSubjectAssignment::Exact {
                        instance_key,
                        attachment: super::attachment::SelectionAttachment::Pinned {
                            values: values.clone(),
                            fingerprint: format!(
                                "{:x}",
                                Sha256::digest(
                                    serde_json::to_vec(values).map_err(|error| error.to_string())?
                                )
                            ),
                        },
                    });
                } else {
                    subjects.push(UiCompositionSubjectAssignment::Portable {
                        resolved_follow_set,
                        instance_key,
                        view_set_id: set.id,
                        source: relationship.source.clone(),
                    });
                }
            }
            for tile in set.tiles.keys() {
                bindings.insert(*tile, tab.binding.clone());
            }
            sets.push(set);
        }
        match &request.mutation {
            CompositionMutation::InsertTab {
                index, activate, ..
            } => {
                if current.surfaces()[owner].view_sets().len()
                    >= crate::surface::view_sets::MAX_VIEW_SETS
                {
                    return Err("open tab limit reached".into());
                }
                staged.insert_view_set(owner, sets.remove(0), *index, bindings, *activate)?;
            }
            CompositionMutation::RestoreTabs { .. } => {
                staged.replace_view_sets(owner, sets, manifest.active_view_set_id, bindings)?
            }
            CompositionMutation::AppendTabs {
                index, activate, ..
            } => {
                let existing = current.surfaces()[owner].view_sets().len();
                if *index > existing
                    || existing
                        .checked_add(sets.len())
                        .is_none_or(|count| count > crate::surface::view_sets::MAX_VIEW_SETS)
                {
                    return Err("tab import exceeds destination bound".into());
                }
                // Only this private candidate changes. A later tab failure
                // drops it; publication remains the single acknowledged delta.
                for (offset, set) in sets.into_iter().enumerate() {
                    let tab_bindings = set
                        .tiles
                        .keys()
                        .map(|tile| (*tile, bindings[tile].clone()))
                        .collect();
                    let select = *activate && Some(set.id) == manifest.active_view_set_id;
                    staged.insert_view_set(owner, set, *index + offset, tab_bindings, select)?;
                }
            }
            _ => unreachable!(),
        }
        // Validate supplied subjects against the exact prospective mounts before
        // the single completion can publish any layout, subject or source work.
        for assignment in &subjects {
            if let UiCompositionSubjectAssignment::Exact {
                instance_key,
                attachment,
            } = assignment
            {
                let validated = stage_subject_delta(
                    &staged,
                    request,
                    revision,
                    manifest.next_mount_id,
                    descriptors,
                    max_request_bytes,
                    instance_key,
                    attachment,
                )?;
                for coordinate in validated.affected_bindings {
                    if !affected_bindings.contains(&coordinate) {
                        affected_bindings.push(coordinate);
                    }
                }
            }
        }
    } else {
        use CompositionMutation::*;
        match &request.mutation {
            InsertView {
                tab_id,
                placement,
                view_ref,
                binding,
                subject,
                ..
            } => {
                if matches!(subject, SavedViewSelectionSource::FollowSet { .. }) {
                    return Err("external FollowSet unresolved".into());
                }
                let descriptor = exact(binding)?;
                validate_subject(descriptor, view_ref, subject, max_request_bytes)?;
                let needs_group = !matches!(
                    placement,
                    super::composition_operation::CompositionPlacement::GroupTab { .. }
                );
                let allocation = UiCompositionMountAllocation::allocate(
                    next_mount_id,
                    1,
                    usize::from(needs_group),
                )?;
                let tile = allocation.tile_ids[0];
                let group = allocation.group_ids.first().copied();
                let view = crate::surface::ViewKindSpec(view_ref.clone()).to_view_spec();
                let instance = RyeOsViewInstanceKey::surface_tile(current.id(), owner, tile);
                staged.mutate_view_set(
                    owner,
                    *tab_id,
                    BTreeMap::from([(tile, binding.clone())]),
                    |set| {
                        place(&mut set.root, tile, placement, group)?;
                        set.tiles.insert(
                            tile,
                            crate::view_set::TileState {
                                instance_key: instance.clone(),
                                local: view.initial_local_state(),
                                view,
                            },
                        );
                        set.focus_tile(tile);
                        Ok(())
                    },
                )?;
                subjects.push(UiCompositionSubjectAssignment::Portable {
                    instance_key: instance,
                    view_set_id: *tab_id,
                    source: subject.clone(),
                    resolved_follow_set: None,
                });
                allocated_mounts = Some(allocation);
            }
            MoveView {
                source,
                destination,
                placement,
            } => {
                if source.surface_instance_id != *owner || destination.surface_instance_id != *owner
                {
                    return Err("cross-owner move needs canonical subject transfer input".into());
                }
                let needs_group = !matches!(
                    placement,
                    super::composition_operation::CompositionPlacement::GroupTab { .. }
                );
                let allocation = UiCompositionMountAllocation::allocate(
                    next_mount_id,
                    0,
                    usize::from(needs_group),
                )?;
                let group = allocation.group_ids.first().copied();
                if source.view_set_id != destination.view_set_id {
                    staged.mutate_two_view_sets(
                        owner,
                        source.view_set_id,
                        destination.view_set_id,
                        |source_set, destination_set| {
                            let source_root =
                                source_set.root.as_ref().ok_or("source layout empty")?;
                            if !source_set.tiles.contains_key(&source.tile_id)
                                || !source_root.tile_ids().contains(&source.tile_id)
                                || destination_set.tiles.contains_key(&source.tile_id)
                            {
                                return Err("move source or destination tile differs".into());
                            }
                            match destination_set.root.as_ref() {
                                Some(root) => {
                                    if !destination_set.tiles.contains_key(&destination.tile_id) {
                                        return Err("move destination tile absent".into());
                                    }
                                    validate_move_destination(
                                        root,
                                        destination.tile_id,
                                        placement,
                                    )?;
                                }
                                None if destination.tile_id == source.tile_id
                                    && matches!(
                                        placement,
                                        super::composition_operation::CompositionPlacement::First
                                    )
                                    && destination_set.tiles.is_empty() => {}
                                None => {
                                    return Err(
                                        "empty destination requires exact First placement".into()
                                    );
                                }
                            }
                            let new_source_root = source_set
                                .root
                                .take()
                                .ok_or("source layout empty")?
                                .without_tile(source.tile_id);
                            let moved = source_set
                                .tiles
                                .remove(&source.tile_id)
                                .ok_or("move source tile absent")?;
                            source_set.root = new_source_root;
                            if source_set.maximized_tile == Some(source.tile_id) {
                                source_set.maximized_tile = None;
                            }
                            if source_set.focused_tile == source.tile_id {
                                if let Some(next) = source_set
                                    .root
                                    .as_ref()
                                    .and_then(|root| root.active_tile_ids().first().copied())
                                {
                                    source_set.focus_tile(next);
                                } else {
                                    source_set.focused_tile = crate::ids::TileId::new(0);
                                    source_set.focus_target = None;
                                }
                            }
                            place(&mut destination_set.root, source.tile_id, placement, group)?;
                            destination_set.tiles.insert(source.tile_id, moved);
                            destination_set.maximized_tile = None;
                            destination_set.focus_tile(source.tile_id);
                            Ok(())
                        },
                    )?;
                } else {
                    staged.mutate_view_set(owner, source.view_set_id, BTreeMap::new(), |set| {
                        if !set.tiles.contains_key(&source.tile_id)
                            || !set.tiles.contains_key(&destination.tile_id)
                        {
                            return Err("move tile absent".into());
                        }
                        validate_move_destination(
                            set.root.as_ref().ok_or("layout empty")?,
                            destination.tile_id,
                            placement,
                        )?;
                        set.root = set
                            .root
                            .take()
                            .ok_or("layout empty")?
                            .without_tile(source.tile_id);
                        place(&mut set.root, source.tile_id, placement, group)?;
                        set.focus_tile(source.tile_id);
                        Ok(())
                    })?;
                }
                if needs_group {
                    allocated_mounts = Some(allocation);
                }
            }
            Arrange { tab_id, axis } => {
                let ids = staged
                    .view_set(owner, *tab_id)
                    .ok_or("arrange tab absent")?
                    .tile_ids();
                let allocation =
                    UiCompositionMountAllocation::allocate(next_mount_id, 0, ids.len())?;
                fn tree(
                    ids: &[crate::ids::TileId],
                    groups: &[crate::ids::ViewGroupId],
                    axis: crate::layout::SplitAxis,
                ) -> Option<crate::layout::LayoutTree> {
                    if ids.is_empty() {
                        None
                    } else if ids.len() == 1 {
                        Some(crate::layout::LayoutTree::Group {
                            group_id: groups[0],
                            label: None,
                            tabs: vec![ids[0]],
                            active: ids[0],
                        })
                    } else {
                        let middle = ids.len() / 2;
                        Some(crate::layout::LayoutTree::Split {
                            axis,
                            ratio: middle as f32 / ids.len() as f32,
                            first: Box::new(tree(&ids[..middle], &groups[..middle], axis)?),
                            second: Box::new(tree(&ids[middle..], &groups[middle..], axis)?),
                        })
                    }
                }
                staged.mutate_view_set(owner, *tab_id, BTreeMap::new(), |set| {
                    set.root = tree(&ids, &allocation.group_ids, *axis);
                    Ok(())
                })?;
                if !ids.is_empty() {
                    allocated_mounts = Some(allocation);
                }
            }
            RenameSurface { title } => staged.rename_surface(owner, title.clone())?,
            RenameTab { tab_id, title } => {
                staged
                    .view_set_mut(owner, *tab_id)
                    .ok_or("tab is absent")?
                    .title = title.clone()
            }
            SelectTab { tab_id } => staged.select_view_set(owner, *tab_id)?,
            CloseTab { tab_id } => {
                staged.close_view_set(owner, *tab_id)?;
            }
            CloseView { tab_id, tile_id } => {
                staged.mutate_view_set(owner, *tab_id, BTreeMap::new(), |set| {
                    if !set.close_tile(*tile_id) {
                        return Err("close target is absent".into());
                    }
                    Ok(())
                })?
            }
            ReplaceView {
                tab_id,
                tile_id,
                view_ref,
                binding,
                subject,
                ..
            } => {
                if matches!(subject, SavedViewSelectionSource::FollowSet { .. }) {
                    return Err("replacement FollowSet needs exact causal target resolution".into());
                }
                let descriptor = exact(binding)?;
                validate_subject(descriptor, view_ref, subject, max_request_bytes)?;
                if !descriptor.presentation.views.contains_key(view_ref) {
                    return Err("replacement View is outside its exact closure".into());
                }
                let instance = staged
                    .view_set(owner, *tab_id)
                    .and_then(|set| set.tiles.get(tile_id))
                    .ok_or("replacement tile is absent")?
                    .instance_key
                    .clone();
                let view = crate::surface::ViewKindSpec(view_ref.clone()).to_view_spec();
                staged.mutate_view_set(
                    owner,
                    *tab_id,
                    BTreeMap::from([(*tile_id, binding.clone())]),
                    |set| {
                        let tile = set
                            .tiles
                            .get_mut(tile_id)
                            .ok_or("replacement tile is absent")?;
                        tile.local = view.initial_local_state();
                        tile.view = view;
                        Ok(())
                    },
                )?;
                subjects.push(UiCompositionSubjectAssignment::Portable {
                    resolved_follow_set: None,
                    instance_key: instance,
                    view_set_id: *tab_id,
                    source: subject.clone(),
                });
            }
            ReorderTab { tab_id, before } => {
                let ordered = staged.surfaces()[owner].view_sets();
                let old = ordered
                    .iter()
                    .position(|set| set.id == *tab_id)
                    .ok_or("tab is absent")?;
                let target = match before {
                    Some(id) => ordered
                        .iter()
                        .position(|set| set.id == *id)
                        .ok_or("destination tab absent")?,
                    None => ordered.len(),
                };
                let index = if target > old { target - 1 } else { target };
                staged.reorder_view_set(owner, *tab_id, index)?;
            }
            SplitRatio {
                tab_id,
                path,
                expected_layout_guard,
                ratio,
            } => staged.mutate_view_set(owner, *tab_id, BTreeMap::new(), |set| {
                let root = set.root.as_mut().ok_or("layout is empty")?;
                let guard = composition_split_guard(root)?;
                if &guard != expected_layout_guard {
                    return Err("split layout guard differs".into());
                }
                if !root.set_split_ratio(path, *ratio) {
                    return Err("split change is unavailable or unchanged".into());
                }
                Ok(())
            })?,
            SelectGroup {
                tab_id,
                group_id,
                tile_id,
            } => staged.mutate_view_set(owner, *tab_id, BTreeMap::new(), |set| {
                fn contains(
                    node: &crate::layout::LayoutTree,
                    group: crate::ids::ViewGroupId,
                    tile: crate::ids::TileId,
                ) -> bool {
                    match node {
                        crate::layout::LayoutTree::Group { group_id, tabs, .. } => {
                            *group_id == group && tabs.contains(&tile)
                        }
                        crate::layout::LayoutTree::Split { first, second, .. } => {
                            contains(first, group, tile) || contains(second, group, tile)
                        }
                    }
                }
                let root = set.root.as_mut().ok_or("layout is empty")?;
                if !contains(root, *group_id, *tile_id) || !root.select_tab(*tile_id) {
                    return Err("group/tile target differs".into());
                }
                Ok(())
            })?,
            IsolateView { tab_id, tile_id } => {
                staged.mutate_view_set(owner, *tab_id, BTreeMap::new(), |set| {
                    if tile_id.is_some_and(|id| !set.tiles.contains_key(&id)) {
                        return Err("isolation tile absent".into());
                    }
                    set.maximized_tile = *tile_id;
                    Ok(())
                })?
            }
            Promote { tab_id, tile_id } => {
                staged.mutate_view_set(owner, *tab_id, BTreeMap::new(), |set| {
                    if !set.zoom_tile(*tile_id) {
                        return Err("promotion unavailable".into());
                    }
                    Ok(())
                })?
            }
            FocusView { mount } => {
                let (surface, set) = staged
                    .view_set_for_instance(mount)
                    .ok_or("focus requires a Surface tile")?;
                if surface != owner {
                    return Err("focus target belongs to another Surface".into());
                }
                let tile = set
                    .tiles
                    .iter()
                    .find(|(_, tile)| &tile.instance_key == mount)
                    .map(|(id, _)| *id)
                    .ok_or("focus tile absent")?;
                let id = set.id;
                staged.mutate_view_set(owner, id, BTreeMap::new(), |set| {
                    if !set.focus_tile(tile) {
                        return Err("focus target unavailable".into());
                    }
                    Ok(())
                })?;
            }
            _ => {
                return Err(
                    "composition mutation requires unresolved exact allocation/subject integration"
                        .into(),
                );
            }
        }
    }
    Ok(UiCompositionDelta {
        mount_remaps: BTreeMap::new(),
        allocated_mounts: allocated_mounts.clone(),
        affected_bindings,
        composition: staged,
        revision: revision
            .checked_add(1)
            .ok_or("composition revision exhausted")?,
        next_mount_id: allocated_mounts
            .as_ref()
            .map(|value| value.next_mount_id)
            .unwrap_or_else(|| {
                allocated
                    .as_ref()
                    .map_or(next_mount_id, |manifest| manifest.next_mount_id)
            }),
        allocated_manifest: allocated,
        subjects,
    })
}
