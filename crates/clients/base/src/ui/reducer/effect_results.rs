use super::effect::{RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
use super::model::RyeOsCore;
use super::view_model::RyeOsTone;

const INPUT_QUEUE_NOTICE_PREFIX: &str = "Queued behind active thread";

impl RyeOsCore {
    pub(crate) fn apply_effect_result(&mut self, result: RyeOsEffectResult) -> Vec<RyeOsEffect> {
        let Some(expected) = self.pending_effects.remove(&result.id) else {
            return Vec::new();
        };
        let completed_source_key = match &expected {
            RyeOsEffectKind::FetchSource { tile_id, .. } => Some(tile_id.clone()),
            _ => None,
        };

        if !effect_result_kind_matches(&expected, &result.kind) {
            self.record_effect_failure(
                result.id,
                super::effect::RyeOsUiError::definite(
                    "mismatched_effect_result",
                    "RyeOS ignored a mismatched platform effect result.",
                ),
            );
            self.notice(
                "RyeOS ignored a mismatched platform effect result.",
                RyeOsTone::Warn,
            );
            return self.finish_source_effect(completed_source_key.as_deref(), Vec::new());
        }

        if result.ok
            && let RyeOsEffectKind::ReleaseBindingAttachment {
                binding_attachment_id,
                binding_generation,
                binding_digest,
            } = &expected
        {
            if let Some(data) = result.data {
                self.apply_released_binding_attachment(
                    binding_attachment_id,
                    *binding_generation,
                    binding_digest,
                    data,
                );
            } else {
                self.notice(
                    "The project-context release returned no confirmation; local state was retained.",
                    RyeOsTone::Danger,
                );
            }
            return Vec::new();
        }

        if !self.effect_binding_is_still_live(&expected) {
            return self.finish_source_effect(completed_source_key.as_deref(), Vec::new());
        }

        if !result.ok {
            let error = result
                .error
                .unwrap_or_else(|| {
                    super::effect::RyeOsUiError::definite(
                        "platform_effect_failed",
                        "RyeOS platform effect failed",
                    )
                })
                .normalized();
            if let Some(source_key) = completed_source_key.as_deref()
                && !self.record_source_failure(source_key, result.id, &error)
            {
                return self.finish_source_effect(Some(source_key), Vec::new());
            }
            self.record_effect_failure(result.id, error.clone());
            self.notice(effect_failure_notice(&expected, &error), RyeOsTone::Danger);
            // A refused, stale, or outcome-unknown mutation is never retried.
            // Re-observe subscribed sources so a concurrent settlement or
            // expired fence does not leave an actionable stale row rendered.
            // Which sources refresh remains signed view data (`on_hint`), not
            // a product/action branch in this reducer.
            let mut effects = matches!(
                expected,
                RyeOsEffectKind::InvokeBinding {
                    intent: super::effect::InvokeIntent::Service,
                    ..
                }
            )
            .then(|| self.effects_for_hint("thread"))
            .unwrap_or_default();
            effects.extend(self.refresh_after_invocation(&expected));
            return self.finish_source_effect(completed_source_key.as_deref(), effects);
        }

        if result.kind == RyeOsEffectResultKind::BindingInvoked {
            let data = result
                .data
                .as_ref()
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let mut effects = self.apply_invocation_result(&expected, result.kind, data);
            effects.extend(self.refresh_after_invocation(&expected));
            return self.finish_source_effect(completed_source_key.as_deref(), effects);
        }

        if matches!(expected, RyeOsEffectKind::ReplaceSession { .. }) {
            // Browser navigation unloads this core and returns null. Native
            // adapters instead return the newly loaded session so the same
            // renderer-neutral state machine can replace its authority in
            // place without retaining any old binding entry.
            let Some(data) = result.data else {
                return Vec::new();
            };
            if data.is_null() {
                return Vec::new();
            }
            let Ok(session) = serde_json::from_value(data) else {
                self.notice(
                    "The replacement UI session could not be loaded.",
                    RyeOsTone::Danger,
                );
                return Vec::new();
            };
            let viewport = self.runtime.viewport;
            let now_ms = self.runtime.now_ms;
            // Effect results may arrive in multiple transport batches. Keep
            // the sequence monotonic across immutable session replacement so
            // a delayed predecessor result can never correlate with a newly
            // emitted successor effect.
            let next_effect_id = self.next_effect_id;
            *self = RyeOsCore::new(session, viewport, now_ms);
            self.next_effect_id = next_effect_id;
            self.bump_generation();
            return self.initial_effects();
        }

        let Some(data) = result.data else {
            self.bump_generation();
            return self.finish_source_effect(completed_source_key.as_deref(), Vec::new());
        };

        let mut effects = self.apply_source_result(&expected, result.kind, result.id, data);
        effects.extend(self.refresh_after_invocation(&expected));
        self.finish_source_effect(completed_source_key.as_deref(), effects)
    }

    /// Refresh only the invoking signed view's mounted sources. This is an
    /// observation after settlement/refusal, never a mutation retry.
    fn refresh_after_invocation(&mut self, expected: &RyeOsEffectKind) -> Vec<RyeOsEffect> {
        let RyeOsEffectKind::InvokeBinding {
            request,
            invocation_origin: Some(invocation_origin),
            ..
        } = expected
        else {
            return Vec::new();
        };
        let crate::ui::binding::UiBindingCoordinate::Affordance { view_ref, .. } =
            &request.coordinate
        else {
            return Vec::new();
        };
        if !self.effect_binding_is_still_live(expected)
            || !self
                .binding_for_instance(invocation_origin, view_ref)
                .is_some_and(|binding| {
                    binding
                        .refresh
                        .get("after_invoke")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                })
        {
            return Vec::new();
        }
        if self.mounted_view_ref(invocation_origin) != Some(view_ref) {
            return Vec::new();
        }
        self.emit_fetch_source_for_instance(invocation_origin.clone(), view_ref)
    }

    fn effect_binding_is_still_live(&self, expected: &RyeOsEffectKind) -> bool {
        let (request, origin, surface_channel) = match expected {
            RyeOsEffectKind::FetchSource {
                tile_id, request, ..
            } => {
                let decoded = crate::ui::source_key::RyeOsSourceInstanceKey::decode(tile_id);
                (
                    request,
                    decoded.map(|key| key.view_instance),
                    tile_id.strip_prefix("surface/").map(str::to_string),
                )
            }
            RyeOsEffectKind::InvokeBinding {
                request,
                invocation_origin,
                input_origin,
                ..
            } => (
                request,
                invocation_origin.clone().or_else(|| {
                    input_origin
                        .as_ref()
                        .map(|address| address.buffer.view_instance_key.clone())
                }),
                None,
            ),
            _ => return true,
        };
        let Some(attachment) = self.binding_attachment(&request.binding_attachment_id) else {
            return false;
        };
        if attachment.binding_generation != request.binding_generation
            || attachment.binding_digest != request.binding_digest
        {
            return false;
        }
        if let Some(surface_channel) = surface_channel {
            let crate::ui::binding::UiBindingCoordinate::Source { view_ref, channel } =
                &request.coordinate
            else {
                return false;
            };
            return !surface_channel.is_empty()
                && surface_channel == *channel
                && request.binding_attachment_id == self.surface_attachment_id
                && attachment.surface_ref == *view_ref;
        }
        let requested_view = match &request.coordinate {
            crate::ui::binding::UiBindingCoordinate::Source { view_ref, .. }
            | crate::ui::binding::UiBindingCoordinate::Affordance { view_ref, .. } => {
                Some(view_ref.as_str())
            }
            crate::ui::binding::UiBindingCoordinate::SurfaceRoute => None,
        };
        origin.is_some_and(|instance| {
            self.instance_binding_attachments.get(&instance) == Some(&request.binding_attachment_id)
                && self.mounted_view_ref(&instance).is_some_and(|mounted| {
                    requested_view.is_none_or(|requested| requested == mounted)
                })
        })
    }

    fn finish_source_effect(
        &mut self,
        completed_source_key: Option<&str>,
        mut effects: Vec<RyeOsEffect>,
    ) -> Vec<RyeOsEffect> {
        if let Some(effect) = completed_source_key
            .and_then(|source_key| self.release_deferred_source_fetch(source_key))
        {
            effects.push(effect);
        }
        effects
    }

    fn record_effect_failure(&mut self, result_id: u64, error: super::effect::RyeOsUiError) {
        const MAX_RETAINED_EFFECT_FAILURES: usize = 32;
        self.ui.effect_failures.insert(result_id, error);
        while self.ui.effect_failures.len() > MAX_RETAINED_EFFECT_FAILURES {
            let Some(oldest) = self.ui.effect_failures.keys().next().copied() else {
                break;
            };
            self.ui.effect_failures.remove(&oldest);
        }
    }

    fn record_source_failure(
        &mut self,
        source_key: &str,
        result_id: u64,
        error: &super::effect::RyeOsUiError,
    ) -> bool {
        let floor = self.data.source_floor.get(source_key).copied().unwrap_or(0);
        let newest = self.data.source_epoch.get(source_key).copied().unwrap_or(0);
        let stored = self.data.source_stored_epoch.get(source_key).copied();
        if result_id < floor
            || result_id < newest
            || stored.is_some_and(|stored_id| result_id < stored_id)
        {
            return false;
        }
        self.data
            .source_errors
            .insert(source_key.to_string(), error.clone());
        self.stop_field_playback_for_source(source_key);
        true
    }

    /// A bound invocation result always resolves through the invocation tower,
    /// never through the source parse-and-store path.
    fn apply_invocation_result(
        &mut self,
        expected: &RyeOsEffectKind,
        kind: RyeOsEffectResultKind,
        data: serde_json::Value,
    ) -> Vec<RyeOsEffect> {
        match kind {
            RyeOsEffectResultKind::BindingInvoked => {
                // Typed submit result: { thread_id?, delivery, notice?, execution? }.
                let outcome: super::dto::LaunchOutcome =
                    serde_json::from_value(data.clone()).unwrap_or_default();
                self.apply_launch_outcome(expected, outcome, &data)
            }
            _ => unreachable!(),
        }
    }

    /// The `Invoked` tower: a plain service-ref success (row management), a
    /// refused/submitted live delivery, or a fresh launch that ratchets the
    /// seat route onto the produced thread.
    fn apply_launch_outcome(
        &mut self,
        expected: &RyeOsEffectKind,
        outcome: super::dto::LaunchOutcome,
        data: &serde_json::Value,
    ) -> Vec<RyeOsEffect> {
        // A `Service`-intent invoke (row management like cancel) is NOT a launch:
        // the emit site declared its intent, so the result never sniffs the ref.
        // Reading its result as a launch outcome would clear the focused filter
        // and falsely claim "Thread launched" — so handle it as a plain service
        // success: refresh the list/braid and preserve the focused input. Copy
        // comes from the affordance's `notice:` template (rendered against the
        // outcome), falling back to the generic success notice.
        let service_notice = match expected {
            RyeOsEffectKind::InvokeBinding {
                intent: super::effect::InvokeIntent::Service,
                success_notice,
                ..
            } => Some(success_notice),
            _ => None,
        };
        if let Some(transition) = data.get("ui_transition")
            && transition.get("kind").and_then(serde_json::Value::as_str)
                == Some("admit_binding_attachment")
        {
            return self.apply_admitted_binding_attachment(transition);
        }
        if let Some(success_notice) = service_notice
            && outcome.delivery.is_none()
        {
            let notice = match success_notice {
                Some(template) => render_result_notice(template, data),
                None => effect_success_notice(expected, data),
            };
            self.notice(notice, RyeOsTone::Good);
            return self.effects_for_hint("thread");
        }
        if outcome.delivery == Some(super::dto::ThreadDelivery::Refused) {
            // A refused delivery (non-continuation target, settled
            // status, or duplicate-submit conflict) delivered
            // nothing: KEEP the buffer so the operator's text isn't
            // lost, surface the daemon's reason, and do NOT ratchet
            // or claim a launch. (`thread_id` may be null or an
            // existing id; either way nothing new was created.)
            self.notice(
                outcome.notice.unwrap_or_else(|| REFUSED_NOTICE.to_string()),
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        if outcome.delivery == Some(super::dto::ThreadDelivery::Submitted) {
            // Live fold into a RUNNING thread: the stimulus was
            // delivered as a new cognition_in on the SAME thread — no
            // new thread, so no ratchet and no "launched" copy. Clear
            // the buffer and keep the route where it is; the live tail
            // shows the folded turn.
            if let RyeOsEffectKind::InvokeBinding {
                input_origin: Some(origin),
                ..
            } = expected
            {
                self.clear_addressed_input(origin);
            }
            let notice = submitted_delivery_notice(
                outcome.thread_id.as_deref(),
                outcome.notice,
                outcome.pending,
            );
            if notice.queued {
                self.notice_replacing_prefix(
                    INPUT_QUEUE_NOTICE_PREFIX,
                    notice.message,
                    notice.tone,
                );
            } else {
                self.notice(notice.message, notice.tone);
            }
            return self.effects_for_hint("thread");
        }
        if let RyeOsEffectKind::InvokeBinding {
            input_origin: Some(origin),
            ..
        } = expected
        {
            self.clear_addressed_input(origin);
        }
        let Some(thread_id) = outcome.thread_id.clone() else {
            self.notice(effect_success_notice(expected, data), RyeOsTone::Good);
            self.bump_generation();
            return Vec::new();
        };
        // Ratchet: the route is live state — a launch retargets
        // the input at the produced thread so the next submit
        // continues the chain. A stale result (route changed
        // since issue) may notice but never retargets.
        let route_metadata = match expected {
            RyeOsEffectKind::InvokeBinding {
                input_origin,
                route_seq,
                ratchet_on_thread_id,
                ..
            } => input_origin
                .as_ref()
                .map(|origin| (origin.clone(), *route_seq, *ratchet_on_thread_id)),
            _ => None,
        };
        if let Some((origin, route_seq, ratchet_on_thread_id)) = route_metadata {
            self.try_ratchet_route(
                &origin,
                route_seq,
                ratchet_on_thread_id,
                &thread_id,
                outcome.execution,
            );
        }
        self.notice(format!("Thread {thread_id} launched."), RyeOsTone::Good);
        let mut effects = Vec::new();
        if let RyeOsEffectKind::InvokeBinding {
            input_origin: Some(origin),
            ..
        } = expected
        {
            effects.extend(self.effects_for_view_instance(&origin.buffer.view_instance_key));
        }
        effects.extend(self.effects_for_hint("thread"));
        effects
    }

    /// Retarget the seat route onto a just-launched thread, honoring the
    /// issue-time ratchet eligibility and the produced thread's substrate
    /// facts. Returns whether the route was retargeted; a stale result (the
    /// route moved since submit) notices and leaves the route untouched.
    fn try_ratchet_route(
        &mut self,
        origin: &super::model::RyeOsInputAddress,
        route_seq: Option<u64>,
        ratchet_on_thread_id: bool,
        thread_id: &str,
        execution: Option<super::dto::ExecutionFacts>,
    ) -> bool {
        if !self.input_address_is_live(origin) {
            self.notice(
                "Input context changed since submit; not retargeting.",
                RyeOsTone::Warn,
            );
            return false;
        }
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
        let result_supports = execution.map(|e| e.supports_operator_followup);
        let targets = ratchet_on_thread_id && result_supports != Some(false);
        let facet_key = origin.buffer.route_facet_key();
        let fold = self.seat.fold();
        if fold.seq_of(&facet_key) != route_seq {
            self.notice(
                "Route changed since submit; not retargeting.",
                RyeOsTone::Warn,
            );
            return false;
        }
        // Only ratchet a continuation target onto routes
        // whose input declares conversation targeting. A
        // fire-and-forget route that happens to produce a
        // thread_id must NOT be retargeted as "continuing" —
        // same declaration the cycle and the label key off.
        if !targets {
            return false;
        }
        let mut route = fold
            .input_route(&facet_key)
            .unwrap_or_else(|| self.route_for_instance(&origin.buffer.view_instance_key));
        // First turn of a conversation: the launched
        // thread IS the chain root (root == head).
        // Continuations (route already had a head) keep
        // the root and only advance the head — so the
        // feed keeps showing the whole braid while the
        // next submit braids onto the newest turn.
        if route.thread.is_none() {
            route.chain_root = Some(thread_id.to_string());
        }
        route.thread = Some(thread_id.to_string());
        if let Ok(value) = serde_json::to_value(&route) {
            self.seat.append_facet(facet_key, value);
        }
        true
    }

    /// The parse-and-store arms: deserialize the optional body into its DTO
    /// and fold it into `data`, honoring per-tile freshness/scope guards.
    fn apply_source_result(
        &mut self,
        expected: &RyeOsEffectKind,
        kind: RyeOsEffectResultKind,
        result_id: u64,
        data: serde_json::Value,
    ) -> Vec<RyeOsEffect> {
        match kind {
            RyeOsEffectResultKind::SourceData => {
                if let RyeOsEffectKind::FetchSource {
                    tile_id, request, ..
                } = expected
                {
                    // Freshness guard, two clauses:
                    // - the floor refuses stragglers from before the key's
                    //   subject changed (lens swap, drill return, selection
                    //   facet write) — mixed-subject data can never land;
                    // - within one subject, responses land MONOTONICALLY
                    //   against what is stored. Requiring the NEWEST request
                    //   here instead would starve any view whose query
                    //   latency exceeds the hint-refetch cadence into a
                    //   permanent "loading" — every response would arrive
                    //   already superseded.
                    let floor = self.data.source_floor.get(tile_id).copied().unwrap_or(0);
                    let stored = self.data.source_stored_epoch.get(tile_id).copied();
                    if result_id >= floor && stored.is_none_or(|s| result_id >= s) {
                        self.apply_bound_source_projection(request, tile_id, &data);
                        let old = self.data.sources.get(tile_id).cloned();
                        self.note_source_row_changes(tile_id, old.as_ref(), &data);
                        self.data.sources.insert(tile_id.clone(), data);
                        self.rebuild_field_source_cache(tile_id);
                        self.data.source_errors.remove(tile_id);
                        self.data
                            .source_stored_epoch
                            .insert(tile_id.clone(), result_id);
                        self.rebuild_timeline_source_cache(tile_id);
                        self.note_field_semantic_changes(tile_id);
                        let accepted = self.data.sources.get(tile_id).cloned();
                        if let Some(accepted) = accepted.as_ref() {
                            self.settle_field_source(tile_id, accepted);
                        }
                    }
                    self.bump_generation();
                }
            }
            RyeOsEffectResultKind::BindingInvoked => {
                unreachable!("command results are handled before optional data extraction")
            }
            RyeOsEffectResultKind::BrowserOnly => {}
        }

        self.bump_generation();
        Vec::new()
    }

    fn apply_admitted_binding_attachment(
        &mut self,
        transition: &serde_json::Value,
    ) -> Vec<RyeOsEffect> {
        let Some(value) = transition.get("attachment") else {
            self.notice(
                "The project response omitted its admitted binding.",
                RyeOsTone::Danger,
            );
            return Vec::new();
        };
        let Ok(descriptor) =
            serde_json::from_value::<crate::ui::UiBindingAttachment>(value.clone())
        else {
            self.notice(
                "The admitted project binding is invalid.",
                RyeOsTone::Danger,
            );
            return Vec::new();
        };
        if descriptor.binding_attachment_id.is_empty()
            || descriptor.binding_generation == 0
            || descriptor.binding_digest.is_empty()
            || descriptor.surface_ref.is_empty()
            || descriptor.surface_generation.is_empty()
            || descriptor.binding_request_bounds.max_request_bytes == 0
            || descriptor.binding_request_bounds.max_input_bytes == 0
            || self
                .binding_attachments
                .contains_key(&descriptor.binding_attachment_id)
        {
            self.notice(
                "The admitted project binding is stale or duplicated.",
                RyeOsTone::Danger,
            );
            return Vec::new();
        }
        let Ok(retained) = crate::ui::binding_context::RetainedUiBindingAttachment::from_descriptor(
            descriptor.clone(),
        ) else {
            self.notice(
                "The admitted project surface is invalid.",
                RyeOsTone::Danger,
            );
            return Vec::new();
        };
        let Ok(surface) = serde_json::from_value::<crate::surface::SurfaceSpec>(
            descriptor.effective_surface.clone(),
        ) else {
            self.notice(
                "The admitted project surface is invalid.",
                RyeOsTone::Danger,
            );
            return Vec::new();
        };
        let Ok(mut admitted_sets) = surface.to_view_sets() else {
            self.notice(
                "The admitted project composition is invalid.",
                RyeOsTone::Danger,
            );
            return Vec::new();
        };
        let distinct_admitted_ids = admitted_sets
            .iter()
            .map(|view_set| view_set.id)
            .collect::<std::collections::HashSet<_>>()
            .len();
        if admitted_sets.is_empty()
            || distinct_admitted_ids != admitted_sets.len()
            || self.view_sets.len() + admitted_sets.len() > crate::surface::view_sets::MAX_VIEW_SETS
            || admitted_sets.iter().any(|candidate| {
                self.view_sets
                    .iter()
                    .any(|existing| existing.id == candidate.id)
            })
        {
            self.notice(
                "The admitted project composition exceeds the view-set limit.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }

        let attachment_id = descriptor.binding_attachment_id.clone();
        let first_new = self.view_sets.len();
        self.binding_attachments
            .insert(attachment_id.clone(), retained);
        if let Some(session) = self.data.session.as_mut() {
            session.binding_attachments.push(descriptor);
        }
        self.view_sets.append(&mut admitted_sets);
        for index in first_new..self.view_sets.len() {
            if !self.stamp_view_set_mounts(index, &attachment_id) {
                self.notice(
                    "The admitted project composition could not be mounted.",
                    RyeOsTone::Danger,
                );
                return Vec::new();
            }
        }
        self.active_view_set = first_new;
        self.focus_default_input();
        self.notice("Project opened in new view sets.", RyeOsTone::Good);
        self.refresh_view_set_sources()
    }

    /// Project the small set of renderer-wide scene datasets from signed
    /// source coordinates. The coordinate names presentation roles; it does
    /// not select an endpoint or grant authority. Ordinary table/timeline/
    /// field sources remain open JSON in `data.sources`.
    fn apply_bound_source_projection(
        &mut self,
        request: &crate::ui::binding::UiBindingRequest,
        source_key: &str,
        data: &serde_json::Value,
    ) {
        let crate::ui::binding::UiBindingCoordinate::Source { view_ref, channel } =
            &request.coordinate
        else {
            return;
        };
        let Some(context) = self.binding_attachments.get(&request.binding_attachment_id) else {
            return;
        };
        let role = (context.descriptor.surface_ref == *view_ref)
            .then(|| context.surface_sources.get(channel))
            .flatten()
            .or_else(|| context.views.get(view_ref)?.sources.get(channel))
            .and_then(|source| source.role.clone());
        if request.binding_attachment_id != self.surface_attachment_id
            && role.as_deref() == Some("projects")
        {
            // The ambient shell's project list belongs only to the immutable
            // root attachment. A project-specific view still renders this
            // response from its exact source key below; do not copy it into
            // the shared shell projection or warn for a successful fetch.
            return;
        }
        // Mounted scenes read these roles back from their exact source key.
        // Never copy a project attachment's scene facts into ambient/global
        // state where another mounted project could overwrite them.
        if request.binding_attachment_id != self.surface_attachment_id
            && matches!(role.as_deref(), Some("dimension" | "topology"))
        {
            return;
        }
        match role.as_deref() {
            Some("dimension") => {
                if let Ok(value) = serde_json::from_value(data.clone()) {
                    self.data.dimension = Some(value);
                }
            }
            Some("projects") => {
                if let Ok(value) = serde_json::from_value(data.clone()) {
                    self.data.projects = Some(value);
                }
            }
            Some("topology") => {
                if let Ok(value) = serde_json::from_value(data.clone()) {
                    self.data.topology = Some(value);
                }
            }
            Some("items") => {
                if let Ok(value) = serde_json::from_value(data.clone()) {
                    if let Some(tile_id) = bound_source_tile_id(source_key) {
                        self.data.tile_items.insert(tile_id, value);
                    } else {
                        self.data.items = Some(value);
                    }
                }
            }
            Some("file_space") => {
                if let Ok(value) = serde_json::from_value(data.clone()) {
                    if let Some(tile_id) = bound_source_tile_id(source_key) {
                        self.data.tile_file_space.insert(tile_id, value);
                    } else {
                        self.data.file_space = Some(value);
                    }
                }
            }
            // File reads remain under their exact source-instance key in
            // `data.sources`. A global projection would let two mounted views
            // overwrite one another and erase the originating coordinate.
            Some("file_read") => {}
            _ => {}
        }
    }
}

fn bound_source_tile_id(source_key: &str) -> Option<String> {
    let key = crate::ui::source_key::RyeOsSourceInstanceKey::decode(source_key)?;
    key.view_instance
        .as_str()
        .strip_prefix("tile:")
        .map(str::to_string)
}

struct SubmittedDeliveryNotice {
    message: String,
    tone: RyeOsTone,
    queued: bool,
}

fn submitted_delivery_notice(
    thread_id: Option<&str>,
    daemon_notice: Option<String>,
    pending: Option<u64>,
) -> SubmittedDeliveryNotice {
    if let Some(pending) = pending.filter(|pending| *pending > 0) {
        return SubmittedDeliveryNotice {
            message: format_staged_input_notice(pending),
            tone: RyeOsTone::Accent,
            queued: true,
        };
    }

    if let Some(notice) = daemon_notice {
        let queued = notice.starts_with("Input queued");
        return SubmittedDeliveryNotice {
            message: if queued {
                notice.replace("Input queued", INPUT_QUEUE_NOTICE_PREFIX)
            } else {
                notice
            },
            tone: if queued {
                RyeOsTone::Accent
            } else {
                RyeOsTone::Warn
            },
            queued,
        };
    }

    SubmittedDeliveryNotice {
        message: match thread_id {
            Some(id) => format!("Input delivered to {id}."),
            None => "Input delivered.".to_string(),
        },
        tone: RyeOsTone::Good,
        queued: false,
    }
}

fn format_staged_input_notice(pending: u64) -> String {
    let label = if pending == 1 { "input" } else { "inputs" };
    format!("{INPUT_QUEUE_NOTICE_PREFIX} · {pending} staged {label}.")
}

/// Fallback notice when a refused delivery carries no reason from the daemon.
const REFUSED_NOTICE: &str = "Delivery refused.";

fn effect_success_notice(expected: &RyeOsEffectKind, _data: &serde_json::Value) -> String {
    match expected {
        RyeOsEffectKind::InvokeBinding { .. } => "Invocation completed.".to_string(),
        _ => "RyeOS command completed.".to_string(),
    }
}

fn effect_failure_notice(
    expected: &RyeOsEffectKind,
    error: &super::effect::RyeOsUiError,
) -> String {
    let reason = if let Some(remediation) = error.remediation.as_deref() {
        format!("{} — {remediation}", error.message)
    } else {
        error.message.clone()
    };
    match expected {
        RyeOsEffectKind::InvokeBinding { .. } => {
            format!("Invocation failed: {reason}")
        }
        _ => reason,
    }
}

/// Render an affordance success-notice template, substituting `{result.<field>}`
/// tokens with the matching field of the invocation outcome (the result body).
fn render_result_notice(template: &str, data: &serde_json::Value) -> String {
    const OPEN: &str = "{result.";
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        match after.find('}') {
            Some(end) => {
                let field = &after[..end];
                out.push_str(&json_field_text(data, &[field]).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn json_field_text(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value.get(*key)).map(|v| {
        v.as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string())
    })
}

fn effect_result_kind_matches(expected: &RyeOsEffectKind, actual: &RyeOsEffectResultKind) -> bool {
    matches!(
        (expected, actual),
        (
            RyeOsEffectKind::FetchSource { .. },
            RyeOsEffectResultKind::SourceData
        ) | (
            RyeOsEffectKind::InvokeBinding { .. },
            RyeOsEffectResultKind::BindingInvoked
        ) | (
            RyeOsEffectKind::SetLocationHash { .. },
            RyeOsEffectResultKind::BrowserOnly
        ) | (
            RyeOsEffectKind::CopyToClipboard { .. },
            RyeOsEffectResultKind::BrowserOnly
        ) | (
            RyeOsEffectKind::OpenUrl { .. },
            RyeOsEffectResultKind::BrowserOnly
        ) | (
            RyeOsEffectKind::ReplaceSession { .. },
            RyeOsEffectResultKind::BrowserOnly
        ) | (
            RyeOsEffectKind::ReleaseBindingAttachment { .. },
            RyeOsEffectResultKind::BrowserOnly
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::*;

    fn test_instance() -> crate::ids::RyeOsViewInstanceKey {
        crate::ids::RyeOsViewInstanceKey::view_set_tile(crate::ids::TileId::new(77))
    }

    fn test_source_key() -> String {
        crate::ui::source_key::RyeOsSourceInstanceKey::named(test_instance(), "default").encode()
    }

    fn test_fetch_source(tile_id: String) -> RyeOsEffectKind {
        RyeOsEffectKind::FetchSource {
            tile_id,
            request: crate::ui::binding::UiBindingRequest {
                binding_attachment_id: "fixture-attachment".to_string(),
                binding_generation: 1,
                binding_digest: "11".repeat(32),
                coordinate: crate::ui::binding::UiBindingCoordinate::Source {
                    view_ref: "view:test/source".to_string(),
                    channel: "default".to_string(),
                },
                payload: crate::ui::binding::UiBindingPayload::SourceParameters {
                    params: serde_json::json!({}),
                },
            },
            request_bounds: crate::ui::binding::UiBindingRequestBounds {
                max_request_bytes: 16 * 1024,
                max_input_bytes: 8 * 1024,
            },
        }
    }

    #[test]
    fn delayed_after_invoke_refresh_targets_exact_origin_after_view_set_switch() {
        let mut session = writable_session();
        session.binding_attachments[0].effective_surface = serde_json::json!({
            "name": "refresh-origin",
            "view_sets": [
                {"id":"one", "title":"One", "root":{"type":"group", "views":["view:test/library"], "active":0}},
                {"id":"two", "title":"Two", "root":{"type":"group", "views":["view:test/library"], "active":0}}
            ],
            "views": {"view:test/library": {
                "widget":"rows",
                "sources":{"default":{"ref":"service:test/config", "collection":"rows"}},
                "refresh":{"after_invoke":true}
            }}
        });
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let origin = core.view_sets[0]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let unrelated = core.view_sets[1]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let invocation = core.emit(RyeOsEffectKind::InvokeBinding {
            request: crate::ui::binding::UiBindingRequest {
                binding_attachment_id: "fixture-attachment".into(),
                binding_generation: 1,
                binding_digest: "11".repeat(32),
                coordinate: crate::ui::binding::UiBindingCoordinate::Affordance {
                    view_ref: "view:test/library".into(),
                    affordance_id: "persist".into(),
                },
                payload: crate::ui::binding::UiBindingPayload::Selection {
                    record: serde_json::json!({}),
                },
            },
            request_bounds: core
                .binding_attachment("fixture-attachment")
                .unwrap()
                .binding_request_bounds,
            intent: crate::ui::effect::InvokeIntent::Service,
            success_notice: None,
            invocation_origin: Some(origin.clone()),
            input_origin: None,
            route_seq: None,
            ratchet_on_thread_id: false,
        });
        core.active_view_set = 1;

        let followups = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: invocation.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(serde_json::json!({"updated":true})),
                error: None,
            },
        });
        let origin_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(origin, "default").encode();
        let unrelated_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(unrelated, "default").encode();
        assert!(followups.iter().any(|effect| matches!(
            &effect.kind, RyeOsEffectKind::FetchSource { tile_id, .. } if tile_id == &origin_key
        )));
        assert!(!followups.iter().any(|effect| matches!(
            &effect.kind, RyeOsEffectKind::FetchSource { tile_id, .. } if tile_id == &unrelated_key
        )));
    }

    #[test]
    fn signed_surface_source_role_projects_shell_data() {
        let mut session = writable_session();
        session.binding_attachments[0].effective_surface["sources"] = serde_json::json!({
            "projects": {
                "ref": "service:projects/list",
                "role": "projects",
                "params": {}
            }
        });
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let effect = core
            .initial_effects()
            .into_iter()
            .find(|effect| {
                matches!(
                    &effect.kind,
                    RyeOsEffectKind::FetchSource { request, .. }
                        if matches!(
                            &request.coordinate,
                            crate::ui::binding::UiBindingCoordinate::Source { channel, .. }
                                if channel == "projects"
                        )
                )
            })
            .expect("surface source fetch");
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(serde_json::json!({"version": 1, "projects": []})),
                error: None,
            },
        });
        assert_eq!(core.data.projects.as_ref().unwrap().version, 1);
    }

    #[test]
    fn project_transition_adds_attachment_scoped_sets_without_replacing_session() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 41);
        let original_session = core.data.session.as_ref().unwrap().session_id.clone();
        let original_sets = core.view_sets.len();
        let origin = core.view_sets[0]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let view_ref = core.mounted_view_ref(&origin).unwrap().to_string();
        let source_attachment = core
            .binding_attachment_for_instance(&origin)
            .unwrap()
            .clone();
        let mut admitted = source_attachment.clone();
        admitted.binding_attachment_id = "project-attachment".into();
        admitted.binding_generation += 1;
        admitted.binding_digest = "22".repeat(32);
        admitted.project_path = Some("/tmp/project-2".into());
        let invocation = core.emit(RyeOsEffectKind::InvokeBinding {
            request: crate::ui::binding::UiBindingRequest {
                binding_attachment_id: source_attachment.binding_attachment_id,
                binding_generation: source_attachment.binding_generation,
                binding_digest: source_attachment.binding_digest,
                coordinate: crate::ui::binding::UiBindingCoordinate::Affordance {
                    view_ref,
                    affordance_id: "open-project".to_string(),
                },
                payload: crate::ui::binding::UiBindingPayload::Selection {
                    record: serde_json::json!({"local_id": "project-2"}),
                },
            },
            request_bounds: crate::ui::binding::UiBindingRequestBounds {
                max_request_bytes: 4096,
                max_input_bytes: 1024,
            },
            intent: crate::ui::effect::InvokeIntent::Service,
            success_notice: None,
            invocation_origin: Some(origin),
            input_origin: None,
            route_seq: None,
            ratchet_on_thread_id: false,
        });
        let effects = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: invocation.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(serde_json::json!({
                    "ui_transition": {
                        "kind": "admit_binding_attachment",
                        "attachment": admitted
                    }
                })),
                error: None,
            },
        });
        assert_eq!(
            core.data.session.as_ref().unwrap().session_id,
            original_session
        );
        assert!(core.binding_attachments.contains_key("project-attachment"));
        assert!(core.view_sets.len() > original_sets);
        assert_eq!(
            core.insertion_attachment_id(core.view_sets[core.active_view_set].id),
            Some("project-attachment")
        );
        assert!(
            effects
                .iter()
                .all(|effect| matches!(effect.kind, RyeOsEffectKind::FetchSource { .. }))
        );
    }

    #[test]
    fn stale_source_response_is_dropped_by_the_freshness_guard() {
        use crate::ui::effect::{RyeOsEffectResult, RyeOsEffectResultKind};
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let key = test_source_key();
        // Two fetches issued for the SAME key — a single-lens tile reused for a
        // new selection keeps its key. The second is the newest request.
        let older = core.emit(test_fetch_source(key.clone()));
        let newer = core.emit(test_fetch_source(key.clone()));
        assert!(newer.id > older.id);
        // build_fetch_source would record the newest request; simulate that.
        core.data.source_epoch.insert(key.clone(), newer.id);

        let deliver = |core: &mut RyeOsCore, id: u64, tag: &str| {
            core.dispatch(RyeOsEvent::EffectResult {
                result: RyeOsEffectResult {
                    id,
                    ok: true,
                    kind: RyeOsEffectResultKind::SourceData,
                    data: Some(serde_json::json!({ "tag": tag })),
                    error: None,
                },
            });
        };

        // Newest resolves first and lands.
        deliver(&mut core, newer.id, "new");
        assert_eq!(core.data.sources[&key]["tag"], "new");
        // An older straggler resolving afterwards is DROPPED — a slow fetch for
        // the previous selection must not overwrite the current one.
        deliver(&mut core, older.id, "old");
        assert_eq!(
            core.data.sources[&key]["tag"], "new",
            "stale straggler must not overwrite the newest response"
        );
    }

    /// Deliver a `SourceData` result carrying `{ "tag": <tag> }`.
    fn deliver(core: &mut RyeOsCore, id: u64, tag: &str) {
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(serde_json::json!({ "tag": tag })),
                error: None,
            },
        });
    }

    #[test]
    fn superseded_first_response_still_lands_when_nothing_is_stored() {
        // A refetch cadence faster than the query latency must not starve
        // the view: the FIRST response to arrive renders even though a
        // newer request is already in flight; the newer response then
        // replaces it monotonically.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/slow");
        let key = test_source_key();
        let older = core
            .emit_fetch_source_for_instance(test_instance(), "view:test/slow")
            .pop()
            .expect("fetch emitted");
        let newer = core
            .emit_fetch_source_for_instance(test_instance(), "view:test/slow")
            .pop()
            .expect("fetch emitted");
        assert!(newer.id > older.id);

        deliver(&mut core, older.id, "first");
        assert_eq!(
            core.data.sources[&key]["tag"], "first",
            "superseded-but-first response must render, not starve"
        );
        deliver(&mut core, newer.id, "second");
        assert_eq!(core.data.sources[&key]["tag"], "second");
    }

    #[test]
    fn source_failure_is_visible_until_retry_or_success() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/slow");
        let key = test_source_key();
        let failed = core
            .emit_fetch_source_for_instance(test_instance(), "view:test/slow")
            .pop()
            .expect("fetch emitted");

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: failed.id,
                ok: false,
                kind: RyeOsEffectResultKind::SourceData,
                data: None,
                error: Some("daemon rejected source".into()),
            },
        });
        assert_eq!(
            core.data
                .source_errors
                .get(&key)
                .map(|error| error.message.as_str()),
            Some("daemon rejected source")
        );
        assert_eq!(
            core.ui.effect_failures[&failed.id].code,
            "platform_effect_failed"
        );

        let retry = core
            .emit_fetch_source_for_instance(test_instance(), "view:test/slow")
            .pop()
            .expect("retry emitted");
        assert!(!core.data.source_errors.contains_key(&key));

        deliver(&mut core, retry.id, "recovered");
        assert_eq!(core.data.sources[&key]["tag"], "recovered");
        assert!(!core.data.source_errors.contains_key(&key));
    }

    #[test]
    fn superseded_source_failure_does_not_hide_newer_request() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/slow");
        let key = test_source_key();
        let older = core
            .emit_fetch_source_for_instance(test_instance(), "view:test/slow")
            .pop()
            .expect("older fetch emitted");
        let newer = core
            .emit_fetch_source_for_instance(test_instance(), "view:test/slow")
            .pop()
            .expect("newer fetch emitted");

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: older.id,
                ok: false,
                kind: RyeOsEffectResultKind::SourceData,
                data: None,
                error: Some("stale failure".into()),
            },
        });
        assert!(!core.data.source_errors.contains_key(&key));

        deliver(&mut core, newer.id, "new");
        assert_eq!(core.data.sources[&key]["tag"], "new");
    }

    #[test]
    fn facet_write_floor_refuses_pre_write_stragglers() {
        // Even with the store empty after eviction, a response from a fetch
        // issued BEFORE the facet write (the old subject) must not land.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/detail",
            serde_json::json!({
                "widget": "rows",
                "refresh": { "on_facet": "selection" },
                "sources": { "default": { "ref": "service:test/detail", "params": {}, "collection": "rows" } }
            }),
        );
        let tile_id = core.view_sets[core.active_view_set]
            .add_tile(ViewSpec {
                view_ref: "view:test/detail".to_string(),
            })
            .expect("fixture layout accepts view");
        let instance_key = core.view_sets[core.active_view_set].tiles[&tile_id]
            .instance_key
            .clone();
        let key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), "default")
                .encode();
        let pre_write = core
            .emit_fetch_source_for_instance(instance_key, "view:test/detail")
            .pop()
            .expect("fetch emitted");

        let refetch = core.effects_for_facet("selection");
        assert!(!refetch.is_empty(), "facet write refetches the subscriber");

        deliver(&mut core, pre_write.id, "old-subject");
        assert!(
            !core.data.sources.contains_key(&key),
            "pre-write straggler must be refused by the floor"
        );
        let fresh = refetch.last().expect("refetch effect");
        deliver(&mut core, fresh.id, "new-subject");
        assert_eq!(core.data.sources[&key]["tag"], "new-subject");
    }

    #[test]
    fn refetching_a_sections_view_keeps_prior_data_while_pending() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/detail",
            serde_json::json!({
                "widget": "sections",
                "sources": {
                    "a": { "ref": "service:a" },
                    "b": { "ref": "service:b" }
                },
                "sections": [
                    { "title": "A", "source_channel": "a", "projection": {} },
                    { "title": "B", "source_channel": "b", "projection": {} }
                ]
            }),
        );
        let instance_key = test_instance();
        let k0 = crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), "a")
            .encode();
        let k1 = crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), "b")
            .encode();
        core.data
            .sources
            .insert(k0.clone(), serde_json::json!({ "stale": "A" }));
        core.data
            .sources
            .insert(k1.clone(), serde_json::json!({ "stale": "B" }));

        // Refetching keeps each section's prior response rendering while the
        // fresh fetches are in flight — hint-driven activity refreshes would
        // otherwise blank the view every coalesced tick. Staleness is the
        // epoch guard's job (see the out-of-order straggler test above), not
        // the emitter's.
        let effects = core.emit_fetch_source_for_instance(instance_key, "view:test/detail");
        assert!(core.data.sources.contains_key(&k0));
        assert!(core.data.sources.contains_key(&k1));
        assert_eq!(effects.len(), 2, "one fetch per section");
    }

    #[test]
    fn facet_write_evicts_prior_section_data_for_subscribed_views() {
        // The counterpart guard to keeps-prior above: a facet write means
        // the SUBJECT changed, and the old subject's sections must never
        // render underneath the new selection — even if the refetch is
        // skipped or fails.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/detail",
            serde_json::json!({
                "widget": "sections",
                "refresh": { "on_facet": "selection" },
                "sources": {
                    "a": { "ref": "service:a" },
                    "b": { "ref": "service:b" }
                },
                "sections": [
                    { "title": "A", "source_channel": "a", "projection": {} },
                    { "title": "B", "source_channel": "b", "projection": {} }
                ]
            }),
        );
        let tile_id = core.view_sets[core.active_view_set]
            .add_tile(ViewSpec {
                view_ref: "view:test/detail".to_string(),
            })
            .expect("fixture layout accepts view");
        let instance_key = core.view_sets[core.active_view_set].tiles[&tile_id]
            .instance_key
            .clone();
        let k0 = crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), "a")
            .encode();
        let k1 = crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key, "b").encode();
        core.data
            .sources
            .insert(k0.clone(), serde_json::json!({ "stale": "A" }));
        core.data
            .sources
            .insert(k1.clone(), serde_json::json!({ "stale": "B" }));

        let effects = core.effects_for_facet("selection");

        assert!(
            !core.data.sources.contains_key(&k0),
            "old subject's section payload must not survive a facet write"
        );
        assert!(!core.data.sources.contains_key(&k1));
        assert_eq!(effects.len(), 2, "one fresh fetch per section");
    }

    #[test]
    fn refused_delivery_surfaces_reason_and_does_not_ratchet() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core); // targeting input → ratchet would be eligible
        set_focused_input(&mut core, "go");
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        // Daemon refuses (e.g. non-continuation target / duplicate conflict).
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(serde_json::json!({
                    "thread_id": serde_json::Value::Null,
                    "delivery": "refused",
                    "notice": "thread is not continuation-capable"
                })),
                error: None,
            },
        });
        let route = core.focused_input_route();
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
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);

        // Turn 1: starts the conversation. root == head == T-1.
        set_focused_input(&mut core, "hello");
        let e1 = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: e1.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(serde_json::json!({ "thread_id": "T-1", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.focused_input_route();
        assert_eq!(route.thread.as_deref(), Some("T-1"));
        assert_eq!(route.chain_root.as_deref(), Some("T-1"));

        // Turn 2: a follow-up braids onto T-1 → new head T-2, same root.
        set_focused_input(&mut core, "and again");
        let e2 = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: e2.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(serde_json::json!({ "thread_id": "T-2", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.focused_input_route();
        // Head advanced to the new turn; the next submit braids onto it.
        assert_eq!(route.thread.as_deref(), Some("T-2"));
        // Root unchanged — the feed keeps showing the whole conversation.
        assert_eq!(route.chain_root.as_deref(), Some("T-1"));
    }

    #[test]
    fn stale_invoke_result_never_retargets_newer_route() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        set_focused_input(&mut core, "first");
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");

        // Route changes after the submit was issued.
        set_focused_route_value(
            &mut core,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "thread": "T-other"
            }),
        );

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(serde_json::json!({
                    "thread_id": "T-stale",
                    "delivery": "launched"
                })),
                error: None,
            },
        });

        let route = core.focused_input_route();
        assert_eq!(route.thread.as_deref(), Some("T-other"));
    }

    #[test]
    fn refused_delivery_keeps_buffer() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        set_focused_input(&mut core, "hold on");
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");

        let followups = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(serde_json::json!({
                    "delivery": "refused",
                    "notice": "Thread is live; delivery refused."
                })),
                error: None,
            },
        });

        assert!(followups.is_empty());
        assert_eq!(focused_input_text(&core), "hold on");
        assert!(
            core.ui
                .notices
                .last()
                .is_some_and(|notice| notice.message.contains("refused"))
        );
    }

    #[test]
    fn mismatched_effect_result_does_not_apply_data() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:ryeos/items/space");
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec {
                        view_ref: "view:ryeos/items/space".to_string(),
                    },
                },
            },
        });
        let fetch_items = effects
            .iter()
            .find(|effect| matches!(effect.kind, RyeOsEffectKind::FetchSource { .. }))
            .expect("open bound view should fetch its source");

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: fetch_items.id,
                ok: true,
                kind: RyeOsEffectResultKind::BrowserOnly,
                data: Some(serde_json::json!({
                    "schema_version": "ryeos.test",
                    "session": { "session_id": "session-1", "surface_ref": "surface:ryeos/ryeos/base", "read_only": true },
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
        let mut core = RyeOsCore::default();

        // Streaming cognition deltas accumulate; no refetch while live.
        let effects = core.dispatch(RyeOsEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "cognition_out".to_string(),
            payload: serde_json::json!({ "delta": "Hel" }),
        });
        assert!(effects.is_empty());
        core.dispatch(RyeOsEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "cognition_out".to_string(),
            payload: serde_json::json!({ "delta": "lo" }),
        });
        assert_eq!(
            core.data.live_delta.as_ref().map(|d| d.text.as_str()),
            Some("Hello")
        );

        // The settled turn (content, no delta) supersedes the live buffer.
        core.dispatch(RyeOsEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "cognition_out".to_string(),
            payload: serde_json::json!({ "content": "Hello", "turn": 1 }),
        });
        assert!(core.data.live_delta.is_none());
    }

    #[test]
    fn thread_tail_ephemeral_nontext_is_noop() {
        let mut core = RyeOsCore::default();
        let effects = core.dispatch(RyeOsEvent::ThreadTail {
            thread_id: "T-1".to_string(),
            event_type: "stream_opened".to_string(),
            payload: serde_json::json!({}),
        });
        assert!(effects.is_empty());
        assert!(core.data.live_delta.is_none());
    }

    // --- Dedicated coverage for the extracted ratchet path (`try_ratchet_route`
    // and the `apply_launch_outcome` delivery tower). ---

    /// Deliver a launched `Invoked` outcome after seeding a targeting route,
    /// returning the resulting seat route. `mutate` runs between submit and
    /// delivery (e.g. to move the route so the result is stale).
    fn launch_and_deliver(
        core: &mut RyeOsCore,
        text: &str,
        data: serde_json::Value,
        mutate: impl FnOnce(&mut RyeOsCore),
    ) {
        seed_service_route(core);
        set_focused_input(core, text);
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        mutate(core);
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(data),
                error: None,
            },
        });
    }

    #[test]
    fn ratchet_route_seq_mismatch_warns_and_leaves_route_unchanged() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "first",
            serde_json::json!({ "thread_id": "T-stale", "delivery": "launched" }),
            |core| {
                // Route moves after the submit was issued → the result is stale.
                set_focused_route_value(
                    core,
                    serde_json::json!({
                        "invoke": { "type": "service", "ref": "service:threads/input" },
                        "thread": "T-other"
                    }),
                );
            },
        );
        let route = core.focused_input_route();
        assert_eq!(
            route.thread.as_deref(),
            Some("T-other"),
            "stale result must not retarget"
        );
        assert!(
            core.ui
                .notices
                .iter()
                .any(|n| n.message.contains("Route changed since submit")
                    && n.tone == RyeOsTone::Warn),
            "a stale ratchet surfaces the route-changed warning"
        );
    }

    #[test]
    fn ratchet_skips_retarget_when_thread_refuses_operator_followup() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "run graph",
            // Machine-continuing graph: continuation-capable but takes no operator input.
            serde_json::json!({
                "thread_id": "G-1",
                "delivery": "launched",
                "execution": { "supports_continuation": true, "supports_operator_followup": false }
            }),
            |_| {},
        );
        let route = core.focused_input_route();
        assert_eq!(
            route.thread, None,
            "no operator follow-up → route not retargeted"
        );
        assert_eq!(route.chain_root, None);
    }

    #[test]
    fn ratchet_first_turn_sets_chain_root_and_head() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "hello",
            serde_json::json!({ "thread_id": "T-1", "delivery": "launched" }),
            |_| {},
        );
        let route = core.focused_input_route();
        // First turn: the launched thread is both the chain root and the head.
        assert_eq!(route.chain_root.as_deref(), Some("T-1"));
        assert_eq!(route.thread.as_deref(), Some("T-1"));
    }

    #[test]
    fn ratchet_continuation_keeps_root_and_advances_head() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "hello",
            serde_json::json!({ "thread_id": "T-1", "delivery": "launched" }),
            |_| {},
        );
        // Second turn braids onto T-1: new head, same root.
        set_focused_input(&mut core, "again");
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(serde_json::json!({ "thread_id": "T-2", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.focused_input_route();
        assert_eq!(
            route.thread.as_deref(),
            Some("T-2"),
            "head advances to the newest turn"
        );
        assert_eq!(
            route.chain_root.as_deref(),
            Some("T-1"),
            "root is preserved across the braid"
        );
    }

    #[test]
    fn ratchet_refused_delivery_preserves_buffer_and_route() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "hold this",
            serde_json::json!({
                "thread_id": serde_json::Value::Null,
                "delivery": "refused",
                "notice": "thread is not continuation-capable"
            }),
            |_| {},
        );
        assert_eq!(
            focused_input_text(&core),
            "hold this",
            "refused delivery keeps the buffer"
        );
        let route = core.focused_input_route();
        assert_eq!(route.thread, None, "refused → no ratchet");
        assert_eq!(route.chain_root, None);
    }

    #[test]
    fn ratchet_submitted_delivery_clears_buffer_without_retarget_and_warns_when_degraded() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "steer it",
            serde_json::json!({
                "thread_id": "T-live",
                "delivery": "submitted",
                "notice": "interrupt degraded to steer"
            }),
            |_| {},
        );
        // Live fold into a running thread: buffer clears, but no new thread and no ratchet.
        assert_eq!(focused_input_text(&core), "");
        let route = core.focused_input_route();
        assert_eq!(route.thread, None, "submitted → no ratchet");
        assert!(
            core.ui
                .notices
                .iter()
                .any(|n| n.message.contains("interrupt degraded to steer")
                    && n.tone == RyeOsTone::Warn),
            "a degraded submitted delivery surfaces its notice as a warning"
        );
    }

    #[test]
    fn submitted_pending_delivery_coalesces_queue_notice() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        launch_and_deliver(
            &mut core,
            "first steer",
            serde_json::json!({
                "thread_id": "T-live",
                "delivery": "submitted",
                "notice": "Input queued (1 staged).",
                "pending": 1
            }),
            |_| {},
        );
        launch_and_deliver(
            &mut core,
            "second steer",
            serde_json::json!({
                "thread_id": "T-live",
                "delivery": "submitted",
                "notice": "Input queued (2 staged).",
                "pending": 2
            }),
            |_| {},
        );

        let queue_notices: Vec<_> = core
            .ui
            .notices
            .iter()
            .filter(|notice| notice.message.starts_with(INPUT_QUEUE_NOTICE_PREFIX))
            .collect();
        assert_eq!(queue_notices.len(), 1);
        assert_eq!(
            queue_notices[0].message,
            "Queued behind active thread · 2 staged inputs."
        );
        assert_eq!(queue_notices[0].tone, RyeOsTone::Accent);
    }
}
