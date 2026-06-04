use super::dto::{
    StudioAddProjectDto, StudioDimensionDto, StudioFileReadDto, StudioFilesDto, StudioGcStatusDto,
    StudioItemInspectionDto, StudioItemsDto, StudioOpenProjectDto, StudioSchedulesDto,
    StudioThreadInspectionDto, StudioThreadsDto, StudioTopologyDto,
};
use super::effect::{StudioEffect, StudioEffectKind, StudioEffectResult, StudioEffectResultKind};
use super::event::{
    StudioAction, StudioEvent, StudioFilterField, StudioStackMoveDirection, StudioUiEvent,
};
use super::model::{StudioCore, StudioInspectorState};
use super::view_model::{
    action_for_focused_row, launcher_items, StudioMotionEventVm, StudioSplitAxisVm, StudioTone,
};
use crate::ids::TileId;
use crate::layout::SplitAxis;
use crate::workspace::{ViewLocalState, ViewSpec};

impl StudioCore {
    pub fn dispatch(&mut self, event: StudioEvent) -> Vec<StudioEffect> {
        self.ui.motion.clear();
        match event {
            StudioEvent::Start {
                session,
                viewport,
                now_ms,
            } => {
                *self = StudioCore::new(session, viewport, now_ms);
                self.bump_generation();
                self.initial_effects()
            }
            StudioEvent::Ui { event } => self.dispatch_ui(event),
            StudioEvent::EffectResult { result } => self.apply_effect_result(result),
            StudioEvent::DaemonEvent { payload: _ } => self.initial_effects(),
            StudioEvent::Tick { now_ms } => {
                self.runtime.now_ms = now_ms;
                Vec::new()
            }
            StudioEvent::Resize { viewport } => {
                self.runtime.viewport = viewport;
                self.bump_generation();
                Vec::new()
            }
            StudioEvent::RouteChanged { route } => {
                self.ui.route = Some(route.clone());
                if let Some(view) = view_from_route(&route) {
                    return self.open_view(view);
                }
                self.bump_generation();
                Vec::new()
            }
        }
    }

