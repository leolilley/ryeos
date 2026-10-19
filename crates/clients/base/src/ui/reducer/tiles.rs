use super::effect::RyeOsEffect;
use super::event::RyeOsStackMoveDirection;
use super::model::RyeOsCore;
use super::view_model::RyeOsMotionEventVm;
use super::wrap_index;
use crate::ids::TileId;
use crate::view_set::{ViewLocalState, ViewSpec};

/// One launcher group: a surface-declared (or ref-path-derived) title and
/// its lensable view refs, in declared order.
pub(crate) struct LibraryGroup {
    pub title: String,
    pub refs: Vec<String>,
}

/// The mechanical group for a view no surface group lists: the ref's path
/// segments between the namespace and the leaf (`view:ryeos/node/events`
/// → `node`), or `views` for refs too short to carry one.
fn derived_group_title(view_ref: &str) -> String {
    let path = view_ref.strip_prefix("view:").unwrap_or(view_ref);
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() > 2 {
        segments[1..segments.len() - 1].join("/")
    } else {
        "views".to_string()
    }
}

impl RyeOsCore {
    /// Presentation concurrency fence, not execution authority. Derive it from
    /// the canonical tree instead of maintaining a second mutable revision.
    pub fn layout_guard(&self) -> String {
        self.active_view_set()
            .and_then(|set| set.root.as_ref())
            .and_then(|root| crate::ui::composition_delta::composition_split_guard(root).ok())
            .unwrap_or_default()
    }

