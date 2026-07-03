//! Studio reducer: the single dispatch over `StudioCore`.
//!
//! `StudioCore::dispatch` is the one public entry; it fans out to the UI and
//! action routers here, then to the concern-cluster modules. State is genuinely
//! shared across clusters (`open_view` touches workspace + seat + data), so the
//! dispatch is not sliced — the split is by concern, not by state ownership, and
//! `StudioCore`'s public API is unchanged.
//!
//! Clusters:
//! - [`input`] — input buffers, routing, targeting, submit.
//! - [`tiles`] — workspace/tile motion and lens/tab switching.
//! - [`affordances`] — content affordance resolution and facet/view fetch effects.
//! - [`effect_results`] — platform effect-result application (launch/ratchet, parse/store).
//!
//! Growth policy: a new interaction cluster gets a new module; any module
//! crossing ~800 impl lines splits. `view_model.rs` gets the same recipe when it
//! next grows — it already delegates from `build_view_model` to focused `*_vm`
//! builders, so it is not split preemptively.

mod affordances;
mod effect_results;
mod input;
mod tiles;
#[cfg(test)]
mod test_support;

use super::effect::{StudioEffect, StudioEffectKind};
use super::event::{StudioAction, StudioEvent, StudioStackMoveDirection, StudioUiEvent};
use super::model::StudioCore;
use super::view_model::{action_for_focused_row, launcher_items_for, StudioMotionEventVm, StudioTone};
use crate::ids::TileId;
use crate::workspace::{ViewSpec};
pub(crate) use super::{content, dto, effect, event, model, seat, tokenize, view_model};

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

    pub(crate) fn dispatch_ui(&mut self, event: StudioUiEvent) -> Vec<StudioEffect> {
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
                let live_filter = self.focused_input_is_live_filter();
                let Some(buffer) = self.focused_input_buffer_mut() else {
                    return Vec::new();
                };
                buffer.insert_char(ch);
                self.bump_generation();
                self.feeds_effects_unless_live_filter(live_filter)
            }
            StudioUiEvent::DeleteInputChar => {
                let live_filter = self.focused_input_is_live_filter();
                let Some(buffer) = self.focused_input_buffer_mut() else {
                    return Vec::new();
                };
                buffer.delete_before_cursor();
                self.bump_generation();
                self.feeds_effects_unless_live_filter(live_filter)
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
                    self.views
                        .get(&view_ref)
                        .and_then(|binding| binding.input.as_ref())
                        .and_then(|input| input.completion.as_ref())
                        .and_then(|completion| {
                            let response = self.data.sources.get(
                                &super::content::completion_source_key(&view_ref, &key.input_id),
                            )?;
                            let records = super::content::completion_records(completion, response);
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
            StudioUiEvent::CycleFilterField { forward } => self.cycle_filter_field(forward),
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

    pub(crate) fn dispatch_action(&mut self, action: StudioAction) -> Vec<StudioEffect> {
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
            StudioAction::PrefillRetryTurn {
                thread_id,
                chain_root_id,
                input,
            } => {
                if self.is_read_only() {
                    self.notice("This session is read-only.", StudioTone::Warn);
                    return Vec::new();
                }
                // Retarget the route at the SELECTED failed thread — not the
                // current head, which the ratchet has advanced past — so the
                // next submit continues THAT turn into a fresh successor. Merge
                // so any other route fields (e.g. the directive) survive.
                let effects = self.apply_ui_affordance(
                    super::seat::KEY_INPUT_ROUTE.to_string(),
                    None,
                    Some(serde_json::json!({ "thread": thread_id, "chain_root": chain_root_id })),
                    None,
                );
                // Stage the failed turn's stimulus for review; the operator
                // presses Enter to resubmit through the normal submit path. No
                // Invoke here — retry is pre-fill, not one-click.
                if let Some(buffer) = self.focused_input_buffer_mut() {
                    let cursor = input.len();
                    buffer.set_text(input, cursor);
                }
                self.bump_generation();
                effects
            }
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
                    // Steer the head thread through the shared control channel:
                    // semantic intent as a typed effect, the executor maps it to
                    // the daemon's control endpoint.
                    vec![self.emit(StudioEffectKind::SubmitThreadCommand {
                        thread_id,
                        command_type: command.as_str().to_string(),
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

    pub(crate) fn is_read_only(&self) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::reducer::test_support::*;

    #[test]
    fn start_emits_initial_effects() {
        let mut core = StudioCore::default();
        let effects = core.dispatch(StudioEvent::Start {
            session: session(),
            viewport: BrowserViewport::default(),
            now_ms: 0,
        });

        // Dimension + Projects + Topology. Completion/commands is fetched only
        // for inputs that declare it (this fixture's input does not).
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

    #[test]
    fn key_context_completion_is_cursor_aware() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_input_view(&mut core); // completion: service:commands/list + target
        seed_commands(
            &mut core,
            serde_json::json!({
                "commands": [{ "tokens": ["deploy"], "description": "d" }]
            }),
        );

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
    fn complete_input_accepts_top_slash_candidate() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_input_view(&mut core);
        seed_commands(
            &mut core,
            serde_json::json!({
                "commands": [
                    { "invocable": true, "tokens": ["thread", "list"], "description": "List threads" },
                    { "invocable": true, "tokens": ["thread", "get"], "description": "Get thread", "arguments": [{ "name": "thread_id" }] }
                ]
            }),
        );
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
    fn submit_thread_command_emits_typed_effect_for_head_thread() {
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
        let StudioEffectKind::SubmitThreadCommand {
            thread_id,
            command_type,
        } = &effects[0].kind
        else {
            panic!("expected a SubmitThreadCommand effect");
        };
        assert_eq!(thread_id, "T-1");
        assert_eq!(command_type, "interrupt");
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

    #[test]
    fn inspect_summary_writes_the_facet_and_a_summary_inspector_renders_it() {
        // The correction a prior prototype missed: writing `selection.summary`
        // is not enough — a view must READ it. The summary-capable inspector is
        // facet-backed (renders `selection.summary` directly, no service round
        // trip) so an inspected error terminal is actually visible.
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/item/inspector",
            serde_json::json!({
                "widget": "key_value",
                "facet": "selection.summary",
                "source": {
                    "ref": "service:ui/studio/item/inspect",
                    "params": { "canonical_ref": "@facet:selection.item" }
                },
                "projections": { "detail": ["canonical_ref", "title", "detail"] },
                "refresh": { "on_facet": "selection" }
            }),
        );
        // Open the right slot so the inspector renders in the dock plane.
        core.ui.docks.right.as_mut().unwrap().visible = true;

        // Enter on a failed feed line → InspectSummary (title = the visible line,
        // detail = the full raw event).
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InspectSummary {
                    title: "failed — boom".to_string(),
                    detail: serde_json::json!({
                        "event_type": "thread_failed",
                        "thread_id": "T-1",
                        "payload": { "error": { "message": "boom" } }
                    }),
                },
            },
        });

        // The facet carries the summary …
        let fold = core.seat.fold();
        assert_eq!(fold.get("selection").unwrap()["summary"]["title"], "failed — boom");

        // … and the inspector actually RENDERS it.
        let vm = build_view_model(&core);
        let dock = vm.workspace.docks.right.expect("right dock open");
        let rows = match dock.view {
            crate::studio::view_model::StudioViewVm::Rows { rows, .. } => rows,
            other => panic!("expected the key_value inspector to render rows, got {other:?}"),
        };
        assert!(
            rows.iter()
                .any(|row| row.primary.starts_with("title:") && row.primary.contains("failed — boom")),
            "the inspector renders the summary title: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.primary.starts_with("detail:") && row.primary.contains("thread_failed")),
            "the inspector renders the full-event detail: {rows:?}"
        );
    }

    #[test]
    fn prefill_retry_turn_retargets_route_and_stages_input() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_input_view(&mut core);
        // A later thread is the current head (the ratchet advanced past the
        // failed turn); retry must retarget at the SELECTED failed thread.
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "thread": "T-head",
                "chain_root": "R-head"
            }),
        );

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::PrefillRetryTurn {
                    thread_id: "T-failed".to_string(),
                    chain_root_id: "R-1".to_string(),
                    input: "run the thing".to_string(),
                },
            },
        });

        let route = core.seat.fold().input_route();
        assert_eq!(
            route.thread.as_deref(),
            Some("T-failed"),
            "route retargets at the selected failed thread, not the ratcheted head"
        );
        assert_eq!(route.chain_root.as_deref(), Some("R-1"));
        assert_eq!(focused_input_text(&core), "run the thing", "the failed turn's input is staged");
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e.kind, StudioEffectKind::Invoke { .. })),
            "retry is pre-fill, not one-click — no submit is emitted"
        );
    }

    #[test]
    fn prefill_retry_turn_read_only_is_a_noop_with_notice() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0); // read-only
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::PrefillRetryTurn {
                    thread_id: "T-failed".to_string(),
                    chain_root_id: "R-1".to_string(),
                    input: "run".to_string(),
                },
            },
        });
        assert!(effects.is_empty());
        assert_eq!(focused_input_text(&core), "", "read-only stages nothing");
        assert!(
            core.seat.fold().input_route().thread.is_none(),
            "read-only does not retarget the route"
        );
        assert!(core.ui.notices.iter().any(|n| n.message.contains("read-only")));
    }

    #[test]
    fn retry_prefill_then_submit_continues_the_selected_failed_thread() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_input_view(&mut core);
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "params": { "directive": "directive:demo/base" },
                "thread": "T-head",
                "chain_root": "R-head"
            }),
        );

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::PrefillRetryTurn {
                    thread_id: "T-failed".to_string(),
                    chain_root_id: "R-1".to_string(),
                    input: "retry me".to_string(),
                },
            },
        });
        assert_eq!(focused_input_text(&core), "retry me");

        // The operator reviews, then Enter → the normal submit path, now aimed
        // at the selected failed thread (a continuation), not the prior head.
        let effect = core
            .dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit emits an effect");
        let StudioEffectKind::Invoke { params, .. } = &effect.kind else {
            panic!("submit emits an Invoke, got {:?}", effect.kind);
        };
        assert_eq!(
            params["thread"], "T-failed",
            "the resubmit continues the selected failed thread"
        );
        assert_eq!(params["input"], "retry me");
    }
}
