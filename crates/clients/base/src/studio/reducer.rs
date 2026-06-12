use super::dto::{
    StudioAddProjectDto, StudioDimensionDto, StudioFileReadDto, StudioFileSpaceDto, StudioFilesDto,
    StudioItemsDto, StudioOpenProjectDto, StudioThreadsDto, StudioTopologyDto,
};
use super::effect::{StudioEffect, StudioEffectKind, StudioEffectResult, StudioEffectResultKind};
use super::event::{
    StudioAction, StudioEvent, StudioFilterField, StudioStackMoveDirection, StudioUiEvent,
};
use super::model::StudioCore;
use super::view_model::{
    action_for_focused_row, launcher_items_for, StudioMotionEventVm, StudioSplitAxisVm, StudioTone,
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
            StudioUiEvent::SetFilesRoot { .. } | StudioUiEvent::SetFilesPath { .. } => {
                // File tiles are content-bound; path state lives in the
                // view binding's params.
                Vec::new()
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
            StudioUiEvent::SetAtlasProjection { projection, root } => {
                self.ui.atlas.active_projection = projection;
                if projection.is_file_space() {
                    if let Some(root) = root {
                        self.ui.atlas.file_space_root = root;
                    }
                    self.ui.atlas.file_space_path.clear();
                    self.ui.atlas.set_lens(crate::atlas::AtlasLensVm::None);
                }
                self.bump_generation();
                match projection {
                    crate::atlas::AtlasProjectionVm::AiSpace => {
                        vec![self.emit(StudioEffectKind::FetchItems {
                            tile_id: None,
                            query: None,
                            kind: None,
                            limit: 1000,
                        })]
                    }
                    crate::atlas::AtlasProjectionVm::FileSpace => {
                        if self.has_project_bound() {
                            vec![self.emit(StudioEffectKind::FetchFileSpace {
                                root: self.ui.atlas.file_space_root.clone(),
                                path: self.ui.atlas.file_space_path.clone(),
                                max_depth: 8,
                                max_entries: 3000,
                            })]
                        } else {
                            Vec::new()
                        }
                    }
                }
            }
            StudioUiEvent::SetAtlasFileSpacePath { root, path } => {
                self.ui.atlas.active_projection = crate::atlas::AtlasProjectionVm::FileSpace;
                self.ui.atlas.file_space_root = root;
                self.ui.atlas.file_space_path = path;
                self.ui.atlas.set_lens(crate::atlas::AtlasLensVm::None);
                self.bump_generation();
                if self.has_project_bound() {
                    vec![self.emit(StudioEffectKind::FetchFileSpace {
                        root: self.ui.atlas.file_space_root.clone(),
                        path: self.ui.atlas.file_space_path.clone(),
                        max_depth: 8,
                        max_entries: 3000,
                    })]
                } else {
                    Vec::new()
                }
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
            StudioUiEvent::InsertInputChar { ch } => {
                if !self.input_surface_visible() {
                    return Vec::new();
                }
                self.ui.input.insert_char(ch);
                self.bump_generation();
                Vec::new()
            }
            StudioUiEvent::DeleteInputChar => {
                if !self.input_surface_visible() {
                    return Vec::new();
                }
                self.ui.input.delete_before_cursor();
                self.bump_generation();
                Vec::new()
            }
            StudioUiEvent::SetInputText { text, cursor } => {
                if !self.input_surface_visible() {
                    return Vec::new();
                }
                self.ui.input.set_text(text, cursor);
                self.bump_generation();
                Vec::new()
            }
            StudioUiEvent::CompleteInput => {
                if !self.input_surface_visible() {
                    return Vec::new();
                }
                let Some(records) =
                    self.data.commands.as_ref().and_then(|data| {
                        data.get("commands").and_then(serde_json::Value::as_array)
                    })
                else {
                    return Vec::new();
                };
                if let Some((text, cursor)) = super::tokenize::accept_slash_completion(
                    records,
                    &self.ui.input.text,
                    self.ui.input.cursor,
                ) {
                    self.ui.input.set_text(text, cursor);
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioUiEvent::SubmitInput => {
                if !self.input_surface_visible() {
                    return Vec::new();
                }
                let text = self.ui.input.text.trim().to_string();
                if text.is_empty() {
                    self.notice("Input is empty.", StudioTone::Warn);
                    return Vec::new();
                }
                if self.is_read_only() {
                    self.notice("This session is read-only.", StudioTone::Warn);
                    return Vec::new();
                }
                let line = match super::tokenize::classify_line(&text) {
                    Ok(line) => line,
                    Err(error) => {
                        self.notice(format!("Input parse error: {error}"), StudioTone::Warn);
                        return Vec::new();
                    }
                };
                match line {
                    super::tokenize::InputLine::SlashEmpty => {
                        self.notice(
                            "Type command tokens after / (e.g. /thread list).",
                            StudioTone::Neutral,
                        );
                        Vec::new()
                    }
                    super::tokenize::InputLine::Slash(tokens) => {
                        // Explicit grammar: tokens resolve + bind
                        // daemon-side (one invocation path for all
                        // clients). Slash bypasses the pinned route —
                        // explicit tokens win; no implicit thread/site.
                        vec![self.emit(StudioEffectKind::Invoke {
                            target: super::effect::InvokeRef::Tokens { tokens },
                            params: serde_json::json!({}),
                            route_seq: None,
                        })]
                    }
                    super::tokenize::InputLine::Plain(plain) => {
                        let fold = self.seat.fold();
                        let route = fold.input_route();
                        let route_seq = fold.seq_of(super::seat::KEY_INPUT_ROUTE);
                        let Some(invoke) = route.invoke.clone() else {
                            self.notice(
                                "Input has no target — the surface declares no route.",
                                StudioTone::Warn,
                            );
                            return Vec::new();
                        };
                        match invoke {
                            super::seat::InvokeTemplate::Service { item_ref } => {
                                // Ground verb: text bound whole to the
                                // service's declared input, never split.
                                let mut params = if route.params.is_object() {
                                    route.params.clone()
                                } else {
                                    serde_json::json!({})
                                };
                                params["input"] = serde_json::Value::String(plain);
                                if let Some(thread) = &route.thread {
                                    params["thread"] = serde_json::Value::String(thread.clone());
                                }
                                vec![self.emit(StudioEffectKind::Invoke {
                                    target: super::effect::InvokeRef::Ref { item_ref },
                                    params,
                                    route_seq,
                                })]
                            }
                            super::seat::InvokeTemplate::Command { mut tokens } => {
                                tokens.push(plain);
                                vec![self.emit(StudioEffectKind::Invoke {
                                    target: super::effect::InvokeRef::Tokens { tokens },
                                    params: serde_json::json!({}),
                                    route_seq,
                                })]
                            }
                            super::seat::InvokeTemplate::UiFacet { key } => {
                                let seq = self
                                    .seat
                                    .append_facet(key, serde_json::Value::String(plain));
                                let _ = seq;
                                self.ui.input.clear();
                                self.bump_generation();
                                Vec::new()
                            }
                        }
                    }
                }
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
                if items.get(selected).is_some_and(|item| !item.enabled) {
                    self.notice("Command is unavailable in this session.", StudioTone::Warn);
                    self.bump_generation();
                    return Vec::new();
                }
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
            StudioAction::InvokeAffordance {
                view_ref,
                affordance_id,
                record,
            } => self.invoke_affordance(&view_ref, &affordance_id, &record),
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
                    .unwrap_or(ViewSpec::Graph { graph_id: None });
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
                    .unwrap_or(ViewSpec::Graph { graph_id: None });
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
            StudioAction::ToggleDock { edge } => {
                let slot = match edge {
                    super::model::StudioDockEdge::Top => &mut self.ui.docks.top,
                    super::model::StudioDockEdge::Bottom => &mut self.ui.docks.bottom,
                    super::model::StudioDockEdge::Left => &mut self.ui.docks.left,
                    super::model::StudioDockEdge::Right => &mut self.ui.docks.right,
                };
                slot.visible = !slot.visible;
                let shown_view = if slot.visible {
                    match &slot.content {
                        super::model::StudioDockContent::View { view_ref } => {
                            Some(view_ref.clone())
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                let key = format!(
                    "dock:{}",
                    match edge {
                        super::model::StudioDockEdge::Top => "top",
                        super::model::StudioDockEdge::Bottom => "bottom",
                        super::model::StudioDockEdge::Left => "left",
                        super::model::StudioDockEdge::Right => "right",
                    }
                );
                self.bump_generation();
                shown_view
                    .and_then(|view_ref| self.emit_fetch_source_keyed(key, &view_ref))
                    .into_iter()
                    .collect()
            }
            StudioAction::ResizeFocused { direction } => {
                if self.workspace.resize_focused(direction) {
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioAction::SelectDimension => {
                self.seat.append_facet(
                    super::seat::KEY_SELECTION,
                    serde_json::json!({ "dimension": true }),
                );
                self.bump_generation();
                self.effects_for_facet(super::seat::KEY_SELECTION)
            }
            // Inspection IS selection: a facet write on the seat braid.
            StudioAction::InspectItem { canonical_ref } => {
                self.seat.append_facet(
                    super::seat::KEY_SELECTION,
                    serde_json::json!({ "item": canonical_ref }),
                );
                self.ensure_inspector_tile();
                self.bump_generation();
                self.effects_for_facet(super::seat::KEY_SELECTION)
            }
            StudioAction::EnterItemFolder { .. } => Vec::new(),
            StudioAction::InspectThread { thread_id } => {
                self.seat.append_facet(
                    super::seat::KEY_SELECTION,
                    serde_json::json!({ "thread": thread_id }),
                );
                self.ensure_inspector_tile();
                self.bump_generation();
                self.effects_for_facet(super::seat::KEY_SELECTION)
            }
            StudioAction::InspectSummary { title, detail } => {
                self.seat.append_facet(
                    super::seat::KEY_SELECTION,
                    serde_json::json!({ "summary": { "title": title, "detail": detail } }),
                );
                self.ensure_inspector_tile();
                self.bump_generation();
                self.effects_for_facet(super::seat::KEY_SELECTION)
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
            StudioAction::ListFiles { .. } => Vec::new(),
            StudioAction::ReadFile { root, path } => {
                if !self.has_project_bound() && file_root_requires_project(&root) {
                    self.notice("No project is bound to this session.", StudioTone::Warn);
                    return Vec::new();
                }
                self.data.file_read = None;
                self.seat.append_facet(
                    super::seat::KEY_SELECTION,
                    serde_json::json!({ "file": { "root": root, "path": path } }),
                );
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
                } else if self.has_pending_invoke(&item_ref, &parameters) {
                    self.notice(
                        format!("Run {item_ref} is already pending."),
                        StudioTone::Warn,
                    );
                    Vec::new()
                } else {
                    vec![self.emit(StudioEffectKind::InvokeAction {
                        command_id: item_ref,
                        args: parameters,
                    })]
                }
            }
            StudioAction::CancelThread { thread_id } => {
                if self.is_read_only() {
                    self.notice("This session is read-only.", StudioTone::Warn);
                    Vec::new()
                } else if self.has_pending_cancel(&thread_id) {
                    self.notice(
                        format!("Cancel {thread_id} is already pending."),
                        StudioTone::Warn,
                    );
                    Vec::new()
                } else {
                    vec![self.emit(StudioEffectKind::CancelThread { thread_id })]
                }
            }
        }
    }

    pub(crate) fn has_pending_invoke(
        &self,
        item_ref: &str,
        parameters: &serde_json::Value,
    ) -> bool {
        self.pending_effects.values().any(|kind| {
            matches!(
                kind,
                StudioEffectKind::InvokeAction { command_id, args }
                    if command_id == item_ref && args == parameters
            )
        })
    }

    pub(crate) fn has_pending_cancel(&self, thread_id: &str) -> bool {
        self.pending_effects.values().any(|kind| {
            matches!(
                kind,
                StudioEffectKind::CancelThread { thread_id: pending } if pending == thread_id
            )
        })
    }

    pub(crate) fn input_surface_visible(&self) -> bool {
        self.ui.docks.has_visible_input()
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
        // Item/file tiles are content-bound now; only the services
        // filter remains engine-local.
        let _ = tile;
        if matches!(field, StudioFilterField::ServicesQuery) {
            self.ui.filters.services_query = value;
            self.bump_generation();
        }
        Vec::new()
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
                self.workspace.tiles.get(tile_id).is_some_and(|tile| {
                    matches!(
                        &tile.view,
                        ViewSpec::Bound { view_ref } if view_ref == "view:ryeos/item/inspector"
                    )
                })
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
        if let Some(tile_id) = self.workspace.add_master_stack_tile(ViewSpec::Bound {
            view_ref: "view:ryeos/item/inspector".to_string(),
        }) {
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

    fn set_tile_cursor(&mut self, tile_id: TileId, index: usize) -> bool {
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
                .unwrap_or(ViewSpec::Graph { graph_id: None });
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
                .unwrap_or(ViewSpec::Graph { graph_id: None });
            self.effects_for_view(&view)
        } else {
            Vec::new()
        }
    }

    fn push_motion(&mut self, motion: StudioMotionEventVm) {
        self.ui.motion.push(motion);
    }

    /// Execute a content-declared affordance: resolve the binding,
    /// substitute the row, apply the plane. UI-plane writes append seat
    /// facets (braided when the seat thread is attached) and refetch
    /// every binding subscribed to that facet; rye-plane dispatches
    /// tokens through the one daemon path.
    fn invoke_affordance(
        &mut self,
        view_ref: &str,
        affordance_id: &str,
        record: &serde_json::Value,
    ) -> Vec<StudioEffect> {
        let Some(binding) = self.views.get(view_ref) else {
            return Vec::new();
        };
        let Some(affordance) = binding
            .affordances
            .iter()
            .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(affordance_id))
            .cloned()
        else {
            return Vec::new();
        };
        match super::content::resolve_affordance_invoke(&affordance, record) {
            Some(super::content::AffordanceInvoke::Ui {
                facet,
                value,
                merge,
            }) => {
                let next = if let Some(merge) = merge {
                    let mut current = self
                        .seat
                        .fold()
                        .get(&facet)
                        .cloned()
                        .unwrap_or(serde_json::json!({}));
                    if let (Some(target), Some(patch)) =
                        (current.as_object_mut(), merge.as_object())
                    {
                        for (key, val) in patch {
                            target.insert(key.clone(), val.clone());
                        }
                    }
                    current
                } else {
                    value.unwrap_or(serde_json::Value::Null)
                };
                self.seat.append_facet(facet.clone(), next);
                self.bump_generation();
                self.effects_for_facet(&facet)
            }
            Some(super::content::AffordanceInvoke::Rye { tokens, args }) => {
                vec![self.emit(StudioEffectKind::Invoke {
                    target: super::effect::InvokeRef::Tokens { tokens },
                    params: args,
                    route_seq: None,
                })]
            }
            None => Vec::new(),
        }
    }

    /// Facet write arrived: refetch every bound tile whose binding
    /// declares `refresh.on_facet: <key>` or whose source params
    /// reference the facet explicitly.
    pub fn effects_for_facet(&mut self, facet: &str) -> Vec<StudioEffect> {
        let targets: Vec<(crate::ids::TileId, String)> = self
            .workspace
            .layout
            .tile_ids()
            .into_iter()
            .filter_map(|tile_id| {
                let tile = self.workspace.tiles.get(&tile_id)?;
                let ViewSpec::Bound { view_ref } = &tile.view else {
                    return None;
                };
                let binding = self.views.get(view_ref)?;
                let subscribed = binding.refresh.get("on_facet").and_then(|v| v.as_str())
                    == Some(facet)
                    || binding
                        .source
                        .as_ref()
                        .map(|source| {
                            serde_json::to_string(&source.params)
                                .unwrap_or_default()
                                .contains(&format!("@facet:{facet}"))
                        })
                        .unwrap_or(false);
                subscribed.then(|| (tile_id, view_ref.clone()))
            })
            .collect();
        targets
            .into_iter()
            .filter_map(|(tile_id, view_ref)| self.emit_fetch_source(tile_id, &view_ref))
            .collect()
    }

    fn effects_for_view(&mut self, view: &ViewSpec) -> Vec<StudioEffect> {
        match view {
            ViewSpec::Bound { view_ref } => {
                let view_ref = view_ref.clone();
                let tile_id = self.workspace.focused_tile;
                self.emit_fetch_source(tile_id, &view_ref)
                    .into_iter()
                    .collect()
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
                effect_failure_notice(&expected, result.error.as_deref()),
                StudioTone::Danger,
            );
            return Vec::new();
        }

        if matches!(
            result.kind,
            StudioEffectResultKind::ActionInvocation
                | StudioEffectResultKind::ThreadCancelled
                | StudioEffectResultKind::Invoked
        ) {
            let data = result
                .data
                .as_ref()
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match result.kind {
                StudioEffectResultKind::ActionInvocation => {
                    self.notice(effect_success_notice(&expected, &data), StudioTone::Good);
                    return vec![
                        self.emit(StudioEffectKind::FetchDimension),
                        self.emit(StudioEffectKind::FetchThreads { limit: 100 }),
                    ];
                }
                StudioEffectResultKind::ThreadCancelled => {
                    self.notice(effect_success_notice(&expected, &data), StudioTone::Good);
                    let mut effects = vec![
                        self.emit(StudioEffectKind::FetchDimension),
                        self.emit(StudioEffectKind::FetchThreads { limit: 200 }),
                    ];
                    effects.extend(self.effects_for_hint("thread"));
                    return effects;
                }
                StudioEffectResultKind::Invoked => {
                    // Submit result contract: { thread_id?, delivery, notice? }.
                    let delivery = data
                        .get("delivery")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("launched");
                    let notice_text = data
                        .get("notice")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    if delivery == "refused" {
                        // Keep the buffer: the operator's text was not
                        // delivered.
                        self.notice(
                            notice_text.unwrap_or_else(|| "Delivery refused.".to_string()),
                            StudioTone::Warn,
                        );
                        return Vec::new();
                    }
                    self.ui.input.clear();
                    let thread_id = data
                        .get("thread_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    let Some(thread_id) = thread_id else {
                        self.notice(effect_success_notice(&expected, &data), StudioTone::Good);
                        self.bump_generation();
                        return Vec::new();
                    };
                    // Ratchet: the route is live state — a launch retargets
                    // the input at the produced thread so the next submit
                    // continues the chain. A stale result (route changed
                    // since issue) may notice but never retargets.
                    if let StudioEffectKind::Invoke { route_seq, .. } = &expected {
                        let fold = self.seat.fold();
                        if fold.seq_of(super::seat::KEY_INPUT_ROUTE) == *route_seq {
                            let mut route = fold.input_route();
                            route.thread = Some(thread_id.clone());
                            if let Ok(value) = serde_json::to_value(&route) {
                                self.seat.append_facet(super::seat::KEY_INPUT_ROUTE, value);
                            }
                        } else {
                            self.notice(
                                "Route changed since submit; not retargeting.",
                                StudioTone::Warn,
                            );
                        }
                    }
                    self.notice(format!("Thread {thread_id} launched."), StudioTone::Good);
                    let mut effects =
                        vec![self.emit(StudioEffectKind::FetchThreads { limit: 200 })];
                    effects.extend(self.effects_for_facet(super::seat::KEY_INPUT_ROUTE));
                    effects.extend(self.effects_for_hint("thread"));
                    return effects;
                }
                _ => unreachable!(),
            }
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
            StudioEffectResultKind::SourceData => {
                if let StudioEffectKind::FetchSource { tile_id, .. } = &expected {
                    self.data.sources.insert(tile_id.clone(), data);
                    self.bump_generation();
                }
            }
            StudioEffectResultKind::Commands => {
                // Open JSON: projected for completion, never typed
                // per-command.
                self.data.commands = Some(data);
                self.bump_generation();
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
            StudioEffectResultKind::ActionInvocation
            | StudioEffectResultKind::ThreadCancelled
            | StudioEffectResultKind::Invoked => {
                unreachable!("command results are handled before optional data extraction")
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
                self.data.file_space = None;
                self.data.tile_items.clear();
                self.data.files = None;
                self.data.tile_files.clear();
                self.data.file_read = None;
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
            StudioEffectResultKind::FileSpace => {
                self.apply_parsed::<StudioFileSpaceDto>(data, "file_space", |core, file_space| {
                    if effect_matches_current_file_space(Some(&expected), core, &file_space) {
                        core.data.file_space = Some(file_space);
                    }
                });
            }
            StudioEffectResultKind::FileRead => {
                self.apply_parsed::<StudioFileReadDto>(data, "file_read", |core, file_read| {
                    if effect_matches_current_file_read(Some(&expected), core, &file_read) {
                        core.data.file_read = Some(file_read);
                    }
                });
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
    launcher_items_for(core)
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

fn effect_success_notice(expected: &StudioEffectKind, data: &serde_json::Value) -> String {
    match expected {
        StudioEffectKind::InvokeAction { command_id, .. } => {
            let item_ref =
                json_field_text(data, &["command_id"]).unwrap_or_else(|| command_id.clone());
            format!("Ran {item_ref}.")
        }
        StudioEffectKind::CancelThread { thread_id } => {
            let thread =
                json_field_text(data, &["thread_id", "id"]).unwrap_or_else(|| thread_id.clone());
            format!("Cancelled {thread}.")
        }
        StudioEffectKind::Invoke { .. } => "Invocation completed.".to_string(),
        _ => "RyeOS command completed.".to_string(),
    }
}

fn effect_failure_notice(expected: &StudioEffectKind, error: Option<&str>) -> String {
    let reason = error
        .and_then(effect_error_summary)
        .unwrap_or_else(|| "RyeOS platform effect failed".to_string());
    match expected {
        StudioEffectKind::InvokeAction { command_id, .. } => {
            format!("Run {command_id} failed: {reason}")
        }
        StudioEffectKind::CancelThread { thread_id } => {
            format!("Cancel {thread_id} failed: {reason}")
        }
        StudioEffectKind::Invoke { .. } => format!("Invocation failed: {reason}"),
        _ => reason,
    }
}

fn effect_error_summary(raw: &str) -> Option<String> {
    structured_error_message(raw).or_else(|| {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn structured_error_message(raw: &str) -> Option<String> {
    raw.char_indices()
        .filter_map(|(index, ch)| (ch == '{').then_some(index))
        .find_map(|index| serde_json::from_str::<serde_json::Value>(&raw[index..]).ok())
        .and_then(|value| {
            json_field_text(&value, &["message", "error", "detail", "code"]).or_else(|| {
                value
                    .get("body")
                    .and_then(|body| json_field_text(body, &["message", "error", "detail", "code"]))
            })
        })
}

fn json_field_text(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value.get(*key)).map(|v| {
        v.as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string())
    })
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
            StudioEffectKind::FetchCommands,
            StudioEffectResultKind::Commands
        ) | (
            StudioEffectKind::FetchSource { .. },
            StudioEffectResultKind::SourceData
        ) | (
            StudioEffectKind::ListFiles { .. },
            StudioEffectResultKind::FilesList
        ) | (
            StudioEffectKind::FetchFileSpace { .. },
            StudioEffectResultKind::FileSpace
        ) | (
            StudioEffectKind::ReadFile { .. },
            StudioEffectResultKind::FileRead
        ) | (
            StudioEffectKind::InvokeAction { .. },
            StudioEffectResultKind::ActionInvocation
        ) | (
            StudioEffectKind::CancelThread { .. },
            StudioEffectResultKind::ThreadCancelled
        ) | (
            StudioEffectKind::Invoke { .. },
            StudioEffectResultKind::Invoked
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
            | StudioEffectKind::FetchFileSpace { .. }
            | StudioEffectKind::ListFiles { .. }
            | StudioEffectKind::ReadFile { .. }
            | StudioEffectKind::InvokeAction { .. }
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
    // Routes name engine views only; content views address by ref
    // (`#view:…`).
    let route = route.trim_start_matches('#');
    if let Some(view_ref) = route.strip_prefix("view:").map(|_| route) {
        return Some(ViewSpec::Bound {
            view_ref: view_ref.to_string(),
        });
    }
    match route {
        "" | "graph" => Some(ViewSpec::Graph { graph_id: None }),
        "atlas" => Some(ViewSpec::Atlas),
        _ => None,
    }
}

fn route_for_view(view: &ViewSpec) -> Option<&'static str> {
    match view {
        ViewSpec::Atlas => Some("atlas"),
        ViewSpec::Graph { graph_id: None } => Some("graph"),
        ViewSpec::Bound { .. } | ViewSpec::Graph { graph_id: Some(_) } => None,
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
    // Tile-scoped item filters died with the legacy item tiles; only
    // untargeted (atlas) fetches remain valid.
    let _ = (core, tile_id);
    query.is_none() && kind.is_none()
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
    // Tile-scoped file listings died with the legacy file tiles.
    let _ = (core, tile_id, root, path, files);
    false
}

fn effect_matches_current_file_space(
    expected: Option<&StudioEffectKind>,
    core: &StudioCore,
    file_space: &StudioFileSpaceDto,
) -> bool {
    let Some(StudioEffectKind::FetchFileSpace { root, path, .. }) = expected else {
        return true;
    };
    core.ui.atlas.active_projection.is_file_space()
        && root == &core.ui.atlas.file_space_root
        && path == &core.ui.atlas.file_space_path
        && root == &file_space.root
        && path == &file_space.path
}

fn effect_matches_current_file_read(
    expected: Option<&StudioEffectKind>,
    core: &StudioCore,
    file_read: &StudioFileReadDto,
) -> bool {
    let Some(StudioEffectKind::ReadFile { root, path }) = expected else {
        return true;
    };
    let selection_matches = core
        .seat
        .fold()
        .get(super::seat::KEY_SELECTION)
        .and_then(|selection| selection.get("file"))
        .is_some_and(|file| {
            file.get("root") == Some(&serde_json::Value::String(root.clone()))
                && file.get("path") == Some(&serde_json::Value::String(path.clone()))
        });
    if !selection_matches {
        return false;
    }
    root == &file_read.root && path == &file_read.path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::effect::StudioEffectResultKind;
    use crate::studio::event::{StudioEvent, StudioUiEvent};
    use crate::studio::model::{BrowserSession, BrowserViewport, StudioCore};
    use crate::studio::view_model::{build_view_model, launcher_items};
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

    fn atlas_session() -> BrowserSession {
        BrowserSession {
            surface_ref: "surface:ryeos/studio/atlas".to_string(),
            effective_surface: Some(serde_json::json!({
                "name": "studio-atlas",
                "version": "1.0.0",
                "layout": {
                    "root": "main",
                    "nodes": {
                        "main": { "type": "pane", "view": "graph" }
                    }
                },
                "ambient": {
                    "show_background": true,
                    "opacity": 1.0,
                    "mode": "namespace_atlas",
                    "atlas": { "style": "flat_2d" }
                }
            })),
            project_path: None,
            ..session()
        }
    }

    fn seed_view(core: &mut StudioCore, view_ref: &str) {
        core.views.insert(
            view_ref.to_string(),
            serde_json::from_value(serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:test/source", "params": {}, "collection": "rows" }
            }))
            .unwrap(),
        );
    }

    fn seed_view_value(core: &mut StudioCore, view_ref: &str, value: serde_json::Value) {
        core.views
            .insert(view_ref.to_string(), serde_json::from_value(value).unwrap());
    }

    #[test]
    fn invoke_affordance_ui_plane_writes_facet_and_refetches_subscribers() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/list",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:test/list", "params": {}, "collection": "rows" },
                "affordances": [{
                    "id": "select-item",
                    "invoke": {
                        "plane": "ui",
                        "facet": "selection",
                        "value": { "item": "{canonical_ref}" }
                    }
                }]
            }),
        );
        seed_view_value(
            &mut core,
            "view:test/inspector",
            serde_json::json!({
                "widget": "key_value",
                "source": {
                    "ref": "service:test/inspect",
                    "params": { "canonical_ref": "@facet:selection.item" }
                }
            }),
        );
        let tile_id = core.workspace.focused_tile.0.to_string();
        core.workspace.replace_focused_view(ViewSpec::Bound {
            view_ref: "view:test/inspector".to_string(),
        });

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:test/list".to_string(),
                    affordance_id: "select-item".to_string(),
                    record: serde_json::json!({ "canonical_ref": "tool:demo/run" }),
                },
            },
        });

        let fold = core.seat.fold();
        assert_eq!(fold.get("selection").unwrap()["item"], "tool:demo/run");
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchSource { tile_id: fetched_tile, source_ref, params })
                if fetched_tile == &tile_id
                    && source_ref == "service:test/inspect"
                    && params["canonical_ref"] == "tool:demo/run"
        ));
    }

    #[test]
    fn invoke_affordance_rye_plane_emits_token_invoke_with_args() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/threads",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:test/threads", "params": {}, "collection": "rows" },
                "affordances": [{
                    "id": "cancel",
                    "invoke": {
                        "plane": "rye",
                        "tokens": ["thread", "cancel"],
                        "args": { "thread_id": "{thread_id}" }
                    }
                }]
            }),
        );

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:test/threads".to_string(),
                    affordance_id: "cancel".to_string(),
                    record: serde_json::json!({ "thread_id": "T-demo" }),
                },
            },
        });

        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::Invoke {
                target: super::super::effect::InvokeRef::Tokens { tokens },
                params,
                route_seq: None,
            }) if tokens == &vec!["thread".to_string(), "cancel".to_string()]
                && params["thread_id"] == "T-demo"
        ));
    }

    #[test]
    fn invoke_affordance_ui_merge_folds_into_existing_facet() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "directive": "directive:demo/base"
            }),
        );
        seed_view_value(
            &mut core,
            "view:test/threads",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:test/threads", "params": {}, "collection": "rows" },
                "affordances": [{
                    "id": "aim-input",
                    "invoke": {
                        "plane": "ui",
                        "facet": "input.route",
                        "merge": { "thread": "{thread_id}" }
                    }
                }]
            }),
        );

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:test/threads".to_string(),
                    affordance_id: "aim-input".to_string(),
                    record: serde_json::json!({ "thread_id": "T-route" }),
                },
            },
        });

        let fold = core.seat.fold();
        let route = fold.get(crate::studio::seat::KEY_INPUT_ROUTE).unwrap();
        assert_eq!(route["directive"], "directive:demo/base");
        assert_eq!(route["thread"], "T-route");
    }

    #[test]
    fn start_emits_initial_effects() {
        let mut core = StudioCore::default();
        let effects = core.dispatch(StudioEvent::Start {
            session: session(),
            viewport: BrowserViewport::default(),
            now_ms: 0,
        });

        assert_eq!(effects.len(), 4);
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchDimension)));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchProjects)));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchCommands)));
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
    fn atlas_surface_fetches_items_and_builds_scene_atlas() {
        let mut core = StudioCore::new(atlas_session(), BrowserViewport::default(), 0);
        let effects = core.initial_effects();
        let items_id = effects
            .iter()
            .find(|effect| {
                matches!(
                    effect.kind,
                    StudioEffectKind::FetchItems {
                        tile_id: None,
                        query: None,
                        kind: None,
                        ..
                    }
                )
            })
            .map(|effect| effect.id)
            .expect("atlas surface should fetch atlas items");

        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: items_id,
                ok: true,
                kind: StudioEffectResultKind::Items,
                data: Some(serde_json::json!({
                    "schema_version": "studio.items.v1",
                    "counts": { "by_kind": {}, "by_space": {} },
                    "items": [{
                        "canonical_ref": "tool:demo/run",
                        "item_kind": "tool",
                        "bare_id": "demo/run",
                        "label": "run",
                        "namespace": "demo",
                        "source_path": "/tmp/.ai/tools/demo/run.yaml",
                        "space": "project",
                        "executable": true
                    }]
                })),
                error: None,
            },
        });

        let scene = crate::studio::scene_model::build_scene_model(&core);
        let atlas = scene.atlas.expect("atlas surface should build scene atlas");
        assert_eq!(atlas.root_label, ".ai");
        assert!(atlas
            .nodes
            .iter()
            .flat_map(|node| &node.stack)
            .any(|item| item.canonical_ref == "tool:demo/run"));
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
    fn launcher_includes_engine_views_and_content_library() {
        let mut core = StudioCore::default();
        core.views.insert(
            "view:ryeos/threads/list".to_string(),
            serde_json::from_value(serde_json::json!({
                "widget": "rows",
                "description": "Thread list"
            }))
            .unwrap(),
        );
        let items = launcher_items(&core);
        assert!(items.iter().any(|item| {
            item.label == "Graph"
                && matches!(
                    item.action,
                    StudioAction::OpenView {
                        view: ViewSpec::Graph { graph_id: None }
                    }
                )
        }));
        // Content views launch as Bound tiles, labeled by ref.
        assert!(items.iter().any(|item| {
            item.label == "ryeos/threads/list"
                && matches!(
                    &item.action,
                    StudioAction::OpenView {
                        view: ViewSpec::Bound { view_ref }
                    } if view_ref == "view:ryeos/threads/list"
                )
        }));
    }

    #[test]
    fn launcher_includes_shared_dock_toggles() {
        let core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let vm = build_view_model(&core);

        assert!(vm.launcher.items.iter().any(|item| {
            item.label == "Hide input dock"
                && matches!(
                    item.action,
                    StudioAction::ToggleDock {
                        edge: crate::studio::model::StudioDockEdge::Bottom
                    }
                )
        }));
        assert!(vm.launcher.items.iter().any(|item| {
            item.label == "Show directive threads dock"
                && matches!(
                    item.action,
                    StudioAction::ToggleDock {
                        edge: crate::studio::model::StudioDockEdge::Left
                    }
                )
        }));
    }

    #[test]
    fn toggle_dock_updates_workspace_dock_vm() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:ui/studio/threads", "params": {}, "collection": "rows" }
            }),
        );
        assert!(build_view_model(&core).workspace.docks.left.is_none());

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ToggleDock {
                    edge: crate::studio::model::StudioDockEdge::Left,
                },
            },
        });

        assert!(build_view_model(&core).workspace.docks.left.is_some());
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchSource { tile_id, source_ref, .. })
                if tile_id == "dock:left" && source_ref == "service:ui/studio/threads"
        ));
    }

    #[test]
    fn directive_threads_dock_renders_bound_view_rows() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.ui.docks.left.visible = true;
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:ui/studio/threads", "params": {}, "collection": "rows" },
                "projections": { "primary": "thread_id", "meta": "item_ref" }
            }),
        );
        core.data.sources.insert(
            "dock:left".to_string(),
            serde_json::json!({
                "rows": [{
                    "thread_id": "T-running",
                    "item_ref": "directive:demo/chat",
                    "status": "running"
                }]
            }),
        );

        let vm = build_view_model(&core);
        let dock = vm.workspace.docks.left.expect("left dock");
        match dock.view {
            crate::studio::view_model::StudioDockViewVm::View(
                crate::studio::view_model::StudioViewVm::Rows { rows, .. },
            ) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].primary, "T-running");
                assert_eq!(rows[0].meta.as_deref(), Some("directive:demo/chat"));
            }
            other => panic!("expected bound rows dock view, got {other:?}"),
        }
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
    fn route_change_focuses_workspace_view() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:ryeos/items/space");
        let effects = core.dispatch(StudioEvent::RouteChanged {
            route: "view:ryeos/items/space".to_string(),
        });

        assert_eq!(
            core.workspace.focused_view(),
            Some(&ViewSpec::Bound {
                view_ref: "view:ryeos/items/space".to_string()
            })
        );
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchSource { .. })
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
    fn writable_execute_invokes_action_endpoint() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ExecuteItem {
                    item_ref: "tool:demo/run".to_string(),
                    parameters: serde_json::json!({ "target": "demo" }),
                },
            },
        });

        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::InvokeAction { command_id, args })
                if command_id == "tool:demo/run" && args["target"] == "demo"
        ));
    }

    fn seed_service_route(core: &mut StudioCore) {
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "params": { "directive": "directive:demo/base" }
            }),
        );
    }

    #[test]
    fn writable_input_submit_emits_invoke_with_text_bound_whole() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        core.ui.input.set_text("  run this  ".to_string(), 12);

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SubmitInput,
        });

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::Invoke {
                target: crate::studio::effect::InvokeRef::Ref { item_ref },
                params,
                route_seq: Some(_),
            }) if item_ref == "service:threads/input"
                && params["input"] == "run this"
                && params["directive"] == "directive:demo/base"
        ));
        // Buffer survives until delivery succeeds.
        assert_eq!(core.ui.input.text, "  run this  ");
    }

    #[test]
    fn complete_input_accepts_top_slash_candidate() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.data.commands = Some(serde_json::json!({
            "commands": [
                { "invocable": true, "tokens": ["thread", "list"], "description": "List threads" },
                { "invocable": true, "tokens": ["thread", "get"], "description": "Get thread", "arguments": [{ "name": "thread_id" }] }
            ]
        }));
        core.ui.input.set_text("/thr".to_string(), 4);

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CompleteInput,
        });

        assert!(effects.is_empty());
        assert_eq!(core.ui.input.text, "/thread ");
        assert_eq!(core.ui.input.cursor, "/thread ".len());
    }

    #[test]
    fn submit_without_route_warns_and_emits_nothing() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.ui.input.set_text("hello".to_string(), 5);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SubmitInput,
        });
        assert!(effects.is_empty());
        assert!(core
            .ui
            .notices
            .last()
            .is_some_and(|notice| notice.message.contains("no target")));
    }

    #[test]
    fn input_submit_launched_clears_buffer_and_ratchets_route() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        core.ui.input.set_text("run this".to_string(), 8);
        let effect = core
            .dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");

        let followups = core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: effect.id,
                ok: true,
                kind: StudioEffectResultKind::Invoked,
                data: Some(serde_json::json!({
                    "thread_id": "T-9",
                    "delivery": "launched"
                })),
                error: None,
            },
        });

        assert!(followups
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchThreads { limit: 200 })));
        assert!(core.ui.input.text.is_empty());
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread.as_deref(), Some("T-9"));
        // Pinned invocation survives the ratchet.
        assert!(route.has_target());
    }

    #[test]
    fn stale_invoke_result_never_retargets_newer_route() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        core.ui.input.set_text("first".to_string(), 5);
        let effect = core
            .dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");

        // Route changes after the submit was issued.
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "thread": "T-other"
            }),
        );

        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: effect.id,
                ok: true,
                kind: StudioEffectResultKind::Invoked,
                data: Some(serde_json::json!({
                    "thread_id": "T-stale",
                    "delivery": "launched"
                })),
                error: None,
            },
        });

        let route = core.seat.fold().input_route();
        assert_eq!(route.thread.as_deref(), Some("T-other"));
    }

    #[test]
    fn refused_delivery_keeps_buffer() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        core.ui.input.set_text("hold on".to_string(), 7);
        let effect = core
            .dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");

        let followups = core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: effect.id,
                ok: true,
                kind: StudioEffectResultKind::Invoked,
                data: Some(serde_json::json!({
                    "delivery": "refused",
                    "notice": "Thread is live; delivery refused."
                })),
                error: None,
            },
        });

        assert!(followups.is_empty());
        assert_eq!(core.ui.input.text, "hold on");
        assert!(core
            .ui
            .notices
            .last()
            .is_some_and(|notice| notice.message.contains("refused")));
    }

    #[test]
    fn duplicate_execute_is_rejected_while_pending() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let first = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ExecuteItem {
                    item_ref: "tool:demo/run".to_string(),
                    parameters: serde_json::json!({ "target": "demo" }),
                },
            },
        });
        assert_eq!(first.len(), 1);

        let duplicate = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ExecuteItem {
                    item_ref: "tool:demo/run".to_string(),
                    parameters: serde_json::json!({ "target": "demo" }),
                },
            },
        });

        assert!(duplicate.is_empty());
        assert!(core
            .ui
            .notices
            .iter()
            .any(|notice| notice.message == "Run tool:demo/run is already pending."));
    }

    #[test]
    fn action_invocation_result_notices_and_refreshes() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let invoke = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ExecuteItem {
                    item_ref: "tool:demo/run".to_string(),
                    parameters: serde_json::json!({}),
                },
            },
        });
        let invoke_id = invoke
            .first()
            .map(|effect| effect.id)
            .expect("execute should emit invoke effect");

        let effects = core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: invoke_id,
                ok: true,
                kind: StudioEffectResultKind::ActionInvocation,
                data: Some(serde_json::json!({
                    "status": "executed",
                    "command_id": "tool:demo/run",
                    "invocation_id": "inv-1"
                })),
                error: None,
            },
        });

        assert!(core
            .ui
            .notices
            .iter()
            .any(|notice| notice.message == "Ran tool:demo/run."));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchDimension)));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchThreads { limit: 100 })));
    }

    #[test]
    fn action_invocation_failure_names_item_and_structured_error() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let invoke = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ExecuteItem {
                    item_ref: "tool:demo/run".to_string(),
                    parameters: serde_json::json!({}),
                },
            },
        });
        let invoke_id = invoke
            .first()
            .map(|effect| effect.id)
            .expect("execute should emit invoke effect");

        let effects = core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: invoke_id,
                ok: false,
                kind: StudioEffectResultKind::ActionInvocation,
                data: None,
                error: Some(
                    "/ui/api/actions/invoke: 500 {\"message\":\"capability denied\"}".to_string(),
                ),
            },
        });

        assert!(effects.is_empty());
        assert!(core.ui.notices.iter().any(|notice| {
            notice.message == "Run tool:demo/run failed: capability denied"
                && notice.tone == StudioTone::Danger
        }));
    }

    #[test]
    fn action_invocation_success_without_body_still_notices_and_refreshes() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let invoke = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ExecuteItem {
                    item_ref: "tool:demo/run".to_string(),
                    parameters: serde_json::json!({}),
                },
            },
        });
        let invoke_id = invoke
            .first()
            .map(|effect| effect.id)
            .expect("execute should emit invoke effect");

        let effects = core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: invoke_id,
                ok: true,
                kind: StudioEffectResultKind::ActionInvocation,
                data: None,
                error: None,
            },
        });

        assert!(core
            .ui
            .notices
            .iter()
            .any(|notice| notice.message == "Ran tool:demo/run."));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchDimension)));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchThreads { limit: 100 })));
    }

    #[test]
    fn writable_cancel_thread_emits_cancel_effect() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CancelThread {
                    thread_id: "T-demo".to_string(),
                },
            },
        });

        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::CancelThread { thread_id }) if thread_id == "T-demo"
        ));
    }

    #[test]
    fn duplicate_cancel_is_rejected_while_pending() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let first = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CancelThread {
                    thread_id: "T-demo".to_string(),
                },
            },
        });
        assert_eq!(first.len(), 1);

        let duplicate = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CancelThread {
                    thread_id: "T-demo".to_string(),
                },
            },
        });

        assert!(duplicate.is_empty());
        assert!(core
            .ui
            .notices
            .iter()
            .any(|notice| notice.message == "Cancel T-demo is already pending."));
    }

    #[test]
    fn thread_cancelled_result_notices_and_refreshes() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let cancel = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CancelThread {
                    thread_id: "T-demo".to_string(),
                },
            },
        });
        let cancel_id = cancel
            .first()
            .map(|effect| effect.id)
            .expect("cancel should emit effect");

        let effects = core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: cancel_id,
                ok: true,
                kind: StudioEffectResultKind::ThreadCancelled,
                data: Some(serde_json::json!({
                    "thread_id": "T-demo",
                    "status": "cancelled"
                })),
                error: None,
            },
        });

        assert!(core
            .ui
            .notices
            .iter()
            .any(|notice| notice.message == "Cancelled T-demo."));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchDimension)));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchThreads { limit: 200 })));
    }

    #[test]
    fn thread_cancel_failure_names_thread() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let cancel = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CancelThread {
                    thread_id: "T-demo".to_string(),
                },
            },
        });
        let cancel_id = cancel
            .first()
            .map(|effect| effect.id)
            .expect("cancel should emit effect");

        let effects = core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: cancel_id,
                ok: false,
                kind: StudioEffectResultKind::ThreadCancelled,
                data: None,
                error: Some("thread already finished".to_string()),
            },
        });

        assert!(effects.is_empty());
        assert!(core.ui.notices.iter().any(|notice| {
            notice.message == "Cancel T-demo failed: thread already finished"
                && notice.tone == StudioTone::Danger
        }));
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
        seed_view(&mut core, "view:ryeos/items/space");
        seed_view(&mut core, "view:test/services");
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Bound {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        let before = core.workspace.layout.tile_ids().len();
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Bound {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });

        assert_eq!(core.workspace.layout.tile_ids().len(), before + 1);
        assert!(matches!(
            core.workspace.focused_view(),
            Some(ViewSpec::Bound { view_ref }) if view_ref == "view:test/services"
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
            Some(StudioEffectKind::FetchSource { .. })
        ));
    }

    #[test]
    fn open_new_view_allows_duplicate_workspace_tiles() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:ryeos/items/space");
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Bound {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        let before = core.workspace.layout.tile_ids().len();
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::Bound {
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
                matches!(&tile.view, ViewSpec::Bound { view_ref } if view_ref == "view:ryeos/items/space")
            })
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
            Some(StudioEffectKind::FetchSource { .. })
        ));
    }

    #[test]
    fn close_tile_closes_target_tile() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Bound {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::Bound {
                        view_ref: "view:ryeos/threads/list".to_string(),
                    },
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
                    view: ViewSpec::Bound {
                        view_ref: "view:test/services".to_string(),
                    },
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
        core.views.insert(
            "view:ryeos/items/space".to_string(),
            serde_json::from_value(serde_json::json!({
                "widget": "rows",
                "description": "Item space",
                "source": { "ref": "service:ui/studio/items/list", "params": {}, "collection": "items" }
            }))
            .unwrap(),
        );
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
            Some(ViewSpec::Bound { view_ref }) if view_ref == "view:ryeos/items/space"
        ));
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchSource { .. })
        ));
    }

    #[test]
    fn arrow_focus_uses_workspace_geometry() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Bound {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        let left = core.workspace.focused_tile;
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::Bound {
                        view_ref: "view:ryeos/threads/list".to_string(),
                    },
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
                    view: ViewSpec::Bound {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::Bound {
                        view_ref: "view:ryeos/threads/list".to_string(),
                    },
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::Bound {
                        view_ref: "view:test/files".to_string(),
                    },
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
        seed_view(&mut core, "view:test/services");
        seed_view(&mut core, "view:test/files");
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Bound {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::Bound {
                        view_ref: "view:test/files".to_string(),
                    },
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
                    view: ViewSpec::Bound {
                        view_ref: "view:test/files".to_string(),
                    },
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec::Bound {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
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
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchSource { .. })));
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
        seed_view(&mut core, "view:ryeos/items/space");
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::Bound {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        let fetch_items = effects
            .iter()
            .find(|effect| matches!(effect.kind, StudioEffectKind::FetchSource { .. }))
            .expect("open bound view should fetch its source");

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
}
