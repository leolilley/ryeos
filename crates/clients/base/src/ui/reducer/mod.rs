//! RyeOs reducer: the single dispatch over `RyeOsCore`.
//!
//! `RyeOsCore::dispatch` is the one public entry; it fans out to the UI and
//! intent routers here, then to the concern-cluster modules. State is genuinely
//! shared across clusters (`open_view` touches view_set + seat + data), so the
//! dispatch is not sliced — the split is by concern, not by state ownership, and
//! `RyeOsCore`'s public API is unchanged.
//!
//! Clusters:
//! - `input` — input buffers, routing, targeting, submit.
//! - `tiles` — view_set/tile motion and lens/tab switching.
//! - `affordances` — content affordance resolution and facet/view fetch effects.
//! - `effect_results` — platform effect-result application (launch/ratchet, parse/store).
//!
//! Growth policy: a new interaction cluster gets a new module; any module
//! crossing ~800 impl lines splits. `view_model.rs` gets the same recipe when it
//! next grows — it already delegates from `build_view_model` to focused `*_vm`
//! builders, so it is not split preemptively.

mod affordances;
mod attachments;
mod binding_lifecycle;
mod effect_results;
mod field_interaction;
mod input;
mod saved_view_sets;
#[cfg(test)]
pub(crate) mod test_support;
mod tiles;

use super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::event::{RyeOsEvent, RyeOsStackMoveDirection, RyeOsUiEvent, RyeOsUiIntent};
use super::model::RyeOsCore;
use super::view_model::{RyeOsMotionEventVm, RyeOsTone, intent_for_focused_row};
pub(crate) use super::{content, dto, effect, event, model, seat, tokenize, view_model};
use crate::view_set::ViewSpec;
use serde::Deserialize;

impl RyeOsCore {
    pub fn dispatch(&mut self, event: RyeOsEvent) -> Vec<RyeOsEffect> {
        self.ui.motion.clear();
        match event {
            RyeOsEvent::Start {
                session,
                viewport,
                now_ms,
            } => {
                *self = RyeOsCore::new(session, viewport, now_ms);
                self.bump_generation();
                self.initial_effects()
            }
            RyeOsEvent::Ui { event } => self.dispatch_ui(event),
            RyeOsEvent::EffectResult { result } => self.apply_effect_result(result),
            RyeOsEvent::DaemonEvent { payload } => self.apply_daemon_event(payload),
            RyeOsEvent::HintReceived { kind, payload } => {
                self.note_hint_received(&kind, &payload);
                Vec::new()
            }
            RyeOsEvent::HintFlushBatch { kinds } => self.effects_for_hints(&kinds),
            RyeOsEvent::TransportStateChanged {
                channel,
                freshness,
                observed_at_ms,
                error,
            } => self.apply_transport_state(channel, freshness, observed_at_ms, error),
            RyeOsEvent::ThreadTail {
                thread_id,
                event_type,
                payload,
            } => self.apply_thread_tail(&thread_id, &event_type, &payload),
            RyeOsEvent::Tick { now_ms } => {
                let dt_ms = now_ms.saturating_sub(self.runtime.last_tick_ms);
                self.runtime.last_tick_ms = now_ms;
                self.runtime.now_ms = now_ms;
                if self.runtime.activity_pulse > 0.0 {
                    self.runtime.activity_pulse *= 0.9_f32.powf(dt_ms as f32 / 250.0);
                    if self.runtime.activity_pulse < 0.005 {
                        self.runtime.activity_pulse = 0.0;
                    }
                }
                self.advance_backdrop_break(dt_ms);
                self.expire_field_changes(now_ms);
                let effects = self.advance_field_playback();
                // The frame clock advances `generation` so generation-keyed
                // motion (the backdrop twinkle, via the generic scene
                // renderer) steps each tick. The loop already repaints on
                // tick; bumping generation is what makes the step visible.
                self.bump_generation();
                effects
            }
            RyeOsEvent::Resize { viewport } => {
                self.runtime.viewport = viewport;
                self.bump_generation();
                Vec::new()
            }
            RyeOsEvent::RouteChanged { route } => {
                self.ui.route = Some(route.clone());
                if let Some(view) = view_from_route(&route) {
                    return self.open_view(view);
                }
                self.bump_generation();
                Vec::new()
            }
        }
    }

    fn apply_transport_state(
        &mut self,
        channel: super::event::RyeOsTransportChannel,
        freshness: super::event::RyeOsTransportFreshness,
        observed_at_ms: Option<u64>,
        error: Option<super::effect::RyeOsUiError>,
    ) -> Vec<RyeOsEffect> {
        let previous = self.runtime.transport.channels.get(&channel).cloned();
        let entering_gap = freshness
            == super::event::RyeOsTransportFreshness::GapResnapshotRequired
            && previous.as_ref().map(|state| state.freshness) != Some(freshness);
        let state = super::model::RyeOsTransportChannelState {
            freshness,
            last_observed_at_ms: observed_at_ms.or_else(|| {
                previous
                    .as_ref()
                    .and_then(|state| state.last_observed_at_ms)
            }),
            error: match freshness {
                super::event::RyeOsTransportFreshness::Current => None,
                super::event::RyeOsTransportFreshness::ExpiredOrRevoked => {
                    error.map(super::effect::RyeOsUiError::normalized)
                }
                _ => error
                    .map(super::effect::RyeOsUiError::normalized)
                    .or_else(|| previous.as_ref().and_then(|state| state.error.clone())),
            },
        };
        if previous.as_ref() == Some(&state) {
            return Vec::new();
        }
        let newly_visible_error = state.error.as_ref().and_then(|error| {
            let already_visible = previous
                .as_ref()
                .and_then(|previous| previous.error.as_ref())
                == Some(error);
            (!already_visible).then(|| {
                let tone = if error.outcome == super::effect::RyeOsEffectOutcome::Unknown {
                    RyeOsTone::Warn
                } else {
                    RyeOsTone::Danger
                };
                (error.message.clone(), tone)
            })
        });
        self.runtime.transport.channels.insert(channel, state);
        if let Some((message, tone)) = newly_visible_error {
            self.notice_deduped(message, tone);
        }
        if freshness == super::event::RyeOsTransportFreshness::ExpiredOrRevoked {
            self.notice_deduped(
                "This UI session expired or was revoked. Relaunch it to continue.",
                RyeOsTone::Warn,
            );
        }
        self.bump_generation();
        if entering_gap {
            // `initial_effects` contains observation only. Never replay a
            // pending mutation merely because a renderer reports a gap.
            self.initial_effects()
        } else {
            Vec::new()
        }
    }

    fn apply_daemon_event(&mut self, payload: serde_json::Value) -> Vec<RyeOsEffect> {
        match decode_ui_intent_applied(payload) {
            UiIntentDecode::Known(intent) => self.apply_ui_intent_applied(intent),
            // A real `ui_intent.applied` this build cannot decode means
            // another client mutated shared session state — resync.
            UiIntentDecode::Unsupported => self.initial_effects(),
            // Anything else on the session bus never drives a refetch;
            // bound views refresh through hints they declared.
            UiIntentDecode::NotUiIntent => Vec::new(),
        }
    }

    fn apply_ui_intent_applied(&mut self, intent: AppliedUiIntent) -> Vec<RyeOsEffect> {
        match intent {
            AppliedUiIntent::OpenView { view } => self.open_view(view),
            AppliedUiIntent::OpenOverlay { overlay_id, query } => {
                let mut effects = self.dispatch_ui(RyeOsUiEvent::OpenOverlay { overlay_id });
                if let Some(query) = query {
                    effects.extend(self.dispatch_ui(RyeOsUiEvent::SetOverlayQuery { query }));
                }
                effects
            }
            AppliedUiIntent::CloseOverlay => self.dispatch_ui(RyeOsUiEvent::CloseOverlay),
            AppliedUiIntent::SetOverlayQuery { query } => {
                self.dispatch_ui(RyeOsUiEvent::SetOverlayQuery { query })
            }
            AppliedUiIntent::FocusInput => self.dispatch_ui(RyeOsUiEvent::FocusInput),
            AppliedUiIntent::FocusView { tile_id } => {
                self.dispatch_ui(RyeOsUiEvent::FocusChanged {
                    target: Some(tile_id),
                })
            }
        }
    }

