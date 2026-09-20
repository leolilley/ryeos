//! Untrusted, bounded presentation preferences. Never serialize RyeOsCore here:
//! grants, seat authority, observations, pending effects and drafts are excluded.
//! Restoring allocates fresh mounts and revalidates every ref against this session.

use super::model::{RyeOsCore, RyeOsDockContent, RyeOsDockSlotState};
use crate::layout::LayoutTree;
use crate::surface::view_sets::{
    LayoutSeedSpec, SavedViewMountRef, SavedViewSelectionRelationship, SavedViewSelectionSource,
    SavedViewSetTemplate, SavedViewSlotEdge, ViewSetSeedSpec, validate_saved_view_set_templates,
    validate_seeds,
};
use crate::surface::{SlotContentSpec, SlotSpec, SlotsSpec, ViewKindSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_LAYOUT_PREFERENCE_BYTES: usize = 256 * 1024;
const SCHEMA: &str = "ryeos.ui.layout-preferences.v4";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequiredSubjectMountPolicy {
    AllowUnresolved,
    RequireResolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    principal: String,
    surface: String,
    /// Exact compiled surface generation that admitted this arrangement.
    /// A stable surface ref is not enough: authored view sets and slots may
    /// change beneath it, and an older complete composition must not replace
    /// the newly compiled defaults merely because all of its views remain
    /// individually admitted.
    surface_generation: String,
    project: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    schema: String,
    scope: Scope,
    active_view_set: usize,
    view_sets: Vec<SavedViewSet>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedViewSet {
    seed: ViewSetSeedSpec,
    focused_view: Option<usize>,
}

fn slot(slot: &Option<RyeOsDockSlotState>) -> Option<SlotSpec> {
    slot.as_ref().map(|slot| SlotSpec {
        content: match &slot.content {
            RyeOsDockContent::View { view_ref } => SlotContentSpec::View(view_ref.clone()),
        },
        open: slot.visible,
        size: slot.size,
    })
}

fn capture_layout(
    tree: &LayoutTree,
    view_set: &crate::view_set::ViewSet,
) -> Result<LayoutSeedSpec, String> {
    Ok(match tree {
        LayoutTree::Group {
            label,
            tabs,
            active,
            ..
        } => LayoutSeedSpec::Group {
            label: label.clone(),
            views: tabs
                .iter()
                .map(|id| {
                    let tile = view_set
                        .tiles
                        .get(id)
                        .ok_or("layout references an unmounted view")?;
                    serde_json::from_value::<ViewKindSpec>(serde_json::Value::String(
                        tile.view.view_ref.clone(),
                    ))
                    .map_err(|e| e.to_string())
                })
                .collect::<Result<_, String>>()?,
            active: tabs
                .iter()
                .position(|id| id == active)
                .ok_or("invalid active view")?,
        },
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => LayoutSeedSpec::Split {
            axis: *axis,
            ratio: *ratio,
            first: Box::new(capture_layout(first, view_set)?),
            second: Box::new(capture_layout(second, view_set)?),
        },
    })
}

fn capture_view_set(
    view_set: &crate::view_set::ViewSet,
    id: String,
) -> Result<ViewSetSeedSpec, String> {
    Ok(ViewSetSeedSpec {
        id,
        title: view_set.title.clone(),
        root: view_set
            .root
            .as_ref()
            .map(|root| capture_layout(root, view_set))
            .transpose()?,
        slots: SlotsSpec {
            top: slot(&view_set.docks.top),
            bottom: slot(&view_set.docks.bottom),
            left: slot(&view_set.docks.left),
            right: slot(&view_set.docks.right),
        },
    })
}

fn center_mounts(
    view_set: &crate::view_set::ViewSet,
) -> Result<Vec<crate::ids::RyeOsViewInstanceKey>, String> {
    fn collect(
        tree: &LayoutTree,
        view_set: &crate::view_set::ViewSet,
        mounts: &mut Vec<crate::ids::RyeOsViewInstanceKey>,
    ) -> Result<(), String> {
        match tree {
            LayoutTree::Group { tabs, .. } => {
                for tile_id in tabs {
                    mounts.push(
                        view_set
                            .tiles
                            .get(tile_id)
                            .ok_or("layout references an unmounted view")?
                            .instance_key
                            .clone(),
                    );
                }
            }
            LayoutTree::Split { first, second, .. } => {
                collect(first, view_set, mounts)?;
                collect(second, view_set, mounts)?;
            }
        }
        Ok(())
    }

    let mut mounts = Vec::new();
    if let Some(root) = &view_set.root {
        collect(root, view_set, &mut mounts)?;
    }
    Ok(mounts)
}

fn relationship_mounts(
    view_set: &crate::view_set::ViewSet,
) -> Result<Vec<(SavedViewMountRef, crate::ids::RyeOsViewInstanceKey)>, String> {
    use super::model::{RyeOsDockEdge, dock_view_instance_key};
    let mut mounts = center_mounts(view_set)?
        .into_iter()
        .enumerate()
        .map(|(index, instance)| (SavedViewMountRef::Tile { index }, instance))
        .collect::<Vec<_>>();
    for (edge, saved_edge) in [
        (RyeOsDockEdge::Top, SavedViewSlotEdge::Top),
        (RyeOsDockEdge::Bottom, SavedViewSlotEdge::Bottom),
        (RyeOsDockEdge::Left, SavedViewSlotEdge::Left),
        (RyeOsDockEdge::Right, SavedViewSlotEdge::Right),
    ] {
        if view_set.docks.slot(edge).is_some() {
            mounts.push((
                SavedViewMountRef::Slot { edge: saved_edge },
                dock_view_instance_key(view_set.id, edge),
            ));
        }
    }
    Ok(mounts)
}

impl RyeOsCore {
    pub(crate) fn live_saved_set_resolutions(
        &self,
    ) -> Result<BTreeMap<String, crate::ids::ViewSetId>, String> {
        let mut resolved = BTreeMap::new();
        for (view_set_id, saved_id) in &self.view_set_template_ids {
            if resolved.insert(saved_id.clone(), *view_set_id).is_some() {
                return Err(format!(
                    "saved view set has multiple live instances and cannot be linked unambiguously: {saved_id}"
                ));
            }
        }
        Ok(resolved)
    }

    fn preference_scope(&self) -> Result<Scope, String> {
        let session = self
            .data
            .session
            .as_ref()
            .ok_or("layout preferences require a session")?;
        let principal = session
            .user_principal_id
            .as_ref()
            .filter(|id| !id.is_empty())
            .ok_or("layout preferences require an authenticated principal")?;
        let surface = self
            .binding_attachment(&self.surface_attachment_id)
            .ok_or("layout preferences require the admitted surface attachment")?;
        if surface.surface_generation.is_empty() {
            return Err("layout preferences require an exact surface generation".into());
        }
        Ok(Scope {
            principal: principal.clone(),
            surface: surface.surface_ref.clone(),
            surface_generation: surface.surface_generation.clone(),
            project: surface.project_path.clone(),
        })
    }

    pub fn layout_preference_key(&self) -> Result<String, String> {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(&self.preference_scope()?).map_err(|e| e.to_string())?;
        Ok(format!("{SCHEMA}:{:x}", Sha256::digest(bytes)))
    }

    pub fn export_layout_preferences(&self) -> Result<String, String> {
        self.validate_layout_export_contexts()?;
        let view_sets = self
            .view_sets
            .iter()
            .enumerate()
            .map(|(index, view_set)| {
                Ok(SavedViewSet {
                    seed: capture_view_set(view_set, format!("view-set-{index}"))?,
                    focused_view: view_set
                        .tile_ids()
                        .iter()
                        .position(|id| *id == view_set.focused_tile),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let snapshot = Preferences {
            schema: SCHEMA.into(),
            scope: self.preference_scope()?,
            active_view_set: self.active_view_set,
            view_sets,
        };
        let encoded = serde_json::to_string(&snapshot).map_err(|e| e.to_string())?;
        if encoded.len() > MAX_LAYOUT_PREFERENCE_BYTES {
            return Err("layout preferences exceed byte limit".into());
        }
        Ok(encoded)
    }

    /// The current preference schema records composition only; it cannot
    /// represent which admitted attachment owns each insertion point/mount.
    /// Refuse mixed or unresolved layouts instead of exporting a snapshot
    /// that restoration would silently retarget onto the authored surface.
    fn validate_layout_export_contexts(&self) -> Result<(), String> {
        use super::model::{RyeOsDockEdge, dock_view_instance_key};

        for (instance, attachment) in &self.selection_attachments {
            let owner = self
                .view_set_index_for_instance(instance)
                .map(|index| self.view_sets[index].id);
            match attachment {
                super::attachment::SelectionAttachment::FollowViewSet { view_set_id }
                    if owner == Some(*view_set_id) => {}
                _ => return Err(
                    "layout preferences cannot preserve pinned or cross-set subject relationships"
                        .into(),
                ),
            }
        }
        for view_set in &self.view_sets {
            if self.view_set_insertion_attachments.get(&view_set.id)
                != Some(&self.surface_attachment_id)
            {
                return Err(
                    "layout preferences cannot represent this view set's insertion context".into(),
                );
            }
            let tile_contexts_match = view_set.tiles.values().all(|tile| {
                self.instance_binding_attachments.get(&tile.instance_key)
                    == Some(&self.surface_attachment_id)
            });
            let dock_contexts_match = [
                RyeOsDockEdge::Top,
                RyeOsDockEdge::Bottom,
                RyeOsDockEdge::Left,
                RyeOsDockEdge::Right,
            ]
            .into_iter()
            .filter(|edge| view_set.docks.slot(*edge).is_some())
            .all(|edge| {
                self.instance_binding_attachments
                    .get(&dock_view_instance_key(view_set.id, edge))
                    == Some(&self.surface_attachment_id)
            });
            if !tile_contexts_match || !dock_contexts_match {
                return Err(
                    "layout preferences cannot represent mixed or unresolved mount contexts".into(),
                );
            }
        }
        Ok(())
    }

    /// Capture only the active set's reusable composition. The caller supplies
    /// the stable personal-library identity and display name; neither can grant
    /// access to a view or retain this session's runtime state.
    pub fn export_active_view_set_template(
        &self,
        id: String,
        name: String,
    ) -> Result<SavedViewSetTemplate, String> {
        self.capture_view_set_template(id, name, None)
    }

    /// A composition-management view is not part of the composition it saves.
    /// Remove only its exact mounted instance from a detached copy; never
    /// close live views or identify management UI by a hardcoded product ref.
    pub(crate) fn capture_view_set_template(
        &self,
        id: String,
        name: String,
        exclude: Option<&crate::ids::RyeOsViewInstanceKey>,
    ) -> Result<SavedViewSetTemplate, String> {
        self.capture_view_set_template_with_relationships(id, name, exclude, &BTreeMap::new())
    }

    /// Capture portable selection relationships. Cross-set followers require
    /// an explicit runtime-set to saved-template mapping; runtime ids are never
    /// guessed or persisted as if they were stable library identities.
    pub(crate) fn capture_view_set_template_with_relationships(
        &self,
        id: String,
        name: String,
        exclude: Option<&crate::ids::RyeOsViewInstanceKey>,
        saved_set_ids: &BTreeMap<crate::ids::ViewSetId, String>,
    ) -> Result<SavedViewSetTemplate, String> {
        let view_set = self
            .view_sets
            .get(self.active_view_set)
            .ok_or("active view set is unavailable")?;
        let mut captured = view_set.clone();
        if let Some(instance) = exclude {
            if self.view_set_index_for_instance(instance) != Some(self.active_view_set)
                || self.mounted_view_ref(instance).is_none()
            {
                return Err(
                    "composition-management instance is not mounted in this view set".into(),
                );
            }
            if let Some(tile) = captured
                .tiles
                .iter()
                .find_map(|(id, tile)| (&tile.instance_key == instance).then_some(*id))
            {
                captured.close_tile(tile);
            } else {
                use super::model::{RyeOsDockEdge, dock_view_instance_key};
                let edge = [
                    RyeOsDockEdge::Top,
                    RyeOsDockEdge::Bottom,
                    RyeOsDockEdge::Left,
                    RyeOsDockEdge::Right,
                ]
                .into_iter()
                .find(|edge| dock_view_instance_key(captured.id, *edge) == *instance)
                .ok_or("composition-management instance is not mounted in this view set")?;
                match edge {
                    RyeOsDockEdge::Top => captured.docks.top = None,
                    RyeOsDockEdge::Bottom => captured.docks.bottom = None,
                    RyeOsDockEdge::Left => captured.docks.left = None,
                    RyeOsDockEdge::Right => captured.docks.right = None,
                }
            }
        }
        // Project bindings remain one insertion context. Subject relationships
        // are captured separately below without persisting runtime authority.
        let insertion = self
            .insertion_attachment_id(captured.id)
            .ok_or("the view set's insertion context is unavailable")?;
        let mounts = relationship_mounts(&captured)?;
        let mut relationships = Vec::with_capacity(mounts.len());
        for (mount, instance) in mounts {
            if self
                .instance_binding_attachments
                .get(&instance)
                .map(String::as_str)
                != Some(insertion)
            {
                return Err("this composition has mixed or unresolved project contexts; the reusable template cannot preserve them".into());
            }
            let source = match self.selection_attachment_for_instance(&instance) {
                Some(super::attachment::SelectionAttachment::RequiredSubject { input, facets }) => {
                    let view_ref = self
                        .mounted_view_ref(&instance)
                        .ok_or("saved relationship references an unmounted view")?;
                    let binding = self
                        .binding_for_instance(&instance, view_ref)
                        .ok_or("saved relationship binding is unavailable")?;
                    let declared = super::attachment::selection_dependencies(binding);
                    if declared.len() != facets.len()
                        || facets.iter().any(|facet| !declared.contains(facet))
                    {
                        return Err(
                            "unresolved subject no longer matches the admitted view binding".into(),
                        );
                    }
                    SavedViewSelectionSource::RequiredSubject { input, facets }
                }
                Some(super::attachment::SelectionAttachment::Pinned { .. }) => {
                    let view_ref = self
                        .mounted_view_ref(&instance)
                        .ok_or("saved relationship references an unmounted view")?;
                    let binding = self
                        .binding_for_instance(&instance, view_ref)
                        .ok_or("saved relationship binding is unavailable")?;
                    let facets = super::attachment::selection_dependencies(binding)
                        .into_iter()
                        .collect::<Vec<_>>();
                    if facets.is_empty() {
                        return Err("pinned subject has no portable selection facets".into());
                    }
                    let input = match &mount {
                        SavedViewMountRef::Tile { index } => format!("subject_tile_{index}"),
                        SavedViewMountRef::Slot { edge } => {
                            format!("subject_slot_{}", format!("{edge:?}").to_lowercase())
                        }
                    };
                    SavedViewSelectionSource::RequiredSubject { input, facets }
                }
                Some(super::attachment::SelectionAttachment::FollowViewSet { view_set_id })
                    if view_set_id == captured.id =>
                {
                    SavedViewSelectionSource::FollowOwnSet
                }
                Some(super::attachment::SelectionAttachment::FollowViewSet { view_set_id }) => {
                    let saved_view_set_id = saved_set_ids.get(&view_set_id).cloned().ok_or(
                        "this composition follows another open set without an explicit saved-set identity",
                    )?;
                    SavedViewSelectionSource::FollowSet { saved_view_set_id }
                }
                None => return Err("saved relationship has no selection owner".into()),
            };
            relationships.push(SavedViewSelectionRelationship { mount, source });
        }
        let template = SavedViewSetTemplate {
            composition: capture_view_set(&captured, id.clone())?,
            id,
            name,
            relationships,
        };
        validate_saved_view_set_templates(std::slice::from_ref(&template))?;
        Ok(template)
    }

    /// Open a reusable composition as a fresh set. This is intentionally not
    /// resume: subjects must be selected again through current admitted
    /// context, and no work, draft, observation or authority is restored.
    pub fn open_saved_view_set_template(
        &mut self,
        template: &SavedViewSetTemplate,
        insertion_attachment_id: &str,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        let saved_sets = if template.relationships.iter().any(|relationship| {
            matches!(
                &relationship.source,
                SavedViewSelectionSource::FollowSet { .. }
            )
        }) {
            self.live_saved_set_resolutions()?
        } else {
            BTreeMap::new()
        };
        self.open_saved_view_set_template_with_policy(
            template,
            insertion_attachment_id,
            &BTreeMap::new(),
            &saved_sets,
            RequiredSubjectMountPolicy::AllowUnresolved,
        )
    }

    /// Open with explicit fresh subjects and explicit saved-set resolutions.
    /// Neither input can grant authority: the insertion attachment and every
    /// view are still revalidated before the fresh mounts are committed.
    pub fn open_saved_view_set_template_with_relationships(
        &mut self,
        template: &SavedViewSetTemplate,
        insertion_attachment_id: &str,
        fresh_subjects: &BTreeMap<String, BTreeMap<String, Value>>,
        saved_sets: &BTreeMap<String, crate::ids::ViewSetId>,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        self.open_saved_view_set_template_with_policy(
            template,
            insertion_attachment_id,
            fresh_subjects,
            saved_sets,
            RequiredSubjectMountPolicy::RequireResolved,
        )
    }

    pub(crate) fn open_saved_view_set_template_with_policy(
        &mut self,
        template: &SavedViewSetTemplate,
        insertion_attachment_id: &str,
        fresh_subjects: &BTreeMap<String, BTreeMap<String, Value>>,
        saved_sets: &BTreeMap<String, crate::ids::ViewSetId>,
        subject_policy: RequiredSubjectMountPolicy,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        self.mount_saved_view_set_template_with_relationships(
            template,
            insertion_attachment_id,
            fresh_subjects,
            saved_sets,
            subject_policy,
        )?;
        Ok(self.refresh_view_set_sources())
    }

    /// Commit a validated composition and its subject relationships without
    /// emitting source work. Particular-set resume uses this boundary so its
    /// freshly resolved logical subject is installed before the first fetch.
    pub(crate) fn mount_saved_view_set_template_with_relationships(
        &mut self,
        template: &SavedViewSetTemplate,
        insertion_attachment_id: &str,
        fresh_subjects: &BTreeMap<String, BTreeMap<String, Value>>,
        saved_sets: &BTreeMap<String, crate::ids::ViewSetId>,
        subject_policy: RequiredSubjectMountPolicy,
    ) -> Result<(), String> {
        if self.view_sets.len() >= crate::surface::view_sets::MAX_VIEW_SETS {
            return Err("view set limit reached".into());
        }
        validate_saved_view_set_templates(std::slice::from_ref(template))?;
        let insertion_context = self
            .binding_attachments
            .get(insertion_attachment_id)
            .ok_or("saved view set requires a live admitted insertion context")?;
        let tiling = self
            .view_sets
            .get(self.active_view_set)
            .ok_or("active view set is unavailable")?
            .tiling
            .clone();
        let view_set = template.composition.instantiate(&tiling)?;
        for tile in view_set.tiles.values() {
            if !insertion_context.views.contains_key(&tile.view.view_ref) {
                return Err(format!(
                    "saved view is not admitted: {}",
                    tile.view.view_ref
                ));
            }
        }
        for slot in [
            &view_set.docks.top,
            &view_set.docks.bottom,
            &view_set.docks.left,
            &view_set.docks.right,
        ]
        .into_iter()
        .flatten()
        {
            let RyeOsDockContent::View { view_ref } = &slot.content;
            if !insertion_context.views.contains_key(view_ref) {
                return Err(format!("saved slot is not admitted: {view_ref}"));
            }
        }
        let relationship_mounts = relationship_mounts(&view_set)?;
        let relationship_instances = relationship_mounts.into_iter().collect::<BTreeMap<_, _>>();
        let mut instance_view_refs = view_set
            .tiles
            .values()
            .map(|tile| (tile.instance_key.clone(), tile.view.view_ref.clone()))
            .collect::<BTreeMap<_, _>>();
        for (edge, slot) in [
            (super::model::RyeOsDockEdge::Top, &view_set.docks.top),
            (super::model::RyeOsDockEdge::Bottom, &view_set.docks.bottom),
            (super::model::RyeOsDockEdge::Left, &view_set.docks.left),
            (super::model::RyeOsDockEdge::Right, &view_set.docks.right),
        ] {
            if let Some(slot) = slot {
                let RyeOsDockContent::View { view_ref } = &slot.content;
                instance_view_refs.insert(
                    super::model::dock_view_instance_key(view_set.id, edge),
                    view_ref.clone(),
                );
            }
        }
        let mut resolved_relationships = Vec::with_capacity(template.relationships.len());
        for relationship in &template.relationships {
            let instance = relationship_instances
                .get(&relationship.mount)
                .cloned()
                .ok_or("saved relationship mount is unavailable")?;
            let attachment = match &relationship.source {
                SavedViewSelectionSource::FollowOwnSet => {
                    super::attachment::SelectionAttachment::FollowViewSet {
                        view_set_id: view_set.id,
                    }
                }
                SavedViewSelectionSource::FollowSet { saved_view_set_id } => {
                    let view_set_id =
                        saved_sets.get(saved_view_set_id).copied().ok_or_else(|| {
                            format!("required saved view set is not open: {saved_view_set_id}")
                        })?;
                    if !self.view_sets.iter().any(|set| set.id == view_set_id) {
                        return Err(format!(
                            "required saved view set is unavailable: {saved_view_set_id}"
                        ));
                    }
                    super::attachment::SelectionAttachment::FollowViewSet { view_set_id }
                }
                SavedViewSelectionSource::RequiredSubject { input, facets } => {
                    let view_ref = instance_view_refs
                        .get(&instance)
                        .ok_or("saved relationship view is unavailable")?;
                    let binding = insertion_context
                        .views
                        .get(view_ref)
                        .ok_or("saved relationship binding is unavailable")?;
                    let declared = super::attachment::selection_dependencies(binding);
                    if declared.len() != facets.len()
                        || facets.iter().any(|facet| !declared.contains(facet))
                    {
                        return Err(format!(
                            "required fresh subject has facets not read by the current view: {input}"
                        ));
                    }
                    let Some(supplied) = fresh_subjects.get(input) else {
                        if subject_policy == RequiredSubjectMountPolicy::AllowUnresolved {
                            resolved_relationships.push((
                                instance,
                                super::attachment::SelectionAttachment::RequiredSubject {
                                    input: input.clone(),
                                    facets: facets.clone(),
                                },
                            ));
                            continue;
                        }
                        return Err(format!("required fresh subject is missing: {input}"));
                    };
                    if supplied.len() != facets.len()
                        || facets.iter().any(|facet| !supplied.contains_key(facet))
                    {
                        return Err(format!(
                            "fresh subject does not exactly satisfy required facets: {input}"
                        ));
                    }
                    let encoded =
                        serde_json::to_vec(supplied).map_err(|error| error.to_string())?;
                    let byte_limit =
                        usize::try_from(insertion_context.binding_request_bounds.max_request_bytes)
                            .map_err(|_| "binding request byte bound exceeds this platform")?;
                    if byte_limit == 0 || encoded.len() > byte_limit {
                        return Err(format!(
                            "fresh subject exceeds binding request bounds: {input}"
                        ));
                    }
                    use sha2::{Digest, Sha256};
                    super::attachment::SelectionAttachment::Pinned {
                        values: supplied.clone(),
                        fingerprint: format!("{:x}", Sha256::digest(&encoded)),
                    }
                }
            };
            resolved_relationships.push((instance, attachment));
        }
        self.view_sets.push(view_set);
        self.active_view_set = self.view_sets.len() - 1;
        if !self.stamp_view_set_mounts(self.active_view_set, insertion_attachment_id) {
            self.view_sets.pop();
            self.active_view_set = self.active_view_set.saturating_sub(1);
            return Err("saved view set lost its admitted insertion context".into());
        }
        for (instance, attachment) in resolved_relationships {
            self.selection_attachments.insert(instance, attachment);
        }
        self.view_set_template_ids
            .insert(self.view_sets[self.active_view_set].id, template.id.clone());
        Ok(())
    }

    pub fn restore_layout_preferences(
        &mut self,
        encoded: &str,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        // This restores presentation only; it has no authority to discard live
        // input. In particular, a later browser/client caller must not turn the
        // startup restore API into an implicit "discard all drafts" action.
        if self.view_sets.iter().any(|view_set| {
            view_set
                .input_buffers
                .values()
                .any(|input| !input.text.is_empty())
        }) {
            return Err(
                "layout restoration would discard current input; clear it explicitly first".into(),
            );
        }
        if encoded.len() > MAX_LAYOUT_PREFERENCE_BYTES {
            return Err("layout preferences exceed byte limit".into());
        }
        let snapshot: Preferences = serde_json::from_str(encoded)
            .map_err(|e| format!("invalid layout preferences: {e}"))?;
        if snapshot.schema != SCHEMA || snapshot.scope != self.preference_scope()? {
            return Err("layout preferences have a different schema or session scope".into());
        }
        if snapshot.view_sets.is_empty() || snapshot.active_view_set >= snapshot.view_sets.len() {
            return Err("layout preferences have no valid active view set".into());
        }
        validate_seeds(
            &snapshot
                .view_sets
                .iter()
                .map(|saved| saved.seed.clone())
                .collect::<Vec<_>>(),
        )?;
        let tiling = self.view_sets[self.active_view_set].tiling.clone();
        let insertion_attachment_id = self.surface_attachment_id.clone();
        let insertion_context = self
            .binding_attachments
            .get(&insertion_attachment_id)
            .ok_or("layout restoration requires the admitted surface attachment")?;
        let mut restored = Vec::with_capacity(snapshot.view_sets.len());
        for saved in snapshot.view_sets {
            let mut view_set = saved.seed.instantiate(&tiling)?;
            for tile in view_set.tiles.values() {
                if !insertion_context.views.contains_key(&tile.view.view_ref) {
                    return Err(format!(
                        "saved view is not admitted: {}",
                        tile.view.view_ref
                    ));
                }
            }
            // Closed slots need the same check as visible slots: they may be
            // revealed later, so hiding cannot smuggle an unadmitted view in.
            for slot in [
                &view_set.docks.top,
                &view_set.docks.bottom,
                &view_set.docks.left,
                &view_set.docks.right,
            ]
            .into_iter()
            .flatten()
            {
                let RyeOsDockContent::View { view_ref } = &slot.content;
                if !insertion_context.views.contains_key(view_ref) {
                    return Err(format!("saved slot is not admitted: {view_ref}"));
                }
            }
            if let Some(index) = saved.focused_view {
                let id = *view_set
                    .tile_ids()
                    .get(index)
                    .ok_or("saved focus is outside the view set")?;
                view_set.focus_tile(id);
            }
            restored.push(view_set);
        }
        // Complete validation precedes replacement. This operation never replays
        // an input/command, mutates the seat route or imports saved privileges.
        // Fresh mounts must not retain pins/follow links to retired instances.
        self.selection_attachments.clear();
        self.instance_binding_attachments.clear();
        self.view_set_insertion_attachments.clear();
        self.view_set_template_ids.clear();
        self.view_sets = restored;
        self.active_view_set = snapshot.active_view_set;
        for index in 0..self.view_sets.len() {
            if !self.stamp_view_set_mounts(index, &insertion_attachment_id) {
                return Err("layout restoration lost its admitted insertion context".into());
            }
        }
        Ok(self.refresh_view_set_sources())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::model::{BrowserSession, BrowserViewport, RyeOsInputState};
    use serde_json::json;

    fn core() -> RyeOsCore {
        RyeOsCore::new(
            BrowserSession {
                ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
                session_id: "session:test".into(),
                user_principal_id: Some("fp:operator".into()),
                surface_attachment_id: "attachment:test".into(),
                binding_attachments: vec![crate::ui::UiBindingAttachment {
                    binding_attachment_id: "attachment:test".into(),
                    binding_generation: 1,
                    binding_digest: "binding-generation-one".into(),
                    surface_ref: "surface:test/work".into(),
                    surface_generation: "surface-generation-one".into(),
                    effective_surface: json!({
                        "name": "Work", "tiles": ["view:test/one", "view:test/two"],
                        "views": {
                            "view:test/one": {
                                "widget": "text",
                                "sources": {"default": {
                                    "ref": "service:test/one",
                                    "params": {"subject": "@facet:selection.work.id"}
                                }}
                            },
                            "view:test/two": { "widget": "text" }
                        }
                    }),
                    project_path: None,
                    posture: Default::default(),
                    binding_request_bounds: crate::ui::UiBindingRequestBounds {
                        max_request_bytes: 64 * 1024,
                        max_input_bytes: 16 * 1024,
                    },
                }],
                ..Default::default()
            },
            BrowserViewport::default(),
            0,
        )
    }

    fn add_subject_dock(core: &mut RyeOsCore) -> crate::ids::RyeOsViewInstanceKey {
        let view_ref = "view:test/subject-dock";
        core.binding_attachments
            .get_mut("attachment:test")
            .unwrap()
            .views
            .insert(
                view_ref.into(),
                serde_json::from_value(json!({
                    "widget": "rows",
                    "sources": {"default": {
                        "ref": "service:test/subject-dock",
                        "params": {"thread": "@facet:selection.work.thread"}
                    }}
                }))
                .unwrap(),
            );
        core.view_sets[core.active_view_set].docks.bottom = Some(RyeOsDockSlotState {
            visible: true,
            size: 9,
            content: RyeOsDockContent::View {
                view_ref: view_ref.into(),
            },
        });
        let instance = super::super::model::dock_view_instance_key(
            core.view_sets[core.active_view_set].id,
            super::super::model::RyeOsDockEdge::Bottom,
        );
        core.stamp_instance_binding(instance.clone(), "attachment:test");
        instance
    }

    #[test]
    fn arrangement_round_trip_allocates_fresh_ids_and_never_saves_drafts() {
        let mut source = core();
        source.view_sets[0].input_buffers.insert(
            "test".into(),
            RyeOsInputState {
                text: "private-unsent-draft".into(),
                ..Default::default()
            },
        );
        let ids = source.view_sets[0].tile_ids();
        source.view_sets[0].move_tile_to_group(ids[1], ids[0], 1);
        let encoded = source.export_layout_preferences().unwrap();
        assert!(!encoded.contains("private-unsent-draft"));
        assert!(!encoded.contains("binding_digest"));
        let mut target = core();
        let seat_before = target.seat.fold().snapshot();
        let effects = target.restore_layout_preferences(&encoded).unwrap();
        assert!(effects.iter().all(|effect| !matches!(
            effect.kind,
            super::super::effect::RyeOsEffectKind::InvokeBinding { .. }
        )));
        assert_eq!(target.seat.fold().snapshot(), seat_before);
        assert_eq!(
            target.view_sets[0]
                .root
                .as_ref()
                .unwrap()
                .active_tile_ids()
                .len(),
            1
        );
        assert!(target.view_sets[0].input_buffers.is_empty());
        assert!(
            target.view_sets[0]
                .tile_ids()
                .iter()
                .all(|id| !ids.contains(id))
        );
    }

    #[test]
    fn management_capture_omits_only_its_mount_without_changing_live_layout() {
        let source = core();
        let original = source.export_layout_preferences().unwrap();
        let ids = source.view_sets[0].tile_ids();
        let instance = &source.view_sets[0].tiles[&ids[0]].instance_key;
        let template = source
            .capture_view_set_template("saved".into(), "Saved".into(), Some(instance))
            .unwrap();
        let encoded = serde_json::to_string(&template).unwrap();
        assert!(!encoded.contains("view:test/one"));
        assert!(encoded.contains("view:test/two"));
        assert_eq!(source.export_layout_preferences().unwrap(), original);
    }

    #[test]
    fn refused_preferences_leave_live_layout_unchanged() {
        let mut target = core();
        let original = target.view_sets[0].root.clone();
        let mut saved: serde_json::Value =
            serde_json::from_str(&target.export_layout_preferences().unwrap()).unwrap();
        saved["scope"]["principal"] = json!("fp:someone-else");
        assert!(
            target
                .restore_layout_preferences(&saved.to_string())
                .is_err()
        );
        assert_eq!(target.view_sets[0].root, original);
        let mut saved: serde_json::Value =
            serde_json::from_str(&target.export_layout_preferences().unwrap()).unwrap();
        saved["view_sets"][0]["seed"]["slots"] =
            json!({"left": {"content": "view:unadmitted", "open": false, "size": 20}});
        assert!(
            target
                .restore_layout_preferences(&saved.to_string())
                .is_err()
        );
        assert_eq!(target.view_sets[0].root, original);
        assert!(
            target
                .restore_layout_preferences(&" ".repeat(MAX_LAYOUT_PREFERENCE_BYTES + 1))
                .is_err()
        );
    }

    #[test]
    fn management_capture_checks_slot_mount_and_preserves_other_instances() {
        use super::super::model::{RyeOsDockEdge, dock_view_instance_key};
        let mut source = core();
        let instance = dock_view_instance_key(source.view_sets[0].id, RyeOsDockEdge::Left);
        assert!(
            source
                .capture_view_set_template("saved".into(), "Saved".into(), Some(&instance))
                .is_err()
        );
        source.view_sets[0].docks.left = Some(RyeOsDockSlotState {
            visible: true,
            size: 24,
            content: RyeOsDockContent::View {
                view_ref: "view:test/one".into(),
            },
        });
        let original = source.export_layout_preferences().unwrap();
        let template = source
            .capture_view_set_template("saved".into(), "Saved".into(), Some(&instance))
            .unwrap();
        assert!(template.composition.slots.left.is_none());
        assert!(
            serde_json::to_string(&template)
                .unwrap()
                .contains("view:test/one")
        );
        assert_eq!(source.export_layout_preferences().unwrap(), original);
    }

    #[test]
    fn authored_surface_generation_retires_predecessor_layout() {
        let source = core();
        let saved = source.export_layout_preferences().unwrap();
        let source_key = source.layout_preference_key().unwrap();

        let mut successor = core();
        successor
            .binding_attachments
            .get_mut("attachment:test")
            .unwrap()
            .descriptor
            .surface_generation = "surface-generation-two".into();

        assert_ne!(source_key, successor.layout_preference_key().unwrap());
        assert!(successor.restore_layout_preferences(&saved).is_err());
    }

    #[test]
    fn unrelated_binding_generation_does_not_retire_layout() {
        let source = core();
        let source_key = source.layout_preference_key().unwrap();

        let mut successor = core();
        successor
            .binding_attachments
            .get_mut("attachment:test")
            .unwrap()
            .descriptor
            .binding_digest = "binding-generation-two".into();

        assert_eq!(source_key, successor.layout_preference_key().unwrap());
    }

    #[test]
    fn missing_surface_generation_disables_layout_persistence() {
        let mut target = core();
        target
            .binding_attachments
            .get_mut("attachment:test")
            .unwrap()
            .descriptor
            .surface_generation
            .clear();

        assert!(target.layout_preference_key().is_err());
        assert!(target.export_layout_preferences().is_err());
    }

    #[test]
    fn mixed_attachment_mounts_cannot_be_exported_as_surface_preferences() {
        let mut target = core();
        let instance = target.view_sets[0]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        target
            .instance_binding_attachments
            .insert(instance, "attachment:project".into());

        assert!(target.export_layout_preferences().is_err());
    }

    #[test]
    fn reusable_template_opens_unresolved_without_serializing_a_pin() {
        let mut target = core();
        let instance = target.view_sets[0]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        target.selection_attachments.insert(
            instance,
            super::super::attachment::SelectionAttachment::Pinned {
                values: std::collections::BTreeMap::new(),
                fingerprint: "fixture-pin".into(),
            },
        );
        let template = target
            .export_active_view_set_template("saved".into(), "Saved".into())
            .unwrap();
        let encoded = serde_json::to_string(&template).unwrap();
        assert!(encoded.contains("required_subject"));
        assert!(encoded.contains("selection.work.id"));
        assert!(!encoded.contains("fixture-pin"));
        assert!(!encoded.contains("fingerprint"));

        let mut reopened = core();
        reopened
            .open_saved_view_set_template(&template, "attachment:test")
            .unwrap();
        let unresolved_instance = center_mounts(&reopened.view_sets[1]).unwrap()[0].clone();
        assert!(matches!(
            reopened.selection_attachments.get(&unresolved_instance),
            Some(super::super::attachment::SelectionAttachment::RequiredSubject {
                input,
                facets,
            }) if input == "subject_tile_0" && facets == &["selection.work.id"]
        ));
        let recaptured = reopened
            .export_active_view_set_template("saved-again".into(), "Saved again".into())
            .unwrap();
        assert_eq!(recaptured.relationships, template.relationships);

        let before_strict = reopened.view_sets.len();
        assert!(
            reopened
                .open_saved_view_set_template_with_relationships(
                    &template,
                    "attachment:test",
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                )
                .unwrap_err()
                .contains("required fresh subject is missing")
        );
        assert_eq!(reopened.view_sets.len(), before_strict);

        let mut inputs = BTreeMap::new();
        inputs.insert(
            "subject_tile_0".into(),
            BTreeMap::from([("selection.work.id".into(), json!("fresh-work"))]),
        );
        reopened
            .open_saved_view_set_template_with_relationships(
                &template,
                "attachment:test",
                &inputs,
                &BTreeMap::new(),
            )
            .unwrap();
        let reopened_instance = center_mounts(&reopened.view_sets[2]).unwrap()[0].clone();
        let Some(super::super::attachment::SelectionAttachment::Pinned { values, .. }) =
            reopened.selection_attachments.get(&reopened_instance)
        else {
            panic!("fresh subject must reopen as an explicit pin")
        };
        assert_eq!(values["selection.work.id"], "fresh-work");
        assert!(target.export_layout_preferences().is_err());
    }

    #[test]
    fn unresolved_dock_save_open_save_preserves_the_exact_requirement() {
        let mut source = core();
        let dock = add_subject_dock(&mut source);
        source.selection_attachments.insert(
            dock,
            super::super::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_slot_bottom".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );
        let template = source
            .export_active_view_set_template("dock-set".into(), "Dock set".into())
            .unwrap();

        let mut reopened = core();
        add_subject_dock(&mut reopened);
        reopened
            .open_saved_view_set_template(&template, "attachment:test")
            .unwrap();
        let reopened_dock = super::super::model::dock_view_instance_key(
            reopened.view_sets[reopened.active_view_set].id,
            super::super::model::RyeOsDockEdge::Bottom,
        );
        assert!(matches!(
            reopened.selection_attachments.get(&reopened_dock),
            Some(super::super::attachment::SelectionAttachment::RequiredSubject {
                input,
                facets,
            }) if input == "subject_slot_bottom" && facets == &["selection.work.thread"]
        ));
        let recaptured = reopened
            .export_active_view_set_template("dock-set-again".into(), "Dock set again".into())
            .unwrap();
        assert_eq!(recaptured.relationships, template.relationships);
    }

    #[test]
    fn restore_does_not_discard_input_in_an_inactive_view_set() {
        let mut target = core();
        let saved = target.export_layout_preferences().unwrap();
        target.view_sets[0].input_buffers.insert(
            "draft".into(),
            RyeOsInputState {
                text: "keep me".into(),
                ..Default::default()
            },
        );
        target.new_view_set();
        let ids: Vec<_> = target
            .view_sets
            .iter()
            .map(|view_set| view_set.id)
            .collect();
        assert!(target.restore_layout_preferences(&saved).is_err());
        assert_eq!(
            target
                .view_sets
                .iter()
                .map(|view_set| view_set.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(target.view_sets[0].input_buffers["draft"].text, "keep me");
    }

    #[test]
    fn reusable_template_opens_fresh_and_never_carries_session_state() {
        let mut source = core();
        source.view_sets[0].input_buffers.insert(
            "draft".into(),
            RyeOsInputState {
                text: "private draft".into(),
                ..Default::default()
            },
        );
        let source_ids = source.view_sets[0].tile_ids();
        let template = source
            .export_active_view_set_template("development".into(), "Development".into())
            .unwrap();
        let encoded = serde_json::to_string(&template).unwrap();
        assert!(!encoded.contains("private draft"));
        assert!(!encoded.contains("session:test"));

        let mut target = core();
        let effects = target
            .open_saved_view_set_template(&template, "attachment:test")
            .unwrap();
        assert_eq!(target.view_sets.len(), 2);
        assert_eq!(target.active_view_set, 1);
        assert!(target.view_sets[1].input_buffers.is_empty());
        assert!(
            target.view_sets[1]
                .tile_ids()
                .iter()
                .all(|id| !source_ids.contains(id))
        );
        assert!(effects.iter().all(|effect| !matches!(
            effect.kind,
            super::super::effect::RyeOsEffectKind::InvokeBinding { .. }
        )));
    }

    #[test]
    fn reusable_cross_set_follow_requires_explicit_saved_set_resolution() {
        let mut source = core();
        let captured_set = source.view_sets[0].id;
        let instance = center_mounts(&source.view_sets[0]).unwrap()[0].clone();
        source.new_view_set();
        let followed_set = source.view_sets[1].id;
        source.selection_attachments.insert(
            instance,
            super::super::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: followed_set,
            },
        );
        source.switch_view_set_tab(0);

        let template = source
            .capture_view_set_template_with_relationships(
                "development".into(),
                "Development".into(),
                None,
                &BTreeMap::from([(followed_set, "support".into())]),
            )
            .unwrap();
        assert!(matches!(
            template.relationships[0].source,
            SavedViewSelectionSource::FollowSet { ref saved_view_set_id }
                if saved_view_set_id == "support"
        ));
        assert_ne!(captured_set, followed_set);

        let mut target = core();
        let target_followed = target.view_sets[0].id;
        let before = target.view_sets.len();
        assert!(
            target
                .open_saved_view_set_template(&template, "attachment:test")
                .unwrap_err()
                .contains("required saved view set is not open")
        );
        assert_eq!(target.view_sets.len(), before);
        target
            .open_saved_view_set_template_with_relationships(
                &template,
                "attachment:test",
                &BTreeMap::new(),
                &BTreeMap::from([("support".into(), target_followed)]),
            )
            .unwrap();
        let reopened_instance = center_mounts(&target.view_sets[1]).unwrap()[0].clone();
        assert_eq!(
            target.selection_attachments.get(&reopened_instance),
            Some(
                &super::super::attachment::SelectionAttachment::FollowViewSet {
                    view_set_id: target_followed,
                }
            )
        );
    }

    #[test]
    fn reusable_template_refuses_an_unresolved_insertion_attachment() {
        let mut target = core();
        let template = target
            .export_active_view_set_template("development".into(), "Development".into())
            .unwrap();
        let set_count = target.view_sets.len();

        assert!(
            target
                .open_saved_view_set_template(&template, "attachment:revoked")
                .is_err()
        );
        assert_eq!(target.view_sets.len(), set_count);
    }

    #[test]
    fn reusable_template_revalidates_views_before_mutating_open_sets() {
        let source = core();
        let mut template = source
            .export_active_view_set_template("development".into(), "Development".into())
            .unwrap();
        template.composition.root = Some(LayoutSeedSpec::Group {
            label: None,
            views: vec![ViewKindSpec("view:unadmitted/private".into())],
            active: 0,
        });
        let mut target = core();
        let before = target.view_sets.len();
        assert!(
            target
                .open_saved_view_set_template(&template, "attachment:test")
                .is_err()
        );
        assert_eq!(target.view_sets.len(), before);
    }

    #[test]
    fn reusable_template_cannot_exceed_the_open_view_set_limit() {
        let mut target = core();
        let template = target
            .export_active_view_set_template("development".into(), "Development".into())
            .unwrap();
        while target.view_sets.len() < crate::surface::view_sets::MAX_VIEW_SETS {
            target.new_view_set();
        }
        let before = target.view_sets.len();

        assert!(
            target
                .open_saved_view_set_template(&template, "attachment:test")
                .is_err()
        );
        assert_eq!(target.view_sets.len(), before);
    }
}
