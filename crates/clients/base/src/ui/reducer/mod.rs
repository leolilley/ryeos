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
mod composition;
mod effect_results;
mod field_interaction;
#[cfg(test)]
mod import_tabs_tests;
mod input;
mod presentation_operations;
mod saved_view_sets;
#[cfg(test)]
pub(crate) mod test_support;
mod tiles;
mod transfer_freeze;
mod view_tab;

use super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::event::{RyeOsEvent, RyeOsStackMoveDirection, RyeOsUiEvent, RyeOsUiIntent};
use super::model::RyeOsCore;
use super::view_model::{RyeOsTone, intent_for_focused_row};
pub(crate) use super::{content, dto, effect, event, model, seat, tokenize, view_model};
use crate::view_set::ViewSpec;
use serde::Deserialize;

impl RyeOsCore {
    /// Finalize a private append-only cold prefix once. Staging has already
    /// updated the acknowledged operation index; only derived presentation
    /// and facet/navigation work remains. Publish only effects still pending
    /// at the completed fence: later subject changes may invalidate an effect
    /// emitted earlier in this same finalization.
    pub(crate) fn finalize_cold_seat_replay(&mut self) -> Result<Vec<RyeOsEffect>, String> {
        if !self.pending_effects.is_empty() {
            return Err("cold seat candidate already has pending platform effects".into());
        }
        let first_effect_id = self.next_effect_id;
        let changed_subjects = self.reconcile_acknowledged_composition()?;
        let facets = self.seat.fold().snapshot();
        if let Some(destination) = facets
            .get(super::seat::KEY_NAVIGATION_DESTINATION)
            .and_then(serde_json::Value::as_str)
            && let Some(entry) = self
                .surface_navigation()
                .into_iter()
                .find(|entry| entry.id == destination)
        {
            let _ = self.open_view(ViewSpec::bound(entry.view));
        }
        for key in facets.keys() {
            if key == super::seat::KEY_NAVIGATION_DESTINATION {
                continue;
            }
            if let Some((view_set_id, logical_facet)) =
                super::seat::parse_selection_storage_key(key)
                && self.view_set(view_set_id).is_some()
            {
                let _ = self.effects_for_facet_in_view_set(&logical_facet, view_set_id);
            } else {
                let _ = self.effects_for_facet(key);
            }
        }
        let _ = self.drive_presentation_operations_with_subjects(changed_subjects);
        if self.presentation_operations.replay_blocked {
            return Err("cold seat causal composition replay was refused".into());
        }
        Ok(self
            .pending_effects
            .range((first_effect_id + 1)..)
            .map(|(&id, kind)| RyeOsEffect {
                id,
                kind: kind.clone(),
            })
            .collect())
    }

    /// One startup delivery set after the exact cold fence. Initial sources
    /// may supersede replay-triggered fetches for the same source coordinate;
    /// only the newest still-pending fetch may run. Non-fetch effects retain
    /// their emitted order and identity.
    pub fn initial_effects_after_cold_replay(
        &mut self,
        replay_effects: Vec<RyeOsEffect>,
    ) -> Vec<RyeOsEffect> {
        let initial = self.initial_effects();
        let mut all = replay_effects;
        all.extend(initial);
        let mut latest_fetch = std::collections::BTreeMap::new();
        for effect in all
            .iter()
            .filter(|effect| self.pending_effects.contains_key(&effect.id))
        {
            if let RyeOsEffectKind::FetchSource { tile_id, .. } = &effect.kind {
                latest_fetch.insert(tile_id.clone(), effect.id);
            }
        }
        all.retain(|effect| {
            if !self.pending_effects.contains_key(&effect.id) {
                return false;
            }
            match &effect.kind {
                RyeOsEffectKind::FetchSource { tile_id, .. } => {
                    let keep = latest_fetch.get(tile_id) == Some(&effect.id);
                    if !keep {
                        self.pending_effects.remove(&effect.id);
                    }
                    keep
                }
                _ => true,
            }
        });
        all
    }

    /// Insert one platform paste into the exact live focused input. Pasted
    /// newlines are text, never key commands or implicit submission.
    pub fn insert_focused_input_text(&mut self, text: &str) -> Vec<RyeOsEffect> {
        if self.presentation_operations.replay_blocked || text.is_empty() {
            return Vec::new();
        }
        let live_filter = self.focused_input_is_live_filter();
        let bytes = self
            .focused_input_buffer()
            .map_or(Some(text.len()), |buffer| {
                buffer.text.len().checked_add(text.len())
            });
        if !self.admit_focused_input_length(bytes) {
            return Vec::new();
        }
        let Some(buffer) = self.focused_input_buffer_mut() else {
            return Vec::new();
        };
        if buffer.cursor > buffer.text.len() || !buffer.text.is_char_boundary(buffer.cursor) {
            return Vec::new();
        }
        buffer.text.insert_str(buffer.cursor, text);
        buffer.cursor += text.len();
        self.bump_generation();
        self.feeds_effects_unless_live_filter(live_filter)
    }

