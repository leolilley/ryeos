//! Closed, data-only client export for cross-presentation transfer.
//!
//! Source binding coordinates are evidence used to validate the frozen source
//! composition. They are never destination admission authority. Destination
//! allocation and binding admission are committed separately.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::ids::{RyeOsViewInstanceKey, UiViewMountOwner, ViewSetId};
use crate::surface::view_sets::{ViewSetSeedSpec, validate_seeds};
use crate::view_set::ViewLocalState;

use super::attachment::SelectionAttachment;
use super::binding::UiBindingAttachmentCoordinate;
use super::composition_lens::RetainedLensFrame;
use super::composition_operation::UiCompositionRouteAssignment;
use super::model::{InputBufferKey, RyeOsDockEdge, RyeOsInputState};

pub const UI_TRANSFER_EXPORT_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiTransferExportMode {
    Move,
    Copy,
}

/// One frozen source composition. IDs in this value describe the source only;
/// an importer must allocate a fresh destination manifest and retain its map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferExport {
    pub schema: u32,
    pub mode: UiTransferExportMode,
    pub content: UiTransferExportContent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiTransferExportContent {
    ViewSets {
        /// Export handle of the active source set when it is included in this
        /// collection. Explicit null means the source selection is outside it.
        #[serde(deserialize_with = "required_optional_string")]
        active_set: Option<String>,
        sets: Vec<UiTransferSetExport>,
    },
    SurfaceMount {
        source_view_set_id: ViewSetId,
        mount: UiTransferMountExport,
    },
    SharedDock {
        edge: RyeOsDockEdge,
        visible: bool,
        size: u16,
        mount: UiTransferMountExport,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferSetExport {
    /// Export-local stable handle used only by the destination allocation map.
    pub handle: String,
    pub source_view_set_id: ViewSetId,
    /// Exact owner of the source set, including when its layout has no mounts.
    /// This is frozen provenance, never destination admission authority.
    pub source_owner: UiViewMountOwner,
    /// Exact insertion owner of the frozen source set; not a portable grant.
    pub binding: UiBindingAttachmentCoordinate,
    /// Nested split/group order and active group members in depth-first order.
    pub seed: ViewSetSeedSpec,
    /// Exact source focus in seed depth-first mount order. This is distinct
    /// from every group's active tab and remains explicitly null when absent.
    #[serde(deserialize_with = "required_optional_usize")]
    pub focused_view: Option<usize>,
    /// One entry for every seed View, in the seed's depth-first order.
    pub mounts: Vec<UiTransferMountExport>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferMountExport {
    pub source_instance: RyeOsViewInstanceKey,
    pub view_ref: String,
    /// Exact source mount owner; destination admission must replace it.
    pub binding: UiBindingAttachmentCoordinate,
    pub local: ViewLocalState,
    pub subject: SelectionAttachment,
    pub route: UiCompositionRouteAssignment,
    /// Exact current lens label; explicit null is distinct from omission.
    #[serde(deserialize_with = "super::composition_lens::required_label")]
    pub current_label: Option<String>,
    pub drafts: Vec<UiTransferDraftExport>,
    /// Exact ordered return frames. Destination admission must independently
    /// replace every frame binding; source coordinates confer no authority.
    pub lens_frames: Vec<RetainedLensFrame>,
}

impl UiTransferMountExport {
    /// One draft namespace closure for live capture and retained validation.
    /// Coordinates describe scopes only; this does not admit frame bindings.
    pub(crate) fn draft_scopes(&self) -> BTreeSet<(String, String)> {
        std::iter::once((&self.view_ref, &self.binding))
            .chain(
                self.lens_frames
                    .iter()
                    .map(|frame| (&frame.view_ref, &frame.binding)),
            )
            .map(|(view_ref, binding)| (view_ref.clone(), InputBufferKey::binding_scope(binding)))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferDraftExport {
    pub key: InputBufferKey,
    pub state: RyeOsInputState,
}

impl UiTransferExport {
    /// Bind the data-only inventory to its exact retained preparation. This
    /// does not prove that the source currently mounts these instances.
    pub fn from_preparation(
        record: &ryeos_ui_presentation_contract::transfer::TransferRecord,
        max_bytes: u64,
    ) -> Result<Self, String> {
        record.validate_bounds(max_bytes)?;
        let ryeos_ui_presentation_contract::transfer::TransferPhase::Prepared { export, .. } =
            &record.phase
        else {
            return Err("source export requires exact preparation testimony".into());
        };
        let decoded: Self = serde_json::from_value(export.clone())
            .map_err(|error| format!("invalid closed source export: {error}"))?;
        decoded.validate_for(&record.transfer, max_bytes)?;
        Ok(decoded)
    }

    pub fn validate_for(
        &self,
        transfer: &ryeos_ui_presentation_contract::transfer::TransferIdentity,
        max_bytes: u64,
    ) -> Result<(), String> {
        self.validate(max_bytes)?;
        let source_presentation = |owner: &crate::ids::UiViewMountOwner| match owner {
            crate::ids::UiViewMountOwner::Shared { presentation_id }
            | crate::ids::UiViewMountOwner::Surface {
                presentation_id, ..
            } => presentation_id.as_str().to_owned(),
        };
        if self
            .sets()
            .into_iter()
            .flatten()
            .any(|set| source_presentation(&set.source_owner) != transfer.source.presentation_id)
            || self.mounts().into_iter().any(|mount| {
                mount.source_instance.mount_owner().is_none_or(|owner| {
                    source_presentation(&owner) != transfer.source.presentation_id
                })
            })
        {
            return Err("exported source owner differs from transfer preparation".into());
        }
        let digest = super::seat_digest::seat_payload_digest(
            &serde_json::to_value(self).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if digest != transfer.export_digest {
            return Err("source export differs from exact retained commitment".into());
        }
        Ok(())
    }

    /// Bind this frozen payload to one exact canonical source replay. The
    /// replay remains the authority for membership and revision; this export
    /// is only its closed transfer representation.
    pub fn validate_snapshot(
        &self,
        transfer: &ryeos_ui_presentation_contract::transfer::TransferIdentity,
        replay: &super::composition_replay::UiCompositionReplay,
        max_bytes: u64,
    ) -> Result<(), String> {
        if replay.revision != transfer.source.revision {
            return Err("source replay revision differs from transfer preparation".into());
        }
        self.validate_affected_snapshot(transfer, replay, max_bytes)
    }

    /// Recheck the exact exported inventory at a later signed source frontier.
    /// Unrelated composition revisions may advance, but exported replay-owned
    /// mounts, sets, subjects, routes, followers and bindings must still match.
    /// Renderer-local state and drafts are NOT represented by this replay:
    /// MOVE admission needs a separate exact-instance client freeze before it
    /// may treat those bytes as current. This method grants no transfer.
    pub fn validate_affected_snapshot(
        &self,
        transfer: &ryeos_ui_presentation_contract::transfer::TransferIdentity,
        replay: &super::composition_replay::UiCompositionReplay,
        max_bytes: u64,
    ) -> Result<(), String> {
        self.validate_for(transfer, max_bytes)?;
        if replay.revision < transfer.source.revision {
            return Err("source replay predates transfer preparation".into());
        }
        let instances = self
            .mounts()
            .into_iter()
            .map(|mount| mount.source_instance.clone())
            .collect::<BTreeSet<_>>();
        let set_ids = self
            .sets()
            .into_iter()
            .flatten()
            .map(|set| set.source_view_set_id)
            .collect::<BTreeSet<_>>();
        let affected_set_ids = match &self.content {
            UiTransferExportContent::ViewSets { .. } => set_ids.clone(),
            UiTransferExportContent::SurfaceMount {
                source_view_set_id, ..
            } => BTreeSet::from([*source_view_set_id]),
            UiTransferExportContent::SharedDock { .. } => BTreeSet::new(),
        };
        if replay.pending.values().any(|operation| {
            operation.composition().is_some_and(|request| {
                pending_request_affects_export(request, &instances, &affected_set_ids, self)
            })
        }) {
            return Err("source transfer has an overlapping unsettled structural operation".into());
        }

        match &self.content {
            UiTransferExportContent::ViewSets { active_set, sets } => {
                for set in sets {
                    let UiViewMountOwner::Surface {
                        surface_instance_id,
                        ..
                    } = &set.source_owner
                    else {
                        return Err("exported set has no exact Surface owner".into());
                    };
                    let canonical = replay
                        .composition
                        .view_set(surface_instance_id, set.source_view_set_id)
                        .ok_or("exported set is not in the canonical source Surface")?;
                    let template_id = replay
                        .template_ids
                        .get(&set.source_view_set_id)
                        .ok_or("exported set has no canonical template identity")?;
                    let canonical_seed = super::layout_preferences::capture_view_set(
                        canonical,
                        template_id.clone(),
                    )?;
                    if canonical_seed != set.seed
                        || canonical_focused_view(canonical)? != set.focused_view
                        || replay.insertion_bindings.get(&set.source_view_set_id)
                            != Some(&set.binding)
                    {
                        return Err(
                            "exported set differs from its canonical layout or insertion binding"
                                .into(),
                        );
                    }
                    for mount in &set.mounts {
                        validate_snapshot_mount(self.mode, mount, replay)?;
                    }
                }
                let first = sets.first().expect("nonempty inventory validated");
                let UiViewMountOwner::Surface {
                    surface_instance_id,
                    ..
                } = &first.source_owner
                else {
                    unreachable!("set owner validated")
                };
                let canonical_active = replay
                    .composition
                    .surfaces()
                    .get(surface_instance_id)
                    .and_then(|surface| surface.active_view_set())
                    .and_then(|active| {
                        sets.iter()
                            .find(|set| set.source_view_set_id == active.id)
                            .map(|set| set.handle.as_str())
                    });
                if active_set.as_deref() != canonical_active {
                    return Err(
                        "exported active set differs from canonical source selection".into(),
                    );
                }
            }
            UiTransferExportContent::SurfaceMount {
                source_view_set_id,
                mount,
            } => {
                let (_, canonical_set) = replay
                    .composition
                    .view_set_for_instance(&mount.source_instance)
                    .ok_or("exported Surface mount is not canonically mounted")?;
                if canonical_set.id != *source_view_set_id {
                    return Err("exported Surface mount belongs to another source set".into());
                }
                validate_snapshot_mount(self.mode, mount, replay)?;
            }
            UiTransferExportContent::SharedDock {
                edge,
                visible,
                size,
                mount,
            } => {
                let canonical = replay
                    .composition
                    .mount(&mount.source_instance)
                    .ok_or("exported shared dock is not canonically mounted")?;
                let super::presentation_state::MountRef::SharedDock {
                    edge: canonical_edge,
                    mount: canonical_mount,
                } = canonical
                else {
                    return Err("exported shared dock resolves to another mount kind".into());
                };
                if canonical_edge != edge_name(edge)
                    || canonical_mount.visible != *visible
                    || canonical_mount.size != *size
                {
                    return Err("exported shared dock differs from its canonical geometry".into());
                }
                validate_snapshot_mount(self.mode, mount, replay)?;
            }
        }

        let incoming_subjects = replay
            .subjects
            .iter()
            .map(|(instance, subject)| (instance, &subject.attachment))
            .chain(replay.lens_frames.iter().flat_map(|(instance, frames)| {
                frames
                    .iter()
                    .map(move |frame| (instance, &frame.attachment))
            }));
        validate_incoming_followers(incoming_subjects, &set_ids, &instances)?;
        Ok(())
    }

    pub fn sets(&self) -> Option<&[UiTransferSetExport]> {
        match &self.content {
            UiTransferExportContent::ViewSets { sets, .. } => Some(sets),
            _ => None,
        }
    }

    /// Exact export order: set order followed by each seed's depth-first mount
    /// order, or the singular mount for a mount-level transfer.
    pub fn mounts(&self) -> Vec<&UiTransferMountExport> {
        match &self.content {
            UiTransferExportContent::ViewSets { sets, .. } => {
                sets.iter().flat_map(|set| set.mounts.iter()).collect()
            }
            UiTransferExportContent::SurfaceMount { mount, .. }
            | UiTransferExportContent::SharedDock { mount, .. } => vec![mount],
        }
    }

    /// Validate only the closed source export. The caller supplies the existing
    /// signed seat bound and separately checks incoming FollowViewSet edges,
    /// source revision, destination admission and allocation.
    pub fn validate(&self, max_bytes: u64) -> Result<(), String> {
        if self.schema != UI_TRANSFER_EXPORT_SCHEMA {
            return Err("transfer export schema is invalid".into());
        }
        let encoded = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        if max_bytes == 0 || encoded.len() as u64 > max_bytes {
            return Err("transfer export exceeds its caller-supplied bound".into());
        }

        let mut instances = BTreeSet::new();
        let mut draft_keys = BTreeSet::new();
        match &self.content {
            UiTransferExportContent::ViewSets { active_set, sets } => {
                if sets.is_empty() {
                    return Err("transfer set inventory is empty".into());
                }
                if active_set
                    .as_ref()
                    .is_some_and(|active| !sets.iter().any(|set| &set.handle == active))
                {
                    return Err("transfer active set handle is outside its collection".into());
                }
                validate_sets(self.mode, sets, &mut instances, &mut draft_keys)?;
            }
            UiTransferExportContent::SurfaceMount {
                source_view_set_id,
                mount,
            } => {
                if source_view_set_id.0 == 0
                    || !matches!(
                        mount.source_instance.mount_owner(),
                        Some(UiViewMountOwner::Surface { .. })
                    )
                    || matches!(&mount.subject, SelectionAttachment::FollowViewSet { .. })
                {
                    return Err("single Surface mount has an invalid owner or follower".into());
                }
                validate_mount(self.mode, mount, &mut instances, &mut draft_keys)?;
            }
            UiTransferExportContent::SharedDock {
                edge, size, mount, ..
            } => {
                let Some(UiViewMountOwner::Shared { presentation_id }) =
                    mount.source_instance.mount_owner()
                else {
                    return Err("shared dock has no exact shared owner".into());
                };
                let edge_name = match edge {
                    RyeOsDockEdge::Top => "top",
                    RyeOsDockEdge::Bottom => "bottom",
                    RyeOsDockEdge::Left => "left",
                    RyeOsDockEdge::Right => "right",
                };
                if RyeOsViewInstanceKey::shared_dock(&presentation_id, edge_name)
                    .ok()
                    .as_ref()
                    != Some(&mount.source_instance)
                    || *size == 0
                    || matches!(&mount.subject, SelectionAttachment::FollowViewSet { .. })
                {
                    return Err("shared dock identity or follower is invalid".into());
                }
                validate_mount(self.mode, mount, &mut instances, &mut draft_keys)?;
            }
        }
        Ok(())
    }
}

fn validate_sets(
    mode: UiTransferExportMode,
    sets: &[UiTransferSetExport],
    instances: &mut BTreeSet<RyeOsViewInstanceKey>,
    draft_keys: &mut BTreeSet<InputBufferKey>,
) -> Result<(), String> {
    let mut handles = BTreeSet::new();
    let mut set_ids = BTreeSet::new();
    let mut collection_owner: Option<&UiViewMountOwner> = None;
    for set in sets {
        if !stable_handle(&set.handle)
            || !handles.insert(set.handle.as_str())
            || set.source_view_set_id.0 == 0
            || !set_ids.insert(set.source_view_set_id)
        {
            return Err("transfer set identity is invalid or repeated".into());
        }
        if !matches!(&set.source_owner, UiViewMountOwner::Surface { .. })
            || collection_owner
                .as_ref()
                .is_some_and(|expected| *expected != &set.source_owner)
        {
            return Err("view-set collection crosses source Surface owners".into());
        }
        collection_owner.get_or_insert(&set.source_owner);
        super::composition_operation::binding(&set.binding)?;
        validate_seeds(std::slice::from_ref(&set.seed))?;
        let mut ordered = Vec::new();
        if let Some(root) = &set.seed.root {
            ordered_seed_views(root, &mut ordered);
        }
        if ordered.len() != set.mounts.len()
            || ordered
                .iter()
                .zip(&set.mounts)
                .any(|(expected, mount)| expected != &mount.view_ref)
        {
            return Err("transfer mounts differ from exact seed order".into());
        }
        if set
            .focused_view
            .is_some_and(|focused| focused >= set.mounts.len())
        {
            return Err("transfer focused View is outside its exact seed mounts".into());
        }
        for mount in &set.mounts {
            let Some(owner @ UiViewMountOwner::Surface { .. }) =
                mount.source_instance.mount_owner()
            else {
                return Err("view-set collection contains a non-Surface mount".into());
            };
            if owner != set.source_owner {
                return Err("view-set collection crosses source Surface owners".into());
            }
            validate_mount(mode, mount, instances, draft_keys)?;
        }
    }

    // Every outgoing runtime follower must remain inside the exported
    // collection. The source caller must additionally scan non-exported
    // mounts for incoming edges before freezing the operation.
    for set in sets {
        for mount in &set.mounts {
            if let SelectionAttachment::FollowViewSet { view_set_id } = &mount.subject
                && !set_ids.contains(view_set_id)
            {
                return Err("transfer FollowViewSet leaves its exported collection".into());
            }
        }
    }
    Ok(())
}

fn validate_mount(
    mode: UiTransferExportMode,
    mount: &UiTransferMountExport,
    instances: &mut BTreeSet<RyeOsViewInstanceKey>,
    draft_keys: &mut BTreeSet<InputBufferKey>,
) -> Result<(), String> {
    if !instances.insert(mount.source_instance.clone())
        || mount.source_instance.mount_owner().is_none()
    {
        return Err("transfer source mount identity is invalid or repeated".into());
    }
    super::composition_operation::view(&mount.view_ref)?;
    super::composition_operation::binding(&mount.binding)?;
    mount.route.validate()?;
    if mount.current_label.as_ref().is_some_and(|label| {
        label.is_empty()
            || label.trim() != label
            || label.chars().any(char::is_control)
            || label.len() > crate::surface::view_sets::MAX_VIEW_SET_LABEL_BYTES
    }) {
        return Err("transfer current lens label is invalid".into());
    }
    validate_subject(&mount.subject)?;
    validate_local_state(&mount.local)?;
    let allowed_scopes = mount.draft_scopes();
    for draft in &mount.drafts {
        if draft.key.view_instance_key != mount.source_instance
            || !allowed_scopes
                .contains(&(draft.key.view_ref.clone(), draft.key.binding_scope.clone()))
            || draft.key.input_id.is_empty()
            || !draft_keys.insert(draft.key.clone())
            || draft.state.cursor > draft.state.text.len()
            || !draft.state.text.is_char_boundary(draft.state.cursor)
        {
            return Err("transfer draft differs from its exact source mount".into());
        }
    }
    for frame in &mount.lens_frames {
        frame.validate_shape(u64::MAX)?;
        validate_subject(&frame.attachment)?;
        if mode == UiTransferExportMode::Copy
            && (matches!(&frame.attachment, SelectionAttachment::Pinned { .. })
                || frame.input_route.is_some())
        {
            return Err("copy lens frame requires a portable subject and fresh route".into());
        }
    }
    if mode == UiTransferExportMode::Copy
        && (mount.local != ViewLocalState::None
            || !mount.drafts.is_empty()
            || !matches!(&mount.route, UiCompositionRouteAssignment::Authored)
            || matches!(&mount.subject, SelectionAttachment::Pinned { .. }))
    {
        return Err("copy export must use fresh local, route, draft and subject state".into());
    }
    Ok(())
}

fn validate_snapshot_mount(
    mode: UiTransferExportMode,
    mount: &UiTransferMountExport,
    replay: &super::composition_replay::UiCompositionReplay,
) -> Result<(), String> {
    let canonical = replay
        .composition
        .mount(&mount.source_instance)
        .ok_or("exported source mount is absent from canonical replay")?;
    let subject = replay
        .subjects
        .get(&mount.source_instance)
        .ok_or("exported source mount has no canonical subject")?;
    let route = snapshot_route(replay.input_routes.get(&mount.source_instance));
    let subject_matches = match mode {
        UiTransferExportMode::Move => subject.attachment == mount.subject,
        UiTransferExportMode::Copy => copy_subject_matches(mount, subject, replay)?,
    };
    let route_matches = mode == UiTransferExportMode::Copy || route == mount.route;
    let label_matches = replay
        .lens_labels
        .get(&mount.source_instance)
        .cloned()
        .flatten()
        == mount.current_label;
    let source_frames = replay
        .lens_frames
        .get(&mount.source_instance)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let frames_match = if mode == UiTransferExportMode::Move {
        source_frames == mount.lens_frames.as_slice()
    } else if source_frames.len() != mount.lens_frames.len() {
        false
    } else {
        let mut matches = true;
        for (source, received) in source_frames.iter().zip(&mount.lens_frames) {
            matches &= source.binding == received.binding
                && source.view_ref == received.view_ref
                && source.label == received.label
                && received.input_route.is_none()
                && portable_copy_subject_matches(
                    &source.binding,
                    &source.view_ref,
                    &source.attachment,
                    &received.attachment,
                    replay,
                )?;
        }
        matches
    };
    if canonical.view_ref() != mount.view_ref
        || canonical.binding() != &mount.binding
        || subject.binding != mount.binding
        || subject.view_ref != mount.view_ref
        || !subject_matches
        || !route_matches
        || !label_matches
        || !frames_match
    {
        return Err("exported mount differs from its exact canonical source state".into());
    }
    Ok(())
}

fn copy_subject_matches(
    mount: &UiTransferMountExport,
    canonical: &super::composition_replay::UiCompositionMountSubject,
    replay: &super::composition_replay::UiCompositionReplay,
) -> Result<bool, String> {
    portable_copy_subject_matches(
        &mount.binding,
        &mount.view_ref,
        &canonical.attachment,
        &mount.subject,
        replay,
    )
}

fn portable_copy_subject_matches(
    coordinate: &UiBindingAttachmentCoordinate,
    view_ref: &str,
    source: &SelectionAttachment,
    received: &SelectionAttachment,
    replay: &super::composition_replay::UiCompositionReplay,
) -> Result<bool, String> {
    match (source, received) {
        (
            SelectionAttachment::Pinned { .. },
            SelectionAttachment::RequiredSubject { facets, .. },
        ) => {
            let descriptor = replay
                .binding_descriptors()
                .get(&coordinate.binding_attachment_id)
                .filter(|descriptor| {
                    descriptor.binding_generation == coordinate.binding_generation
                        && descriptor.binding_digest == coordinate.binding_digest
                })
                .ok_or("copy source has no exact retained binding descriptor")?;
            let binding = descriptor
                .presentation
                .views
                .get(view_ref)
                .ok_or("copy source View is absent from its retained binding")?;
            let binding: super::content::ViewBinding = serde_json::from_value(binding.clone())
                .map_err(|error| format!("copy source View contract is invalid: {error}"))?;
            let dependencies = super::attachment::selection_dependencies(&binding);
            Ok(!dependencies.is_empty()
                && dependencies.len() == facets.len()
                && facets.iter().all(|facet| dependencies.contains(facet)))
        }
        (expected, received) => Ok(expected == received),
    }
}

pub(crate) fn pending_request_affects_export(
    request: &super::composition_operation::UiCompositionRequest,
    instances: &BTreeSet<RyeOsViewInstanceKey>,
    set_ids: &BTreeSet<ViewSetId>,
    export: &UiTransferExport,
) -> bool {
    use super::composition_operation::{CompositionMutation, CompositionOwner};
    if let CompositionMutation::MoveViewAcrossSurfaces {
        source,
        destination,
        ..
    } = &request.mutation
    {
        let export_owner = match &export.content {
            UiTransferExportContent::ViewSets { sets, .. } => {
                sets.first().map(|set| set.source_owner.clone())
            }
            UiTransferExportContent::SurfaceMount { mount, .. } => {
                mount.source_instance.mount_owner()
            }
            UiTransferExportContent::SharedDock { .. } => None,
        };
        let Some(UiViewMountOwner::Surface {
            surface_instance_id,
            ..
        }) = export_owner
        else {
            return false;
        };
        if surface_instance_id != source.surface_instance_id
            && surface_instance_id != destination.surface_instance_id
        {
            return false;
        }
        // Destination activation changes the exact collection selection even
        // when its edited tab is not among the frozen exported tabs.
        if surface_instance_id == destination.surface_instance_id
            && matches!(&export.content, UiTransferExportContent::ViewSets { .. })
        {
            return true;
        }
        return set_ids.contains(&source.view_set_id) || set_ids.contains(&destination.view_set_id);
    }
    match (&export.content, &request.owner) {
        (
            UiTransferExportContent::ViewSets { sets, .. },
            CompositionOwner::Surface {
                surface_instance_id,
                ..
            },
        ) if sets.first().is_some_and(|set| {
            matches!(&set.source_owner, UiViewMountOwner::Surface { surface_instance_id: owner, .. } if owner == surface_instance_id)
        }) => {}
        (
            UiTransferExportContent::SurfaceMount { mount, .. },
            CompositionOwner::Surface {
                surface_instance_id,
                ..
            },
        ) if matches!(mount.source_instance.mount_owner(), Some(UiViewMountOwner::Surface { surface_instance_id: owner, .. }) if &owner == surface_instance_id) => {}
        (UiTransferExportContent::SharedDock { .. }, CompositionOwner::Shared { .. }) => {
            return true;
        }
        _ => return false,
    }
    // A collection exports the source Surface's active-set identity as well
    // as its listed tabs. Operations on an unexported tab can still change
    // that exact selection while preparation is unsettled.
    if let UiTransferExportContent::ViewSets { active_set, .. } = &export.content {
        match &request.mutation {
            CompositionMutation::SelectTab { .. } => return true,
            CompositionMutation::CloseTab { tab_id }
                if set_ids.contains(tab_id) || active_set.is_none() =>
            {
                return true;
            }
            CompositionMutation::InsertTab { activate: true, .. }
            | CompositionMutation::AppendTabs { activate: true, .. } => return true,
            _ => {}
        }
    }
    match &request.mutation {
        CompositionMutation::SetSubject { mount, .. }
        | CompositionMutation::FocusView { mount } => instances.contains(mount),
        CompositionMutation::RestoreTabs { .. } => true,
        CompositionMutation::InsertTab { .. } | CompositionMutation::AppendTabs { .. } => false,
        CompositionMutation::RenameSurface { .. } => false,
        CompositionMutation::RenameTab { tab_id, .. }
        | CompositionMutation::CloseTab { tab_id }
        | CompositionMutation::ReorderTab { tab_id, .. }
        | CompositionMutation::SelectTab { tab_id }
        | CompositionMutation::InsertView { tab_id, .. }
        | CompositionMutation::InsertPinnedView { tab_id, .. }
        | CompositionMutation::InsertFollowingView { tab_id, .. }
        | CompositionMutation::ReplaceFollowingView { tab_id, .. }
        | CompositionMutation::ReplaceView { tab_id, .. }
        | CompositionMutation::CloseView { tab_id, .. }
        | CompositionMutation::SplitRatio { tab_id, .. }
        | CompositionMutation::SelectGroup { tab_id, .. }
        | CompositionMutation::Arrange { tab_id, .. }
        | CompositionMutation::Promote { tab_id, .. }
        | CompositionMutation::IsolateView { tab_id, .. }
        | CompositionMutation::LensPush { tab_id, .. }
        | CompositionMutation::LensPop { tab_id, .. } => set_ids.contains(tab_id),
        CompositionMutation::MoveView {
            source,
            destination,
            ..
        } => set_ids.contains(&source.view_set_id) || set_ids.contains(&destination.view_set_id),
        CompositionMutation::MoveViewAcrossSurfaces { .. } => {
            unreachable!("dual-owner freeze check precedes single-owner dispatch")
        }
    }
}

fn snapshot_route(
    retained: Option<&(u64, Option<serde_json::Value>)>,
) -> UiCompositionRouteAssignment {
    retained.map_or(
        UiCompositionRouteAssignment::Authored,
        |(_, input_route)| UiCompositionRouteAssignment::Override {
            input_route: input_route.clone(),
        },
    )
}

fn validate_incoming_followers<'a>(
    subjects: impl IntoIterator<Item = (&'a RyeOsViewInstanceKey, &'a SelectionAttachment)>,
    exported_sets: &BTreeSet<ViewSetId>,
    exported_instances: &BTreeSet<RyeOsViewInstanceKey>,
) -> Result<(), String> {
    for (instance, subject) in subjects {
        if let SelectionAttachment::FollowViewSet { view_set_id } = subject
            && exported_sets.contains(view_set_id)
            && !exported_instances.contains(instance)
        {
            return Err("source transfer leaves an incoming FollowViewSet edge behind".into());
        }
    }
    Ok(())
}

fn edge_name(edge: &RyeOsDockEdge) -> &'static str {
    match edge {
        RyeOsDockEdge::Top => "top",
        RyeOsDockEdge::Bottom => "bottom",
        RyeOsDockEdge::Left => "left",
        RyeOsDockEdge::Right => "right",
    }
}

fn stable_handle(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= crate::surface::view_sets::MAX_SAVED_VIEW_SET_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn ordered_seed_views<'a>(
    node: &'a crate::surface::view_sets::LayoutSeedSpec,
    output: &mut Vec<&'a str>,
) {
    use crate::surface::view_sets::LayoutSeedSpec;
    match node {
        LayoutSeedSpec::Group { views, .. } => {
            output.extend(views.iter().map(|view| view.0.as_str()));
        }
        LayoutSeedSpec::Split { first, second, .. } => {
            ordered_seed_views(first, output);
            ordered_seed_views(second, output);
        }
    }
}

fn canonical_focused_view(view_set: &crate::view_set::ViewSet) -> Result<Option<usize>, String> {
    let focused = view_set.focused_tile;
    if view_set.root.is_none() && view_set.tiles.is_empty() && focused == crate::ids::TileId::new(0)
    {
        return Ok(None);
    }
    let ordered = view_set
        .root
        .as_ref()
        .map(crate::layout::LayoutTree::tile_ids)
        .unwrap_or_default();
    ordered
        .iter()
        .position(|tile| *tile == focused)
        .map(Some)
        .ok_or("canonical focused View is absent from its layout".into())
}

fn required_optional_usize<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    Option::<usize>::deserialize(deserializer)
}

fn required_optional_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

fn validate_local_state(local: &ViewLocalState) -> Result<(), String> {
    if let ViewLocalState::Field(field) = local
        && (field.playback.playing || field.playback.awaiting.is_some())
    {
        return Err("transfer local state contains pending Field playback".into());
    }
    Ok(())
}

fn validate_subject(subject: &SelectionAttachment) -> Result<(), String> {
    match subject {
        SelectionAttachment::FollowViewSet { view_set_id } if view_set_id.0 == 0 => {
            Err("transfer subject follows a zero set identity".into())
        }
        SelectionAttachment::FollowViewSet { .. } => Ok(()),
        SelectionAttachment::Pinned {
            values,
            fingerprint,
        } => {
            if values.is_empty() || values.values().any(serde_json::Value::is_null) {
                return Err("transfer pinned subject is empty or unresolved".into());
            }
            use sha2::{Digest, Sha256};
            let encoded = serde_json::to_vec(values).map_err(|error| error.to_string())?;
            if *fingerprint != format!("{:x}", Sha256::digest(encoded)) {
                return Err("transfer pinned subject fingerprint differs".into());
            }
            Ok(())
        }
        SelectionAttachment::RequiredSubject { input, facets } => {
            let unique = facets.iter().collect::<BTreeSet<_>>();
            if input.is_empty()
                || input.trim() != input
                || input.chars().any(char::is_control)
                || facets.is_empty()
                || unique.len() != facets.len()
                || facets.iter().any(|facet| {
                    facet.is_empty() || facet.trim() != facet || facet.chars().any(char::is_control)
                })
            {
                return Err("transfer required subject contract is invalid".into());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{PresentationId, SurfaceInstanceId, TileId};
    use crate::surface::ViewKindSpec;
    use crate::surface::view_sets::LayoutSeedSpec;

    fn binding() -> UiBindingAttachmentCoordinate {
        UiBindingAttachmentCoordinate {
            binding_attachment_id: "source-binding".into(),
            binding_generation: 1,
            binding_digest: "ab".repeat(32),
        }
    }

    fn export(mode: UiTransferExportMode) -> UiTransferExport {
        let presentation = PresentationId::try_new("source-presentation").unwrap();
        let surface = SurfaceInstanceId::try_new("source-surface").unwrap();
        let source_instance =
            RyeOsViewInstanceKey::surface_tile(&presentation, &surface, TileId::new(7));
        UiTransferExport {
            schema: UI_TRANSFER_EXPORT_SCHEMA,
            mode,
            content: UiTransferExportContent::ViewSets {
                active_set: Some("set-one".into()),
                sets: vec![UiTransferSetExport {
                    handle: "set-one".into(),
                    source_view_set_id: ViewSetId::new(3),
                    source_owner: UiViewMountOwner::Surface {
                        presentation_id: presentation.clone(),
                        surface_instance_id: surface.clone(),
                    },
                    binding: binding(),
                    seed: ViewSetSeedSpec {
                        schema: crate::surface::view_sets::VIEW_SET_COMPOSITION_SCHEMA,
                        id: "set-one".into(),
                        title: "Set one".into(),
                        root: Some(LayoutSeedSpec::Group {
                            label: None,
                            views: vec![ViewKindSpec("view:test/one".into())],
                            active: 0,
                        }),
                    },
                    focused_view: Some(0),
                    mounts: vec![UiTransferMountExport {
                        source_instance,
                        view_ref: "view:test/one".into(),
                        binding: binding(),
                        local: ViewLocalState::None,
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
        }
    }

    fn mount_mut(value: &mut UiTransferExport) -> &mut UiTransferMountExport {
        let UiTransferExportContent::ViewSets { sets, .. } = &mut value.content else {
            panic!("set fixture")
        };
        &mut sets[0].mounts[0]
    }

    #[test]
    fn cross_surface_move_checks_destination_freeze_before_source_owner_filter() {
        use super::super::composition_operation::{
            CompositionMutation, CompositionOwner, CompositionPlacement, CompositionTileTarget,
            UiCompositionRequest,
        };
        let exported = export(UiTransferExportMode::Move);
        let target_owner = SurfaceInstanceId::try_new("source-surface").unwrap();
        let other_owner = SurfaceInstanceId::try_new("other-surface").unwrap();
        let mut request = UiCompositionRequest {
            schema: 1,
            presentation_id: PresentationId::try_new("source-presentation").unwrap(),
            operation_id: "11111111-1111-4111-8111-111111111111".into(),
            expected_revision: 1,
            expected_next_mount_id: 10,
            owner: CompositionOwner::Surface {
                surface_instance_id: other_owner.clone(),
                binding: binding(),
            },
            mutation: CompositionMutation::MoveViewAcrossSurfaces {
                source: CompositionTileTarget {
                    surface_instance_id: other_owner,
                    view_set_id: ViewSetId::new(8),
                    tile_id: crate::ids::TileId::new(8),
                },
                destination: CompositionTileTarget {
                    surface_instance_id: target_owner,
                    view_set_id: ViewSetId::new(9),
                    tile_id: crate::ids::TileId::new(9),
                },
                destination_binding: binding(),
                placement: CompositionPlacement::First,
            },
        };
        let instances = exported
            .mounts()
            .iter()
            .map(|mount| mount.source_instance.clone())
            .collect();
        let sets = BTreeSet::from([ViewSetId::new(3)]);
        assert!(
            pending_request_affects_export(&request, &instances, &sets, &exported),
            "destination active-set selection belongs to the frozen collection"
        );
        if let CompositionMutation::MoveViewAcrossSurfaces { destination, .. } =
            &mut request.mutation
        {
            destination.surface_instance_id =
                SurfaceInstanceId::try_new("unrelated-surface").unwrap();
        }
        assert!(!pending_request_affects_export(
            &request, &instances, &sets, &exported
        ));
    }

    #[test]
    fn pending_surface_selection_changes_overlap_exported_active_set() {
        use super::super::composition_operation::{
            CompositionMutation, CompositionOwner, CompositionTabSeed, UiCompositionRequest,
        };
        use crate::surface::view_sets::{
            SavedViewMountRef, SavedViewSelectionRelationship, SavedViewSelectionSource,
            SavedViewSetTemplate,
        };
        let exported = export(UiTransferExportMode::Move);
        let owner = CompositionOwner::Surface {
            surface_instance_id: SurfaceInstanceId::try_new("source-surface").unwrap(),
            binding: binding(),
        };
        let mut request = UiCompositionRequest {
            schema: 1,
            presentation_id: PresentationId::try_new("source-presentation").unwrap(),
            operation_id: "11111111-1111-4111-8111-111111111111".into(),
            expected_revision: 1,
            expected_next_mount_id: 10,
            owner,
            mutation: CompositionMutation::SelectTab {
                tab_id: ViewSetId::new(9),
            },
        };
        let instances = exported
            .mounts()
            .iter()
            .map(|mount| mount.source_instance.clone())
            .collect::<BTreeSet<_>>();
        let sets = BTreeSet::from([ViewSetId::new(3)]);
        assert!(pending_request_affects_export(
            &request, &instances, &sets, &exported
        ));
        request.mutation = CompositionMutation::CloseTab {
            tab_id: ViewSetId::new(9),
        };
        assert!(!pending_request_affects_export(
            &request, &instances, &sets, &exported
        ));
        let mut unselected = exported.clone();
        let UiTransferExportContent::ViewSets { active_set, .. } = &mut unselected.content else {
            unreachable!()
        };
        *active_set = None;
        assert!(pending_request_affects_export(
            &request,
            &instances,
            &sets,
            &unselected
        ));
        request.mutation = CompositionMutation::CloseTab {
            tab_id: ViewSetId::new(3),
        };
        assert!(pending_request_affects_export(
            &request, &instances, &sets, &exported
        ));
        let UiTransferExportContent::ViewSets {
            sets: exported_sets,
            ..
        } = &exported.content
        else {
            unreachable!()
        };
        let mut seed = exported_sets[0].seed.clone();
        seed.id = "new".into();
        let tab = CompositionTabSeed {
            template: SavedViewSetTemplate {
                id: "new".into(),
                name: "New".into(),
                composition: seed,
                relationships: vec![SavedViewSelectionRelationship {
                    mount: SavedViewMountRef::Tile { index: 0 },
                    source: SavedViewSelectionSource::FollowOwnSet,
                }],
            },
            binding: binding(),
            fresh_subjects: Default::default(),
            followed_sets: Default::default(),
            focused_view: None,
        };
        request.mutation = CompositionMutation::InsertTab {
            index: 0,
            tab: tab.clone(),
            activate: true,
        };
        assert!(pending_request_affects_export(
            &request, &instances, &sets, &exported
        ));
        request.mutation = CompositionMutation::InsertTab {
            index: 0,
            tab,
            activate: false,
        };
        assert!(!pending_request_affects_export(
            &request, &instances, &sets, &exported
        ));
    }

    #[test]
    fn exact_seed_order_and_follower_closure_are_required() {
        let valid = export(UiTransferExportMode::Move);
        valid.validate(64 * 1024).unwrap();

        let mut wrong_view = valid.clone();
        mount_mut(&mut wrong_view).view_ref = "view:test/other".into();
        assert!(wrong_view.validate(64 * 1024).is_err());

        let mut external_follower = valid;
        mount_mut(&mut external_follower).subject = SelectionAttachment::FollowViewSet {
            view_set_id: ViewSetId::new(99),
        };
        assert!(external_follower.validate(64 * 1024).is_err());
    }

    #[test]
    fn empty_sets_retain_exact_collection_owner() {
        let mut value = export(UiTransferExportMode::Move);
        let UiTransferExportContent::ViewSets { sets, .. } = &mut value.content else {
            unreachable!()
        };
        sets[0].seed.root = None;
        sets[0].focused_view = None;
        sets[0].mounts.clear();
        value.validate(64 * 1024).unwrap();

        let mut missing_owner = serde_json::to_value(&value).unwrap();
        missing_owner["content"]["sets"][0]
            .as_object_mut()
            .unwrap()
            .remove("source_owner");
        assert!(serde_json::from_value::<UiTransferExport>(missing_owner).is_err());

        let mut missing_focus = serde_json::to_value(&value).unwrap();
        missing_focus["content"]["sets"][0]
            .as_object_mut()
            .unwrap()
            .remove("focused_view");
        assert!(serde_json::from_value::<UiTransferExport>(missing_focus).is_err());

        let mut missing_active = serde_json::to_value(&value).unwrap();
        missing_active["content"]
            .as_object_mut()
            .unwrap()
            .remove("active_set");
        assert!(serde_json::from_value::<UiTransferExport>(missing_active).is_err());

        let UiTransferExportContent::ViewSets { sets, .. } = &mut value.content else {
            unreachable!()
        };
        let mut second = sets[0].clone();
        second.handle = "set-two".into();
        second.source_view_set_id = ViewSetId::new(4);
        second.seed.id = "set-two".into();
        second.source_owner = UiViewMountOwner::Surface {
            presentation_id: PresentationId::try_new("other-presentation").unwrap(),
            surface_instance_id: SurfaceInstanceId::try_new("other-surface").unwrap(),
        };
        sets.push(second);
        assert!(value.validate(64 * 1024).is_err());
    }

    #[test]
    fn mount_owner_must_equal_the_declared_set_owner() {
        let mut value = export(UiTransferExportMode::Move);
        let UiTransferExportContent::ViewSets { sets, .. } = &mut value.content else {
            unreachable!()
        };
        sets[0].source_owner = UiViewMountOwner::Surface {
            presentation_id: PresentationId::try_new("source-presentation").unwrap(),
            surface_instance_id: SurfaceInstanceId::try_new("other-surface").unwrap(),
        };
        assert!(value.validate(64 * 1024).is_err());
    }

    #[test]
    fn focus_and_active_set_are_exact_bounded_handles() {
        let mut value = export(UiTransferExportMode::Move);
        let UiTransferExportContent::ViewSets { sets, .. } = &mut value.content else {
            unreachable!()
        };
        sets[0].focused_view = Some(1);
        assert!(value.validate(64 * 1024).is_err());

        let UiTransferExportContent::ViewSets { active_set, sets } = &mut value.content else {
            unreachable!()
        };
        sets[0].focused_view = Some(0);
        *active_set = Some("missing-set".into());
        assert!(value.validate(64 * 1024).is_err());
    }

    #[test]
    fn move_keeps_exact_draft_but_copy_requires_fresh_state() {
        let mut moved = export(UiTransferExportMode::Move);
        let mount = mount_mut(&mut moved);
        mount.drafts.push(UiTransferDraftExport {
            key: InputBufferKey::from_binding_coordinate(
                mount.source_instance.clone(),
                mount.view_ref.clone(),
                "line",
                &mount.binding,
            ),
            state: RyeOsInputState {
                text: "retained draft".into(),
                cursor: "retained draft".len(),
                filter_field: 0,
            },
        });
        moved.validate(64 * 1024).unwrap();

        moved.mode = UiTransferExportMode::Copy;
        assert!(moved.validate(64 * 1024).is_err());
        mount_mut(&mut moved).drafts.clear();
        let values = std::collections::BTreeMap::from([(
            "selection.work".into(),
            serde_json::json!({"thread":"T-one"}),
        )]);
        use sha2::{Digest, Sha256};
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&values).unwrap()));
        mount_mut(&mut moved).subject = SelectionAttachment::Pinned {
            values,
            fingerprint,
        };
        assert!(moved.validate(64 * 1024).is_err());
    }

    #[test]
    fn hidden_lens_followers_cannot_be_left_outside_the_export() {
        let exported = export(UiTransferExportMode::Move);
        let source = exported.mounts()[0].source_instance.clone();
        let other = RyeOsViewInstanceKey::surface_tile(
            &PresentationId::try_new("source-presentation").unwrap(),
            &SurfaceInstanceId::try_new("source-surface").unwrap(),
            TileId::new(9),
        );
        let frame = RetainedLensFrame {
            binding: binding(),
            view_ref: "view:test/return".into(),
            attachment: SelectionAttachment::FollowViewSet {
                view_set_id: ViewSetId::new(3),
            },
            input_route: None,
            label: None,
        };
        let sets = BTreeSet::from([ViewSetId::new(3)]);
        let instances = BTreeSet::from([source.clone()]);
        assert!(
            validate_incoming_followers([(&other, &frame.attachment)], &sets, &instances).is_err()
        );
        validate_incoming_followers([(&source, &frame.attachment)], &sets, &instances).unwrap();
        validate_incoming_followers([(&other, &frame.attachment)], &BTreeSet::new(), &instances)
            .unwrap();
    }

    #[test]
    fn lens_frames_preserve_order_and_validate_each_subject() {
        let mut value = export(UiTransferExportMode::Move);
        let mount = mount_mut(&mut value);
        mount.lens_frames.push(RetainedLensFrame {
            binding: mount.binding.clone(),
            view_ref: mount.view_ref.clone(),
            attachment: mount.subject.clone(),
            input_route: None,
            label: None,
        });
        value.validate(64 * 1024).unwrap();
        mount_mut(&mut value).lens_frames[0].view_ref = "not-a-view".into();
        assert!(value.validate(64 * 1024).is_err());
    }

    #[test]
    fn copy_frame_route_must_be_fresh_even_before_snapshot_validation() {
        let mut value = export(UiTransferExportMode::Copy);
        let mount = mount_mut(&mut value);
        mount.lens_frames.push(RetainedLensFrame {
            binding: mount.binding.clone(),
            view_ref: mount.view_ref.clone(),
            attachment: mount.subject.clone(),
            input_route: Some(serde_json::json!({})),
            label: None,
        });
        assert!(
            value
                .validate(64 * 1024)
                .unwrap_err()
                .contains("fresh route")
        );
        mount_mut(&mut value).lens_frames[0].input_route = None;
        value.validate(64 * 1024).unwrap();
    }

    #[test]
    fn canonical_explicit_null_route_is_not_flattened_to_authored() {
        assert_eq!(snapshot_route(None), UiCompositionRouteAssignment::Authored);
        assert_eq!(
            snapshot_route(Some(&(7, None))),
            UiCompositionRouteAssignment::Override { input_route: None }
        );
    }

    #[test]
    fn singular_surface_mount_needs_no_dummy_set_and_refuses_followers() {
        let fixture = export(UiTransferExportMode::Move);
        let mount = fixture.mounts()[0].clone();
        let mut value = UiTransferExport {
            schema: UI_TRANSFER_EXPORT_SCHEMA,
            mode: UiTransferExportMode::Move,
            content: UiTransferExportContent::SurfaceMount {
                source_view_set_id: ViewSetId::new(3),
                mount,
            },
        };
        assert!(value.sets().is_none());
        assert_eq!(value.mounts().len(), 1);
        value.validate(64 * 1024).unwrap();
        let UiTransferExportContent::SurfaceMount { mount, .. } = &mut value.content else {
            unreachable!()
        };
        mount.subject = SelectionAttachment::FollowViewSet {
            view_set_id: ViewSetId::new(3),
        };
        assert!(value.validate(64 * 1024).is_err());
    }

    #[test]
    fn shared_dock_requires_the_exact_declared_edge_and_no_surface_follower() {
        let fixture = export(UiTransferExportMode::Move);
        let mut mount = fixture.mounts()[0].clone();
        let presentation = PresentationId::try_new("source-presentation").unwrap();
        mount.source_instance = RyeOsViewInstanceKey::shared_dock(&presentation, "left").unwrap();
        let mut value = UiTransferExport {
            schema: UI_TRANSFER_EXPORT_SCHEMA,
            mode: UiTransferExportMode::Move,
            content: UiTransferExportContent::SharedDock {
                edge: RyeOsDockEdge::Left,
                visible: true,
                size: 18,
                mount,
            },
        };
        value.validate(64 * 1024).unwrap();
        let UiTransferExportContent::SharedDock { edge, .. } = &mut value.content else {
            unreachable!()
        };
        *edge = RyeOsDockEdge::Right;
        assert!(value.validate(64 * 1024).is_err());
    }
}