    fn dispatch_ui(&mut self, event: StudioUiEvent) -> Vec<StudioEffect> {
        match event {
            StudioUiEvent::Activate { action } => self.dispatch_action(action),
            StudioUiEvent::SetFilter {
                tile_id,
                field,
                value,
            } => self.set_tile_filter(tile_id, field, value),
            StudioUiEvent::SetFilesRoot { tile_id, root } => {
                self.set_tile_files_path(tile_id, root, String::new())
            }
            StudioUiEvent::SetFilesPath { tile_id, path } => {
                let Some(tile_id) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                let root = tile_file_state(self, tile_id)
                    .map(|(root, _)| root)
                    .unwrap_or_else(|| "project_ai".to_string());
                self.set_tile_files_path(tile_id.0.to_string(), root, path)
            }
            StudioUiEvent::SetAtlasLayerVisible { kind, visible } => {
                self.ui.atlas.set_layer_visible(kind, visible);
                self.bump_generation();
                Vec::new()
            }
            StudioUiEvent::SetAtlasLens { lens } => {
                self.ui.atlas.set_lens(lens);
                self.bump_generation();
                Vec::new()
            }
            StudioUiEvent::FocusChanged { target } => {
                let Some(tile_id) = target
                    .and_then(|target| target.parse::<u64>().ok())
                    .map(crate::ids::TileId::new)
                else {
                    return Vec::new();
                };
                if self.workspace.tiles.contains_key(&tile_id) {
                    self.workspace.focused_tile = tile_id;
                    self.push_motion(StudioMotionEventVm::FocusChanged {
                        tile_id: tile_id.0.to_string(),
                    });
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioUiEvent::FocusDirection { direction } => {
                if self.workspace.focus_in_direction(direction) {
                    self.push_motion(StudioMotionEventVm::FocusChanged {
                        tile_id: self.workspace.focused_tile.0.to_string(),
                    });
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioUiEvent::OpenLauncher => {
                self.ui.launcher.open = true;
                self.ui.launcher.query.clear();
                self.ui.launcher.selected = 0;
                self.push_motion(StudioMotionEventVm::LauncherOpen);
                self.bump_generation();
                Vec::new()
            }
            StudioUiEvent::CloseLauncher => {
                if self.ui.launcher.open {
                    self.ui.launcher.open = false;
                    self.push_motion(StudioMotionEventVm::LauncherClose);
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioUiEvent::SetLauncherQuery { query } => {
                self.ui.launcher.query = query;
                self.ui.launcher.selected = 0;
                self.bump_generation();
                Vec::new()
            }
            StudioUiEvent::MoveLauncherSelection { delta } => {
                let len = filtered_launcher_items(self).len();
                if len > 0 {
                    self.ui.launcher.selected = wrap_index(self.ui.launcher.selected, delta, len);
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioUiEvent::ChooseLauncher { secondary } => {
                let items = filtered_launcher_items(self);
                let selected = self.ui.launcher.selected.min(items.len().saturating_sub(1));
                let action = items.get(selected).and_then(|item| {
                    if secondary {
                        item.secondary_action
                            .clone()
                            .or_else(|| Some(item.action.clone()))
                    } else {
                        Some(item.action.clone())
                    }
                });
                self.ui.launcher.open = false;
                self.ui.launcher.query.clear();
                self.ui.launcher.selected = 0;
                self.push_motion(StudioMotionEventVm::LauncherClose);
                self.bump_generation();
                action.map_or_else(Vec::new, |action| self.dispatch_action(action))
            }
            StudioUiEvent::SetTileCursor { tile_id, index } => {
                let Some(tile_id) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                if self.set_tile_cursor(tile_id, index) {
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioUiEvent::ActivateFocused => action_for_focused_row(self)
                .map_or_else(Vec::new, |action| self.dispatch_action(action)),
        }
    }

    fn dispatch_action(&mut self, action: StudioAction) -> Vec<StudioEffect> {
        match action {
            StudioAction::Refresh => self.initial_effects(),
            StudioAction::OpenView { view } => {
                let mut effects = self.open_view(view.clone());
                if let Some(hash) = route_for_view(&view) {
                    effects.push(self.emit(StudioEffectKind::SetLocationHash {
                        hash: hash.to_string(),
                    }));
                }
                effects
            }
            StudioAction::OpenNewView { view } => {
                let effects = self.add_slave_tile(view);
                self.bump_generation();
                effects
            }
            StudioAction::SplitFocused { axis } => {
                let view = self
                    .workspace
                    .focused_view()
                    .cloned()
                    .unwrap_or(ViewSpec::Overview);
                let effects = self.split_focused_tile(axis, view);
                self.bump_generation();
                effects
            }
            StudioAction::SplitTile { tile_id, axis } => {
                let Some(tile_id) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                if !self.workspace.layout.tile_ids().contains(&tile_id) {
                    return Vec::new();
                }
                self.workspace.focused_tile = tile_id;
                let view = self
                    .workspace
                    .focused_view()
                    .cloned()
                    .unwrap_or(ViewSpec::Overview);
                let effects = self.split_focused_tile(axis, view);
                self.bump_generation();
                effects
            }
            StudioAction::CloseFocused => {
                if self.close_tile_or_home(self.workspace.focused_tile) {
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioAction::CloseTile { tile_id } => {
                let Some(tile_id) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                if self.close_tile_or_home(tile_id) {
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioAction::ToggleFocusedMaster => {
                if self.workspace.toggle_focused_master() {
                    self.push_motion(StudioMotionEventVm::FocusChanged {
                        tile_id: self.workspace.focused_tile.0.to_string(),
                    });
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioAction::MoveFocusedTile { direction } => {
                let delta = match direction {
                    StudioStackMoveDirection::Up => -1,
                    StudioStackMoveDirection::Down => 1,
                };
                if self.workspace.move_focused_in_stack(delta) {
                    self.push_motion(StudioMotionEventVm::FocusChanged {
                        tile_id: self.workspace.focused_tile.0.to_string(),
                    });
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioAction::CycleTab { direction } => self.cycle_workspace_tab(direction),
            StudioAction::SwitchTab { index } => self.switch_workspace_tab(index),
            StudioAction::ToggleTopStatusBar => {
                self.ui.top_status_visible = !self.ui.top_status_visible;
                self.bump_generation();
                Vec::new()
            }
            StudioAction::ToggleBottomStatusBar => {
                self.ui.bottom_status_visible = !self.ui.bottom_status_visible;
                self.bump_generation();
                Vec::new()
            }
            StudioAction::ResizeFocused { direction } => {
                if self.workspace.resize_focused(direction) {
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioAction::SelectDimension => {
                self.ui.inspector = StudioInspectorState::Dimension;
                self.bump_generation();
                Vec::new()
            }
            StudioAction::InspectItem { canonical_ref } => {
                self.data.item_inspection = None;
                self.ui.inspector = StudioInspectorState::Item {
                    canonical_ref: canonical_ref.clone(),
                };
                self.ensure_inspector_tile();
                self.bump_generation();
                vec![self.emit(StudioEffectKind::InspectItem {
                    canonical_ref,
                    include_raw: true,
                    include_effective: true,
                })]
            }
            StudioAction::EnterItemFolder { tile_id, path } => {
                self.set_item_folder(tile_id, path);
                Vec::new()
            }
            StudioAction::InspectThread { thread_id } => {
                self.data.thread_inspection = None;
                self.ui.inspector = StudioInspectorState::Thread {
                    thread_id: thread_id.clone(),
                };
                self.bump_generation();
                vec![self.emit(StudioEffectKind::InspectThread {
                    thread_id,
                    event_limit: 100,
                })]
            }
            StudioAction::InspectSummary { title, detail } => {
                self.ui.inspector = StudioInspectorState::Summary { title, detail };
                self.ensure_inspector_tile();
                self.bump_generation();
                Vec::new()
            }
            StudioAction::AddCurrentProject => {
                if self.is_read_only() {
                    self.notice("This session is read-only.", StudioTone::Warn);
                    Vec::new()
                } else if let Some(root) = current_project_path(self) {
                    vec![self.emit(StudioEffectKind::AddProject { root })]
                } else {
                    self.notice("No project is bound to this session.", StudioTone::Warn);
                    Vec::new()
                }
            }
            StudioAction::OpenProject { local_id } => {
                if self.is_read_only() {
                    self.notice("This session is read-only.", StudioTone::Warn);
                    Vec::new()
                } else {
                    vec![self.emit(StudioEffectKind::OpenProject { local_id })]
                }
            }
            StudioAction::ListFiles {
                tile_id,
                root,
                path,
            } => self.set_tile_files_path(tile_id, root, path),
            StudioAction::ReadFile { root, path } => {
                if !self.has_project_bound() && file_root_requires_project(&root) {
                    self.notice("No project is bound to this session.", StudioTone::Warn);
                    return Vec::new();
                }
                self.data.file_read = None;
                self.ui.inspector = StudioInspectorState::File {
                    root: root.clone(),
                    path: path.clone(),
                };
                self.bump_generation();
                vec![self.emit(StudioEffectKind::ReadFile { root, path })]
            }
            StudioAction::CopyText { text } => {
                vec![self.emit(StudioEffectKind::CopyToClipboard { text })]
            }
            StudioAction::OpenExternal { url } => {
                vec![self.emit(StudioEffectKind::OpenUrl { url })]
            }
            StudioAction::ExecuteItem {
                item_ref,
                parameters,
            } => {
                if self.is_read_only() {
                    self.notice("This session is read-only.", StudioTone::Warn);
                    Vec::new()
                } else {
                    let _ = (item_ref, parameters);
                    self.notice("Execution from RyeOS is not wired yet.", StudioTone::Warn);
                    Vec::new()
                }
            }
            StudioAction::CancelThread { thread_id } => {
                if self.is_read_only() {
                    self.notice("This session is read-only.", StudioTone::Warn);
                    Vec::new()
                } else {
                    let _ = thread_id;
                    self.notice("Thread commands are not wired yet.", StudioTone::Warn);
                    Vec::new()
                }
            }
        }
    }

    fn emit_fetch_items(&mut self, tile_id: TileId) -> StudioEffect {
        let (query, kind) =
            tile_item_state(self, tile_id).unwrap_or_else(|| (String::new(), String::new()));
        self.emit(StudioEffectKind::FetchItems {
            tile_id: Some(tile_id.0.to_string()),
            query: non_empty(query),
            kind: non_empty(kind),
            limit: 1000,
        })
    }

    fn set_tile_filter(
        &mut self,
        tile_id: String,
        field: StudioFilterField,
        value: String,
    ) -> Vec<StudioEffect> {
        let Some(tile_id) = parse_tile_id(&tile_id) else {
            return Vec::new();
        };
        let Some(tile) = self.workspace.tiles.get_mut(&tile_id) else {
            return Vec::new();
        };
        let ViewLocalState::SpaceBrowser { query, kind, .. } = &mut tile.local else {
            return Vec::new();
        };
        match field {
            StudioFilterField::ItemsQuery => *query = value,
            StudioFilterField::ItemsKind => *kind = value,
            StudioFilterField::ServicesQuery => {
                self.ui.filters.services_query = value;
                self.bump_generation();
                return Vec::new();
            }
        }
        self.data.tile_items.remove(&tile_id.0.to_string());
        self.bump_generation();
        vec![self.emit_fetch_items(tile_id)]
    }

    fn set_item_folder(&mut self, tile_id: String, path: String) {
        let Some(tile_id) = parse_tile_id(&tile_id) else {
            return;
        };
        let Some(tile) = self.workspace.tiles.get_mut(&tile_id) else {
            return;
        };
        let ViewLocalState::SpaceBrowser {
            cursor,
            path: local_path,
            ..
        } = &mut tile.local
        else {
            return;
        };
        *local_path = path.trim_matches('/').to_string();
        *cursor = 0;
        self.bump_generation();
    }

    fn cycle_workspace_tab(&mut self, direction: StudioStackMoveDirection) -> Vec<StudioEffect> {
        let len = self.workspaces.len().max(1);
        let delta = match direction {
            StudioStackMoveDirection::Up => -1,
            StudioStackMoveDirection::Down => 1,
        };
        let next = wrap_index(self.active_workspace, delta, len);
        self.switch_workspace_tab(next)
    }

    fn switch_workspace_tab(&mut self, index: usize) -> Vec<StudioEffect> {
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
        self.data.file_read = None;
        self.ui.inspector = StudioInspectorState::Dimension;
        self.push_motion(StudioMotionEventVm::FocusChanged {
            tile_id: self.workspace.focused_tile.0.to_string(),
        });
        self.push_motion(StudioMotionEventVm::TabChanged {
            workspace_number: index + 1,
        });
        self.bump_generation();
        self.initial_effects()
    }

    fn ensure_inspector_tile(&mut self) {
        if let Some(tile_id) = self
            .workspace
            .layout
            .tile_ids()
            .into_iter()
            .find(|tile_id| {
                self.workspace
                    .tiles
                    .get(tile_id)
                    .is_some_and(|tile| matches!(tile.view, ViewSpec::ItemInspector))
            })
        {
            self.workspace.focused_tile = tile_id;
            self.push_motion(StudioMotionEventVm::FocusChanged {
                tile_id: tile_id.0.to_string(),
            });
            return;
        }
        let previous_focus = self.workspace.focused_tile;
        let prior_tile_count = self.workspace.layout.tile_ids().len();
        if let Some(tile_id) = self
            .workspace
            .add_master_stack_tile(ViewSpec::ItemInspector)
        {
            self.workspace.focused_tile = tile_id;
            self.push_motion(StudioMotionEventVm::TileSplit {
                source_tile_id: previous_focus.0.to_string(),
                new_tile_id: tile_id.0.to_string(),
                axis: split_axis_vm(
                    master_stack_added_axis(prior_tile_count).unwrap_or(SplitAxis::Horizontal),
                ),
            });
            self.push_motion(StudioMotionEventVm::TileEnter {
                tile_id: tile_id.0.to_string(),
            });
            self.push_motion(StudioMotionEventVm::FocusChanged {
                tile_id: tile_id.0.to_string(),
            });
        }
    }

    fn set_tile_files_path(
        &mut self,
        tile_id: String,
        root: String,
        path: String,
    ) -> Vec<StudioEffect> {
        let Some(tile_id) = parse_tile_id(&tile_id) else {
            return Vec::new();
        };
        let Some(tile) = self.workspace.tiles.get_mut(&tile_id) else {
            return Vec::new();
        };
        let ViewLocalState::Files {
            root: local_root,
            path: local_path,
            ..
        } = &mut tile.local
        else {
            return Vec::new();
        };
        *local_root = root.clone();
        *local_path = path.clone();
        self.data.tile_files.remove(&tile_id.0.to_string());
        self.bump_generation();
        if !self.has_project_bound() && file_root_requires_project(&root) {
            return Vec::new();
        }
        vec![self.emit(StudioEffectKind::ListFiles {
            tile_id: Some(tile_id.0.to_string()),
            root,
            path,
        })]
    }

    fn set_tile_cursor(&mut self, tile_id: TileId, index: usize) -> bool {
        let Some(tile) = self.workspace.tiles.get_mut(&tile_id) else {
            return false;
        };
        match &mut tile.local {
            ViewLocalState::ThreadList { cursor, .. }
            | ViewLocalState::SpaceBrowser { cursor, .. }
            | ViewLocalState::Files { cursor, .. }
            | ViewLocalState::GenericList { cursor, .. } => {
                if *cursor == index {
                    return false;
                }
                *cursor = index;
                true
            }
            ViewLocalState::Thread(state) => {
                if state.timeline_cursor == index {
                    return false;
                }
                state.timeline_cursor = index;
                true
            }
            ViewLocalState::None => false,
        }
    }

    fn open_view(&mut self, view: ViewSpec) -> Vec<StudioEffect> {
        if self.workspace.is_home() && !is_home_view(&view) {
            if let Some(tile_id) = self.workspace.replace_focused_view(view.clone()) {
                self.push_motion(StudioMotionEventVm::HomeExit);
                self.push_motion(StudioMotionEventVm::TileEnter {
                    tile_id: tile_id.0.to_string(),
                });
                self.push_motion(StudioMotionEventVm::FocusChanged {
                    tile_id: tile_id.0.to_string(),
                });
            }
            self.bump_generation();
            return self.effects_for_view(&view);
        }
        if is_home_view(&view) {
            if !self.workspace.is_home() {
                for tile_id in self.workspace.layout.tile_ids() {
                    self.push_motion(StudioMotionEventVm::TileExit {
                        tile_id: tile_id.0.to_string(),
                    });
                }
                self.push_motion(StudioMotionEventVm::HomeEnter);
            }
            self.workspace.reset_to_home();
            self.bump_generation();
            return self.effects_for_view(&view);
        }
        for tile_id in self.workspace.layout.tile_ids() {
            if self
                .workspace
                .tiles
                .get(&tile_id)
                .is_some_and(|tile| tile.view == view)
            {
                self.workspace.focused_tile = tile_id;
                self.push_motion(StudioMotionEventVm::FocusChanged {
                    tile_id: tile_id.0.to_string(),
                });
                self.bump_generation();
                return self.effects_for_view(&view);
            }
        }

        let effects = self.add_slave_tile(view);
        self.bump_generation();
        effects
    }

    fn close_tile_or_home(&mut self, tile_id: TileId) -> bool {
        if self.workspace.layout.tile_ids().len() <= 1 {
            if self.workspace.is_home() || !self.workspace.tiles.contains_key(&tile_id) {
                return false;
            }
            self.push_motion(StudioMotionEventVm::TileExit {
                tile_id: tile_id.0.to_string(),
            });
            self.push_motion(StudioMotionEventVm::HomeEnter);
            self.workspace.reset_to_home();
            return true;
        }
        if self.workspace.close_tile_master_stack(tile_id) {
            self.push_motion(StudioMotionEventVm::TileExit {
                tile_id: tile_id.0.to_string(),
            });
            self.push_motion(StudioMotionEventVm::FocusChanged {
                tile_id: self.workspace.focused_tile.0.to_string(),
            });
            true
        } else {
            false
        }
    }

    fn split_focused_tile(&mut self, axis: SplitAxis, view: ViewSpec) -> Vec<StudioEffect> {
        let source_tile_id = self.workspace.focused_tile;
        if let Some(tile_id) = self.workspace.split_focused(axis, view) {
            self.workspace.focused_tile = tile_id;
            self.push_motion(StudioMotionEventVm::TileSplit {
                source_tile_id: source_tile_id.0.to_string(),
                new_tile_id: tile_id.0.to_string(),
                axis: split_axis_vm(axis),
            });
            self.push_motion(StudioMotionEventVm::TileEnter {
                tile_id: tile_id.0.to_string(),
            });
            self.push_motion(StudioMotionEventVm::FocusChanged {
                tile_id: tile_id.0.to_string(),
            });
            let view = self
                .workspace
                .tiles
                .get(&tile_id)
                .map(|tile| tile.view.clone())
                .unwrap_or(ViewSpec::Overview);
            self.effects_for_view(&view)
        } else {
            Vec::new()
        }
    }

    fn add_slave_tile(&mut self, view: ViewSpec) -> Vec<StudioEffect> {
        let source_tile_id = self.workspace.focused_tile;
        let prior_tile_count = self.workspace.layout.tile_ids().len();
        if let Some(tile_id) = self.workspace.add_master_stack_tile(view) {
            self.workspace.focused_tile = tile_id;
            self.push_motion(StudioMotionEventVm::TileSplit {
                source_tile_id: source_tile_id.0.to_string(),
                new_tile_id: tile_id.0.to_string(),
                axis: split_axis_vm(
                    master_stack_added_axis(prior_tile_count).unwrap_or(SplitAxis::Horizontal),
                ),
            });
            self.push_motion(StudioMotionEventVm::TileEnter {
                tile_id: tile_id.0.to_string(),
            });
            self.push_motion(StudioMotionEventVm::FocusChanged {
                tile_id: tile_id.0.to_string(),
            });
            let view = self
                .workspace
                .tiles
                .get(&tile_id)
                .map(|tile| tile.view.clone())
                .unwrap_or(ViewSpec::Overview);
            self.effects_for_view(&view)
        } else {
            Vec::new()
        }
    }

    fn push_motion(&mut self, motion: StudioMotionEventVm) {
        self.ui.motion.push(motion);
    }

    fn effects_for_view(&mut self, view: &ViewSpec) -> Vec<StudioEffect> {
        match view {
            ViewSpec::Thread { .. } | ViewSpec::ThreadList => {
                vec![self.emit(StudioEffectKind::FetchThreads { limit: 200 })]
            }
            ViewSpec::SpaceBrowser { .. } => {
                vec![self.emit_fetch_items(self.workspace.focused_tile)]
            }
            ViewSpec::Files => {
                let (root, path) = tile_file_state(self, self.workspace.focused_tile)
                    .unwrap_or_else(|| ("project".to_string(), String::new()));
                if !self.has_project_bound() && file_root_requires_project(&root) {
                    return Vec::new();
                }
                vec![self.emit(StudioEffectKind::ListFiles {
                    tile_id: Some(self.workspace.focused_tile.0.to_string()),
                    root,
                    path,
                })]
            }
            ViewSpec::Schedules => vec![self.emit(StudioEffectKind::FetchSchedules)],
            ViewSpec::GcStatus => vec![self.emit(StudioEffectKind::FetchGcStatus)],
            ViewSpec::Projects => {
                vec![self.emit(StudioEffectKind::FetchProjects)]
            }
            ViewSpec::Atlas => vec![
                self.emit(StudioEffectKind::FetchDimension),
                self.emit(StudioEffectKind::FetchTopology),
                self.emit(StudioEffectKind::FetchItems {
                    tile_id: None,
                    query: None,
                    kind: None,
                    limit: 1000,
                }),
            ],
            ViewSpec::Graph { .. } => vec![
                self.emit(StudioEffectKind::FetchDimension),
                self.emit(StudioEffectKind::FetchTopology),
            ],
            ViewSpec::Overview
            | ViewSpec::Remotes
            | ViewSpec::Services
            | ViewSpec::ItemInspector
            | ViewSpec::Trust
            | ViewSpec::EventInspector => vec![self.emit(StudioEffectKind::FetchDimension)],
        }
    }

    fn apply_effect_result(&mut self, result: StudioEffectResult) -> Vec<StudioEffect> {
        let Some(expected) = self.pending_effects.remove(&result.id) else {
            return Vec::new();
        };

        if !effect_result_kind_matches(&expected, &result.kind) {
            self.notice(
                "RyeOS ignored a mismatched platform effect result.",
                StudioTone::Warn,
            );
            return Vec::new();
        }

        if !result.ok {
            self.notice(
                result
                    .error
                    .unwrap_or_else(|| "RyeOS platform effect failed".to_string()),
                StudioTone::Danger,
            );
            return Vec::new();
        }

        let Some(data) = result.data else {
            self.bump_generation();
            return Vec::new();
        };

        match result.kind {
            StudioEffectResultKind::Dimension => {
                self.apply_parsed::<StudioDimensionDto>(data, "dimension", |core, dimension| {
                    core.data.dimension = Some(dimension);
                });
            }
            StudioEffectResultKind::Projects => {
                self.apply_parsed::<super::dto::StudioProjectsDto>(
                    data,
                    "projects",
                    |core, projects| {
                        core.data.projects = Some(projects);
                    },
                );
            }
            StudioEffectResultKind::Topology => {
                self.apply_parsed::<StudioTopologyDto>(data, "topology", |core, topology| {
                    core.data.topology = Some(topology);
                });
            }
            StudioEffectResultKind::ProjectAdded => {
                let added = match serde_json::from_value::<StudioAddProjectDto>(data) {
                    Ok(added) => added,
                    Err(error) => {
                        self.notice(
                            format!("RyeOS could not read project_add response: {error}"),
                            StudioTone::Danger,
                        );
                        return Vec::new();
                    }
                };
                self.notice(
                    if added.created {
                        format!("Registered project {}.", added.project.name)
                    } else {
                        format!("Updated project {}.", added.project.name)
                    },
                    StudioTone::Good,
                );
                return vec![self.emit(StudioEffectKind::FetchProjects)];
            }
            StudioEffectResultKind::ProjectOpened => {
                let opened = match serde_json::from_value::<StudioOpenProjectDto>(data) {
                    Ok(opened) => opened,
                    Err(error) => {
                        self.notice(
                            format!("RyeOS could not read project_open response: {error}"),
                            StudioTone::Danger,
                        );
                        return Vec::new();
                    }
                };
                let project_root = opened.session.project_root.or_else(|| {
                    (!opened.project.root.is_empty()).then_some(opened.project.root.clone())
                });
                if let Some(session) = &mut self.data.session {
                    if !opened.session.session_id.is_empty() {
                        session.session_id = opened.session.session_id;
                    }
                    session.project_path = project_root;
                    session.read_only = opened.session.read_only;
                }
                if let Some(projects) = &mut self.data.projects {
                    for project in &mut projects.projects {
                        if project.local_id == opened.project.local_id {
                            *project = opened.project.clone();
                            break;
                        }
                    }
                }
                self.data.dimension = None;
                self.data.topology = None;
                self.data.items = None;
                self.data.tile_items.clear();
                self.data.files = None;
                self.data.tile_files.clear();
                self.data.file_read = None;
                self.data.item_inspection = None;
                self.ui.inspector = StudioInspectorState::Dimension;
                self.pending_effects
                    .retain(|_, kind| !effect_depends_on_project_binding(kind));
                self.notice(
                    format!("Opened project {}.", opened.project.name),
                    StudioTone::Good,
                );
                return self.initial_effects();
            }
            StudioEffectResultKind::Threads => {
                self.apply_parsed::<StudioThreadsDto>(data, "threads", |core, threads| {
                    core.data.threads = Some(threads);
                });
            }
            StudioEffectResultKind::Items => {
                self.apply_parsed::<StudioItemsDto>(data, "items", |core, items| {
                    if effect_matches_current_items(Some(&expected), core) {
                        if let StudioEffectKind::FetchItems {
                            tile_id: Some(tile_id),
                            ..
                        } = &expected
                        {
                            core.data.tile_items.insert(tile_id.clone(), items.clone());
                        } else {
                            core.data.items = Some(items);
                        }
                    }
                });
            }
            StudioEffectResultKind::Schedules => {
                self.apply_parsed::<StudioSchedulesDto>(data, "schedules", |core, schedules| {
                    core.data.schedules = Some(schedules);
                });
            }
            StudioEffectResultKind::GcStatus => {
                self.apply_parsed::<StudioGcStatusDto>(data, "gc_status", |core, gc_status| {
                    core.data.gc_status = Some(gc_status);
                });
            }
            StudioEffectResultKind::FilesList => {
                self.apply_parsed::<StudioFilesDto>(data, "files_list", |core, files| {
                    if effect_matches_current_files(Some(&expected), core, &files) {
                        if let StudioEffectKind::ListFiles {
                            tile_id: Some(tile_id),
                            ..
                        } = &expected
                        {
                            core.data.tile_files.insert(tile_id.clone(), files.clone());
                        }
                        core.data.files = Some(files);
                    }
                });
            }
            StudioEffectResultKind::FileRead => {
                self.apply_parsed::<StudioFileReadDto>(data, "file_read", |core, file_read| {
                    let current = match &core.ui.inspector {
                        StudioInspectorState::File { root, path } => {
                            Some((root.as_str(), path.as_str()))
                        }
                        _ => None,
                    };
                    if effect_matches_current_file_read(Some(&expected), core, &file_read)
                        && current == Some((file_read.root.as_str(), file_read.path.as_str()))
                    {
                        core.data.file_read = Some(file_read);
                    }
                });
            }
            StudioEffectResultKind::ItemInspection => {
                self.apply_parsed::<StudioItemInspectionDto>(
                    data,
                    "item_inspection",
                    |core, item_inspection| {
                        let current_ref = match &core.ui.inspector {
                            StudioInspectorState::Item { canonical_ref } => {
                                Some(canonical_ref.as_str())
                            }
                            _ => None,
                        };
                        if current_ref == Some(item_inspection.item.canonical_ref.as_str()) {
                            core.data.item_inspection = Some(item_inspection);
                        }
                    },
                );
            }
            StudioEffectResultKind::ThreadInspection => {
                self.apply_parsed::<StudioThreadInspectionDto>(
                    data,
                    "thread_inspection",
                    |core, thread_inspection| {
                        let current_id = match &core.ui.inspector {
                            StudioInspectorState::Thread { thread_id } => Some(thread_id.as_str()),
                            _ => None,
                        };
                        let returned_id = thread_id_from_inspection(&thread_inspection);
                        let returned_matches = returned_id
                            .as_deref()
                            .map(|id| Some(id) == current_id)
                            .unwrap_or_else(|| {
                                matches!(expected, StudioEffectKind::InspectThread { .. })
                            });
                        if effect_matches_current_thread(Some(&expected), core) && returned_matches
                        {
                            core.data.thread_inspection = Some(thread_inspection);
                        }
                    },
                );
            }
            StudioEffectResultKind::BrowserOnly => {}
        }

        self.bump_generation();
        Vec::new()
    }

    fn apply_parsed<T>(
        &mut self,
        data: serde_json::Value,
        label: &'static str,
        apply: impl FnOnce(&mut Self, T),
    ) where
        T: serde::de::DeserializeOwned,
    {
        match serde_json::from_value::<T>(data) {
            Ok(value) => apply(self, value),
            Err(error) => self.notice(
                format!("RyeOS could not read {label} response: {error}"),
                StudioTone::Danger,
            ),
        }
    }

    fn is_read_only(&self) -> bool {
        self.data
            .session
            .as_ref()
            .map(|session| session.read_only)
            .or_else(|| {
                self.data
                    .dimension
                    .as_ref()
                    .map(|dimension| dimension.session.read_only)
            })
            .unwrap_or(true)
    }
}

fn parse_tile_id(tile_id: &str) -> Option<crate::ids::TileId> {
    tile_id.parse::<u64>().ok().map(crate::ids::TileId::new)
}

fn split_axis_vm(axis: SplitAxis) -> StudioSplitAxisVm {
    match axis {
        SplitAxis::Horizontal => StudioSplitAxisVm::Horizontal,
        SplitAxis::Vertical => StudioSplitAxisVm::Vertical,
    }
}

fn master_stack_added_axis(prior_tile_count: usize) -> Option<SplitAxis> {
    match prior_tile_count {
        0 => None,
        1 => Some(SplitAxis::Horizontal),
        _ => Some(SplitAxis::Vertical),
    }
}

fn is_home_view(view: &ViewSpec) -> bool {
    matches!(view, ViewSpec::Graph { graph_id: None })
}

fn filtered_launcher_items(core: &StudioCore) -> Vec<super::view_model::StudioLauncherItemVm> {
    let query = core.ui.launcher.query.trim().to_lowercase();
    launcher_items()
        .into_iter()
        .filter(|item| {
            let haystack = format!("{} {}", item.label, item.hint).to_lowercase();
            query.is_empty() || haystack.contains(&query)
        })
        .collect()
}

fn wrap_index(current: usize, delta: i32, len: usize) -> usize {
    (current as i32 + delta).rem_euclid(len as i32) as usize
}

fn tile_item_state(core: &StudioCore, tile_id: TileId) -> Option<(String, String)> {
    let tile = core.workspace.tiles.get(&tile_id)?;
    let ViewLocalState::SpaceBrowser { query, kind, .. } = &tile.local else {
        return None;
    };
    Some((query.clone(), kind.clone()))
}

fn tile_file_state(core: &StudioCore, tile_id: TileId) -> Option<(String, String)> {
    let tile = core.workspace.tiles.get(&tile_id)?;
    let ViewLocalState::Files { root, path, .. } = &tile.local else {
        return None;
    };
    Some((root.clone(), path.clone()))
}

fn effect_result_kind_matches(
    expected: &StudioEffectKind,
    actual: &StudioEffectResultKind,
) -> bool {
    matches!(
        (expected, actual),
        (
            StudioEffectKind::FetchDimension,
            StudioEffectResultKind::Dimension
        ) | (
            StudioEffectKind::FetchProjects,
            StudioEffectResultKind::Projects
        ) | (
            StudioEffectKind::FetchTopology,
            StudioEffectResultKind::Topology
        ) | (
            StudioEffectKind::AddProject { .. },
            StudioEffectResultKind::ProjectAdded
        ) | (
            StudioEffectKind::OpenProject { .. },
            StudioEffectResultKind::ProjectOpened
        ) | (
            StudioEffectKind::FetchThreads { .. },
            StudioEffectResultKind::Threads
        ) | (
            StudioEffectKind::FetchItems { .. },
            StudioEffectResultKind::Items
        ) | (
            StudioEffectKind::FetchSchedules,
            StudioEffectResultKind::Schedules
        ) | (
            StudioEffectKind::FetchGcStatus,
            StudioEffectResultKind::GcStatus
        ) | (
            StudioEffectKind::ListFiles { .. },
            StudioEffectResultKind::FilesList
        ) | (
            StudioEffectKind::ReadFile { .. },
            StudioEffectResultKind::FileRead
        ) | (
            StudioEffectKind::InspectItem { .. },
            StudioEffectResultKind::ItemInspection
        ) | (
            StudioEffectKind::InspectThread { .. },
            StudioEffectResultKind::ThreadInspection
        ) | (
            StudioEffectKind::SetLocationHash { .. },
            StudioEffectResultKind::BrowserOnly
        ) | (
            StudioEffectKind::CopyToClipboard { .. },
            StudioEffectResultKind::BrowserOnly
        ) | (
            StudioEffectKind::OpenUrl { .. },
            StudioEffectResultKind::BrowserOnly
        )
    )
}

fn effect_depends_on_project_binding(kind: &StudioEffectKind) -> bool {
    matches!(
        kind,
        StudioEffectKind::FetchDimension
            | StudioEffectKind::FetchTopology
            | StudioEffectKind::FetchItems { .. }
            | StudioEffectKind::ListFiles { .. }
            | StudioEffectKind::ReadFile { .. }
            | StudioEffectKind::InspectItem { .. }
    )
}

fn current_project_path(core: &StudioCore) -> Option<String> {
    core.data
        .session
        .as_ref()
        .and_then(|session| session.project_path.clone())
        .or_else(|| {
            core.data.dimension.as_ref().and_then(|dimension| {
                dimension
                    .project
                    .as_ref()
                    .map(|project| project.path.clone())
            })
        })
}

fn file_root_requires_project(root: &str) -> bool {
    matches!(root, "project" | "project_ai")
}

fn view_from_route(route: &str) -> Option<ViewSpec> {
    match route.trim_start_matches('#') {
        "" | "graph" => Some(ViewSpec::Graph { graph_id: None }),
        "atlas" => Some(ViewSpec::Atlas),
        "overview" => Some(ViewSpec::Overview),
        "threads" => Some(ViewSpec::ThreadList),
        "items" => Some(ViewSpec::SpaceBrowser { project: None }),
        "files" => Some(ViewSpec::Files),
        "projects" => Some(ViewSpec::Projects),
        "schedules" => Some(ViewSpec::Schedules),
        "gc" => Some(ViewSpec::GcStatus),
        "remotes" => Some(ViewSpec::Remotes),
        "services" => Some(ViewSpec::Services),
        _ => None,
    }
}

fn route_for_view(view: &ViewSpec) -> Option<&'static str> {
    match view {
        ViewSpec::Atlas => Some("atlas"),
        ViewSpec::Graph { graph_id: None } => Some("graph"),
        ViewSpec::Overview => Some("overview"),
        ViewSpec::ThreadList => Some("threads"),
        ViewSpec::SpaceBrowser { project: None } => Some("items"),
        ViewSpec::Files => Some("files"),
        ViewSpec::Projects => Some("projects"),
        ViewSpec::Schedules => Some("schedules"),
        ViewSpec::GcStatus => Some("gc"),
        ViewSpec::Remotes => Some("remotes"),
        ViewSpec::Services => Some("services"),
        ViewSpec::Thread { .. }
        | ViewSpec::ItemInspector
        | ViewSpec::SpaceBrowser { project: Some(_) }
        | ViewSpec::Trust
        | ViewSpec::Graph { graph_id: Some(_) }
        | ViewSpec::EventInspector => None,
    }
}

fn effect_matches_current_items(expected: Option<&StudioEffectKind>, core: &StudioCore) -> bool {
    let Some(StudioEffectKind::FetchItems {
        tile_id,
        query,
        kind,
        ..
    }) = expected
    else {
        return true;
    };
    if tile_id.is_none() {
        return query.is_none() && kind.is_none();
    }
    let Some(tile_id) = tile_id.as_deref().and_then(parse_tile_id) else {
        return false;
    };
    let Some((current_query, current_kind)) = tile_item_state(core, tile_id) else {
        return false;
    };
    query == &non_empty(current_query) && kind == &non_empty(current_kind)
}

fn effect_matches_current_files(
    expected: Option<&StudioEffectKind>,
    core: &StudioCore,
    files: &StudioFilesDto,
) -> bool {
    let Some(StudioEffectKind::ListFiles {
        tile_id,
        root,
        path,
    }) = expected
    else {
        return true;
    };
    let Some(tile_id) = tile_id.as_deref().and_then(parse_tile_id) else {
        return false;
    };
    let Some((current_root, current_path)) = tile_file_state(core, tile_id) else {
        return false;
    };
    root == &current_root && path == &current_path && root == &files.root && path == &files.path
}

fn effect_matches_current_file_read(
    expected: Option<&StudioEffectKind>,
    core: &StudioCore,
    file_read: &StudioFileReadDto,
) -> bool {
    let Some(StudioEffectKind::ReadFile { root, path }) = expected else {
        return true;
    };
    matches!(
        &core.ui.inspector,
        StudioInspectorState::File { root: current_root, path: current_path }
            if current_root == root && current_path == path
    ) && root == &file_read.root
        && path == &file_read.path
}

fn effect_matches_current_thread(expected: Option<&StudioEffectKind>, core: &StudioCore) -> bool {
    let Some(StudioEffectKind::InspectThread { thread_id, .. }) = expected else {
        return true;
    };
    matches!(
        &core.ui.inspector,
        StudioInspectorState::Thread { thread_id: current_id } if current_id == thread_id
    )
}

fn thread_id_from_inspection(inspection: &StudioThreadInspectionDto) -> Option<String> {
    inspection
        .thread
        .get("thread_id")
        .or_else(|| inspection.thread.get("id"))
        .and_then(|value| {
            value.as_str().map(str::to_string).or_else(|| {
                if value.is_number() || value.is_boolean() {
                    Some(value.to_string())
                } else {
                    None
                }
            })
        })
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::effect::StudioEffectResultKind;
    use crate::studio::event::{StudioEvent, StudioFilterField, StudioUiEvent};
    use crate::studio::model::{BrowserSession, BrowserViewport, StudioCore};
    use crate::studio::view_model::{StudioLayoutNodeVm, StudioViewVm};
    use crate::workspace::FocusDirection;

    fn session() -> BrowserSession {
        BrowserSession {
            session_id: "session-1".to_string(),
            surface_ref: "surface:ryeos/studio/base".to_string(),
            user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
            effective_surface: None,
            project_path: Some("/tmp/project".to_string()),
            read_only: true,
            granted_caps: Vec::new(),
            events_url: Some("/ui/events/session/session-1".to_string()),
        }
    }

    fn writable_session() -> BrowserSession {
        BrowserSession {
            read_only: false,
            ..session()
        }
    }

    fn item_tile_id(core: &StudioCore) -> TileId {
        core.workspace
            .tiles
            .iter()
            .find_map(|(tile_id, tile)| {
                matches!(tile.view, ViewSpec::SpaceBrowser { .. }).then_some(*tile_id)
            })
            .expect("workspace should include an item tile")
    }

    fn open_items_tile(core: &mut StudioCore) -> TileId {
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::SpaceBrowser { project: None },
                },
            },
        });
        item_tile_id(core)
    }

    #[test]
    fn start_emits_initial_effects() {
        let mut core = StudioCore::default();
        let effects = core.dispatch(StudioEvent::Start {
            session: session(),
            viewport: BrowserViewport::default(),
            now_ms: 0,
        });

        assert_eq!(effects.len(), 3);
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchDimension)));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchProjects)));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchTopology)));
    }

    #[test]
    fn graph_view_effects_fetch_topology() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Graph { graph_id: None },
                },
            },
        });

        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchTopology)));
    }

    #[test]
    fn topology_effect_result_updates_state() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let effects = core.initial_effects();
        let topology_id = effects
            .iter()
            .find(|effect| matches!(effect.kind, StudioEffectKind::FetchTopology))
            .map(|effect| effect.id)
            .expect("graph startup should fetch topology");

        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: topology_id,
                ok: true,
                kind: StudioEffectResultKind::Topology,
                data: Some(serde_json::json!({
                    "version": "1",
                    "kind": "topology",
                    "metadata": {},
                    "nodes": [{
                        "id": "tool:demo/run",
                        "kind": "tool",
                        "label": "run",
                        "ref": "tool:demo/run"
                    }],
                    "edges": []
                })),
                error: None,
            },
        });

        let topology = core.data.topology.as_ref().expect("topology state");
        assert_eq!(topology.nodes.len(), 1);
        assert_eq!(topology.nodes[0].ref_, "tool:demo/run");
    }

    #[test]
    fn launcher_includes_graph_view() {
        assert!(launcher_items().iter().any(|item| {
            item.label == "Graph"
                && matches!(
                    item.action,
                    StudioAction::OpenView {
                        view: ViewSpec::Graph { graph_id: None }
                    }
                )
        }));
    }

    #[test]
    fn launcher_includes_trust_view() {
        assert!(launcher_items().iter().any(|item| {
            item.label == "Trust"
                && matches!(
                    item.action,
                    StudioAction::OpenView {
                        view: ViewSpec::Trust
                    }
                )
        }));
    }

    #[test]
    fn status_bar_exposes_principal_and_surface() {
        let core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let envelope = core.envelope(Vec::new());
        let segments = &envelope.view_model.presentation.chrome.status_bar.segments;
        let value = |id: &str| {
            segments
                .iter()
                .find(|segment| segment.id == id)
                .map(|segment| segment.value.as_str())
        };

        assert_eq!(value("principal"), Some("fp:abababab…"));
        assert_eq!(value("surface"), Some("ryeos/studio/base"));
    }

    #[test]
    fn trust_view_exposes_principals_and_capabilities() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.data.dimension = Some(
            serde_json::from_value(serde_json::json!({
                "schema_version": "studio.test",
                "session": {
                    "session_id": "session-1",
                    "surface_ref": "surface:ryeos/studio/base",
                    "user_principal_id": "fp:session",
                    "read_only": false,
                    "granted_caps": ["rye.execute.service.ui.*"]
                },
                "local_node": {
                    "identity": {
                        "principal_id": "fp:node",
                        "fingerprint": "node-fingerprint"
                    },
                    "services": [
                        {
                            "endpoint": "ui.session.current",
                            "service_ref": "service:ui/session/current",
                            "availability": "daemon",
                            "required_caps": ["rye.execute.service.ui.session.current"]
                        }
                    ]
                }
            }))
            .expect("dimension dto should parse"),
        );
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Trust,
                },
            },
        });

        let envelope = core.envelope(Vec::new());
        let tile = match envelope.view_model.workspace.root {
            StudioLayoutNodeVm::Tile { view, .. } => view,
            _ => panic!("expected single trust tile"),
        };
        let rows = match tile {
            StudioViewVm::Rows { title, rows, .. } => {
                assert_eq!(title, "Trust");
                rows
            }
            other => panic!("expected trust rows, got {other:?}"),
        };

        assert!(rows.iter().any(|row| {
            row.primary == "Session principal"
                && row.secondary.as_deref()
                    == Some("fp:abababababababababababababababababababababababababababababababab")
        }));
        assert!(rows.iter().any(|row| {
            row.primary == "Local node principal" && row.secondary.as_deref() == Some("fp:node")
        }));
        assert!(rows.iter().any(|row| {
            row.primary == "Granted capability"
                && row.secondary.as_deref() == Some("rye.execute.service.ui.*")
        }));
        assert!(rows.iter().any(|row| {
            row.primary == "Required capability"
                && row.secondary.as_deref() == Some("rye.execute.service.ui.session.current")
                && row.meta.as_deref() == Some("ui.session.current")
        }));
    }

    #[test]
    fn inspect_summary_opens_inspector_tile() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        assert!(core
            .workspace
            .tiles
            .values()
            .all(|tile| !matches!(tile.view, ViewSpec::ItemInspector)));

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InspectSummary {
                    title: "Topology: run".to_string(),
                    detail: serde_json::json!({ "ref": "tool:demo/run" }),
                },
            },
        });

        let focused = core
            .workspace
            .tiles
            .get(&core.workspace.focused_tile)
            .expect("focused tile");
        assert!(matches!(focused.view, ViewSpec::ItemInspector));
        assert!(matches!(
            core.ui.inspector,
            StudioInspectorState::Summary { .. }
        ));
    }

    #[test]
    fn open_project_requires_writable_session() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenProject {
                    local_id: "prj_1".to_string(),
                },
            },
        });

        assert!(effects.is_empty());
        assert_eq!(core.ui.notices.len(), 1);
    }

    #[test]
    fn open_project_effect_result_rebinds_session_and_reloads() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenProject {
                    local_id: "prj_1".to_string(),
                },
            },
        });
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::OpenProject { local_id }) if local_id == "prj_1"
        ));

        let reloads = core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: effects[0].id,
                ok: true,
                kind: StudioEffectResultKind::ProjectOpened,
                data: Some(serde_json::json!({
                    "project": {
                        "local_id": "prj_1",
                        "name": "next",
                        "root": "/tmp/next",
                        "exists": true
                    },
                    "session": {
                        "session_id": "session-1",
                        "project_root": "/tmp/next",
                        "read_only": false
                    },
                    "recent": []
                })),
                error: None,
            },
        });

        assert_eq!(
            core.data
                .session
                .as_ref()
                .and_then(|s| s.project_path.as_deref()),
            Some("/tmp/next")
        );
        assert!(reloads
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchDimension)));
        assert!(reloads
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchProjects)));
    }

    #[test]
    fn open_project_invalidates_pending_project_bound_effects() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let tile_id = open_items_tile(&mut core);
        let stale = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetFilter {
                tile_id: tile_id.0.to_string(),
                field: StudioFilterField::ItemsQuery,
                value: "old".to_string(),
            },
        });
        let open = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenProject {
                    local_id: "prj_1".to_string(),
                },
            },
        });

        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: open[0].id,
                ok: true,
                kind: StudioEffectResultKind::ProjectOpened,
                data: Some(serde_json::json!({
                    "project": {
                        "local_id": "prj_1",
                        "name": "next",
                        "root": "/tmp/next",
                        "exists": true
                    },
                    "session": {
                        "session_id": "session-1",
                        "project_root": "/tmp/next",
                        "read_only": false
                    },
                    "recent": []
                })),
                error: None,
            },
        });
        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: stale[0].id,
                ok: true,
                kind: StudioEffectResultKind::Items,
                data: Some(serde_json::json!({
                    "items": [{
                        "canonical_ref": "tool:old/run",
                        "item_kind": "tool",
                        "bare_id": "old/run",
                        "label": "old/run"
                    }]
                })),
                error: None,
            },
        });

        assert!(core.data.items.is_none());
        assert!(core.data.tile_items.is_empty());
    }

    #[test]
    fn route_change_focuses_workspace_view() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::RouteChanged {
            route: "items".to_string(),
        });

        assert_eq!(
            core.workspace.focused_view(),
            Some(&ViewSpec::SpaceBrowser { project: None })
        );
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchItems { limit: 1000, .. })
        ));
    }

    #[test]
    fn projects_view_fetches_projects() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Projects,
                },
            },
        });

        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchProjects)
        ));
    }

    #[test]
    fn projects_focused_activation_uses_selected_row() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.data.projects = Some(
            serde_json::from_value(serde_json::json!({
                "version": 1,
                "projects": [
                    {
                        "local_id": "first",
                        "name": "first",
                        "root": "/tmp/first",
                        "exists": true
                    },
                    {
                        "local_id": "second",
                        "name": "second",
                        "root": "/tmp/second",
                        "exists": true
                    }
                ]
            }))
            .expect("projects dto should parse"),
        );
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Projects,
                },
            },
        });
        let tile_id = core.workspace.focused_tile.0.to_string();
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetTileCursor { tile_id, index: 2 },
        });

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::ActivateFocused,
        });

        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::OpenProject { local_id }) if local_id == "second"
        ));
    }

    #[test]
    fn projects_view_can_register_current_project() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.data.projects = Some(
            serde_json::from_value(serde_json::json!({
                "version": 1,
                "projects": []
            }))
            .expect("projects dto should parse"),
        );
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Projects,
                },
            },
        });

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::ActivateFocused,
        });

        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::AddProject { root }) if root == "/tmp/project"
        ));
    }

    #[test]
    fn project_added_refetches_projects() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::AddCurrentProject,
            },
        });
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::AddProject { root }) if root == "/tmp/project"
        ));

        let followups = core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: effects[0].id,
                ok: true,
                kind: StudioEffectResultKind::ProjectAdded,
                data: Some(serde_json::json!({
                    "project": {
                        "local_id": "prj_1",
                        "name": "project",
                        "root": "/tmp/project",
                        "exists": true
                    },
                    "created": true
                })),
                error: None,
            },
        });

        assert!(matches!(
            followups.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchProjects)
        ));
    }

    #[test]
    fn item_filter_emits_fetch_items_effect() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let tile_id = open_items_tile(&mut core);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetFilter {
                tile_id: tile_id.0.to_string(),
                field: StudioFilterField::ItemsQuery,
                value: "parser".to_string(),
            },
        });

        assert_eq!(
            tile_item_state(&core, tile_id).map(|(query, _)| query),
            Some("parser".to_string())
        );
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchItems { tile_id: Some(effect_tile), query: Some(query), limit: 1000, .. })
                if effect_tile == &tile_id.0.to_string() && query == "parser"
        ));
    }

    #[test]
    fn read_only_execute_does_not_emit_effect() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ExecuteItem {
                    item_ref: "tool:demo/run".to_string(),
                    parameters: serde_json::json!({}),
                },
            },
        });

        assert!(effects.is_empty());
        assert_eq!(core.ui.notices.len(), 1);
    }

    #[test]
    fn dimension_effect_result_updates_view_model() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let effects = core.initial_effects();
        let dimension_id = effects
            .iter()
            .find(|effect| matches!(effect.kind, StudioEffectKind::FetchDimension))
            .map(|effect| effect.id)
            .expect("initial load should fetch dimension");
        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: dimension_id,
                ok: true,
                kind: StudioEffectResultKind::Dimension,
                data: Some(serde_json::json!({
                    "schema_version": "studio.test",
                    "session": {
                        "session_id": "session-1",
                        "surface_ref": "surface:ryeos/studio/base",
                        "read_only": true
                    },
                    "local_node": {
                        "health": { "status": "healthy" },
                        "services": [
                            { "endpoint": "ui.session.current", "service_ref": "service:ui/session/current", "availability": "DaemonOnly" }
                        ]
                    },
                    "project": { "path": "/tmp/project" }
                })),
                error: None,
            },
        });

        let envelope = core.envelope(Vec::new());
        assert_eq!(envelope.view_model.chrome.health_label, "healthy");
        assert_eq!(
            envelope.view_model.session.project_path.as_deref(),
            Some("/tmp/project")
        );
    }

    #[test]
    fn stale_items_result_does_not_replace_current_filter_results() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let tile_id = open_items_tile(&mut core);
        let old = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetFilter {
                tile_id: tile_id.0.to_string(),
                field: StudioFilterField::ItemsQuery,
                value: "old".to_string(),
            },
        });
        let new = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetFilter {
                tile_id: tile_id.0.to_string(),
                field: StudioFilterField::ItemsQuery,
                value: "new".to_string(),
            },
        });

        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: new[0].id,
                ok: true,
                kind: StudioEffectResultKind::Items,
                data: Some(serde_json::json!({
                    "items": [{
                        "canonical_ref": "tool:new/run",
                        "item_kind": "tool",
                        "bare_id": "new/run",
                        "label": "new/run"
                    }]
                })),
                error: None,
            },
        });
        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: old[0].id,
                ok: true,
                kind: StudioEffectResultKind::Items,
                data: Some(serde_json::json!({
                    "items": [{
                        "canonical_ref": "tool:old/run",
                        "item_kind": "tool",
                        "bare_id": "old/run",
                        "label": "old/run"
                    }]
                })),
                error: None,
            },
        });

        let items = core
            .data
            .tile_items
            .get(&tile_id.0.to_string())
            .expect("items loaded");
        assert_eq!(items.items[0].canonical_ref, "tool:new/run");
    }

    #[test]
    fn stale_file_read_result_requires_current_root_and_path() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let old = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ReadFile {
                    root: "project_ai".to_string(),
                    path: "README.md".to_string(),
                },
            },
        });
        let new = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ReadFile {
                    root: "user_ai".to_string(),
                    path: "README.md".to_string(),
                },
            },
        });

        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: new[0].id,
                ok: true,
                kind: StudioEffectResultKind::FileRead,
                data: Some(serde_json::json!({
                    "root": "user_ai",
                    "path": "README.md",
                    "content": "new"
                })),
                error: None,
            },
        });
        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: old[0].id,
                ok: true,
                kind: StudioEffectResultKind::FileRead,
                data: Some(serde_json::json!({
                    "root": "project_ai",
                    "path": "README.md",
                    "content": "old"
                })),
                error: None,
            },
        });

        assert_eq!(
            core.data
                .file_read
                .as_ref()
                .map(|file| file.content.as_str()),
            Some("new")
        );
    }

    #[test]
    fn open_view_adds_missing_workspace_tile() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::SpaceBrowser { project: None },
                },
            },
        });
        let before = core.workspace.layout.tile_ids().len();
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Services,
                },
            },
        });

        assert_eq!(core.workspace.layout.tile_ids().len(), before + 1);
        assert!(matches!(
            core.workspace.focused_view(),
            Some(ViewSpec::Services)
        ));
        assert!(core
            .ui
            .motion
            .iter()
            .any(|event| matches!(event, StudioMotionEventVm::TileSplit { .. })));
        assert!(core.ui.motion.iter().any(|event| matches!(
            event,
            StudioMotionEventVm::TileEnter { tile_id } if tile_id == &core.workspace.focused_tile.0.to_string()
        )));
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchDimension)
        ));
    }

    #[test]
    fn open_new_view_allows_duplicate_workspace_tiles() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::SpaceBrowser { project: None },
                },
            },
        });
        let before = core.workspace.layout.tile_ids().len();
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::SpaceBrowser { project: None },
                },
            },
        });

        let item_tile_count = core
            .workspace
            .tiles
            .values()
            .filter(|tile| matches!(tile.view, ViewSpec::SpaceBrowser { .. }))
            .count();
        assert_eq!(core.workspace.layout.tile_ids().len(), before + 1);
        assert_eq!(item_tile_count, 2);
        assert!(core
            .ui
            .motion
            .iter()
            .any(|event| matches!(event, StudioMotionEventVm::TileSplit { .. })));
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchItems { .. })
        ));
    }

    #[test]
    fn close_tile_closes_target_tile() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Services,
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::ThreadList,
                },
            },
        });
        let tile_id = core.workspace.layout.tile_ids()[1];
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CloseTile {
                    tile_id: tile_id.0.to_string(),
                },
            },
        });

        assert!(!core.workspace.tiles.contains_key(&tile_id));
        assert!(!core.workspace.layout.tile_ids().contains(&tile_id));
        assert!(core.ui.motion.iter().any(|event| matches!(
            event,
            StudioMotionEventVm::TileExit { tile_id: closed } if closed == &tile_id.0.to_string()
        )));
    }

    #[test]
    fn closing_last_app_tile_returns_home() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Services,
                },
            },
        });
        assert!(!core.workspace.is_home());

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CloseFocused,
            },
        });

        assert!(core.workspace.is_home());
        assert!(core
            .ui
            .motion
            .iter()
            .any(|event| matches!(event, StudioMotionEventVm::HomeEnter)));
    }

    #[test]
    fn launcher_state_is_reduced_in_core() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::OpenLauncher,
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetLauncherQuery {
                query: "items".to_string(),
            },
        });

        assert!(core.ui.launcher.open);
        assert_eq!(core.ui.launcher.query, "items");
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::ChooseLauncher { secondary: false },
        });

        assert!(!core.ui.launcher.open);
        assert!(matches!(
            core.workspace.focused_view(),
            Some(ViewSpec::SpaceBrowser { project: None })
        ));
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchItems { .. })
        ));
    }

    #[test]
    fn arrow_focus_uses_workspace_geometry() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Services,
                },
            },
        });
        let left = core.workspace.focused_tile;
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::ThreadList,
                },
            },
        });
        let right = core.workspace.focused_tile;
        assert_ne!(left, right);

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::FocusDirection {
                direction: FocusDirection::Left,
            },
        });

        assert_eq!(core.workspace.focused_tile, left);
    }

    #[test]
    fn master_stack_places_master_left_and_slaves_right() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Services,
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::ThreadList,
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::Files,
                },
            },
        });

        let crate::layout::LayoutTree::Split {
            axis,
            first,
            second,
            ..
        } = &core.workspace.layout
        else {
            panic!("master stack should split root");
        };
        assert_eq!(*axis, SplitAxis::Horizontal);
        assert!(matches!(first.as_ref(), crate::layout::LayoutTree::Leaf(_)));
        let crate::layout::LayoutTree::Split { axis, .. } = second.as_ref() else {
            panic!("slave stack should split stack");
        };
        assert_eq!(*axis, SplitAxis::Vertical);
    }

    #[test]
    fn workspace_tabs_keep_independent_tile_layouts() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Services,
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::Files,
                },
            },
        });
        let first_tab_tiles = core.workspace.layout.tile_ids().len();

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::SwitchTab { index: 1 },
            },
        });

        assert_eq!(core.active_workspace, 1);
        assert_eq!(core.workspace.layout.tile_ids().len(), 1);

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Files,
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::SpaceBrowser { project: None },
                },
            },
        });

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::SwitchTab { index: 0 },
            },
        });

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::SwitchTab { index: 1 },
            },
        });

        assert_eq!(core.active_workspace, 1);
        assert_eq!(core.workspaces[0].layout.tile_ids().len(), first_tab_tiles);
        assert!(effects.iter().any(|effect| matches!(
            effect.kind,
            StudioEffectKind::ListFiles {
                tile_id: Some(_),
                ..
            }
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect.kind,
            StudioEffectKind::FetchItems {
                tile_id: Some(_),
                ..
            }
        )));
    }

    #[test]
    fn invalid_close_tile_does_not_close_focused_tile() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let focused = core.workspace.focused_tile;
        let count = core.workspace.layout.tile_ids().len();

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CloseTile {
                    tile_id: "999".to_string(),
                },
            },
        });

        assert_eq!(core.workspace.focused_tile, focused);
        assert_eq!(core.workspace.layout.tile_ids().len(), count);
        assert!(core.workspace.tiles.contains_key(&focused));
    }

    #[test]
    fn mismatched_effect_result_does_not_apply_data() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::SpaceBrowser { project: None },
                },
            },
        });
        let fetch_items = effects
            .iter()
            .find(|effect| matches!(effect.kind, StudioEffectKind::FetchItems { .. }))
            .expect("open items should fetch items");

        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: fetch_items.id,
                ok: true,
                kind: StudioEffectResultKind::Dimension,
                data: Some(serde_json::json!({
                    "schema_version": "studio.test",
                    "session": { "session_id": "session-1", "surface_ref": "surface:ryeos/studio/base", "read_only": true },
                    "local_node": { "health": { "status": "healthy" }, "services": [] }
                })),
                error: None,
            },
        });

        assert!(core.data.dimension.is_none());
        assert!(core.data.items.is_none());
        assert_eq!(core.ui.notices.len(), 1);
    }

    #[test]
    fn item_filters_are_tile_local() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        let first = open_items_tile(&mut core);
        core.workspace.focused_tile = first;
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::SplitFocused {
                    axis: SplitAxis::Horizontal,
                },
            },
        });
        let second = core.workspace.focused_tile;

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetFilter {
                tile_id: second.0.to_string(),
                field: StudioFilterField::ItemsQuery,
                value: "handler".to_string(),
            },
        });

        assert_eq!(
            tile_item_state(&core, first),
            Some((String::new(), String::new()))
        );
        assert_eq!(
            tile_item_state(&core, second),
            Some(("handler".to_string(), String::new()))
        );
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchItems { tile_id: Some(effect_tile), query: Some(query), .. })
                if effect_tile == &second.0.to_string() && query == "handler"
        ));
    }
}