    pub(crate) fn dispatch_ui(&mut self, event: RyeOsUiEvent) -> Vec<RyeOsEffect> {
        match event {
            RyeOsUiEvent::InputAt { address, action } => {
                self.dispatch_addressed_input(address, action)
            }
            RyeOsUiEvent::Activate { intent } => self.dispatch_intent(intent),
            RyeOsUiEvent::SetFilter {
                tile_id,
                field,
                value,
            } => self.set_tile_filter(tile_id, field, value),
            RyeOsUiEvent::SetFilesRoot { .. } | RyeOsUiEvent::SetFilesPath { .. } => {
                // File tiles are content-bound; path state lives in the
                // view binding's params.
                Vec::new()
            }
            RyeOsUiEvent::SetAtlasLayerVisible {
                tile_id,
                kind,
                visible,
            } => {
                self.atlas_target_mut(&tile_id)
                    .set_layer_visible(kind, visible);
                self.bump_generation();
                Vec::new()
            }
            RyeOsUiEvent::SetAtlasLens { tile_id, lens } => {
                self.atlas_target_mut(&tile_id).set_lens(lens);
                self.bump_generation();
                Vec::new()
            }
            RyeOsUiEvent::SetAtlasProjection {
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
                    crate::atlas::AtlasProjectionVm::AiSpace => self
                        .fetch_atlas_source_role(tile_id.as_deref(), "items", serde_json::json!({}))
                        .into_iter()
                        .collect(),
                    crate::atlas::AtlasProjectionVm::FileSpace => {
                        if self.atlas_target_has_project_bound(tile_id.as_deref()) {
                            let (root, path) = {
                                let atlas = self.atlas_target(&tile_id);
                                (atlas.file_space_root.clone(), atlas.file_space_path.clone())
                            };
                            self.fetch_atlas_source_role(
                                tile_id.as_deref(),
                                "file_space",
                                serde_json::json!({
                                    "root": root,
                                    "path": path
                                }),
                            )
                            .into_iter()
                            .collect()
                        } else {
                            Vec::new()
                        }
                    }
                }
            }
            RyeOsUiEvent::SetAtlasFileSpacePath {
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
                if self.atlas_target_has_project_bound(tile_id.as_deref()) {
                    let (root, path) = {
                        let atlas = self.atlas_target(&tile_id);
                        (atlas.file_space_root.clone(), atlas.file_space_path.clone())
                    };
                    self.fetch_atlas_source_role(
                        tile_id.as_deref(),
                        "file_space",
                        serde_json::json!({
                            "root": root,
                            "path": path
                        }),
                    )
                    .into_iter()
                    .collect()
                } else {
                    Vec::new()
                }
            }
            event @ (RyeOsUiEvent::SetFieldSelection { .. }
            | RyeOsUiEvent::MoveFieldSelection { .. }
            | RyeOsUiEvent::SetFieldGroupCollapsed { .. }
            | RyeOsUiEvent::SetFieldLayerVisible { .. }
            | RyeOsUiEvent::SetFieldCursor { .. }
            | RyeOsUiEvent::StepFieldCursor { .. }
            | RyeOsUiEvent::SetFieldPlayback { .. }
            | RyeOsUiEvent::SetFieldQuery { .. }
            | RyeOsUiEvent::MoveFieldSearchMatch { .. }
            | RyeOsUiEvent::ToggleFieldCompare { .. }
            | RyeOsUiEvent::RequestFieldExpansion { .. }
            | RyeOsUiEvent::ContinueFieldExpansion { .. }
            | RyeOsUiEvent::ClearFieldExpansion { .. }) => self.dispatch_field_event(event),
            RyeOsUiEvent::FocusChanged { target } => {
                let Some(tile_id) = target
                    .and_then(|target| target.parse::<u64>().ok())
                    .map(crate::ids::TileId::new)
                else {
                    return Vec::new();
                };
                if self.view_sets[self.active_view_set]
                    .tiles
                    .contains_key(&tile_id)
                {
                    self.view_sets[self.active_view_set].focus_tile(tile_id);
                    self.view_sets[self.active_view_set].focus_target =
                        Some(super::model::RyeOsFocusTarget::ViewSetTile {
                            tile_id: self.view_sets[self.active_view_set]
                                .focused_tile
                                .0
                                .to_string(),
                        });
                    self.push_motion(RyeOsMotionEventVm::FocusChanged {
                        tile_id: tile_id.0.to_string(),
                    });
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::FocusDock { edge } => {
                let Some(slot) = self.view_sets[self.active_view_set]
                    .docks
                    .slot(edge)
                    .filter(|slot| slot.visible)
                else {
                    return Vec::new();
                };
                let super::model::RyeOsDockContent::View { view_ref } = &slot.content;
                let view_ref = view_ref.clone();
                self.view_sets[self.active_view_set].focus_target =
                    Some(super::model::RyeOsFocusTarget::Dock { edge });
                let key = super::model::dock_view_instance_key(
                    self.view_sets[self.active_view_set].id,
                    edge,
                );
                self.view_sets[self.active_view_set]
                    .dock_local
                    .entry(key.clone())
                    .or_insert_with(initial_list_local_state);
                self.bump_generation();
                self.emit_fetch_source_for_instance(key, &view_ref)
            }
            RyeOsUiEvent::FocusDirection { direction } => {
                if self.view_sets[self.active_view_set].focus_in_direction(direction) {
                    self.view_sets[self.active_view_set].focus_target =
                        Some(super::model::RyeOsFocusTarget::ViewSetTile {
                            tile_id: self.view_sets[self.active_view_set]
                                .focused_tile
                                .0
                                .to_string(),
                        });
                    self.push_motion(RyeOsMotionEventVm::FocusChanged {
                        tile_id: self.view_sets[self.active_view_set]
                            .focused_tile
                            .0
                            .to_string(),
                    });
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::OpenOverlay { overlay_id } => {
                self.ui.overlay.active = Some(overlay_id);
                self.ui.overlay.query.clear();
                self.ui.overlay.selected = 0;
                self.bump_generation();
                Vec::new()
            }
            RyeOsUiEvent::CloseOverlay => {
                if self.ui.overlay.active.is_some() {
                    self.ui.overlay.active = None;
                    self.ui.overlay.query.clear();
                    self.ui.overlay.selected = 0;
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::SetOverlayQuery { query } => {
                self.ui.overlay.query = query;
                // Enter always lands on a match: a live query renders group
                // headers inert, so the selection snaps to the first
                // actionable row rather than resting on an inert header.
                self.ui.overlay.selected = 0;
                let items = super::view_model::active_overlay_items(self);
                if let Some(first) = items.iter().position(|item| item.enabled) {
                    self.ui.overlay.selected = first;
                }
                self.bump_generation();
                Vec::new()
            }
            RyeOsUiEvent::FocusInput => {
                if self.focus_default_input() {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::BlurInput => {
                if matches!(
                    self.view_sets[self.active_view_set].focus_target.as_ref(),
                    Some(super::model::RyeOsFocusTarget::Dock { .. })
                ) {
                    self.view_sets[self.active_view_set].focus_target =
                        Some(super::model::RyeOsFocusTarget::ViewSetTile {
                            tile_id: self.view_sets[self.active_view_set]
                                .focused_tile
                                .0
                                .to_string(),
                        });
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::InsertInputChar { ch } => {
                let live_filter = self.focused_input_is_live_filter();
                let Some(buffer) = self.focused_input_buffer_mut() else {
                    return Vec::new();
                };
                buffer.insert_char(ch);
                self.bump_generation();
                self.feeds_effects_unless_live_filter(live_filter)
            }
            RyeOsUiEvent::DeleteInputChar => {
                let live_filter = self.focused_input_is_live_filter();
                let Some(buffer) = self.focused_input_buffer_mut() else {
                    return Vec::new();
                };
                buffer.delete_before_cursor();
                self.bump_generation();
                self.feeds_effects_unless_live_filter(live_filter)
            }
            RyeOsUiEvent::SetInputText { text, cursor } => {
                let Some(buffer) = self.focused_input_buffer_mut() else {
                    return Vec::new();
                };
                buffer.set_text(text, cursor);
                self.bump_generation();
                self.effects_for_focused_feeds()
            }
            RyeOsUiEvent::CompleteInput => {
                let Some((key, view_ref)) = self.focused_input_instance() else {
                    return Vec::new();
                };
                let buffer = self.view_sets[self.active_view_set]
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
                        .binding_for_instance(&key.view_instance_key, &view_ref)
                        .and_then(|binding| binding.input.as_ref())
                        .and_then(|input| input.mentions.as_ref())
                        .and_then(|mentions| {
                            let response = self.data.sources.get(
                                &super::source_key::RyeOsSourceInstanceKey::mention(
                                    key.view_instance_key.clone(),
                                    &key.input_id,
                                )
                                .encode(),
                            )?;
                            Some(super::content::project_mentions(mentions, response))
                        })
                        .unwrap_or_default();
                    super::tokenize::accept_mention_completion(
                        &records,
                        &buffer.text,
                        buffer.cursor,
                    )
                } else {
                    self.binding_for_instance(&key.view_instance_key, &view_ref)
                        .and_then(|binding| binding.input.as_ref())
                        .and_then(|input| input.completion.as_ref())
                        .and_then(|completion| {
                            let response = self.data.sources.get(
                                &super::source_key::RyeOsSourceInstanceKey::completion(
                                    key.view_instance_key.clone(),
                                    &key.input_id,
                                )
                                .encode(),
                            )?;
                            let records = super::content::completion_records(completion, response);
                            super::tokenize::accept_slash_completion(
                                records,
                                &buffer.text,
                                buffer.cursor,
                            )
                        })
                };
                if let Some((text, cursor)) = completed
                    && let Some(buffer) = self.focused_input_buffer_mut()
                {
                    buffer.set_text(text, cursor);
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::CycleInputTarget { forward } => self.cycle_input_target(forward),
            RyeOsUiEvent::CycleFilterField { forward } => self.cycle_filter_field(forward),
            RyeOsUiEvent::InterruptHead => {
                let Some((origin, _)) = self.focused_input_instance() else {
                    return Vec::new();
                };
                if self.instance_has_unresolved_required_subject(&origin.view_instance_key) {
                    self.notice(
                        "This view requires a subject before controlling work.",
                        RyeOsTone::Warn,
                    );
                    return Vec::new();
                }
                // Esc while the head thread works → cancel it through the single
                // ryeos cancel path: `service:commands/submit { cancel }`, the
                // same channel row affordances use. No-op if
                // there's no running head.
                let Some(head) = self.focused_input_route().thread else {
                    return Vec::new();
                };
                if !self.head_thread_running(&head) {
                    return Vec::new();
                }
                if self.refuse_blocked_mutation_for_instance(&origin.view_instance_key) {
                    return Vec::new();
                }
                if self.has_pending_cancel(&head) {
                    self.notice(
                        format!("Cancel {head} is already pending."),
                        RyeOsTone::Warn,
                    );
                    return Vec::new();
                }
                let Some(coordinate) = self.thread_control_coordinate() else {
                    self.notice(
                        "The effective UI declares no unambiguous thread-control affordance.",
                        RyeOsTone::Warn,
                    );
                    return Vec::new();
                };
                let Some((request, request_bounds)) = self.compiled_binding_operation(
                    &origin.view_instance_key,
                    coordinate,
                    crate::ui::binding::UiBindingPayload::Selection {
                        record: serde_json::json!({
                            "thread_id": head,
                            "command_type": "cancel",
                        }),
                    },
                ) else {
                    return Vec::new();
                };
                vec![self.emit(RyeOsEffectKind::InvokeBinding {
                    request,
                    request_bounds,
                    intent: super::effect::InvokeIntent::Service,
                    success_notice: None,
                    invocation_origin: Some(origin.view_instance_key.clone()),
                    input_origin: None,
                    route_seq: None,
                    ratchet_on_thread_id: false,
                })]
            }
            RyeOsUiEvent::SubmitInput => self.submit_focused_input(false),
            RyeOsUiEvent::SubmitInputInterrupt => self.submit_focused_input(true),
            RyeOsUiEvent::MoveOverlaySelection { delta } => {
                let items = super::view_model::active_overlay_items(self);
                let len = items.len();
                if len > 0 {
                    let mut next = self.ui.overlay.selected.min(len.saturating_sub(1));
                    for _ in 0..len {
                        next = wrap_index(next, delta, len);
                        if items.get(next).is_some_and(|item| item.enabled) {
                            break;
                        }
                    }
                    self.ui.overlay.selected = next;
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::SetOverlaySelection { item_id } => {
                let items = super::view_model::active_overlay_items(self);
                if let Some(index) = items
                    .iter()
                    .position(|item| item.id == item_id && item.enabled)
                    && self.ui.overlay.selected != index
                {
                    self.ui.overlay.selected = index;
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::ChooseOverlayAt { item_id, secondary } => {
                let items = super::view_model::active_overlay_items(self);
                let Some(index) = items
                    .iter()
                    .position(|item| item.id == item_id && item.enabled)
                else {
                    return Vec::new();
                };
                self.ui.overlay.selected = index;
                self.dispatch_ui(RyeOsUiEvent::ChooseOverlay { secondary })
            }
            RyeOsUiEvent::ChooseOverlay { secondary } => {
                let items = super::view_model::active_overlay_items(self);
                let selected = self.ui.overlay.selected.min(items.len().saturating_sub(1));
                let intent = items.get(selected).and_then(|item| {
                    if !item.enabled {
                        return None;
                    }
                    if secondary {
                        item.secondary_intent
                            .clone()
                            .or_else(|| item.intent.clone())
                    } else {
                        item.intent.clone()
                    }
                });
                let Some(intent) = intent else {
                    return Vec::new();
                };
                // A group fold acts inside the overlay — it never closes it.
                if matches!(intent, RyeOsUiIntent::ToggleOverlayGroup { .. }) {
                    return self.dispatch_intent(intent);
                }
                self.ui.overlay.active = None;
                self.ui.overlay.query.clear();
                self.ui.overlay.selected = 0;
                self.bump_generation();
                self.dispatch_intent(intent)
            }
            RyeOsUiEvent::FoldOverlayGroup { expand } => {
                // Folds act on the tree presentation; a live search shows
                // matches under force-expanded, inert headers.
                if !self.ui.overlay.query.trim().is_empty() {
                    return Vec::new();
                }
                let items = super::view_model::active_overlay_items(self);
                if items.is_empty() {
                    return Vec::new();
                }
                let selected = self.ui.overlay.selected.min(items.len() - 1);
                let group = items[selected].category.clone();
                // Only rows under a real header fold — flat overlays
                // (commands, help) share the item shape but have none.
                let Some(header_idx) = items
                    .iter()
                    .position(|item| item.header && item.category == group)
                else {
                    return Vec::new();
                };
                let collapsed = self.ui.overlay.collapsed.contains(&group);
                if expand == collapsed {
                    if expand {
                        self.ui.overlay.collapsed.remove(&group);
                    } else {
                        self.ui.overlay.collapsed.insert(group.clone());
                        // The fold hides only the group's leaves, which all
                        // sit AFTER their header in the visible list — the
                        // header's index is unchanged, so the selection
                        // lands there without re-projecting.
                        self.ui.overlay.selected = header_idx;
                    }
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::SetTileCursor { tile_id, index } => {
                let Some(tile_id) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                if self.set_tile_cursor(tile_id, index) {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::SetViewCursor {
                instance_key,
                index,
            } => {
                if self.set_view_cursor(&instance_key, index) {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::ChooseViewItem {
                instance_key,
                item_id,
                activate,
            } => {
                let Some((cursor, intent)) =
                    super::view_model::view_pointer_item(self, &instance_key, &item_id)
                else {
                    return Vec::new();
                };
                if self.set_view_cursor(&instance_key, cursor) {
                    self.bump_generation();
                }
                if activate && let Some(intent) = intent {
                    return self.dispatch_intent(intent);
                }
                Vec::new()
            }
            RyeOsUiEvent::ToggleViewItemExpansion {
                instance_key,
                item_id,
                expand,
            } => {
                let Some((cursor, expanded, row_key)) =
                    super::view_model::view_pointer_expansion(self, &instance_key, &item_id)
                else {
                    return Vec::new();
                };
                let cursor_changed = self.set_view_cursor(&instance_key, cursor);
                let expansion_changed = expanded != expand
                    && self.set_view_row_expanded_key(&instance_key, row_key, expand);
                if cursor_changed || expansion_changed {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::DismissNotice { id } => {
                let before = self.ui.notices.len();
                self.ui.notices.retain(|notice| notice.id != id);
                if self.ui.notices.len() != before {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::ToggleViewSection {
                instance_key,
                section_id,
            } => {
                let Some((section, cursor, collapsed)) =
                    super::view_model::view_section(self, &instance_key, &section_id)
                else {
                    return Vec::new();
                };
                let cursor_changed = collapsed && self.set_view_cursor(&instance_key, cursor);
                let fold_changed = self.set_view_fold(&instance_key, section, !collapsed);
                if cursor_changed || fold_changed {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::SetFold {
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
            RyeOsUiEvent::SetViewFold {
                instance_key,
                section,
                collapsed,
            } => {
                if self.set_view_fold(&instance_key, section, collapsed) {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::ExpandSelectedRow { expand } => {
                if self.set_focused_row_expanded(expand) {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::SetTreeRowCollapsed { collapsed } => {
                if self.set_focused_tree_row_collapsed(collapsed) {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::ActivateFocused => intent_for_focused_row(self)
                .map_or_else(Vec::new, |intent| self.dispatch_intent(intent)),
            RyeOsUiEvent::PopLens => self.pop_view(),
        }
    }

    pub(crate) fn dispatch_intent(&mut self, intent: RyeOsUiIntent) -> Vec<RyeOsEffect> {
        match intent {
            RyeOsUiIntent::Refresh => self.initial_effects(),
            RyeOsUiIntent::InvokeAffordance {
                instance_key,
                view_ref,
                affordance_id,
                record,
            } => self.invoke_affordance(&instance_key, &view_ref, &affordance_id, &record),
            RyeOsUiIntent::OpenOverlay { overlay_id } => self.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::OpenOverlay { overlay_id },
            }),
            RyeOsUiIntent::ToggleOverlayGroup { group } => {
                if !self.ui.overlay.collapsed.remove(&group) {
                    self.ui.overlay.collapsed.insert(group);
                }
                self.bump_generation();
                Vec::new()
            }
            RyeOsUiIntent::OpenView { view } => {
                if let Some(destination) = self
                    .surface_navigation()
                    .into_iter()
                    .find(|entry| entry.view == view.view_ref)
                {
                    self.seat.append_facet(
                        super::seat::KEY_NAVIGATION_DESTINATION,
                        serde_json::Value::String(destination.id),
                    );
                }
                let mut effects = self.open_view(view.clone());
                if let Some(hash) = route_for_view(&view) {
                    effects.push(self.emit(RyeOsEffectKind::SetLocationHash {
                        hash: hash.to_string(),
                    }));
                }
                effects
            }
            RyeOsUiIntent::OpenNewView { view } => {
                // Single-lens surfaces have no "another tile": a new-view
                // request collapses to replacing the one center lens.
                if self.view_sets[self.active_view_set].tiling.mode
                    == crate::surface::TilingModeSpec::SingleLens
                {
                    self.open_view(view)
                } else {
                    let effects = self.add_center_tile(view);
                    self.bump_generation();
                    effects
                }
            }
            RyeOsUiIntent::CloseFocused => {
                if self.close_tile_or_empty(self.view_sets[self.active_view_set].focused_tile) {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::CloseTile { tile_id } => {
                let Some(tile_id) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                if self.close_tile_or_empty(tile_id) {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::ToggleTileMaximized { tile_id } => {
                let Some(tile_id) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                if self.view_sets[self.active_view_set].toggle_maximized(tile_id) {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::ToggleFocusedMaster => {
                if self.view_sets[self.active_view_set].tiling.mode
                    == crate::surface::TilingModeSpec::MasterStack
                    && self.view_sets[self.active_view_set].zoom_focused()
                {
                    self.push_motion(RyeOsMotionEventVm::FocusChanged {
                        tile_id: self.view_sets[self.active_view_set]
                            .focused_tile
                            .0
                            .to_string(),
                    });
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::PromoteTileToMaster {
                layout_guard,
                tile_id,
            } => {
                if layout_guard != self.layout_guard()
                    || self.view_sets[self.active_view_set].tiling.mode
                        != crate::surface::TilingModeSpec::MasterStack
                {
                    return Vec::new();
                }
                let Some(tile_id) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                if self.view_sets[self.active_view_set]
                    .tile_ids()
                    .first()
                    .copied()
                    == Some(tile_id)
                {
                    return Vec::new();
                }
                if self.view_sets[self.active_view_set].zoom_tile(tile_id) {
                    self.view_sets[self.active_view_set].focus_target =
                        Some(super::model::RyeOsFocusTarget::ViewSetTile {
                            tile_id: tile_id.0.to_string(),
                        });
                    self.push_motion(RyeOsMotionEventVm::FocusChanged {
                        tile_id: tile_id.0.to_string(),
                    });
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::MoveFocusedTile { direction } => {
                let delta = match direction {
                    RyeOsStackMoveDirection::Up => -1,
                    RyeOsStackMoveDirection::Down => 1,
                };
                if self.view_sets[self.active_view_set].move_focused_in_stack(delta) {
                    self.push_motion(RyeOsMotionEventVm::FocusChanged {
                        tile_id: self.view_sets[self.active_view_set]
                            .focused_tile
                            .0
                            .to_string(),
                    });
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::MoveTileBeside {
                layout_guard,
                tile_id,
                target_tile_id,
                edge,
            } => {
                if layout_guard != self.layout_guard() {
                    return Vec::new();
                }
                if let (Some(tile), Some(target)) =
                    (parse_tile_id(&tile_id), parse_tile_id(&target_tile_id))
                    && self.view_sets[self.active_view_set].move_tile_beside(tile, target, edge)
                {
                    self.view_sets[self.active_view_set].focus_target =
                        Some(super::model::RyeOsFocusTarget::ViewSetTile {
                            tile_id: tile.0.to_string(),
                        });
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::CycleTab { direction } => self.cycle_view_set_tab(direction),
            RyeOsUiIntent::CycleViewTab { direction } => {
                let forward = matches!(direction, RyeOsStackMoveDirection::Down);
                if self.view_sets[self.active_view_set].cycle_view_tab(forward) {
                    self.view_sets[self.active_view_set].focus_target =
                        Some(super::model::RyeOsFocusTarget::ViewSetTile {
                            tile_id: self.view_sets[self.active_view_set]
                                .focused_tile
                                .0
                                .to_string(),
                        });
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::MoveTileToGroup {
                layout_guard,
                tile_id,
                target_tile_id,
                index,
            } => {
                if layout_guard != self.layout_guard() {
                    return Vec::new();
                }
                if let (Some(tile), Some(target)) =
                    (parse_tile_id(&tile_id), parse_tile_id(&target_tile_id))
                    && self.view_sets[self.active_view_set].move_tile_to_group(tile, target, index)
                {
                    self.view_sets[self.active_view_set].focus_target =
                        Some(super::model::RyeOsFocusTarget::ViewSetTile {
                            tile_id: tile.0.to_string(),
                        });
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::SwitchTab { index } => self.switch_view_set_tab(index),
            RyeOsUiIntent::NewViewSet => self.new_view_set(),
            RyeOsUiIntent::SelectViewSet { view_set_id } => {
                match self
                    .view_sets
                    .iter()
                    .position(|view_set| view_set.id == view_set_id)
                {
                    Some(index) => self.switch_view_set_tab(index),
                    None => Vec::new(),
                }
            }
            RyeOsUiIntent::RenameViewSet { view_set_id, title } => {
                self.rename_view_set(view_set_id, &title);
                Vec::new()
            }
            RyeOsUiIntent::DuplicateViewSet { view_set_id } => self.duplicate_view_set(view_set_id),
            RyeOsUiIntent::CloseViewSet { view_set_id } => self.close_view_set(view_set_id),
            RyeOsUiIntent::MoveTileToViewSet {
                layout_guard,
                tile_id,
                view_set_id,
            } => {
                if layout_guard != self.layout_guard() {
                    return Vec::new();
                }
                let Some(tile) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                let Some(target) = self
                    .view_sets
                    .iter()
                    .position(|view_set| view_set.id == view_set_id)
                else {
                    return Vec::new();
                };
                let source = self.active_view_set;
                if target == source {
                    return Vec::new();
                }
                let Some(instance_key) = self.view_sets[source]
                    .tiles
                    .get(&tile)
                    .map(|tile| tile.instance_key.clone())
                else {
                    return Vec::new();
                };
                let retains_selection_owner = self
                    .mounted_view_ref(&instance_key)
                    .and_then(|view_ref| self.binding_for_instance(&instance_key, view_ref))
                    .is_some_and(super::attachment::participates_in_selection);
                let attachment = retains_selection_owner
                    .then(|| self.selection_attachment_for_instance(&instance_key))
                    .flatten();
                let moved = if source < target {
                    let (before, after) = self.view_sets.split_at_mut(target);
                    before[source].move_tile_to_view_set(&mut after[0], tile)
                } else {
                    let (before, after) = self.view_sets.split_at_mut(source);
                    after[0].move_tile_to_view_set(&mut before[target], tile)
                };
                if moved {
                    if let Some(attachment) = attachment {
                        self.selection_attachments
                            .insert(instance_key.clone(), attachment);
                    } else {
                        self.selection_attachments.remove(&instance_key);
                    }
                    self.switch_view_set_tab(target)
                } else {
                    Vec::new()
                }
            }
            RyeOsUiIntent::PinViewSelection { instance_key } => {
                self.pin_view_selection(instance_key)
            }
            RyeOsUiIntent::OpenPinnedViewAlongside { instance_key } => {
                self.open_pinned_view_alongside(instance_key)
            }
            RyeOsUiIntent::FollowViewSetSelection {
                instance_key,
                view_set_id,
            } => self.follow_view_set_selection(instance_key, view_set_id),
            RyeOsUiIntent::SupplyRequiredSubject {
                instance_key,
                source_view_set_id,
            } => self.supply_required_subject(instance_key, source_view_set_id),
            RyeOsUiIntent::ResizeSplit {
                layout_guard,
                path,
                ratio,
            } => {
                if layout_guard == self.layout_guard()
                    && self.view_sets[self.active_view_set]
                        .root
                        .as_mut()
                        .is_some_and(|root| root.set_split_ratio(&path, ratio))
                {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::ToggleTopStatusBar => {
                self.ui.top_status_visible = !self.ui.top_status_visible;
                self.bump_generation();
                Vec::new()
            }
            RyeOsUiIntent::ToggleBottomStatusBar => {
                self.ui.bottom_status_visible = !self.ui.bottom_status_visible;
                self.bump_generation();
                Vec::new()
            }
            RyeOsUiIntent::ToggleBackdropBreak => {
                self.toggle_backdrop_break();
                Vec::new()
            }
            RyeOsUiIntent::ToggleDock { edge } => {
                // Toggling flips a surface-declared slot open/closed; a
                // closed slot frees its space. Absent edges have no slot.
                let Some(slot) = self.view_sets[self.active_view_set].docks.slot_mut(edge) else {
                    return Vec::new();
                };
                slot.visible = !slot.visible;
                let shown_view = if slot.visible {
                    let super::model::RyeOsDockContent::View { view_ref } = &slot.content;
                    Some(view_ref.clone())
                } else {
                    None
                };
                let key = super::model::dock_view_instance_key(
                    self.view_sets[self.active_view_set].id,
                    edge,
                );
                self.normalize_field_local_states();
                self.bump_generation();
                shown_view
                    .map(|view_ref| self.emit_fetch_source_for_instance(key, &view_ref))
                    .unwrap_or_default()
            }
            RyeOsUiIntent::ResizeFocused { direction } => {
                if self.resize_focused_dock(direction)
                    || self.view_sets[self.active_view_set].resize_focused_split(direction)
                {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiIntent::SelectDimension => self.apply_ui_affordance(
                super::seat::KEY_SELECTION.to_string(),
                Some(serde_json::json!({ "dimension": true })),
                None,
                None,
                false,
            ),
            // Inspection IS selection: a facet write on the seat braid.
            // Inspection IS selection: a facet write, peer to `input.route`.
            // The engine never opens or names the inspector — it's a view that
            // reads `@facet:selection.*` and refreshes `on_facet: selection`,
            // shown as a slot or a lens like any other facet-bound view.
            RyeOsUiIntent::InspectItem { canonical_ref } => self.apply_ui_affordance(
                super::seat::KEY_SELECTION.to_string(),
                Some(serde_json::json!({ "item": canonical_ref })),
                None,
                None,
                false,
            ),
            RyeOsUiIntent::InspectThread { thread_id } => self.apply_ui_affordance(
                super::seat::KEY_SELECTION.to_string(),
                Some(serde_json::json!({ "thread_id": thread_id })),
                None,
                None,
                false,
            ),
            RyeOsUiIntent::AimThread { thread_id } => self.apply_ui_affordance(
                super::seat::KEY_INPUT_ROUTE.to_string(),
                None,
                Some(serde_json::json!({ "thread": thread_id })),
                None,
                false,
            ),
            // Step into a child execution: retarget the route at the child AND
            // push a return frame (drill = true), so Backspace walks back to the
            // parent braid. No `open_view` — the braid lens re-projects onto the
            // child via the route facet, keeping view refs out of code.
            RyeOsUiIntent::DrillThread {
                thread_id,
                chain_root_id,
                label,
            } => {
                let effects = self.apply_ui_affordance(
                    super::seat::KEY_INPUT_ROUTE.to_string(),
                    None,
                    Some(serde_json::json!({ "thread": thread_id, "chain_root": chain_root_id })),
                    None,
                    true,
                );
                // Prefer the node name (e.g. `study`) over the default child-id
                // label the drill just set, so the breadcrumb reads the cognition.
                if let Some(label) = label {
                    self.view_sets[self.active_view_set].lens_label = Some(label);
                }
                effects
            }
            RyeOsUiIntent::PrefillRetryTurn {
                thread_id,
                chain_root_id,
                input,
            } => {
                let Some((origin, _)) = self.focused_input_instance() else {
                    return Vec::new();
                };
                if self.instance_has_unresolved_required_subject(&origin.view_instance_key) {
                    self.notice(
                        "This view requires a subject before controlling work.",
                        RyeOsTone::Warn,
                    );
                    return Vec::new();
                }
                if self.refuse_blocked_mutation_for_instance(&origin.view_instance_key) {
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
                    false,
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
            RyeOsUiIntent::InspectSummary { title, detail } => self.apply_ui_affordance(
                super::seat::KEY_SELECTION.to_string(),
                Some(serde_json::json!({ "summary": { "title": title, "detail": detail } })),
                None,
                None,
                false,
            ),
            RyeOsUiIntent::ReadFile { root, path } => {
                if !self.has_project_bound() && file_root_requires_project(&root) {
                    self.notice("No project is bound to this session.", RyeOsTone::Warn);
                    return Vec::new();
                }
                self.seat.append_facet(
                    super::seat::selection_facet_key(self.view_sets[self.active_view_set].id),
                    serde_json::json!({ "file": { "root": root, "path": path } }),
                );
                self.bump_generation();
                self.fetch_surface_source_role(
                    "file_read",
                    serde_json::json!({ "root": root, "path": path }),
                )
                .into_iter()
                .collect()
            }
            RyeOsUiIntent::ReleaseBindingAttachment {
                binding_attachment_id,
                binding_generation,
                binding_digest,
            } => self.release_binding_attachment(
                binding_attachment_id,
                binding_generation,
                binding_digest,
            ),
            RyeOsUiIntent::CopyText { text } => {
                vec![self.emit(RyeOsEffectKind::CopyToClipboard { text })]
            }
            RyeOsUiIntent::OpenExternal { url } => {
                vec![self.emit(RyeOsEffectKind::OpenUrl { url })]
            }
            RyeOsUiIntent::SubmitThreadCommand { command } => {
                let Some((origin, _)) = self.focused_input_instance() else {
                    return Vec::new();
                };
                if self.refuse_blocked_mutation_for_instance(&origin.view_instance_key) {
                    Vec::new()
                } else if let Some(thread_id) = self.focused_input_route().thread {
                    // Thread control is the input view's signed affordance, not
                    // a privileged renderer endpoint. The selection is
                    // bounded data; the binding owns the executable target.
                    let Some(coordinate) = self.thread_control_coordinate() else {
                        self.notice(
                            "The effective UI declares no unambiguous thread-control affordance.",
                            RyeOsTone::Warn,
                        );
                        return Vec::new();
                    };
                    if command == crate::ui::dto::ThreadControlCommand::Cancel
                        && self.has_pending_thread_command(
                            &origin.view_instance_key,
                            &thread_id,
                            command,
                            &coordinate,
                        )
                    {
                        self.notice(
                            format!("Cancel {thread_id} is already pending."),
                            RyeOsTone::Warn,
                        );
                        return Vec::new();
                    }
                    let Some((request, request_bounds)) = self.compiled_binding_operation(
                        &origin.view_instance_key,
                        coordinate,
                        crate::ui::binding::UiBindingPayload::Selection {
                            record: serde_json::json!({
                                "thread_id": thread_id,
                                "command_type": command.as_str(),
                            }),
                        },
                    ) else {
                        return Vec::new();
                    };
                    vec![self.emit(RyeOsEffectKind::InvokeBinding {
                        request,
                        request_bounds,
                        intent: super::effect::InvokeIntent::Service,
                        success_notice: None,
                        invocation_origin: Some(origin.view_instance_key.clone()),
                        input_origin: None,
                        route_seq: None,
                        ratchet_on_thread_id: false,
                    })]
                } else {
                    self.notice(
                        format!("No active thread to {}.", command.as_str()),
                        RyeOsTone::Warn,
                    );
                    Vec::new()
                }
            }
        }
    }

    /// Fold durable seat history and restore only state named by the current
    /// signed surface. Unknown or stale navigation destinations remain inert.
    pub fn replay_seat_events(
        &mut self,
        events: impl IntoIterator<Item = super::seat::SeatEvent>,
    ) -> Vec<RyeOsEffect> {
        let before = self.seat.fold().snapshot();
        for event in events {
            self.seat.append_replayed(event);
        }
        let after = self.seat.fold().snapshot();
        let changed = after
            .iter()
            .filter(|(key, value)| before.get(*key) != Some(*value))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();

        let mut effects = Vec::new();
        if changed
            .iter()
            .any(|key| key == super::seat::KEY_NAVIGATION_DESTINATION)
            && let Some(destination) = after
                .get(super::seat::KEY_NAVIGATION_DESTINATION)
                .and_then(serde_json::Value::as_str)
            && let Some(entry) = self
                .surface_navigation()
                .into_iter()
                .find(|entry| entry.id == destination)
        {
            effects.extend(self.open_view(ViewSpec::bound(entry.view)));
        }
        for key in changed {
            if key != super::seat::KEY_NAVIGATION_DESTINATION {
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
        }
        effects
    }

    fn toggle_backdrop_break(&mut self) {
        self.ui.backdrop_break_target = if self.ui.backdrop_break_target >= 0.5 {
            0.0
        } else {
            1.0
        };
        self.bump_generation();
    }

    fn advance_backdrop_break(&mut self, dt_ms: u64) {
        let target = self.ui.backdrop_break_target.clamp(0.0, 1.0);
        let current = self.ui.backdrop_break_amount.clamp(0.0, 1.0);
        let delta = target - current;
        if delta.abs() < 0.001 {
            self.ui.backdrop_break_amount = target;
            return;
        }
        let step = (dt_ms as f32 / 520.0).clamp(0.04, 0.22);
        self.ui.backdrop_break_amount =
            (current + delta.signum() * step.min(delta.abs())).clamp(0.0, 1.0);
    }

    pub(crate) fn has_pending_cancel(&self, thread_id: &str) -> bool {
        let Some((origin, _)) = self.focused_input_instance() else {
            return false;
        };
        let Some(coordinate) = self.thread_control_coordinate() else {
            return false;
        };
        self.has_pending_thread_command(
            &origin.view_instance_key,
            thread_id,
            crate::ui::dto::ThreadControlCommand::Cancel,
            &coordinate,
        )
    }

    fn has_pending_thread_command(
        &self,
        instance: &crate::ids::RyeOsViewInstanceKey,
        thread_id: &str,
        command: crate::ui::dto::ThreadControlCommand,
        coordinate: &crate::ui::binding::UiBindingCoordinate,
    ) -> bool {
        let Some(attachment) = self.binding_attachment_for_instance(instance) else {
            return false;
        };
        self.pending_effects.values().any(|kind| {
            matches!(
                kind,
                RyeOsEffectKind::InvokeBinding {
                    request: crate::ui::binding::UiBindingRequest {
                        binding_attachment_id,
                        binding_generation,
                        binding_digest,
                        coordinate: pending_coordinate,
                        payload: crate::ui::binding::UiBindingPayload::Selection { record },
                        ..
                    },
                    ..
                } if pending_coordinate == coordinate
                    && binding_attachment_id == &attachment.binding_attachment_id
                    && *binding_generation == attachment.binding_generation
                    && binding_digest == &attachment.binding_digest
                    && record.get("thread_id").and_then(serde_json::Value::as_str) == Some(thread_id)
                    && record.get("command_type").and_then(serde_json::Value::as_str) == Some(command.as_str())
            )
        })
    }

    pub(crate) fn mutation_block_reason(
        &self,
        instance: &crate::ids::RyeOsViewInstanceKey,
    ) -> Option<&'static str> {
        let Some(attachment) = self.binding_attachment_for_instance(instance) else {
            return Some("This UI has no current compiled operation binding.");
        };
        if attachment.binding_digest.is_empty() || attachment.binding_generation == 0 {
            return Some("This view has no current compiled operation binding.");
        }
        if attachment.posture == super::binding::UiEffectivePosture::ObservationOnly {
            return Some("This view's admitted binding is observation-only.");
        }
        match self.runtime.transport.overall_freshness() {
            super::event::RyeOsTransportFreshness::Current => None,
            super::event::RyeOsTransportFreshness::ExpiredOrRevoked => {
                Some("This UI session expired or was revoked. Relaunch it to continue.")
            }
            super::event::RyeOsTransportFreshness::GapResnapshotRequired => {
                Some("RyeOS state is stale while the UI resnapshots.")
            }
            super::event::RyeOsTransportFreshness::Connecting
            | super::event::RyeOsTransportFreshness::Reconnecting => {
                Some("RyeOS is reconnecting; wait for current state before acting.")
            }
        }
    }

    pub(crate) fn refuse_blocked_mutation_for_instance(
        &mut self,
        instance: &crate::ids::RyeOsViewInstanceKey,
    ) -> bool {
        let Some(reason) = self.mutation_block_reason(instance) else {
            return false;
        };
        self.notice_deduped(reason, RyeOsTone::Warn);
        true
    }
}

fn parse_tile_id(tile_id: &str) -> Option<crate::ids::TileId> {
    tile_id.parse::<u64>().ok().map(crate::ids::TileId::new)
}

fn wrap_index(current: usize, delta: i32, len: usize) -> usize {
    (current as i32 + delta).rem_euclid(len as i32) as usize
}

fn file_root_requires_project(root: &str) -> bool {
    matches!(root, "project" | "project_ai")
}

impl RyeOsCore {
    fn resize_focused_dock(&mut self, direction: crate::view_set::FocusDirection) -> bool {
        let super::model::RyeOsFocusTarget::Dock { edge } = self.focus_target() else {
            return false;
        };
        let Some(slot) = self.view_sets[self.active_view_set]
            .docks
            .slot_mut(edge)
            .filter(|slot| slot.visible)
        else {
            return false;
        };
        let delta: i16 = match (edge, direction) {
            (super::model::RyeOsDockEdge::Top, crate::view_set::FocusDirection::Down)
            | (super::model::RyeOsDockEdge::Bottom, crate::view_set::FocusDirection::Up)
            | (super::model::RyeOsDockEdge::Left, crate::view_set::FocusDirection::Right)
            | (super::model::RyeOsDockEdge::Right, crate::view_set::FocusDirection::Left) => 1,
            (super::model::RyeOsDockEdge::Top, crate::view_set::FocusDirection::Up)
            | (super::model::RyeOsDockEdge::Bottom, crate::view_set::FocusDirection::Down)
            | (super::model::RyeOsDockEdge::Left, crate::view_set::FocusDirection::Left)
            | (super::model::RyeOsDockEdge::Right, crate::view_set::FocusDirection::Right) => -1,
            _ => return false,
        };
        let next = (slot.size as i16 + delta).clamp(3, 18) as u16;
        if next == slot.size {
            return false;
        }
        slot.size = next;
        true
    }
}

fn initial_list_local_state() -> crate::view_set::ViewLocalState {
    crate::view_set::ViewLocalState::GenericList {
        cursor: 0,
        scroll: 0,
        collapsed: std::collections::BTreeSet::new(),
        expanded_rows: std::collections::BTreeSet::new(),
        collapsed_tree_rows: std::collections::BTreeSet::new(),
        changed_rows: std::collections::BTreeMap::new(),
    }
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

enum UiIntentDecode {
    Known(AppliedUiIntent),
    Unsupported,
    NotUiIntent,
}

enum AppliedUiIntent {
    OpenView {
        view: ViewSpec,
    },
    OpenOverlay {
        overlay_id: String,
        query: Option<String>,
    },
    CloseOverlay,
    SetOverlayQuery {
        query: String,
    },
    FocusInput,
    FocusView {
        tile_id: String,
    },
}

#[derive(Debug, Deserialize)]
struct UiIntentEnvelope {
    event_type: String,
    payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct UiIntentApplied {
    intent: String,
    #[serde(default)]
    payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OpenViewPayload {
    #[serde(default)]
    view_ref: Option<String>,
    #[serde(default)]
    view: Option<ViewSpec>,
}

#[derive(Debug, Deserialize)]
struct OpenOverlayPayload {
    overlay_id: String,
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SetOverlayQueryPayload {
    query: String,
}

#[derive(Debug, Deserialize)]
struct FocusViewPayload {
    tile_id: String,
}

fn decode_ui_intent_applied(payload: serde_json::Value) -> UiIntentDecode {
    let payload = match serde_json::from_value::<UiIntentEnvelope>(payload.clone()) {
        Ok(envelope) if envelope.event_type == "ui_intent.applied" => envelope.payload,
        Ok(_) => return UiIntentDecode::NotUiIntent,
        Err(_) => payload,
    };

    let Ok(applied) = serde_json::from_value::<UiIntentApplied>(payload) else {
        return UiIntentDecode::NotUiIntent;
    };

    match applied.intent.as_str() {
        "open_view" => {
            let Ok(payload) = serde_json::from_value::<OpenViewPayload>(applied.payload) else {
                return UiIntentDecode::Unsupported;
            };
            let view = payload
                .view
                .or_else(|| payload.view_ref.map(|view_ref| ViewSpec { view_ref }));
            match view {
                Some(view) => UiIntentDecode::Known(AppliedUiIntent::OpenView { view }),
                None => UiIntentDecode::Unsupported,
            }
        }
        "open_overlay" => {
            let Ok(payload) = serde_json::from_value::<OpenOverlayPayload>(applied.payload) else {
                return UiIntentDecode::Unsupported;
            };
            UiIntentDecode::Known(AppliedUiIntent::OpenOverlay {
                overlay_id: payload.overlay_id,
                query: payload.query,
            })
        }
        "close_overlay" => UiIntentDecode::Known(AppliedUiIntent::CloseOverlay),
        "set_overlay_query" => {
            let Ok(payload) = serde_json::from_value::<SetOverlayQueryPayload>(applied.payload)
            else {
                return UiIntentDecode::Unsupported;
            };
            UiIntentDecode::Known(AppliedUiIntent::SetOverlayQuery {
                query: payload.query,
            })
        }
        "focus_input" => UiIntentDecode::Known(AppliedUiIntent::FocusInput),
        "focus_view" => {
            let Ok(payload) = serde_json::from_value::<FocusViewPayload>(applied.payload) else {
                return UiIntentDecode::Unsupported;
            };
            UiIntentDecode::Known(AppliedUiIntent::FocusView {
                tile_id: payload.tile_id,
            })
        }
        _ => UiIntentDecode::NotUiIntent,
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::reducer::test_support::*;

    #[test]
    fn stale_layout_gesture_cannot_resize_a_replacement_tree() {
        let mut core = RyeOsCore::default();
        let view_set = &mut core.view_sets[0];
        view_set
            .add_tile(ViewSpec {
                view_ref: "view:test/one".into(),
            })
            .unwrap();
        view_set
            .add_tile(ViewSpec {
                view_ref: "view:test/two".into(),
            })
            .unwrap();
        let guard = core.layout_guard();
        let resize = |guard| RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ResizeSplit {
                    layout_guard: guard,
                    path: vec![],
                    ratio: 0.7,
                },
            },
        };
        assert!(core.dispatch(resize(guard.clone())).is_empty());
        assert_ne!(core.layout_guard(), guard);
        let current = core.view_sets[0].root.clone();
        core.dispatch(resize(guard));
        assert_eq!(core.view_sets[0].root, current);
        let guard = core.layout_guard();
        core.new_view_set();
        core.dispatch(resize(guard));
        assert!(core.view_sets[1].root.is_none());
        assert_eq!(core.view_sets[0].root, current);
    }

    #[test]
    fn layout_edits_keep_input_focus_on_the_selected_view() {
        use crate::ui::model::{RyeOsDockEdge, RyeOsFocusTarget};
        let mut core = RyeOsCore::default();
        let view_set = &mut core.view_sets[core.active_view_set];
        let first = view_set
            .add_tile(ViewSpec {
                view_ref: "view:test/first".into(),
            })
            .unwrap();
        let second = view_set
            .add_tile(ViewSpec {
                view_ref: "view:test/second".into(),
            })
            .unwrap();
        core.view_sets[core.active_view_set].focus_target = Some(RyeOsFocusTarget::Dock {
            edge: RyeOsDockEdge::Bottom,
        });
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::MoveTileToGroup {
                    layout_guard: core.layout_guard(),
                    tile_id: second.0.to_string(),
                    target_tile_id: first.0.to_string(),
                    index: 1,
                },
            },
        });
        assert!(
            effects.is_empty(),
            "geometry changes must not invoke services"
        );
        assert_eq!(
            core.view_sets[core.active_view_set].focus_target,
            Some(RyeOsFocusTarget::ViewSetTile {
                tile_id: second.0.to_string()
            })
        );
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::CycleViewTab {
                    direction: crate::ui::event::RyeOsStackMoveDirection::Down,
                },
            },
        });
        assert_eq!(core.view_sets[core.active_view_set].focused_tile, first);
        assert_eq!(
            core.view_sets[core.active_view_set].focus_target,
            Some(RyeOsFocusTarget::ViewSetTile {
                tile_id: first.0.to_string()
            })
        );
    }

    fn source_request(
        effect: &crate::ui::effect::RyeOsEffect,
    ) -> Option<(&str, &str, &str, &serde_json::Value)> {
        let RyeOsEffectKind::FetchSource {
            tile_id, request, ..
        } = &effect.kind
        else {
            return None;
        };
        let crate::ui::binding::UiBindingCoordinate::Source { view_ref, channel } =
            &request.coordinate
        else {
            return None;
        };
        let crate::ui::binding::UiBindingPayload::SourceParameters { params } = &request.payload
        else {
            return None;
        };
        Some((
            tile_id.as_str(),
            view_ref.as_str(),
            channel.as_str(),
            params,
        ))
    }

    #[test]
    fn view_overlay_lists_embedded_views_including_scene_widgets() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "rows",
                "description": "Thread list"
            }),
        );
        // Graph/atlas are ordinary embedded views now — no hardcoded items.
        seed_view_value(
            &mut core,
            "view:ryeos/graph/topology",
            serde_json::json!({ "widget": "graph" }),
        );
        let items = view_overlay_items(&core);
        // No declared library: the completeness groups still surface every
        // embedded view, labeled by ref, under a path-derived header.
        assert!(items.iter().any(|item| {
            item.primary == "ryeos/graph/topology"
                && matches!(
                    &item.intent,
                    Some(RyeOsUiIntent::OpenView {
                        view: ViewSpec { view_ref }
                    }) if view_ref == "view:ryeos/graph/topology"
                )
        }));
        assert!(items.iter().any(|item| {
            item.primary == "ryeos/threads/list"
                && matches!(
                    &item.intent,
                    Some(RyeOsUiIntent::OpenView {
                        view: ViewSpec { view_ref }
                    }) if view_ref == "view:ryeos/threads/list"
                )
        }));
        // Each derived group leads with a foldable header row.
        assert!(
            items
                .iter()
                .any(|item| item.header && item.primary == "graph" && item.expanded)
        );
    }

    #[test]
    fn command_overlay_includes_shared_dock_toggles() {
        let core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let items = command_overlay_items_for(&core);

        assert!(items.iter().any(|item| {
            item.label == "Hide bottom slot"
                && matches!(
                    item.intent,
                    RyeOsUiIntent::ToggleDock {
                        edge: crate::ui::model::RyeOsDockEdge::Bottom
                    }
                )
        }));
        assert!(items.iter().any(|item| {
            item.label == "Show left slot"
                && matches!(
                    item.intent,
                    RyeOsUiIntent::ToggleDock {
                        edge: crate::ui::model::RyeOsDockEdge::Left
                    }
                )
        }));
        // No surface-declared top slot → nothing to toggle there.
        assert!(!items.iter().any(|item| matches!(
            item.intent,
            RyeOsUiIntent::ToggleDock {
                edge: crate::ui::model::RyeOsDockEdge::Top
            }
        )));
    }

    #[test]
    fn toggle_dock_updates_view_set_dock_vm() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "rows",
                "sources": { "default": { "ref": "service:ui/ryeos-ui/threads/list", "params": {}, "collection": "rows" } }
            }),
        );
        assert!(build_view_model(&core).view_set.docks.left.is_none());

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ToggleDock {
                    edge: crate::ui::model::RyeOsDockEdge::Left,
                },
            },
        });

        assert!(build_view_model(&core).view_set.docks.left.is_some());
        let source_key = crate::ui::source_key::RyeOsSourceInstanceKey::named(
            crate::ui::model::dock_view_instance_key(
                core.view_sets[core.active_view_set].id,
                crate::ui::model::RyeOsDockEdge::Left,
            ),
            "default",
        )
        .encode();
        assert!(matches!(
            effects.first().and_then(source_request),
            Some((tile_id, "view:ryeos/threads/list", "default", _))
                if tile_id == source_key
        ));
    }

    #[test]
    fn toggling_open_slot_closes_it_and_frees_its_space() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        // The bottom input slot starts open.
        assert!(build_view_model(&core).view_set.docks.bottom.is_some());

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ToggleDock {
                    edge: crate::ui::model::RyeOsDockEdge::Bottom,
                },
            },
        });

        // Closed slots vanish from the dock plane: renderers reserve no
        // space for them. Content and size are retained for reopening.
        assert!(build_view_model(&core).view_set.docks.bottom.is_none());
        let bottom = core.view_sets[core.active_view_set]
            .docks
            .bottom
            .as_ref()
            .expect("slot retained");
        assert!(!bottom.visible);
        assert_eq!(bottom.size, 7);

        // Toggling an absent edge is a no-op (no slot declared).
        assert!(core.view_sets[core.active_view_set].docks.top.is_none());
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ToggleDock {
                    edge: crate::ui::model::RyeOsDockEdge::Top,
                },
            },
        });
        assert!(effects.is_empty());
        assert!(core.view_sets[core.active_view_set].docks.top.is_none());
    }

    #[test]
    fn help_overlay_toggles_through_the_view_model() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        assert!(build_view_model(&core).overlays.is_empty());

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenOverlay {
                overlay_id: "help".to_string(),
            },
        });
        let vm = build_view_model(&core);
        assert_eq!(
            vm.overlays.first().map(|overlay| overlay.id.as_str()),
            Some("help")
        );
        assert!(!vm.overlays[0].items.is_empty());

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CloseOverlay,
        });
        assert!(build_view_model(&core).overlays.is_empty());
    }

    #[test]
    fn status_bar_exposes_principal_and_surface() {
        let core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let envelope = core.envelope(Vec::new());
        let segments = &envelope.view_model.presentation.chrome.status_bar.segments;
        let value = |id: &str| {
            segments
                .iter()
                .find(|segment| segment.id == id)
                .map(|segment| segment.value.as_str())
        };

        assert_eq!(value("principal"), Some("fp:abababab…"));
        assert_eq!(value("surface"), Some("ryeos/ryeos/base"));
    }

    #[test]
    fn route_change_focuses_view_set_view() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:ryeos/items/space");
        let effects = core.dispatch(RyeOsEvent::RouteChanged {
            route: "view:ryeos/items/space".to_string(),
        });

        assert_eq!(
            core.view_sets[core.active_view_set].focused_view(),
            Some(&ViewSpec {
                view_ref: "view:ryeos/items/space".to_string()
            })
        );
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::FetchSource { .. })
        ));
    }

    #[test]
    fn key_context_completion_is_cursor_aware() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
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
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
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

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CompleteInput,
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
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        // An input declaring an @-mention source (projected from threads).
        seed_view_value(
            &mut core,
            "view:ryeos/input",
            serde_json::json!({
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
            }),
        );
        // The refs land under the mention source key via the generic fetch.
        core.data.sources.insert(
            crate::ui::source_key::RyeOsSourceInstanceKey::mention(
                crate::ui::model::dock_view_instance_key(
                    core.view_sets[core.active_view_set].id,
                    crate::ui::model::RyeOsDockEdge::Bottom,
                ),
                "line",
            )
            .encode(),
            serde_json::json!({ "threads": [
                { "thread_id": "T-ab", "item_ref": "directive:ops/base" },
                { "thread_id": "T-cd", "item_ref": "directive:demo/chat" }
            ]}),
        );
        set_focused_input(&mut core, "look @T-a");

        // Cursor sits in an @-mention with a match → Tab can accept it.
        assert!(core.key_context().input_can_accept_completion);

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CompleteInput,
        });
        assert!(effects.is_empty());
        assert_eq!(focused_input_text(&core), "look @T-ab ");
    }

    #[test]
    fn cancel_thread_effect_form_is_rejected() {
        // The `cancel_thread` effect variant is gone: its wire tag no longer
        // deserializes into a RyeOsEffectKind. The one cancel path is
        // `submit_thread_command { command_type: "cancel" }`.
        assert!(
            serde_json::from_value::<RyeOsEffectKind>(serde_json::json!({
                "type": "cancel_thread",
                "thread_id": "T-x"
            }))
            .is_err()
        );
        // And its result tag is likewise gone.
        assert!(
            serde_json::from_value::<crate::ui::effect::RyeOsEffectResultKind>(serde_json::json!(
                "thread_cancelled"
            ))
            .is_err()
        );
    }

    #[test]
    fn interrupt_head_is_noop_when_head_settled() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        set_focused_route_value(&mut core, serde_json::json!({ "thread": "T-done" }));
        core.data.threads = Some(RyeOsThreadsDto {
            threads: vec![serde_json::json!({ "thread_id": "T-done", "status": "completed" })],
        });
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::InterruptHead,
        });
        assert!(effects.is_empty(), "settled head → no interrupt");
    }

    #[test]
    fn duplicate_cancel_is_rejected_while_pending() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/thread-control",
            serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "thread_control": "control" },
                "affordances": [{
                    "id": "control",
                    "producer": "selection",
                    "invoke": {
                        "plane": "rye",
                        "ref": "service:commands/submit",
                        "args": {
                            "thread_id": "{record.thread_id}",
                            "command_type": "{record.command_type}"
                        }
                    }
                }]
            }),
        );
        set_focused_route_value(&mut core, serde_json::json!({ "thread": "T-run" }));
        core.data.threads = Some(RyeOsThreadsDto {
            threads: vec![serde_json::json!({ "thread_id": "T-run", "status": "running" })],
        });
        let first = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::SubmitThreadCommand {
                    command: crate::ui::dto::ThreadControlCommand::Cancel,
                },
            },
        });
        assert_eq!(first.len(), 1);
        assert!(core.has_pending_cancel("T-run"));

        let duplicate = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::InterruptHead,
        });

        assert!(duplicate.is_empty());
        assert!(
            core.ui
                .notices
                .iter()
                .any(|notice| notice.message == "Cancel T-run is already pending.")
        );

        let origin = core.focused_input_instance().unwrap().0.view_instance_key;
        let coordinate = core.thread_control_coordinate().unwrap();
        let other = fixture_attachment(
            "other-attachment",
            9,
            &"99".repeat(32),
            Some("/other"),
            serde_json::json!({"name":"other", "views":{}}),
        );
        let retained =
            crate::ui::binding_context::RetainedUiBindingAttachment::from_descriptor(other)
                .unwrap();
        core.binding_attachments
            .insert("other-attachment".into(), retained);
        assert!(core.stamp_instance_binding(origin.clone(), "other-attachment"));
        assert!(
            !core.has_pending_thread_command(
                &origin,
                "T-run",
                crate::ui::dto::ThreadControlCommand::Cancel,
                &coordinate,
            ),
            "the same thread, command, and coordinate under another attachment is not a duplicate"
        );
    }

    #[test]
    fn inspect_is_a_plain_selection_facet_write() {
        // Inspection is a facet write, peer to input.route — the engine never
        // opens, names, or swaps to the inspector. The center lens is
        // unchanged and no tile is added; the inspector is a facet-bound view
        // (slot or lens) reached by ordinary navigation, live via on_facet.
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        core.view_sets[core.active_view_set].tiling.mode =
            crate::surface::TilingModeSpec::SingleLens;
        seed_view(&mut core, "view:ryeos/items/space");

        // Start on a list lens.
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

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InspectItem {
                    canonical_ref: "tool:ryeos/x".to_string(),
                },
            },
        });

        // The selection facet is set …
        assert_eq!(
            active_selection(&core),
            serde_json::json!({ "item": "tool:ryeos/x" }),
        );
        // … and nothing was opened or swapped: same single tile, same lens.
        assert_eq!(core.view_sets[core.active_view_set].tile_ids().len(), 1);
        assert!(
            matches!(core.view_sets[core.active_view_set].focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:ryeos/items/space"),
            "inspect does not open or swap to the inspector — it only writes the facet"
        );
    }

    #[test]
    fn overlay_state_is_reduced_in_core() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/items/space",
            serde_json::json!({
                "widget": "rows",
                "description": "Item space",
                "sources": { "default": { "ref": "service:ui/ryeos-ui/items/list", "params": {}, "collection": "items" } }
            }),
        );
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenOverlay {
                overlay_id: "views".to_string(),
            },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetOverlayQuery {
                query: "items".to_string(),
            },
        });

        assert_eq!(core.ui.overlay.active.as_deref(), Some("views"));
        assert_eq!(core.ui.overlay.query, "items");
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::ChooseOverlay { secondary: false },
        });

        assert!(core.ui.overlay.active.is_none());
        assert!(matches!(
            core.view_sets[core.active_view_set].focused_view(),
            Some(ViewSpec { view_ref }) if view_ref == "view:ryeos/items/space"
        ));
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::FetchSource { .. })
        ));
    }

    #[test]
    fn transport_outcome_error_is_projected_as_a_visible_notice_once() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        let event = RyeOsEvent::TransportStateChanged {
            channel: crate::ui::event::RyeOsTransportChannel::Session,
            freshness: crate::ui::event::RyeOsTransportFreshness::Reconnecting,
            observed_at_ms: Some(12),
            error: Some(crate::ui::effect::RyeOsUiError::outcome_unknown(
                "seat_append_outcome_unknown",
                "Seat history is not confirmed durable.",
            )),
        };
        core.dispatch(event.clone());
        core.dispatch(event);

        let matching = core
            .notices_vm()
            .into_iter()
            .filter(|notice| notice.message == "Seat history is not confirmed durable.")
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].tone, crate::ui::view_model::RyeOsTone::Warn);
    }

    /// Two derived launcher groups (`alpha`, `beta`) with the views
    /// overlay open — items: [alpha header, one, two, beta header, one].
    fn fold_fixture() -> RyeOsCore {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/alpha/one");
        seed_view(&mut core, "view:test/alpha/two");
        seed_view(&mut core, "view:test/beta/one");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenOverlay {
                overlay_id: "views".to_string(),
            },
        });
        core
    }

    #[test]
    fn exact_overlay_choice_is_not_relative_to_a_stale_rendered_selection() {
        let mut core = fold_fixture();
        let items = crate::ui::view_model::active_overlay_items(&core);
        let hovered = items[1].id.clone();
        let chosen = items[2].id.clone();
        // A pointer can enter one row and click another before the renderer
        // publishes the intermediate envelope. Both events carry exact
        // semantic identities, so the click cannot apply a stale relative move.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetOverlaySelection { item_id: hovered },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::ChooseOverlayAt {
                item_id: chosen,
                secondary: false,
            },
        });

        assert!(core.ui.overlay.active.is_none());
        assert!(matches!(
            core.view_sets[core.active_view_set].focused_view(),
            Some(ViewSpec { view_ref }) if view_ref == "view:test/alpha/two"
        ));
    }

    #[test]
    fn flat_library_entries_shelve_under_their_path_derived_group() {
        // Both library shapes are supported: grouped entries are the
        // canonical form, bare refs (the legacy flat form) shelve under
        // their path-derived group — and merge into a declared group of
        // the same name rather than duplicating the header.
        let session = session_with_surface(serde_json::json!({
            "name": "mixed",
            "library": [
                { "group": "Alpha", "views": ["view:test/alpha/one"] },
                "view:test/alpha/two",
                "view:test/beta/one"
            ]
        }));
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        for view_ref in [
            "view:test/alpha/one",
            "view:test/alpha/two",
            "view:test/beta/one",
        ] {
            seed_view(&mut core, view_ref);
        }
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenOverlay {
                overlay_id: "views".to_string(),
            },
        });
        let items = crate::ui::view_model::active_overlay_items(&core);
        let headers: Vec<&str> = items
            .iter()
            .filter(|item| item.header)
            .map(|item| item.category.as_str())
            .collect();
        assert_eq!(
            headers,
            ["Alpha", "beta"],
            "flat alpha ref merges into declared Alpha; beta derives its own group"
        );
        assert_eq!(items.len(), 5, "two headers + three leaves");
    }

    #[test]
    fn filter_input_views_are_launchable_but_the_bare_input_line_is_not() {
        // The thread history views carry live FILTER inputs and are the
        // canonical center lenses; only the content-less chat line
        // (input, no source, no sections) stays out of the launcher.
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/threads/history",
            serde_json::json!({
                "widget": "table",
                "sources": { "default": { "ref": "service:test/threads", "params": {}, "collection": "threads" } },
                "input": { "id": "q", "feeds": { "param": "filter" } }
            }),
        );
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenOverlay {
                overlay_id: "views".to_string(),
            },
        });
        let items = crate::ui::view_model::active_overlay_items(&core);
        assert!(
            items
                .iter()
                .any(|item| !item.header && item.primary.contains("threads/history")),
            "a sourced view with a filter input must be launchable"
        );
        assert!(
            !items
                .iter()
                .any(|item| item.primary.contains("ryeos/input")),
            "the bare input line must not appear as a lens"
        );
    }

    #[test]
    fn derived_groups_merge_into_declared_groups_case_insensitively() {
        // A surface declaring a "Node" group plus an embedded-but-undeclared
        // view under a `node/` path must render ONE header, not "Node" and
        // "node" side by side.
        let session = session_with_surface(serde_json::json!({
            "name": "grouped",
            "library": [
                { "group": "Node", "views": ["view:test/node/events"] }
            ]
        }));
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/node/events");
        seed_view(&mut core, "view:test/node/status");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenOverlay {
                overlay_id: "views".to_string(),
            },
        });
        let items = crate::ui::view_model::active_overlay_items(&core);
        let headers: Vec<&str> = items
            .iter()
            .filter(|item| item.header)
            .map(|item| item.category.as_str())
            .collect();
        assert_eq!(headers, ["Node"], "one merged header, got {headers:?}");
        assert_eq!(items.len(), 3, "header + declared leaf + appended leaf");
    }

    #[test]
    fn fold_overlay_group_from_leaf_lands_on_its_header() {
        let mut core = fold_fixture();
        let items = crate::ui::view_model::active_overlay_items(&core);
        assert_eq!(items.len(), 5, "two groups with three leaves");
        core.ui.overlay.selected = 2; // view:test/alpha/two
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::FoldOverlayGroup { expand: false },
        });
        assert!(core.ui.overlay.collapsed.contains("alpha"));
        // The fold hid the group's leaves; the selection sits on its header.
        let items = crate::ui::view_model::active_overlay_items(&core);
        assert!(items[core.ui.overlay.selected].header);
        assert_eq!(items[core.ui.overlay.selected].category, "alpha");
    }

    #[test]
    fn fold_overlay_group_expand_reopens_a_collapsed_group() {
        let mut core = fold_fixture();
        core.ui.overlay.selected = 0; // alpha header
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::FoldOverlayGroup { expand: false },
        });
        assert!(core.ui.overlay.collapsed.contains("alpha"));
        // Folding an already-collapsed group is a no-op…
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::FoldOverlayGroup { expand: false },
        });
        assert!(core.ui.overlay.collapsed.contains("alpha"));
        // …and unfolding restores the leaves.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::FoldOverlayGroup { expand: true },
        });
        assert!(core.ui.overlay.collapsed.is_empty());
        assert_eq!(crate::ui::view_model::active_overlay_items(&core).len(), 5);
    }

    #[test]
    fn fold_overlay_group_is_inert_while_a_query_is_live() {
        let mut core = fold_fixture();
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetOverlayQuery {
                query: "alpha".to_string(),
            },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::FoldOverlayGroup { expand: false },
        });
        assert!(
            core.ui.overlay.collapsed.is_empty(),
            "search shows matches under force-expanded, inert headers"
        );
    }

    #[test]
    fn fold_overlay_group_ignores_flat_overlays() {
        let mut core = fold_fixture();
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CloseOverlay,
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenOverlay {
                overlay_id: "help".to_string(),
            },
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::FoldOverlayGroup { expand: false },
        });
        assert!(
            core.ui.overlay.collapsed.is_empty(),
            "flat overlays share the item shape but have no headers"
        );
    }

    #[test]
    fn overlay_query_snaps_selection_to_the_first_match() {
        let mut core = fold_fixture();
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetOverlayQuery {
                query: "beta".to_string(),
            },
        });
        let items = crate::ui::view_model::active_overlay_items(&core);
        let selected = &items[core.ui.overlay.selected];
        assert!(
            selected.enabled && !selected.header,
            "selection lands on the first actionable match, not an inert header"
        );
    }

    #[test]
    fn arrow_focus_uses_view_set_geometry() {
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
        // The first tile fills the view_set.
        let master = core.view_sets[core.active_view_set].focused_tile;
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/threads/list".to_string(),
                    },
                },
            },
        });
        // An ordinary open splits beside it, preserving existing geometry.
        let stacked = core.view_sets[core.active_view_set].focused_tile;
        assert_ne!(master, stacked);

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::FocusDirection {
                direction: FocusDirection::Left,
            },
        });

        assert_eq!(core.view_sets[core.active_view_set].focused_tile, master);
    }

    #[test]
    fn inspect_summary_writes_the_facet_and_a_summary_inspector_renders_it() {
        // The correction a prior prototype missed: writing `selection.summary`
        // is not enough — a view must READ it. The summary-capable inspector is
        // facet-backed (renders `selection.summary` directly, no service round
        // trip) so an inspected error terminal is actually visible.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/item/inspector",
            serde_json::json!({
                "widget": "key_value",
                "facet": "selection.summary",
                "sources": { "default": {
                    "ref": "service:ui/ryeos-ui/item/inspect",
                    "params": { "canonical_ref": "@facet:selection.item" }
                } },
                "projections": { "detail": ["canonical_ref", "title", "detail"] },
                "refresh": { "on_facet": "selection" }
            }),
        );
        // Open the right slot so the inspector renders in the dock plane.
        core.view_sets[core.active_view_set]
            .docks
            .right
            .as_mut()
            .unwrap()
            .visible = true;

        // Enter on a failed feed line → InspectSummary (title = the visible line,
        // detail = the full raw event).
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InspectSummary {
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
        assert_eq!(active_selection(&core)["summary"]["title"], "failed — boom");

        // … and the inspector actually RENDERS it.
        let vm = build_view_model(&core);
        let dock = vm.view_set.docks.right.expect("right dock open");
        let rows = match dock.view {
            crate::ui::view_model::RyeOsViewVm::Rows { rows, .. } => rows,
            other => panic!("expected the key_value inspector to render rows, got {other:?}"),
        };
        assert!(
            rows.iter()
                .any(|row| row.primary.starts_with("title:")
                    && row.primary.contains("failed — boom")),
            "the inspector renders the summary title: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.primary.starts_with("detail:")
                    && row.primary.contains("thread_failed")),
            "the inspector renders the full-event detail: {rows:?}"
        );
    }

    #[test]
    fn prefill_retry_turn_without_binding_is_a_noop_with_notice() {
        let mut unbound = session();
        unbound.binding_attachments[0].binding_digest.clear();
        let mut core = RyeOsCore::new(unbound, BrowserViewport::default(), 0);
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::PrefillRetryTurn {
                    thread_id: "T-failed".to_string(),
                    chain_root_id: "R-1".to_string(),
                    input: "run".to_string(),
                },
            },
        });
        assert!(effects.is_empty());
        assert_eq!(focused_input_text(&core), "", "unbound UI stages nothing");
        assert!(
            core.focused_input_route().thread.is_none(),
            "unbound UI does not retarget the route"
        );
        assert!(
            core.ui
                .notices
                .iter()
                .any(|n| n.message.contains("compiled operation binding"))
        );
    }

    #[test]
    fn exact_view_choice_revalidates_identity_and_intent_after_reorder() {
        let mut browser = writable_session();
        browser.binding_attachments[0].effective_surface = serde_json::json!({
            "name": "exact-pointer",
            "tiles": ["view:test/exact"],
            "views": {
                "view:test/exact": {
                    "widget": "rows",
                    "sources": { "default": {
                        "ref": "service:test/rows",
                        "collection": "rows"
                    } },
                    "projections": { "primary": "id", "expand": { "fields": ["detail"] } },
                    "selection": { "activate": "choose" },
                    "affordances": [{
                        "id": "choose",
                        "invoke": {
                            "plane": "ui",
                            "facet": "active.thread",
                            "value": "{record.id}"
                        }
                    }]
                }
            }
        });
        let mut core = RyeOsCore::new(browser, BrowserViewport::default(), 0);
        let tile_id = core.view_sets[core.active_view_set].focused_tile;
        let instance_key = core.view_sets[core.active_view_set].tiles[&tile_id]
            .instance_key
            .clone();
        let source_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), "default")
                .encode();
        core.data.sources.insert(
            source_key.clone(),
            serde_json::json!({ "rows": [{"id": "a", "detail": "A"}, {"id": "b", "detail": "B"}] }),
        );
        let b_id = match crate::ui::view_model::view_vm_for_instance(&core, &instance_key)
            .expect("mounted rows view")
        {
            crate::ui::view_model::RyeOsViewVm::Rows { rows, .. } => rows[1].id.clone(),
            other => panic!("expected rows, got {other:?}"),
        };

        // The producer changes after this browser frame: B moves from index 1
        // to index 0. The reducer must resolve the semantic id against its
        // current projection and invoke B's current intent atomically.
        core.data.sources.insert(
            source_key.clone(),
            serde_json::json!({ "rows": [{"id": "b", "detail": "B"}, {"id": "a", "detail": "A"}] }),
        );
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::ChooseViewItem {
                instance_key: instance_key.clone(),
                item_id: b_id,
                activate: true,
            },
        });
        let crate::view_set::ViewLocalState::GenericList { cursor, .. } =
            &core.view_sets[core.active_view_set].tiles[&tile_id].local
        else {
            panic!("rows view retains generic-list state");
        };
        assert_eq!(*cursor, 0, "selection follows B, not its stale index");
        assert_eq!(core.seat.fold().get("active.thread").unwrap(), "b");

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::ToggleViewItemExpansion {
                instance_key: instance_key.clone(),
                item_id: format!("view:test/exact#id:b"),
                expand: true,
            },
        });
        let rows = match crate::ui::view_model::view_vm_for_instance(&core, &instance_key)
            .expect("mounted rows view")
        {
            crate::ui::view_model::RyeOsViewVm::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        };
        assert!(rows[0].expanded);
        assert_eq!(rows[0].detail[0].value, "B");

        // A removed stale target is an exact no-op; it cannot fall through to
        // whichever row remains at the old position.
        let a_id = format!("view:test/exact#id:a");
        core.data.sources.insert(
            source_key,
            serde_json::json!({ "rows": [{"id": "b", "detail": "B"}] }),
        );
        let generation = core.generation;
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::ChooseViewItem {
                instance_key,
                item_id: a_id,
                activate: true,
            },
        });
        assert_eq!(core.generation, generation);
        assert_eq!(core.seat.fold().get("active.thread").unwrap(), "b");
    }

    #[test]
    fn exact_dock_disclosure_mutates_the_named_mounted_record() {
        let browser = session_with_surface(serde_json::json!({
            "name": "dock-disclosure",
            "slots": {
                "right": {"content": "view:test/dock", "open": true, "size": 32}
            },
            "views": {
                "view:test/dock": {
                    "widget": "rows",
                    "sources": {"default": {
                        "ref": "service:test/dock-rows",
                        "collection": "rows"
                    }},
                    "projections": {"primary": "id", "expand": {"fields": ["detail"]}}
                }
            }
        }));
        let mut core = RyeOsCore::new(browser, BrowserViewport::default(), 0);
        let instance_key = crate::ids::RyeOsViewInstanceKey::view_set_slot(
            core.view_sets[core.active_view_set].id,
            "right",
        );
        let source_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), "default")
                .encode();
        core.data.sources.insert(
            source_key,
            serde_json::json!({"rows": [{"id": "dock-a", "detail": "exact"}]}),
        );
        let item_id = match crate::ui::view_model::view_vm_for_instance(&core, &instance_key)
            .expect("mounted dock rows")
        {
            crate::ui::view_model::RyeOsViewVm::Rows { rows, .. } => rows[0].id.clone(),
            other => panic!("expected dock rows, got {other:?}"),
        };

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::ToggleViewItemExpansion {
                instance_key: instance_key.clone(),
                item_id,
                expand: true,
            },
        });

        let rows = match crate::ui::view_model::view_vm_for_instance(&core, &instance_key)
            .expect("mounted dock rows")
        {
            crate::ui::view_model::RyeOsViewVm::Rows { rows, .. } => rows,
            other => panic!("expected dock rows, got {other:?}"),
        };
        assert!(rows[0].expanded);
        assert_eq!(rows[0].detail[0].value, "exact");
    }

    #[test]
    fn delayed_choice_from_a_hidden_group_tab_is_rejected() {
        let mut browser = writable_session();
        browser.binding_attachments[0].effective_surface = serde_json::json!({
            "name": "group-pointer",
            "tiles": ["view:test/a", "view:test/b"],
            "views": {
                "view:test/a": { "widget": "text", "body": { "lines": ["A"] } },
                "view:test/b": {
                    "widget": "rows",
                    "sources": { "default": { "ref": "service:test/b", "collection": "rows" } },
                    "projections": { "primary": "id" },
                    "selection": { "activate": "choose" },
                    "affordances": [{ "id": "choose", "invoke": {
                        "plane": "ui", "facet": "active.thread", "value": "{record.id}"
                    }}]
                }
            }
        });
        let mut core = RyeOsCore::new(browser, BrowserViewport::default(), 0);
        let view_set = &core.view_sets[core.active_view_set];
        let a = view_set
            .tiles
            .iter()
            .find(|(_, tile)| tile.view.view_ref == "view:test/a")
            .map(|(id, _)| *id)
            .unwrap();
        let b = view_set
            .tiles
            .iter()
            .find(|(_, tile)| tile.view.view_ref == "view:test/b")
            .map(|(id, _)| *id)
            .unwrap();
        assert!(core.view_sets[core.active_view_set].move_tile_to_group(b, a, 1));
        let b_instance = core.view_sets[core.active_view_set].tiles[&b]
            .instance_key
            .clone();
        let source_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(b_instance.clone(), "default")
                .encode();
        core.data
            .sources
            .insert(source_key, serde_json::json!({ "rows": [{"id": "b"}] }));
        let item_id = match crate::ui::view_model::view_vm_for_instance(&core, &b_instance)
            .expect("active B tab projects")
        {
            crate::ui::view_model::RyeOsViewVm::Rows { rows, .. } => rows[0].id.clone(),
            other => panic!("expected rows, got {other:?}"),
        };

        assert!(
            core.view_sets[core.active_view_set]
                .root
                .as_mut()
                .unwrap()
                .select_tab(a)
        );
        core.view_sets[core.active_view_set].focused_tile = a;
        let generation = core.generation;
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::ChooseViewItem {
                instance_key: b_instance,
                item_id,
                activate: true,
            },
        });
        assert_eq!(core.generation, generation);
        assert!(core.seat.fold().get("active.thread").is_none());
    }

    #[test]
    fn exact_section_toggle_resolves_current_position_after_binding_reorder() {
        let mut browser = writable_session();
        browser.binding_attachments[0].effective_surface = serde_json::json!({
            "name": "section-pointer",
            "tiles": ["view:test/sections"],
            "views": {
                "view:test/sections": {
                    "widget": "sections",
                    "sources": {
                        "a": { "ref": "service:test/a" },
                        "b": { "ref": "service:test/b" }
                    },
                    "sections": [
                        { "title": "A", "source_channel": "a", "collection": "rows", "projection": { "primary": "id" } },
                        { "title": "B", "source_channel": "b", "collection": "rows", "projection": { "primary": "id" } }
                    ]
                }
            }
        });
        let mut core = RyeOsCore::new(browser, BrowserViewport::default(), 0);
        let tile_id = core.view_sets[core.active_view_set].focused_tile;
        let instance_key = core.view_sets[core.active_view_set].tiles[&tile_id]
            .instance_key
            .clone();
        let section_id = match crate::ui::view_model::view_vm_for_instance(&core, &instance_key)
            .expect("sections view projects")
        {
            crate::ui::view_model::RyeOsViewVm::Sections { sections, .. } => sections[0].id.clone(),
            other => panic!("expected sections, got {other:?}"),
        };
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .views
            .get_mut("view:test/sections")
            .unwrap()
            .sections
            .swap(0, 1);

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::ToggleViewSection {
                instance_key,
                section_id,
            },
        });
        let crate::view_set::ViewLocalState::GenericList { collapsed, .. } =
            &core.view_sets[core.active_view_set].tiles[&tile_id].local
        else {
            panic!("sections view retains generic-list state");
        };
        assert_eq!(collapsed.iter().copied().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn dismiss_notice_is_exact_and_idempotent() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        core.notice("first", crate::ui::view_model::RyeOsTone::Neutral);
        core.notice("second", crate::ui::view_model::RyeOsTone::Warn);
        let first_id = core.ui.notices[0].id.clone();
        let second_id = core.ui.notices[1].id.clone();

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::DismissNotice {
                id: first_id.clone(),
            },
        });
        assert_eq!(core.ui.notices.len(), 1);
        assert_eq!(core.ui.notices[0].id, second_id);
        let generation = core.generation;
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::DismissNotice { id: first_id },
        });
        assert_eq!(core.generation, generation, "repeat dismiss is a no-op");
    }
}