    pub fn dispatch(&mut self, event: RyeOsEvent) -> Vec<RyeOsEffect> {
        if self.presentation_operations.replay_blocked {
            return Vec::new();
        }
        self.ui.motion.clear();
        match event {
            RyeOsEvent::Start {
                session,
                presentation_id,
                viewport,
                now_ms,
            } => {
                let supports_independent_documents = self.supports_independent_documents;
                *self = RyeOsCore::new(session, &presentation_id, viewport, now_ms);
                self.supports_independent_documents = supports_independent_documents;
                self.bump_generation();
                self.initial_effects()
            }
            RyeOsEvent::Ui { event } => self.dispatch_ui(event),
            RyeOsEvent::EffectResult { result } => self.apply_effect_result(result),
            RyeOsEvent::PresentationProducerReady {
                producer,
                next_engine_seq,
            } => self.presentation_producer_ready(producer, next_engine_seq),
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
                let Some(tile) = target.and_then(|target| parse_tile_id(&target)) else {
                    return Vec::new();
                };
                let Some(mount) = self
                    .active_view_set()
                    .and_then(|set| set.tiles.get(&tile))
                    .map(|tile| tile.instance_key.clone())
                else {
                    return Vec::new();
                };
                self.prepare_composition_mutation(
                    crate::ui::composition_operation::CompositionMutation::FocusView { mount },
                )
            }
            RyeOsUiEvent::FocusDock { edge } => {
                let Some(key) = self.shared_dock_instance_key(edge) else {
                    return Vec::new();
                };
                let Some(mount) = self
                    .presentation
                    .as_ref()
                    .and_then(|presentation| presentation.mount(&key))
                else {
                    return Vec::new();
                };
                if !mount.visible() {
                    return Vec::new();
                }
                let view_ref = mount.view_ref().to_owned();
                if let Some(local) = self.mounted_local_state_mut(&key)
                    && matches!(local, crate::view_set::ViewLocalState::None)
                {
                    *local = initial_list_local_state();
                }
                self.focused_input = Some(key.clone());
                self.field_query_editing = None;
                self.bump_generation();
                self.emit_fetch_source_for_instance(key, &view_ref)
            }
            RyeOsUiEvent::FocusDirection { direction } => {
                let Some(mut preview) = self.active_view_set().cloned() else {
                    return Vec::new();
                };
                if !preview.focus_in_direction(direction) {
                    return Vec::new();
                }
                let Some(tile) = preview.tiles.get(&preview.focused_tile) else {
                    return Vec::new();
                };
                self.prepare_composition_mutation(
                    crate::ui::composition_operation::CompositionMutation::FocusView {
                        mount: tile.instance_key.clone(),
                    },
                )
            }
            RyeOsUiEvent::OpenLauncher => {
                let Some(id) = self.presentation.as_ref().and_then(|p| {
                    p.dialogs()
                        .get(p.launcher())
                        .map(|mount| mount.view().instance_key().as_str().to_owned())
                }) else {
                    return Vec::new();
                };
                self.dispatch_ui(RyeOsUiEvent::OpenDialog { dialog_id: id })
            }
            RyeOsUiEvent::OpenDialog { dialog_id } => self.open_dialog(&dialog_id),
            RyeOsUiEvent::CloseDialog { instance_key } => self.close_dialog(&instance_key),
            RyeOsUiEvent::FocusInput => {
                if self.focus_default_input() {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::BlurInput => {
                if self.focused_input.take().is_some() {
                    self.field_query_editing = None;
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::InsertInputChar { ch } => {
                let live_filter = self.focused_input_is_live_filter();
                let bytes = self
                    .focused_input_buffer()
                    .map_or(Some(ch.len_utf8()), |buffer| {
                        buffer.text.len().checked_add(ch.len_utf8())
                    });
                if !self.admit_focused_input_length(bytes) {
                    return Vec::new();
                }
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
                if !self.set_focused_input_text_bounded(text, cursor) {
                    return Vec::new();
                }
                self.bump_generation();
                self.effects_for_focused_feeds()
            }
            RyeOsUiEvent::CompleteInput => {
                let Some((key, view_ref)) = self.focused_input_instance() else {
                    return Vec::new();
                };
                let buffer = self
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
                    && self.set_focused_input_text_bounded(text, cursor)
                {
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
            RyeOsUiEvent::SetViewTreeItemCollapsed {
                instance_key,
                item_id,
                collapsed,
            } => {
                let Some((cursor, current, key)) =
                    super::view_model::view_pointer_tree_fold(self, &instance_key, &item_id)
                else {
                    return Vec::new();
                };
                let cursor_changed = self.set_view_cursor(&instance_key, cursor);
                let fold_changed = current != collapsed
                    && self.set_view_tree_row_collapsed_key(&instance_key, key, collapsed);
                if cursor_changed || fold_changed {
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
            RyeOsUiIntent::OpenNewPresentationDocument => self.open_new_presentation_document(),
            RyeOsUiIntent::OpenSurfaceInNewPresentation {
                surface_instance_id,
                binding,
                dialog_id,
                dialog_binding,
            } => self.open_surface_in_new_presentation(
                surface_instance_id,
                binding,
                dialog_id,
                dialog_binding,
            ),
            RyeOsUiIntent::BeginTransfer {
                mode,
                source,
                destination_presentation_id,
            } => self.begin_transfer(mode, source, destination_presentation_id),
            RyeOsUiIntent::PrepareTransfer { transfer, export } => {
                self.prepare_transfer(transfer, export)
            }
            RyeOsUiIntent::ContactTransfer { transfer, action } => {
                self.contact_transfer(transfer, action)
            }
            RyeOsUiIntent::ActivateLocalProjection {
                instance_key,
                record_id,
                projection_guard,
            } => {
                match crate::ui::local_projection::activate(
                    self,
                    &instance_key,
                    &record_id,
                    &projection_guard,
                ) {
                    Some(intent) => self.dispatch_intent(intent),
                    None => Vec::new(),
                }
            }
            RyeOsUiIntent::InvokeAffordance {
                instance_key,
                view_ref,
                affordance_id,
                record,
            } => self.invoke_affordance(&instance_key, &view_ref, &affordance_id, &record),
            RyeOsUiIntent::OpenLauncher => self.dispatch_ui(RyeOsUiEvent::OpenLauncher),
            RyeOsUiIntent::SelectSurface {
                surface_instance_id,
                binding,
            } => self.select_surface_instance(&surface_instance_id, &binding),
            RyeOsUiIntent::RenameSurface {
                surface_instance_id,
                binding,
                title,
            } => {
                if !self
                    .presentation
                    .as_ref()
                    .and_then(|presentation| presentation.surfaces().get(&surface_instance_id))
                    .is_some_and(|surface| surface.binding() == &binding)
                {
                    return Vec::new();
                }
                self.prepare_composition_owned(
                    crate::ui::composition_operation::CompositionMutation::RenameSurface { title },
                    crate::ui::composition_operation::CompositionOwner::Surface {
                        surface_instance_id,
                        binding,
                    },
                )
            }
            RyeOsUiIntent::CloseSurface {
                surface_instance_id,
                binding,
            } => {
                if !self
                    .presentation
                    .as_ref()
                    .and_then(|p| p.surfaces().get(&surface_instance_id))
                    .is_some_and(|surface| surface.binding() == &binding)
                {
                    return Vec::new();
                }
                self.request_surface_close(&surface_instance_id)
            }
            RyeOsUiIntent::OpenDialog { dialog_id } => self.open_dialog(&dialog_id),
            RyeOsUiIntent::CloseDialog { instance_key } => self.close_dialog(&instance_key),
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
            RyeOsUiIntent::OpenViewInNewTab {
                surface_instance_id,
                view_set_id,
                binding,
                view_ref,
            } => self.open_view_in_new_tab(surface_instance_id, view_set_id, binding, view_ref),
            RyeOsUiIntent::OpenCompositionInNewSurface {
                surface_instance_id,
                view_set_id,
                binding,
                view_ref,
                dialog_id,
                dialog_binding,
            } => self.open_composition_in_new_surface(
                surface_instance_id,
                view_set_id,
                binding,
                view_ref,
                dialog_id,
                dialog_binding,
            ),
            RyeOsUiIntent::OpenNewView { view } => {
                if self.active_view_set().is_none() {
                    return Vec::new();
                }

                // Single-lens surfaces have no "another tile": a new-view
                // request collapses to replacing the one center lens.
                if self
                    .active_view_set()
                    .expect("real tab checked")
                    .tiling
                    .mode
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
                if let Some(tile) = self.active_view_set().map(|set| set.focused_tile) {
                    self.close_tile_or_empty(tile);
                }
                Vec::new()
            }
            RyeOsUiIntent::CloseTile { tile_id } => {
                if let Some(tile) = parse_tile_id(&tile_id) {
                    self.close_tile_or_empty(tile);
                }
                Vec::new()
            }
            RyeOsUiIntent::ToggleTileMaximized { tile_id } => {
                let Some(tile) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                let Some(set) = self.active_view_set() else {
                    return Vec::new();
                };
                self.prepare_composition_mutation(
                    crate::ui::composition_operation::CompositionMutation::IsolateView {
                        tab_id: set.id,
                        tile_id: if set.maximized_tile == Some(tile) {
                            None
                        } else {
                            Some(tile)
                        },
                    },
                )
            }
            RyeOsUiIntent::ToggleFocusedMaster => {
                let Some(set) = self.active_view_set() else {
                    return Vec::new();
                };
                self.prepare_composition_mutation(
                    crate::ui::composition_operation::CompositionMutation::Promote {
                        tab_id: set.id,
                        tile_id: set.focused_tile,
                    },
                )
            }
            RyeOsUiIntent::PromoteTileToMaster {
                layout_guard,
                tile_id,
            } => {
                if layout_guard != self.layout_guard() {
                    return Vec::new();
                }
                let Some(tile) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                let Some(set) = self.active_view_set() else {
                    return Vec::new();
                };
                self.prepare_composition_mutation(
                    crate::ui::composition_operation::CompositionMutation::Promote {
                        tab_id: set.id,
                        tile_id: tile,
                    },
                )
            }
            RyeOsUiIntent::MoveFocusedTile { .. } => {
                self.notice(
                    "Stack reorder requires an explicit retained placement.",
                    super::view_model::RyeOsTone::Warn,
                );
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
                let (Some(tile), Some(target)) =
                    (parse_tile_id(&tile_id), parse_tile_id(&target_tile_id))
                else {
                    return Vec::new();
                };
                let Some(surface) = self.active_surface() else {
                    return Vec::new();
                };
                let Some(set) = surface.active_view_set() else {
                    return Vec::new();
                };
                let (axis, before) = match edge {
                    crate::view_set::FocusDirection::Left => {
                        (crate::layout::SplitAxis::Horizontal, true)
                    }
                    crate::view_set::FocusDirection::Right => {
                        (crate::layout::SplitAxis::Horizontal, false)
                    }
                    crate::view_set::FocusDirection::Up => {
                        (crate::layout::SplitAxis::Vertical, true)
                    }
                    crate::view_set::FocusDirection::Down => {
                        (crate::layout::SplitAxis::Vertical, false)
                    }
                };
                self.prepare_composition_mutation(
                    crate::ui::composition_operation::CompositionMutation::MoveView {
                        source: crate::ui::composition_operation::CompositionTileTarget {
                            surface_instance_id: surface.id().clone(),
                            view_set_id: set.id,
                            tile_id: tile,
                        },
                        destination: crate::ui::composition_operation::CompositionTileTarget {
                            surface_instance_id: surface.id().clone(),
                            view_set_id: set.id,
                            tile_id: target,
                        },
                        placement: crate::ui::composition_operation::CompositionPlacement::Split {
                            target,
                            axis,
                            before,
                            ratio: 0.5,
                        },
                    },
                )
            }
            RyeOsUiIntent::CycleTab { direction } => self.cycle_view_set_tab(direction),
            RyeOsUiIntent::CycleViewTab { direction } => {
                let Some(mut preview) = self.active_view_set().cloned() else {
                    return Vec::new();
                };
                if !preview.cycle_view_tab(matches!(direction, RyeOsStackMoveDirection::Down)) {
                    return Vec::new();
                }
                let Some(tile) = preview.tiles.get(&preview.focused_tile) else {
                    return Vec::new();
                };
                self.prepare_composition_mutation(
                    crate::ui::composition_operation::CompositionMutation::FocusView {
                        mount: tile.instance_key.clone(),
                    },
                )
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
                let (Some(tile), Some(target)) =
                    (parse_tile_id(&tile_id), parse_tile_id(&target_tile_id))
                else {
                    return Vec::new();
                };
                let Some(surface) = self.active_surface() else {
                    return Vec::new();
                };
                let Some(set) = surface.active_view_set() else {
                    return Vec::new();
                };
                fn group(
                    node: &crate::layout::LayoutTree,
                    target: crate::ids::TileId,
                ) -> Option<(crate::ids::ViewGroupId, &[crate::ids::TileId])> {
                    match node {
                        crate::layout::LayoutTree::Group { group_id, tabs, .. }
                            if tabs.contains(&target) =>
                        {
                            Some((*group_id, tabs))
                        }
                        crate::layout::LayoutTree::Split { first, second, .. } => {
                            group(first, target).or_else(|| group(second, target))
                        }
                        _ => None,
                    }
                }
                let Some((group_id, tabs)) = set.root.as_ref().and_then(|root| group(root, target))
                else {
                    return Vec::new();
                };
                let remaining = tabs
                    .iter()
                    .copied()
                    .filter(|id| *id != tile)
                    .collect::<Vec<_>>();
                if index > remaining.len() {
                    return Vec::new();
                }
                let before = remaining.get(index).copied();
                self.prepare_composition_mutation(
                    crate::ui::composition_operation::CompositionMutation::MoveView {
                        source: crate::ui::composition_operation::CompositionTileTarget {
                            surface_instance_id: surface.id().clone(),
                            view_set_id: set.id,
                            tile_id: tile,
                        },
                        destination: crate::ui::composition_operation::CompositionTileTarget {
                            surface_instance_id: surface.id().clone(),
                            view_set_id: set.id,
                            tile_id: target,
                        },
                        placement:
                            crate::ui::composition_operation::CompositionPlacement::GroupTab {
                                group_id,
                                before,
                            },
                    },
                )
            }
            RyeOsUiIntent::SwitchTab { index } => self.switch_view_set_tab(index),
            RyeOsUiIntent::NewViewSet => self.new_view_set(),
            RyeOsUiIntent::SelectViewSet { view_set_id } => {
                let index = self
                    .active_surface_view_sets()
                    .and_then(|sets| sets.iter().position(|set| set.id == view_set_id));
                index
                    .map(|index| self.switch_view_set_tab(index))
                    .unwrap_or_default()
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
                let Some(surface) = self.active_surface() else {
                    return Vec::new();
                };
                let Some(source) = surface.active_view_set() else {
                    return Vec::new();
                };
                let Some(destination) = surface
                    .view_sets()
                    .iter()
                    .find(|set| set.id == view_set_id && set.id != source.id)
                else {
                    return Vec::new();
                };
                if !source.tiles.contains_key(&tile) {
                    return Vec::new();
                }
                let (destination_tile, placement) = if let Some(root) = &destination.root {
                    let Some(target) = root.tile_ids().last().copied() else {
                        return Vec::new();
                    };
                    let axis = match destination.tiling.stack.arrange {
                        crate::surface::ArrangeSpec::Horizontal => {
                            crate::layout::SplitAxis::Horizontal
                        }
                        crate::surface::ArrangeSpec::Vertical => crate::layout::SplitAxis::Vertical,
                    };
                    (
                        target,
                        crate::ui::composition_operation::CompositionPlacement::Split {
                            target,
                            axis,
                            before: false,
                            ratio: 0.5,
                        },
                    )
                } else {
                    (
                        tile,
                        crate::ui::composition_operation::CompositionPlacement::First,
                    )
                };
                self.prepare_composition_mutation(
                    crate::ui::composition_operation::CompositionMutation::MoveView {
                        source: crate::ui::composition_operation::CompositionTileTarget {
                            surface_instance_id: surface.id().clone(),
                            view_set_id: source.id,
                            tile_id: tile,
                        },
                        destination: crate::ui::composition_operation::CompositionTileTarget {
                            surface_instance_id: surface.id().clone(),
                            view_set_id,
                            tile_id: destination_tile,
                        },
                        placement,
                    },
                )
            }
            RyeOsUiIntent::MoveTileToSurfaceViewSet {
                layout_guard,
                tile_id,
                surface_instance_id,
                view_set_id,
            } => {
                if layout_guard != self.layout_guard() {
                    return Vec::new();
                }
                let Some(tile) = parse_tile_id(&tile_id) else {
                    return Vec::new();
                };
                let Some(presentation) = self.presentation.as_ref() else {
                    return Vec::new();
                };
                let Some(surface) = presentation.active_surface() else {
                    return Vec::new();
                };
                let Some(source) = surface.active_view_set() else {
                    return Vec::new();
                };
                if surface.id() == &surface_instance_id || !source.tiles.contains_key(&tile) {
                    return Vec::new();
                }
                let Some(destination_surface) = presentation.surfaces().get(&surface_instance_id)
                else {
                    return Vec::new();
                };
                let Some(destination) = destination_surface
                    .view_sets()
                    .iter()
                    .find(|set| set.id == view_set_id)
                else {
                    return Vec::new();
                };
                let (destination_tile, placement) = if let Some(root) = &destination.root {
                    let Some(target) = root.tile_ids().last().copied() else {
                        return Vec::new();
                    };
                    let axis = match destination.tiling.stack.arrange {
                        crate::surface::ArrangeSpec::Horizontal => {
                            crate::layout::SplitAxis::Horizontal
                        }
                        crate::surface::ArrangeSpec::Vertical => crate::layout::SplitAxis::Vertical,
                    };
                    (
                        target,
                        crate::ui::composition_operation::CompositionPlacement::Split {
                            target,
                            axis,
                            before: false,
                            ratio: 0.5,
                        },
                    )
                } else {
                    (
                        tile,
                        crate::ui::composition_operation::CompositionPlacement::First,
                    )
                };
                let mutation =
                    crate::ui::composition_operation::CompositionMutation::MoveViewAcrossSurfaces {
                        source: crate::ui::composition_operation::CompositionTileTarget {
                            surface_instance_id: surface.id().clone(),
                            view_set_id: source.id,
                            tile_id: tile,
                        },
                        destination: crate::ui::composition_operation::CompositionTileTarget {
                            surface_instance_id,
                            view_set_id,
                            tile_id: destination_tile,
                        },
                        destination_binding: destination_surface.binding().clone(),
                        placement,
                    };
                self.prepare_composition_mutation(mutation)
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
                if layout_guard != self.layout_guard() {
                    return Vec::new();
                }
                let Some(set) = self.active_view_set() else {
                    return Vec::new();
                };
                self.prepare_composition_mutation(
                    crate::ui::composition_operation::CompositionMutation::SplitRatio {
                        tab_id: set.id,
                        path,
                        expected_layout_guard: layout_guard,
                        ratio,
                    },
                )
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
                let Some(key) = self.shared_dock_instance_key(edge) else {
                    return Vec::new();
                };
                let edge_name = match edge {
                    super::model::RyeOsDockEdge::Top => "top",
                    super::model::RyeOsDockEdge::Left => "left",
                    super::model::RyeOsDockEdge::Right => "right",
                    super::model::RyeOsDockEdge::Bottom => "bottom",
                };
                let Some(dock) = self
                    .presentation
                    .as_mut()
                    .and_then(|presentation| presentation.dock_mut(edge_name))
                else {
                    return Vec::new();
                };
                dock.visible = !dock.visible;
                let shown = dock.visible.then(|| dock.view().view_ref().to_string());
                if shown.is_none() {
                    if self.focused_input.as_ref() == Some(&key) {
                        self.focused_input = None;
                    }
                    if self.field_query_editing.as_ref() == Some(&key) {
                        self.field_query_editing = None;
                    }
                }
                self.normalize_mounted_local_states();
                self.bump_generation();
                shown
                    .map(|view| self.emit_fetch_source_for_instance(key, &view))
                    .unwrap_or_default()
            }
            RyeOsUiIntent::ResizeFocused { direction } => {
                if self.resize_focused_dock(direction) {
                    self.bump_generation();
                } else {
                    self.notice(
                        "Resize the exact split handle to retain a bounded composition edit.",
                        super::view_model::RyeOsTone::Warn,
                    );
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
                if self.active_view_set().is_some_and(|set| {
                    set.tiling.mode == crate::surface::TilingModeSpec::SingleLens
                }) {
                    let Some(origin) = self.focused_view_instance_key() else {
                        return Vec::new();
                    };
                    let mut route = serde_json::to_value(self.route_for_instance(&origin))
                        .expect("InputRoute serializes");
                    let Some(fields) = route.as_object_mut() else {
                        return Vec::new();
                    };
                    fields.insert("thread".into(), serde_json::json!(thread_id));
                    fields.insert("chain_root".into(), serde_json::json!(chain_root_id));
                    return self.prepare_lens_route_drill(&origin, None, Some(route), label);
                }
                self.apply_ui_affordance(
                    super::seat::KEY_INPUT_ROUTE.to_string(),
                    None,
                    Some(serde_json::json!({ "thread": thread_id, "chain_root": chain_root_id })),
                    None,
                    true,
                )
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
                if !self.admit_focused_input_length(Some(input.len())) {
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
                let cursor = input.len();
                self.set_focused_input_text_bounded(input, cursor);
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
                    super::seat::selection_facet_key(match self.active_view_set() {
                        Some(set) => set.id,
                        None => return Vec::new(),
                    }),
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

    fn open_new_presentation_document(&mut self) -> Vec<RyeOsEffect> {
        if !self.supports_independent_documents {
            self.notice(
                "This host does not support independent presentation documents.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let Some(session) = self.data.session.as_ref() else {
            return Vec::new();
        };
        let Some(presentation) = self.presentation.as_ref() else {
            return Vec::new();
        };
        let Some(producer) = self.presentation_operations.producer.as_ref() else {
            return Vec::new();
        };
        if session.session_id.is_empty()
            || self.presentation_operations.replay_blocked
            || !self.producer_matches_membership(producer)
        {
            return Vec::new();
        }
        let root_binding = presentation.root_binding();
        if !self
            .binding_attachment(&root_binding.binding_attachment_id)
            .is_some_and(|attachment| {
                attachment.binding_generation == root_binding.binding_generation
                    && attachment.binding_digest == root_binding.binding_digest
            })
        {
            return Vec::new();
        }
        let expected_session_id = session.session_id.clone();
        let root_binding = root_binding.clone();
        vec![self.emit(RyeOsEffectKind::OpenNewPresentationDocument {
            expected_session_id,
            root_binding,
        })]
    }

    /// Fold a retained transport batch without losing its authoritative event
    /// coordinates. The ordinary reducer still decides semantic acceptance;
    /// testimony is installed only on a private candidate after that fold.
    pub fn replay_authenticated_seat_events(
        &mut self,
        events: Vec<super::seat::RetainedSeatEvent>,
    ) -> Result<Vec<RyeOsEffect>, String> {
        let (thread_id, chain_root_id) = self.exact_retained_seat_coordinate()?;
        let mut candidate = self.clone();
        let effects = candidate.replay_seat_events(events.iter().map(|row| row.event.clone()));
        if candidate.presentation_operations.replay_blocked {
            *self = candidate;
            return Err("authenticated seat replay refused its semantic prefix".into());
        }
        let acknowledged =
            &candidate.seat.events()[..candidate.presentation_operations.acknowledged_prefix_len];
        if events.iter().any(|row| {
            acknowledged
                .binary_search_by_key(&row.event.seq, |event| event.seq)
                .ok()
                .and_then(|index| acknowledged.get(index))
                != Some(&row.event)
        }) {
            // The ordinary reducer may notice a non-prefix event without
            // accepting it. Never attach signed testimony to that provisional
            // local tail or return its effects as an admitted replay.
            self.presentation_operations.replay_blocked = true;
            self.presentation_operations.producer = None;
            self.presentation_operations.acknowledged_engine_frontier = None;
            return Err("authenticated seat replay was not an exact acknowledged prefix".into());
        }
        for retained in events {
            if let Err(error) = candidate
                .seat
                .append_retained(&thread_id, &chain_root_id, retained)
            {
                self.presentation_operations.replay_blocked = true;
                self.presentation_operations.producer = None;
                self.presentation_operations.acknowledged_engine_frontier = None;
                return Err(format!("authenticated seat evidence refused: {error}"));
            }
        }
        let mut effects = effects;
        effects.extend(candidate.drive_presentation_operations());
        *self = candidate;
        Ok(effects)
    }

    /// Fold durable seat history and restore only state named by the current
    /// signed surface. Unknown or stale navigation destinations remain inert.
    pub fn replay_seat_events(
        &mut self,
        events: impl IntoIterator<Item = super::seat::SeatEvent>,
    ) -> Vec<RyeOsEffect> {
        let events = events.into_iter().collect::<Vec<_>>();
        let acknowledged =
            &self.seat.events()[..self.presentation_operations.acknowledged_prefix_len];
        if (self.presentation_operations.producer.is_some()
            || self
                .acknowledged_composition_cache
                .as_ref()
                .is_some_and(|(_, replay)| replay.requires_paired_reconstruction()))
            && events.iter().any(|event| {
                acknowledged
                    .binary_search_by_key(&event.seq, |retained| retained.seq)
                    .ok()
                    .and_then(|index| acknowledged.get(index))
                    != Some(event)
            })
        {
            // The authenticated engine frontier belongs to the exact local ACK
            // prefix installed at bootstrap. New history cannot move that
            // prefix independently or reinterpret a provisional reservation.
            // A paired observer also needs a fresh complete cut: rebuilding
            // its ordinary log would omit the non-SeatEvent transfer lane.
            self.presentation_operations.replay_blocked = true;
            self.presentation_operations.producer = None;
            self.presentation_operations.acknowledged_engine_frontier = None;
            self.authenticated_transfer_proofs.clear();
            self.authenticated_cancellation_proofs.clear();
            self.authenticated_reproof_tip = None;
            self.contacted_transfer_actions.clear();
            self.refused_transfer_contacts.clear();
            self.notice(
                "New retained seat history requires authenticated presentation rebootstrap.",
                super::view_model::RyeOsTone::Danger,
            );
            return Vec::new();
        }
        if let Err(error) = self.validate_replayed_initialization(&events) {
            self.presentation_operations.replay_blocked = true;
            self.presentation_operations.producer = None;
            self.notice(
                format!("Presentation initialization replay refused: {error}"),
                super::view_model::RyeOsTone::Danger,
            );
            return Vec::new();
        }
        let before = self.seat.fold().snapshot();
        for event in events {
            if self
                .seat
                .event_at_sequence(event.seq)
                .is_some_and(|previous| {
                    previous != &event
                        && matches!(
                            &previous.kind,
                            super::seat::SeatEventKind::PresentationOperationPrepared(_)
                                | super::seat::SeatEventKind::PresentationOperationCompleted(_)
                                | super::seat::SeatEventKind::PresentationInitialized(_)
                                | super::seat::SeatEventKind::SurfaceClosed(_)
                        )
                })
            {
                self.presentation_operations.replay_blocked = true;
                self.presentation_operations.producer = None;
                self.notice("An acknowledged structural record changed during replay; reload the authenticated presentation.", super::view_model::RyeOsTone::Danger);
                return Vec::new();
            }
            let changed_operation_ids = self
                .seat
                .event_at_sequence(event.seq)
                .filter(|previous| *previous != &event)
                .map(|previous| {
                    [previous, &event]
                        .into_iter()
                        .filter_map(|entry| match &entry.kind {
                            super::seat::SeatEventKind::PresentationOperationPrepared(
                                operation,
                            ) => Some(operation.operation_id.clone()),
                            super::seat::SeatEventKind::PresentationOperationCompleted(
                                completion,
                            ) => Some(completion.operation_id.clone()),
                            super::seat::SeatEventKind::Facet { .. }
                            | super::seat::SeatEventKind::PresentationInitialized(_)
                            | super::seat::SeatEventKind::SurfaceClosed(_) => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for id in changed_operation_ids {
                // Earlier authoritative replacement cannot lend an old
                // success/refusal marker to different content under one ID.
                self.presentation_operations.applied.remove(&id);
                self.presentation_operations.refused.remove(&id);
                self.presentation_operations.dispatched.remove(&id);
                self.presentation_operations
                    .replay_conflicts
                    .insert(id.clone());
                self.presentation_operations.invalid.insert(id);
            }
            let outcome = self.seat.append_replayed(event);
            let prefix = self.presentation_operations.acknowledged_prefix_len;
            match outcome {
                super::seat::SeatReplayOutcome::Identical { index } if index < prefix => {}
                super::seat::SeatReplayOutcome::Inserted { index } if index < prefix => {
                    self.acknowledged_composition_cache = None;
                    self.presentation_operations.acknowledged_prefix_len += 1;
                    self.presentation_operations
                        .rebuild_index(&self.seat.events()[..prefix + 1]);
                }
                super::seat::SeatReplayOutcome::Replaced { index } if index < prefix => {
                    self.acknowledged_composition_cache = None;
                    self.presentation_operations
                        .rebuild_index(&self.seat.events()[..prefix]);
                }
                super::seat::SeatReplayOutcome::Identical { index }
                | super::seat::SeatReplayOutcome::Inserted { index }
                | super::seat::SeatReplayOutcome::Replaced { index }
                    if index == prefix =>
                {
                    let accepted = self.seat.events()[index].clone();
                    if self.acknowledged_composition_cache.is_some()
                        && let Err(reason) = self.advance_acknowledged_composition_replay(
                            std::slice::from_ref(&accepted),
                        )
                    {
                        self.presentation_operations.replay_blocked = true;
                        self.presentation_operations.producer = None;
                        self.notice(
                            format!("Causal composition replay refused: {reason}"),
                            super::view_model::RyeOsTone::Danger,
                        );
                        return Vec::new();
                    }
                    self.presentation_operations
                        .fold_acknowledged(&self.seat.events()[index]);
                    self.presentation_operations.acknowledged_prefix_len += 1;
                }
                _ => {
                    // A durable page cannot certify earlier provisional local
                    // writes. Bootstrap must replay before creating mutations.
                    self.notice("Seat replay is not an exact authoritative prefix; presentation admission remains blocked.", super::view_model::RyeOsTone::Danger);
                }
            }
        }
        let changed_subjects = match self.reconcile_acknowledged_composition() {
            Ok(changed) => changed,
            Err(reason) => {
                self.presentation_operations.replay_blocked = true;
                self.presentation_operations.producer = None;
                self.notice(
                    format!("Causal composition replay refused: {reason}"),
                    super::view_model::RyeOsTone::Danger,
                );
                return Vec::new();
            }
        };
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
                    && self.view_set(view_set_id).is_some()
                {
                    effects.extend(self.effects_for_facet_in_view_set(&logical_facet, view_set_id));
                } else {
                    effects.extend(self.effects_for_facet(&key));
                }
            }
        }
        effects.extend(self.drive_presentation_operations_with_subjects(changed_subjects));
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
        if self.move_freeze_restricts_instance(instance) {
            return Some("This view is frozen while its MOVE transfer is reconciled.");
        }
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
        let Some(instance) = self.focused_view_instance_key() else {
            return false;
        };
        let edge = [
            super::model::RyeOsDockEdge::Top,
            super::model::RyeOsDockEdge::Left,
            super::model::RyeOsDockEdge::Right,
            super::model::RyeOsDockEdge::Bottom,
        ]
        .into_iter()
        .find(|edge| self.shared_dock_instance_key(*edge).as_ref() == Some(&instance));
        let Some(edge) = edge else {
            return false;
        };
        let edge_name = match edge {
            super::model::RyeOsDockEdge::Top => "top",
            super::model::RyeOsDockEdge::Left => "left",
            super::model::RyeOsDockEdge::Right => "right",
            super::model::RyeOsDockEdge::Bottom => "bottom",
        };
        let Some(slot) = self
            .presentation
            .as_mut()
            .and_then(|presentation| presentation.dock_mut(edge_name))
            .filter(|dock| dock.visible)
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
    OpenView { view: ViewSpec },
    FocusInput,
    FocusView { tile_id: String },
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
    use crate::ui::reducer::test_support::core as test_support_core;
    use crate::ui::reducer::test_support::*;

    fn seed_input_view(core: &mut RyeOsCore) {
        crate::ui::reducer::test_support::seed_input_view(core);
        core.focused_input = core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom);
        assert!(core.focused_input_instance().is_some());
    }

    #[test]
    fn stale_layout_gesture_cannot_resize_a_replacement_tree() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"Geometry","tiles":["view:test/one","view:test/two"],
            "views":{"view:test/one":{"widget":"rows"},"view:test/two":{"widget":"rows"}}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
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
        assert_eq!(core.layout_guard(), guard, "resize requires exact ACK");
        settle_pending_prefix(&mut core, &producer);
        assert_ne!(core.layout_guard(), guard);
        let current = core.active_surface_view_sets().unwrap()[0].root.clone();
        core.dispatch(resize(guard));
        assert_eq!(core.active_surface_view_sets().unwrap()[0].root, current);
        let guard = core.layout_guard();
        core.new_view_set();
        settle_pending_prefix(&mut core, &producer);
        core.dispatch(resize(guard));
        assert!(core.active_surface_view_sets().unwrap()[1].root.is_none());
        assert_eq!(core.active_surface_view_sets().unwrap()[0].root, current);
    }

    #[test]
    fn layout_edits_keep_input_focus_on_the_selected_view() {
        use crate::ui::model::RyeOsFocusTarget;
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"Geometry","tiles":["view:test/first","view:test/second"],
            "views":{"view:test/first":{"widget":"rows"},"view:test/second":{"widget":"rows"}}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let first = core
            .active_view_set()
            .unwrap()
            .tiles
            .iter()
            .find(|(_, tile)| tile.view.view_ref == "view:test/first")
            .unwrap()
            .0
            .to_owned();
        let second = core
            .active_view_set()
            .unwrap()
            .tiles
            .iter()
            .find(|(_, tile)| tile.view.view_ref == "view:test/second")
            .unwrap()
            .0
            .to_owned();
        let bottom = core
            .shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom)
            .unwrap();
        core.focused_input = Some(bottom);
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
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(
            core.active_view_set().unwrap().focus_target,
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
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.active_view_set().unwrap().focused_tile, first);
        assert_eq!(
            core.active_view_set().unwrap().focus_target,
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
    fn command_overlay_includes_shared_dock_toggles() {
        let core = test_support_core(writable_session());
        let items = available_command_items(&core);

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
        let mut core = test_support_core(writable_session());
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "rows",
                "sources": { "default": { "ref": "service:ui/ryeos-ui/threads/list", "params": {}, "collection": "rows" } }
            }),
        );
        assert!(build_view_model(&core).docks.left.is_none());

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ToggleDock {
                    edge: crate::ui::model::RyeOsDockEdge::Left,
                },
            },
        });

        assert!(build_view_model(&core).docks.left.is_some());
        let source_key = crate::ui::source_key::RyeOsSourceInstanceKey::named(
            core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Left)
                .unwrap(),
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
        let mut core = test_support_core(writable_session());
        // The bottom input slot starts open.
        assert!(build_view_model(&core).docks.bottom.is_some());

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ToggleDock {
                    edge: crate::ui::model::RyeOsDockEdge::Bottom,
                },
            },
        });

        // Closed slots vanish from the dock plane: renderers reserve no
        // space for them. Content and size are retained for reopening.
        assert!(build_view_model(&core).docks.bottom.is_none());
        let bottom = core
            .presentation
            .as_ref()
            .unwrap()
            .docks()
            .get("bottom")
            .expect("slot retained");
        assert!(!bottom.visible);
        assert_eq!(bottom.size, 7);

        // Toggling an absent edge is a no-op (no slot declared).
        assert!(
            !core
                .presentation
                .as_ref()
                .unwrap()
                .docks()
                .contains_key("top")
        );
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::ToggleDock {
                    edge: crate::ui::model::RyeOsDockEdge::Top,
                },
            },
        });
        assert!(effects.is_empty());
        assert!(
            !core
                .presentation
                .as_ref()
                .unwrap()
                .docks()
                .contains_key("top")
        );
    }

    #[test]
    fn status_bar_keeps_interaction_hints_without_duplicate_identity() {
        let core = test_support_core(session());
        let envelope = core.envelope(Vec::new());
        let segments = &envelope.view_model.presentation.chrome.status_bar.segments;
        let value = |id: &str| {
            segments
                .iter()
                .find(|segment| segment.id == id)
                .map(|segment| segment.value.as_str())
        };

        assert_eq!(value("principal"), None);
        assert_eq!(value("surface"), None);
        assert!(
            envelope
                .view_model
                .presentation
                .chrome
                .status_bar
                .key_hint
                .contains("ctrl+k")
        );
    }

    #[test]
    fn route_change_focuses_view_set_view() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"route-test", "tiles":["view:test/home"],
            "views":{
                "view:test/home":{"widget":"text"},
                "view:ryeos/items/space":{"widget":"rows","sources":{"default":{"ref":"service:test/items"}}}
            }
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let effects = core.dispatch(RyeOsEvent::RouteChanged {
            route: "view:ryeos/items/space".to_string(),
        });
        assert!(
            effects.is_empty(),
            "route placement waits for acknowledgement"
        );
        let effects = settle_pending_prefix(&mut core, &producer);

        assert_eq!(
            core.active_view_set().unwrap().focused_view(),
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
        let mut core = test_support_core(writable_session());
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
        let mut core = test_support_core(writable_session());
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
    fn completion_rejects_over_budget_candidate_without_clipping_draft() {
        let mut core = test_support_core(writable_session());
        seed_input_view(&mut core);
        seed_commands(
            &mut core,
            serde_json::json!({"commands":[
                {"invocable":true,"tokens":["thread","list"],"description":"List threads"}
            ]}),
        );
        set_focused_input(&mut core, "/thr");
        let (key, _) = core.focused_input_instance().unwrap();
        let id = core
            .binding_attachment_for_instance(&key.view_instance_key)
            .unwrap()
            .binding_attachment_id
            .clone();
        core.binding_attachments
            .get_mut(&id)
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_input_bytes = 4;
        let before = core.focused_input_buffer().unwrap().clone();
        assert!(core.key_context().input_can_accept_completion);
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CompleteInput,
        });
        assert!(effects.is_empty());
        assert_eq!(core.focused_input_buffer(), Some(&before));
    }

    #[test]
    fn complete_input_accepts_top_mention() {
        let mut core = test_support_core(writable_session());
        seed_input_view(&mut core);
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
                core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom)
                    .unwrap(),
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
        let mut core = test_support_core(writable_session());
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
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"thread-control", "tiles":["view:test/thread-control"],
            "views":{"view:test/thread-control":{
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
            }}
        })));
        let tile = core.active_view_set().unwrap().focused_tile;
        focus_tile(&mut core, tile);
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
        let view = core.binding_attachments["fixture-attachment"]
            .descriptor
            .presentation
            .views["view:test/thread-control"]
            .clone();
        let other = fixture_attachment(
            "other-attachment",
            9,
            &"99".repeat(32),
            Some("/other"),
            serde_json::json!({"name":"other", "views":{"view:test/thread-control":view}}),
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
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"inspect", "tiling":{"mode":"single_lens"},
            "tiles":["view:ryeos/items/space"],
            "views":{"view:ryeos/items/space":{"widget":"rows"}}
        })));

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
        assert_eq!(core.active_view_set().unwrap().tile_ids().len(), 1);

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
        assert_eq!(core.active_view_set().unwrap().tile_ids().len(), 1);
        assert!(
            matches!(core.active_view_set().unwrap().focused_view(), Some(ViewSpec { view_ref }) if view_ref == "view:ryeos/items/space"),
            "inspect does not open or swap to the inspector — it only writes the facet"
        );
    }

    #[test]
    fn transport_outcome_error_is_projected_as_a_visible_notice_once() {
        let mut core = test_support_core(session());
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

    #[test]
    fn authored_library_groups_remain_available_to_an_ordinary_launcher_projection() {
        let core = core(session_with_surface(serde_json::json!({
            "name":"mixed","library":[{"group":"Alpha","views":["view:test/alpha/one","view:test/alpha/two"]},{"group":"beta","views":["view:test/beta/one"]}],
            "views":{"view:test/alpha/one":{"widget":"rows"},"view:test/alpha/two":{"widget":"rows"},"view:test/beta/one":{"widget":"rows"}}
        })));
        let groups = core.library_groups();
        assert_eq!(
            groups
                .iter()
                .map(|group| group.title.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "beta"]
        );
        assert_eq!(groups[0].refs.len(), 2);
    }

    #[test]
    fn launcher_projection_qualifies_filter_views_without_including_bare_input_lines() {
        let core = test_support_core(session_with_surface(serde_json::json!({
            "name":"Library qualification",
            "library":[{"group":"Declared", "views":["view:test/filter","view:test/input"]}],
            "views": {
                "view:test/filter": {"widget":"rows", "input":{"id":"query","feeds":{"param":"query"}},
                    "sources":{"default":{"ref":"service:test/rows","collection":"rows"}}},
                "view:test/input": {"widget":"text", "input":{"id":"line","submit":"route"}},
                "view:test/derived-filter": {"widget":"rows", "input":{"id":"query","feeds":{"param":"query"}},
                    "sources":{"default":{"ref":"service:test/rows","collection":"rows"}}},
                "view:test/derived-input": {"widget":"text", "input":{"id":"line","submit":"route"}}
            }
        })));
        let groups = core.library_groups();
        let refs: Vec<_> = groups
            .iter()
            .flat_map(|group| group.refs.iter().map(String::as_str))
            .collect();
        assert!(refs.contains(&"view:test/filter"));
        assert!(refs.contains(&"view:test/derived-filter"));
        assert!(!refs.contains(&"view:test/input"));
        assert!(!refs.contains(&"view:test/derived-input"));
        assert_eq!(groups[0].title, "Declared");
        assert_eq!(groups[0].refs, vec!["view:test/filter"]);
    }

    #[test]
    fn derived_groups_merge_into_declared_groups_case_insensitively() {
        let mut core = core(session_with_surface(serde_json::json!({
            "name":"grouped","library":[{"group":"Node","views":["view:test/node/events"]}]
        })));
        seed_view(&mut core, "view:test/node/events");
        seed_view(&mut core, "view:test/node/status");
        let groups = core.library_groups();
        assert_eq!(
            groups
                .iter()
                .map(|group| group.title.as_str())
                .collect::<Vec<_>>(),
            ["Node"]
        );
        assert_eq!(groups[0].refs.len(), 2);
    }

    #[test]
    fn arrow_focus_uses_view_set_geometry() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"geometry", "tiles":["view:test/services"],
            "views":{
                "view:test/services":{"widget":"rows"},
                "view:ryeos/threads/list":{"widget":"rows"}
            }
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:test/services".to_string(),
                    },
                },
            },
        });
        settle_pending_prefix(&mut core, &producer);
        // The first tile fills the view_set.
        let master = core.active_view_set().unwrap().focused_tile;
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenNewView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/threads/list".to_string(),
                    },
                },
            },
        });
        settle_pending_prefix(&mut core, &producer);
        // An ordinary open splits beside it, preserving existing geometry.
        let stacked = core.active_view_set().unwrap().focused_tile;
        assert_ne!(master, stacked);

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::FocusDirection {
                direction: FocusDirection::Left,
            },
        });
        settle_pending_prefix(&mut core, &producer);

        assert_eq!(core.active_view_set().unwrap().focused_tile, master);
    }

    #[test]
    fn inspect_summary_writes_the_facet_and_a_summary_inspector_renders_it() {
        // The correction a prior prototype missed: writing `selection.summary`
        // is not enough — a view must READ it. The summary-capable inspector is
        // facet-backed (renders `selection.summary` directly, no service round
        // trip) so an inspected error terminal is actually visible.
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"summary-inspector", "tiles":["view:ryeos/item/inspector"],
            "views":{"view:ryeos/item/inspector":{
                "widget": "key_value",
                "facet": "selection.summary",
                "sources": { "default": {
                    "ref": "service:ui/ryeos-ui/item/inspect",
                    "params": { "canonical_ref": "@facet:selection.item" }
                } },
                "projections": { "detail": ["canonical_ref", "title", "detail"] },
                "refresh": { "on_facet": "selection" }
            }}
        })));
        let tile = core.active_view_set().unwrap().focused_tile;
        focus_tile(&mut core, tile);
        let instance = core.focused_view_instance_key().unwrap();

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
        // A shared dock does not inherit this Surface's selection. Project
        // the explicitly Surface-owned inspector instead.
        let view = crate::ui::view_model::view_vm_for_instance(&core, &instance).unwrap();
        let rows = match view {
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
    fn prefill_retry_turn_without_binding_cannot_resolve_or_mutate_input() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"retry", "tiles":["view:test/retry"],
            "views":{"view:test/retry":{"widget":"text","input":{"id":"line","submit":"route"}}}
        })));
        let tile = core.active_view_set().unwrap().focused_tile;
        focus_tile(&mut core, tile);
        assert!(core.focused_input_instance().is_some());
        let before = core.input_buffers.clone();
        let history = core.seat.events().to_vec();
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .binding_digest
            .clear();
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
        assert!(core.focused_input_instance().is_none());
        assert_eq!(core.input_buffers, before);
        assert_eq!(core.seat.events(), history.as_slice());
    }

    #[test]
    fn exact_view_choice_revalidates_identity_and_intent_after_reorder() {
        let browser = session_with_surface(serde_json::json!({
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
        }));
        let mut core = test_support_core(browser);
        let tile_id = core.active_view_set().unwrap().focused_tile;
        let instance_key = core.active_view_set().unwrap().tiles[&tile_id]
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
            &core.active_view_set().unwrap().tiles[&tile_id].local
        else {
            panic!("rows view retains generic-list state");
        };
        assert_eq!(*cursor, 0, "selection follows B, not its stale index");
        assert_eq!(core.seat.fold().get("active.thread").unwrap(), "b");

        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::ToggleViewItemExpansion {
                instance_key: instance_key.clone(),
                item_id: "view:test/exact#id:b".to_string(),
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
        let a_id = "view:test/exact#id:a".to_string();
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
    fn exact_tree_folding_uses_branch_identity_in_tiles_and_shared_docks() {
        for dock in [false, true] {
            let browser = session_with_surface_and_docks(
                serde_json::json!({
                    "name": "tree-pointer",
                    "tiles": if dock { vec!["view:test/idle"] } else { vec!["view:test/tree", "view:test/idle"] },
                    "views": {
                        "view:test/idle": {"widget":"text", "body":{"lines":["Idle"]}},
                        "view:test/tree": {
                            "widget": "table",
                            "sources": {"default":{"ref":"service:test/tree", "collection":"rows"}},
                            "projections": {
                                "hierarchy":{"id":"id", "parent":"parent"},
                                "columns":[{"label":"Node", "field":"id"}]
                            }
                        }
                    }
                }),
                if dock {
                    serde_json::json!({"right":{"content":"view:test/tree","open":true,"size":32}})
                } else {
                    serde_json::json!({})
                },
            );
            let mut core = test_support_core(browser);
            let instance = if dock {
                core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Right)
                    .unwrap()
            } else {
                core.active_view_set()
                    .unwrap()
                    .tiles
                    .values()
                    .find(|tile| tile.view.view_ref == "view:test/tree")
                    .unwrap()
                    .instance_key
                    .clone()
            };
            let idle = core
                .active_view_set()
                .unwrap()
                .tiles
                .iter()
                .find(|(_, tile)| tile.view.view_ref == "view:test/idle")
                .map(|(id, _)| *id)
                .unwrap();
            core.active_view_set_mut().unwrap().focused_tile = idle;
            let source =
                crate::ui::source_key::RyeOsSourceInstanceKey::named(instance.clone(), "default")
                    .encode();
            core.data.sources.insert(
                source.clone(),
                serde_json::json!({"rows":[
                    {"id":"parent#one"}, {"id":"child","parent":"parent#one"}, {"id":"other"}
                ]}),
            );
            let branch_id = "view:test/tree#parent#one";
            assert!(
                crate::ui::view_model::view_pointer_tree_fold(&core, &instance, branch_id)
                    .is_some()
            );
            // Reordering after the old frame must fold the same branch, not
            // the item now occupying the old cursor or the focused centre View.
            core.data.sources.insert(
                source.clone(),
                serde_json::json!({"rows":[
                    {"id":"other"}, {"id":"parent#one"}, {"id":"child","parent":"parent#one"}
                ]}),
            );
            assert!(
                core.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::SetViewTreeItemCollapsed {
                        instance_key: instance.clone(),
                        item_id: branch_id.into(),
                        collapsed: true,
                    }
                })
                .is_empty()
            );
            assert_eq!(core.active_view_set().unwrap().focused_tile, idle);
            let crate::view_set::ViewLocalState::GenericList {
                collapsed_tree_rows,
                ..
            } = core.mounted_local_state(&instance).unwrap()
            else {
                panic!("table state");
            };
            assert!(collapsed_tree_rows.contains("parent#one"));
            let crate::ui::view_model::RyeOsViewVm::Table { rows, .. } =
                crate::ui::view_model::view_vm_for_instance(&core, &instance).unwrap()
            else {
                panic!("table");
            };
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|row| row.id != "view:test/tree#child"));
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SetViewTreeItemCollapsed {
                    instance_key: instance.clone(),
                    item_id: branch_id.into(),
                    collapsed: false,
                },
            });
            let crate::ui::view_model::RyeOsViewVm::Table { rows, .. } =
                crate::ui::view_model::view_vm_for_instance(&core, &instance).unwrap()
            else {
                panic!("table");
            };
            assert_eq!(rows.len(), 3);
            // Leaf and missing identities are no-ops, including cursor state.
            for item_id in ["view:test/tree#child", "view:test/tree#removed"] {
                let generation = core.generation;
                core.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::SetViewTreeItemCollapsed {
                        instance_key: instance.clone(),
                        item_id: item_id.into(),
                        collapsed: true,
                    },
                });
                assert_eq!(core.generation, generation);
            }
            // Duplicate IDs cannot establish which branch the old event meant.
            core.data.sources.insert(
                source.clone(),
                serde_json::json!({"rows":[
                    {"id":"parent#one"}, {"id":"parent#one"}, {"id":"child","parent":"parent#one"}
                ]}),
            );
            assert!(
                crate::ui::view_model::view_pointer_tree_fold(&core, &instance, branch_id)
                    .is_none()
            );
            core.data.sources.insert(
                source.clone(),
                serde_json::json!({"rows":[
                    {}, {"id":"child","parent":"index:0"}
                ]}),
            );
            assert!(
                crate::ui::view_model::view_pointer_tree_fold(
                    &core,
                    &instance,
                    "view:test/tree#index:0"
                )
                .is_none()
            );
            if dock {
                core.data.sources.insert(
                    source,
                    serde_json::json!({"rows":[
                        {"id":"parent#one"}, {"id":"child","parent":"parent#one"}
                    ]}),
                );
                core.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::Activate {
                        intent: RyeOsUiIntent::ToggleDock {
                            edge: crate::ui::model::RyeOsDockEdge::Right,
                        },
                    },
                });
                assert!(crate::ui::view_model::view_vm_for_instance(&core, &instance).is_none());
                let generation = core.generation;
                core.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::SetViewTreeItemCollapsed {
                        instance_key: instance,
                        item_id: branch_id.into(),
                        collapsed: true,
                    },
                });
                assert_eq!(
                    core.generation, generation,
                    "late pointer cannot fold a closed dock"
                );
            }
        }
    }

    #[test]
    fn exact_tree_fold_refuses_inactive_tabs_and_surfaces() {
        let mut browser = session_with_surface(serde_json::json!({
            "name":"tree-tabs",
            "view_sets":[
                {"schema":2,"id":"one","title":"One","root":{"type":"group","views":["view:test/tree"],"active":0}},
                {"schema":2,"id":"two","title":"Two","root":{"type":"group","views":["view:test/tree"],"active":0}}
            ],
            "views":{"view:test/tree":{
                "widget":"table",
                "sources":{"default":{"ref":"service:test/tree","collection":"rows"}},
                "projections":{"hierarchy":{"id":"id","parent":"parent"},"columns":[{"label":"Node","field":"id"}]}
            }}
        }));
        let descriptor = browser.binding_attachments[0].clone();
        let membership = &mut browser.presentations[0];
        let mut other = membership.surfaces[0].clone();
        other.surface_instance_id = "00000000-0000-4000-8000-000000000099".into();
        other.ordinal = 2;
        let seeds = descriptor
            .presentation
            .definition
            .surface()
            .unwrap()
            .to_mount_seeds()
            .unwrap();
        let first = seeds.first().unwrap().id.clone();
        other.mount_manifest = crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
            crate::ids::PresentationId::try_new(membership.presentation_id.clone()).unwrap(),
            crate::ids::SurfaceInstanceId::try_new(other.surface_instance_id.clone()).unwrap(),
            membership.next_mount_id,
            seeds
                .into_iter()
                .map(|seed| (seed, other.binding.clone()))
                .collect(),
            Some(&first),
        )
        .unwrap();
        membership.next_mount_id = other.mount_manifest.next_mount_id;
        membership.surfaces.push(other);
        let mut core = test_support_core(browser);
        let surface = core.active_surface().unwrap().id().clone();
        let set = core.active_view_set().unwrap();
        let original_tab = set.id;
        let instance = set.tiles[&set.focused_tile].instance_key.clone();
        let second_tab = core.active_surface().unwrap().view_sets()[1].id;
        let second_surface = core
            .presentation
            .as_ref()
            .unwrap()
            .surfaces()
            .keys()
            .find(|id| *id != &surface)
            .unwrap()
            .clone();
        let source =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance.clone(), "default")
                .encode();
        core.data.sources.insert(
            source,
            serde_json::json!({"rows":[
                {"id":"other"},{"id":"parent"},{"id":"child","parent":"parent"}
            ]}),
        );
        assert!(
            crate::ui::view_model::view_pointer_tree_fold(
                &core,
                &instance,
                "view:test/tree#parent"
            )
            .is_some()
        );
        for inactive_surface in [false, true] {
            if inactive_surface {
                core.presentation
                    .as_mut()
                    .unwrap()
                    .select_view_set(&surface, original_tab)
                    .unwrap();
                core.presentation
                    .as_mut()
                    .unwrap()
                    .select_surface(&second_surface)
                    .unwrap();
            } else {
                core.presentation
                    .as_mut()
                    .unwrap()
                    .select_view_set(&surface, second_tab)
                    .unwrap();
            }
            let before = core.mounted_local_state(&instance).unwrap().clone();
            let generation = core.generation;
            assert!(
                core.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::SetViewTreeItemCollapsed {
                        instance_key: instance.clone(),
                        item_id: "view:test/tree#parent".into(),
                        collapsed: true,
                    }
                })
                .is_empty()
            );
            assert_eq!(core.mounted_local_state(&instance), Some(&before));
            assert_eq!(core.generation, generation);
        }
    }

    #[test]
    fn exact_tree_fold_in_dialog_survives_close_without_accepting_late_events() {
        let mut browser = session();
        let root = browser.root_attachment_id.clone();
        browser.binding_attachments.iter_mut().find(|attachment| attachment.binding_attachment_id == root)
            .unwrap().presentation.views.insert("view:ryeos/launcher".into(), serde_json::json!({
                "widget":"table",
                "sources":{"default":{"ref":"service:test/tree","collection":"rows"}},
                "projections":{"hierarchy":{"id":"id","parent":"parent"},"columns":[{"label":"Node","field":"id"}]}
            }));
        let mut core = test_support_core(browser);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenLauncher,
        });
        let instance = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .instance_key()
            .clone();
        let source =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance.clone(), "default")
                .encode();
        core.data.sources.insert(
            source,
            serde_json::json!({"rows":[
                {"id":"parent"},{"id":"child","parent":"parent"}
            ]}),
        );
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetViewTreeItemCollapsed {
                instance_key: instance.clone(),
                item_id: "view:ryeos/launcher#parent".into(),
                collapsed: true,
            },
        });
        let crate::view_set::ViewLocalState::GenericList {
            collapsed_tree_rows,
            ..
        } = core.mounted_local_state(&instance).unwrap()
        else {
            panic!("dialog table state")
        };
        assert!(collapsed_tree_rows.contains("parent"));
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CloseDialog {
                instance_key: instance.clone(),
            },
        });
        let before = core.mounted_local_state(&instance).unwrap().clone();
        let generation = core.generation;
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetViewTreeItemCollapsed {
                instance_key: instance.clone(),
                item_id: "view:ryeos/launcher#parent".into(),
                collapsed: false,
            },
        });
        assert_eq!(core.mounted_local_state(&instance), Some(&before));
        assert_eq!(core.generation, generation);
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenLauncher,
        });
        assert_eq!(core.mounted_local_state(&instance), Some(&before));
    }

    #[test]
    fn exact_dock_disclosure_mutates_the_named_mounted_record() {
        let browser = session_with_surface_and_docks(
            serde_json::json!({
                "name": "dock-disclosure",
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
            }),
            serde_json::json!({"right":{"content":"view:test/dock","open":true,"size":32}}),
        );
        let mut core = test_support_core(browser);
        let instance_key = core
            .shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Right)
            .unwrap();
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
        let browser = session_with_surface(serde_json::json!({
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
        }));
        let mut core = test_support_core(browser);
        let view_set = &core.active_view_set().unwrap();
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
        assert!(
            core.active_view_set_mut()
                .unwrap()
                .move_tile_to_group(b, a, 1)
        );
        let b_instance = core.active_view_set().unwrap().tiles[&b]
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
            core.active_view_set_mut()
                .unwrap()
                .root
                .as_mut()
                .unwrap()
                .select_tab(a)
        );
        core.active_view_set_mut().unwrap().focused_tile = a;
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
        let browser = session_with_surface(serde_json::json!({
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
        }));
        let mut core = test_support_core(browser);
        let tile_id = core.active_view_set().unwrap().focused_tile;
        let instance_key = core.active_view_set().unwrap().tiles[&tile_id]
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
            &core.active_view_set().unwrap().tiles[&tile_id].local
        else {
            panic!("sections view retains generic-list state");
        };
        assert_eq!(collapsed.iter().copied().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn dismiss_notice_is_exact_and_idempotent() {
        let mut core = test_support_core(session());
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
    #[test]
    fn launcher_opens_the_admitted_shared_view_and_closes_only_its_exact_mount() {
        let mut core = core(session());
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenLauncher,
        });
        let key = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .instance_key()
            .clone();
        let vm = build_view_model(&core);
        assert_eq!(vm.dialogs.len(), 1);
        assert_eq!(vm.dialogs[0].instance_key, key);
        assert!(
            core.presentation.as_ref().unwrap().dialogs()["launcher"]
                .insertion()
                .is_some()
        );
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CloseDialog { instance_key: key },
        });
        assert!(build_view_model(&core).dialogs.is_empty());
    }

    #[test]
    fn undeclared_dialog_does_not_manufacture_a_builtin_launcher() {
        let mut core = core(session());
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::OpenDialog {
                dialog_id: "commands".into(),
            },
        });
        assert!(build_view_model(&core).dialogs.is_empty());
    }
}
