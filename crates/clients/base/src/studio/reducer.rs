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
use crate::surface::ArrangeSpec;
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
            StudioEvent::ThreadTail {
                thread_id,
                event_type,
                payload,
            } => self.apply_thread_tail(&thread_id, &event_type, &payload),
            StudioEvent::Tick { now_ms } => {
                self.runtime.now_ms = now_ms;
                // The frame clock advances `generation` so generation-keyed
                // motion (the backdrop twinkle, via the generic scene
                // renderer) steps each tick. The loop already repaints on
                // tick; bumping generation is what makes the step visible.
                self.bump_generation();
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
            StudioUiEvent::SetAtlasLayerVisible {
                tile_id,
                kind,
                visible,
            } => {
                self.atlas_target_mut(&tile_id)
                    .set_layer_visible(kind, visible);
                self.bump_generation();
                Vec::new()
            }
            StudioUiEvent::SetAtlasLens { tile_id, lens } => {
                self.atlas_target_mut(&tile_id).set_lens(lens);
                self.bump_generation();
                Vec::new()
            }
            StudioUiEvent::SetAtlasProjection {
                tile_id,
                projection,
                root,
            } => {
                {
                    let atlas = self.atlas_target_mut(&tile_id);
                    atlas.active_projection = projection;
                    if projection.is_file_space() {
                        if let Some(root) = root {
                            atlas.file_space_root = root;
                        }
                        atlas.file_space_path.clear();
                        atlas.set_lens(crate::atlas::AtlasLensVm::None);
                    }
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
                            let (root, path) = {
                                let atlas = self.atlas_target(&tile_id);
                                (atlas.file_space_root.clone(), atlas.file_space_path.clone())
                            };
                            vec![self.emit(StudioEffectKind::FetchFileSpace {
                                tile_id: tile_id.clone(),
                                root,
                                path,
                                max_depth: 8,
                                max_entries: 3000,
                            })]
                        } else {
                            Vec::new()
                        }
                    }
                }
            }
            StudioUiEvent::SetAtlasFileSpacePath {
                tile_id,
                root,
                path,
            } => {
                {
                    let atlas = self.atlas_target_mut(&tile_id);
                    atlas.active_projection = crate::atlas::AtlasProjectionVm::FileSpace;
                    atlas.file_space_root = root;
                    atlas.file_space_path = path;
                    atlas.set_lens(crate::atlas::AtlasLensVm::None);
                }
                self.bump_generation();
                if self.has_project_bound() {
                    let (root, path) = {
                        let atlas = self.atlas_target(&tile_id);
                        (atlas.file_space_root.clone(), atlas.file_space_path.clone())
                    };
                    vec![self.emit(StudioEffectKind::FetchFileSpace {
                        tile_id: tile_id.clone(),
                        root,
                        path,
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
            StudioUiEvent::OpenHelp => {
                self.ui.help_open = true;
                self.bump_generation();
                Vec::new()
            }
            StudioUiEvent::CloseHelp => {
                if self.ui.help_open {
                    self.ui.help_open = false;
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
                let Some(buffer) = self.focused_input_buffer_mut() else {
                    return Vec::new();
                };
                buffer.insert_char(ch);
                self.bump_generation();
                self.effects_for_focused_feeds()
            }
            StudioUiEvent::DeleteInputChar => {
                let Some(buffer) = self.focused_input_buffer_mut() else {
                    return Vec::new();
                };
                buffer.delete_before_cursor();
                self.bump_generation();
                self.effects_for_focused_feeds()
            }
            StudioUiEvent::SetInputText { text, cursor } => {
                let Some(buffer) = self.focused_input_buffer_mut() else {
                    return Vec::new();
                };
                buffer.set_text(text, cursor);
                self.bump_generation();
                self.effects_for_focused_feeds()
            }
            StudioUiEvent::CompleteInput => {
                let Some((key, view_ref)) = self.focused_input_instance() else {
                    return Vec::new();
                };
                let buffer = self
                    .ui
                    .input_buffers
                    .get(&key.storage_key())
                    .cloned()
                    .unwrap_or_default();
                // An inline @-mention under the cursor wins; otherwise the
                // line-start / command grammar. Both resolve to an optional
                // (text, cursor) before the buffer is mutated.
                let completed = if super::tokenize::active_mention(&buffer.text, buffer.cursor)
                    .is_some()
                {
                    let records = self
                        .views
                        .get(&view_ref)
                        .and_then(|binding| binding.input.as_ref())
                        .and_then(|input| input.mentions.as_ref())
                        .and_then(|mentions| {
                            let response = self.data.sources.get(
                                &super::content::mention_source_key(&view_ref, &key.input_id),
                            )?;
                            Some(super::content::project_mentions(mentions, response))
                        })
                        .unwrap_or_default();
                    super::tokenize::accept_mention_completion(&records, &buffer.text, buffer.cursor)
                } else {
                    self.data
                        .commands
                        .as_ref()
                        .and_then(|data| data.get("commands").and_then(serde_json::Value::as_array))
                        .and_then(|records| {
                            super::tokenize::accept_slash_completion(
                                records,
                                &buffer.text,
                                buffer.cursor,
                            )
                        })
                };
                if let Some((text, cursor)) = completed {
                    if let Some(buffer) = self.focused_input_buffer_mut() {
                        buffer.set_text(text, cursor);
                        self.bump_generation();
                    }
                }
                Vec::new()
            }
            StudioUiEvent::CycleInputTarget { forward } => self.cycle_input_target(forward),
            StudioUiEvent::InterruptHead => {
                // Esc while the head thread works → cancel it through the
                // thread-control channel (reuses the read-only + dedup guards).
                // No-op if there's no running head.
                let Some(head) = self.seat.fold().input_route().thread else {
                    return Vec::new();
                };
                if !self.head_thread_running(&head) {
                    return Vec::new();
                }
                self.dispatch_action(StudioAction::CancelThread { thread_id: head })
            }
            StudioUiEvent::SubmitInput => self.submit_focused_input(false),
            StudioUiEvent::SubmitInputInterrupt => self.submit_focused_input(true),
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
            StudioUiEvent::SetFold {
                tile_id,
                section,
                collapsed,
            } => {
                let Some(tile_id) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                if self.set_tile_fold(tile_id, section, collapsed) {
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
                // Single-lens surfaces have no "another tile": a new-view
                // request collapses to replacing the one center lens.
                if self.workspace.tiling.mode == crate::surface::TilingModeSpec::SingleLens {
                    self.open_view(view)
                } else {
                    let effects = self.add_center_tile(view);
                    self.bump_generation();
                    effects
                }
            }
            StudioAction::CloseFocused => {
                if self.close_tile_or_empty(self.workspace.focused_tile) {
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioAction::CloseTile { tile_id } => {
                let Some(tile_id) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                if self.close_tile_or_empty(tile_id) {
                    self.bump_generation();
                }
                Vec::new()
            }
            StudioAction::ToggleFocusedMaster => {
                if self.workspace.zoom_focused() {
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
                // Toggling flips a surface-declared slot open/closed; a
                // closed slot frees its space. Absent edges have no slot.
                let Some(slot) = self.ui.docks.slot_mut(edge) else {
                    return Vec::new();
                };
                slot.visible = !slot.visible;
                let shown_view = if slot.visible {
                    let super::model::StudioDockContent::View { view_ref } = &slot.content;
                    Some(view_ref.clone())
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
                    .map(|view_ref| self.emit_fetch_source_keyed(key, &view_ref))
                    .unwrap_or_default()
            }
            StudioAction::ResizeFocused { direction } => {
                if self.workspace.resize_master(direction) {
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
            // Inspection IS selection: a facet write, peer to `input.route`.
            // The engine never opens or names the inspector — it's a view that
            // reads `@facet:selection.*` and refreshes `on_facet: selection`,
            // shown as a slot or a lens like any other facet-bound view.
            StudioAction::InspectItem { canonical_ref } => {
                self.seat.append_facet(
                    super::seat::KEY_SELECTION,
                    serde_json::json!({ "item": canonical_ref }),
                );
                self.bump_generation();
                self.effects_for_facet(super::seat::KEY_SELECTION)
            }
            StudioAction::EnterItemFolder { .. } => Vec::new(),
            StudioAction::InspectThread { thread_id } => {
                self.seat.append_facet(
                    super::seat::KEY_SELECTION,
                    serde_json::json!({ "thread": thread_id }),
                );
                self.bump_generation();
                self.effects_for_facet(super::seat::KEY_SELECTION)
            }
            StudioAction::AimThread { thread_id } => self.apply_ui_affordance(
                super::seat::KEY_INPUT_ROUTE.to_string(),
                None,
                Some(serde_json::json!({ "thread": thread_id })),
                None,
            ),
            StudioAction::InspectSummary { title, detail } => {
                self.seat.append_facet(
                    super::seat::KEY_SELECTION,
                    serde_json::json!({ "summary": { "title": title, "detail": detail } }),
                );
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
            StudioAction::SubmitThreadCommand { command } => {
                if self.is_read_only() {
                    self.notice("This session is read-only.", StudioTone::Warn);
                    Vec::new()
                } else if let Some(thread_id) = self.seat.fold().input_route().thread {
                    // Steer the head thread through the shared control channel.
                    // Authority == the CLI's `commands submit`; see
                    // .tmp/thread-authorization-review.md for the authz model.
                    vec![self.emit(StudioEffectKind::Invoke {
                        target: super::effect::InvokeRef::Ref {
                            item_ref: "service:commands/submit".to_string(),
                        },
                        params: serde_json::json!({
                            "thread_id": thread_id,
                            "command_type": command.as_str(),
                        }),
                        route_seq: None,
                        ratchet_on_thread_id: false,
                    })]
                } else {
                    self.notice(
                        format!("No active thread to {}.", command.as_str()),
                        StudioTone::Warn,
                    );
                    Vec::new()
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

    /// Refetch the focused instance's source when its input declares
    /// `feeds` (the buffer is a writer of one source param). Debounce is a
    /// renderer/transport concern; the reducer emits the refetch and the
    /// binding carries `debounce_ms` for the renderer to honour.
    fn effects_for_focused_feeds(&mut self) -> Vec<StudioEffect> {
        let Some((key, view_ref)) = self.focused_input_instance() else {
            return Vec::new();
        };
        let feeds = self
            .views
            .get(&view_ref)
            .and_then(|binding| binding.input.as_ref())
            .and_then(|input| input.feeds.as_ref())
            .is_some();
        if !feeds {
            return Vec::new();
        }
        self.emit_fetch_source_keyed(key.view_instance_id.clone(), &view_ref)
            .into_iter()
            .collect()
    }

    /// Submit the focused instance's input buffer. Three modes: `feeds`
    /// (no submit — buffer is live), `submit: <affordance>` (fire it with
    /// `{value}`), `submit: route` (the engine route-fold: classification
    /// + route_seq + ratchet, unchanged).
    /// `interrupt` selects the live-delivery intent for a submit that routes at a
    /// RUNNING thread: `true` cuts the in-flight cognition (forceful redirect),
    /// `false` steers (folds at the next turn boundary). It is carried as the
    /// `intent` param and ignored by the daemon on non-running targets (fresh
    /// launch / settled continuation land at boundaries by construction).
    fn submit_focused_input(&mut self, interrupt: bool) -> Vec<StudioEffect> {
        let Some((key, view_ref)) = self.focused_input_instance() else {
            return Vec::new();
        };
        let Some(input) = self
            .views
            .get(&view_ref)
            .and_then(|binding| binding.input.clone())
        else {
            return Vec::new();
        };
        // `feeds`-only inputs have no submit — Enter does nothing durable.
        if input.submit.is_none() {
            return Vec::new();
        }
        let text = self
            .ui
            .input_buffers
            .get(&key.storage_key())
            .map(|buffer| buffer.text.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            self.notice("Input is empty.", StudioTone::Warn);
            return Vec::new();
        }

        if let Some(affordance_id) = input.submit_affordance() {
            // Mode 2: Enter fires a content affordance with `{value}`.
            if self.is_read_only() {
                self.notice("This session is read-only.", StudioTone::Warn);
                return Vec::new();
            }
            return self.invoke_input_affordance(&view_ref, affordance_id, &text);
        }

        // Mode 3: `submit: route` — the existing engine route-fold.
        debug_assert!(input.submits_to_route());
        self.submit_route(&text, interrupt)
    }

    /// The open conversation chains the input can target, as
    /// `(chain_root_id, head_thread_id)` in the thread list's order
    /// (most-recent first, as the daemon returns them). The head of a chain
    /// is the thread no other thread continues from (its `thread_id` is not
    /// any sibling's `upstream_thread_id`); a follow-up braids onto it.
    /// The daemon-authored `execution.supports_continuation` for a thread, read
    /// from the fetched thread projections (`threads` data carries it per row
    /// via the thread-view layer). `None` = the thread isn't in the fetched
    /// data (e.g. a just-launched thread before the list refresh) or carries no
    /// execution facts — callers treat unknown optimistically, distrusting only
    /// an explicit `Some(false)`. Symmetric counterpart to
    /// [`Self::thread_supports_operator_followup`] — the machine-continuation
    /// fact. Operator surfaces gate on operator-follow-up, so this isn't gated on
    /// in prod today; kept (and tested) as the substrate accessor for a future
    /// machine-continuation affordance.
    #[allow(dead_code)]
    pub(crate) fn thread_supports_continuation(&self, thread_id: &str) -> Option<bool> {
        let row = self
            .data
            .threads
            .as_ref()?
            .threads
            .iter()
            .find(|row| {
                row.get("thread_id").and_then(serde_json::Value::as_str) == Some(thread_id)
            })?;
        // Typed execution facts (no `supports_continuation` string literal).
        // `None` = no execution object on the row (unknown), distinct from an
        // explicit `Some(false)`.
        let facts: super::dto::ExecutionFacts =
            serde_json::from_value(row.get("execution")?.clone()).ok()?;
        Some(facts.supports_continuation)
    }

    /// The daemon-authored `execution.supports_operator_followup` for a thread.
    /// Gates OPERATOR-input targeting/labels: a graph is continuation-capable but
    /// machine-only, so it accepts no operator input even though it continues.
    /// Same unknown-optimistic semantics as [`Self::thread_supports_continuation`]
    /// — distrust only an explicit `Some(false)`.
    pub(crate) fn thread_supports_operator_followup(&self, thread_id: &str) -> Option<bool> {
        let row = self.data.threads.as_ref()?.threads.iter().find(|row| {
            row.get("thread_id").and_then(serde_json::Value::as_str) == Some(thread_id)
        })?;
        let facts: super::dto::ExecutionFacts =
            serde_json::from_value(row.get("execution")?.clone()).ok()?;
        Some(facts.supports_operator_followup)
    }

    fn input_target_chains(&self) -> Vec<(String, String)> {
        let Some(threads) = self.data.threads.as_ref() else {
            return Vec::new();
        };
        let rows = &threads.threads;
        let upstreams: std::collections::HashSet<&str> = rows
            .iter()
            .filter_map(|t| t.get("upstream_thread_id").and_then(serde_json::Value::as_str))
            .collect();
        let mut out: Vec<(String, String)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            let Some(root) = row.get("chain_root_id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !seen.insert(root) {
                continue;
            }
            // Head = the chain member nothing continues from; fall back to
            // the root if the list is partial.
            let head = rows
                .iter()
                .filter(|x| x.get("chain_root_id").and_then(serde_json::Value::as_str) == Some(root))
                .filter_map(|x| x.get("thread_id").and_then(serde_json::Value::as_str))
                .find(|id| !upstreams.contains(id))
                .unwrap_or(root);
            // Only offer chains whose head accepts OPERATOR input — gate on
            // `execution.supports_operator_followup`, not `supports_continuation`
            // (a graph continues by machine but takes no operator input).
            // Distrust only an explicit `false`; unknown stays offered (the
            // daemon refuses a real non-followup submit anyway).
            if self.thread_supports_operator_followup(head) == Some(false) {
                continue;
            }
            out.push((root.to_string(), head.to_string()));
        }
        out
    }

    /// The focused input's declared targeting capability, if any. The cycle
    /// only acts when the FOCUSED input owns a `target` — the capability is
    /// content-declared, never assumed for every route-input.
    fn focused_input_target_cycle(&self) -> Option<super::content::InputTargetCycle> {
        self.focused_input_instance()
            .and_then(|(_, view_ref)| self.views.get(&view_ref))
            .and_then(|binding| binding.input.as_ref())
            // Defense-in-depth: targeting retargets the ROUTE, so it is only
            // meaningful on a route-submit input. Content validation degrades
            // a target on a non-route input, but don't rely on that alone.
            .filter(|input| input.submits_to_route())
            .and_then(|input| input.target.as_ref())
            .map(|target| target.cycle)
    }

    /// Cycle the input's route target through `[new conversation]
    /// + [synthetic current if not yet fetched] + [fetched chain heads]`.
    /// New = no `thread`/`chain_root` (spawns a fresh chain); a chain slot
    /// retargets the head so the next submit braids onto it. Gated on the
    /// focused input declaring `target.cycle: route_chains`.
    fn cycle_input_target(&mut self, forward: bool) -> Vec<StudioEffect> {
        // Capability gate (decision: content-declared). The keymap shouldn't
        // emit this without a declaration; if a direct/stale event arrives
        // anyway, no-op silently (no mutation, no user notice) rather than
        // panic — the reducer never trusts the caller was correct.
        let Some(super::content::InputTargetCycle::RouteChains) = self.focused_input_target_cycle()
        else {
            return Vec::new();
        };

        let mut route = self.seat.fold().input_route();
        // The input declared route-chain targeting (the author's assertion
        // that this route continues conversations). The only thing the engine
        // can't paper over is a route with no invoke at all — there's nothing
        // to submit onto. Surface that (deduped), don't silently no-op.
        if route.invoke.is_none() {
            self.notice_deduped(
                "This input has no route to target a conversation on.",
                StudioTone::Warn,
            );
            return Vec::new();
        }

        let slots = self.route_chain_slots(&route);
        // Only "new conversation" → nothing to cycle (not an error).
        if slots.len() <= 1 {
            return Vec::new();
        }
        let current = slots
            .iter()
            .position(|slot| match (slot, &route.chain_root) {
                (TargetSlot::NewConversation, None) => true,
                (TargetSlot::Chain { root, .. }, Some(cr)) => root == cr,
                _ => false,
            })
            .unwrap_or(0);
        let len = slots.len();
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        match &slots[next] {
            TargetSlot::NewConversation => {
                route.thread = None;
                route.chain_root = None;
            }
            TargetSlot::Chain { root, head } => {
                route.thread = Some(head.clone());
                route.chain_root = Some(root.clone());
            }
        }
        // A non-serializable InputRoute is a bug, not a runtime branch.
        let value = serde_json::to_value(&route).expect("InputRoute serializes");
        self.seat.append_facet(super::seat::KEY_INPUT_ROUTE, value);
        self.bump_generation();
        let mut effects = self.effects_for_facet(super::seat::KEY_INPUT_ROUTE);
        effects.extend(self.effects_for_hint("thread"));
        effects
    }

    /// Build the ordered target slots for route-chain cycling:
    /// `[NewConversation] + [synthetic current if its root isn't fetched]
    /// + [fetched chain heads]`, deduped by `chain_root` (preferring the
    /// fetched head when the current chain is also present in fetched data).
    fn route_chain_slots(&self, route: &super::seat::InputRoute) -> Vec<TargetSlot> {
        let fetched = self.input_target_chains();
        let mut slots = vec![TargetSlot::NewConversation];

        // Synthetic current: the route points at a chain not yet in the
        // fetched list (async refresh hasn't landed) — keep it cyclable.
        if let Some(root) = route.chain_root.as_ref() {
            let in_fetched = fetched.iter().any(|(r, _)| r == root);
            if !in_fetched {
                slots.push(TargetSlot::Chain {
                    root: root.clone(),
                    head: route.thread.clone().unwrap_or_else(|| root.clone()),
                });
            }
        }

        for (root, head) in fetched {
            slots.push(TargetSlot::Chain { root, head });
        }
        slots
    }

    /// `submit: route` — classify the line and dispatch through the engine
    /// route-fold. Behaviour (slash/plain, route_seq, read-only/empty) is
    /// unchanged; it is now reached through the `input` grammar instead of
    /// the deleted Input dock special-case.
    fn submit_route(&mut self, text: &str, interrupt: bool) -> Vec<StudioEffect> {
        if self.is_read_only() {
            self.notice("This session is read-only.", StudioTone::Warn);
            return Vec::new();
        }
        let line = match super::tokenize::classify_line(text) {
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
                // Explicit grammar: tokens resolve + bind daemon-side (one
                // invocation path for all clients). Slash bypasses the
                // pinned route — explicit tokens win; no implicit
                // thread/site.
                vec![self.emit(StudioEffectKind::Invoke {
                    target: super::effect::InvokeRef::Tokens { tokens },
                    params: serde_json::json!({}),
                    route_seq: None,
                    ratchet_on_thread_id: false,
                })]
            }
            super::tokenize::InputLine::Plain(plain) => {
                // Capture ratchet eligibility NOW (issue time), not at result
                // time: the focused input declares conversation targeting, so a
                // successful launch should braid the route onto the produced
                // thread. Computed once here so a focus change while the async
                // launch is in flight can't corrupt the ratchet decision.
                let ratchet_on_thread_id = self.focused_input_target_cycle().is_some();
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
                        // Ground verb: text bound whole to the service's
                        // declared input, never split.
                        let mut params = if route.params.is_object() {
                            route.params.clone()
                        } else {
                            serde_json::json!({})
                        };
                        params["input"] = serde_json::Value::String(plain);
                        if let Some(thread) = &route.thread {
                            params["thread"] = serde_json::Value::String(thread.clone());
                        }
                        // Live-delivery intent for a running-thread target. Only
                        // set for interrupt; steer is the daemon default (and the
                        // wire-compatible value for older daemons). Ignored by the
                        // daemon on non-running targets.
                        if interrupt {
                            params["intent"] = serde_json::Value::String("interrupt".to_string());
                        }
                        vec![self.emit(StudioEffectKind::Invoke {
                            target: super::effect::InvokeRef::Ref { item_ref },
                            params,
                            route_seq,
                            ratchet_on_thread_id,
                        })]
                    }
                    super::seat::InvokeTemplate::Command { mut tokens } => {
                        tokens.push(plain);
                        vec![self.emit(StudioEffectKind::Invoke {
                            target: super::effect::InvokeRef::Tokens { tokens },
                            params: serde_json::json!({}),
                            route_seq,
                            ratchet_on_thread_id,
                        })]
                    }
                    super::seat::InvokeTemplate::UiFacet { key } => {
                        self.seat
                            .append_facet(key, serde_json::Value::String(plain));
                        self.clear_focused_input();
                        self.bump_generation();
                        Vec::new()
                    }
                }
            }
        }
    }

    /// Fire a content affordance bound to `input.submit` with the buffer
    /// text as the `{value}` payload (the input producer namespace).
    fn invoke_input_affordance(
        &mut self,
        view_ref: &str,
        affordance_id: &str,
        value: &str,
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
        let payload = super::content::Payload::Input(value);
        match super::content::resolve_affordance_invoke(
            &affordance,
            super::content::Producer::Input,
            &payload,
        ) {
            Some(super::content::AffordanceInvoke::Ui {
                facet,
                value,
                merge,
                open_view,
            }) => {
                let effects = self.apply_ui_affordance(facet, value, merge, open_view);
                self.clear_focused_input();
                effects
            }
            Some(super::content::AffordanceInvoke::Rye { tokens, args }) => {
                vec![self.emit(StudioEffectKind::Invoke {
                    target: super::effect::InvokeRef::Tokens { tokens },
                    params: args,
                    route_seq: None,
                    ratchet_on_thread_id: false,
                })]
            }
            Some(super::content::AffordanceInvoke::Service { item_ref, args }) => {
                vec![self.emit(StudioEffectKind::Invoke {
                    target: super::effect::InvokeRef::Ref { item_ref },
                    params: args,
                    route_seq: None,
                    ratchet_on_thread_id: false,
                })]
            }
            None => {
                self.notice(
                    "Input affordance cannot be supplied by {value}.",
                    StudioTone::Warn,
                );
                Vec::new()
            }
        }
    }

    fn clear_focused_input(&mut self) {
        if let Some(buffer) = self.focused_input_buffer_mut() {
            buffer.clear();
        }
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
        let delta = match direction {
            StudioStackMoveDirection::Up => -1,
            StudioStackMoveDirection::Down => 1,
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

    /// The surface's ordered library, filtered to refs that work as a center
    /// lens: a real bound view that is neither a scene backdrop nor the foot
    /// input. Single-lens `Ctrl+←/→` cycles this list.
    fn lens_library(&self) -> Vec<String> {
        self.data
            .session
            .as_ref()
            .and_then(|session| session.effective_surface.as_ref())
            .and_then(|surface| surface.get("library"))
            .and_then(|library| library.as_array())
            .map(|refs| {
                refs.iter()
                    .filter_map(|value| value.as_str())
                    .filter(|view_ref| {
                        self.views.get(*view_ref).is_some_and(|binding| {
                            binding.widget != "scene" && binding.input.is_none()
                        })
                    })
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn cycle_lens(&mut self, delta: i32) -> Vec<StudioEffect> {
        let lenses = self.lens_library();
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
        self.data.tile_file_space.clear();
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

    fn set_tile_fold(&mut self, tile_id: TileId, section: usize, collapsed: bool) -> bool {
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

    fn open_view(&mut self, view: ViewSpec) -> Vec<StudioEffect> {
        for tile_id in self.workspace.tile_ids() {
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

        // Single-lens surfaces (the cell-grid TUI) hold exactly one center
        // tile: opening a different view REPLACES the lens in place rather
        // than splitting a second tile. Breadth comes from swapping the
        // single lens, never from arranging panes.
        if self.workspace.tiling.mode == crate::surface::TilingModeSpec::SingleLens
            && !self.workspace.center_is_empty()
        {
            if let Some(tile_id) = self.workspace.replace_focused_view(view.clone()) {
                self.push_motion(StudioMotionEventVm::FocusChanged {
                    tile_id: tile_id.0.to_string(),
                });
                self.bump_generation();
                return self.effects_for_view(&view);
            }
        }

        let effects = self.add_center_tile(view);
        self.bump_generation();
        effects
    }

    fn close_tile_or_empty(&mut self, tile_id: TileId) -> bool {
        if self.workspace.tile_ids().len() <= 1 {
            if self.workspace.center_is_empty() || !self.workspace.tiles.contains_key(&tile_id) {
                return false;
            }
            self.push_motion(StudioMotionEventVm::TileExit {
                tile_id: tile_id.0.to_string(),
            });
            self.workspace.reset_to_empty();
            return true;
        }
        if self.workspace.close_tile(tile_id) {
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

    /// Add a center tile through the tiling algorithm (insert: end) and
    /// emit the motions a renderer needs. Returns the new tile id.
    fn add_tile_motions(&mut self, view: ViewSpec) -> TileId {
        let was_empty = self.workspace.center_is_empty();
        let source_tile_id = self.workspace.focused_tile;
        let tile_id = self.workspace.add_tile(view);
        if !was_empty {
            // New tiles land in the stack region; the motion axis is
            // the stack arrangement. (The first tile into an empty center
            // needs no split motion — it simply fills the center.)
            self.push_motion(StudioMotionEventVm::TileSplit {
                source_tile_id: source_tile_id.0.to_string(),
                new_tile_id: tile_id.0.to_string(),
                axis: arrange_axis_vm(self.workspace.tiling.stack.arrange),
            });
        }
        self.push_motion(StudioMotionEventVm::TileEnter {
            tile_id: tile_id.0.to_string(),
        });
        self.push_motion(StudioMotionEventVm::FocusChanged {
            tile_id: tile_id.0.to_string(),
        });
        tile_id
    }

    fn add_center_tile(&mut self, view: ViewSpec) -> Vec<StudioEffect> {
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
        // Row activation is the `selection` producer: affordances read
        // `{record.<field>}`. Validation is binding-time (fails closed).
        let payload = super::content::Payload::Selection(record);
        match super::content::resolve_affordance_invoke(
            &affordance,
            super::content::Producer::Selection,
            &payload,
        ) {
            Some(super::content::AffordanceInvoke::Ui {
                facet,
                value,
                merge,
                open_view,
            }) => self.apply_ui_affordance(facet, value, merge, open_view),
            Some(super::content::AffordanceInvoke::Rye { tokens, args }) => {
                vec![self.emit(StudioEffectKind::Invoke {
                    target: super::effect::InvokeRef::Tokens { tokens },
                    params: args,
                    route_seq: None,
                    ratchet_on_thread_id: false,
                })]
            }
            Some(super::content::AffordanceInvoke::Service { item_ref, args }) => {
                vec![self.emit(StudioEffectKind::Invoke {
                    target: super::effect::InvokeRef::Ref { item_ref },
                    params: args,
                    route_seq: None,
                    ratchet_on_thread_id: false,
                })]
            }
            None => Vec::new(),
        }
    }

    /// Apply a resolved Ui-plane affordance: write the seat facet (value
    /// replaces; merge folds into the existing value) and refetch every
    /// binding subscribed to that facet.
    fn apply_ui_affordance(
        &mut self,
        facet: String,
        value: Option<serde_json::Value>,
        merge: Option<serde_json::Value>,
        open_view: Option<String>,
    ) -> Vec<StudioEffect> {
        let next = if let Some(merge) = merge {
            let mut current = self
                .seat
                .fold()
                .get(&facet)
                .cloned()
                .unwrap_or(serde_json::json!({}));
            if let (Some(target), Some(patch)) = (current.as_object_mut(), merge.as_object()) {
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
        let mut effects = self.effects_for_facet(&facet);
        // Open the view AFTER the facet write, so the opened view's fetch
        // resolves its `@facet:` params against the value just written (e.g. a
        // row drill-in sets input.route.chain_root, then the braid lens fetches
        // that chain). Single-lens surfaces replace the center in place.
        if let Some(view_ref) = open_view {
            effects.extend(self.open_view(ViewSpec::bound(view_ref)));
        }
        effects
    }

    /// Facet write arrived: refetch every bound tile whose binding
    /// declares `refresh.on_facet: <key>` or whose source params
    /// reference the facet explicitly.
    pub fn effects_for_facet(&mut self, facet: &str) -> Vec<StudioEffect> {
        let targets: Vec<(crate::ids::TileId, String)> = self
            .workspace
            .tile_ids()
            .into_iter()
            .filter_map(|tile_id| {
                let tile = self.workspace.tiles.get(&tile_id)?;
                let view_ref = &tile.view.view_ref;
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
            .flat_map(|(tile_id, view_ref)| self.emit_fetch_source(tile_id, &view_ref))
            .collect()
    }

    /// Resolve the atlas arrangement a `SetAtlas*` event targets: `Some(tile)`
    /// → that tile's per-tile arrangement, created from the default on first
    /// touch; `None` → the ambient backdrop atlas.
    fn atlas_target_mut(&mut self, tile_id: &Option<String>) -> &mut crate::atlas::AtlasUiStateVm {
        match tile_id {
            Some(id) => self.ui.tile_atlas.entry(id.clone()).or_default(),
            None => &mut self.ui.atlas,
        }
    }

    fn atlas_target(&self, tile_id: &Option<String>) -> &crate::atlas::AtlasUiStateVm {
        match tile_id {
            Some(id) => self.ui.tile_atlas.get(id).unwrap_or(&self.ui.atlas),
            None => &self.ui.atlas,
        }
    }

    fn effects_for_view(&mut self, view: &ViewSpec) -> Vec<StudioEffect> {
        let view_ref = view.view_ref.clone();
        // Scene widgets pull engine data the generic source path doesn't
        // carry; everything else fetches its declared source.
        let widget = self
            .views
            .get(&view_ref)
            .map(|binding| binding.widget.clone());
        match widget.as_deref() {
            Some("atlas") => vec![
                self.emit(StudioEffectKind::FetchDimension),
                self.emit(StudioEffectKind::FetchTopology),
                self.emit(StudioEffectKind::FetchItems {
                    tile_id: None,
                    query: None,
                    kind: None,
                    limit: 1000,
                }),
            ],
            Some("graph") => vec![
                self.emit(StudioEffectKind::FetchDimension),
                self.emit(StudioEffectKind::FetchTopology),
            ],
            _ => {
                let tile_id = self.workspace.focused_tile;
                self.emit_fetch_source(tile_id, &view_ref)
                    .into_iter()
                    .collect()
            }
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
                    // Typed submit result: { thread_id?, delivery, notice?, execution? }.
                    let outcome: super::dto::LaunchOutcome =
                        serde_json::from_value(data.clone()).unwrap_or_default();
                    if outcome.delivery == Some(super::dto::ThreadDelivery::Refused) {
                        // A refused delivery (non-continuation target, settled
                        // status, or duplicate-submit conflict) delivered
                        // nothing: KEEP the buffer so the operator's text isn't
                        // lost, surface the daemon's reason, and do NOT ratchet
                        // or claim a launch. (`thread_id` may be null or an
                        // existing id; either way nothing new was created.)
                        self.notice(
                            outcome.notice.unwrap_or_else(|| REFUSED_NOTICE.to_string()),
                            StudioTone::Warn,
                        );
                        return Vec::new();
                    }
                    if outcome.delivery == Some(super::dto::ThreadDelivery::Submitted) {
                        // Live fold into a RUNNING thread: the stimulus was
                        // delivered as a new cognition_in on the SAME thread — no
                        // new thread, so no ratchet and no "launched" copy. Clear
                        // the buffer and keep the route where it is; the live tail
                        // shows the folded turn. A notice present here means a
                        // degradation (interrupt → steer); surface it as a warning.
                        self.clear_focused_input();
                        let degraded = outcome.notice.is_some();
                        self.notice(
                            outcome.notice.unwrap_or_else(|| match outcome.thread_id.as_deref()
                            {
                                Some(id) => format!("Input delivered to {id}."),
                                None => "Input delivered.".to_string(),
                            }),
                            if degraded {
                                StudioTone::Warn
                            } else {
                                StudioTone::Good
                            },
                        );
                        let mut effects =
                            vec![self.emit(StudioEffectKind::FetchThreads { limit: 200 })];
                        effects.extend(self.effects_for_hint("thread"));
                        return effects;
                    }
                    self.clear_focused_input();
                    let Some(thread_id) = outcome.thread_id.clone() else {
                        self.notice(effect_success_notice(&expected, &data), StudioTone::Good);
                        self.bump_generation();
                        return Vec::new();
                    };
                    // Ratchet: the route is live state — a launch retargets
                    // the input at the produced thread so the next submit
                    // continues the chain. A stale result (route changed
                    // since issue) may notice but never retargets.
                    if let StudioEffectKind::Invoke {
                        route_seq,
                        ratchet_on_thread_id,
                        ..
                    } = &expected
                    {
                        let fold = self.seat.fold();
                        // Eligibility was decided at issue time (see submit_route)
                        // — read it, don't recompute from current focus, which
                        // may have moved while the launch was in flight. AND in
                        // the produced thread's substrate facts when the result
                        // carries them (an operator continuation does; a fresh
                        // async launch doesn't — unknown stays eligible, and the
                        // daemon refuses a real non-continuation continue).
                        // Operator-input targeting: ratchet only onto a successor
                        // that accepts OPERATOR follow-up (a graph continues by
                        // machine but takes no operator input).
                        let result_supports = outcome.execution.map(|e| e.supports_operator_followup);
                        let targets = *ratchet_on_thread_id && result_supports != Some(false);
                        if fold.seq_of(super::seat::KEY_INPUT_ROUTE) == *route_seq {
                            let mut route = fold.input_route();
                            // Only ratchet a continuation target onto routes
                            // whose input declares conversation targeting. A
                            // fire-and-forget route that happens to produce a
                            // thread_id must NOT be retargeted as "continuing" —
                            // same declaration the cycle and the label key off.
                            if targets {
                                // First turn of a conversation: the launched
                                // thread IS the chain root (root == head).
                                // Continuations (route already had a head) keep
                                // the root and only advance the head — so the
                                // feed keeps showing the whole braid while the
                                // next submit braids onto the newest turn.
                                if route.thread.is_none() {
                                    route.chain_root = Some(thread_id.clone());
                                }
                                route.thread = Some(thread_id.clone());
                                if let Ok(value) = serde_json::to_value(&route) {
                                    self.seat.append_facet(super::seat::KEY_INPUT_ROUTE, value);
                                }
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
                    // Freshness guard: only the newest request for this key may
                    // land. An older fetch (e.g. the previously selected thread's
                    // section) resolving late is dropped, so a reused single-lens
                    // tile never shows mixed data from two selections.
                    let is_latest = self
                        .data
                        .source_epoch
                        .get(tile_id)
                        .map_or(true, |&latest| result.id >= latest);
                    if is_latest {
                        self.data.sources.insert(tile_id.clone(), data);
                    }
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
                self.data.tile_file_space.clear();
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
                        if let StudioEffectKind::FetchFileSpace {
                            tile_id: Some(tile_id),
                            ..
                        } = &expected
                        {
                            core.data
                                .tile_file_space
                                .insert(tile_id.clone(), file_space);
                        } else {
                            core.data.file_space = Some(file_space);
                        }
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

/// Fallback notice when a refused delivery carries no reason from the daemon.
const REFUSED_NOTICE: &str = "Delivery refused.";

/// A route-chain target the input can cycle onto.
#[derive(Debug, Clone, PartialEq)]
enum TargetSlot {
    /// No target thread/root — a submit starts a fresh chain.
    NewConversation,
    /// Braid onto an existing chain: `head` is the turn the next submit
    /// continues, `root` is the conversation identity the feed follows.
    Chain { root: String, head: String },
}


fn arrange_axis_vm(arrange: ArrangeSpec) -> StudioSplitAxisVm {
    match arrange {
        ArrangeSpec::Horizontal => StudioSplitAxisVm::Horizontal,
        ArrangeSpec::Vertical => StudioSplitAxisVm::Vertical,
    }
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

/// A route is the view's own ref — every view is addressable by ref
/// (`#view:…`), graph/atlas included. The engine names no specific view.
fn view_from_route(route: &str) -> Option<ViewSpec> {
    let route = route.trim_start_matches('#');
    route.starts_with("view:").then(|| ViewSpec::bound(route))
}

fn route_for_view(view: &ViewSpec) -> Option<String> {
    Some(view.view_ref.clone())
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
    let Some(tile_id) = tile_id.as_deref() else {
        // The shared/ambient fetch is unscoped.
        return query.is_none() && kind.is_none();
    };
    // A tile-scoped fetch is current iff that tile still binds an atlas view
    // whose declared `body.scope` still matches this fetch's (query, kind) —
    // content is the scope, so a re-bound or re-scoped tile drops the stale
    // response instead of caching it.
    let Some(tile_id) = parse_tile_id(tile_id) else {
        return false;
    };
    let Some(binding) = core
        .workspace
        .tiles
        .get(&tile_id)
        .and_then(|tile| core.views.get(&tile.view.view_ref))
    else {
        return false;
    };
    if binding.widget != "atlas" {
        return false;
    }
    let (want_query, want_kind) = super::model::atlas_item_scope(binding);
    &want_query == query && &want_kind == kind
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
    let Some(StudioEffectKind::FetchFileSpace {
        tile_id,
        root,
        path,
        ..
    }) = expected
    else {
        return true;
    };
    let response_matches = root == &file_space.root && path == &file_space.path;
    // Per-tile fetch: validate against THIS tile's file-space arrangement;
    // the shared fetch validates against the ambient atlas state.
    let atlas = match tile_id.as_deref().map(parse_tile_id) {
        Some(Some(tile_id)) => core.tile_atlas_state(tile_id),
        Some(None) => return false,
        None => &core.ui.atlas,
    };
    atlas.active_projection.is_file_space()
        && root == &atlas.file_space_root
        && path == &atlas.file_space_path
        && response_matches
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
            // A realistic session carries its surface as data: the engine's
            // default slot set is now empty (it names no views), so the test
            // session declares its slots here as fixture data — the input,
            // threads, and inspector slots the suite was written against.
            effective_surface: Some(serde_json::json!({
                "name": "studio-base",
                "slots": {
                    "bottom": { "content": "view:ryeos/input", "open": true, "size": 7 },
                    "left": { "content": "view:ryeos/threads/list", "open": false, "size": 32 },
                    "right": { "content": "view:ryeos/item/inspector", "open": false, "size": 40 }
                },
                "views": {
                    "view:ryeos/input": {
                        "widget": "text",
                        "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route" }
                    }
                }
            })),
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
                "tiles": [],
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
                        "value": { "item": "{record.canonical_ref}" }
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
        let tile_id = core
            .workspace
            .add_tile(ViewSpec {
                view_ref: "view:test/inspector".to_string(),
            })
            .0
            .to_string();

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
    fn ui_affordance_open_view_writes_route_then_opens_view_with_new_facet() {
        // The watch drill-in (P1): a row activation merges route {thread, chain_root}
        // AND opens the braid lens, whose fetch must resolve the just-written chain_root.
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "table",
                "source": { "ref": "service:ui/studio/threads/list", "params": {}, "collection": "threads" },
                "affordances": [{
                    "id": "watch",
                    "invoke": {
                        "plane": "ui",
                        "facet": "input.route",
                        "merge": { "thread": "{record.thread_id}", "chain_root": "{record.chain_root_id}" },
                        "open_view": "view:ryeos/chain/timeline"
                    }
                }]
            }),
        );
        seed_view_value(
            &mut core,
            "view:ryeos/chain/timeline",
            serde_json::json!({
                "widget": "timeline",
                "source": {
                    "ref": "service:events/chain_replay",
                    "params": { "chain_root_id": "@facet:input.route.chain_root" },
                    "collection": "events"
                }
            }),
        );

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:ryeos/threads/list".to_string(),
                    affordance_id: "watch".to_string(),
                    record: serde_json::json!({ "thread_id": "T-9", "chain_root_id": "T-root" }),
                },
            },
        });

        // 1. Route facet carries BOTH thread and chain_root.
        let fold = core.seat.fold();
        let route = fold.get("input.route").expect("route facet written");
        assert_eq!(route["thread"], "T-9");
        assert_eq!(route["chain_root"], "T-root");

        // 2. The braid lens was opened as a tile.
        assert!(
            core.workspace
                .tiles
                .values()
                .any(|t| t.view.view_ref == "view:ryeos/chain/timeline"),
            "drill-in opens the braid lens"
        );

        // 3. Its fetch resolved the just-written chain_root (write-then-open order).
        assert!(
            effects.iter().any(|e| matches!(&e.kind,
                StudioEffectKind::FetchSource { source_ref, params, .. }
                    if source_ref == "service:events/chain_replay"
                        && params["chain_root_id"] == "T-root")),
            "timeline fetch must use the selected chain_root; got {effects:?}"
        );
    }

    #[test]
    fn service_ref_affordance_emits_execute_invoke_with_row_args() {
        // Row management (P2): a service-ref affordance emits an /execute Invoke
        // carrying the row's args — so cancel/kill/continue target that row, not
        // the route head (the token path would drop the args).
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "table",
                "source": { "ref": "service:ui/studio/threads/list", "params": {}, "collection": "threads" },
                "affordances": [{
                    "id": "cancel",
                    "invoke": {
                        "plane": "rye",
                        "ref": "service:commands/submit",
                        "args": { "thread_id": "{record.thread_id}", "command_type": "cancel" }
                    }
                }]
            }),
        );

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:ryeos/threads/list".to_string(),
                    affordance_id: "cancel".to_string(),
                    record: serde_json::json!({ "thread_id": "T-7" }),
                },
            },
        });

        assert!(
            matches!(effects.first().map(|e| &e.kind),
                Some(StudioEffectKind::Invoke {
                    target: crate::studio::effect::InvokeRef::Ref { item_ref },
                    params,
                    ..
                }) if item_ref == "service:commands/submit"
                    && params["thread_id"] == "T-7"
                    && params["command_type"] == "cancel"),
            "service-ref affordance must /execute with the row's args; got {effects:?}"
        );
    }

    #[test]
    fn actual_threads_list_watch_affordance_drills_into_braid() {
        // The shipped product contract: the real threads/list.yaml `watch`
        // affordance drills a row into its braid.
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let binding: crate::studio::content::ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/studio/.ai/views/ryeos/threads/list.yaml"
        ))
        .unwrap();
        core.views
            .insert("view:ryeos/threads/list".to_string(), binding);
        seed_view_value(
            &mut core,
            "view:ryeos/chain/timeline",
            serde_json::json!({
                "widget": "timeline",
                "source": {
                    "ref": "service:events/chain_replay",
                    "params": { "chain_root_id": "@facet:input.route.chain_root" },
                    "collection": "events"
                }
            }),
        );
        // A pre-existing route field must survive the merge.
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({ "directive": "directive:ryeos/ops/base" }),
        );

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:ryeos/threads/list".to_string(),
                    affordance_id: "watch".to_string(),
                    record: serde_json::json!({ "thread_id": "T-9", "chain_root_id": "T-root" }),
                },
            },
        });

        let fold = core.seat.fold();
        let route = fold.get("input.route").expect("route facet");
        assert_eq!(route["thread"], "T-9");
        assert_eq!(route["chain_root"], "T-root");
        assert_eq!(
            route["directive"], "directive:ryeos/ops/base",
            "merge preserves existing route fields"
        );
        assert!(
            core.workspace
                .tiles
                .values()
                .any(|t| t.view.view_ref == "view:ryeos/chain/timeline"),
            "watch opens the braid lens"
        );
        assert!(
            effects.iter().any(|e| matches!(&e.kind,
                StudioEffectKind::FetchSource { source_ref, params, .. }
                    if source_ref == "service:events/chain_replay"
                        && params["chain_root_id"] == "T-root")),
            "timeline fetch uses the row's chain_root; got {effects:?}"
        );
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
                        "args": { "thread_id": "{record.thread_id}" }
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
                ..
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
                        "merge": { "thread": "{record.thread_id}" }
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
        seed_view_value(
            &mut core,
            "view:ryeos/graph/topology",
            serde_json::json!({ "widget": "graph" }),
        );
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::bound("view:ryeos/graph/topology"),
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

        let scene =
            crate::studio::scene_model::build_scene_model(&core, &core.ui.atlas, None, None);
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
    fn launcher_lists_embedded_views_including_scene_widgets() {
        let mut core = StudioCore::default();
        core.views.insert(
            "view:ryeos/threads/list".to_string(),
            serde_json::from_value(serde_json::json!({
                "widget": "rows",
                "description": "Thread list"
            }))
            .unwrap(),
        );
        // Graph/atlas are ordinary embedded views now — no hardcoded items.
        core.views.insert(
            "view:ryeos/graph/topology".to_string(),
            serde_json::from_value(serde_json::json!({ "widget": "graph" })).unwrap(),
        );
        let items = launcher_items(&core);
        // The scene-widget view launches as a Bound tile, labeled by ref.
        assert!(items.iter().any(|item| {
            item.label == "ryeos/graph/topology"
                && matches!(
                    &item.action,
                    StudioAction::OpenView {
                        view: ViewSpec { view_ref }
                    } if view_ref == "view:ryeos/graph/topology"
                )
        }));
        // Content views launch as Bound tiles, labeled by ref.
        assert!(items.iter().any(|item| {
            item.label == "ryeos/threads/list"
                && matches!(
                    &item.action,
                    StudioAction::OpenView {
                        view: ViewSpec { view_ref }
                    } if view_ref == "view:ryeos/threads/list"
                )
        }));
    }

    #[test]
    fn launcher_includes_shared_dock_toggles() {
        let core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let vm = build_view_model(&core);

        assert!(vm.launcher.items.iter().any(|item| {
            item.label == "Hide bottom slot"
                && matches!(
                    item.action,
                    StudioAction::ToggleDock {
                        edge: crate::studio::model::StudioDockEdge::Bottom
                    }
                )
        }));
        assert!(vm.launcher.items.iter().any(|item| {
            item.label == "Show left slot"
                && matches!(
                    item.action,
                    StudioAction::ToggleDock {
                        edge: crate::studio::model::StudioDockEdge::Left
                    }
                )
        }));
        // No surface-declared top slot → nothing to toggle there.
        assert!(!vm.launcher.items.iter().any(|item| matches!(
            item.action,
            StudioAction::ToggleDock {
                edge: crate::studio::model::StudioDockEdge::Top
            }
        )));
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
    fn toggling_open_slot_closes_it_and_frees_its_space() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        // The bottom input slot starts open.
        assert!(build_view_model(&core).workspace.docks.bottom.is_some());

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ToggleDock {
                    edge: crate::studio::model::StudioDockEdge::Bottom,
                },
            },
        });

        // Closed slots vanish from the dock plane: renderers reserve no
        // space for them. Content and size are retained for reopening.
        assert!(build_view_model(&core).workspace.docks.bottom.is_none());
        let bottom = core.ui.docks.bottom.as_ref().expect("slot retained");
        assert!(!bottom.visible);
        assert_eq!(bottom.size, 7);

        // Toggling an absent edge is a no-op (no slot declared).
        assert!(core.ui.docks.top.is_none());
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::ToggleDock {
                    edge: crate::studio::model::StudioDockEdge::Top,
                },
            },
        });
        assert!(effects.is_empty());
        assert!(core.ui.docks.top.is_none());
    }

    #[test]
    fn directive_threads_dock_renders_bound_view_rows() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.ui.docks.left.as_mut().unwrap().visible = true;
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
        assert!(dock.input.is_none(), "a rows view declares no input");
        match dock.view {
            crate::studio::view_model::StudioViewVm::Rows { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].primary, "T-running");
                assert_eq!(rows[0].meta.as_deref(), Some("directive:demo/chat"));
            }
            other => panic!("expected bound rows dock view, got {other:?}"),
        }
    }

    #[test]
    fn stale_source_response_is_dropped_by_the_freshness_guard() {
        use crate::studio::effect::{StudioEffectKind, StudioEffectResult, StudioEffectResultKind};
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        // Two fetches issued for the SAME key — a single-lens tile reused for a
        // new selection keeps its key. The second is the newest request.
        let older = core.emit(StudioEffectKind::FetchSource {
            tile_id: "K".to_string(),
            source_ref: "service:x".to_string(),
            params: serde_json::json!({}),
        });
        let newer = core.emit(StudioEffectKind::FetchSource {
            tile_id: "K".to_string(),
            source_ref: "service:x".to_string(),
            params: serde_json::json!({}),
        });
        assert!(newer.id > older.id);
        // build_fetch_source would record the newest request; simulate that.
        core.data.source_epoch.insert("K".to_string(), newer.id);

        let deliver = |core: &mut StudioCore, id: u64, tag: &str| {
            core.dispatch(StudioEvent::EffectResult {
                result: StudioEffectResult {
                    id,
                    ok: true,
                    kind: StudioEffectResultKind::SourceData,
                    data: Some(serde_json::json!({ "tag": tag })),
                    error: None,
                },
            });
        };

        // Newest resolves first and lands.
        deliver(&mut core, newer.id, "new");
        assert_eq!(core.data.sources["K"]["tag"], "new");
        // An older straggler resolving afterwards is DROPPED — a slow fetch for
        // the previous selection must not overwrite the current one.
        deliver(&mut core, older.id, "old");
        assert_eq!(
            core.data.sources["K"]["tag"], "new",
            "stale straggler must not overwrite the newest response"
        );
    }

    #[test]
    fn refetching_a_sections_view_clears_prior_section_data() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/detail",
            serde_json::json!({
                "widget": "sections",
                "sections": [
                    { "title": "A", "source": { "ref": "service:a" }, "projection": {} },
                    { "title": "B", "source": { "ref": "service:b" }, "projection": {} }
                ]
            }),
        );
        let k0 = crate::studio::content::section_source_key("K", 0);
        let k1 = crate::studio::content::section_source_key("K", 1);
        core.data
            .sources
            .insert(k0.clone(), serde_json::json!({ "stale": "A" }));
        core.data
            .sources
            .insert(k1.clone(), serde_json::json!({ "stale": "B" }));

        // Refetching (the lens reused for a new selection) drops each section's
        // prior response so the previous selection can't render underneath, and
        // emits a fresh fetch per section.
        let effects = core.emit_fetch_source_keyed("K".to_string(), "view:test/detail");
        assert!(core.data.sources.get(&k0).is_none());
        assert!(core.data.sources.get(&k1).is_none());
        assert_eq!(effects.len(), 2);
    }

    #[test]
    fn sections_view_assembles_one_group_per_section_from_its_own_source() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.ui.docks.left.as_mut().unwrap().visible = true;
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "sections",
                "sections": [
                    {
                        "title": "Threads",
                        "source": { "ref": "service:ui/studio/threads", "collection": "rows" },
                        "projection": { "primary": "thread_id", "meta": "status" }
                    },
                    {
                        "title": "Bundles",
                        "source": { "ref": "service:ui/studio/bundles", "collection": "rows" },
                        "projection": { "primary": "name", "meta": "version" }
                    }
                ]
            }),
        );
        // Each section's response lands under its own per-section key.
        core.data.sources.insert(
            crate::studio::content::section_source_key("dock:left", 0),
            serde_json::json!({ "rows": [
                { "thread_id": "T-ab", "status": "running" },
                { "thread_id": "T-cd", "status": "done" }
            ]}),
        );
        core.data.sources.insert(
            crate::studio::content::section_source_key("dock:left", 1),
            serde_json::json!({ "rows": [ { "name": "studio", "version": "v1.0.0" } ]}),
        );

        let vm = build_view_model(&core);
        let dock = vm.workspace.docks.left.expect("left dock");
        match dock.view {
            crate::studio::view_model::StudioViewVm::Sections { sections, .. } => {
                assert_eq!(sections.len(), 2);
                assert_eq!(sections[0].title, "Threads");
                assert_eq!(sections[0].count, 2);
                assert_eq!(sections[0].rows[0].primary, "T-ab");
                assert_eq!(sections[0].rows[0].meta.as_deref(), Some("running"));
                assert_eq!(sections[1].title, "Bundles");
                assert_eq!(sections[1].count, 1);
                assert_eq!(sections[1].rows[0].primary, "studio");
                assert_eq!(sections[1].rows[0].meta.as_deref(), Some("v1.0.0"));
            }
            other => panic!("expected bound sections dock view, got {other:?}"),
        }
    }

    #[test]
    fn sections_flat_cursor_selects_a_row_and_resolves_its_section_activation() {
        use crate::studio::view_model::{
            action_for_focused_row, StudioLayoutNodeVm, StudioViewVm,
        };
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/studio/status"],
                "views": {
                    "view:ryeos/studio/status": {
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
        let mut core = StudioCore::new(session, BrowserViewport::default(), 0);
        let tile = core.workspace.focused_tile;
        let key = tile.0.to_string();
        core.data.sources.insert(
            crate::studio::content::section_source_key(&key, 0),
            serde_json::json!({ "threads": [ { "thread_id": "T-ab" }, { "thread_id": "T-cd" } ]}),
        );
        core.data.sources.insert(
            crate::studio::content::section_source_key(&key, 1),
            serde_json::json!({ "bundles": [ { "name": "studio" } ]}),
        );

        fn find_tile_view(node: &StudioLayoutNodeVm) -> Option<&StudioViewVm> {
            match node {
                StudioLayoutNodeVm::Tile { view, .. } => Some(view),
                StudioLayoutNodeVm::Split { first, second, .. } => {
                    find_tile_view(first).or_else(|| find_tile_view(second))
                }
            }
        }
        let selected_primaries = |core: &StudioCore| -> Vec<String> {
            let vm = build_view_model(core);
            let root = vm.workspace.root.expect("layout root");
            match find_tile_view(&root).expect("tile view") {
                StudioViewVm::Sections { sections, .. } => sections
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
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetTileCursor { tile_id: key.clone(), index: 0 },
        });
        assert_eq!(selected_primaries(&core), vec!["T-ab".to_string()]);
        match action_for_focused_row(&core).expect("threads row activates") {
            StudioAction::InvokeAffordance { affordance_id, record, .. } => {
                assert_eq!(affordance_id, "aim-input");
                assert_eq!(record["thread_id"], "T-ab");
            }
            other => panic!("expected aim-input invoke, got {other:?}"),
        }

        // Flat cursor 2 = the first Bundles row (Threads contributed 2). Bundles
        // declares no activation, so the point resolves a row but no action.
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetTileCursor { tile_id: key.clone(), index: 2 },
        });
        assert_eq!(selected_primaries(&core), vec!["studio".to_string()]);
        assert!(
            action_for_focused_row(&core).is_none(),
            "a bundles row has no section activation"
        );
    }

    #[test]
    fn folding_a_section_collapses_it_to_a_single_header_point() {
        use crate::studio::view_model::{StudioLayoutNodeVm, StudioViewVm};
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/studio/status"],
                "views": {
                    "view:ryeos/studio/status": {
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
        let mut core = StudioCore::new(session, BrowserViewport::default(), 0);
        let tile = core.workspace.focused_tile;
        let key = tile.0.to_string();
        core.data.sources.insert(
            crate::studio::content::section_source_key(&key, 0),
            serde_json::json!({ "threads": [ { "thread_id": "T-ab" }, { "thread_id": "T-cd" } ]}),
        );
        core.data.sources.insert(
            crate::studio::content::section_source_key(&key, 1),
            serde_json::json!({ "bundles": [ { "name": "studio" } ]}),
        );

        fn tile_sections(
            core: &StudioCore,
        ) -> (Vec<crate::studio::view_model::StudioSectionVm>, Option<usize>) {
            fn find(node: &StudioLayoutNodeVm) -> Option<&StudioViewVm> {
                match node {
                    StudioLayoutNodeVm::Tile { view, .. } => Some(view),
                    StudioLayoutNodeVm::Split { first, second, .. } => {
                        find(first).or_else(|| find(second))
                    }
                }
            }
            let vm = build_view_model(core);
            match find(&vm.workspace.root.expect("root")).expect("tile view") {
                StudioViewVm::Sections {
                    sections,
                    fold_section,
                    ..
                } => (sections.clone(), *fold_section),
                other => panic!("expected sections, got {other:?}"),
            }
        }

        // Fold section 0 (Threads).
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetFold {
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
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetTileCursor { tile_id: key.clone(), index: 0 },
        });
        let (sections, fold_section) = tile_sections(&core);
        assert!(sections[0].header_selected, "collapsed header carries the point");
        assert_eq!(fold_section, Some(0), "fold key would toggle threads");

        // Flat index 1 is now the first Bundles row (Threads contributes one
        // header point, not two rows).
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetTileCursor { tile_id: key.clone(), index: 1 },
        });
        let (sections, fold_section) = tile_sections(&core);
        assert!(!sections[0].header_selected);
        assert!(sections[1].rows[0].selected, "point lands on the bundles row");
        assert_eq!(fold_section, Some(1));
    }

    #[test]
    fn help_overlay_toggles_through_the_view_model() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        // The catalogue is always present (discovery); only `open` toggles.
        assert!(!build_view_model(&core).help.open);
        assert!(!build_view_model(&core).help.entries.is_empty());

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::OpenHelp,
        });
        assert!(build_view_model(&core).help.open);

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CloseHelp,
        });
        assert!(!build_view_model(&core).help.open);
    }

    #[test]
    fn sections_view_without_a_loaded_source_shows_an_empty_group() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.ui.docks.left.as_mut().unwrap().visible = true;
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "sections",
                "sections": [{
                    "title": "Threads",
                    "source": { "ref": "service:ui/studio/threads", "collection": "rows" },
                    "projection": { "primary": "thread_id" }
                }]
            }),
        );
        // No source seeded → the section is present but empty (count 0), not a
        // placeholder: the surface is up, the data just hasn't arrived.
        let vm = build_view_model(&core);
        match vm.workspace.docks.left.expect("left dock").view {
            crate::studio::view_model::StudioViewVm::Sections { sections, .. } => {
                assert_eq!(sections.len(), 1);
                assert_eq!(sections[0].count, 0);
                assert!(sections[0].rows.is_empty());
            }
            other => panic!("expected sections dock view, got {other:?}"),
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
            Some(&ViewSpec {
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

    /// Seed the `view:ryeos/input` chat box (`submit: route`) so the
    /// bottom slot instance owns input.
    fn seed_input_view(core: &mut StudioCore) {
        core.views.insert(
            "view:ryeos/input".to_string(),
            serde_json::from_value(serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route",
                           "completion": { "ref": "service:commands/list", "collection": "commands" },
                           "target": { "cycle": "route_chains" } }
            }))
            .unwrap(),
        );
    }

    /// Write the focused input instance's transient buffer.
    fn set_focused_input(core: &mut StudioCore, text: &str) {
        let len = text.len();
        core.focused_input_buffer_mut()
            .expect("an input instance is focused")
            .set_text(text.to_string(), len);
    }

    /// Read the focused input instance's buffer text.
    fn focused_input_text(core: &StudioCore) -> String {
        core.focused_input_buffer()
            .map(|buffer| buffer.text.clone())
            .unwrap_or_default()
    }

    fn seed_service_route(core: &mut StudioCore) {
        seed_input_view(core);
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "params": { "directive": "directive:demo/base" }
            }),
        );
    }

    #[test]
    fn tab_cycles_input_target_through_new_then_chains() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        // Two conversations in list order (most-recent first): chain B is a
        // single thread; chain A has a follow-up (head T-a2 braids on T-a1).
        core.data.threads = Some(StudioThreadsDto {
            threads: vec![
                serde_json::json!({ "thread_id": "T-b1", "chain_root_id": "T-b1" }),
                serde_json::json!({
                    "thread_id": "T-a2", "chain_root_id": "T-a1",
                    "upstream_thread_id": "T-a1"
                }),
                serde_json::json!({ "thread_id": "T-a1", "chain_root_id": "T-a1" }),
            ],
        });

        // Starts on "new conversation" — no target thread, no chain root.
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread, None);
        assert_eq!(route.chain_root, None);

        // Tab → first chain (B, a single thread: head == root).
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.chain_root.as_deref(), Some("T-b1"));
        assert_eq!(route.thread.as_deref(), Some("T-b1"));

        // Tab → chain A, targeting its HEAD (T-a2, the turn nothing continues).
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.chain_root.as_deref(), Some("T-a1"));
        assert_eq!(route.thread.as_deref(), Some("T-a2"));

        // Tab → wraps back to "new conversation".
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread, None);
        assert_eq!(route.chain_root, None);

        // Shift+Tab from "new" wraps backward to the last chain (A).
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: false },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.chain_root.as_deref(), Some("T-a1"));
        assert_eq!(route.thread.as_deref(), Some("T-a2"));
    }

    #[test]
    fn thread_execution_facts_accessors() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.data.threads = Some(StudioThreadsDto {
            threads: vec![
                // directive: continuation + operator follow-up.
                serde_json::json!({ "thread_id": "T-a",
                    "execution": { "supports_continuation": true, "supports_operator_followup": true } }),
                // graph: machine continuation, NO operator follow-up.
                serde_json::json!({ "thread_id": "T-g",
                    "execution": { "supports_continuation": true, "supports_operator_followup": false } }),
                serde_json::json!({ "thread_id": "T-b", "execution": { "supports_continuation": false } }),
                serde_json::json!({ "thread_id": "T-c" }), // no execution facts
            ],
        });
        assert_eq!(core.thread_supports_continuation("T-a"), Some(true));
        assert_eq!(core.thread_supports_operator_followup("T-a"), Some(true));
        assert_eq!(core.thread_supports_continuation("T-g"), Some(true));
        assert_eq!(
            core.thread_supports_operator_followup("T-g"),
            Some(false),
            "graph is machine-only"
        );
        assert_eq!(core.thread_supports_continuation("T-b"), Some(false));
        assert_eq!(core.thread_supports_continuation("T-c"), None, "missing facts → unknown");
        assert_eq!(core.thread_supports_operator_followup("T-c"), None);
        assert_eq!(core.thread_supports_continuation("T-missing"), None);
    }

    #[test]
    fn cycle_input_target_excludes_machine_only_chain() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        // Two single-thread chains: one accepts operator follow-up, one is
        // machine-only (a graph — continuation-capable but no operator input).
        // Only the operator-followup chain is a valid input target.
        core.data.threads = Some(StudioThreadsDto {
            threads: vec![
                serde_json::json!({ "thread_id": "T-yes", "chain_root_id": "T-yes",
                    "execution": { "supports_continuation": true, "supports_operator_followup": true } }),
                serde_json::json!({ "thread_id": "T-no", "chain_root_id": "T-no",
                    "execution": { "supports_continuation": true, "supports_operator_followup": false } }),
            ],
        });
        // New → the continuation-capable chain (T-no is never offered).
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(core.seat.fold().input_route().chain_root.as_deref(), Some("T-yes"));
        // Forward again → wraps straight back to "new conversation".
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(core.seat.fold().input_route().chain_root, None);
    }

    #[test]
    fn cycle_input_target_is_noop_with_no_chains() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        // No threads fetched yet → only "new conversation" exists.
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread, None);
        assert_eq!(route.chain_root, None);
    }

    #[test]
    fn cycle_input_target_noop_without_declaration() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        // Replace the input with one that does NOT declare targeting.
        core.views.insert(
            "view:ryeos/input".to_string(),
            serde_json::from_value(serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "submit": "route" }
            }))
            .unwrap(),
        );
        core.data.threads = Some(StudioThreadsDto {
            threads: vec![serde_json::json!({ "thread_id": "T-x", "chain_root_id": "T-x" })],
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread, None, "no declaration → no mutation");
        assert_eq!(route.chain_root, None);
        assert!(core.ui.notices.is_empty(), "no declaration → silent (no notice)");
    }

    #[test]
    fn cycle_input_target_notices_when_route_has_no_invoke() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_input_view(&mut core);
        // Route facet with no invoke template at all.
        core.seat
            .append_facet(crate::studio::seat::KEY_INPUT_ROUTE, serde_json::json!({ "thread": "T-x" }));
        core.data.threads = Some(StudioThreadsDto {
            threads: vec![serde_json::json!({ "thread_id": "T-x", "chain_root_id": "T-x" })],
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        let n = core.ui.notices.len();
        assert!(n > 0, "no invoke → notice, not silent");
        // Deduped: a repeat press doesn't spam an identical notice.
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(core.ui.notices.len(), n, "notice deduped on repeat press");
    }

    #[test]
    fn cycle_input_target_dedupes_current_chain_and_prefers_fetched_head() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        // Route already on chain A with a stale head (T-a1).
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "thread": "T-a1", "chain_root": "T-a1"
            }),
        );
        // Fetched data shows chain A advanced to head T-a2.
        core.data.threads = Some(StudioThreadsDto {
            threads: vec![
                serde_json::json!({ "thread_id": "T-a2", "chain_root_id": "T-a1", "upstream_thread_id": "T-a1" }),
                serde_json::json!({ "thread_id": "T-a1", "chain_root_id": "T-a1" }),
            ],
        });
        // Slots = [New, Chain(A)] — A appears once. Current is A → forward → New.
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(core.seat.fold().input_route().chain_root, None);
        // Forward again → chain A using the FETCHED head, not the stale T-a1.
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.chain_root.as_deref(), Some("T-a1"));
        assert_eq!(route.thread.as_deref(), Some("T-a2"), "prefers fetched head");
    }

    #[test]
    fn cycle_input_target_keeps_synthetic_current_before_refresh() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        // Route aimed at a freshly-launched chain not yet in the thread list.
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "thread": "T-new", "chain_root": "T-new"
            }),
        );
        core.data.threads = Some(StudioThreadsDto { threads: vec![] }); // refresh not landed
        // Slots = [New, SyntheticChain(T-new)]. The guarantee: you can move
        // AWAY from the unfetched current chain before the refresh lands —
        // forward from the synthetic current reaches "new conversation".
        // (Returning to it relies on the refresh, which lands quickly.)
        assert_eq!(
            core.seat.fold().input_route().chain_root.as_deref(),
            Some("T-new"),
            "starts on the unfetched current chain"
        );
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(
            core.seat.fold().input_route().chain_root,
            None,
            "synthetic current did not trap the cycle — moved to new conversation"
        );
    }

    #[test]
    fn key_context_completion_is_cursor_aware() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_input_view(&mut core); // completion: service:commands/list + target
        core.data.commands = Some(serde_json::json!({
            "commands": [{ "tokens": ["deploy"], "description": "d" }]
        }));

        // Cursor at the end of "/de" with a matching record → can accept.
        set_focused_input(&mut core, "/de");
        let ctx = core.key_context();
        assert!(
            ctx.input_can_accept_completion,
            "cursor at end + a match → completion can accept (Tab completes)"
        );
        assert!(ctx.input_target_cycle.is_some(), "targeting still exposed");

        // Same text, cursor mid-line → completion would no-op, so it must NOT
        // claim it can accept (Tab should cycle the target instead).
        core.focused_input_buffer_mut()
            .unwrap()
            .set_text("/de".to_string(), 1);
        assert!(
            !core.key_context().input_can_accept_completion,
            "cursor mid-line → cannot accept; Tab cycles, not a no-op completion"
        );

        // Prose (no leading slash) → cannot accept either.
        core.focused_input_buffer_mut()
            .unwrap()
            .set_text("hello world".to_string(), 11);
        assert!(!core.key_context().input_can_accept_completion);
    }

    #[test]
    fn writable_input_submit_emits_invoke_with_text_bound_whole() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        set_focused_input(&mut core, "  run this  ");

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
                ..
            }) if item_ref == "service:threads/input"
                && params["input"] == "run this"
                && params["directive"] == "directive:demo/base"
        ));
        // Buffer survives until delivery succeeds.
        assert_eq!(focused_input_text(&core), "  run this  ");
    }

    #[test]
    fn plain_submit_carries_no_interrupt_intent() {
        // Steer is the daemon default → the wire omits `intent` entirely
        // (backward-compatible with older daemons).
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        set_focused_input(&mut core, "steer me");

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SubmitInput,
        });
        let Some(StudioEffectKind::Invoke { params, .. }) =
            effects.first().map(|e| &e.kind)
        else {
            panic!("expected an Invoke effect");
        };
        assert_eq!(params["input"], "steer me");
        assert!(params.get("intent").is_none(), "steer must not set intent");
    }

    #[test]
    fn interrupt_submit_sets_interrupt_intent() {
        // Alt+Enter → SubmitInputInterrupt injects intent=interrupt so a running
        // thread cuts its current cognition.
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        set_focused_input(&mut core, "stop, do X");

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SubmitInputInterrupt,
        });
        let Some(StudioEffectKind::Invoke { params, .. }) =
            effects.first().map(|e| &e.kind)
        else {
            panic!("expected an Invoke effect");
        };
        assert_eq!(params["input"], "stop, do X");
        assert_eq!(params["intent"], "interrupt");
    }

    #[test]
    fn complete_input_accepts_top_slash_candidate() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_input_view(&mut core);
        core.data.commands = Some(serde_json::json!({
            "commands": [
                { "invocable": true, "tokens": ["thread", "list"], "description": "List threads" },
                { "invocable": true, "tokens": ["thread", "get"], "description": "Get thread", "arguments": [{ "name": "thread_id" }] }
            ]
        }));
        set_focused_input(&mut core, "/thr");

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CompleteInput,
        });

        assert!(effects.is_empty());
        assert_eq!(focused_input_text(&core), "/thread ");
        assert_eq!(
            core.focused_input_buffer().unwrap().cursor,
            "/thread ".len()
        );
    }

    #[test]
    fn complete_input_accepts_top_mention() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        // An input declaring an @-mention source (projected from threads).
        core.views.insert(
            "view:ryeos/input".to_string(),
            serde_json::from_value(serde_json::json!({
                "widget": "text",
                "input": {
                    "id": "line",
                    "submit": "route",
                    "mentions": {
                        "ref": "service:threads/list",
                        "collection": "threads",
                        "reference": "thread_id",
                        "label": "item_ref"
                    }
                }
            }))
            .unwrap(),
        );
        // The refs land under the mention source key via the generic fetch.
        core.data.sources.insert(
            crate::studio::content::mention_source_key("view:ryeos/input", "line"),
            serde_json::json!({ "threads": [
                { "thread_id": "T-ab", "item_ref": "directive:ops/base" },
                { "thread_id": "T-cd", "item_ref": "directive:demo/chat" }
            ]}),
        );
        set_focused_input(&mut core, "look @T-a");

        // Cursor sits in an @-mention with a match → Tab can accept it.
        assert!(core.key_context().input_can_accept_completion);

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CompleteInput,
        });
        assert!(effects.is_empty());
        assert_eq!(focused_input_text(&core), "look @T-ab ");
    }

    #[test]
    fn submit_without_route_warns_and_emits_nothing() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_input_view(&mut core);
        set_focused_input(&mut core, "hello");
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
        set_focused_input(&mut core, "run this");
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
        assert!(focused_input_text(&core).is_empty());
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread.as_deref(), Some("T-9"));
        // First turn of a conversation: the launched thread is the chain root
        // (root == head), so the feed (which follows chain_root) shows it.
        assert_eq!(route.chain_root.as_deref(), Some("T-9"));
        // Pinned invocation survives the ratchet.
        assert!(route.has_target());
    }

    #[test]
    fn launch_does_not_ratchet_a_non_targeting_input() {
        // The ratchet keys off the input's `target` declaration, not the
        // invoke ref: an input that does NOT declare conversation targeting is
        // never retargeted onto the produced thread (no false "continuing").
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        core.views.insert(
            "view:ryeos/input".to_string(),
            serde_json::from_value(serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "submit": "route" }
            }))
            .unwrap(),
        );
        set_focused_input(&mut core, "go");
        let effect = core
            .dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: effect.id,
                ok: true,
                kind: StudioEffectResultKind::Invoked,
                data: Some(serde_json::json!({ "thread_id": "T-9", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread, None, "non-targeting input is not retargeted");
        assert_eq!(route.chain_root, None);
    }

    #[test]
    fn ratchet_eligibility_is_captured_at_issue_time_not_result_time() {
        // A targeting input submits → eligibility captured TRUE on the effect.
        // Focus then moves to a non-targeting input before the async result
        // lands. The launch must STILL ratchet (issue-time decision), proving
        // the result handler doesn't recompute from current focus.
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core); // targeting input focused
        set_focused_input(&mut core, "hi");
        let effect = core
            .dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");

        // Focus moves to a NON-targeting input while the launch is in flight.
        core.views.insert(
            "view:ryeos/input".to_string(),
            serde_json::from_value(serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "submit": "route" }
            }))
            .unwrap(),
        );
        assert!(
            core.focused_input_target_cycle().is_none(),
            "focus now resolves a non-targeting input"
        );

        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: effect.id,
                ok: true,
                kind: StudioEffectResultKind::Invoked,
                data: Some(serde_json::json!({ "thread_id": "T-9", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(
            route.thread.as_deref(),
            Some("T-9"),
            "ratcheted on the issue-time decision, not the moved focus"
        );
        assert_eq!(route.chain_root.as_deref(), Some("T-9"));
    }

    #[test]
    fn refused_delivery_surfaces_reason_and_does_not_ratchet() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core); // targeting input → ratchet would be eligible
        set_focused_input(&mut core, "go");
        let effect = core
            .dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        // Daemon refuses (e.g. non-continuation target / duplicate conflict).
        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: effect.id,
                ok: true,
                kind: StudioEffectResultKind::Invoked,
                data: Some(serde_json::json!({
                    "thread_id": serde_json::Value::Null,
                    "delivery": "refused",
                    "notice": "thread is not continuation-capable"
                })),
                error: None,
            },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread, None, "refused → no ratchet");
        assert_eq!(route.chain_root, None);
        assert!(
            core.ui
                .notices
                .iter()
                .any(|n| n.message.contains("not continuation-capable")),
            "surfaces the daemon's refusal reason, not a generic success"
        );
    }

    #[test]
    fn continuation_advances_head_but_preserves_chain_root() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);

        // Turn 1: starts the conversation. root == head == T-1.
        set_focused_input(&mut core, "hello");
        let e1 = core
            .dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: e1.id,
                ok: true,
                kind: StudioEffectResultKind::Invoked,
                data: Some(serde_json::json!({ "thread_id": "T-1", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.seat.fold().input_route();
        assert_eq!(route.thread.as_deref(), Some("T-1"));
        assert_eq!(route.chain_root.as_deref(), Some("T-1"));

        // Turn 2: a follow-up braids onto T-1 → new head T-2, same root.
        set_focused_input(&mut core, "and again");
        let e2 = core
            .dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: e2.id,
                ok: true,
                kind: StudioEffectResultKind::Invoked,
                data: Some(serde_json::json!({ "thread_id": "T-2", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.seat.fold().input_route();
        // Head advanced to the new turn; the next submit braids onto it.
        assert_eq!(route.thread.as_deref(), Some("T-2"));
        // Root unchanged — the feed keeps showing the whole conversation.
        assert_eq!(route.chain_root.as_deref(), Some("T-1"));
    }

    #[test]
    fn stale_invoke_result_never_retargets_newer_route() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        set_focused_input(&mut core, "first");
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
        set_focused_input(&mut core, "hold on");
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
        assert_eq!(focused_input_text(&core), "hold on");
        assert!(core
            .ui
            .notices
            .last()
            .is_some_and(|notice| notice.message.contains("refused")));
    }

    /// Seed a filtered-list view (`feeds` -> source param) into a focused
    /// center tile and return the tile id string (buffer instance id).
    fn seed_filter_tile(core: &mut StudioCore) -> String {
        seed_view_value(
            core,
            "view:test/filter",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:test/items", "params": { "limit": 50 }, "collection": "items" },
                "input": { "id": "q", "placeholder": "filter…", "feeds": { "param": "query", "debounce_ms": 120 } }
            }),
        );
        let tile_id = core.workspace.add_tile(ViewSpec {
            view_ref: "view:test/filter".to_string(),
        });
        core.workspace.focused_tile = tile_id;
        tile_id.0.to_string()
    }

    #[test]
    fn feeds_input_drives_its_source_param() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let tile_id = seed_filter_tile(&mut core);
        // The focused tile declares `input.feeds`, so it owns input.
        assert!(core.has_focused_input());

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetInputText {
                text: "wid".to_string(),
                cursor: 3,
            },
        });

        // Editing a feeds buffer refetches the source with the buffer text
        // injected into the named param.
        let fetch = effects.iter().find_map(|effect| match &effect.kind {
            StudioEffectKind::FetchSource {
                tile_id: fetched,
                source_ref,
                params,
            } => Some((fetched.clone(), source_ref.clone(), params.clone())),
            _ => None,
        });
        let (fetched, source_ref, params) = fetch.expect("feeds edit refetches source");
        assert_eq!(fetched, tile_id);
        assert_eq!(source_ref, "service:test/items");
        assert_eq!(params["query"], "wid");
        assert_eq!(params["limit"], 50);
    }

    #[test]
    fn feeds_input_has_no_submit_and_allows_read_only() {
        // `feeds` works in a read-only session (no durable write); Enter
        // does nothing.
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        seed_filter_tile(&mut core);
        let edit = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::InsertInputChar { ch: 'x' },
        });
        assert!(
            edit.iter()
                .any(|e| matches!(e.kind, StudioEffectKind::FetchSource { .. })),
            "feeds refetch is allowed read-only"
        );
        let submit = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SubmitInput,
        });
        assert!(submit.is_empty());
        // No read-only notice: a feeds input has no submit to block.
        assert!(core.ui.notices.is_empty());
    }

    #[test]
    fn submit_affordance_fires_with_value_payload() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/palette",
            serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "submit": "run" },
                "affordances": [{
                    "id": "run",
                    "invoke": { "plane": "rye", "tokens": ["thread", "input"], "args": { "line": "{value}" } }
                }]
            }),
        );
        let tile_id = core.workspace.add_tile(ViewSpec {
            view_ref: "view:test/palette".to_string(),
        });
        core.workspace.focused_tile = tile_id;
        set_focused_input(&mut core, "do the thing");

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SubmitInput,
        });

        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::Invoke {
                target: super::super::effect::InvokeRef::Tokens { tokens },
                params,
                route_seq: None,
                ..
            }) if tokens == &vec!["thread".to_string(), "input".to_string()]
                && params["line"] == "do the thing"
        ));
    }

    #[test]
    fn submit_affordance_blocked_when_read_only() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/palette",
            serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "submit": "run" },
                "affordances": [{
                    "id": "run",
                    "invoke": { "plane": "rye", "tokens": ["x"], "args": { "line": "{value}" } }
                }]
            }),
        );
        let tile_id = core.workspace.add_tile(ViewSpec {
            view_ref: "view:test/palette".to_string(),
        });
        core.workspace.focused_tile = tile_id;
        set_focused_input(&mut core, "blocked");
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SubmitInput,
        });
        assert!(effects.is_empty());
        assert!(core
            .ui
            .notices
            .last()
            .is_some_and(|notice| notice.message.contains("read-only")));
    }

    #[test]
    fn duplicate_view_instances_have_independent_buffers() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/filter",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:test/items", "params": {}, "collection": "items" },
                "input": { "id": "q", "feeds": { "param": "query" } }
            }),
        );
        let first = core.workspace.add_tile(ViewSpec {
            view_ref: "view:test/filter".to_string(),
        });
        let second = core.workspace.add_tile(ViewSpec {
            view_ref: "view:test/filter".to_string(),
        });
        assert_ne!(first, second);

        core.workspace.focused_tile = first;
        set_focused_input(&mut core, "first-buffer");
        core.workspace.focused_tile = second;
        set_focused_input(&mut core, "second-buffer");

        // The same `view:` rendered twice keeps independent buffers.
        core.workspace.focused_tile = first;
        assert_eq!(focused_input_text(&core), "first-buffer");
        core.workspace.focused_tile = second;
        assert_eq!(focused_input_text(&core), "second-buffer");
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
    fn interrupt_head_cancels_a_running_head_thread() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({ "thread": "T-run" }),
        );
        core.data.threads = Some(StudioThreadsDto {
            threads: vec![serde_json::json!({ "thread_id": "T-run", "status": "running" })],
        });
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::InterruptHead,
        });
        assert!(
            effects.iter().any(|e| matches!(
                &e.kind,
                StudioEffectKind::CancelThread { thread_id } if thread_id == "T-run"
            )),
            "running head → cancel effect: {effects:?}"
        );
    }

    #[test]
    fn interrupt_head_is_noop_when_head_settled() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({ "thread": "T-done" }),
        );
        core.data.threads = Some(StudioThreadsDto {
            threads: vec![serde_json::json!({ "thread_id": "T-done", "status": "completed" })],
        });
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::InterruptHead,
        });
        assert!(effects.is_empty(), "settled head → no interrupt");
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
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        let before = core.workspace.tile_ids().len();
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
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
    fn single_lens_open_view_replaces_center_instead_of_splitting() {
        // The cell-grid (TUI) composition: one center lens. Opening a
        // different view swaps the lens in place — the tile count stays at
        // one, no split, and the new view fetches.
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.workspace.tiling.mode = crate::surface::TilingModeSpec::SingleLens;
        seed_view(&mut core, "view:ryeos/items/space");
        seed_view(&mut core, "view:test/services");

        // First open fills the empty center with the one lens.
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        assert_eq!(core.workspace.tile_ids().len(), 1);
        core.ui.motion.clear();

        // Switching the lens replaces in place — still exactly one tile.
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
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
                .any(|event| matches!(event, StudioMotionEventVm::TileSplit { .. })),
            "no split motion when swapping the single lens"
        );
        assert!(
            matches!(
                effects.first().map(|effect| &effect.kind),
                Some(StudioEffectKind::FetchSource { .. })
            ),
            "the swapped-in lens fetches its source"
        );

        // OpenNewView also collapses to a replace — no second tile.
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
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
            surface_ref: "surface:ryeos/studio/lens".to_string(),
            user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
            effective_surface: Some(serde_json::json!({
                "name": "lens-test",
                "library": ["view:a", "view:scene", "view:input", "view:b"],
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
        let mut core = StudioCore::new(session, BrowserViewport::default(), 0);
        core.workspace.tiling.mode = crate::surface::TilingModeSpec::SingleLens;

        // The lens-able library excludes the scene backdrop and the input.
        assert_eq!(
            core.lens_library(),
            vec!["view:a".to_string(), "view:b".to_string()]
        );

        let open = |core: &mut StudioCore, view_ref: &str| {
            core.dispatch(StudioEvent::Ui {
                event: StudioUiEvent::Activate {
                    action: StudioAction::OpenView {
                        view: ViewSpec {
                            view_ref: view_ref.to_string(),
                        },
                    },
                },
            });
        };
        let cycle = |core: &mut StudioCore| {
            core.dispatch(StudioEvent::Ui {
                event: StudioUiEvent::Activate {
                    action: StudioAction::CycleTab {
                        direction: StudioStackMoveDirection::Down,
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
    fn inspect_is_a_plain_selection_facet_write() {
        // Inspection is a facet write, peer to input.route — the engine never
        // opens, names, or swaps to the inspector. The center lens is
        // unchanged and no tile is added; the inspector is a facet-bound view
        // (slot or lens) reached by ordinary navigation, live via on_facet.
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.workspace.tiling.mode = crate::surface::TilingModeSpec::SingleLens;
        seed_view(&mut core, "view:ryeos/items/space");

        // Start on a list lens.
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        assert_eq!(core.workspace.tile_ids().len(), 1);

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InspectItem {
                    canonical_ref: "tool:ryeos/x".to_string(),
                },
            },
        });

        // The selection facet is set …
        assert_eq!(
            core.seat.fold().get(crate::studio::seat::KEY_SELECTION),
            Some(&serde_json::json!({ "item": "tool:ryeos/x" })),
        );
        // … and nothing was opened or swapped: same single tile, same lens.
        assert_eq!(core.workspace.tile_ids().len(), 1);
        assert!(
            matches!(core.workspace.focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:ryeos/items/space"),
            "inspect does not open or swap to the inspector — it only writes the facet"
        );
    }

    #[test]
    fn open_new_view_allows_duplicate_workspace_tiles() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:ryeos/items/space");
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        let before = core.workspace.tile_ids().len();
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
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
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/threads/list".to_string(),
                    },
                },
            },
        });
        let tile_id = core.workspace.tile_ids()[1];
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CloseTile {
                    tile_id: tile_id.0.to_string(),
                },
            },
        });

        assert!(!core.workspace.tiles.contains_key(&tile_id));
        assert!(!core.workspace.tile_ids().contains(&tile_id));
        assert!(core.ui.motion.iter().any(|event| matches!(
            event,
            StudioMotionEventVm::TileExit { tile_id: closed } if closed == &tile_id.0.to_string()
        )));
    }

    #[test]
    fn closing_last_app_tile_empties_center() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        assert!(!core.workspace.center_is_empty());

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CloseFocused,
            },
        });

        assert!(core.workspace.center_is_empty());
        // The last-tile close emits a tile-exit motion (no home mode).
        assert!(core
            .ui
            .motion
            .iter()
            .any(|event| matches!(event, StudioMotionEventVm::TileExit { .. })));
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
            Some(ViewSpec { view_ref }) if view_ref == "view:ryeos/items/space"
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
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        // First tile is the master (right side under the default tiling).
        let master = core.workspace.focused_tile;
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/threads/list".to_string(),
                    },
                },
            },
        });
        // The new tile lands in the stack region on the left.
        let stacked = core.workspace.focused_tile;
        assert_ne!(master, stacked);

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::FocusDirection {
                direction: FocusDirection::Right,
            },
        });

        assert_eq!(core.workspace.focused_tile, master);
    }

    #[test]
    fn master_stack_places_master_right_and_stack_left() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/threads/list".to_string(),
                    },
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
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
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/services");
        seed_view(&mut core, "view:test/files");
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:test/files".to_string(),
                    },
                },
            },
        });
        let first_tab_tiles = core.workspace.tile_ids().len();

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::SwitchTab { index: 1 },
            },
        });

        assert_eq!(core.active_workspace, 1);
        // Fresh tabs start at home: an empty center.
        assert_eq!(core.workspace.tile_ids().len(), 0);

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/files".to_string(),
                    },
                },
            },
        });
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenNewView {
                    view: ViewSpec {
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
        assert_eq!(core.workspaces[0].tile_ids().len(), first_tab_tiles);
        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchSource { .. })));
    }

    #[test]
    fn invalid_close_tile_does_not_close_focused_tile() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/services");
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        let focused = core.workspace.focused_tile;
        let count = core.workspace.tile_ids().len();

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::CloseTile {
                    tile_id: "999".to_string(),
                },
            },
        });

        assert_eq!(core.workspace.focused_tile, focused);
        assert_eq!(core.workspace.tile_ids().len(), count);
        assert!(core.workspace.tiles.contains_key(&focused));
    }

    #[test]
    fn mismatched_effect_result_does_not_apply_data() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:ryeos/items/space");
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec {
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

    #[test]
    fn thread_tail_deltas_accumulate_then_clear_on_durable() {
        let mut core = StudioCore::default();

        // Streaming cognition deltas accumulate; no refetch while live.
        let effects = core.dispatch(StudioEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "cognition_out".to_string(),
            payload: serde_json::json!({ "delta": "Hel" }),
        });
        assert!(effects.is_empty());
        core.dispatch(StudioEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "cognition_out".to_string(),
            payload: serde_json::json!({ "delta": "lo" }),
        });
        assert_eq!(
            core.data.live_delta.as_ref().map(|d| d.text.as_str()),
            Some("Hello")
        );

        // The settled turn (content, no delta) supersedes the live buffer.
        core.dispatch(StudioEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "cognition_out".to_string(),
            payload: serde_json::json!({ "content": "Hello", "turn": 1 }),
        });
        assert!(core.data.live_delta.is_none());
    }

    #[test]
    fn thread_tail_ephemeral_nontext_is_noop() {
        let mut core = StudioCore::default();
        let effects = core.dispatch(StudioEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "stream_opened".to_string(),
            payload: serde_json::json!({}),
        });
        assert!(effects.is_empty());
        assert!(core.data.live_delta.is_none());
    }

    #[test]
    fn submit_thread_command_targets_commands_submit_for_head_thread() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({ "thread": "T-1" }),
        );
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::SubmitThreadCommand {
                    command: crate::studio::dto::ThreadControlCommand::Interrupt,
                },
            },
        });
        assert_eq!(effects.len(), 1);
        let StudioEffectKind::Invoke { target, params, .. } = &effects[0].kind else {
            panic!("expected an Invoke effect");
        };
        assert!(matches!(
            target,
            crate::studio::effect::InvokeRef::Ref { item_ref }
                if item_ref == "service:commands/submit"
        ));
        assert_eq!(params["thread_id"], "T-1");
        assert_eq!(params["command_type"], "interrupt");
    }

    #[test]
    fn submit_thread_command_without_head_thread_notices() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::SubmitThreadCommand {
                    command: crate::studio::dto::ThreadControlCommand::Interrupt,
                },
            },
        });
        assert!(effects.is_empty());
        assert!(!core.ui.notices.is_empty());
    }
}
