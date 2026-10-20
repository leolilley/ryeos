//! Untrusted, bounded presentation preferences. Never serialize RyeOsCore here:
//! grants, seat authority, observations, pending effects and drafts are excluded.
//! Restoring allocates fresh mounts and revalidates every ref against this session.

use super::model::RyeOsCore;
use crate::layout::LayoutTree;
use crate::surface::ViewKindSpec;
use crate::surface::view_sets::{
    LayoutSeedSpec, SavedViewMountRef, SavedViewSelectionRelationship, SavedViewSelectionSource,
    SavedViewSetTemplate, VIEW_SET_COMPOSITION_SCHEMA, ViewSetSeedSpec,
    validate_saved_view_set_templates, validate_seeds,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_LAYOUT_PREFERENCE_BYTES: usize = 256 * 1024;
const SCHEMA: &str = "ryeos.ui.layout-preferences.v6";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequiredSubjectMountPolicy {
    AllowUnresolved,
    RequireResolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    principal: String,
    presentation_id: crate::ids::PresentationId,
    surface_instance_id: crate::ids::SurfaceInstanceId,
    surface: String,
    /// Exact compiled surface generation that admitted this arrangement.
    /// A stable surface ref is not enough: authored view sets may
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

pub(crate) fn capture_view_set(
    view_set: &crate::view_set::ViewSet,
    id: String,
) -> Result<ViewSetSeedSpec, String> {
    Ok(ViewSetSeedSpec {
        schema: VIEW_SET_COMPOSITION_SCHEMA,
        id,
        title: view_set.title.clone(),
        root: view_set
            .root
            .as_ref()
            .map(|root| capture_layout(root, view_set))
            .transpose()?,
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
    let mounts = center_mounts(view_set)?
        .into_iter()
        .enumerate()
        .map(|(index, instance)| (SavedViewMountRef::Tile { index }, instance))
        .collect::<Vec<_>>();
    Ok(mounts)
}

impl RyeOsCore {
    pub(crate) fn live_saved_set_resolutions_at(
        &self,
        surface_id: &crate::ids::SurfaceInstanceId,
        required_names: &std::collections::BTreeSet<&str>,
    ) -> Result<BTreeMap<String, crate::ids::ViewSetId>, String> {
        let mut resolved = BTreeMap::new();
        let surface = self
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.surfaces().get(surface_id))
            .ok_or("saved-set resolution requires a live captured Surface")?;
        for (view_set_id, saved_id) in &self.view_set_template_ids {
            if !required_names.contains(saved_id.as_str()) {
                continue;
            }
            if !surface.view_sets().iter().any(|set| set.id == *view_set_id) {
                continue;
            }
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
        let presentation = self
            .presentation
            .as_ref()
            .ok_or("layout preferences require an admitted presentation")?;
        let instance = self
            .active_surface()
            .ok_or("layout preferences require a selected Surface")?;
        let surface = self
            .binding_attachment(&instance.binding().binding_attachment_id)
            .ok_or("layout preferences require the admitted surface attachment")?;
        if surface.binding_generation != instance.binding().binding_generation
            || surface.binding_digest != instance.binding().binding_digest
        {
            return Err("layout preferences require the exact Surface attachment".into());
        }
        if surface.presentation.definition.surface().is_none() {
            return Err(
                "Surface layout preferences cannot capture shared presentation state".into(),
            );
        }
        if surface.root_generation.is_empty() {
            return Err("layout preferences require an exact surface generation".into());
        }
        Ok(Scope {
            principal: principal.clone(),
            presentation_id: presentation.id().clone(),
            surface_instance_id: instance.id().clone(),
            surface: surface.root_ref.clone(),
            surface_generation: surface.root_generation.clone(),
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
        let surface = self
            .active_surface()
            .ok_or("layout preferences require a selected Surface")?;
        let selected = surface
            .active_view_set()
            .ok_or("layout preferences require a selected tab")?
            .id;
        let active_view_set = surface
            .view_sets()
            .iter()
            .position(|set| set.id == selected)
            .ok_or("selected tab is not part of the Surface")?;
        let view_sets = surface
            .view_sets()
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
            active_view_set,
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
        let surface = self
            .active_surface()
            .ok_or("layout export requires a selected Surface")?;
        let sets = surface.view_sets();
        let surface_attachment_id = &surface.binding().binding_attachment_id;
        for (instance, attachment) in &self.selection_attachments {
            if !sets.iter().any(|set| {
                set.tiles
                    .values()
                    .any(|tile| &tile.instance_key == instance)
            }) {
                continue; // Shared dock/dialog state is not part of tab persistence.
            }
            if self
                .mounted_view_ref(instance)
                .and_then(|view_ref| self.binding_for_instance(instance, view_ref))
                .is_some_and(|binding| !super::attachment::participates_in_selection(binding))
            {
                continue; // Inert replay metadata is not a portable relationship.
            }
            let owner = self.view_set_for_instance(instance).map(|set| set.id);
            match attachment {
                super::attachment::SelectionAttachment::FollowViewSet { view_set_id }
                    if owner == Some(*view_set_id) => {}
                _ => return Err(
                    "layout preferences cannot preserve pinned or cross-set subject relationships"
                        .into(),
                ),
            }
        }
        for view_set in sets {
            if self.view_set_insertion_attachments.get(&view_set.id) != Some(surface_attachment_id)
            {
                return Err(
                    "layout preferences cannot represent this view set's insertion context".into(),
                );
            }
            let tile_contexts_match = view_set.tiles.values().all(|tile| {
                self.binding_attachment_for_instance(&tile.instance_key)
                    .is_some_and(|attachment| {
                        attachment.binding_attachment_id == *surface_attachment_id
                            && attachment.binding_generation == surface.binding().binding_generation
                            && attachment.binding_digest == surface.binding().binding_digest
                    })
            });
            if !tile_contexts_match {
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
        let view_set_id = match exclude {
            Some(instance) => self.composition_destination(instance)?.view_set_id,
            None => {
                self.active_view_set()
                    .ok_or("captured view set is unavailable")?
                    .id
            }
        };
        self.capture_view_set_template_at(view_set_id, id, name, exclude, saved_set_ids)
    }

    pub(crate) fn capture_view_set_template_at(
        &self,
        view_set_id: crate::ids::ViewSetId,
        id: String,
        name: String,
        exclude: Option<&crate::ids::RyeOsViewInstanceKey>,
        saved_set_ids: &BTreeMap<crate::ids::ViewSetId, String>,
    ) -> Result<SavedViewSetTemplate, String> {
        let view_set = self
            .view_set(view_set_id)
            .ok_or("captured view set is unavailable")?;
        let mut captured = view_set.clone();
        if let Some(instance) = exclude {
            let management = self
                .presentation
                .as_ref()
                .and_then(|composition| composition.mount(instance))
                .ok_or("composition-management instance is not mounted")?;
            if matches!(
                management,
                super::presentation_state::MountRef::SurfaceTile { .. }
            ) && self.view_set_for_instance(instance).map(|set| set.id) != Some(view_set.id)
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
            }
            // Shared management Views need no exclusion: they never belong
            // to the captured nested tab layout.
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
                .binding_attachment_for_instance(&instance)
                .map(|attachment| attachment.binding_attachment_id.as_str())
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
            let required = template
                .relationships
                .iter()
                .filter_map(|relationship| match &relationship.source {
                    SavedViewSelectionSource::FollowSet { saved_view_set_id } => {
                        Some(saved_view_set_id.as_str())
                    }
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>();
            let surface_id = self
                .active_surface()
                .ok_or("saved-set resolution requires a selected Surface")?
                .id()
                .clone();
            self.live_saved_set_resolutions_at(&surface_id, &required)?
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

    /// Open a reusable composition using the admitted insertion context
    /// already retained for the active view set. Browser/client adapters must
    /// not choose an attachment coordinate supplied by presentation code.
    pub fn open_saved_view_set_template_in_active_context(
        &mut self,
        template: &SavedViewSetTemplate,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        let active_view_set_id = self
            .active_view_set()
            .ok_or("active view set is unavailable")?
            .id;
        let insertion_attachment_id = self
            .insertion_attachment_id(active_view_set_id)
            .ok_or("active view set has no admitted insertion context")?
            .to_owned();
        self.open_saved_view_set_template(template, &insertion_attachment_id)
    }

    /// Prepare a reusable set through the existing retained operation owner.
    /// Supplied subjects and external follows are validated and installed
    /// atomically by the retained composition owner after acknowledgement.
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
        let surface = self
            .active_surface()
            .ok_or("opening a reusable set requires a selected Surface")?;
        let destination = super::presentation_state::PresentationInsertionDestination {
            surface_instance_id: surface.id().clone(),
            view_set_id: self
                .active_view_set()
                .ok_or("active view set is unavailable")?
                .id,
        };
        self.open_saved_view_set_template_with_policy_at(
            template,
            insertion_attachment_id,
            fresh_subjects,
            saved_sets,
            subject_policy,
            &destination,
        )
    }

    pub(crate) fn open_saved_view_set_template_with_policy_at(
        &mut self,
        template: &SavedViewSetTemplate,
        insertion_attachment_id: &str,
        fresh_subjects: &BTreeMap<String, BTreeMap<String, Value>>,
        saved_sets: &BTreeMap<String, crate::ids::ViewSetId>,
        subject_policy: RequiredSubjectMountPolicy,
        destination: &super::presentation_state::PresentationInsertionDestination,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        validate_saved_view_set_templates(std::slice::from_ref(template))?;
        let required_inputs = template
            .relationships
            .iter()
            .filter_map(|relationship| match &relationship.source {
                SavedViewSelectionSource::RequiredSubject { input, .. } => Some(input.as_str()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        let required_sets = template
            .relationships
            .iter()
            .filter_map(|relationship| match &relationship.source {
                SavedViewSelectionSource::FollowSet { saved_view_set_id } => {
                    Some(saved_view_set_id.as_str())
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        if fresh_subjects
            .keys()
            .any(|input| !required_inputs.contains(input.as_str()))
        {
            return Err("fresh subject input is not used by this template".into());
        }
        if saved_sets
            .keys()
            .any(|id| !required_sets.contains(id.as_str()))
        {
            return Err("saved-set resolution is not used by this template".into());
        }
        if subject_policy == RequiredSubjectMountPolicy::RequireResolved {
            for input in required_inputs {
                if !fresh_subjects.contains_key(input) {
                    return Err(format!("required fresh subject is missing: {input}"));
                }
            }
        }
        for id in required_sets {
            if !saved_sets.contains_key(id) {
                return Err(format!("required saved view set is not open: {id}"));
            }
        }
        let presentation = self
            .presentation
            .as_ref()
            .ok_or("presentation is unavailable")?;
        presentation
            .view_set(&destination.surface_instance_id, destination.view_set_id)
            .ok_or("captured insertion tab is no longer open")?;
        if self.insertion_attachment_id(destination.view_set_id) != Some(insertion_attachment_id) {
            return Err(
                "insertion attachment does not belong to the captured destination tab".into(),
            );
        }
        let surface = presentation
            .surfaces()
            .get(&destination.surface_instance_id)
            .ok_or("captured insertion Surface is no longer open")?;
        let owner = crate::ui::composition_operation::CompositionOwner::Surface {
            surface_instance_id: surface.id().clone(),
            binding: surface.binding().clone(),
        };
        let index = surface.view_sets().len();
        if index >= crate::surface::view_sets::MAX_VIEW_SETS {
            return Err("view set limit reached".into());
        }
        let descriptor = self
            .binding_attachment(insertion_attachment_id)
            .ok_or("saved view set requires a live admitted insertion context")?;
        let binding = super::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: descriptor.binding_attachment_id.clone(),
            binding_generation: descriptor.binding_generation,
            binding_digest: descriptor.binding_digest.clone(),
        };
        self.prepare_saved_composition_owned(
            crate::ui::composition_operation::CompositionMutation::InsertTab {
                index,
                tab: crate::ui::composition_operation::CompositionTabSeed {
                    template: template.clone(),
                    binding,
                    focused_view: None,
                    fresh_subjects: fresh_subjects.clone(),
                    followed_sets: saved_sets.clone(),
                },
                activate: true,
            },
            owner,
        )
    }

    /// A successful result means write-ahead PREPARATION only. Publication and
    /// first source fetch wait for the exact preparation/completion ACKs.
    fn prepare_saved_composition(
        &mut self,
        mutation: crate::ui::composition_operation::CompositionMutation,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        let surface = self
            .active_surface()
            .ok_or("saved composition requires a selected Surface")?;
        let owner = crate::ui::composition_operation::CompositionOwner::Surface {
            surface_instance_id: surface.id().clone(),
            binding: surface.binding().clone(),
        };
        self.prepare_saved_composition_owned(mutation, owner)
    }

    fn prepare_saved_composition_owned(
        &mut self,
        mutation: crate::ui::composition_operation::CompositionMutation,
        owner: crate::ui::composition_operation::CompositionOwner,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        let before = self.seat.events().len();
        let effects = self.prepare_composition_owned(mutation, owner);
        if self.seat.events().len() != before + 1
            || !matches!(
                self.seat.events().last().map(|event| &event.kind),
                Some(super::seat::SeatEventKind::PresentationOperationPrepared(_))
            )
        {
            return Err("saved composition was not prepared; inspect the current owner or pending operation".into());
        }
        Ok(effects)
    }

    pub fn restore_layout_preferences(
        &mut self,
        encoded: &str,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        let surface = self
            .active_surface()
            .ok_or("layout restoration requires a selected Surface")?;
        let old_mounts = surface
            .view_sets()
            .iter()
            .flat_map(|set| set.tiles.values())
            .map(|tile| tile.instance_key.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if self.input_buffers.iter().any(|(key, input)| {
            !input.text.is_empty()
                && old_mounts.iter().any(|instance| {
                    super::model::InputBufferKey::storage_key_belongs_to(key, instance)
                })
        }) {
            return Err(
                "layout restoration would discard current input; clear it explicitly first".into(),
            );
        }
        if encoded.len() > MAX_LAYOUT_PREFERENCE_BYTES {
            return Err("layout preferences exceed byte limit".into());
        }
        let snapshot: Preferences = serde_json::from_str(encoded)
            .map_err(|error| format!("invalid layout preferences: {error}"))?;
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
        let descriptor = self
            .binding_attachment(surface.binding().binding_attachment_id.as_str())
            .ok_or("layout restoration requires the admitted Surface attachment")?;
        let binding = super::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: descriptor.binding_attachment_id.clone(),
            binding_generation: descriptor.binding_generation,
            binding_digest: descriptor.binding_digest.clone(),
        };
        let views = self
            .binding_attachments
            .get(&binding.binding_attachment_id)
            .ok_or("layout binding context absent")?;
        let active_seed_id = snapshot.view_sets[snapshot.active_view_set].seed.id.clone();
        let mut tabs = Vec::new();
        // Preferences retain arrangement, not pinned values or execution state.
        // Reused subject-dependent Views reopen explicitly unresolved.
        for saved in snapshot.view_sets {
            fn refs<'a>(node: &'a LayoutSeedSpec, output: &mut Vec<&'a str>) {
                match node {
                    LayoutSeedSpec::Group { views, .. } => {
                        output.extend(views.iter().map(|view| view.0.as_str()))
                    }
                    LayoutSeedSpec::Split { first, second, .. } => {
                        refs(first, output);
                        refs(second, output);
                    }
                }
            }
            let mut ordered = Vec::new();
            if let Some(root) = &saved.seed.root {
                refs(root, &mut ordered);
            }
            let mut relationships = Vec::new();
            for (index, view_ref) in ordered.into_iter().enumerate() {
                let view = views
                    .views
                    .get(view_ref)
                    .ok_or_else(|| format!("saved view is not admitted: {view_ref}"))?;
                let facets = super::attachment::selection_dependencies(view)
                    .into_iter()
                    .collect::<Vec<_>>();
                let source = if facets.is_empty() {
                    SavedViewSelectionSource::FollowOwnSet
                } else {
                    SavedViewSelectionSource::RequiredSubject {
                        input: format!("restored_subject_{index}"),
                        facets,
                    }
                };
                relationships.push(SavedViewSelectionRelationship {
                    mount: SavedViewMountRef::Tile { index },
                    source,
                });
            }
            tabs.push(crate::ui::composition_operation::CompositionTabSeed {
                template: SavedViewSetTemplate {
                    id: saved.seed.id.clone(),
                    name: saved.seed.title.clone(),
                    composition: saved.seed,
                    relationships,
                },
                binding: binding.clone(),
                focused_view: saved.focused_view,
                fresh_subjects: BTreeMap::new(),
                followed_sets: BTreeMap::new(),
            });
        }
        self.prepare_saved_composition(
            crate::ui::composition_operation::CompositionMutation::RestoreTabs {
                tabs,
                active_seed_id,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::model::{InputBufferKey, RyeOsDockEdge, RyeOsInputState};
    use crate::view_set::ViewSet;
    use serde_json::json;

    fn core() -> RyeOsCore {
        use crate::ui::reducer::test_support::*;
        let mut session = session_with_surface_and_docks(
            json!({
                "name":"Work","view_sets":[{"schema":2,"id":"work","title":"Work",
                    "root":{"type":"group","views":["view:test/one","view:test/two"],"active":0}}],"views":{
                    "view:test/one":{"widget":"text","sources":{"default":{"ref":"service:test/one","params":{"subject":"@facet:selection.work.id"}}}},
                    "view:test/two":{"widget":"text"},
                    "view:test/subject-dock":{"widget":"rows","sources":{"default":{"ref":"service:test/subject-dock","params":{"thread":"@facet:selection.work.thread"}}}}
                }
            }),
            json!({"left":{"content":"view:test/one","open":false,"size":24},
            "bottom":{"content":"view:test/subject-dock","open":false,"size":9}}),
        );
        session.session_id = "session:test".into();
        session.user_principal_id = Some("fp:operator".into());
        session.binding_attachments[0].binding_attachment_id = "attachment:test".into();
        session.presentations[0].surfaces[0]
            .binding
            .binding_attachment_id = "attachment:test".into();
        for tab in &mut session.presentations[0].surfaces[0].mount_manifest.tabs {
            tab.binding.binding_attachment_id = "attachment:test".into();
        }
        core_for_presentation(
            session,
            FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        )
    }

    fn set(core: &RyeOsCore, index: usize) -> &ViewSet {
        &core.active_surface().unwrap().view_sets()[index]
    }

    fn draft_key(core: &RyeOsCore, index: usize) -> String {
        let set = set(core, index);
        let tile = set.tiles.values().next().unwrap();
        InputBufferKey::new(
            tile.instance_key.clone(),
            tile.view.view_ref.clone(),
            "line",
            core.binding_attachment_for_instance(&tile.instance_key)
                .unwrap(),
        )
        .storage_key()
    }

    fn add_subject_dock(core: &mut RyeOsCore) -> crate::ids::RyeOsViewInstanceKey {
        core.presentation
            .as_mut()
            .unwrap()
            .dock_mut("bottom")
            .unwrap()
            .visible = true;
        core.shared_dock_instance_key(RyeOsDockEdge::Bottom)
            .unwrap()
    }

    #[test]
    fn arrangement_round_trip_allocates_fresh_ids_and_never_saves_drafts() {
        use crate::ui::reducer::test_support::{
            claim_fixture_producer, pending_prefix, settle_pending_prefix,
        };
        let mut source = core();
        let producer = claim_fixture_producer(&mut source);
        settle_pending_prefix(&mut source, &producer);
        let draft = draft_key(&source, 0);
        source.input_buffers.insert(
            draft,
            RyeOsInputState {
                text: "private-unsent-draft".into(),
                ..Default::default()
            },
        );
        let ids = set(&source, 0).tile_ids();
        fn group_for(
            root: &crate::layout::LayoutTree,
            tile: crate::ids::TileId,
        ) -> Option<crate::ids::ViewGroupId> {
            match root {
                crate::layout::LayoutTree::Group { group_id, tabs, .. } => {
                    tabs.contains(&tile).then_some(*group_id)
                }
                crate::layout::LayoutTree::Split { first, second, .. } => {
                    group_for(first, tile).or_else(|| group_for(second, tile))
                }
            }
        }
        let tab = set(&source, 0).id;
        let surface = source.active_surface().unwrap().id().clone();
        let group = group_for(set(&source, 0).root.as_ref().unwrap(), ids[0]).unwrap();
        source.prepare_composition_mutation(
            super::super::composition_operation::CompositionMutation::MoveView {
                source: super::super::composition_operation::CompositionTileTarget {
                    surface_instance_id: surface.clone(),
                    view_set_id: tab,
                    tile_id: ids[1],
                },
                destination: super::super::composition_operation::CompositionTileTarget {
                    surface_instance_id: surface,
                    view_set_id: tab,
                    tile_id: ids[0],
                },
                placement: super::super::composition_operation::CompositionPlacement::GroupTab {
                    group_id: group,
                    before: None,
                },
            },
        );
        settle_pending_prefix(&mut source, &producer);
        let default_mount = set(&source, 0).tiles[&ids[0]].instance_key.clone();
        source.prepare_composition_mutation(
            super::super::composition_operation::CompositionMutation::FocusView {
                mount: default_mount,
            },
        );
        settle_pending_prefix(&mut source, &producer);
        let encoded = source.export_layout_preferences().unwrap();
        assert!(!encoded.contains("private-unsent-draft"));
        assert!(!encoded.contains("binding_digest"));
        let mut target = core();
        let target_producer = claim_fixture_producer(&mut target);
        settle_pending_prefix(&mut target, &target_producer);
        let seat_before = target.seat.fold().snapshot();
        let live_before = set(&target, 0).root.clone();
        assert!(
            target
                .restore_layout_preferences(&encoded)
                .unwrap()
                .is_empty()
        );
        assert_eq!(set(&target, 0).root, live_before);
        target.acknowledge_seat_prefix(&target_producer, pending_prefix(&target));
        assert_eq!(
            set(&target, 0).root,
            live_before,
            "first ACK cannot install restored layout"
        );
        let effects = settle_pending_prefix(&mut target, &target_producer);
        assert!(effects.iter().all(|effect| !matches!(
            effect.kind,
            super::super::effect::RyeOsEffectKind::InvokeBinding { .. }
        )));
        assert_eq!(target.seat.fold().snapshot(), seat_before);
        assert_eq!(
            set(&target, 0)
                .root
                .as_ref()
                .unwrap()
                .active_tile_ids()
                .len(),
            1
        );
        assert!(target.input_buffers.is_empty());
        assert!(
            set(&target, 0)
                .tile_ids()
                .iter()
                .all(|id| !ids.contains(id))
        );
    }

    #[test]
    fn management_capture_omits_only_its_mount_without_changing_live_layout() {
        let source = core();
        let original = source.export_layout_preferences().unwrap();
        let ids = set(&source, 0).tile_ids();
        let instance = &set(&source, 0).tiles[&ids[0]].instance_key;
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
        let original = set(&target, 0).root.clone();
        let mut saved: serde_json::Value =
            serde_json::from_str(&target.export_layout_preferences().unwrap()).unwrap();
        saved["scope"]["principal"] = json!("fp:someone-else");
        assert!(
            target
                .restore_layout_preferences(&saved.to_string())
                .is_err()
        );
        assert_eq!(set(&target, 0).root, original);
        let mut saved: serde_json::Value =
            serde_json::from_str(&target.export_layout_preferences().unwrap()).unwrap();
        saved["view_sets"][0]["seed"]["slots"] =
            json!({"left": {"content": "view:unadmitted", "open": false, "size": 20}});
        assert!(
            target
                .restore_layout_preferences(&saved.to_string())
                .is_err()
        );
        assert_eq!(set(&target, 0).root, original);
        assert!(
            target
                .restore_layout_preferences(&" ".repeat(MAX_LAYOUT_PREFERENCE_BYTES + 1))
                .is_err()
        );
    }

    #[test]
    fn management_capture_checks_shared_mount_and_preserves_tab_instances() {
        let mut source = core();
        let presentation = source.presentation.as_ref().unwrap().id();
        let missing = crate::ids::RyeOsViewInstanceKey::shared_dock(presentation, "right").unwrap();
        assert!(
            source
                .capture_view_set_template("saved".into(), "Saved".into(), Some(&missing))
                .is_err()
        );
        let instance = source
            .shared_dock_instance_key(RyeOsDockEdge::Left)
            .unwrap();
        assert!(
            source
                .capture_view_set_template("saved".into(), "Saved".into(), Some(&instance))
                .is_err(),
            "a global dock does not inherit the active tab as an insertion target"
        );
        source.open_dialog("launcher");
        let instance = source.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .instance_key()
            .clone();
        let original = source.export_layout_preferences().unwrap();
        let template = source
            .capture_view_set_template("saved".into(), "Saved".into(), Some(&instance))
            .unwrap();
        assert!(
            serde_json::to_value(&template.composition)
                .unwrap()
                .get("slots")
                .is_none()
        );
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
            .root_generation = "surface-generation-two".into();

        assert_ne!(source_key, successor.layout_preference_key().unwrap());
        assert!(successor.restore_layout_preferences(&saved).is_err());
    }

    #[test]
    fn unrelated_binding_generation_does_not_retire_layout() {
        let source = core();
        let source_key = source.layout_preference_key().unwrap();
        let mut successor = core();
        let mut unrelated = successor.binding_attachments["attachment:test"].clone();
        unrelated.descriptor.binding_attachment_id = "unrelated".into();
        unrelated.descriptor.binding_digest = "unrelated-generation-two".into();
        successor
            .binding_attachments
            .insert("unrelated".into(), unrelated);
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
            .root_generation
            .clear();

        assert!(target.layout_preference_key().is_err());
        assert!(target.export_layout_preferences().is_err());
    }

    #[test]
    fn mixed_attachment_mounts_cannot_be_exported_as_surface_preferences() {
        let mut target = core();
        let instance = set(&target, 0)
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let mut unrelated = target.binding_attachments["attachment:test"].clone();
        unrelated.descriptor.binding_attachment_id = "attachment:project".into();
        unrelated.descriptor.binding_digest = "project-binding".into();
        target
            .binding_attachments
            .insert("attachment:project".into(), unrelated);
        target.stamp_instance_binding(instance, "attachment:project");
        assert!(target.export_layout_preferences().is_err());
    }

    #[test]
    fn reusable_template_opens_unresolved_without_serializing_a_pin() {
        use crate::ui::reducer::test_support::{claim_fixture_producer, settle_pending_prefix};
        let mut target = core();
        let tile = set(&target, 0).root.as_ref().unwrap().tile_ids()[0];
        let instance = set(&target, 0).tiles[&tile].instance_key.clone();
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
        let producer = claim_fixture_producer(&mut reopened);
        settle_pending_prefix(&mut reopened, &producer);
        let before = reopened.view_sets().count();
        reopened
            .open_saved_view_set_template_in_active_context(&template)
            .unwrap();
        assert_eq!(
            reopened.view_sets().count(),
            before,
            "preparation cannot mount a tab"
        );
        settle_pending_prefix(&mut reopened, &producer);
        let unresolved_instance = center_mounts(set(&reopened, 1)).unwrap()[0].clone();
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

        let before_strict = reopened.view_sets().count();
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
        assert_eq!(reopened.view_sets().count(), before_strict);

        let mut inputs = BTreeMap::new();
        inputs.insert(
            "subject_tile_0".into(),
            BTreeMap::from([("selection.work.id".into(), json!("fresh-work"))]),
        );
        let mut unused = inputs.clone();
        unused.insert("unused_input".into(), BTreeMap::new());
        let seat_before = reopened.seat.events().len();
        assert!(
            reopened
                .open_saved_view_set_template_with_relationships(
                    &template,
                    "attachment:test",
                    &unused,
                    &BTreeMap::new(),
                )
                .unwrap_err()
                .contains("not used")
        );
        assert_eq!(reopened.seat.events().len(), seat_before);
        reopened
            .open_saved_view_set_template_with_relationships(
                &template,
                "attachment:test",
                &inputs,
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(reopened.view_sets().count(), before_strict);
        settle_pending_prefix(&mut reopened, &producer);
        let reopened_instance = center_mounts(set(&reopened, 2)).unwrap()[0].clone();
        let Some(super::super::attachment::SelectionAttachment::Pinned { values, .. }) =
            reopened.selection_attachments.get(&reopened_instance)
        else {
            panic!("fresh subject must reopen as an explicit pin")
        };
        assert_eq!(values["selection.work.id"], "fresh-work");
        assert!(target.export_layout_preferences().is_err());
    }

    #[test]
    fn shared_dock_subjects_never_enter_saved_tab_composition() {
        use crate::ui::reducer::test_support::{claim_fixture_producer, settle_pending_prefix};
        let mut source = core();
        let dock = add_subject_dock(&mut source);
        source.selection_attachments.insert(
            dock.clone(),
            super::super::attachment::SelectionAttachment::RequiredSubject {
                input: "shared_subject".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );
        let template = source
            .export_active_view_set_template("saved".into(), "Saved".into())
            .unwrap();
        let encoded = serde_json::to_value(&template).unwrap();
        assert!(encoded["composition"].get("slots").is_none());
        assert!(!encoded.to_string().contains("shared_subject"));
        let mut reopened = core();
        let producer = claim_fixture_producer(&mut reopened);
        settle_pending_prefix(&mut reopened, &producer);
        reopened
            .open_saved_view_set_template(&template, "attachment:test")
            .unwrap();
        settle_pending_prefix(&mut reopened, &producer);
        assert!(!reopened.presentation.as_ref().unwrap().docks()["bottom"].visible);
        assert!(source.selection_attachments.contains_key(&dock));
    }

    #[test]
    fn restore_does_not_discard_input_in_an_inactive_view_set() {
        use crate::ui::reducer::test_support::{claim_fixture_producer, settle_pending_prefix};
        let mut target = core();
        let producer = claim_fixture_producer(&mut target);
        settle_pending_prefix(&mut target, &producer);
        let saved = target.export_layout_preferences().unwrap();
        let draft = draft_key(&target, 0);
        target.input_buffers.insert(
            draft,
            RyeOsInputState {
                text: "keep me".into(),
                ..Default::default()
            },
        );
        target.new_view_set();
        settle_pending_prefix(&mut target, &producer);
        let ids: Vec<_> = target.view_sets().map(|view_set| view_set.id).collect();
        assert!(target.restore_layout_preferences(&saved).is_err());
        assert_eq!(
            target
                .view_sets()
                .map(|view_set| view_set.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(target.input_buffers[&draft_key(&target, 0)].text, "keep me");
    }

    #[test]
    fn reusable_template_opens_fresh_and_never_carries_session_state() {
        use crate::ui::reducer::test_support::{
            claim_fixture_producer, pending_prefix, settle_pending_prefix,
        };
        let mut source = core();
        let draft = draft_key(&source, 0);
        source.input_buffers.insert(
            draft,
            RyeOsInputState {
                text: "private draft".into(),
                ..Default::default()
            },
        );
        let source_ids = set(&source, 0).tile_ids();
        let template = source
            .export_active_view_set_template("development".into(), "Development".into())
            .unwrap();
        let encoded = serde_json::to_string(&template).unwrap();
        assert!(!encoded.contains("private draft"));
        assert!(!encoded.contains("session:test"));

        let mut target = core();
        let producer = claim_fixture_producer(&mut target);
        settle_pending_prefix(&mut target, &producer);
        let preparation = target
            .open_saved_view_set_template(&template, "attachment:test")
            .unwrap();
        assert!(preparation.is_empty());
        assert_eq!(
            target.view_sets().count(),
            1,
            "template preparation is not publication"
        );
        target.acknowledge_seat_prefix(&producer, pending_prefix(&target));
        assert_eq!(
            target.view_sets().count(),
            1,
            "prepared template ACK only retains its completion"
        );
        let effects = settle_pending_prefix(&mut target, &producer);
        assert_eq!(target.view_sets().count(), 2);
        assert_eq!(target.active_view_set().unwrap().id, set(&target, 1).id);
        assert!(!target.input_buffers.keys().any(|key| {
            set(&target, 1)
                .tiles
                .values()
                .any(|tile| InputBufferKey::storage_key_belongs_to(key, &tile.instance_key))
        }));
        assert!(
            set(&target, 1)
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
        use crate::ui::reducer::test_support::{claim_fixture_producer, settle_pending_prefix};
        let mut source = core();
        let producer = claim_fixture_producer(&mut source);
        settle_pending_prefix(&mut source, &producer);
        let captured_set = set(&source, 0).id;
        let instance = center_mounts(set(&source, 0)).unwrap()[0].clone();
        source.new_view_set();
        settle_pending_prefix(&mut source, &producer);
        let followed_set = set(&source, 1).id;
        source.switch_view_set_tab(0);
        settle_pending_prefix(&mut source, &producer);
        source.prepare_subject_operation(
            instance,
            super::super::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: followed_set,
            },
        );
        settle_pending_prefix(&mut source, &producer);

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
        let target_producer = claim_fixture_producer(&mut target);
        settle_pending_prefix(&mut target, &target_producer);
        let target_followed = set(&target, 0).id;
        let before = target.view_sets().count();
        assert!(
            target
                .open_saved_view_set_template(&template, "attachment:test")
                .unwrap_err()
                .contains("required saved view set is not open")
        );
        assert_eq!(target.view_sets().count(), before);
        assert!(
            target
                .open_saved_view_set_template_with_relationships(
                    &template,
                    "attachment:test",
                    &BTreeMap::new(),
                    &BTreeMap::from([
                        ("support".into(), target_followed),
                        ("unused_set".into(), target_followed)
                    ]),
                )
                .unwrap_err()
                .contains("not used")
        );
        target
            .open_saved_view_set_template_with_relationships(
                &template,
                "attachment:test",
                &BTreeMap::new(),
                &BTreeMap::from([("support".into(), target_followed)]),
            )
            .unwrap();
        assert_eq!(target.view_sets().count(), before);
        settle_pending_prefix(&mut target, &target_producer);
        let reopened_instance = center_mounts(set(&target, 1)).unwrap()[0].clone();
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
    fn reusable_template_refuses_a_mismatched_captured_insertion_binding() {
        let mut target = core();
        let template = target
            .export_active_view_set_template("saved".into(), "Saved".into())
            .unwrap();
        let destination = super::super::presentation_state::PresentationInsertionDestination {
            surface_instance_id: target.active_surface().unwrap().id().clone(),
            view_set_id: target.active_view_set().unwrap().id,
        };
        // The shared root is admitted, but is not this tab's insertion context.
        let other_attachment = target
            .data
            .session
            .as_ref()
            .unwrap()
            .root_attachment_id
            .clone();
        assert_ne!(
            target.insertion_attachment_id(destination.view_set_id),
            Some(other_attachment.as_str())
        );
        let before = target.seat.events().len();
        let result = target.open_saved_view_set_template_with_policy_at(
            &template,
            &other_attachment,
            &BTreeMap::new(),
            &BTreeMap::new(),
            RequiredSubjectMountPolicy::AllowUnresolved,
            &destination,
        );
        assert!(result.unwrap_err().contains("captured destination tab"));
        assert_eq!(target.seat.events().len(), before);
    }

    #[test]
    fn reusable_template_refuses_a_missing_captured_destination_without_preparing() {
        let mut target = core();
        let template = target
            .export_active_view_set_template("saved".into(), "Saved".into())
            .unwrap();
        let destination = super::super::presentation_state::PresentationInsertionDestination {
            surface_instance_id: target.active_surface().unwrap().id().clone(),
            view_set_id: crate::ids::ViewSetId::new(u64::MAX),
        };
        let before = target.seat.events().len();
        let result = target.open_saved_view_set_template_with_policy_at(
            &template,
            "attachment:test",
            &BTreeMap::new(),
            &BTreeMap::new(),
            RequiredSubjectMountPolicy::AllowUnresolved,
            &destination,
        );
        assert!(result.unwrap_err().contains("captured insertion tab"));
        assert_eq!(target.seat.events().len(), before);
    }

    #[test]
    fn reusable_template_refuses_an_unresolved_insertion_attachment() {
        let mut target = core();
        let template = target
            .export_active_view_set_template("development".into(), "Development".into())
            .unwrap();
        let set_count = target.view_sets().count();

        assert!(
            target
                .open_saved_view_set_template(&template, "attachment:revoked")
                .is_err()
        );
        assert_eq!(target.view_sets().count(), set_count);
    }

    #[test]
    fn reusable_template_revalidates_views_before_mutating_open_sets() {
        use crate::ui::reducer::test_support::{claim_fixture_producer, settle_pending_prefix};
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
        let producer = claim_fixture_producer(&mut target);
        settle_pending_prefix(&mut target, &producer);
        let before = target.view_sets().count();
        assert!(
            target
                .open_saved_view_set_template(&template, "attachment:test")
                .is_err()
        );
        assert_eq!(target.view_sets().count(), before);
    }

    #[test]
    fn reusable_template_cannot_exceed_the_open_view_set_limit() {
        use crate::ui::reducer::test_support::{claim_fixture_producer, settle_pending_prefix};
        let mut target = core();
        let producer = claim_fixture_producer(&mut target);
        settle_pending_prefix(&mut target, &producer);
        let template = target
            .export_active_view_set_template("development".into(), "Development".into())
            .unwrap();
        while target.view_sets().count() < crate::surface::view_sets::MAX_VIEW_SETS {
            let before = target.view_sets().count();
            target.new_view_set();
            assert_eq!(target.view_sets().count(), before);
            settle_pending_prefix(&mut target, &producer);
            assert_eq!(target.view_sets().count(), before + 1);
        }
        let before = target.view_sets().count();

        assert!(
            target
                .open_saved_view_set_template(&template, "attachment:test")
                .is_err()
        );
        assert_eq!(target.view_sets().count(), before);
    }
}
