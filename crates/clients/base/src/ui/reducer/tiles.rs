use super::effect::RyeOsEffect;
use super::event::RyeOsStackMoveDirection;
use super::model::RyeOsCore;
use super::view_model::{RyeOsMotionEventVm, RyeOsSplitAxisVm};
use super::wrap_index;
use crate::ids::TileId;
use crate::surface::ArrangeSpec;
use crate::workspace::{ViewLocalState, ViewSpec};

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
    pub(crate) fn cycle_workspace_tab(
        &mut self,
        direction: RyeOsStackMoveDirection,
    ) -> Vec<RyeOsEffect> {
        let delta = match direction {
            RyeOsStackMoveDirection::Up => -1,
            RyeOsStackMoveDirection::Down => 1,
        };
        // Single-lens has no workspace tabs to page — "cycle" swaps the one
        // center lens through the surface library instead.
        if self.workspace.tiling.mode == crate::surface::TilingModeSpec::SingleLens {
            return self.cycle_lens(delta);
        }
        let len = self.workspaces.len().max(1);
        let next = wrap_index(self.active_workspace, delta, len);
        self.switch_workspace_tab(next)
    }

    /// Whether a view works as a center lens: a real bound view that is
    /// neither a scene backdrop nor the foot input.
    fn lensable(&self, view_ref: &str) -> bool {
        self.views
            .get(view_ref)
            .is_some_and(|binding| binding.widget != "scene" && binding.input.is_none())
    }

    /// The surface's declared library groups, filtered to lensable refs.
    /// The `library:` shape is grouped — `[{ group, views: [ref…] }]` —
    /// one key serving two consumers: `lens_library` cycles the flattened
    /// declared order, `library_groups` hands the launcher the tree.
    fn library_groups_declared(&self) -> Vec<LibraryGroup> {
        self.data
            .session
            .as_ref()
            .and_then(|session| session.effective_surface.as_ref())
            .and_then(|surface| surface.get("library"))
            .and_then(|library| library.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let title = entry.get("group")?.as_str()?.trim().to_string();
                        let refs = entry
                            .get("views")?
                            .as_array()?
                            .iter()
                            .filter_map(|value| value.as_str())
                            .filter(|view_ref| self.lensable(view_ref))
                            .map(str::to_string)
                            .collect::<Vec<_>>();
                        (!title.is_empty()).then_some(LibraryGroup { title, refs })
                    })
                    .collect()
            })
            .unwrap_or_default()
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
        for view_ref in self.views.keys() {
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
        let lenses: Vec<String> = self
            .lens_library()
            .into_iter()
            .filter(|lens| {
                self.views.get(lens).is_none_or(|binding| {
                    super::view_model::unsatisfied_facets(self, binding).is_empty()
                })
            })
            .collect();
        if lenses.is_empty() {
            return Vec::new();
        }
        let current = self
            .workspace
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

    pub(crate) fn switch_workspace_tab(&mut self, index: usize) -> Vec<RyeOsEffect> {
        if index >= self.workspaces.len() || index == self.active_workspace {
            return Vec::new();
        }
        if self.active_workspace < self.workspaces.len() {
            self.workspaces[self.active_workspace] = self.workspace.clone();
        }
        self.workspace = self.workspaces[index].clone();
        self.active_workspace = index;
        self.data.tile_items.clear();
        self.data.tile_files.clear();
        self.data.tile_file_space.clear();
        self.data.timeline_sources.clear();
        self.data.file_read = None;
        self.push_motion(RyeOsMotionEventVm::FocusChanged {
            tile_id: self.workspace.focused_tile.0.to_string(),
        });
        self.push_motion(RyeOsMotionEventVm::TabChanged {
            workspace_number: index + 1,
        });
        self.bump_generation();
        self.initial_effects()
    }

    pub(crate) fn set_tile_cursor(&mut self, tile_id: TileId, index: usize) -> bool {
        let Some(tile) = self.workspace.tiles.get_mut(&tile_id) else {
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
            ViewLocalState::None => false,
        }
    }

    pub(crate) fn set_tile_fold(
        &mut self,
        tile_id: TileId,
        section: usize,
        collapsed: bool,
    ) -> bool {
        let Some(tile) = self.workspace.tiles.get_mut(&tile_id) else {
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
            ViewLocalState::None => false,
        }
    }

    pub(crate) fn open_view(&mut self, view: ViewSpec) -> Vec<RyeOsEffect> {
        for tile_id in self.workspace.tile_ids() {
            if self
                .workspace
                .tiles
                .get(&tile_id)
                .is_some_and(|tile| tile.view == view)
            {
                self.workspace.focused_tile = tile_id;
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
        if self.workspace.tiling.mode == crate::surface::TilingModeSpec::SingleLens
            && !self.workspace.center_is_empty()
        {
            if let Some(tile_id) = self.workspace.replace_focused_view(view.clone()) {
                let key = tile_id.0.to_string();
                self.data.sources.remove(&key);
                self.data.source_epoch.remove(&key);
                self.data.timeline_sources.remove(&key);
                self.push_motion(RyeOsMotionEventVm::FocusChanged { tile_id: key });
                self.bump_generation();
                return self.effects_for_view(&view);
            }
        }

        let effects = self.add_center_tile(view);
        self.bump_generation();
        effects
    }

    /// Return one level up the step-in stack: restore the view a drill left and
    /// the facet context it read, then refetch so the restored trace re-resolves
    /// and re-subscribes its tail. No-op at the top of the tree (empty stack).
    pub(crate) fn pop_view(&mut self) -> Vec<RyeOsEffect> {
        let Some(frame) = self.workspace.pop_lens_frame() else {
            return Vec::new();
        };
        // Restore the captured facet context by re-appending any facet whose
        // current value differs — last-writer-wins over the seat log, so the
        // fold returns to the pre-drill value without rewriting history.
        let current = self.seat.fold();
        let mut restored_facets: Vec<String> = Vec::new();
        for (key, value) in &frame.facets {
            if current.get(key) != Some(value) {
                self.seat.append_facet(key.clone(), value.clone());
                restored_facets.push(key.clone());
            }
        }
        // Restore the view into the focused center tile, and the breadcrumb
        // label of the level being returned to.
        self.workspace.replace_focused_view(frame.view.clone());
        self.workspace.lens_label = frame.label.clone();
        self.push_motion(RyeOsMotionEventVm::FocusChanged {
            tile_id: self.workspace.focused_tile.0.to_string(),
        });
        self.bump_generation();
        // Refetch: the restored view resolves against the restored facets and
        // re-subscribes its tail; facet subscribers (docks/slots) refresh too.
        let mut effects = self.effects_for_view(&frame.view);
        for key in restored_facets {
            effects.extend(self.effects_for_facet(&key));
        }
        effects
    }

    pub(crate) fn close_tile_or_empty(&mut self, tile_id: TileId) -> bool {
        if self.workspace.tile_ids().len() <= 1 {
            if self.workspace.center_is_empty() || !self.workspace.tiles.contains_key(&tile_id) {
                return false;
            }
            self.push_motion(RyeOsMotionEventVm::TileExit {
                tile_id: tile_id.0.to_string(),
            });
            self.workspace.reset_to_empty();
            return true;
        }
        if self.workspace.close_tile(tile_id) {
            self.push_motion(RyeOsMotionEventVm::TileExit {
                tile_id: tile_id.0.to_string(),
            });
            self.push_motion(RyeOsMotionEventVm::FocusChanged {
                tile_id: self.workspace.focused_tile.0.to_string(),
            });
            true
        } else {
            false
        }
    }

    /// Add a center tile through the tiling algorithm (insert: end) and
    /// emit the motions a renderer needs. Returns the new tile id.
    pub(crate) fn add_tile_motions(&mut self, view: ViewSpec) -> TileId {
        let was_empty = self.workspace.center_is_empty();
        let source_tile_id = self.workspace.focused_tile;
        let tile_id = self.workspace.add_tile(view);
        if !was_empty {
            // New tiles land in the stack region; the motion axis is
            // the stack arrangement. (The first tile into an empty center
            // needs no split motion — it simply fills the center.)
            self.push_motion(RyeOsMotionEventVm::TileSplit {
                source_tile_id: source_tile_id.0.to_string(),
                new_tile_id: tile_id.0.to_string(),
                axis: arrange_axis_vm(self.workspace.tiling.stack.arrange),
            });
        }
        self.push_motion(RyeOsMotionEventVm::TileEnter {
            tile_id: tile_id.0.to_string(),
        });
        self.push_motion(RyeOsMotionEventVm::FocusChanged {
            tile_id: tile_id.0.to_string(),
        });
        tile_id
    }

    pub(crate) fn add_center_tile(&mut self, view: ViewSpec) -> Vec<RyeOsEffect> {
        let tile_id = self.add_tile_motions(view);
        let Some(view) = self
            .workspace
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
    fn sections_flat_cursor_selects_a_row_and_resolves_its_section_activation() {
        use crate::ui::view_model::{intent_for_focused_row, RyeOsLayoutNodeVm, RyeOsViewVm};
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/ryeos/status"],
                "views": {
                    "view:ryeos/ryeos/status": {
                        "widget": "sections",
                        "affordances": [{
                            "id": "aim-input",
                            "label": "Aim",
                            "invoke": { "plane": "ui", "facet": "input.route", "merge": { "thread": "{record.thread_id}" } }
                        }],
                        "sections": [
                            { "title": "Threads", "source": { "ref": "service:threads/list", "collection": "threads" }, "projection": { "primary": "thread_id" }, "activate": "aim-input" },
                            { "title": "Bundles", "source": { "ref": "service:bundle/list", "collection": "bundles" }, "projection": { "primary": "name" } }
                        ]
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let tile = core.workspace.focused_tile;
        let key = tile.0.to_string();
        core.data.sources.insert(
            crate::ui::content::section_source_key(&key, 0),
            serde_json::json!({ "threads": [ { "thread_id": "T-ab" }, { "thread_id": "T-cd" } ]}),
        );
        core.data.sources.insert(
            crate::ui::content::section_source_key(&key, 1),
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
            let root = vm.workspace.root.expect("layout root");
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
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/ryeos/status"],
                "views": {
                    "view:ryeos/ryeos/status": {
                        "widget": "sections",
                        "sections": [
                            { "title": "Threads", "source": { "ref": "service:threads/list", "collection": "threads" }, "projection": { "primary": "thread_id" } },
                            { "title": "Bundles", "source": { "ref": "service:bundle/list", "collection": "bundles" }, "projection": { "primary": "name" } }
                        ]
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let tile = core.workspace.focused_tile;
        let key = tile.0.to_string();
        core.data.sources.insert(
            crate::ui::content::section_source_key(&key, 0),
            serde_json::json!({ "threads": [ { "thread_id": "T-ab" }, { "thread_id": "T-cd" } ]}),
        );
        core.data.sources.insert(
            crate::ui::content::section_source_key(&key, 1),
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
            match find(&vm.workspace.root.expect("root")).expect("tile view") {
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
    fn open_view_adds_missing_workspace_tile() {
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
        let before = core.workspace.tile_ids().len();
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });

        assert_eq!(core.workspace.tile_ids().len(), before + 1);
        assert!(matches!(
            core.workspace.focused_view(),
            Some(ViewSpec { view_ref }) if view_ref == "view:test/services"
        ));
        assert!(core
            .ui
            .motion
            .iter()
            .any(|event| matches!(event, RyeOsMotionEventVm::TileSplit { .. })));
        assert!(core.ui.motion.iter().any(|event| matches!(
            event,
            RyeOsMotionEventVm::TileEnter { tile_id } if tile_id == &core.workspace.focused_tile.0.to_string()
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
        core.workspace.tiling.mode = crate::surface::TilingModeSpec::SingleLens;
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
        assert_eq!(core.workspace.tile_ids().len(), 1);
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
            core.workspace.tile_ids().len(),
            1,
            "single-lens never splits a second tile"
        );
        assert!(matches!(
            core.workspace.focused_view(),
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
            core.workspace.tile_ids().len(),
            1,
            "OpenNewView does not add a tile in single-lens"
        );
    }

    #[test]
    fn single_lens_cycle_tab_walks_the_library_skipping_scene_and_input() {
        // In single-lens, Ctrl+←/→ (CycleTab) swaps the one center lens
        // through the surface library — scene backdrops and the foot input
        // are not lenses and are skipped.
        let session = BrowserSession {
            session_id: "s".to_string(),
            surface_ref: "surface:ryeos/ryeos/lens".to_string(),
            user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
            effective_surface: Some(serde_json::json!({
                "name": "lens-test",
                "library": [
                    { "group": "Lenses", "views": ["view:a", "view:scene", "view:input", "view:b"] }
                ],
                "views": {
                    "view:a": { "widget": "rows", "source": { "ref": "service:x", "params": {}, "collection": "rows" } },
                    "view:scene": { "widget": "scene" },
                    "view:input": { "widget": "text", "input": { "id": "line" } },
                    "view:b": { "widget": "rows", "source": { "ref": "service:x", "params": {}, "collection": "rows" } }
                }
            })),
            project_path: Some("/tmp/p".to_string()),
            read_only: false,
            granted_caps: Vec::new(),
            events_url: None,
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        core.workspace.tiling.mode = crate::surface::TilingModeSpec::SingleLens;

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
            matches!(core.workspace.focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:b"),
            "cycle forward moves to the next lens"
        );
        assert_eq!(
            core.workspace.tile_ids().len(),
            1,
            "cycling stays single-lens"
        );

        cycle(&mut core);
        assert!(
            matches!(core.workspace.focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:a"),
            "cycle wraps back to the first lens"
        );
    }

    #[test]
    fn open_new_view_allows_duplicate_workspace_tiles() {
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
        let before = core.workspace.tile_ids().len();
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });

        let item_tile_count = core
            .workspace
            .tiles
            .values()
            .filter(|tile| {
                matches!(&tile.view, ViewSpec { view_ref } if view_ref == "view:ryeos/items/space")
            })
            .count();
        assert_eq!(core.workspace.tile_ids().len(), before + 1);
        assert_eq!(item_tile_count, 2);
        assert!(core
            .ui
            .motion
            .iter()
            .any(|event| matches!(event, RyeOsMotionEventVm::TileSplit { .. })));
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::FetchSource { .. })
        ));
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
        let tile_id = core.workspace.tile_ids()[1];
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::CloseTile {
                    tile_id: tile_id.0.to_string(),
                },
            },
        });

        assert!(!core.workspace.tiles.contains_key(&tile_id));
        assert!(!core.workspace.tile_ids().contains(&tile_id));
        assert!(core.ui.motion.iter().any(|event| matches!(
            event,
            RyeOsMotionEventVm::TileExit { tile_id: closed } if closed == &tile_id.0.to_string()
        )));
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
        assert!(!core.workspace.center_is_empty());

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::CloseFocused,
            },
        });

        assert!(core.workspace.center_is_empty());
        // The last-tile close emits a tile-exit motion (no home mode).
        assert!(core
            .ui
            .motion
            .iter()
            .any(|event| matches!(event, RyeOsMotionEventVm::TileExit { .. })));
    }

    #[test]
    fn master_stack_places_master_right_and_stack_left() {
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
        }) = core.workspace.layout()
        else {
            panic!("master stack should split root");
        };
        assert_eq!(axis, crate::layout::SplitAxis::Horizontal);
        // The first tile opened is the single master on the RIGHT; the
        // two later tiles sit side-by-side in the stack on the left.
        assert!(matches!(
            second.as_ref(),
            crate::layout::LayoutTree::Leaf(_)
        ));
        let crate::layout::LayoutTree::Split { axis, .. } = first.as_ref() else {
            panic!("stack region should split");
        };
        assert_eq!(*axis, crate::layout::SplitAxis::Horizontal);
    }

    #[test]
    fn workspace_tabs_keep_independent_tile_layouts() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
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
        let first_tab_tiles = core.workspace.tile_ids().len();

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::SwitchTab { index: 1 },
            },
        });

        assert_eq!(core.active_workspace, 1);
        // Fresh tabs start at home: an empty center.
        assert_eq!(core.workspace.tile_ids().len(), 0);

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

        assert_eq!(core.active_workspace, 1);
        assert_eq!(core.workspaces[0].tile_ids().len(), first_tab_tiles);
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchSource { .. })));
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
        let focused = core.workspace.focused_tile;
        let count = core.workspace.tile_ids().len();

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::CloseTile {
                    tile_id: "999".to_string(),
                },
            },
        });

        assert_eq!(core.workspace.focused_tile, focused);
        assert_eq!(core.workspace.tile_ids().len(), count);
        assert!(core.workspace.tiles.contains_key(&focused));
    }
}
