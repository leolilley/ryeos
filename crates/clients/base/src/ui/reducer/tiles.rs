use super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::event::RyeOsStackMoveDirection;
use super::model::RyeOsCore;
use super::view_model::{RyeOsMotionEventVm, RyeOsSplitAxisVm};
use super::wrap_index;
use crate::ids::TileId;
use crate::surface::ArrangeSpec;
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
        use sha2::{Digest, Sha256};
        let session = self.data.session.as_ref();
        let surface_attachment = self.binding_attachment(&self.surface_attachment_id);
        let bytes = serde_json::to_vec(&(
            session.map(|s| &s.session_id),
            surface_attachment.map(|attachment| {
                (
                    &attachment.binding_attachment_id,
                    attachment.binding_generation,
                    &attachment.binding_digest,
                )
            }),
            self.active_view_set,
            self.view_sets[self.active_view_set].id,
            &self.view_sets[self.active_view_set].root,
        ))
        .expect("layout coordinate serializes");
        format!("{:x}", Sha256::digest(bytes))
    }

    pub(crate) fn new_view_set(&mut self) -> Vec<RyeOsEffect> {
        if self.view_sets.len() >= crate::surface::view_sets::MAX_VIEW_SETS {
            return Vec::new();
        }
        let mut view_set = crate::view_set::ViewSet::from_tiling(
            self.view_sets[self.active_view_set].tiling.clone(),
            Vec::new(),
        );
        view_set.title = format!("View set {}", self.view_sets.len() + 1);
        let attachment_id = self
            .insertion_attachment_id(self.view_sets[self.active_view_set].id)
            .map(str::to_string);
        let view_set_id = view_set.id;
        self.view_sets.push(view_set);
        if let Some(attachment_id) = attachment_id {
            self.view_set_insertion_attachments
                .insert(view_set_id, attachment_id);
        }
        self.switch_view_set_tab(self.view_sets.len() - 1)
    }

    pub(crate) fn rename_view_set(&mut self, id: crate::ids::ViewSetId, title: &str) {
        let title = title.trim();
        if title.is_empty()
            || title.len() > crate::surface::view_sets::MAX_VIEW_SET_LABEL_BYTES
            || title.chars().any(char::is_control)
        {
            return;
        }
        if let Some(view_set) = self.view_sets.iter_mut().find(|view_set| view_set.id == id) {
            if view_set.title != title {
                view_set.title = title.to_owned();
                self.bump_generation();
            }
        }
    }

    /// Duplicate presentation composition without duplicating mounted
    /// identity, transient observations or drafts. The new set follows the
    /// same authored views and arrangement, but every mounted view receives a
    /// fresh instance coordinate and therefore an independent source, input
    /// and local-state lifetime.
    pub(crate) fn duplicate_view_set(&mut self, id: crate::ids::ViewSetId) -> Vec<RyeOsEffect> {
        if self.view_sets.len() >= crate::surface::view_sets::MAX_VIEW_SETS {
            return Vec::new();
        }
        let Some(source_index) = self.view_sets.iter().position(|view_set| view_set.id == id)
        else {
            return Vec::new();
        };
        let source = &self.view_sets[source_index];
        let mut duplicate = source.duplicate_composition();
        duplicate.title = format!("{} copy", source.title);
        let source_tiles = source
            .tile_ids()
            .into_iter()
            .filter_map(|tile_id| {
                source
                    .tiles
                    .get(&tile_id)
                    .map(|tile| tile.instance_key.clone())
            })
            .collect::<Vec<_>>();
        let duplicate_tiles = duplicate
            .tile_ids()
            .into_iter()
            .filter_map(|tile_id| {
                duplicate
                    .tiles
                    .get(&tile_id)
                    .map(|tile| tile.instance_key.clone())
            })
            .collect::<Vec<_>>();
        let duplicate_id = duplicate.id;
        let insertion = self.insertion_attachment_id(id).map(str::to_string);
        self.view_sets.push(duplicate);
        if let Some(insertion) = insertion {
            self.view_set_insertion_attachments
                .insert(duplicate_id, insertion);
        }
        for (source, duplicate) in source_tiles.into_iter().zip(duplicate_tiles) {
            if let Some(attachment_id) = self.instance_binding_attachments.get(&source).cloned() {
                self.instance_binding_attachments
                    .insert(duplicate.clone(), attachment_id);
            }
            if let Some(
                attachment @ super::super::attachment::SelectionAttachment::RequiredSubject {
                    ..
                },
            ) = self.selection_attachments.get(&source).cloned()
            {
                self.selection_attachments.insert(duplicate, attachment);
            }
        }
        for edge in [
            super::model::RyeOsDockEdge::Top,
            super::model::RyeOsDockEdge::Bottom,
            super::model::RyeOsDockEdge::Left,
            super::model::RyeOsDockEdge::Right,
        ] {
            let source = super::model::dock_view_instance_key(id, edge);
            let duplicate = super::model::dock_view_instance_key(duplicate_id, edge);
            if let Some(attachment_id) = self.instance_binding_attachments.get(&source).cloned() {
                self.instance_binding_attachments
                    .insert(duplicate.clone(), attachment_id);
            }
            if let Some(
                attachment @ super::super::attachment::SelectionAttachment::RequiredSubject {
                    ..
                },
            ) = self.selection_attachments.get(&source).cloned()
            {
                self.selection_attachments.insert(duplicate, attachment);
            }
        }
        self.switch_view_set_tab(self.view_sets.len() - 1)
    }

    /// Closing presentation never terminates an execution. Stable identity is
    /// essential here: a delayed close must not target a newly shifted index.
    pub(crate) fn close_view_set(&mut self, id: crate::ids::ViewSetId) -> Vec<RyeOsEffect> {
        let Some(index) = self.view_sets.iter().position(|view_set| view_set.id == id) else {
            return Vec::new();
        };
        if self.view_sets.len() == 1 {
            return Vec::new();
        }
        let externally_followed =
            self.selection_attachments
                .iter()
                .any(|(instance, attachment)| {
                    matches!(
                        attachment,
                        super::super::attachment::SelectionAttachment::FollowViewSet { view_set_id }
                            if *view_set_id == id
                    ) && self
                        .view_set_index_for_instance(instance)
                        .is_some_and(|owner| self.view_sets[owner].id != id)
                });
        if externally_followed {
            self.notice(
                "ViewSet is still followed by a view in another set. Reattach that view before closing.",
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        }
        if self.view_sets[index]
            .input_buffers
            .values()
            .any(|input| !input.text.is_empty())
        {
            self.notice(
                "ViewSet has current input. Clear it explicitly before closing.",
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let mut closed_instances = self.view_sets[index]
            .tiles
            .values()
            .map(|tile| tile.instance_key.clone())
            .chain(self.view_sets[index].dock_local.keys().cloned())
            .collect::<Vec<_>>();
        for edge in [
            super::model::RyeOsDockEdge::Top,
            super::model::RyeOsDockEdge::Bottom,
            super::model::RyeOsDockEdge::Left,
            super::model::RyeOsDockEdge::Right,
        ] {
            if self.view_sets[index].docks.slot(edge).is_some() {
                closed_instances.push(super::model::dock_view_instance_key(id, edge));
            }
        }
        for instance in &closed_instances {
            self.invalidate_view_sources(instance);
            self.selection_attachments.remove(instance);
            self.instance_binding_attachments.remove(instance);
        }
        self.view_set_insertion_attachments.remove(&id);
        self.view_set_template_ids.remove(&id);
        let previous_active = self.view_sets[self.active_view_set].id;
        self.view_sets.remove(index);
        self.active_view_set = self
            .view_sets
            .iter()
            .position(|view_set| view_set.id == previous_active)
            .unwrap_or(index.min(self.view_sets.len() - 1));
        if previous_active == id {
            self.refresh_view_set_sources()
        } else {
            self.bump_generation();
            Vec::new()
        }
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
        if self.view_sets[self.active_view_set].tiling.mode
            == crate::surface::TilingModeSpec::SingleLens
        {
            return self.cycle_lens(delta);
        }
        let len = self.view_sets.len().max(1);
        let next = wrap_index(self.active_view_set, delta, len);
        self.switch_view_set_tab(next)
    }

    /// Whether a view works as a center lens: a real bound view that is
    /// neither a scene backdrop nor a pure input line. An `input` block
    /// alone does not disqualify — the thread history views carry live
    /// FILTER inputs and are the canonical center lenses; only a view
    /// with no content of its own (no source, no sections — the foot
    /// chat line) is input-only.
    fn lensable(&self, view_ref: &str) -> bool {
        self.binding_for_insertion(self.view_sets[self.active_view_set].id, view_ref)
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
            .insertion_attachment_id(self.view_sets[self.active_view_set].id)
            .and_then(|id| self.binding_attachment(id))
            .map(|attachment| &attachment.effective_surface)
            .and_then(|surface| surface.get("library"))
            .and_then(|library| library.as_array())
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
        for entry in entries {
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
        let Some(attachment_id) =
            self.insertion_attachment_id(self.view_sets[self.active_view_set].id)
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
            .insertion_attachment_id(self.view_sets[self.active_view_set].id)
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
        let current = self.view_sets[self.active_view_set]
            .focused_view()
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
        if index >= self.view_sets.len() || index == self.active_view_set {
            return Vec::new();
        }
        self.active_view_set = index;
        self.refresh_view_set_sources()
    }

    pub(crate) fn refresh_view_set_sources(&mut self) -> Vec<RyeOsEffect> {
        self.data.tile_items.clear();
        self.data.tile_files.clear();
        self.data.tile_file_space.clear();
        self.data.sources.clear();
        self.data.source_errors.clear();
        self.data.source_epoch.clear();
        self.data.source_stored_epoch.clear();
        self.data.source_floor.clear();
        self.data.source_subject_fingerprint.clear();
        self.data.timeline_sources.clear();
        self.data.field_sources.clear();
        self.data.field_projections.borrow_mut().clear();
        self.deferred_source_fetches.clear();
        self.pending_effects
            .retain(|_, kind| !matches!(kind, RyeOsEffectKind::FetchSource { .. }));
        self.push_motion(RyeOsMotionEventVm::FocusChanged {
            tile_id: self.view_sets[self.active_view_set]
                .focused_tile
                .0
                .to_string(),
        });
        self.push_motion(RyeOsMotionEventVm::TabChanged {
            view_set_number: self.active_view_set + 1,
        });
        self.bump_generation();
        self.initial_effects()
    }

    pub(crate) fn set_tile_cursor(&mut self, tile_id: TileId, index: usize) -> bool {
        let Some(tile) = self.view_sets[self.active_view_set].tiles.get_mut(&tile_id) else {
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
        if let Some(tile_id) = instance_key.view_set_tile_id() {
            return self.set_tile_cursor(tile_id, index);
        }
        let Some(local) = self.view_sets[self.active_view_set]
            .dock_local
            .get_mut(instance_key)
        else {
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
        let local = if let Some(tile_id) = instance_key.view_set_tile_id() {
            let Some(tile) = self.view_sets[self.active_view_set].tiles.get_mut(&tile_id) else {
                return false;
            };
            if tile.instance_key != *instance_key {
                return false;
            }
            &mut tile.local
        } else {
            let Some(local) = self.view_sets[self.active_view_set]
                .dock_local
                .get_mut(instance_key)
            else {
                return false;
            };
            local
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
        let Some(tile) = self.view_sets[self.active_view_set].tiles.get_mut(&tile_id) else {
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
        if let Some(tile_id) = instance_key.view_set_tile_id() {
            return self.set_tile_fold(tile_id, section, collapsed);
        }
        let Some(local) = self.view_sets[self.active_view_set]
            .dock_local
            .get_mut(instance_key)
        else {
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
        let Some(attachment_id) = self
            .insertion_attachment_id(self.view_sets[self.active_view_set].id)
            .map(str::to_string)
        else {
            return Vec::new();
        };
        self.open_view_under_binding(view, &attachment_id)
    }

    /// Open a view under one explicit retained authority. Reuse is scoped to
    /// that authority: an equal ref mounted from another project is a distinct
    /// view, not a focus target for this request.
    pub(crate) fn open_view_under_binding(
        &mut self,
        view: ViewSpec,
        attachment_id: &str,
    ) -> Vec<RyeOsEffect> {
        if !self
            .binding_attachments
            .get(attachment_id)
            .is_some_and(|attachment| attachment.views.contains_key(&view.view_ref))
        {
            return Vec::new();
        }
        for tile_id in self.view_sets[self.active_view_set].tile_ids() {
            let existing = self.view_sets[self.active_view_set].tiles.get(&tile_id);
            if existing.is_some_and(|tile| tile.view == view)
                && existing.is_some_and(|tile| {
                    self.instance_binding_attachments
                        .get(&tile.instance_key)
                        .is_some_and(|mounted| mounted == attachment_id)
                })
            {
                self.view_sets[self.active_view_set].focus_tile(tile_id);
                self.push_motion(RyeOsMotionEventVm::FocusChanged {
                    tile_id: tile_id.0.to_string(),
                });
                self.bump_generation();
                return self.effects_for_view(&view);
            }
        }

        // Single-lens surfaces (the cell-grid TUI) hold exactly one center
        // tile: opening a different view REPLACES the lens in place rather
        // than splitting a second tile. Breadth comes from swapping the
        // single lens, never from arranging panes.
        let replaced = (self.view_sets[self.active_view_set].tiling.mode
            == crate::surface::TilingModeSpec::SingleLens
            && !self.view_sets[self.active_view_set].center_is_empty())
        .then(|| {
            self.view_sets[self.active_view_set]
                .tiles
                .get(&self.view_sets[self.active_view_set].focused_tile)
                .map(|tile| tile.instance_key.clone())
        })
        .flatten();
        if let Some(instance_key) = replaced
            && let Some(tile_id) =
                self.view_sets[self.active_view_set].replace_focused_view(view.clone())
        {
            self.invalidate_view_sources(&instance_key);
            self.selection_attachments.remove(&instance_key);
            self.instance_binding_attachments
                .insert(instance_key.clone(), attachment_id.to_string());
            self.normalize_field_local_states();
            let tile_id_text = tile_id.0.to_string();
            self.push_motion(RyeOsMotionEventVm::FocusChanged {
                tile_id: tile_id_text,
            });
            self.bump_generation();
            let effects = self.effects_for_view(&view);
            // The lens swapped subjects: eviction alone would let an
            // in-flight response for the OLD lens land into the empty
            // store — the floor refuses it outright.
            self.floor_source_fetches(&effects, true);
            return effects;
        }

        let effects = self.add_center_tile_under_binding(view, attachment_id);
        self.bump_generation();
        effects
    }

    /// Return one level up the step-in stack: restore the view a drill left and
    /// the facet context it read, then refetch so the restored trace re-resolves
    /// and re-subscribes its tail. No-op at the top of the tree (empty stack).
    pub(crate) fn pop_view(&mut self) -> Vec<RyeOsEffect> {
        if let Some(frame) = self.view_sets[self.active_view_set].lens_stack.last()
            && !self
                .binding_attachments
                .get(&frame.binding_attachment_id)
                .is_some_and(|binding| binding.views.contains_key(&frame.view.view_ref))
        {
            self.notice(
                "The prior view's binding is unavailable; it cannot be retargeted to this lens.",
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let Some(frame) = self.view_sets[self.active_view_set].pop_lens_frame() else {
            return Vec::new();
        };
        let replaced_instance = self.view_sets[self.active_view_set]
            .tiles
            .get(&self.view_sets[self.active_view_set].focused_tile)
            .map(|tile| tile.instance_key.clone());
        // Restore the captured facet context by re-appending any facet whose
        // current value differs — last-writer-wins over the seat log, so the
        // fold returns to the pre-drill value without rewriting history.
        let current = self.seat.fold();
        let mut restored_facets: Vec<String> = Vec::new();
        for (key, value) in &frame.facets {
            let current_value = current.get(key).filter(|value| !value.is_null());
            if current_value != value.as_ref() {
                self.seat.append_facet(
                    key.clone(),
                    value.clone().unwrap_or(serde_json::Value::Null),
                );
                restored_facets.push(key.clone());
            }
        }
        // Restore the view into the focused center tile, and the breadcrumb
        // label of the level being returned to.
        if let Some(instance_key) = &replaced_instance {
            self.invalidate_view_sources(instance_key);
        }
        self.view_sets[self.active_view_set].replace_focused_view(frame.view.clone());
        if let Some(instance_key) = &replaced_instance {
            self.instance_binding_attachments
                .insert(instance_key.clone(), frame.binding_attachment_id.clone());
            if let Some(attachment) = frame.attachment {
                self.selection_attachments
                    .insert(instance_key.clone(), attachment);
            } else {
                self.selection_attachments.remove(instance_key);
            }
        }
        self.normalize_field_local_states();
        self.view_sets[self.active_view_set].lens_label = frame.label.clone();
        self.push_motion(RyeOsMotionEventVm::FocusChanged {
            tile_id: self.view_sets[self.active_view_set]
                .focused_tile
                .0
                .to_string(),
        });
        self.bump_generation();
        // Refetch: the restored view resolves against the restored facets and
        // re-subscribes its tail; facet subscribers (docks/slots) refresh too.
        let mut effects = self.effects_for_view(&frame.view);
        for key in restored_facets {
            if let Some((view_set_id, logical_facet)) =
                super::seat::parse_selection_storage_key(&key)
                && let Some(index) = self
                    .view_sets
                    .iter()
                    .position(|view_set| view_set.id == view_set_id)
            {
                effects.extend(self.effects_for_facet_in_view_set(&logical_facet, index));
            } else {
                effects.extend(self.effects_for_facet(&key));
            }
        }
        // Returning up the drill stack swaps the subject back: a straggler
        // from the drilled-into lens must not land under the restored one.
        self.floor_source_fetches(&effects, true);
        effects
    }

    pub(crate) fn close_tile_or_empty(&mut self, tile_id: TileId) -> bool {
        let Some(instance_key) = self.view_sets[self.active_view_set]
            .tiles
            .get(&tile_id)
            .map(|tile| tile.instance_key.clone())
        else {
            return false;
        };
        if self.view_sets[self.active_view_set]
            .input_buffers
            .iter()
            .any(|(key, input)| {
                !input.text.is_empty()
                    && super::model::InputBufferKey::storage_key_belongs_to(key, &instance_key)
            })
        {
            self.notice(
                "View has current input. Clear it explicitly before closing.",
                super::view_model::RyeOsTone::Warn,
            );
            return false;
        }
        let tile_id_text = tile_id.0.to_string();
        if self.view_sets[self.active_view_set].tile_ids().len() <= 1 {
            if self.view_sets[self.active_view_set].center_is_empty() {
                return false;
            }
            self.invalidate_view_sources(&instance_key);
            self.selection_attachments.remove(&instance_key);
            self.instance_binding_attachments.remove(&instance_key);
            self.push_motion(RyeOsMotionEventVm::TileExit {
                tile_id: tile_id_text,
            });
            self.view_sets[self.active_view_set].reset_to_empty();
            return true;
        }
        if self.view_sets[self.active_view_set].close_tile(tile_id) {
            self.invalidate_view_sources(&instance_key);
            self.selection_attachments.remove(&instance_key);
            self.instance_binding_attachments.remove(&instance_key);
            self.push_motion(RyeOsMotionEventVm::TileExit {
                tile_id: tile_id_text,
            });
            self.push_motion(RyeOsMotionEventVm::FocusChanged {
                tile_id: self.view_sets[self.active_view_set]
                    .focused_tile
                    .0
                    .to_string(),
            });
            true
        } else {
            false
        }
    }

    /// Insert into the canonical layout and emit motion only after acceptance.
    pub(crate) fn add_tile_motions(&mut self, view: ViewSpec) -> Option<TileId> {
        let attachment_id = self
            .insertion_attachment_id(self.view_sets[self.active_view_set].id)?
            .to_string();
        self.add_tile_motions_under_binding(view, &attachment_id)
    }

    pub(crate) fn add_tile_motions_under_binding(
        &mut self,
        view: ViewSpec,
        attachment_id: &str,
    ) -> Option<TileId> {
        if !self
            .binding_attachments
            .get(attachment_id)
            .is_some_and(|attachment| attachment.views.contains_key(&view.view_ref))
        {
            return None;
        }
        let was_empty = self.view_sets[self.active_view_set].center_is_empty();
        let source_tile_id = self.view_sets[self.active_view_set]
            .tile_ids()
            .last()
            .copied()
            .unwrap_or(self.view_sets[self.active_view_set].focused_tile);
        let tile_id = self.view_sets[self.active_view_set].add_tile(view)?;
        let instance = self.view_sets[self.active_view_set]
            .tiles
            .get(&tile_id)
            .map(|tile| tile.instance_key.clone())?;
        if !self.stamp_instance_binding(instance, attachment_id) {
            return None;
        }
        if !was_empty {
            // New tiles land in the stack region; the motion axis is
            // the stack arrangement. (The first tile into an empty center
            // needs no split motion — it simply fills the center.)
            self.push_motion(RyeOsMotionEventVm::TileSplit {
                source_tile_id: source_tile_id.0.to_string(),
                new_tile_id: tile_id.0.to_string(),
                axis: arrange_axis_vm(self.view_sets[self.active_view_set].tiling.stack.arrange),
            });
        }
        self.push_motion(RyeOsMotionEventVm::TileEnter {
            tile_id: tile_id.0.to_string(),
        });
        self.push_motion(RyeOsMotionEventVm::FocusChanged {
            tile_id: tile_id.0.to_string(),
        });
        Some(tile_id)
    }

    pub(crate) fn add_center_tile(&mut self, view: ViewSpec) -> Vec<RyeOsEffect> {
        let Some(attachment_id) = self
            .insertion_attachment_id(self.view_sets[self.active_view_set].id)
            .map(str::to_string)
        else {
            return Vec::new();
        };
        self.add_center_tile_under_binding(view, &attachment_id)
    }

    pub(crate) fn add_center_tile_under_binding(
        &mut self,
        view: ViewSpec,
        attachment_id: &str,
    ) -> Vec<RyeOsEffect> {
        let Some(tile_id) = self.add_tile_motions_under_binding(view, attachment_id) else {
            self.notice(
                "The layout cannot accept another view at this position.",
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        };
        self.normalize_field_local_states();
        let Some(view) = self.view_sets[self.active_view_set]
            .tiles
            .get(&tile_id)
            .map(|tile| tile.view.clone())
        else {
            return Vec::new();
        };
        self.effects_for_view(&view)
    }

    pub(crate) fn push_motion(&mut self, motion: RyeOsMotionEventVm) {
        self.ui.motion.push(motion);
    }
}

fn arrange_axis_vm(arrange: ArrangeSpec) -> RyeOsSplitAxisVm {
    match arrange {
        ArrangeSpec::Horizontal => RyeOsSplitAxisVm::Horizontal,
        ArrangeSpec::Vertical => RyeOsSplitAxisVm::Vertical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::*;

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
        let mut core = RyeOsCore::new(
            session_with_attachments("project-a", vec![a, b]),
            BrowserViewport::default(),
            0,
        );
        let first = core.view_sets[0].tiles[&core.view_sets[0].focused_tile]
            .instance_key
            .clone();
        core.open_view_under_binding(ViewSpec::bound("view:test/shared"), "project-b");
        let second = core.view_sets[0].tiles[&core.view_sets[0].focused_tile]
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
            "slots": {"bottom": {"content": "view:test/input", "open": true}},
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
        let mut core = RyeOsCore::new(
            session_with_attachments("project-a", vec![a, b]),
            BrowserViewport::default(),
            0,
        );
        let (key, _) = core
            .focused_input_instance()
            .expect("authored input is focused");
        let address = core.input_address_for(key.clone());
        assert!(core.input_address_is_live(&address));

        assert!(core.stamp_instance_binding(key.view_instance_key, "project-b"));
        assert!(!core.input_address_is_live(&address));
    }

    #[test]
    fn cross_view_set_move_preserves_the_source_selection_attachment() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/selection-dependent",
            serde_json::json!({
                "widget": "rows",
                "sources": {"default": {
                    "ref": "service:test/source",
                    "params": {"thread": "@facet:selection.work.thread"}
                }}
            }),
        );
        core.add_center_tile(ViewSpec::bound("view:test/selection-dependent"));
        let source = core.active_view_set;
        let source_id = core.view_sets[source].id;
        let tile = core.view_sets[source].focused_tile;
        let instance = core.view_sets[source].tiles[&tile].instance_key.clone();
        let source_attachment = core
            .instance_binding_attachments
            .get(&instance)
            .cloned()
            .unwrap();
        let source_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance.clone(), "default")
                .encode();
        core.data
            .sources
            .insert(source_key.clone(), serde_json::json!({"retained": true}));

        core.new_view_set();
        let target = core.active_view_set;
        let target_id = core.view_sets[target].id;
        core.switch_view_set_tab(source);
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

        assert!(!core.view_sets[source].tiles.contains_key(&tile));
        assert!(core.view_sets[target].tiles.contains_key(&tile));
        assert_eq!(core.active_view_set, target);
        assert_eq!(core.data.sources[&source_key]["retained"], true);
        assert_eq!(
            core.instance_binding_attachments.get(&instance),
            Some(&source_attachment)
        );
        assert_eq!(
            core.selection_attachments.get(&instance),
            Some(&crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: source_id,
            })
        );
    }

    #[test]
    fn cross_view_set_move_still_allows_selection_independent_views() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/independent");
        core.add_center_tile(ViewSpec::bound("view:test/independent"));
        let source = core.active_view_set;
        let tile = core.view_sets[source].focused_tile;
        let instance = core.view_sets[source].tiles[&tile].instance_key.clone();

        core.new_view_set();
        let target = core.active_view_set;
        let target_id = core.view_sets[target].id;
        core.switch_view_set_tab(source);
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

        assert!(!core.view_sets[source].tiles.contains_key(&tile));
        assert!(core.view_sets[target].tiles.contains_key(&tile));
        assert_eq!(core.active_view_set, target);
        assert!(!core.selection_attachments.contains_key(&instance));
    }

    #[test]
    fn cross_view_set_move_preserves_writer_owner_and_offers_follow_not_pin() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/selection-writer",
            serde_json::json!({
                "widget": "rows",
                "affordances": [{
                    "id": "select",
                    "invoke": {"plane": "ui", "facet": "selection.work", "value": {}}
                }]
            }),
        );
        core.add_center_tile(ViewSpec::bound("view:test/selection-writer"));
        let source = core.active_view_set;
        let source_id = core.view_sets[source].id;
        let tile = core.view_sets[source].focused_tile;
        let instance = core.view_sets[source].tiles[&tile].instance_key.clone();
        core.new_view_set();
        let target = core.active_view_set;
        let target_id = core.view_sets[target].id;
        core.switch_view_set_tab(source);

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::MoveTileToViewSet {
                    layout_guard: core.layout_guard(),
                    tile_id: tile.0.to_string(),
                    view_set_id: target_id,
                },
            },
        });

        assert_eq!(
            core.selection_attachments.get(&instance),
            Some(&crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: source_id,
            })
        );
        let actions = crate::ui::view_model::command_overlay_items_for(&core);
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
    fn exact_view_pointer_state_addresses_dock_instances() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let instance_key = crate::ids::RyeOsViewInstanceKey::view_set_slot(
            core.view_sets[core.active_view_set].id,
            "right",
        );
        core.view_sets[core.active_view_set].dock_local.insert(
            instance_key.clone(),
            ViewSpec::bound("view:test/dock").initial_local_state(),
        );

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
        } = &core.view_sets[core.active_view_set].dock_local[&instance_key]
        else {
            panic!("dock should retain generic list state");
        };
        assert_eq!(*cursor, 7);
        assert_eq!(collapsed.iter().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn field_view_added_mid_session_gets_field_local_state_before_fetch() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/field",
            serde_json::json!({
                "widget": "field",
                "sources": {"default": {"ref": "service:test/field"}},
                "projections": {"schema_version": "ryeos.ui.field.projection.v1"}
            }),
        );
        core.add_center_tile(ViewSpec::bound("view:test/field"));
        let tile = core.view_sets[core.active_view_set]
            .tiles
            .get(&core.view_sets[core.active_view_set].focused_tile)
            .unwrap();
        assert!(matches!(
            tile.local,
            crate::view_set::ViewLocalState::Field(_)
        ));
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
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let tile = core.view_sets[core.active_view_set].focused_tile;
        let key = tile.0.to_string();
        let instance_key = core.view_sets[core.active_view_set].tiles[&tile]
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
            let root = vm.view_set.root.expect("layout root");
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
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let tile = core.view_sets[core.active_view_set].focused_tile;
        let key = tile.0.to_string();
        let instance_key = core.view_sets[core.active_view_set].tiles[&tile]
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
            match find(&vm.view_set.root.expect("root")).expect("tile view") {
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
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:ryeos/items/space");
        seed_view(&mut core, "view:test/services");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        let before = core.view_sets[core.active_view_set].tile_ids().len();
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });

        assert_eq!(
            core.view_sets[core.active_view_set].tile_ids().len(),
            before + 1
        );
        assert!(matches!(
            core.view_sets[core.active_view_set].focused_view(),
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
            RyeOsMotionEventVm::TileEnter { tile_id } if tile_id == &core.view_sets[core.active_view_set].focused_tile.0.to_string()
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
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        core.view_sets[core.active_view_set].tiling.mode =
            crate::surface::TilingModeSpec::SingleLens;
        seed_view(&mut core, "view:ryeos/items/space");
        seed_view(&mut core, "view:test/services");

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
        assert_eq!(core.view_sets[core.active_view_set].tile_ids().len(), 1);
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
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });

        assert_eq!(
            core.view_sets[core.active_view_set].tile_ids().len(),
            1,
            "single-lens never splits a second tile"
        );
        assert!(matches!(
            core.view_sets[core.active_view_set].focused_view(),
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
            !core.selection_attachments.contains_key(&instance),
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
        assert_eq!(
            core.view_sets[core.active_view_set].tile_ids().len(),
            1,
            "OpenNewView does not add a tile in single-lens"
        );
    }

    #[test]
    fn lens_pop_restores_the_attachment_captured_by_the_return_frame() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        core.view_sets[core.active_view_set].tiling.mode =
            crate::surface::TilingModeSpec::SingleLens;
        seed_view(&mut core, "view:test/parent");
        seed_view(&mut core, "view:test/child");
        core.open_view(ViewSpec::bound("view:test/parent"));
        let instance = core.focused_view_instance_key().unwrap();
        let attachment = crate::ui::attachment::SelectionAttachment::Pinned {
            values: std::collections::BTreeMap::new(),
            fingerprint: "parent-subject".into(),
        };
        core.selection_attachments
            .insert(instance.clone(), attachment.clone());
        core.view_sets[core.active_view_set].push_lens_frame(
            core.instance_binding_attachments
                .get(&instance)
                .unwrap()
                .clone(),
            ViewSpec::bound("view:test/parent"),
            std::collections::BTreeMap::new(),
            Some("parent".into()),
            Some(attachment.clone()),
        );

        core.open_view(ViewSpec::bound("view:test/child"));
        assert!(!core.selection_attachments.contains_key(&instance));
        core.pop_view();

        assert_eq!(core.selection_attachments.get(&instance), Some(&attachment));
        assert!(matches!(
            core.view_sets[core.active_view_set].focused_view(),
            Some(ViewSpec { view_ref }) if view_ref == "view:test/parent"
        ));
    }

    #[test]
    fn lens_pop_restores_an_absent_referenced_facet_as_null() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        core.view_sets[0].tiling.mode = crate::surface::TilingModeSpec::SingleLens;
        seed_view(&mut core, "view:test/parent");
        core.open_view(ViewSpec::bound("view:test/parent"));
        let instance = core.focused_view_instance_key().unwrap();
        let key = crate::ui::seat::input_route_facet_key(&instance);
        core.view_sets[0].push_lens_frame(
            "fixture-attachment".into(),
            ViewSpec::bound("view:test/parent"),
            std::collections::BTreeMap::from([(key.clone(), None)]),
            None,
            None,
        );
        core.seat
            .append_facet(key.clone(), serde_json::json!({"thread":"T-child"}));

        core.pop_view();

        assert_eq!(core.seat.fold().get(&key), Some(&serde_json::Value::Null));
    }

    #[test]
    fn lens_pop_restores_cross_attachment_authority() {
        let surface = serde_json::json!({
            "name": "lens",
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
        let mut core = RyeOsCore::new(
            session_with_attachments("project-a", vec![a, b]),
            BrowserViewport::default(),
            0,
        );
        core.view_sets[0].tiling.mode = crate::surface::TilingModeSpec::SingleLens;
        let instance = core.focused_view_instance_key().unwrap();
        core.view_sets[0].push_lens_frame(
            "project-a".into(),
            ViewSpec::bound("view:test/parent"),
            std::collections::BTreeMap::new(),
            None,
            None,
        );
        core.open_view_under_binding(ViewSpec::bound("view:test/child"), "project-b");
        assert_eq!(
            core.instance_binding_attachments
                .get(&instance)
                .map(String::as_str),
            Some("project-b")
        );

        core.pop_view();
        assert_eq!(
            core.instance_binding_attachments
                .get(&instance)
                .map(String::as_str),
            Some("project-a")
        );
        assert!(
            matches!(core.view_sets[0].focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:test/parent")
        );
    }

    #[test]
    fn lens_pop_refuses_a_revoked_attachment_without_consuming_history() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/parent");
        core.open_view(ViewSpec::bound("view:test/parent"));
        core.view_sets[0].push_lens_frame(
            "fixture-attachment".into(),
            ViewSpec::bound("view:test/parent"),
            std::collections::BTreeMap::new(),
            None,
            None,
        );
        core.binding_attachments.remove("fixture-attachment");
        let depth = core.view_sets[0].lens_stack.len();

        assert!(core.pop_view().is_empty());
        assert_eq!(core.view_sets[0].lens_stack.len(), depth);
        assert!(
            core.ui
                .notices
                .last()
                .is_some_and(|notice| notice.message.contains("binding is unavailable"))
        );
    }

    #[test]
    fn single_lens_cycle_tab_walks_the_library_skipping_scene_and_input() {
        // In single-lens, Ctrl+←/→ (CycleTab) swaps the one center lens
        // through the surface library — scene backdrops and the foot input
        // are not lenses and are skipped.
        let session = session_with_surface(serde_json::json!({
            "name": "lens-test",
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
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        core.view_sets[core.active_view_set].tiling.mode =
            crate::surface::TilingModeSpec::SingleLens;

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
        };
        let cycle = |core: &mut RyeOsCore| {
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::Activate {
                    intent: RyeOsUiIntent::CycleTab {
                        direction: RyeOsStackMoveDirection::Down,
                    },
                },
            });
        };

        open(&mut core, "view:a");
        cycle(&mut core);
        assert!(
            matches!(core.view_sets[core.active_view_set].focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:b"),
            "cycle forward moves to the next lens"
        );
        assert_eq!(
            core.view_sets[core.active_view_set].tile_ids().len(),
            1,
            "cycling stays single-lens"
        );

        cycle(&mut core);
        assert!(
            matches!(core.view_sets[core.active_view_set].focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:a"),
            "cycle wraps back to the first lens"
        );
    }

    #[test]
    fn open_new_view_allows_duplicate_view_set_tiles() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:ryeos/items/space");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        let before = core.view_sets[core.active_view_set].tile_ids().len();
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });

        let item_tile_count = core.view_sets[core.active_view_set]
            .tiles
            .values()
            .filter(|tile| {
                matches!(&tile.view, ViewSpec { view_ref } if view_ref == "view:ryeos/items/space")
            })
            .count();
        assert_eq!(
            core.view_sets[core.active_view_set].tile_ids().len(),
            before + 1
        );
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
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let tile_id = core.view_sets[0]
            .add_tile(ViewSpec {
                view_ref: "view:test/conversation".into(),
            })
            .unwrap();
        let instance = core.view_sets[0].tiles[&tile_id].instance_key.clone();
        let mut key =
            crate::ui::model::InputBufferKey::new(instance, "view:test/conversation", "message");
        key.target_scope = Some("chain:previous-subject".into());
        core.view_sets[0].input_buffers.insert(
            key.storage_key(),
            crate::ui::model::RyeOsInputState {
                text: "retain previous draft".into(),
                ..Default::default()
            },
        );
        assert!(!core.close_tile_or_empty(tile_id));
        assert!(core.view_sets[0].tiles.contains_key(&tile_id));
        assert_eq!(
            core.view_sets[0].input_buffers[&key.storage_key()].text,
            "retain previous draft"
        );
        core.view_sets[0]
            .input_buffers
            .get_mut(&key.storage_key())
            .unwrap()
            .text
            .clear();
        assert!(core.close_tile_or_empty(tile_id));
    }

    #[test]
    fn close_tile_closes_target_tile() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/threads/list".to_string(),
                    },
                },
            },
        });
        let tile_id = core.view_sets[core.active_view_set].tile_ids()[1];
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::CloseTile {
                    tile_id: tile_id.0.to_string(),
                },
            },
        });

        assert!(
            !core.view_sets[core.active_view_set]
                .tiles
                .contains_key(&tile_id)
        );
        assert!(
            !core.view_sets[core.active_view_set]
                .tile_ids()
                .contains(&tile_id)
        );
        assert!(core.ui.motion.iter().any(|event| matches!(
            event,
            RyeOsMotionEventVm::TileExit { tile_id: closed } if closed == &tile_id.0.to_string()
        )));
    }

    #[test]
    fn closing_one_duplicate_view_cleans_only_its_source_instance() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let view_ref = "view:test/shared";
        seed_view(&mut core, view_ref);

        core.add_center_tile(ViewSpec::bound(view_ref));
        let closing_tile = core.view_sets[core.active_view_set].focused_tile;
        let closing_instance = core.view_sets[core.active_view_set].tiles[&closing_tile]
            .instance_key
            .clone();
        core.add_center_tile(ViewSpec::bound(view_ref));
        let surviving_tile = core.view_sets[core.active_view_set].focused_tile;
        let surviving_instance = core.view_sets[core.active_view_set].tiles[&surviving_tile]
            .instance_key
            .clone();
        let pinned = crate::ui::attachment::SelectionAttachment::Pinned {
            values: std::collections::BTreeMap::new(),
            fingerprint: "captured".into(),
        };
        core.selection_attachments
            .insert(closing_instance.clone(), pinned.clone());
        core.selection_attachments
            .insert(surviving_instance.clone(), pinned);

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

        assert!(core.close_tile_or_empty(closing_tile));
        assert!(
            core.view_sets[core.active_view_set]
                .tiles
                .contains_key(&surviving_tile)
        );
        for key in &closing_keys {
            assert!(!core.data.sources.contains_key(key));
            assert!(!core.data.source_errors.contains_key(key));
            assert!(!core.data.source_epoch.contains_key(key));
            assert!(!core.data.source_stored_epoch.contains_key(key));
            assert!(!core.data.source_floor.contains_key(key));
            assert!(!core.data.timeline_sources.contains_key(key));
            assert!(!core.deferred_source_fetches.contains_key(key));
        }
        assert!(core.data.sources.contains_key(&surviving_key));
        assert!(!core.selection_attachments.contains_key(&closing_instance));
        assert!(core.selection_attachments.contains_key(&surviving_instance));
        assert!(core.data.source_errors.contains_key(&surviving_key));
        assert!(core.data.source_epoch.contains_key(&surviving_key));
        assert!(core.data.source_stored_epoch.contains_key(&surviving_key));
        assert!(core.data.source_floor.contains_key(&surviving_key));
        assert!(core.pending_effects.values().all(|effect| {
            !matches!(effect, RyeOsEffectKind::FetchSource { tile_id, .. }
                if crate::ui::source_key::RyeOsSourceInstanceKey::decode(tile_id)
                    .is_some_and(|key| key.belongs_to(&closing_instance)))
        }));
        assert!(core.pending_effects.values().any(|effect| {
            matches!(effect, RyeOsEffectKind::FetchSource { tile_id, .. } if tile_id == &surviving_key)
        }));
    }

    #[test]
    fn closing_last_app_tile_empties_center() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        assert!(!core.view_sets[core.active_view_set].center_is_empty());

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::CloseFocused,
            },
        });

        assert!(core.view_sets[core.active_view_set].center_is_empty());
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
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/threads/list".to_string(),
                    },
                },
            },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:test/files".to_string(),
                    },
                },
            },
        });

        let Some(crate::layout::LayoutTree::Split {
            axis,
            first,
            second,
            ..
        }) = core.view_sets[core.active_view_set].layout()
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
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let first = core.view_sets[0].id;
        core.new_view_set();
        let second = core.view_sets[1].id;
        core.new_view_set();
        let third = core.view_sets[2].id;
        assert!(core.close_view_set(second).is_empty());
        assert_eq!(core.view_sets[core.active_view_set].id, third);
        assert!(core.close_view_set(second).is_empty());
        assert_eq!(core.view_sets.len(), 2);
        core.view_sets[0].input_buffers.insert(
            "draft".into(),
            crate::ui::model::RyeOsInputState {
                text: "unsent".into(),
                ..Default::default()
            },
        );
        assert!(core.close_view_set(first).is_empty());
        assert_eq!(core.view_sets.len(), 2);
        assert_eq!(core.view_sets[0].input_buffers["draft"].text, "unsent");
        core.view_sets[0].input_buffers.clear();
        core.close_view_set(third);
        assert_eq!(core.view_sets[core.active_view_set].id, first);
        core.close_view_set(first);
        assert_eq!(core.view_sets.len(), 1, "retain a usable final view_set");
    }

    #[test]
    fn view_set_close_refuses_an_external_follow_attachment() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let followed = core.view_sets[0].id;
        core.new_view_set();
        seed_view(&mut core, "view:test/follower");
        core.add_center_tile(ViewSpec::bound("view:test/follower"));
        let tile = core.view_sets[core.active_view_set].focused_tile;
        let instance = core.view_sets[core.active_view_set].tiles[&tile]
            .instance_key
            .clone();
        core.selection_attachments.insert(
            instance,
            crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: followed,
            },
        );

        assert!(core.close_view_set(followed).is_empty());
        assert_eq!(core.view_sets.len(), 2);
        assert!(core.ui.notices.iter().any(|notice| {
            notice
                .message
                .contains("still followed by a view in another set")
        }));
    }

    #[test]
    fn view_set_rename_bounds_labels_without_changing_identity() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let id = core.view_sets[0].id;
        core.rename_view_set(id, "  My work  ");
        assert_eq!(core.view_sets[0].title, "My work");
        for invalid in ["".to_owned(), "bad\nlabel".to_owned(), "x".repeat(129)] {
            core.rename_view_set(id, &invalid);
            assert_eq!(core.view_sets[0].title, "My work");
        }
        assert_eq!(core.view_sets[0].id, id);
    }

    #[test]
    fn duplicate_view_set_preserves_composition_without_aliasing_runtime_state() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/services");
        seed_view(&mut core, "view:test/files");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec::bound("view:test/services"),
                },
            },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec::bound("view:test/files"),
                },
            },
        });
        core.view_sets[0].title = "Development".into();
        core.view_sets[0].input_buffers.insert(
            "draft".into(),
            crate::ui::model::RyeOsInputState {
                text: "private draft".into(),
                ..Default::default()
            },
        );
        let source_id = core.view_sets[0].id;
        let source_instances: std::collections::BTreeSet<_> = core.view_sets[0]
            .tiles
            .values()
            .map(|tile| tile.instance_key.clone())
            .collect();
        let source_views: std::collections::BTreeSet<_> = core.view_sets[0]
            .tiles
            .values()
            .map(|tile| tile.view.view_ref.clone())
            .collect();
        let unresolved_source = core.view_sets[0]
            .tile_ids()
            .into_iter()
            .find_map(|tile_id| core.view_sets[0].tiles.get(&tile_id))
            .unwrap()
            .instance_key
            .clone();
        core.selection_attachments.insert(
            unresolved_source,
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );
        seed_view_value(
            &mut core,
            "view:ryeos/input",
            serde_json::json!({
                "widget": "rows",
                "sources": {"default": {
                    "ref": "service:test/dock",
                    "params": {"thread": "@facet:selection.work.thread"}
                }}
            }),
        );
        let source_dock = crate::ui::model::dock_view_instance_key(
            source_id,
            crate::ui::model::RyeOsDockEdge::Bottom,
        );
        core.selection_attachments.insert(
            source_dock,
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_slot_bottom".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );

        let effects = core.duplicate_view_set(source_id);

        let duplicate = &core.view_sets[core.active_view_set];
        assert_ne!(duplicate.id, source_id);
        assert_eq!(duplicate.title, "Development copy");
        assert_eq!(duplicate.tile_ids().len(), 2);
        assert!(duplicate.input_buffers.is_empty());
        assert!(duplicate.lens_stack.is_empty());
        let duplicate_instances: std::collections::BTreeSet<_> = duplicate
            .tiles
            .values()
            .map(|tile| tile.instance_key.clone())
            .collect();
        assert!(source_instances.is_disjoint(&duplicate_instances));
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
        let duplicate_dock = crate::ui::model::dock_view_instance_key(
            duplicate.id,
            crate::ui::model::RyeOsDockEdge::Bottom,
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
    fn view_set_switch_preserves_slots_drafts_and_focus_independently() {
        use crate::ui::model::{RyeOsDockEdge, RyeOsFocusTarget};
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let original_focus = core.focus_target();
        assert_eq!(
            original_focus,
            RyeOsFocusTarget::Dock {
                edge: RyeOsDockEdge::Bottom
            }
        );
        core.focused_input_buffer_mut()
            .expect("authored input")
            .text = "unsent original".into();
        core.view_sets[0].docks.bottom.as_mut().unwrap().size = 11;

        core.new_view_set();
        assert!(core.view_sets[1].docks.bottom.is_none());
        assert!(core.focused_input_buffer().is_none());
        assert!(core.view_sets[1].input_buffers.is_empty());

        core.switch_view_set_tab(0);
        assert_eq!(core.focus_target(), original_focus);
        assert_eq!(core.focused_input_buffer().unwrap().text, "unsent original");
        assert_eq!(core.view_sets[0].docks.bottom.as_ref().unwrap().size, 11);
    }

    #[test]
    fn view_set_tabs_keep_independent_tile_layouts() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        core.new_view_set();
        core.switch_view_set_tab(0);
        seed_view(&mut core, "view:test/services");
        seed_view(&mut core, "view:test/files");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:test/files".to_string(),
                    },
                },
            },
        });
        let first_tab_tiles = core.view_sets[core.active_view_set].tile_ids().len();

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::SwitchTab { index: 1 },
            },
        });

        assert_eq!(core.active_view_set, 1);
        // Fresh tabs start at home: an empty center.
        assert_eq!(core.view_sets[core.active_view_set].tile_ids().len(), 0);

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/files".to_string(),
                    },
                },
            },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::SwitchTab { index: 0 },
            },
        });

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::SwitchTab { index: 1 },
            },
        });

        assert_eq!(core.active_view_set, 1);
        assert_eq!(core.view_sets[0].tile_ids().len(), first_tab_tiles);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchSource { .. }))
        );
    }

    #[test]
    fn invalid_close_tile_does_not_close_focused_tile() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
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
        let focused = core.view_sets[core.active_view_set].focused_tile;
        let count = core.view_sets[core.active_view_set].tile_ids().len();

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::CloseTile {
                    tile_id: "999".to_string(),
                },
            },
        });

        assert_eq!(core.view_sets[core.active_view_set].focused_tile, focused);
        assert_eq!(core.view_sets[core.active_view_set].tile_ids().len(), count);
        assert!(
            core.view_sets[core.active_view_set]
                .tiles
                .contains_key(&focused)
        );
    }
}
