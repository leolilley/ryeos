//! Closed local composition requests. Shape validation does not admit authority.
use super::binding::UiBindingAttachmentCoordinate;
use crate::ids::{
    PresentationId, RyeOsViewInstanceKey, SurfaceInstanceId, TileId, ViewGroupId, ViewSetId,
};
use crate::layout::{MAX_LAYOUT_DEPTH, MAX_SPLIT_RATIO, MIN_SPLIT_RATIO, SplitAxis, SplitBranch};
use crate::surface::view_sets::{
    MAX_VIEW_SET_LABEL_BYTES, SavedViewSelectionSource, SavedViewSetTemplate,
    validate_saved_view_set_templates,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiCompositionRequest {
    pub schema: u32,
    pub presentation_id: PresentationId,
    pub operation_id: String,
    pub expected_revision: u64,
    pub expected_next_mount_id: u64,
    pub owner: CompositionOwner,
    pub mutation: CompositionMutation,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompositionOwner {
    Shared {
        binding: UiBindingAttachmentCoordinate,
    },
    Surface {
        surface_instance_id: SurfaceInstanceId,
        binding: UiBindingAttachmentCoordinate,
    },
}
impl CompositionOwner {
    pub fn binding(&self) -> &UiBindingAttachmentCoordinate {
        match self {
            Self::Shared { binding } | Self::Surface { binding, .. } => binding,
        }
    }
    pub fn surface_instance_id(&self) -> Option<&SurfaceInstanceId> {
        match self {
            Self::Shared { .. } => None,
            Self::Surface {
                surface_instance_id,
                ..
            } => Some(surface_instance_id),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionTabSeed {
    pub template: SavedViewSetTemplate,
    pub binding: UiBindingAttachmentCoordinate,
    /// Explicit fresh values for reusable required inputs; never copied pins.
    pub fresh_subjects:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, serde_json::Value>>,
    /// Exact existing same-Surface targets, resolved under this request revision.
    pub followed_sets: std::collections::BTreeMap<String, ViewSetId>,
    /// Null requests the seed's normal first-active-view focus. An index selects
    /// one exact recursively ordered seed mount, not a previous mounted ID.
    #[serde(deserialize_with = "required_focus")]
    pub focused_view: Option<usize>,
}
fn required_focus<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    Option::<usize>::deserialize(deserializer)
}
fn validate_tab_focus(tab: &CompositionTabSeed) -> Result<(), String> {
    let required = tab
        .template
        .relationships
        .iter()
        .filter_map(|relationship| {
            if let SavedViewSelectionSource::RequiredSubject { input, .. } = &relationship.source {
                Some(input.as_str())
            } else {
                None
            }
        })
        .collect::<std::collections::BTreeSet<_>>();
    if tab
        .fresh_subjects
        .keys()
        .any(|input| !required.contains(input.as_str()))
    {
        return Err("fresh subject input is not declared by the exact template".into());
    }
    let followed = tab
        .template
        .relationships
        .iter()
        .filter_map(|relationship| {
            if let SavedViewSelectionSource::FollowSet { saved_view_set_id } = &relationship.source
            {
                Some(saved_view_set_id.as_str())
            } else {
                None
            }
        })
        .collect::<std::collections::BTreeSet<_>>();
    for (input, id) in &tab.followed_sets {
        if !followed.contains(input.as_str()) {
            return Err("follow target is not declared by the exact template".into());
        }
        positive(id.0)?;
    }
    fn count(node: &crate::surface::view_sets::LayoutSeedSpec) -> usize {
        match node {
            crate::surface::view_sets::LayoutSeedSpec::Group { views, .. } => views.len(),
            crate::surface::view_sets::LayoutSeedSpec::Split { first, second, .. } => {
                count(first) + count(second)
            }
        }
    }
    let mounts = tab.template.composition.root.as_ref().map_or(0, count);
    if tab.focused_view.is_some_and(|index| index >= mounts) {
        return Err("tab focus index is outside its exact seed mounts".into());
    }
    Ok(())
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionTileTarget {
    pub surface_instance_id: SurfaceInstanceId,
    pub view_set_id: ViewSetId,
    pub tile_id: TileId,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompositionPlacement {
    First,
    GroupTab {
        group_id: ViewGroupId,
        before: Option<TileId>,
    },
    Split {
        target: TileId,
        axis: SplitAxis,
        before: bool,
        ratio: f32,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiCompositionRouteAssignment {
    /// No retained override: the freshly admitted View's authored route owns
    /// initialization. This is distinct from an explicit targetless override.
    Authored,
    Override {
        #[serde(deserialize_with = "super::composition_lens::required_route")]
        input_route: Option<serde_json::Value>,
    },
}
impl UiCompositionRouteAssignment {
    pub fn validate(&self) -> Result<(), String> {
        if let Self::Override {
            input_route: Some(value),
        } = self
        {
            serde_json::from_value::<super::seat::InputRoute>(value.clone())
                .map_err(|error| format!("invalid composition input route: {error}"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompositionMutation {
    SetSubject {
        mount: RyeOsViewInstanceKey,
        attachment: super::attachment::SelectionAttachment,
    },
    InsertTab {
        index: usize,
        tab: CompositionTabSeed,
        activate: bool,
    },
    RestoreTabs {
        tabs: Vec<CompositionTabSeed>,
        active_seed_id: String,
    },
    AppendTabs {
        index: usize,
        tabs: Vec<CompositionTabSeed>,
        active_seed_id: String,
        activate: bool,
    },
    RenameSurface {
        title: String,
    },
    RenameTab {
        tab_id: ViewSetId,
        title: String,
    },
    CloseTab {
        tab_id: ViewSetId,
    },
    ReorderTab {
        tab_id: ViewSetId,
        before: Option<ViewSetId>,
    },
    SelectTab {
        tab_id: ViewSetId,
    },
    InsertView {
        tab_id: ViewSetId,
        placement: CompositionPlacement,
        view_ref: String,
        binding: UiBindingAttachmentCoordinate,
        subject: SavedViewSelectionSource,
        route_assignment: UiCompositionRouteAssignment,
    },
    InsertPinnedView {
        tab_id: ViewSetId,
        placement: CompositionPlacement,
        view_ref: String,
        binding: UiBindingAttachmentCoordinate,
        attachment: super::attachment::SelectionAttachment,
        route_assignment: UiCompositionRouteAssignment,
    },
    /// Live same-Surface relationship, not a reusable saved-set locator.
    InsertFollowingView {
        tab_id: ViewSetId,
        placement: CompositionPlacement,
        view_ref: String,
        binding: UiBindingAttachmentCoordinate,
        attachment: super::attachment::SelectionAttachment,
        route_assignment: UiCompositionRouteAssignment,
    },
    ReplaceFollowingView {
        tab_id: ViewSetId,
        tile_id: TileId,
        view_ref: String,
        binding: UiBindingAttachmentCoordinate,
        attachment: super::attachment::SelectionAttachment,
        route_assignment: UiCompositionRouteAssignment,
    },
    ReplaceView {
        tab_id: ViewSetId,
        tile_id: TileId,
        view_ref: String,
        binding: UiBindingAttachmentCoordinate,
        subject: SavedViewSelectionSource,
        route_assignment: UiCompositionRouteAssignment,
    },
    CloseView {
        tab_id: ViewSetId,
        tile_id: TileId,
    },
    SplitRatio {
        tab_id: ViewSetId,
        path: Vec<SplitBranch>,
        expected_layout_guard: String,
        ratio: f32,
    },
    SelectGroup {
        tab_id: ViewSetId,
        group_id: ViewGroupId,
        tile_id: TileId,
    },
    MoveView {
        source: CompositionTileTarget,
        destination: CompositionTileTarget,
        placement: CompositionPlacement,
    },
    /// One same-seat mutation of two exact Surface owners. `owner` names the
    /// source Surface; this coordinate names the destination Surface. Neither
    /// coordinate grants an attachment or permits cross-seat transfer.
    MoveViewAcrossSurfaces {
        source: CompositionTileTarget,
        destination: CompositionTileTarget,
        destination_binding: UiBindingAttachmentCoordinate,
        placement: CompositionPlacement,
    },
    Arrange {
        tab_id: ViewSetId,
        axis: SplitAxis,
    },
    Promote {
        tab_id: ViewSetId,
        tile_id: TileId,
    },
    FocusView {
        mount: RyeOsViewInstanceKey,
    },
    IsolateView {
        tab_id: ViewSetId,
        tile_id: Option<TileId>,
    },
    LensPush {
        tab_id: ViewSetId,
        tile_id: TileId,
        view_ref: String,
        binding: UiBindingAttachmentCoordinate,
        attachment: super::attachment::SelectionAttachment,
        #[serde(deserialize_with = "super::composition_lens::required_route")]
        input_route: Option<serde_json::Value>,
        #[serde(deserialize_with = "super::composition_lens::required_label")]
        label: Option<String>,
    },
    LensPop {
        tab_id: ViewSetId,
        tile_id: TileId,
    },
}
fn positive(id: u64) -> Result<(), String> {
    if id == 0 {
        Err("zero mounted identity".into())
    } else {
        Ok(())
    }
}
pub(crate) fn binding(value: &UiBindingAttachmentCoordinate) -> Result<(), String> {
    if value.binding_attachment_id.is_empty()
        || value.binding_generation == 0
        || value.binding_digest.is_empty()
        || value.binding_attachment_id.chars().any(char::is_control)
        || value.binding_digest.chars().any(char::is_control)
    {
        Err("incomplete binding coordinate".into())
    } else {
        Ok(())
    }
}
fn ratio(value: f32) -> Result<(), String> {
    if value.is_finite() && (MIN_SPLIT_RATIO..=MAX_SPLIT_RATIO).contains(&value) {
        Ok(())
    } else {
        Err("invalid split ratio".into())
    }
}
fn placement(value: &CompositionPlacement) -> Result<(), String> {
    match value {
        CompositionPlacement::First => Ok(()),
        CompositionPlacement::GroupTab { group_id, before } => {
            positive(group_id.0)?;
            if let Some(id) = before {
                positive(id.0)?;
            }
            Ok(())
        }
        CompositionPlacement::Split {
            target,
            ratio: value,
            ..
        } => {
            positive(target.0)?;
            ratio(*value)
        }
    }
}
fn subject(value: &SavedViewSelectionSource) -> Result<(), String> {
    // Reuse the portable relationship validator with one real local seed slot.
    let template:SavedViewSetTemplate=serde_json::from_value(serde_json::json!({"id":"subject","name":"Subject","composition":{"schema":2,"id":"subject","title":"Subject","root":{"type":"group","views":["view:validation/subject"],"active":0}},"relationships":[{"mount":{"kind":"tile","index":0},"source":value}]})).map_err(|error|error.to_string())?;
    validate_saved_view_set_templates(&[template])
}
pub(crate) fn view(reference: &str) -> Result<(), String> {
    if reference
        .strip_prefix("view:")
        .is_some_and(|path| !path.is_empty())
        && reference.trim() == reference
        && !reference.chars().any(char::is_control)
    {
        Ok(())
    } else {
        Err("invalid View reference shape".into())
    }
}
impl UiCompositionRequest {
    /// Allocate only explicit new-tab seeds. Other allocating mutations require
    /// causal fold context (existing layout/groups), never guessed local state.
    pub fn allocate_tab_manifest(
        &self,
        max_request_bytes: u64,
    ) -> Result<Option<super::composition_identity::UiSurfaceMountManifest>, String> {
        self.validate(max_request_bytes)?;
        let (tabs, active) = match &self.mutation {
            CompositionMutation::InsertTab { tab, .. } => (
                vec![(tab.template.composition.clone(), tab.binding.clone())],
                tab.template.composition.id.clone(),
            ),
            CompositionMutation::RestoreTabs {
                tabs,
                active_seed_id,
            }
            | CompositionMutation::AppendTabs {
                tabs,
                active_seed_id,
                ..
            } => (
                tabs.iter()
                    .map(|tab| (tab.template.composition.clone(), tab.binding.clone()))
                    .collect(),
                active_seed_id.clone(),
            ),
            _ => return Ok(None),
        };
        super::composition_identity::UiSurfaceMountManifest::allocate(
            self.presentation_id.clone(),
            self.owner
                .surface_instance_id()
                .ok_or("shared owner cannot allocate tabs")?
                .clone(),
            self.expected_next_mount_id,
            tabs,
            Some(&active),
        )
        .map(Some)
    }
    pub fn validate(&self, max_request_bytes: u64) -> Result<(), String> {
        if self.schema != 1
            || !super::admission_operation::canonical_operation_id(&self.operation_id)
            || self.expected_next_mount_id == 0
        {
            return Err("invalid composition request identity or frontier".into());
        }
        let bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        if max_request_bytes == 0 || bytes.len() as u64 > max_request_bytes {
            return Err("composition request exceeds signed bound".into());
        }
        binding(self.owner.binding())?;
        if self.owner.surface_instance_id().is_none()
            && !matches!(self.mutation, CompositionMutation::SetSubject { .. })
        {
            return Err("shared owner permits only exact subject changes".into());
        }
        use CompositionMutation::*;
        match &self.mutation {
            InsertFollowingView {
                tab_id,
                placement: where_,
                view_ref,
                binding: coordinate,
                attachment,
                route_assignment,
            } => {
                positive(tab_id.0)?;
                placement(where_)?;
                view(view_ref)?;
                binding(coordinate)?;
                route_assignment.validate()?;
                match attachment {
                    super::attachment::SelectionAttachment::FollowViewSet { view_set_id } => {
                        positive(view_set_id.0)?
                    }
                    _ => return Err("following insertion requires an exact live follower".into()),
                }
            }
            ReplaceFollowingView {
                tab_id,
                tile_id,
                view_ref,
                binding: coordinate,
                attachment,
                route_assignment,
            } => {
                positive(tab_id.0)?;
                positive(tile_id.0)?;
                view(view_ref)?;
                binding(coordinate)?;
                route_assignment.validate()?;
                match attachment {
                    super::attachment::SelectionAttachment::FollowViewSet { view_set_id } => {
                        positive(view_set_id.0)?
                    }
                    _ => return Err("following replacement requires an exact live follower".into()),
                }
            }
            InsertPinnedView {
                tab_id,
                placement: where_,
                view_ref,
                binding: coordinate,
                attachment,
                route_assignment,
            } => {
                positive(tab_id.0)?;
                placement(where_)?;
                view(view_ref)?;
                binding(coordinate)?;
                route_assignment.validate()?;
                if !matches!(
                    attachment,
                    super::attachment::SelectionAttachment::Pinned { .. }
                ) {
                    return Err("pinned insertion requires an exact pinned attachment".into());
                }
            }
            SetSubject { mount, .. } => {
                if mount.mount_owner().is_none() {
                    return Err("subject mount identity is invalid".into());
                }
            }
            InsertTab { index, tab, .. } => {
                if *index >= crate::surface::view_sets::MAX_VIEW_SETS {
                    return Err("tab insertion index exceeds bound".into());
                }
                binding(&tab.binding)?;
                validate_saved_view_set_templates(std::slice::from_ref(&tab.template))?;
                validate_tab_focus(tab)?;
            }
            RestoreTabs {
                tabs,
                active_seed_id,
            }
            | AppendTabs {
                tabs,
                active_seed_id,
                ..
            } => {
                if matches!(&self.mutation, AppendTabs {index,..} if *index >= crate::surface::view_sets::MAX_VIEW_SETS)
                {
                    return Err("tab import insertion index exceeds bound".into());
                }
                if tabs.is_empty() || tabs.len() > crate::surface::view_sets::MAX_VIEW_SETS {
                    return Err("invalid restored tab count".into());
                }
                for tab in tabs {
                    binding(&tab.binding)?;
                    validate_tab_focus(tab)?;
                }
                validate_saved_view_set_templates(
                    &tabs
                        .iter()
                        .map(|tab| tab.template.clone())
                        .collect::<Vec<_>>(),
                )?;
                if !tabs
                    .iter()
                    .any(|tab| &tab.template.composition.id == active_seed_id)
                {
                    return Err("restored active tab is absent".into());
                }
            }
            RenameSurface { title } => {
                if self.owner.surface_instance_id().is_none()
                    || title.trim().is_empty()
                    || title.len() > MAX_VIEW_SET_LABEL_BYTES
                    || title.chars().any(char::is_control)
                {
                    return Err("invalid Surface title or owner".into());
                }
            }
            RenameTab { tab_id, title } => {
                positive(tab_id.0)?;
                if title.trim().is_empty()
                    || title.len() > MAX_VIEW_SET_LABEL_BYTES
                    || title.chars().any(char::is_control)
                {
                    return Err("invalid tab title".into());
                }
            }
            CloseTab { tab_id } | SelectTab { tab_id } | Arrange { tab_id, .. } => {
                positive(tab_id.0)?
            }
            ReorderTab { tab_id, before } => {
                positive(tab_id.0)?;
                if let Some(id) = before {
                    positive(id.0)?;
                    if id == tab_id {
                        return Err("tab cannot precede itself".into());
                    }
                }
            }
            InsertView {
                tab_id,
                placement: where_,
                view_ref,
                binding: coordinate,
                subject: relationship,
                route_assignment,
            } => {
                positive(tab_id.0)?;
                placement(where_)?;
                view(view_ref)?;
                binding(coordinate)?;
                subject(relationship)?;
                route_assignment.validate()?;
            }
            ReplaceView {
                tab_id,
                tile_id,
                view_ref,
                binding: coordinate,
                subject: relationship,
                route_assignment,
            } => {
                positive(tab_id.0)?;
                positive(tile_id.0)?;
                view(view_ref)?;
                binding(coordinate)?;
                subject(relationship)?;
                route_assignment.validate()?;
            }
            LensPush {
                tab_id,
                tile_id,
                view_ref,
                binding: coordinate,
                attachment,
                input_route,
                label,
            } => {
                positive(tab_id.0)?;
                positive(tile_id.0)?;
                view(view_ref)?;
                binding(coordinate)?;
                super::composition_lens::RetainedLensFrame {
                    binding: coordinate.clone(),
                    view_ref: view_ref.clone(),
                    attachment: attachment.clone(),
                    input_route: input_route.clone(),
                    label: label.clone(),
                }
                .validate_shape(max_request_bytes)?;
            }
            CloseView { tab_id, tile_id }
            | Promote { tab_id, tile_id }
            | LensPop { tab_id, tile_id } => {
                positive(tab_id.0)?;
                positive(tile_id.0)?;
            }
            SplitRatio {
                tab_id,
                path,
                expected_layout_guard,
                ratio: value,
            } => {
                positive(tab_id.0)?;
                if path.len() >= MAX_LAYOUT_DEPTH
                    || expected_layout_guard.is_empty()
                    || expected_layout_guard.chars().any(char::is_control)
                {
                    return Err("invalid guarded split path".into());
                }
                ratio(*value)?;
            }
            SelectGroup {
                tab_id,
                group_id,
                tile_id,
            } => {
                positive(tab_id.0)?;
                positive(group_id.0)?;
                positive(tile_id.0)?;
            }
            MoveView {
                source,
                destination,
                placement: where_,
            } => {
                for target in [source, destination] {
                    positive(target.view_set_id.0)?;
                    positive(target.tile_id.0)?;
                }
                if source == destination {
                    return Err("move source equals destination".into());
                }
                placement(where_)?;
            }
            MoveViewAcrossSurfaces {
                source,
                destination,
                destination_binding,
                placement: where_,
            } => {
                for target in [source, destination] {
                    positive(target.view_set_id.0)?;
                    positive(target.tile_id.0)?;
                }
                if self.owner.surface_instance_id() != Some(&source.surface_instance_id)
                    || source.surface_instance_id == destination.surface_instance_id
                {
                    return Err(
                        "cross-Surface move requires distinct exact source and destination owners"
                            .into(),
                    );
                }
                binding(destination_binding)?;
                placement(where_)?;
            }
            FocusView { mount } => {
                let owned_presentation = match mount.mount_owner() {
                    Some(crate::ids::UiViewMountOwner::Surface {
                        presentation_id,
                        surface_instance_id,
                    }) => {
                        presentation_id == self.presentation_id
                            && self.owner.surface_instance_id() == Some(&surface_instance_id)
                    }
                    _ => false,
                };
                if !owned_presentation {
                    return Err("focus requires an exact mount in this presentation".into());
                }
            }
            IsolateView { tab_id, tile_id } => {
                positive(tab_id.0)?;
                if let Some(id) = tile_id {
                    positive(id.0)?;
                }
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> UiCompositionRequest {
        serde_json::from_value(serde_json::json!({"schema":1,"presentation_id":"11111111-1111-4111-8111-111111111111","operation_id":"22222222-2222-4222-8222-222222222222","expected_revision":0,"expected_next_mount_id":1,"owner":{"kind":"surface","surface_instance_id":"33333333-3333-4333-8333-333333333333","binding":{"binding_attachment_id":"binding","binding_generation":1,"binding_digest":"digest"}},"mutation":{"kind":"rename_tab","tab_id":1,"title":"Development"}})).unwrap()
    }
    #[test]
    fn surface_rename_has_exact_owner_and_existing_title_bounds() {
        let mut value = request();
        value.mutation = CompositionMutation::RenameSurface {
            title: "Research".into(),
        };
        value.validate(65536).unwrap();
        for title in [
            " ".to_string(),
            "bad\nlabel".into(),
            "x".repeat(MAX_VIEW_SET_LABEL_BYTES + 1),
        ] {
            value.mutation = CompositionMutation::RenameSurface { title };
            assert!(value.validate(65536).is_err());
        }
        value.mutation = CompositionMutation::RenameSurface {
            title: "Research".into(),
        };
        value.owner = CompositionOwner::Shared {
            binding: value.owner.binding().clone(),
        };
        assert!(value.validate(65536).is_err());
    }

    #[test]
    fn closed_contract_rejects_wrong_identity_limits_and_layout_shapes() {
        let valid = request();
        valid.validate(65536).unwrap();
        assert!(valid.validate(1).is_err());
        let mut zero = valid.clone();
        zero.expected_next_mount_id = 0;
        assert!(zero.validate(65536).is_err());
        let mut operation = valid.clone();
        operation.operation_id = "not-an-operation".into();
        assert!(operation.validate(65536).is_err());
        let mut title = valid.clone();
        title.mutation = CompositionMutation::RenameTab {
            tab_id: ViewSetId::new(0),
            title: "Valid".into(),
        };
        assert!(title.validate(65536).is_err());
        let mut ratio = valid.clone();
        ratio.mutation = CompositionMutation::SplitRatio {
            tab_id: ViewSetId::new(1),
            path: vec![],
            expected_layout_guard: "guard".into(),
            ratio: 1.0,
        };
        assert!(ratio.validate(65536).is_err());
        let mut raw = serde_json::to_value(valid).unwrap();
        raw["mutation"]["intent"] = serde_json::json!({"type":"execute"});
        assert!(serde_json::from_value::<UiCompositionRequest>(raw).is_err());
        let mut focus = request();
        focus.mutation = CompositionMutation::FocusView {
            mount: RyeOsViewInstanceKey::shared_dialog(
                &PresentationId::try_new("foreign-presentation").unwrap(),
                "launcher",
            )
            .unwrap(),
        };
        assert!(focus.validate(65536).is_err());
    }
    #[test]
    fn cross_surface_move_names_two_distinct_exact_owner_coordinates() {
        let mut request = request();
        let source = request.owner.surface_instance_id().unwrap().clone();
        let destination =
            SurfaceInstanceId::try_new("44444444-4444-4444-8444-444444444444").unwrap();
        let destination_binding = UiBindingAttachmentCoordinate {
            binding_attachment_id: "destination".into(),
            binding_generation: 2,
            binding_digest: "digest".into(),
        };
        request.mutation = CompositionMutation::MoveViewAcrossSurfaces {
            source: CompositionTileTarget {
                surface_instance_id: source.clone(),
                view_set_id: ViewSetId::new(1),
                tile_id: TileId::new(1),
            },
            destination: CompositionTileTarget {
                surface_instance_id: destination.clone(),
                view_set_id: ViewSetId::new(2),
                tile_id: TileId::new(2),
            },
            destination_binding: destination_binding.clone(),
            placement: CompositionPlacement::First,
        };
        request.validate(65536).unwrap();
        let mut same = request.clone();
        if let CompositionMutation::MoveViewAcrossSurfaces { destination, .. } = &mut same.mutation
        {
            destination.surface_instance_id = source.clone();
        }
        assert!(same.validate(65536).is_err());
        let mut foreign = request.clone();
        if let CompositionMutation::MoveViewAcrossSurfaces { source, .. } = &mut foreign.mutation {
            source.surface_instance_id = destination;
        }
        assert!(foreign.validate(65536).is_err());
        let mut incomplete = request;
        if let CompositionMutation::MoveViewAcrossSurfaces {
            destination_binding,
            ..
        } = &mut incomplete.mutation
        {
            destination_binding.binding_generation = 0;
        }
        assert!(incomplete.validate(65536).is_err());
        assert_eq!(destination_binding.binding_generation, 2);
    }
    #[test]
    fn explicit_tab_allocation_is_repeatable_and_preserves_frontier() {
        let mut value = request();
        let template=serde_json::from_value(serde_json::json!({"id":"development","name":"Development","composition":{"schema":2,"id":"development","title":"Development","root":{"type":"group","views":["view:test/work","view:test/work"],"active":1}},"relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"follow_own_set"}},{"mount":{"kind":"tile","index":1},"source":{"mode":"follow_own_set"}}]})).unwrap();
        value.mutation = CompositionMutation::InsertTab {
            index: 0,
            tab: CompositionTabSeed {
                template,
                binding: value.owner.binding().clone(),
                fresh_subjects: std::collections::BTreeMap::new(),
                followed_sets: std::collections::BTreeMap::new(),
                focused_view: None,
            },
            activate: true,
        };
        let mut predecessor = serde_json::to_value(&value).unwrap();
        predecessor["mutation"]["tab"]
            .as_object_mut()
            .unwrap()
            .remove("focused_view");
        assert!(serde_json::from_value::<UiCompositionRequest>(predecessor).is_err());
        let before = value.allocate_tab_manifest(65536).unwrap().unwrap();
        for _ in 0..3 {
            let _ = crate::view_set::ViewSet::from_tiling(
                crate::surface::TilingSpec::default(),
                Vec::new(),
                &crate::ids::PresentationId::try_new("allocation-fixture").unwrap(),
                &crate::ids::SurfaceInstanceId::try_new("allocation-surface").unwrap(),
            );
        }
        assert_eq!(before, value.allocate_tab_manifest(65536).unwrap().unwrap());
        assert_eq!(value.expected_next_mount_id, 1);
        value.expected_next_mount_id = u64::MAX;
        assert!(value.allocate_tab_manifest(65536).is_err());
    }
}