    pub(crate) fn new_view_set(&mut self) -> Vec<RyeOsEffect> {
        let Some(surface) = self.active_surface() else {
            return Vec::new();
        };
        let count = surface.view_sets().len();
        let Some(binding) = self
            .binding_attachment(surface.binding().binding_attachment_id.as_str())
            .map(|_| surface.binding().clone())
        else {
            return Vec::new();
        };
        let template = crate::surface::view_sets::SavedViewSetTemplate {
            id: format!("new_{count}"),
            name: format!("View set {}", count + 1),
            composition: crate::surface::view_sets::ViewSetSeedSpec {
                schema: crate::surface::view_sets::VIEW_SET_COMPOSITION_SCHEMA,
                id: format!("new_{count}"),
                title: format!("View set {}", count + 1),
                root: None,
            },
            relationships: Vec::new(),
        };
        self.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::InsertTab {
                index: count,
                tab: crate::ui::composition_operation::CompositionTabSeed {
                    template,
                    binding,
                    focused_view: None,
                    fresh_subjects: std::collections::BTreeMap::new(),
                    followed_sets: std::collections::BTreeMap::new(),
                },
                activate: true,
            },
        )
    }

    pub(crate) fn rename_view_set(&mut self, id: crate::ids::ViewSetId, title: &str) {
        let _ = self.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::RenameTab {
                tab_id: id,
                title: title.trim().into(),
            },
        );
    }

    /// Duplicate presentation composition without duplicating mounted
    /// identity, transient observations or drafts. The new set follows the
    /// same authored views and arrangement, but every mounted view receives a
    /// fresh instance coordinate and therefore an independent source, input
    /// and local-state lifetime.
    pub(crate) fn duplicate_view_set(&mut self, id: crate::ids::ViewSetId) -> Vec<RyeOsEffect> {
        let attempt = (|| -> Result<_, String> {
            let surface = self.active_surface().ok_or("no selected Surface")?;
            let index = surface
                .view_sets()
                .iter()
                .position(|set| set.id == id)
                .ok_or("copied tab is not in the selected Surface")?;
            let set = &surface.view_sets()[index];
            let focused_view = set
                .tile_ids()
                .iter()
                .position(|tile| *tile == set.focused_tile);
            let title = format!("{} copy", set.title);
            let mut template = self.capture_view_set_template_at(
                id,
                format!("copy_{}", id.0),
                title,
                None,
                &std::collections::BTreeMap::new(),
            )?;
            // The saved record name and its authored tab title are distinct.
            // An explicit duplicate uses the copy label for the new tab too.
            template.composition.title = template.name.clone();
            // The portable exporter turns captured pins into required fresh
            // subjects, and refuses cross-set links without explicit resolution.
            // Copy does not carry drafts, observations or a live subject grant.
            let insertion = self
                .insertion_attachment_id(id)
                .ok_or("copied tab has no admitted insertion context")?;
            let descriptor = self
                .binding_attachment(insertion)
                .ok_or("copied tab descriptor is unavailable")?;
            let binding = crate::ui::binding::UiBindingAttachmentCoordinate {
                binding_attachment_id: descriptor.binding_attachment_id.clone(),
                binding_generation: descriptor.binding_generation,
                binding_digest: descriptor.binding_digest.clone(),
            };
            Ok(
                crate::ui::composition_operation::CompositionMutation::InsertTab {
                    index: index + 1,
                    tab: crate::ui::composition_operation::CompositionTabSeed {
                        template,
                        binding,
                        focused_view,
                        fresh_subjects: std::collections::BTreeMap::new(),
                        followed_sets: std::collections::BTreeMap::new(),
                    },
                    activate: true,
                },
            )
        })();
        match attempt {
            Ok(mutation) => self.prepare_composition_mutation(mutation),
            Err(reason) => {
                self.notice(
                    format!("Cannot copy view set: {reason}"),
                    super::view_model::RyeOsTone::Warn,
                );
                Vec::new()
            }
        }
    }

    /// Closing one tab never closes shared mounts or terminates execution.
    pub(crate) fn close_view_set(&mut self, id: crate::ids::ViewSetId) -> Vec<RyeOsEffect> {
        let Some(set) = self.view_set(id) else {
            return Vec::new();
        };
        if self.input_buffers.iter().any(|(key, input)| {
            !input.text.is_empty()
                && set.tiles.values().any(|tile| {
                    super::model::InputBufferKey::storage_key_belongs_to(key, &tile.instance_key)
                })
        }) {
            self.notice(
                "Clear current input explicitly before closing this tab.",
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        }
        if self.selection_attachments.iter().any(|(instance, attachment)| {
            matches!(attachment, crate::ui::attachment::SelectionAttachment::FollowViewSet { view_set_id } if *view_set_id == id)
                && self.view_set_for_instance(instance).is_some_and(|owner| owner.id != id)
                && self
                    .mounted_view_ref(instance)
                    .and_then(|view_ref| self.binding_for_instance(instance, view_ref))
                    .is_none_or(crate::ui::attachment::participates_in_selection)
        }) {
            self.notice(
                "Resolve incoming tab relationships before closing.",
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        }
        self.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::CloseTab { tab_id: id },
        )
    }

    pub(crate) fn cycle_view_set_tab(
        &mut self,
        direction: RyeOsStackMoveDirection,
    ) -> Vec<RyeOsEffect> {
        let delta = match direction {
            RyeOsStackMoveDirection::Up => -1,
            RyeOsStackMoveDirection::Down => 1,
        };
        // Single-lens has no view-set tabs to page — "cycle" swaps the one
        // center lens through the surface library instead.
        let Some(active) = self.active_view_set() else {
            return Vec::new();
        };
        if active.tiling.mode == crate::surface::TilingModeSpec::SingleLens {
            return self.cycle_lens(delta);
        }
        let Some(sets) = self.active_surface_view_sets() else {
            return Vec::new();
        };
        let Some(index) = sets.iter().position(|set| set.id == active.id) else {
            return Vec::new();
        };
        let next = wrap_index(index, delta, sets.len());
        self.switch_view_set_tab(next)
    }

    /// Whether a view works as a center lens: a real bound view that is
    /// neither a scene backdrop nor a pure input line. An `input` block
    /// alone does not disqualify — the thread history views carry live
    /// FILTER inputs and are the canonical center lenses; only a view
    /// with no content of its own (no source, no sections — the foot
    /// chat line) is input-only.
    fn lensable(&self, view_ref: &str) -> bool {
        self.active_view_set()
            .and_then(|set| self.binding_for_insertion(set.id, view_ref))
            .is_some_and(|binding| {
                let input_only = binding.input.is_some()
                    && binding.sources.is_empty()
                    && binding.sections.is_empty();
                binding.widget != "scene" && !input_only
            })
    }

    /// The surface's declared library groups, filtered to lensable refs.
    /// Grouped entries — `{ group, views: [ref…] }` — are the canonical
    /// shape; a bare `view:` ref string (the legacy flat form) is shelved
    /// under its path-derived group in declared order. One key serving
    /// two consumers: `lens_library` cycles the flattened declared
    /// order, `library_groups` hands the launcher the tree.
    fn library_groups_declared(&self) -> Vec<LibraryGroup> {
        let Some(entries) = self
            .active_view_set()
            .and_then(|set| self.insertion_attachment_id(set.id))
            .and_then(|id| self.binding_attachment(id))
            .and_then(|attachment| attachment.presentation.surface_value().ok().flatten())
            .and_then(|surface| surface.get("library").cloned())
            .and_then(|library| library.as_array().cloned())
        else {
            return Vec::new();
        };
        let mut groups: Vec<LibraryGroup> = Vec::new();
        let add = |groups: &mut Vec<LibraryGroup>, title: String, refs: Vec<String>| match groups
            .iter_mut()
            .find(|group| group.title.eq_ignore_ascii_case(&title))
        {
            Some(existing) => existing.refs.extend(refs),
            None => groups.push(LibraryGroup { title, refs }),
        };
        for entry in &entries {
            if let Some(view_ref) = entry.as_str() {
                if self.lensable(view_ref) {
                    add(
                        &mut groups,
                        derived_group_title(view_ref),
                        vec![view_ref.to_string()],
                    );
                }
                continue;
            }
            let Some(title) = entry
                .get("group")
                .and_then(|value| value.as_str())
                .map(|title| title.trim().to_string())
                .filter(|title| !title.is_empty())
            else {
                continue;
            };
            let refs = entry
                .get("views")
                .and_then(|value| value.as_array())
                .map(|views| {
                    views
                        .iter()
                        .filter_map(|value| value.as_str())
                        .filter(|view_ref| self.lensable(view_ref))
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            add(&mut groups, title, refs);
        }
        groups
    }

    /// The launcher's full tree: the declared groups plus every OTHER
    /// lensable view in the surface, grouped by its ref's path segments.
    /// The append is a completeness invariant, not curation — a view that
    /// exists is always reachable, whether or not the surface author
    /// listed it.
    pub(crate) fn library_groups(&self) -> Vec<LibraryGroup> {
        let mut groups = self.library_groups_declared();
        let declared: std::collections::BTreeSet<&str> = groups
            .iter()
            .flat_map(|group| group.refs.iter().map(String::as_str))
            .collect();
        let mut derived: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        let Some(attachment_id) = self
            .active_view_set()
            .and_then(|set| self.insertion_attachment_id(set.id))
        else {
            return groups;
        };
        let Some(attachment) = self.binding_attachments.get(attachment_id) else {
            return groups;
        };
        for view_ref in attachment.views.keys() {
            if declared.contains(view_ref.as_str()) || !self.lensable(view_ref) {
                continue;
            }
            derived
                .entry(derived_group_title(view_ref))
                .or_default()
                .push(view_ref.clone());
        }
        for (title, refs) in derived {
            // A derived group that matches a declared one (path segment
            // vs authored title, case-insensitively) appends to it —
            // "node" must not sit beside "Node" as a second header.
            if let Some(existing) = groups
                .iter_mut()
                .find(|group| group.title.eq_ignore_ascii_case(&title))
            {
                existing.refs.extend(refs);
            } else {
                groups.push(LibraryGroup { title, refs });
            }
        }
        groups
    }

    /// The flattened declared library, in group order. Single-lens
    /// `Ctrl+←/→` cycles this list.
    pub(crate) fn lens_library(&self) -> Vec<String> {
        self.library_groups_declared()
            .into_iter()
            .flat_map(|group| group.refs)
            .collect()
    }

    pub(crate) fn cycle_lens(&mut self, delta: i32) -> Vec<RyeOsEffect> {
        // Cycling only lands on views whose facet params the seat fold can
        // satisfy — the same gate the launcher shows as a disabled row.
        let Some(attachment) = self
            .active_view_set()
            .and_then(|set| self.insertion_attachment_id(set.id))
            .and_then(|id| self.binding_attachments.get(id))
        else {
            return Vec::new();
        };
        let lenses: Vec<String> = self
            .lens_library()
            .into_iter()
            .filter(|lens| {
                attachment.views.get(lens).is_none_or(|binding| {
                    super::view_model::unsatisfied_facets(self, binding).is_empty()
                })
            })
            .collect();
        if lenses.is_empty() {
            return Vec::new();
        }
        let current = self
            .active_view_set()
            .and_then(|set| set.focused_view())
            .map(|view| view.view_ref.clone());
        let index = current
            .as_ref()
            .and_then(|cur| lenses.iter().position(|lens| lens == cur))
            .unwrap_or(0);
        let next = wrap_index(index, delta, lenses.len());
        self.open_view(ViewSpec {
            view_ref: lenses[next].clone(),
        })
    }

    pub(crate) fn switch_view_set_tab(&mut self, index: usize) -> Vec<RyeOsEffect> {
        let Some(id) = self
            .active_surface()
            .and_then(|surface| surface.view_sets().get(index))
            .map(|set| set.id)
        else {
            return Vec::new();
        };
        self.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::SelectTab { tab_id: id },
        )
    }

    pub(crate) fn refresh_view_set_sources_excluding_subjects(
        &mut self,
        refreshed_subjects: &std::collections::BTreeSet<crate::ids::RyeOsViewInstanceKey>,
    ) -> Vec<RyeOsEffect> {
        let Some(active) = self.active_view_set() else {
            return Vec::new();
        };
        let focused = active.focused_tile;
        let id = active.id;
        let index = self
            .active_surface_view_sets()
            .and_then(|sets| sets.iter().position(|set| set.id == id))
            .unwrap_or(0);
        // Sources are mount-addressed. Switching a Surface-local tab must not
        // erase shared observations, independent tabs, drafts or pending work.
        self.push_motion(RyeOsMotionEventVm::FocusChanged {
            tile_id: focused.0.to_string(),
        });
        self.push_motion(RyeOsMotionEventVm::TabChanged {
            view_set_number: index + 1,
        });
        self.bump_generation();
        self.initial_effects_excluding_subject_refresh(refreshed_subjects)
    }

    pub(crate) fn set_tile_cursor(&mut self, tile_id: TileId, index: usize) -> bool {
        let Some(tile) = self
            .active_view_set_mut()
            .and_then(|set| set.tiles.get_mut(&tile_id))
        else {
            return false;
        };
        match &mut tile.local {
            ViewLocalState::GenericList { cursor, .. } => {
                if *cursor == index {
                    return false;
                }
                *cursor = index;
                true
            }
            ViewLocalState::None | ViewLocalState::Field(_) => false,
        }
    }

    pub(crate) fn set_view_cursor(
        &mut self,
        instance_key: &crate::ids::RyeOsViewInstanceKey,
        index: usize,
    ) -> bool {
        if !self
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.mount(instance_key))
            .is_some_and(|mount| mount.visible())
        {
            return false;
        }
        let Some(local) = self.mounted_local_state_mut(instance_key) else {
            return false;
        };
        match local {
            ViewLocalState::GenericList { cursor, .. } if *cursor != index => {
                *cursor = index;
                true
            }
            ViewLocalState::GenericList { .. }
            | ViewLocalState::None
            | ViewLocalState::Field(_) => false,
        }
    }

    pub(crate) fn set_view_row_expanded_key(
        &mut self,
        instance_key: &crate::ids::RyeOsViewInstanceKey,
        key: String,
        expand: bool,
    ) -> bool {
        if !self
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.mount(instance_key))
            .is_some_and(|mount| mount.visible())
        {
            return false;
        }
        let Some(local) = self.mounted_local_state_mut(instance_key) else {
            return false;
        };
        let ViewLocalState::GenericList { expanded_rows, .. } = local else {
            return false;
        };
        if expand {
            expanded_rows.insert(key)
        } else {
            expanded_rows.remove(&key)
        }
    }

    pub(crate) fn set_tile_fold(
        &mut self,
        tile_id: TileId,
        section: usize,
        collapsed: bool,
    ) -> bool {
        let Some(tile) = self
            .active_view_set_mut()
            .and_then(|set| set.tiles.get_mut(&tile_id))
        else {
            return false;
        };
        match &mut tile.local {
            ViewLocalState::GenericList {
                collapsed: folds, ..
            } => {
                if collapsed {
                    folds.insert(section)
                } else {
                    folds.remove(&section)
                }
            }
            ViewLocalState::None | ViewLocalState::Field(_) => false,
        }
    }

    pub(crate) fn set_view_fold(
        &mut self,
        instance_key: &crate::ids::RyeOsViewInstanceKey,
        section: usize,
        collapsed: bool,
    ) -> bool {
        if !self
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.mount(instance_key))
            .is_some_and(|mount| mount.visible())
        {
            return false;
        }
        let Some(local) = self.mounted_local_state_mut(instance_key) else {
            return false;
        };
        let ViewLocalState::GenericList {
            collapsed: folds, ..
        } = local
        else {
            return false;
        };
        if collapsed {
            folds.insert(section)
        } else {
            folds.remove(&section)
        }
    }

    pub(crate) fn open_view(&mut self, view: ViewSpec) -> Vec<RyeOsEffect> {
        let Some(id) = self.active_view_set().map(|set| set.id) else {
            return Vec::new();
        };
        let Some(attachment_id) = self.insertion_attachment_id(id).map(str::to_string) else {
            return Vec::new();
        };
        self.open_view_under_binding(view, &attachment_id)
    }

    pub(crate) fn open_view_under_binding(
        &mut self,
        view: ViewSpec,
        attachment_id: &str,
    ) -> Vec<RyeOsEffect> {
        self.open_view_under_binding_with_route(
            view,
            attachment_id,
            crate::ui::composition_operation::UiCompositionRouteAssignment::Authored,
        )
    }

    pub(crate) fn open_view_under_binding_with_route(
        &mut self,
        view: ViewSpec,
        attachment_id: &str,
        route_assignment: crate::ui::composition_operation::UiCompositionRouteAssignment,
    ) -> Vec<RyeOsEffect> {
        let Some(retained) = self.binding_attachments.get(attachment_id) else {
            return Vec::new();
        };
        if !retained.views.contains_key(&view.view_ref) {
            return Vec::new();
        }
        let binding = crate::ui::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: retained.descriptor.binding_attachment_id.clone(),
            binding_generation: retained.descriptor.binding_generation,
            binding_digest: retained.descriptor.binding_digest.clone(),
        };
        let Some(set) = self.active_view_set() else {
            return Vec::new();
        };
        if set.tiling.mode == crate::surface::TilingModeSpec::SingleLens && !set.center_is_empty() {
            return self.prepare_composition_mutation(
                crate::ui::composition_operation::CompositionMutation::ReplaceView {
                    tab_id: set.id,
                    tile_id: set.focused_tile,
                    view_ref: view.view_ref,
                    binding,
                    subject: crate::surface::view_sets::SavedViewSelectionSource::FollowOwnSet,
                    route_assignment,
                },
            );
        }
        self.add_center_tile_under_binding_with_route(view, attachment_id, route_assignment)
    }

    /// Return one level up the step-in stack: restore the view a drill left and
    /// the facet context it read, then refetch so the restored trace re-resolves
    /// and re-subscribes its tail. No-op at the top of the tree (empty stack).
    pub(crate) fn pop_view(&mut self) -> Vec<RyeOsEffect> {
        let Some(origin) = self.focused_view_instance_key() else {
            return Vec::new();
        };
        if !matches!(
            self.presentation.as_ref().and_then(|p| p.mount(&origin)),
            Some(crate::ui::presentation_state::MountRef::SurfaceTile { .. })
        ) {
            return Vec::new();
        }
        let Some(set) = self.view_set_for_instance(&origin) else {
            return Vec::new();
        };
        self.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::LensPop {
                tab_id: set.id,
                tile_id: set.focused_tile,
            },
        )
    }

    /// A drill retains intent only. The causal fold captures the return frame;
    /// neither this caller nor the first ACK changes the mounted lens or route.
    pub(crate) fn prepare_lens_route_drill(
        &mut self,
        origin: &crate::ids::RyeOsViewInstanceKey,
        view_ref: Option<String>,
        route: Option<serde_json::Value>,
        label: Option<String>,
    ) -> Vec<RyeOsEffect> {
        let Some(attachment) = self.selection_attachment_for_instance(origin) else {
            return Vec::new();
        };
        self.prepare_lens_drill(origin, view_ref, attachment, route, label)
    }

    pub(crate) fn prepare_lens_drill(
        &mut self,
        origin: &crate::ids::RyeOsViewInstanceKey,
        view_ref: Option<String>,
        attachment: crate::ui::attachment::SelectionAttachment,
        route: Option<serde_json::Value>,
        label: Option<String>,
    ) -> Vec<RyeOsEffect> {
        let Some(set) = self.view_set_for_instance(origin) else {
            return Vec::new();
        };
        if set.tiling.mode != crate::surface::TilingModeSpec::SingleLens {
            return Vec::new();
        }
        let Some(tile) = set.tiles.get(&set.focused_tile) else {
            return Vec::new();
        };
        // The originating mount, not a later keyboard focus, owns the drill.
        if &tile.instance_key != origin {
            return Vec::new();
        }
        let tab_id = set.id;
        let tile_id = set.focused_tile;
        let view_ref = view_ref.unwrap_or_else(|| tile.view.view_ref.clone());
        let Some(descriptor) = self.binding_attachment_for_instance(origin) else {
            return Vec::new();
        };
        let binding = crate::ui::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: descriptor.binding_attachment_id.clone(),
            binding_generation: descriptor.binding_generation,
            binding_digest: descriptor.binding_digest.clone(),
        };
        let Some(crate::ui::presentation_state::MountRef::SurfaceTile { surface_id, .. }) =
            self.presentation.as_ref().and_then(|p| p.mount(origin))
        else {
            return Vec::new();
        };
        let Some(surface) = self
            .presentation
            .as_ref()
            .and_then(|p| p.surfaces().get(surface_id))
        else {
            return Vec::new();
        };
        let owner = crate::ui::composition_operation::CompositionOwner::Surface {
            surface_instance_id: surface.id().clone(),
            binding: surface.binding().clone(),
        };
        self.prepare_composition_owned(
            crate::ui::composition_operation::CompositionMutation::LensPush {
                tab_id,
                tile_id,
                view_ref,
                binding,
                attachment,
                input_route: route,
                label,
            },
            owner,
        )
    }

    pub(crate) fn close_tile_or_empty(&mut self, tile_id: TileId) -> bool {
        let Some(instance) = self
            .active_view_set()
            .and_then(|set| set.tiles.get(&tile_id))
            .map(|tile| tile.instance_key.clone())
        else {
            return false;
        };
        if self.input_buffers.iter().any(|(key, input)| {
            !input.text.is_empty()
                && super::model::InputBufferKey::storage_key_belongs_to(key, &instance)
        }) {
            self.notice(
                "Clear current input explicitly before closing this View.",
                super::view_model::RyeOsTone::Warn,
            );
            return false;
        }
        let Some(tab_id) = self.active_view_set().map(|set| set.id) else {
            return false;
        };
        let _ = self.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::CloseView { tab_id, tile_id },
        );
        false
    }

    pub(crate) fn add_center_tile(&mut self, view: ViewSpec) -> Vec<RyeOsEffect> {
        let Some(set_id) = self.active_view_set().map(|set| set.id) else {
            return Vec::new();
        };
        let Some(attachment_id) = self.insertion_attachment_id(set_id).map(str::to_string) else {
            return Vec::new();
        };
        self.add_center_tile_under_binding(view, &attachment_id)
    }

    pub(crate) fn add_center_tile_under_binding(
        &mut self,
        view: ViewSpec,
        attachment_id: &str,
    ) -> Vec<RyeOsEffect> {
        self.add_center_tile_under_binding_with_route(
            view,
            attachment_id,
            crate::ui::composition_operation::UiCompositionRouteAssignment::Authored,
        )
    }

    fn add_center_tile_under_binding_with_route(
        &mut self,
        view: ViewSpec,
        attachment_id: &str,
        route_assignment: crate::ui::composition_operation::UiCompositionRouteAssignment,
    ) -> Vec<RyeOsEffect> {
        let Some(retained) = self.binding_attachments.get(attachment_id) else {
            return Vec::new();
        };
        if !retained.views.contains_key(&view.view_ref) {
            return Vec::new();
        }
        let binding = crate::ui::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: retained.descriptor.binding_attachment_id.clone(),
            binding_generation: retained.descriptor.binding_generation,
            binding_digest: retained.descriptor.binding_digest.clone(),
        };
        let Some(set) = self.active_view_set() else {
            return Vec::new();
        };
        let placement = if let Some(target) = set.tile_ids().last().copied() {
            crate::ui::composition_operation::CompositionPlacement::Split {
                target,
                axis: match set.tiling.stack.arrange {
                    crate::surface::ArrangeSpec::Horizontal => crate::layout::SplitAxis::Horizontal,
                    crate::surface::ArrangeSpec::Vertical => crate::layout::SplitAxis::Vertical,
                },
                before: false,
                ratio: 0.5,
            }
        } else {
            crate::ui::composition_operation::CompositionPlacement::First
        };
        self.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::InsertView {
                tab_id: set.id,
                placement,
                view_ref: view.view_ref,
                binding,
                subject: crate::surface::view_sets::SavedViewSelectionSource::FollowOwnSet,
                route_assignment,
            },
        )
    }

    pub(crate) fn push_motion(&mut self, motion: RyeOsMotionEventVm) {
        self.ui.motion.push(motion);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::*;
    use crate::ui::reducer::test_support::{FIXTURE_PRESENTATION_ID, core as test_support_core};

    fn structural_core() -> RyeOsCore {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"structural-fixture", "tiles":["view:test/draft"],
            "views":{
                "view:test/draft":{"widget":"text","input":{"id":"line","submit":"route"}},
                "view:test/services":{"widget":"rows","sources":{"default":{"ref":"service:test/services"}}},
                "view:test/files":{"widget":"rows","sources":{"default":{"ref":"service:test/files"}}},
                "view:ryeos/items/space":{"widget":"rows","sources":{"default":{"ref":"service:test/space"}}}
            }
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core
    }

    fn settle_structure(core: &mut RyeOsCore) -> Vec<RyeOsEffect> {
        let producer = core
            .presentation_operations
            .producer
            .clone()
            .expect("fixture writer claimed");
        settle_pending_prefix(core, &producer)
    }

    fn select_attached_surface(core: &mut RyeOsCore, attachment_id: &str) {
        let surface = core
            .presentation
            .as_ref()
            .unwrap()
            .surfaces()
            .values()
            .find(|surface| surface.binding().binding_attachment_id == attachment_id)
            .unwrap();
        let id = surface.id().clone();
        let binding = surface.binding().clone();
        core.select_surface_instance(&id, &binding);
        settle_structure(core);
    }

    fn subject_core() -> RyeOsCore {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"subject-fixture", "tiles":["view:test/draft"],
            "views":{
                "view:test/draft":{"widget":"text","body":"@facet:selection.work.thread"},
                "view:test/services":{"widget":"rows","sources":{"default":{
                    "ref":"service:test/services","params":{"thread":"@facet:selection.work.thread"}
                }}}
            }
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core
    }

    fn lens_core() -> RyeOsCore {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"lens-fixture", "tiling":{"mode":"single_lens"},"tiles":["view:test/parent"],
            "views":{"view:test/parent":{"widget":"rows","input":{"id":"line","submit":"route"}},"view:test/child":{"widget":"rows"}}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core
    }

    fn push_child_lens(
        core: &mut RyeOsCore,
        attachment_id: &str,
        route: Option<serde_json::Value>,
    ) {
        let descriptor = core.binding_attachment(attachment_id).unwrap();
        let binding = crate::ui::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: descriptor.binding_attachment_id.clone(),
            binding_generation: descriptor.binding_generation,
            binding_digest: descriptor.binding_digest.clone(),
        };
        let set = core.active_view_set().unwrap();
        core.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::LensPush {
                tab_id: set.id,
                tile_id: set.focused_tile,
                view_ref: "view:test/child".into(),
                binding,
                attachment: crate::ui::attachment::SelectionAttachment::FollowViewSet {
                    view_set_id: set.id,
                },
                input_route: route,
                label: Some("child".into()),
            },
        );
        settle_structure(core);
    }

    #[test]
    fn equal_view_refs_keep_independent_attachment_bindings_and_requests() {
        let surface = |service: &str| {
            serde_json::json!({
                "name": service,
                "tiles": ["view:test/shared"],
                "views": {
                    "view:test/shared": {
                        "widget": "rows",
                        "sources": {"default": {"ref": service, "collection": "rows"}}
                    }
                }
            })
        };
        let a = fixture_attachment(
            "project-a",
            7,
            &"aa".repeat(32),
            Some("/project/a"),
            surface("service:test/a"),
        );
        let b = fixture_attachment(
            "project-b",
            11,
            &"bb".repeat(32),
            Some("/project/b"),
            surface("service:test/b"),
        );
        let mut core = crate::ui::reducer::test_support::core_for_presentation(
            session_with_attachments("project-a", vec![a, b]),
            FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        select_attached_surface(&mut core, "project-a");
        let first = core.active_surface_view_sets().unwrap()[0].tiles
            [&core.active_surface_view_sets().unwrap()[0].focused_tile]
            .instance_key
            .clone();
        core.open_view_under_binding(ViewSpec::bound("view:test/shared"), "project-b");
        settle_pending_prefix(&mut core, &producer);
        let second = core.active_surface_view_sets().unwrap()[0].tiles
            [&core.active_surface_view_sets().unwrap()[0].focused_tile]
            .instance_key
            .clone();

        assert_ne!(first, second);
        assert_eq!(
            core.binding_for_instance(&first, "view:test/shared")
                .unwrap()
                .sources["default"]
                .item_ref,
            "service:test/a"
        );
        assert_eq!(
            core.binding_for_instance(&second, "view:test/shared")
                .unwrap()
                .sources["default"]
                .item_ref,
            "service:test/b"
        );
        let coordinate = crate::ui::binding::UiBindingCoordinate::Source {
            view_ref: "view:test/shared".into(),
            channel: "default".into(),
        };
        let payload = crate::ui::binding::UiBindingPayload::SourceParameters {
            params: serde_json::json!({}),
        };
        let (request_a, _) = core
            .compiled_binding_operation(&first, coordinate.clone(), payload.clone())
            .unwrap();
        let (request_b, _) = core
            .compiled_binding_operation(&second, coordinate, payload)
            .unwrap();
        assert_eq!(request_a.binding_attachment_id, "project-a");
        assert_eq!(request_a.binding_generation, 7);
        assert_eq!(request_a.binding_digest, "aa".repeat(32));
        assert_eq!(request_b.binding_attachment_id, "project-b");
        assert_eq!(request_b.binding_generation, 11);
        assert_eq!(request_b.binding_digest, "bb".repeat(32));
    }

    #[test]
    fn stale_input_address_is_rejected_after_attachment_identity_changes() {
        let surface = serde_json::json!({
            "name": "input",
            "tiles": ["view:test/input"],
            "views": {"view:test/input": {"widget": "text", "input": {"id": "line"}}}
        });
        let a = fixture_attachment(
            "project-a",
            7,
            &"aa".repeat(32),
            Some("/project/a"),
            surface.clone(),
        );
        let b = fixture_attachment(
            "project-b",
            11,
            &"bb".repeat(32),
            Some("/project/b"),
            surface,
        );
        let mut core = crate::ui::reducer::test_support::core_for_presentation(
            session_with_attachments("project-a", vec![a, b]),
            FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        select_attached_surface(&mut core, "project-a");
        core.focused_input = Some(
            core.active_view_set()
                .unwrap()
                .tiles
                .values()
                .next()
                .unwrap()
                .instance_key
                .clone(),
        );
        let (key, _) = core
            .focused_input_instance()
            .expect("authored input is focused");
        let address = core.input_address_for(key.clone()).unwrap();
        assert!(core.input_address_is_live(&address));

        assert!(core.stamp_instance_binding(key.view_instance_key, "project-b"));
        assert!(!core.input_address_is_live(&address));
    }

    #[test]
    fn cross_view_set_move_preserves_the_source_selection_attachment() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name": "selection-dependent-move",
            "tiles": ["view:test/selection-dependent"],
            "views": {"view:test/selection-dependent": {
                "widget": "rows",
                "input": {"id": "message", "submit": "route"},
                "sources": {"default": {
                    "ref": "service:test/source",
                    "params": {"thread": "@facet:selection.work.thread"}
                }}
            }}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let source = core.active_view_set().unwrap().id;
        let source_id = core.view_set(source).unwrap().id;
        let tile = core.view_set(source).unwrap().focused_tile;
        let instance = core.view_set(source).unwrap().tiles[&tile]
            .instance_key
            .clone();
        let source_attachment = core
            .binding_attachment_for_instance(&instance)
            .unwrap()
            .clone();
        let draft_key = crate::ui::model::InputBufferKey::new(
            instance.clone(),
            "view:test/selection-dependent",
            "message",
            &source_attachment,
        )
        .storage_key();
        core.input_buffers.insert(
            draft_key.clone(),
            crate::ui::model::RyeOsInputState {
                text: "unsent move draft".into(),
                ..Default::default()
            },
        );
        let source_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance.clone(), "default")
                .encode();
        core.data
            .sources
            .insert(source_key.clone(), serde_json::json!({"retained": true}));

        core.new_view_set();
        settle_pending_prefix(&mut core, &producer);
        let target = core.active_view_set().unwrap().id;
        let target_id = core.view_set(target).unwrap().id;
        let source_index = core
            .active_surface_view_sets()
            .unwrap()
            .iter()
            .position(|set| set.id == source)
            .unwrap();
        core.switch_view_set_tab(source_index);
        settle_pending_prefix(&mut core, &producer);
        let guard = core.layout_guard();

        let before_stale = core.seat.events().len();
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::MoveTileToViewSet {
                    layout_guard: "stale-layout".into(),
                    tile_id: tile.0.to_string(),
                    view_set_id: target_id,
                },
            },
        });
        assert_eq!(core.seat.events().len(), before_stale);

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::MoveTileToViewSet {
                    layout_guard: guard,
                    tile_id: tile.0.to_string(),
                    view_set_id: target_id,
                },
            },
        });

        assert!(core.view_set(source).unwrap().tiles.contains_key(&tile));
        assert!(!core.view_set(target).unwrap().tiles.contains_key(&tile));
        let prepared = pending_prefix(&core);
        assert_eq!(prepared.len(), 1);
        core.acknowledge_seat_prefix(&producer, prepared);
        assert!(core.view_set(source).unwrap().tiles.contains_key(&tile));
        assert!(!core.view_set(target).unwrap().tiles.contains_key(&tile));
        let completion = pending_prefix(&core);
        assert_eq!(completion.len(), 1);
        core.acknowledge_seat_prefix(&producer, completion);

        assert!(!core.view_set(source).unwrap().tiles.contains_key(&tile));
        assert!(core.view_set(target).unwrap().tiles.contains_key(&tile));
        assert_eq!(core.active_view_set().unwrap().id, target);
        assert_eq!(core.data.sources[&source_key]["retained"], true);
        assert_eq!(
            core.binding_attachment_for_instance(&instance),
            Some(&source_attachment)
        );
        assert_eq!(core.input_buffers[&draft_key].text, "unsent move draft");
        assert_eq!(
            core.selection_attachments.get(&instance),
            Some(&crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: source_id,
            })
        );
    }

    fn two_surface_actor_core() -> RyeOsCore {
        // Session admission is fixture metadata; structure still goes through
        // the actual actor preparation/completion and acknowledged replay owner.
        let mut session = session_with_surface(serde_json::json!({
            "name":"cross-surface", "tiles":["view:test/plain"],
            "views":{"view:test/plain":{"widget":"rows","input":{"id":"message","submit":"route"},
                "sources":{"default":{"ref":"service:test/source","params":{"thread":"@facet:selection.work.thread"}}}}}
        }));
        let descriptor = session.binding_attachments[0].clone();
        let membership = &mut session.presentations[0];
        let mut destination = membership.surfaces[0].clone();
        destination.surface_instance_id = "00000000-0000-4000-8000-000000000099".into();
        destination.ordinal = 2;
        let seeds = descriptor
            .presentation
            .definition
            .surface()
            .unwrap()
            .to_mount_seeds()
            .unwrap();
        let active = seeds.first().map(|seed| seed.id.clone());
        destination.mount_manifest =
            crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
                crate::ids::PresentationId::try_new(membership.presentation_id.clone()).unwrap(),
                crate::ids::SurfaceInstanceId::try_new(destination.surface_instance_id.clone())
                    .unwrap(),
                membership.next_mount_id,
                seeds
                    .into_iter()
                    .map(|seed| (seed, destination.binding.clone()))
                    .collect(),
                active.as_deref(),
            )
            .unwrap();
        membership.next_mount_id = destination.mount_manifest.next_mount_id;
        membership.surfaces.push(destination);
        test_support_core(session)
    }

    #[test]
    fn inactive_surface_rename_requires_completion_ack_and_replays_exact_identity() {
        let mut core = two_surface_actor_core();
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let active = core.active_surface().unwrap().id().clone();
        let target = core
            .presentation
            .as_ref()
            .unwrap()
            .surfaces()
            .values()
            .find(|surface| surface.id() != &active)
            .unwrap();
        let target_id = target.id().clone();
        let binding = target.binding().clone();
        let original_title = target.title().to_owned();
        let mount_keys = target
            .view_sets()
            .iter()
            .flat_map(|set| set.tiles.values())
            .map(|tile| tile.instance_key.clone())
            .collect::<Vec<_>>();
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::RenameSurface {
                    surface_instance_id: target_id.clone(),
                    binding: binding.clone(),
                    title: "Inactive comparison".into(),
                },
            },
        });
        assert_eq!(pending_prefix(&core).len(), 1);
        assert_eq!(
            core.presentation.as_ref().unwrap().surfaces()[&target_id].title(),
            original_title
        );
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert_eq!(pending_prefix(&core).len(), 1);
        assert_eq!(
            core.presentation.as_ref().unwrap().surfaces()[&target_id].title(),
            original_title
        );
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        let renamed = &core.presentation.as_ref().unwrap().surfaces()[&target_id];
        assert_eq!(renamed.title(), "Inactive comparison");
        assert_eq!(renamed.binding(), &binding);
        assert_eq!(core.active_surface().unwrap().id(), &active);
        let crate::ui::seat::SeatEventKind::PresentationInitialized(record) =
            &core.seat.events()[0].kind
        else {
            panic!("actor fixture requires retained initialization");
        };
        let initial = record.initialization.clone();
        let cold = crate::ui::composition_replay::UiCompositionReplay::from_acknowledged_prefix(
            &initial,
            core.seat.events(),
        )
        .unwrap();
        let cold_surface = &cold.composition.surfaces()[&target_id];
        assert_eq!(cold_surface.title(), "Inactive comparison");
        assert_eq!(cold_surface.binding(), &binding);
        assert_eq!(
            cold_surface
                .view_sets()
                .iter()
                .flat_map(|set| set.tiles.values())
                .map(|tile| tile.instance_key.clone())
                .collect::<Vec<_>>(),
            mount_keys
        );
        assert!(pending_prefix(&core).is_empty());
    }

    #[test]
    fn cross_surface_move_migrates_local_draft_only_after_completion_ack() {
        let mut core = two_surface_actor_core();
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let source_surface = core.active_surface().unwrap().id().clone();
        let source_set = core.active_view_set().unwrap().id;
        let tile = core.active_view_set().unwrap().focused_tile;
        let old = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();
        // Cross-Surface movement cannot retain an implicit FollowViewSet to
        // the old Surface. Resolve the fixture's subject through the existing
        // typed owner before asking it to move, without weakening that fence.
        let pinned = core
            .bounded_pinned_subject(
                &old,
                std::collections::BTreeMap::from([(
                    "selection.work.thread".into(),
                    serde_json::json!("move-thread"),
                )]),
            )
            .unwrap();
        core.prepare_subject_operation(old.clone(), pinned.clone());
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.selection_attachments.get(&old), Some(&pinned));
        let old_source =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(old.clone(), "default").encode();
        core.emit_fetch_source_for_instance(old.clone(), "view:test/plain");
        let late_effect = core
            .pending_effects
            .iter()
            .find_map(|(id, effect)| {
                matches!(effect, crate::ui::effect::RyeOsEffectKind::FetchSource { tile_id, .. }
                if tile_id == &old_source)
                .then_some(*id)
            })
            .expect("actual source emission must retain its pending request");
        let old_epoch = core.data.source_epoch.get(&old_source).copied();
        let destination = core
            .presentation
            .as_ref()
            .unwrap()
            .surfaces()
            .values()
            .find(|surface| surface.id() != &source_surface)
            .unwrap();
        let destination_surface = destination.id().clone();
        let destination_set = destination.view_sets()[0].id;
        let mut draft = crate::ui::model::InputBufferKey::new(
            old.clone(),
            "view:test/plain",
            "message",
            core.binding_attachment_for_instance(&old).unwrap(),
        );
        let old_storage = draft.storage_key();
        core.input_buffers.insert(
            old_storage.clone(),
            crate::ui::model::RyeOsInputState {
                text: "draft survives move".into(),
                cursor: 5,
                ..Default::default()
            },
        );
        let crate::view_set::ViewLocalState::GenericList { cursor, .. } = core
            .presentation
            .as_mut()
            .unwrap()
            .mount_local_mut(&old)
            .unwrap()
        else {
            panic!("rows fixture must have list-local state");
        };
        *cursor = 7;
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::MoveTileToSurfaceViewSet {
                    layout_guard: core.layout_guard(),
                    tile_id: tile.0.to_string(),
                    surface_instance_id: destination_surface.clone(),
                    view_set_id: destination_set,
                },
            },
        });
        assert_eq!(
            pending_prefix(&core).len(),
            1,
            "notices: {:?}",
            core.ui.notices
        );
        assert!(core.route_write_fenced_by_composition(&old));
        assert!(core.view_set(source_set).unwrap().tiles.contains_key(&tile));
        assert_eq!(core.input_buffers[&old_storage].text, "draft survives move");
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert!(core.route_write_fenced_by_composition(&old));
        assert!(core.view_set(source_set).unwrap().tiles.contains_key(&tile));
        assert!(core.input_buffers.contains_key(&old_storage));
        assert_eq!(pending_prefix(&core).len(), 1);
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        let moved = core.presentation.as_ref().unwrap().surfaces()[&destination_surface]
            .view_sets()
            .iter()
            .find(|set| set.id == destination_set)
            .unwrap()
            .tiles[&tile]
            .instance_key
            .clone();
        assert_ne!(old, moved);
        assert!(!core.route_write_fenced_by_composition(&old));
        assert!(!core.route_write_fenced_by_composition(&moved));
        assert!(core.pending_effects.contains_key(&late_effect));
        assert_eq!(core.data.source_epoch.get(&old_source).copied(), old_epoch);
        let new_source =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(moved.clone(), "default").encode();
        let new_observation = core.data.sources.get(&new_source).cloned();
        let new_epoch = core.data.source_epoch.get(&new_source).copied();
        core.apply_effect_result(crate::ui::effect::RyeOsEffectResult {
            id: late_effect,
            ok: true,
            kind: crate::ui::effect::RyeOsEffectResultKind::SourceData,
            data: Some(serde_json::json!({"late_old_mount":true})),
            error: None,
        });
        assert!(!core.pending_effects.contains_key(&late_effect));
        assert_eq!(core.data.sources.get(&new_source).cloned(), new_observation);
        assert_eq!(core.data.source_epoch.get(&new_source).copied(), new_epoch);
        assert!(!core.data.sources.contains_key(&old_source));
        assert!(core.presentation.as_ref().unwrap().mount(&old).is_none());
        draft.view_instance_key = moved.clone();
        let new_storage = draft.storage_key();
        assert!(!core.input_buffers.contains_key(&old_storage));
        assert_eq!(core.input_buffers[&new_storage].text, "draft survives move");
        assert_eq!(core.input_buffers[&new_storage].cursor, 5);
        assert!(matches!(
            core.presentation
                .as_ref()
                .unwrap()
                .mount(&moved)
                .unwrap()
                .local(),
            crate::view_set::ViewLocalState::GenericList { cursor: 7, .. }
        ));
        core.input_buffers.get_mut(&new_storage).unwrap().text = "later edit".into();
        let replay = core.acknowledged_composition_replay().unwrap();
        core.reconcile_acknowledged_composition_with_replay(replay)
            .unwrap();
        assert_eq!(core.input_buffers[&new_storage].text, "later edit");
        assert!(!core.input_buffers.contains_key(&old_storage));
        assert!(pending_prefix(&core).is_empty());
    }

    #[test]
    fn cross_view_set_move_still_allows_selection_independent_views() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name": "independent-move",
            "tiles": ["view:test/independent"],
            "views": {"view:test/independent": {"widget": "rows"}}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let source = core.active_view_set().unwrap().id;
        let tile = core.view_set(source).unwrap().focused_tile;
        let instance = core.view_set(source).unwrap().tiles[&tile]
            .instance_key
            .clone();

        core.new_view_set();
        settle_pending_prefix(&mut core, &producer);
        let target = core.active_view_set().unwrap().id;
        let target_id = core.view_set(target).unwrap().id;
        core.add_center_tile(ViewSpec::bound("view:test/independent"));
        settle_pending_prefix(&mut core, &producer);
        let existing_target_tile = core.view_set(target).unwrap().focused_tile;
        core.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::IsolateView {
                tab_id: target,
                tile_id: Some(existing_target_tile),
            },
        );
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(
            core.view_set(target).unwrap().maximized_tile,
            Some(existing_target_tile)
        );
        let source_index = core
            .active_surface_view_sets()
            .unwrap()
            .iter()
            .position(|set| set.id == source)
            .unwrap();
        core.switch_view_set_tab(source_index);
        settle_pending_prefix(&mut core, &producer);
        let guard = core.layout_guard();

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::MoveTileToViewSet {
                    layout_guard: guard,
                    tile_id: tile.0.to_string(),
                    view_set_id: target_id,
                },
            },
        });

        assert!(core.view_set(source).unwrap().tiles.contains_key(&tile));
        settle_pending_prefix(&mut core, &producer);

        assert!(!core.view_set(source).unwrap().tiles.contains_key(&tile));
        assert!(core.view_set(target).unwrap().tiles.contains_key(&tile));
        assert_eq!(core.active_view_set().unwrap().id, target);
        assert_eq!(core.view_set(target).unwrap().maximized_tile, None);
        assert!(
            core.view_set(target)
                .unwrap()
                .tiles
                .contains_key(&existing_target_tile)
        );
        assert_eq!(
            core.selection_attachments.get(&instance),
            Some(&crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: source,
            })
        );
        let before_close = core.seat.events().len();
        core.close_view_set(source);
        assert!(
            core.seat.events().len() > before_close,
            "an inert follower cannot strand an empty source tab"
        );
        settle_pending_prefix(&mut core, &producer);
        assert!(core.view_set(source).is_none());
        assert!(core.view_set(target).unwrap().tiles.contains_key(&tile));
        assert_eq!(
            core.selection_attachment_for_instance(&instance),
            Some(crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: target,
            }),
            "an independent View has no retained cross-tab selection dependency"
        );
        assert_eq!(core.followed_selection_view_set(&instance), Some(target));
    }

    #[test]
    fn cross_view_set_move_preserves_writer_owner_and_offers_follow_not_pin() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name": "writer-move",
            "tiles": ["view:test/selection-writer"],
            "views": {"view:test/selection-writer": {
                "widget": "rows",
                "affordances": [{
                    "id": "select",
                    "invoke": {"plane": "ui", "facet": "selection.work", "value": {}}
                }]
            }}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let source = core.active_view_set().unwrap().id;
        let source_id = core.view_set(source).unwrap().id;
        let tile = core.view_set(source).unwrap().focused_tile;
        let instance = core.view_set(source).unwrap().tiles[&tile]
            .instance_key
            .clone();
        core.new_view_set();
        settle_pending_prefix(&mut core, &producer);
        let target = core.active_view_set().unwrap().id;
        let target_id = core.view_set(target).unwrap().id;
        let source_index = core
            .active_surface_view_sets()
            .unwrap()
            .iter()
            .position(|set| set.id == source)
            .unwrap();
        core.switch_view_set_tab(source_index);
        settle_pending_prefix(&mut core, &producer);

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::MoveTileToViewSet {
                    layout_guard: core.layout_guard(),
                    tile_id: tile.0.to_string(),
                    view_set_id: target_id,
                },
            },
        });
        settle_pending_prefix(&mut core, &producer);

        assert_eq!(
            core.selection_attachments.get(&instance),
            Some(&crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: source_id,
            })
        );
        let actions = crate::ui::view_model::available_command_items(&core);
        assert!(actions.iter().any(|action| matches!(
            &action.intent,
            RyeOsUiIntent::FollowViewSetSelection { view_set_id, .. } if *view_set_id == target_id
        )));
        assert!(
            !actions
                .iter()
                .any(|action| matches!(&action.intent, RyeOsUiIntent::PinViewSelection { .. }))
        );
    }

    #[test]
    fn same_seat_move_keeps_retained_lens_navigation_on_the_same_mount() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name": "lens-move",
            "tiling": {"mode": "single_lens"},
            "tiles": ["view:test/parent"],
            "views": {
                "view:test/parent": {"widget": "rows", "input": {"id": "line", "submit": "route"}},
                "view:test/child": {"widget": "rows"}
            }
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let source = core.active_view_set().unwrap().id;
        let tile = core.active_view_set().unwrap().focused_tile;
        let instance = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();
        push_child_lens(&mut core, "fixture-attachment", None);
        assert_eq!(core.retained_lens_frames[&instance].len(), 1);

        core.new_view_set();
        settle_pending_prefix(&mut core, &producer);
        let destination = core.active_view_set().unwrap().id;
        let source_index = core
            .active_surface_view_sets()
            .unwrap()
            .iter()
            .position(|set| set.id == source)
            .unwrap();
        core.switch_view_set_tab(source_index);
        settle_pending_prefix(&mut core, &producer);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::MoveTileToViewSet {
                    layout_guard: core.layout_guard(),
                    tile_id: tile.0.to_string(),
                    view_set_id: destination,
                },
            },
        });
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.active_view_set().unwrap().id, destination);
        assert_eq!(core.retained_lens_frames[&instance].len(), 1);
        core.pop_view();
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(
            core.active_view_set().unwrap().tiles[&tile].instance_key,
            instance
        );
        assert!(matches!(
            core.active_view_set().unwrap().focused_view(),
            Some(ViewSpec { view_ref }) if view_ref == "view:test/parent"
        ));
    }

    #[test]
    fn exact_view_pointer_state_addresses_dock_instances() {
        let mut core = test_support_core(session());
        core.presentation
            .as_mut()
            .unwrap()
            .dock_mut("right")
            .unwrap()
            .visible = true;
        let instance_key = core
            .shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Right)
            .unwrap();
        *core.mounted_local_state_mut(&instance_key).unwrap() =
            ViewSpec::bound("view:test/dock").initial_local_state();

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetViewCursor {
                instance_key: instance_key.clone(),
                index: 7,
            },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetViewFold {
                instance_key: instance_key.clone(),
                section: 2,
                collapsed: true,
            },
        });

        let ViewLocalState::GenericList {
            cursor, collapsed, ..
        } = core.mounted_local_state(&instance_key).unwrap()
        else {
            panic!("dock should retain generic list state");
        };
        assert_eq!(*cursor, 7);
        assert_eq!(collapsed.iter().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn field_view_added_mid_session_gets_field_local_state_before_fetch() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"field-add", "tiles":["view:test/start"],
            "views":{
                "view:test/start":{"widget":"text"},
                "view:test/field":{
                    "widget":"field",
                    "sources":{"default":{"ref":"service:test/field"}},
                    "projections":{"schema_version":"ryeos.ui.field.projection.v1"}
                }
            }
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core.add_center_tile(ViewSpec::bound("view:test/field"));
        let effects = settle_pending_prefix(&mut core, &producer);
        let tile = core
            .active_view_set()
            .unwrap()
            .tiles
            .get(&core.active_view_set().unwrap().focused_tile)
            .unwrap();
        assert!(matches!(
            tile.local,
            crate::view_set::ViewLocalState::Field(_)
        ));
        assert!(effects.iter().any(|effect| matches!(
            &effect.kind, RyeOsEffectKind::FetchSource { tile_id, .. }
                if crate::ui::source_key::RyeOsSourceInstanceKey::decode(tile_id)
                    .is_some_and(|key| key.belongs_to(&tile.instance_key))
        )));
    }

    #[test]
    fn sections_flat_cursor_selects_a_row_and_resolves_its_section_activation() {
        use crate::ui::view_model::{RyeOsLayoutNodeVm, RyeOsViewVm, intent_for_focused_row};
        let session = session_with_surface(serde_json::json!({
            "name": "t",
            "tiles": ["view:ryeos/ryeos/status"],
            "views": {
                "view:ryeos/ryeos/status": {
                    "widget": "sections",
                    "sources": {
                        "threads": { "ref": "service:threads/list" },
                        "bundles": { "ref": "service:bundle/list" }
                    },
                    "affordances": [{
                        "id": "aim-input",
                        "label": "Aim",
                        "invoke": { "plane": "ui", "facet": "input.route", "merge": { "thread": "{record.thread_id}" } }
                    }],
                    "sections": [
                        { "title": "Threads", "source_channel": "threads", "collection": "threads", "projection": { "primary": "thread_id" }, "activate": "aim-input" },
                        { "title": "Bundles", "source_channel": "bundles", "collection": "bundles", "projection": { "primary": "name" } }
                    ]
                }
            }
        }));
        let mut core = test_support_core(session);
        let tile = core.active_view_set().unwrap().focused_tile;
        let key = tile.0.to_string();
        let instance_key = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();
        core.data.sources.insert(
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), "threads")
                .encode(),
            serde_json::json!({ "threads": [ { "thread_id": "T-ab" }, { "thread_id": "T-cd" } ]}),
        );
        core.data.sources.insert(
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key, "bundles").encode(),
            serde_json::json!({ "bundles": [ { "name": "ryeos" } ]}),
        );

        fn find_tile_view(node: &RyeOsLayoutNodeVm) -> Option<&RyeOsViewVm> {
            match node {
                RyeOsLayoutNodeVm::Tile { view, .. } => Some(view),
                RyeOsLayoutNodeVm::Split { first, second, .. } => {
                    find_tile_view(first).or_else(|| find_tile_view(second))
                }
            }
        }
        let selected_primaries = |core: &RyeOsCore| -> Vec<String> {
            let vm = build_view_model(core);
            let root = vm
                .view_set
                .as_ref()
                .unwrap()
                .root
                .clone()
                .expect("layout root");
            match find_tile_view(&root).expect("tile view") {
                RyeOsViewVm::Sections { sections, .. } => sections
                    .iter()
                    .flat_map(|s| &s.rows)
                    .filter(|r| r.selected)
                    .map(|r| r.primary.clone())
                    .collect(),
                other => panic!("expected sections view, got {other:?}"),
            }
        };

        // Flat cursor 0 = the first Threads row; its section's activation fires
        // carrying that row's record.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetTileCursor {
                tile_id: key.clone(),
                index: 0,
            },
        });
        assert_eq!(selected_primaries(&core), vec!["T-ab".to_string()]);
        match intent_for_focused_row(&core).expect("threads row activates") {
            RyeOsUiIntent::InvokeAffordance {
                affordance_id,
                record,
                ..
            } => {
                assert_eq!(affordance_id, "aim-input");
                assert_eq!(record["thread_id"], "T-ab");
            }
            other => panic!("expected aim-input invoke, got {other:?}"),
        }

        // Flat cursor 2 = the first Bundles row (Threads contributed 2). Bundles
        // declares no activation, so the point resolves a row but no intent.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetTileCursor {
                tile_id: key.clone(),
                index: 2,
            },
        });
        assert_eq!(selected_primaries(&core), vec!["ryeos".to_string()]);
        assert!(
            intent_for_focused_row(&core).is_none(),
            "a bundles row has no section activation"
        );
    }

    #[test]
    fn folding_a_section_collapses_it_to_a_single_header_point() {
        use crate::ui::view_model::{RyeOsLayoutNodeVm, RyeOsViewVm};
        let session = session_with_surface(serde_json::json!({
            "name": "t",
            "tiles": ["view:ryeos/ryeos/status"],
            "views": {
                "view:ryeos/ryeos/status": {
                    "widget": "sections",
                    "sources": {
                        "threads": { "ref": "service:threads/list" },
                        "bundles": { "ref": "service:bundle/list" }
                    },
                    "sections": [
                        { "title": "Threads", "source_channel": "threads", "collection": "threads", "projection": { "primary": "thread_id" } },
                        { "title": "Bundles", "source_channel": "bundles", "collection": "bundles", "projection": { "primary": "name" } }
                    ]
                }
            }
        }));
        let mut core = test_support_core(session);
        let tile = core.active_view_set().unwrap().focused_tile;
        let key = tile.0.to_string();
        let instance_key = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();
        core.data.sources.insert(
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), "threads")
                .encode(),
            serde_json::json!({ "threads": [ { "thread_id": "T-ab" }, { "thread_id": "T-cd" } ]}),
        );
        core.data.sources.insert(
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key, "bundles").encode(),
            serde_json::json!({ "bundles": [ { "name": "ryeos" } ]}),
        );

        fn tile_sections(
            core: &RyeOsCore,
        ) -> (Vec<crate::ui::view_model::RyeOsSectionVm>, Option<usize>) {
            fn find(node: &RyeOsLayoutNodeVm) -> Option<&RyeOsViewVm> {
                match node {
                    RyeOsLayoutNodeVm::Tile { view, .. } => Some(view),
                    RyeOsLayoutNodeVm::Split { first, second, .. } => {
                        find(first).or_else(|| find(second))
                    }
                }
            }
            let vm = build_view_model(core);
            match find(&vm.view_set.as_ref().unwrap().root.clone().expect("root"))
                .expect("tile view")
            {
                RyeOsViewVm::Sections {
                    sections,
                    fold_section,
                    ..
                } => (sections.clone(), *fold_section),
                other => panic!("expected sections, got {other:?}"),
            }
        }

        // Fold section 0 (Threads).
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetFold {
                tile_id: key.clone(),
                section: 0,
                collapsed: true,
            },
        });
        let (sections, _) = tile_sections(&core);
        assert!(sections[0].collapsed, "threads is collapsed");
        assert_eq!(sections[0].count, 2, "collapsed header still reports count");
        assert!(sections[0].rows.is_empty(), "collapsed rows are hidden");
        assert!(!sections[1].collapsed);
        assert_eq!(sections[1].rows.len(), 1);

        // The collapsed section now occupies one flat point: its header at
        // index 0. The point there marks the header (no row) and folds it.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetTileCursor {
                tile_id: key.clone(),
                index: 0,
            },
        });
        let (sections, fold_section) = tile_sections(&core);
        assert!(
            sections[0].header_selected,
            "collapsed header carries the point"
        );
        assert_eq!(fold_section, Some(0), "fold key would toggle threads");

        // Flat index 1 is now the first Bundles row (Threads contributes one
        // header point, not two rows).
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetTileCursor {
                tile_id: key.clone(),
                index: 1,
            },
        });
        let (sections, fold_section) = tile_sections(&core);
        assert!(!sections[0].header_selected);
        assert!(
            sections[1].rows[0].selected,
            "point lands on the bundles row"
        );
        assert_eq!(fold_section, Some(1));
    }

    #[test]
    fn open_view_adds_missing_view_set_tile() {
        let mut core = structural_core();
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);
        let before = core.active_view_set().unwrap().tile_ids().len();
        let preparation = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        assert!(preparation.iter().all(|effect| !matches!(
            &effect.kind, RyeOsEffectKind::FetchSource { request, .. }
                if matches!(&request.coordinate, crate::ui::binding::UiBindingCoordinate::Source { view_ref, .. } if view_ref == "view:test/services")
        )));
        assert_eq!(core.active_view_set().unwrap().tile_ids().len(), before);
        let effects = settle_structure(&mut core);
        assert_eq!(core.active_view_set().unwrap().tile_ids().len(), before + 1);
        assert!(matches!(
            core.active_view_set().unwrap().focused_view(),
            Some(ViewSpec { view_ref }) if view_ref == "view:test/services"
        ));
        assert!(
            core.ui
                .motion
                .iter()
                .any(|event| matches!(event, RyeOsMotionEventVm::TileSplit { .. }))
        );
        assert!(core.ui.motion.iter().any(|event| matches!(
            event,
            RyeOsMotionEventVm::TileEnter { tile_id } if tile_id == &core.active_view_set().unwrap().focused_tile.0.to_string()
        )));
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::FetchSource { .. })
        ));
    }

    #[test]
    fn single_lens_open_view_replaces_center_instead_of_splitting() {
        // The cell-grid (TUI) composition: one center lens. Opening a
        // different view swaps the lens in place — the tile count stays at
        // one, no split, and the new view fetches.
        let mut core = test_support_core(session_with_surface(
            serde_json::json!({"name":"single-open","tiling":{"mode":"single_lens"},"tiles":["view:ryeos/items/space"],"views":{"view:ryeos/items/space":{"widget":"rows","sources":{"default":{"ref":"service:test/space"}}},"view:test/services":{"widget":"rows","sources":{"default":{"ref":"service:test/services"}}}}}),
        ));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core.focused_input = None;

        // First open fills the empty center with the one lens.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);
        assert_eq!(core.active_view_set().unwrap().tile_ids().len(), 1);
        let instance = core.focused_view_instance_key().unwrap();
        core.selection_attachments.insert(
            instance.clone(),
            crate::ui::attachment::SelectionAttachment::Pinned {
                values: std::collections::BTreeMap::new(),
                fingerprint: "old-lens".into(),
            },
        );
        core.ui.motion.clear();

        // Switching the lens replaces in place — still exactly one tile.
        let preparation = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        assert!(preparation.iter().all(|effect| !matches!(
            &effect.kind, RyeOsEffectKind::FetchSource { request, .. }
                if matches!(&request.coordinate, crate::ui::binding::UiBindingCoordinate::Source { view_ref, .. } if view_ref == "view:test/services")
        )));
        let effects = settle_structure(&mut core);

        assert_eq!(
            core.active_view_set().unwrap().tile_ids().len(),
            1,
            "single-lens never splits a second tile"
        );
        assert!(matches!(
            core.active_view_set().unwrap().focused_view(),
            Some(ViewSpec { view_ref }) if view_ref == "view:test/services"
        ));
        assert!(
            !core
                .ui
                .motion
                .iter()
                .any(|event| matches!(event, RyeOsMotionEventVm::TileSplit { .. })),
            "no split motion when swapping the single lens"
        );
        assert!(
            matches!(
                effects.first().map(|effect| &effect.kind),
                Some(RyeOsEffectKind::FetchSource { .. })
            ),
            "the swapped-in lens fetches its source"
        );
        assert!(
            !matches!(core.selection_attachments.get(&instance),Some(crate::ui::attachment::SelectionAttachment::Pinned {fingerprint,..}) if fingerprint=="old-lens"),
            "a different lens must not inherit the replaced subject attachment"
        );

        // OpenNewView also collapses to a replace — no second tile.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);
        assert_eq!(
            core.active_view_set().unwrap().tile_ids().len(),
            1,
            "OpenNewView does not add a tile in single-lens"
        );
    }

    #[test]
    fn lens_pop_restores_the_attachment_captured_by_the_return_frame() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"subject-lens", "tiling":{"mode":"single_lens"},"tiles":["view:test/parent"],
            "views":{
                "view:test/parent":{"widget":"rows","body":"@facet:selection.work.thread"},
                "view:test/child":{"widget":"rows"}
            }
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core.focused_input = None;
        let instance = core.focused_view_instance_key().unwrap();
        let attachment = core
            .bounded_pinned_subject(
                &instance,
                std::collections::BTreeMap::from([(
                    "selection.work.thread".into(),
                    serde_json::json!("parent-thread"),
                )]),
            )
            .unwrap();
        core.prepare_subject_operation(instance.clone(), attachment.clone());
        settle_structure(&mut core);
        push_child_lens(&mut core, "fixture-attachment", None);
        assert_ne!(core.selection_attachments.get(&instance), Some(&attachment));
        core.pop_view();
        settle_structure(&mut core);

        assert_eq!(core.selection_attachments.get(&instance), Some(&attachment));
        assert!(matches!(
            core.active_view_set().unwrap().focused_view(),
            Some(ViewSpec { view_ref }) if view_ref == "view:test/parent"
        ));
    }

    #[test]
    fn closing_last_drilled_tile_prunes_its_return_frame_before_later_pop() {
        let mut core = lens_core();
        core.focused_input = None;
        let instance = core.focused_view_instance_key().unwrap();
        push_child_lens(&mut core, "fixture-attachment", None);
        assert!(!core.retained_lens_frames[&instance].is_empty());
        let tile_id = core.active_view_set().unwrap().focused_tile;

        core.close_tile_or_empty(tile_id);
        settle_structure(&mut core);
        assert!(core.active_view_set().unwrap().center_is_empty());
        assert!(!core.retained_lens_frames.contains_key(&instance));
        assert!(!core.retained_lens_labels.contains_key(&instance));
        let motion_count = core.ui.motion.len();
        let effect_count = core.pending_effects.len();
        let seat_event_count = core.seat.events().len();
        assert!(core.pop_view().is_empty());
        assert_eq!(core.ui.motion.len(), motion_count);
        assert_eq!(core.pending_effects.len(), effect_count);
        assert_eq!(core.seat.events().len(), seat_event_count);
    }

    #[test]
    fn lens_pop_restores_authored_targetless_route_without_global_facet_write() {
        let mut core = lens_core();
        core.focused_input = None;
        let instance = core.focused_view_instance_key().unwrap();
        let key = crate::ui::seat::input_route_facet_key(&instance);
        push_child_lens(
            &mut core,
            "fixture-attachment",
            Some(serde_json::json!({"thread":"T-child"})),
        );
        core.pop_view();
        settle_structure(&mut core);
        assert_eq!(
            core.retained_input_routes
                .get(&instance)
                .map(|(_, route)| route),
            Some(&Some(serde_json::json!({})))
        );
        assert!(
            core.seat.fold().get(&key).is_none(),
            "lens return restores its exact route without inventing a global facet write"
        );
    }

    #[test]
    fn lens_pop_restores_cross_attachment_authority() {
        let surface = serde_json::json!({
            "name": "lens",
            "tiling":{"mode":"single_lens"},
            "tiles": ["view:test/parent"],
            "views": {
                "view:test/parent": {"widget": "rows"},
                "view:test/child": {"widget": "rows"}
            }
        });
        let a = fixture_attachment(
            "project-a",
            7,
            &"aa".repeat(32),
            Some("/a"),
            surface.clone(),
        );
        let b = fixture_attachment("project-b", 11, &"bb".repeat(32), Some("/b"), surface);
        let mut core = crate::ui::reducer::test_support::core_for_presentation(
            session_with_attachments("project-a", vec![a, b]),
            FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        select_attached_surface(&mut core, "project-a");
        core.focused_input = None;
        let instance = core.focused_view_instance_key().unwrap();
        push_child_lens(&mut core, "project-b", None);
        assert_eq!(
            core.binding_attachment_for_instance(&instance)
                .map(|binding| binding.binding_attachment_id.as_str()),
            Some("project-b")
        );

        core.pop_view();
        settle_structure(&mut core);
        assert_eq!(
            core.binding_attachment_for_instance(&instance)
                .map(|binding| binding.binding_attachment_id.as_str()),
            Some("project-a")
        );
        assert!(
            matches!(core.active_surface_view_sets().unwrap()[0].focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:test/parent")
        );
    }

    #[test]
    fn lens_pop_rebuilds_a_missing_lookup_cache_from_retained_admitted_history() {
        let mut core = lens_core();
        let instance = core.focused_view_instance_key().unwrap();
        push_child_lens(&mut core, "fixture-attachment", None);
        core.binding_attachments.remove("fixture-attachment");
        let depth = core.retained_lens_frames[&instance].len();
        // This is a lost parsed lookup, not session revocation. The exact
        // descriptor still belongs to the initialized session and signed
        // prefix. Release refuses live lens owners; ReplaceSession builds a
        // new Core rather than treating cache removal as new authority.
        assert!(core.pop_view().is_empty());
        assert_eq!(core.retained_lens_frames[&instance].len(), depth);
        settle_structure(&mut core);
        assert!(
            core.retained_lens_frames
                .get(&instance)
                .is_none_or(Vec::is_empty)
        );
        assert_eq!(core.mounted_view_ref(&instance), Some("view:test/parent"));
        assert_eq!(
            core.binding_attachment_for_instance(&instance)
                .unwrap()
                .binding_attachment_id,
            "fixture-attachment"
        );
    }

    #[test]
    fn single_lens_cycle_tab_walks_the_library_skipping_scene_and_input() {
        // In single-lens, Ctrl+←/→ (CycleTab) swaps the one center lens
        // through the surface library — scene backdrops and the foot input
        // are not lenses and are skipped.
        let session = session_with_surface(serde_json::json!({
            "name": "lens-test",
            "tiling":{"mode":"single_lens"},
            "library": [
                { "group": "Lenses", "views": ["view:a", "view:scene", "view:input", "view:b"] }
            ],
            "views": {
                "view:a": { "widget": "rows", "sources": { "default": { "ref": "service:x", "params": {}, "collection": "rows" } } },
                "view:scene": { "widget": "scene" },
                "view:input": { "widget": "text", "input": { "id": "line" } },
                "view:b": { "widget": "rows", "sources": { "default": { "ref": "service:x", "params": {}, "collection": "rows" } } }
            }
        }));
        let mut core = test_support_core(session);
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);

        // The lens-able library excludes the scene backdrop and the input.
        assert_eq!(
            core.lens_library(),
            vec!["view:a".to_string(), "view:b".to_string()]
        );

        let open = |core: &mut RyeOsCore, view_ref: &str| {
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::Activate {
                    intent: RyeOsUiIntent::OpenView {
                        view: ViewSpec {
                            view_ref: view_ref.to_string(),
                        },
                    },
                },
            });
            settle_structure(core);
        };
        let cycle = |core: &mut RyeOsCore| {
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::Activate {
                    intent: RyeOsUiIntent::CycleTab {
                        direction: RyeOsStackMoveDirection::Down,
                    },
                },
            });
            settle_structure(core);
        };

        open(&mut core, "view:a");
        cycle(&mut core);
        assert!(
            matches!(core.active_view_set().unwrap().focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:b"),
            "cycle forward moves to the next lens"
        );
        assert_eq!(
            core.active_view_set().unwrap().tile_ids().len(),
            1,
            "cycling stays single-lens"
        );

        cycle(&mut core);
        assert!(
            matches!(core.active_view_set().unwrap().focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:a"),
            "cycle wraps back to the first lens"
        );
    }

    #[test]
    fn open_new_view_allows_duplicate_view_set_tiles() {
        let mut core = structural_core();
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);
        let before = core.active_view_set().unwrap().tile_ids().len();
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });

        let effects = settle_structure(&mut core);
        let item_tile_count = core.active_view_set().unwrap()
            .tiles
            .values()
            .filter(|tile| {
                matches!(&tile.view, ViewSpec { view_ref } if view_ref == "view:ryeos/items/space")
            })
            .count();
        assert_eq!(core.active_view_set().unwrap().tile_ids().len(), before + 1);
        assert_eq!(item_tile_count, 2);
        assert!(
            core.ui
                .motion
                .iter()
                .any(|event| matches!(event, RyeOsMotionEventVm::TileSplit { .. }))
        );
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::FetchSource { .. })
        ));
    }

    #[test]
    fn close_view_keeps_drafts_for_previous_subjects() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"Conversation draft fixture", "tiles":["view:test/conversation"],
            "views":{"view:test/conversation":{"widget":"text","input":{"id":"message","local_filter":true}}}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let tile_id = core.active_view_set().unwrap().focused_tile;
        let instance = core.active_surface_view_sets().unwrap()[0].tiles[&tile_id]
            .instance_key
            .clone();
        let mut key = crate::ui::model::InputBufferKey::new(
            instance.clone(),
            "view:test/conversation",
            "message",
            core.binding_attachment_for_instance(&instance).unwrap(),
        );
        key.target_scope = Some("chain:previous-subject".into());
        core.input_buffers.insert(
            key.storage_key(),
            crate::ui::model::RyeOsInputState {
                text: "retain previous draft".into(),
                ..Default::default()
            },
        );
        assert!(!core.close_tile_or_empty(tile_id));
        assert!(
            core.active_surface_view_sets().unwrap()[0]
                .tiles
                .contains_key(&tile_id)
        );
        assert_eq!(
            core.input_buffers[&key.storage_key()].text,
            "retain previous draft"
        );
        core.input_buffers
            .get_mut(&key.storage_key())
            .unwrap()
            .text
            .clear();
        assert!(
            !core.close_tile_or_empty(tile_id),
            "close is provisional until ACK"
        );
        settle_structure(&mut core);
        assert!(!core.active_view_set().unwrap().tiles.contains_key(&tile_id));
    }

    #[test]
    fn close_tile_closes_target_tile() {
        let mut core = structural_core();
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:test/files".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);
        let tile_id = core.active_view_set().unwrap().tile_ids()[1];
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::CloseTile {
                    tile_id: tile_id.0.to_string(),
                },
            },
        });

        assert!(core.active_view_set().unwrap().tiles.contains_key(&tile_id));
        settle_structure(&mut core);
        assert!(!core.active_view_set().unwrap().tiles.contains_key(&tile_id));
        assert!(
            !core
                .active_view_set()
                .unwrap()
                .tile_ids()
                .contains(&tile_id)
        );
        assert!(core.ui.motion.iter().any(|event| matches!(
            event,
            RyeOsMotionEventVm::TileExit { tile_id: closed } if closed == &tile_id.0.to_string()
        )));
    }

    #[test]
    fn closing_one_duplicate_view_fences_its_late_result_without_retargeting_sources() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"Duplicate sources", "tiles":["view:test/shared","view:test/shared"],
            "views":{"view:test/shared":{"widget":"rows","sources":{"default":{"ref":"service:test/source","params":{},"collection":"rows"}}}}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let view_ref = "view:test/shared";
        let closing_tile = core.active_view_set().unwrap().tile_ids()[0];
        let closing_instance = core.active_view_set().unwrap().tiles[&closing_tile]
            .instance_key
            .clone();
        let surviving_tile = core.active_view_set().unwrap().tile_ids()[1];
        let surviving_instance = core.active_view_set().unwrap().tiles[&surviving_tile]
            .instance_key
            .clone();
        let closing_keys = [
            crate::ui::source_key::RyeOsSourceInstanceKey::named(
                closing_instance.clone(),
                "default",
            )
            .encode(),
            crate::ui::source_key::RyeOsSourceInstanceKey::named(
                closing_instance.clone(),
                "section-3",
            )
            .encode(),
            crate::ui::source_key::RyeOsSourceInstanceKey::mention(
                closing_instance.clone(),
                "query",
            )
            .encode(),
            crate::ui::source_key::RyeOsSourceInstanceKey::completion(
                closing_instance.clone(),
                "query",
            )
            .encode(),
        ];
        let surviving_key = crate::ui::source_key::RyeOsSourceInstanceKey::named(
            surviving_instance.clone(),
            "default",
        )
        .encode();

        for key in closing_keys.iter().chain(std::iter::once(&surviving_key)) {
            core.data
                .sources
                .insert(key.clone(), serde_json::json!({ "key": key }));
            core.data.source_errors.insert(key.clone(), "error".into());
            core.data.source_epoch.insert(key.clone(), 4);
            core.data.source_stored_epoch.insert(key.clone(), 3);
            core.data.source_floor.insert(key.clone(), 2);
        }
        core.data.timeline_sources.insert(
            closing_keys[0].clone(),
            crate::ui::model::RyeOsTimelineSourceCache {
                entries: Vec::new(),
                indents: Vec::new(),
                sources: Vec::new(),
                arrivals: Vec::new(),
                sections: Vec::new(),
                collapsible: std::collections::BTreeSet::new(),
            },
        );
        core.deferred_source_fetches.insert(
            closing_keys[2].clone(),
            crate::ui::model::DeferredSourceFetch {
                view_ref: "view:test/source".to_string(),
                channel: "default".to_string(),
                params: serde_json::json!({}),
            },
        );

        let late_effect = core
            .emit_fetch_source_for_instance(closing_instance.clone(), view_ref)
            .into_iter()
            .find(|effect| {
                matches!(&effect.kind,
                    RyeOsEffectKind::FetchSource { tile_id, .. } if tile_id == &closing_keys[0]
                )
            })
            .expect("actual closed-mount request is pending");
        let original_epoch = core.data.source_epoch[&closing_keys[0]];
        let original_observation = core.data.sources.get(&closing_keys[0]).cloned();

        assert!(
            !core.close_tile_or_empty(closing_tile),
            "close is provisional until ACK"
        );
        let producer = core
            .presentation_operations
            .producer
            .clone()
            .expect("fixture writer claimed");
        settle_pending_prefix(&mut core, &producer);
        assert!(
            core.active_view_set()
                .unwrap()
                .tiles
                .contains_key(&surviving_tile)
        );
        assert!(
            core.presentation
                .as_ref()
                .unwrap()
                .mount(&closing_instance)
                .is_none()
        );
        assert!(core.data.sources.contains_key(&surviving_key));
        assert!(!core.selection_attachments.contains_key(&closing_instance));
        assert!(core.selection_attachments.contains_key(&surviving_instance));
        // The still-mounted sibling is refreshed after completion, so an old
        // error need not remain; its observation and request owner must remain.
        assert!(core.data.source_epoch.contains_key(&surviving_key));
        assert!(core.data.source_stored_epoch.contains_key(&surviving_key));
        assert!(core.data.source_floor.contains_key(&surviving_key));
        assert!(core.pending_effects.contains_key(&late_effect.id));
        assert_eq!(core.data.source_epoch[&closing_keys[0]], original_epoch);
        core.apply_effect_result(crate::ui::effect::RyeOsEffectResult {
            id: late_effect.id,
            ok: true,
            kind: crate::ui::effect::RyeOsEffectResultKind::SourceData,
            data: Some(serde_json::json!({"late_closed_mount":true})),
            error: None,
        });
        assert!(!core.pending_effects.contains_key(&late_effect.id));
        assert_eq!(
            core.data.sources.get(&closing_keys[0]).cloned(),
            original_observation
        );
        assert!(
            core.presentation
                .as_ref()
                .unwrap()
                .mount(&closing_instance)
                .is_none()
        );
        assert!(core.pending_effects.values().any(|effect| {
            matches!(effect, RyeOsEffectKind::FetchSource { tile_id, .. } if tile_id == &surviving_key)
        }));
    }

    #[test]
    fn closing_last_app_tile_empties_center() {
        let mut core = structural_core();
        assert!(!core.active_view_set().unwrap().center_is_empty());

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::CloseFocused,
            },
        });

        assert!(!core.active_view_set().unwrap().center_is_empty());
        settle_structure(&mut core);
        assert!(core.active_view_set().unwrap().center_is_empty());
        // The last-tile close emits a tile-exit motion (no home mode).
        assert!(
            core.ui
                .motion
                .iter()
                .any(|event| matches!(event, RyeOsMotionEventVm::TileExit { .. }))
        );
    }

    #[test]
    fn opening_views_preserves_existing_split_instead_of_reapplying_recipe() {
        let mut core = structural_core();
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:test/files".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);

        let Some(crate::layout::LayoutTree::Split {
            axis,
            first,
            second,
            ..
        }) = core.active_view_set().unwrap().layout()
        else {
            panic!("opening alongside should split root");
        };
        assert_eq!(axis, crate::layout::SplitAxis::Horizontal);
        // Existing geometry stays put; the last placement is split again.
        assert!(matches!(
            first.as_ref(),
            crate::layout::LayoutTree::Group { .. }
        ));
        let crate::layout::LayoutTree::Split { axis, .. } = second.as_ref() else {
            panic!("last region should split");
        };
        assert_eq!(*axis, crate::layout::SplitAxis::Horizontal);
    }

    #[test]
    fn view_set_close_is_identity_addressed_and_preserves_input() {
        let mut core = structural_core();
        let first = core.active_surface_view_sets().unwrap()[0].id;
        let draft_tile = core.active_view_set().unwrap().focused_tile;
        let draft_instance = core.active_view_set().unwrap().tiles[&draft_tile]
            .instance_key
            .clone();
        let draft_key = crate::ui::model::InputBufferKey::new(
            draft_instance.clone(),
            "view:test/draft",
            "line",
            core.binding_attachment_for_instance(&draft_instance)
                .unwrap(),
        )
        .storage_key();
        core.new_view_set();
        settle_structure(&mut core);
        let second = core.active_surface_view_sets().unwrap()[1].id;
        core.new_view_set();
        settle_structure(&mut core);
        let third = core.active_surface_view_sets().unwrap()[2].id;
        assert!(core.close_view_set(second).is_empty());
        assert_eq!(
            core.active_surface_view_sets().unwrap().len(),
            3,
            "close remains provisional until ACK"
        );
        settle_structure(&mut core);
        assert_eq!(core.active_view_set().unwrap().id, third);
        assert!(core.close_view_set(second).is_empty());
        assert_eq!(core.active_surface_view_sets().unwrap().len(), 2);
        core.input_buffers.insert(
            draft_key.clone(),
            crate::ui::model::RyeOsInputState {
                text: "unsent".into(),
                ..Default::default()
            },
        );
        assert!(core.close_view_set(first).is_empty());
        assert_eq!(core.active_surface_view_sets().unwrap().len(), 2);
        assert_eq!(core.input_buffers[&draft_key].text, "unsent");
        core.input_buffers.get_mut(&draft_key).unwrap().text.clear();
        core.close_view_set(third);
        settle_structure(&mut core);
        assert_eq!(core.active_view_set().unwrap().id, first);
        core.close_view_set(first);
        settle_structure(&mut core);
        assert_eq!(
            core.active_surface_view_sets().unwrap().len(),
            0,
            "closing the final tab preserves the Surface without inventing a replacement"
        );
    }

    #[test]
    fn view_set_close_refuses_an_external_follow_attachment() {
        let mut core = subject_core();
        let followed = core.active_surface_view_sets().unwrap()[0].id;
        core.new_view_set();
        settle_structure(&mut core);
        core.add_center_tile(ViewSpec::bound("view:test/services"));
        settle_structure(&mut core);
        let tile = core.active_view_set().unwrap().focused_tile;
        let instance = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();
        core.prepare_subject_operation(
            instance,
            crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: followed,
            },
        );
        settle_structure(&mut core);

        assert!(pending_prefix(&core).is_empty());
        assert!(core.close_view_set(followed).is_empty());
        assert!(
            pending_prefix(&core).is_empty(),
            "refusal must not prepare CloseTab"
        );
        assert_eq!(core.active_surface_view_sets().unwrap().len(), 2);
        assert!(core.ui.notices.iter().any(|notice| {
            notice
                .message
                .contains("Resolve incoming tab relationships before closing.")
        }));
    }

    #[test]
    fn view_set_rename_bounds_labels_without_changing_identity() {
        let mut core = structural_core();
        let id = core.active_surface_view_sets().unwrap()[0].id;
        core.rename_view_set(id, "  My work  ");
        assert_ne!(core.active_surface_view_sets().unwrap()[0].title, "My work");
        settle_structure(&mut core);
        assert_eq!(core.active_surface_view_sets().unwrap()[0].title, "My work");
        for invalid in ["".to_owned(), "bad\nlabel".to_owned(), "x".repeat(129)] {
            core.rename_view_set(id, &invalid);
            assert_eq!(core.active_surface_view_sets().unwrap()[0].title, "My work");
        }
        assert_eq!(core.active_surface_view_sets().unwrap()[0].id, id);
    }

    #[test]
    fn duplicate_view_set_preserves_composition_without_aliasing_runtime_state() {
        let mut session = session_with_surface(serde_json::json!({
            "name":"Development", "tiles":["view:test/services","view:test/files"],
            "views":{
                "view:test/services":{"widget":"rows","input":{"id":"line","submit":"route"},
                    "sources":{"default":{"ref":"service:test/services","params":{"thread":"@facet:selection.work.thread"}}}},
                "view:test/files":{"widget":"rows"}
            }
        }));
        session
            .binding_attachments
            .iter_mut()
            .find(|binding| binding.binding_attachment_id == "fixture-shared-attachment")
            .unwrap()
            .presentation
            .views
            .insert(
                "view:ryeos/input".into(),
                serde_json::json!({
                    "widget":"rows", "sources":{"default":{"ref":"service:test/dock",
                        "params":{"thread":"@facet:selection.work.thread"}}}
                }),
            );
        let mut core = test_support_core(session);
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let source_instance = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .find(|tile| tile.view.view_ref == "view:test/services")
            .unwrap()
            .instance_key
            .clone();
        let source_draft = crate::ui::model::InputBufferKey::new(
            source_instance.clone(),
            "view:test/services",
            "line",
            core.binding_attachment_for_instance(&source_instance)
                .unwrap(),
        )
        .storage_key();
        core.input_buffers.insert(
            source_draft.clone(),
            crate::ui::model::RyeOsInputState {
                text: "private draft".into(),
                ..Default::default()
            },
        );
        let source_id = core.active_surface_view_sets().unwrap()[0].id;
        let source_instances: std::collections::BTreeSet<_> =
            core.active_surface_view_sets().unwrap()[0]
                .tiles
                .values()
                .map(|tile| tile.instance_key.clone())
                .collect();
        let source_views: std::collections::BTreeSet<_> = core.active_surface_view_sets().unwrap()
            [0]
        .tiles
        .values()
        .map(|tile| tile.view.view_ref.clone())
        .collect();
        let unresolved_source = source_instance.clone();
        core.prepare_subject_operation(
            unresolved_source,
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );
        settle_structure(&mut core);
        let source_dock = core
            .shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom)
            .unwrap();
        core.prepare_subject_operation(
            source_dock.clone(),
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_slot_bottom".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );
        settle_structure(&mut core);
        assert!(core.duplicate_view_set(source_id).is_empty());
        let effects = settle_structure(&mut core);

        let duplicate = &core.active_view_set().unwrap();
        assert_ne!(duplicate.id, source_id);
        assert_eq!(duplicate.title, "Development copy");
        assert_eq!(duplicate.tile_ids().len(), 2);
        assert!(duplicate.tiles.values().all(|tile| {
            core.retained_lens_frames
                .get(&tile.instance_key)
                .is_none_or(|frames| frames.is_empty())
        }));
        let duplicate_instances: std::collections::BTreeSet<_> = duplicate
            .tiles
            .values()
            .map(|tile| tile.instance_key.clone())
            .collect();
        assert!(source_instances.is_disjoint(&duplicate_instances));
        assert_eq!(core.input_buffers[&source_draft].text, "private draft");
        assert!(
            core.input_buffers
                .keys()
                .all(|key| duplicate_instances.iter().all(|instance| {
                    !crate::ui::model::InputBufferKey::storage_key_belongs_to(key, instance)
                }))
        );
        assert_eq!(
            duplicate
                .tiles
                .values()
                .map(|tile| tile.view.view_ref.clone())
                .collect::<std::collections::BTreeSet<_>>(),
            source_views
        );
        assert_eq!(
            duplicate_instances
                .iter()
                .filter(|instance| matches!(
                    core.selection_attachments.get(*instance),
                    Some(crate::ui::attachment::SelectionAttachment::RequiredSubject { .. })
                ))
                .count(),
            1
        );
        let duplicate_dock = core
            .shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom)
            .unwrap();
        assert_eq!(
            duplicate_dock, source_dock,
            "duplicating a tab never duplicates shared mounts"
        );
        assert!(matches!(
            core.selection_attachments.get(&duplicate_dock),
            Some(crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input,
                facets,
            }) if input == "subject_slot_bottom" && facets == &["selection.work.thread"]
        ));
        assert!(effects.iter().all(|effect| {
            let crate::ui::effect::RyeOsEffectKind::FetchSource { tile_id, .. } = &effect.kind
            else {
                return true;
            };
            !crate::ui::source_key::RyeOsSourceInstanceKey::decode(tile_id)
                .is_some_and(|key| key.belongs_to(&duplicate_dock))
        }));
    }

    #[test]
    fn duplicate_nested_tab_requires_two_acks_and_fresh_subjects_and_mounts() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"nested-copy", "tiles":["view:test/subject","view:test/plain"],
            "views":{
                "view:test/subject":{"widget":"rows","sources":{"default":{"ref":"service:test/subject","params":{"thread":"@facet:selection.work.thread"}}}},
                "view:test/plain":{"widget":"text"}
            }
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let source = core.active_view_set().unwrap().id;
        let mut source_keys = core
            .active_view_set()
            .unwrap()
            .tile_ids()
            .iter()
            .map(|id| {
                core.active_view_set().unwrap().tiles[id]
                    .instance_key
                    .clone()
            })
            .collect::<Vec<_>>();
        source_keys.sort_by_key(|key| core.mounted_view_ref(key) != Some("view:test/subject"));
        core.seat.append_facet(
            crate::ui::seat::selection_storage_key(source, "selection.work").unwrap(),
            serde_json::json!({"thread":"private-thread"}),
        );
        settle_pending_prefix(&mut core, &producer);
        let pinned = core.capture_pinned_selection(&source_keys[0]).unwrap();
        core.prepare_subject_operation(source_keys[0].clone(), pinned.clone());
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(
            core.selection_attachments.get(&source_keys[0]),
            Some(&pinned)
        );
        let draft = crate::ui::model::InputBufferKey::new(
            source_keys[0].clone(),
            "view:test/subject",
            "query",
            core.binding_attachment_for_instance(&source_keys[0])
                .unwrap(),
        )
        .storage_key();
        core.input_buffers.insert(
            draft.clone(),
            crate::ui::model::RyeOsInputState {
                text: "private draft".into(),
                ..Default::default()
            },
        );
        let before = core
            .capture_view_set_template_at(
                source,
                "comparison".into(),
                "Comparison".into(),
                None,
                &std::collections::BTreeMap::new(),
            )
            .unwrap();
        assert!(core.duplicate_view_set(source).is_empty());
        assert_eq!(core.active_surface().unwrap().view_sets().len(), 1);
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert_eq!(
            core.active_surface().unwrap().view_sets().len(),
            1,
            "first ACK only retains completion"
        );
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        let copy = core.active_view_set().unwrap();
        assert_ne!(copy.id, source);
        let mut copy_keys = copy
            .tile_ids()
            .iter()
            .map(|id| copy.tiles[id].instance_key.clone())
            .collect::<Vec<_>>();
        copy_keys.sort_by_key(|key| core.mounted_view_ref(key) != Some("view:test/subject"));
        assert!(copy_keys.iter().all(|key| !source_keys.contains(key)));
        let after = core
            .capture_view_set_template_at(
                copy.id,
                "comparison".into(),
                "Comparison".into(),
                None,
                &std::collections::BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(
            before.composition.root, after.composition.root,
            "nested layout and ordered Views remain exact"
        );
        assert_eq!(before.relationships, after.relationships);
        assert!(matches!(
            core.selection_attachments.get(&copy_keys[0]),
            Some(crate::ui::attachment::SelectionAttachment::RequiredSubject { .. })
        ));
        assert_eq!(core.input_buffers[&draft].text, "private draft");
        assert!(core.input_buffers.keys().all(|key| {
            copy_keys
                .iter()
                .all(|mount| !crate::ui::model::InputBufferKey::storage_key_belongs_to(key, mount))
        }));
    }

    #[test]
    fn duplicate_refuses_external_follow_without_explicit_saved_target() {
        let mut core = subject_core();
        let source = core.active_view_set().unwrap().id;
        let mount = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        core.new_view_set();
        settle_structure(&mut core);
        let external = core.active_view_set().unwrap().id;
        core.switch_view_set_tab(0);
        settle_structure(&mut core);
        core.prepare_subject_operation(
            mount,
            crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: external,
            },
        );
        settle_structure(&mut core);
        let before = core.seat.events().to_vec();
        assert!(core.duplicate_view_set(source).is_empty());
        assert_eq!(
            core.seat.events(),
            before.as_slice(),
            "copy cannot silently reinterpret an external FollowSet"
        );
        assert_eq!(core.active_surface().unwrap().view_sets().len(), 2);
    }

    #[test]
    fn view_set_switch_preserves_one_shared_dock_draft_and_focus() {
        use crate::ui::model::{RyeOsDockEdge, RyeOsFocusTarget};
        let mut core = test_support_core(session());
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core.focused_input = core.shared_dock_instance_key(RyeOsDockEdge::Bottom);
        let original_focus = core.focus_target();
        assert_eq!(
            original_focus,
            Some(RyeOsFocusTarget::MountedView {
                instance_key: core
                    .shared_dock_instance_key(RyeOsDockEdge::Bottom)
                    .unwrap()
            })
        );
        core.focused_input_buffer_mut()
            .expect("authored input")
            .text = "unsent original".into();
        core.presentation
            .as_mut()
            .unwrap()
            .dock_mut("bottom")
            .unwrap()
            .size = 11;

        core.new_view_set();
        settle_structure(&mut core);
        assert_eq!(core.focus_target(), original_focus);
        assert_eq!(core.focused_input_buffer().unwrap().text, "unsent original");
        assert_eq!(core.input_buffers.len(), 1);

        core.switch_view_set_tab(0);
        settle_structure(&mut core);
        assert_eq!(core.focus_target(), original_focus);
        assert_eq!(core.focused_input_buffer().unwrap().text, "unsent original");
        assert_eq!(
            core.presentation.as_ref().unwrap().docks()["bottom"].size,
            11
        );
    }

    #[test]
    fn explicit_view_focus_replaces_surviving_shared_input_focus() {
        let mut core = test_support_core(session());
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let shared = core
            .shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom)
            .unwrap();
        core.focused_input = Some(shared.clone());
        let draft = core.focused_input_instance().unwrap().0.storage_key();
        core.focused_input_buffer_mut().unwrap().text = "shared unsent draft".into();
        let destination = core.active_view_set().unwrap().tiles
            [&core.active_view_set().unwrap().focused_tile]
            .instance_key
            .clone();

        // The retained center focus already names this tile. The explicit
        // operation must still displace transient shared-dock input focus.
        core.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::FocusView {
                mount: destination.clone(),
            },
        );
        assert_eq!(core.focused_input.as_ref(), Some(&shared));
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.focused_view_instance_key(), Some(destination));
        assert_ne!(core.focused_input.as_ref(), Some(&shared));
        assert_eq!(core.input_buffers[&draft].text, "shared unsent draft");
        assert!(
            core.presentation
                .as_ref()
                .unwrap()
                .mount(&shared)
                .unwrap()
                .visible()
        );
    }

    #[test]
    fn view_set_tabs_keep_independent_tile_layouts() {
        let mut core = structural_core();
        core.new_view_set();
        settle_structure(&mut core);
        core.switch_view_set_tab(0);
        settle_structure(&mut core);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:test/files".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);
        let first_tab_tiles = core.active_view_set().unwrap().tile_ids().len();

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::SwitchTab { index: 1 },
            },
        });
        settle_structure(&mut core);

        assert_eq!(
            core.active_view_set().unwrap().id,
            core.active_surface_view_sets().unwrap()[1].id
        );
        // Fresh tabs start at home: an empty center.
        assert_eq!(core.active_view_set().unwrap().tile_ids().len(), 0);

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/files".to_string(),
                    },
                },
            },
        });
        settle_structure(&mut core);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });

        settle_structure(&mut core);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::SwitchTab { index: 0 },
            },
        });

        settle_structure(&mut core);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::SwitchTab { index: 1 },
            },
        });
        let effects = settle_structure(&mut core);

        assert_eq!(
            core.active_view_set().unwrap().id,
            core.active_surface_view_sets().unwrap()[1].id
        );
        assert_eq!(
            core.active_surface_view_sets().unwrap()[0].tile_ids().len(),
            first_tab_tiles
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchSource { .. }))
        );
    }

    #[test]
    fn invalid_close_tile_does_not_close_focused_tile() {
        let mut core = test_support_core(session());
        seed_view(&mut core, "view:test/services");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        let focused = core.active_view_set().unwrap().focused_tile;
        let count = core.active_view_set().unwrap().tile_ids().len();

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::CloseTile {
                    tile_id: "999".to_string(),
                },
            },
        });

        assert_eq!(core.active_view_set().unwrap().focused_tile, focused);
        assert_eq!(core.active_view_set().unwrap().tile_ids().len(), count);
        assert!(core.active_view_set().unwrap().tiles.contains_key(&focused));
    }
}
