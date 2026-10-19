use super::effect::{RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
use super::model::RyeOsCore;
use super::view_model::RyeOsTone;

const INPUT_QUEUE_NOTICE_PREFIX: &str = "Queued behind active thread";

impl RyeOsCore {
    pub(crate) fn apply_effect_result(&mut self, result: RyeOsEffectResult) -> Vec<RyeOsEffect> {
        if self.presentation_operations.replay_blocked {
            // Contradictory authoritative replay requires a fresh validated
            // bootstrap. Late platform results cannot mutate the fenced core.
            return Vec::new();
        }
        let Some(expected) = self.pending_effects.remove(&result.id) else {
            return Vec::new();
        };
        if matches!(
            &expected,
            RyeOsEffectKind::ObserveTransferDestination { .. }
        ) {
            return self.apply_transfer_destination_observation(expected, result);
        }
        if matches!(
            &expected,
            RyeOsEffectKind::PrepareTransfer { .. } | RyeOsEffectKind::ContactTransfer { .. }
        ) {
            let contact = match &expected {
                RyeOsEffectKind::ContactTransfer { request, .. } => Some((
                    request.transfer().operation_id.clone(),
                    request.action_name().to_owned(),
                )),
                _ => None,
            };
            // Even an accepted transport response cannot release a MOVE or
            // materialize a destination. Only authenticated paired replay may
            // change that ownership. Preserve uncertainty without blind retry.
            if result.kind != RyeOsEffectResultKind::BindingInvoked {
                self.record_effect_failure(
                    result.id,
                    super::effect::RyeOsUiError::outcome_unknown(
                        "mismatched_effect_result",
                        "Transfer contact returned a mismatched platform result; observe the retained outcome.",
                    ),
                );
                self.notice(
                    "Transfer outcome requires authenticated observation.",
                    RyeOsTone::Warn,
                );
            } else if !result.ok {
                let error = result.error.unwrap_or_else(|| {
                        super::effect::RyeOsUiError::outcome_unknown(
                            "transfer_contact_unknown",
                            "Transfer contact failed without outcome evidence; observe the retained outcome.",
                        )
                    }).normalized();
                if error.outcome == super::effect::RyeOsEffectOutcome::Refused
                    && let Some(contact) = contact
                {
                    self.refused_transfer_contacts.insert(contact);
                }
                self.record_effect_failure(result.id, error);
                self.notice(
                    "Transfer outcome requires authenticated observation.",
                    RyeOsTone::Warn,
                );
            }
            return Vec::new();
        }
        let expected = if let RyeOsEffectKind::InvokePresentationOperation { operation } = expected
        {
            if result.ok {
                if result.kind == RyeOsEffectResultKind::BindingInvoked {
                    self.retain_presentation_completion(
                        &operation,
                        result.data.unwrap_or(serde_json::Value::Null),
                    );
                } else {
                    self.record_effect_failure(
                        result.id,
                        super::effect::RyeOsUiError::outcome_unknown(
                            "mismatched_effect_result",
                            "Presentation admission returned a mismatched result; observe the retained outcome.",
                        ),
                    );
                    self.notice(
                        "Presentation admission returned a mismatched effect result.",
                        RyeOsTone::Danger,
                    );
                }
                // Completion is itself write-ahead. Do not expose the new
                // composition until its exact retained prefix is acknowledged.
                return Vec::new();
            }
            // Keep the prepared operation in the braid on an unknown/refused
            // contact. There is no blind mutation retry in this running core.
            let Some(invocation) = operation.as_invoke_binding() else {
                self.notice(
                    "A composition operation cannot be dispatched as an executable call.",
                    RyeOsTone::Danger,
                );
                return Vec::new();
            };
            invocation
        } else {
            expected
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
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct AdmittedReplacement {
                session: super::model::BrowserSession,
                presentation_id: String,
                replayed: Vec<super::seat::RetainedSeatEvent>,
            }
            let Ok(replacement) = serde_json::from_value::<AdmittedReplacement>(data) else {
                self.presentation_operations.producer = None;
                self.presentation_operations.replay_blocked = true;
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
            let mut candidate = RyeOsCore::new(
                replacement.session,
                &replacement.presentation_id,
                viewport,
                now_ms,
            );
            candidate.supports_independent_documents = self.supports_independent_documents;
            candidate.next_effect_id = next_effect_id;
            let replayed = replacement
                .replayed
                .iter()
                .map(|row| row.event.clone())
                .collect::<Vec<_>>();
            if let Err(error) = candidate.validate_replayed_initialization(&replayed) {
                self.presentation_operations.producer = None;
                self.presentation_operations.replay_blocked = true;
                self.notice(
                    format!("Replacement initialization refused: {error}"),
                    RyeOsTone::Danger,
                );
                return Vec::new();
            }
            let replay_effects =
                match candidate.replay_authenticated_seat_events(replacement.replayed) {
                    Ok(effects) => effects,
                    Err(error) => {
                        self.presentation_operations.producer = None;
                        self.presentation_operations.replay_blocked = true;
                        self.notice(
                            format!("Replacement retained history refused: {error}"),
                            RyeOsTone::Danger,
                        );
                        return Vec::new();
                    }
                };
            if let Err(error) = candidate.require_replayed_initialization() {
                self.presentation_operations.producer = None;
                self.presentation_operations.replay_blocked = true;
                self.notice(
                    format!("Replacement initialization refused: {error}"),
                    RyeOsTone::Danger,
                );
                return Vec::new();
            }
            *self = candidate;
            self.bump_generation();
            let mut effects = replay_effects;
            effects.extend(self.initial_effects());
            return effects;
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
                    crate::ui::source_key::RyeOsSurfaceSourceKey::decode(tile_id),
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
        if let Some(surface_source) = surface_channel {
            let crate::ui::binding::UiBindingCoordinate::Source { view_ref, channel } =
                &request.coordinate
            else {
                return false;
            };
            let Some(composition) = self.presentation.as_ref() else {
                return false;
            };
            let Some(surface) = composition
                .surfaces()
                .get(&surface_source.surface_instance_id)
            else {
                return false;
            };
            return composition.id() == &surface_source.presentation_id
                && surface_source.channel == *channel
                && surface.binding().binding_attachment_id == request.binding_attachment_id
                && surface.binding().binding_generation == request.binding_generation
                && surface.binding().binding_digest == request.binding_digest
                && attachment.root_ref == *view_ref;
        }
        let requested_view = match &request.coordinate {
            crate::ui::binding::UiBindingCoordinate::Source { view_ref, .. }
            | crate::ui::binding::UiBindingCoordinate::Affordance { view_ref, .. } => {
                Some(view_ref.as_str())
            }
            crate::ui::binding::UiBindingCoordinate::SurfaceRoute => None,
        };
        origin.is_some_and(|instance| {
            self.binding_attachment_for_instance(&instance)
                .is_some_and(|attachment| {
                    attachment.binding_attachment_id == request.binding_attachment_id
                        && attachment.binding_generation == request.binding_generation
                        && attachment.binding_digest == request.binding_digest
                })
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
    pub(crate) fn apply_invocation_result(
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
        if data.get("ui_transition").is_some() {
            self.notice(
                "An unacknowledged presentation transition cannot change composition.",
                RyeOsTone::Danger,
            );
            return Vec::new();
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
        if self.route_write_fenced_by_composition(&origin.buffer.view_instance_key) {
            self.notice(
                "View transition is settling; not retargeting its previous input route.",
                RyeOsTone::Warn,
            );
            return false;
        }
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
        if self.route_seq_for_instance(&origin.buffer.view_instance_key) != route_seq {
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
        let mut route = self.route_for_instance(&origin.buffer.view_instance_key);
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
            RyeOsEffectResultKind::TransferDestinationObserved => {
                unreachable!("destination observation is handled before generic data")
            }
        }

        self.bump_generation();
        Vec::new()
    }

    /// Project signed source roles without admitting another executable source.
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
        let role = (context.descriptor.root_ref == *view_ref)
            .then(|| context.surface_sources.get(channel))
            .flatten()
            .or_else(|| context.views.get(view_ref)?.sources.get(channel))
            .and_then(|source| source.role.clone());
        if request.binding_attachment_id != self.root_attachment_id
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
        if request.binding_attachment_id != self.root_attachment_id
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

impl RyeOsCore {
    fn apply_transfer_destination_observation(
        &mut self,
        expected: RyeOsEffectKind,
        result: RyeOsEffectResult,
    ) -> Vec<RyeOsEffect> {
        use ryeos_ui_presentation_contract::transfer::{
            TRANSFER_SCHEMA, TransferIdentity, TransferPhase, TransferRecord, TransferSeat,
        };
        let RyeOsEffectKind::ObserveTransferDestination {
            destination_presentation_id,
            destination_thread_id,
            destination_chain_root_id,
            destination_root_binding,
            destination_initialization_digest,
            source_producer,
            source_chain_root_id,
            source_root_binding,
            source_revision,
            source_export,
            request_bounds,
        } = expected
        else {
            unreachable!("called for typed destination observation")
        };
        let prepared = (|| -> Result<_, String> {
            if !result.ok || result.kind != RyeOsEffectResultKind::TransferDestinationObserved {
                return Err("transfer destination observation was not authenticated".into());
            }
            let payload: super::effect::UiTransferDestinationObservationResult =
                serde_json::from_value(result.data.ok_or("destination observation absent")?)
                    .map_err(|error| format!("invalid destination observation: {error}"))?;
            let operation_id = payload.operation_id;
            let bytes = operation_id.as_bytes();
            if bytes.len() != 36
                || bytes[14] != b'4'
                || !matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
                || bytes.iter().enumerate().any(|(index, byte)| {
                    if matches!(index, 8 | 13 | 18 | 23) {
                        *byte != b'-'
                    } else {
                        !byte.is_ascii_digit() && !(b'a'..=b'f').contains(byte)
                    }
                })
                || self
                    .presentation_operations
                    .prepared
                    .contains_key(&operation_id)
                || self
                    .presentation_operations
                    .dispatched
                    .contains(&operation_id)
                || self
                    .authenticated_transfer_proofs
                    .contains_key(&operation_id)
                || self
                    .authenticated_cancellation_proofs
                    .contains_key(&operation_id)
            {
                return Err("transfer operation nonce is invalid or already used".into());
            }
            let observation = payload.observation;
            let super::super::binding::UiObservedPresentationProducer::Active {
                producer_incarnation: destination_incarnation,
            } = observation.producer
            else {
                return Err("destination has no active observed producer".into());
            };
            observation
                .control_predecessor
                .validate()
                .map_err(str::to_owned)?;
            if !matches!(
                observation.control_predecessor,
                super::super::binding::UiSeatControlPredecessor::Control { .. }
            ) {
                return Err("active destination observation has no signed control".into());
            }
            if observation.presentation_id != destination_presentation_id
                || observation.thread_id != destination_thread_id
                || observation.chain_root_id != destination_chain_root_id
                || observation.root_binding != destination_root_binding
                || !self.composition_write_available()
                || self.presentation_operations.producer.as_ref() != Some(&source_producer)
                || !self.producer_matches_membership(&source_producer)
                || self
                    .presentation
                    .as_ref()
                    .is_none_or(|presentation| presentation.root_binding() != &source_root_binding)
            {
                return Err("transfer source or destination changed during observation".into());
            }
            let session = self
                .data
                .session
                .as_ref()
                .ok_or("transfer session absent")?;
            let destinations = session
                .presentations
                .iter()
                .filter(|member| member.presentation_id == destination_presentation_id)
                .collect::<Vec<_>>();
            let [destination] = destinations.as_slice() else {
                return Err("destination membership changed during observation".into());
            };
            let sources = session
                .presentations
                .iter()
                .filter(|member| member.presentation_id == source_producer.presentation_id)
                .collect::<Vec<_>>();
            let [source] = sources.as_slice() else {
                return Err("source membership changed during observation".into());
            };
            if destination.thread_id.as_deref() != Some(destination_thread_id.as_str())
                || destination.chain_root_id.as_deref() != Some(destination_chain_root_id.as_str())
                || destination.root_binding != destination_root_binding
                || destination.initialization_digest != destination_initialization_digest
                || source.thread_id.as_deref() != Some(source_producer.thread_id.as_str())
                || source.chain_root_id.as_deref() != Some(source_chain_root_id.as_str())
                || source.root_binding != source_root_binding
            {
                return Err("transfer membership changed during destination observation".into());
            }
            let current_bounds = self
                .binding_attachment(&source_root_binding.binding_attachment_id)
                .ok_or("source root descriptor absent")?
                .binding_request_bounds;
            if current_bounds != request_bounds {
                return Err("source request bounds changed during observation".into());
            }
            let replay = self.acknowledged_composition_replay()?;
            if replay.revision != source_revision {
                return Err("source revision changed during destination observation".into());
            }
            crate::ui::transfer_capture::validate_transfer_export_against_core(
                self,
                &source_export,
                request_bounds.max_request_bytes,
            )?;
            let export_digest = crate::ui::seat_digest::seat_payload_digest(
                &serde_json::to_value(&source_export).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            let transfer = TransferIdentity {
                operation_id,
                source: TransferSeat {
                    presentation_id: source_producer.presentation_id,
                    thread_id: source_producer.thread_id,
                    chain_root_id: source_chain_root_id,
                    producer_incarnation: source_producer.producer_incarnation,
                    revision: source_revision,
                },
                destination: TransferSeat {
                    presentation_id: destination_presentation_id,
                    thread_id: destination_thread_id,
                    chain_root_id: destination_chain_root_id,
                    producer_incarnation: destination_incarnation,
                    revision: observation.revision,
                },
                export_digest,
            };
            TransferRecord {
                schema: TRANSFER_SCHEMA,
                transfer: transfer.clone(),
                phase: TransferPhase::InboundCommitment,
            }
            .validate_shape()?;
            source_export.validate_for(&transfer, request_bounds.max_request_bytes)?;
            source_export.validate_snapshot(
                &transfer,
                &replay,
                request_bounds.max_request_bytes,
            )?;
            Ok((transfer, source_export))
        })();
        match prepared {
            Ok((transfer, export)) => self.prepare_transfer(transfer, export),
            Err(reason) => {
                self.record_effect_failure(
                    result.id,
                    super::effect::RyeOsUiError::definite(
                        "transfer_destination_observation_refused",
                        reason.clone(),
                    ),
                );
                self.notice(reason, RyeOsTone::Warn);
                Vec::new()
            }
        }
    }
}

fn bound_source_tile_id(source_key: &str) -> Option<String> {
    let key = crate::ui::source_key::RyeOsSourceInstanceKey::decode(source_key)?;
    key.view_instance
        .surface_tile_id()
        .map(|tile| tile.0.to_string())
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
            RyeOsEffectKind::ObserveTransferDestination { .. },
            RyeOsEffectResultKind::TransferDestinationObserved
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
            RyeOsEffectKind::OpenNewPresentationDocument { .. },
            RyeOsEffectResultKind::BrowserOnly
        ) | (
            RyeOsEffectKind::ShowPreparedPresentationDocument { .. },
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
    #[test]
    fn pending_lens_completion_refuses_old_input_result_ratchet() {
        use crate::ui::reducer::test_support::{
            claim_fixture_producer, core, pending_prefix, session_with_surface,
            settle_pending_prefix,
        };
        let mut core = core(session_with_surface(
            serde_json::json!({"name":"ratchet-lens","tiling":{"mode":"single_lens"},"tiles":["view:test/lens"],"views":{"view:test/lens":{"widget":"text","input":{"id":"line","submit":"route"}}}}),
        ));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let mount = core.focused_view_instance_key().unwrap();
        core.focused_input = Some(mount.clone());
        let (key, _) = core.focused_input_instance().unwrap();
        let address = core.input_address_for(key).unwrap();
        let old_sequence = core.route_seq_for_instance(&mount);
        core.prepare_lens_route_drill(
            &mount,
            None,
            Some(serde_json::json!({"thread":"child","chain_root":"child"})),
            None,
        );
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        let before = core.seat.events().to_vec();
        assert!(
            core.set_input_route(&address.buffer, &core.route_for_instance(&mount))
                .is_none()
        );
        assert!(!core.try_ratchet_route(&address, old_sequence, true, "old-result", None));
        assert_eq!(
            core.seat.events(),
            before.as_slice(),
            "old result cannot append a route suffix behind lens completion"
        );
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(
            core.route_for_instance(&mount).thread.as_deref(),
            Some("child")
        );
    }
    use super::*;
    use crate::ui::reducer::test_support::*;

    fn input_core() -> RyeOsCore {
        let mut core = core(session_with_surface(serde_json::json!({
            "name":"Input result fixture", "tiles":["view:ryeos/input"],
            "input":{"invoke":{"type":"service","ref":"service:threads/input"}},
            "views":{"view:ryeos/input":{"widget":"text","input":{
                "id":"line","submit":"route","target":{"cycle":"route_chains"}
            }}}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let tile = core.active_view_set().unwrap().focused_tile;
        focus_tile(&mut core, tile);
        core
    }

    fn source_core(view_ref: &str, binding: serde_json::Value) -> RyeOsCore {
        let core = core(session_with_surface(serde_json::json!({
            "name":"Source fixture", "tiles":[view_ref], "views":{view_ref:binding}
        })));
        assert_eq!(core.focused_view_instance_key(), Some(test_instance()));
        core
    }

    fn test_instance() -> crate::ids::RyeOsViewInstanceKey {
        crate::ids::RyeOsViewInstanceKey::surface_tile(
            &crate::ids::PresentationId::try_new(FIXTURE_PRESENTATION_ID).unwrap(),
            &crate::ids::SurfaceInstanceId::try_new("00000000-0000-4000-8000-000000000001")
                .unwrap(),
            crate::ids::TileId::new(2),
        )
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
                max_seat_append_bytes: 64 * 1024,
            },
        }
    }

    #[test]
    fn delayed_after_invoke_refresh_targets_exact_origin_after_view_set_switch() {
        let session = session_with_surface(serde_json::json!({
            "name": "refresh-origin",
            "view_sets": [
                {"schema":2,"id":"one", "title":"One", "root":{"type":"group", "views":["view:test/library"], "active":0}},
                {"schema":2,"id":"two", "title":"Two", "root":{"type":"group", "views":["view:test/library"], "active":0}}
            ],
            "views": {"view:test/library": {
                "widget":"rows",
                "sources":{"default":{"ref":"service:test/config", "collection":"rows"}},
                "refresh":{"after_invoke":true}
            }}
        }));
        let mut core = core(session);
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let origin = core
            .view_sets()
            .next()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let unrelated = core
            .view_sets()
            .nth(1)
            .unwrap()
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
        core.switch_view_set_tab(1);
        settle_pending_prefix(&mut core, &producer);

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
    fn signed_surface_source_role_keeps_projects_scoped_out_of_shared_shell() {
        let mut session = writable_session();
        let crate::presentation::UiCompositionDefinition::Surface { surface } =
            &mut session.binding_attachments[0].presentation.definition
        else {
            panic!("Surface fixture required")
        };
        surface.sources = serde_json::from_value(serde_json::json!({
            "projects": {
                "ref": "service:projects/list",
                "role": "projects",
                "params": {}
            }
        }))
        .unwrap();
        let mut core = core(session);
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
        let _effects = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(serde_json::json!({"version": 1, "projects": []})),
                error: None,
            },
        });
        assert!(
            core.data.projects.is_none(),
            "a Surface role cannot overwrite shared-root project data"
        );
        assert!(effects.is_empty());
        assert!(
            core.data
                .sources
                .values()
                .any(|data| data == &serde_json::json!({"version":1,"projects":[]}))
        );
    }

    #[test]
    fn raw_project_transition_cannot_publish_without_exact_seat_acknowledgement() {
        let mut core = core(writable_session());
        let original_session = core.data.session.as_ref().unwrap().session_id.clone();
        let original_sets = core.view_sets().count();
        let origin = core
            .view_sets()
            .next()
            .unwrap()
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
        admitted.subject = crate::ui::binding::UiBindingSubject::RegisteredProject {
            local_id: "project-2".into(),
        };
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
                max_seat_append_bytes: 64 * 1024,
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
                        "kind": "open_surface",
                        "presentation_id": FIXTURE_PRESENTATION_ID,
                        "surface": {
                            "surface_instance_id": "22222222-2222-4222-8222-222222222222",
                            "ordinal": 2,
                            "title": admitted.presentation.definition.title(),
                            "binding": {
                                "binding_attachment_id": admitted.binding_attachment_id,
                                "binding_generation": admitted.binding_generation,
                                "binding_digest": admitted.binding_digest
                            }
                        },
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
        assert!(!core.binding_attachments.contains_key("project-attachment"));
        assert_eq!(core.view_sets().count(), original_sets);
        assert!(effects.is_empty());
    }

    fn particular_response(core: &RyeOsCore) -> serde_json::Value {
        let origin = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let mut attachment = core
            .binding_attachment_for_instance(&origin)
            .unwrap()
            .clone();
        attachment.binding_attachment_id = "resume-attachment".into();
        attachment.binding_generation += 1;
        attachment.binding_digest = "55".repeat(32);
        let view_ref = core.mounted_view_ref(&origin).unwrap();
        let resolved: crate::surface::view_sets::ResolvedParticularViewSet = serde_json::from_value(serde_json::json!({
            "id":"current","name":"Current work",
            "composition":{"schema":2,"id":"current","title":"Current work","root":{"type":"group","views":[view_ref],"active":0}},
            "relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"follow_own_set"}}],
            "resolved_selection_work":{"thread":"T-head","chain_root":"T-root"}
        })).unwrap();
        let binding = crate::ui::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: attachment.binding_attachment_id.clone(),
            binding_generation: attachment.binding_generation,
            binding_digest: attachment.binding_digest.clone(),
        };
        let manifest = crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
            core.presentation.as_ref().unwrap().id().clone(),
            crate::ids::SurfaceInstanceId::try_new("33333333-3333-4333-8333-333333333333").unwrap(),
            core.data.session.as_ref().unwrap().presentations[0].next_mount_id,
            vec![(resolved.composition.clone(), binding.clone())],
            Some("current"),
        )
        .unwrap();
        serde_json::json!({"particular_selection":{"kind":"view_set","id":"current",
            "expected_revision":1,"surface_ref":attachment.root_ref},"particular_view_set":resolved,"ui_transition":{
            "kind":"resume_particular_view_set","presentation_id":FIXTURE_PRESENTATION_ID,
            "creation_request_id":"44444444-4444-4444-8444-444444444444",
            "surface":{"surface_instance_id":manifest.surface_instance_id,"ordinal":2,
                "title":"Current work","binding":binding,"mount_manifest":manifest},
            "attachment":attachment
        }})
    }

    fn acknowledged_particular_history(
        core: &RyeOsCore,
        mut response: serde_json::Value,
    ) -> Vec<crate::ui::seat::SeatEvent> {
        use crate::ui::admission_operation::*;
        use crate::ui::binding::*;
        use crate::ui::seat::{SeatEvent, SeatEventKind};
        let origin = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let descriptor = core.binding_attachment_for_instance(&origin).unwrap();
        let operation_id = "44444444-4444-4444-8444-444444444444".to_string();
        let mut prepared = UiPreparedPresentationOperation {
            schema: 2,
            prepared_engine_seq: 0,
            operation_id: operation_id.clone(),
            producer: UiPresentationProducer {
                presentation_id: FIXTURE_PRESENTATION_ID.into(),
                thread_id: "fixture-seat".into(),
                producer_incarnation: "a".repeat(64),
            },
            root_binding: core.presentation.as_ref().unwrap().root_binding().clone(),
            request_bounds: descriptor.binding_request_bounds,
            body: UiPreparedPresentationOperationBody::Admission(UiPreparedAdmissionOperation {
                purpose: UiPresentationOperationKind::ResumeParticularViewSet,
                surface_open_target: None,
                particular_selection: Some(UiParticularAdmissionSelection::ViewSet {
                    id: "current".into(),
                    expected_revision: 1,
                    surface_ref: descriptor.root_ref.clone(),
                }),
                invocation_origin: origin.clone(),
                request: UiBindingRequest {
                    binding_attachment_id: descriptor.binding_attachment_id.clone(),
                    binding_generation: descriptor.binding_generation,
                    binding_digest: descriptor.binding_digest.clone(),
                    coordinate: UiBindingCoordinate::Affordance {
                        view_ref: core.mounted_view_ref(&origin).unwrap().into(),
                        affordance_id: "resume".into(),
                    },
                    payload: UiBindingPayload::Selection {
                        record: serde_json::json!({"creation_request_id":operation_id}),
                    },
                },
                success_notice: None,
            }),
        };
        prepared.assign_identity().unwrap();
        if response["ui_transition"]["creation_request_id"] == serde_json::json!(operation_id) {
            response["ui_transition"]["creation_request_id"] =
                serde_json::json!(prepared.operation_id);
        }
        let operation_id = prepared.operation_id.clone();
        let mut history = core.seat.events().to_vec();
        let seq = history.last().unwrap().seq + 1;
        history.push(SeatEvent {
            producer_engine_seq: None,
            seq,
            kind: SeatEventKind::PresentationOperationPrepared(prepared),
        });
        history.push(SeatEvent {
            producer_engine_seq: None,
            seq: seq + 1,
            kind: SeatEventKind::PresentationOperationCompleted(UiCompletedPresentationOperation {
                operation_id,
                result: response,
            }),
        });
        history
    }

    fn fold_particular(
        core: &RyeOsCore,
        response: serde_json::Value,
    ) -> Result<crate::ui::composition_replay::UiCompositionReplay, String> {
        let history = acknowledged_particular_history(core, response);
        let crate::ui::seat::SeatEventKind::PresentationInitialized(record) = &history[0].kind
        else {
            panic!("exact fixture initialization")
        };
        crate::ui::composition_replay::UiCompositionReplay::from_acknowledged_prefix(
            &record.initialization,
            &history,
        )
    }

    #[test]
    fn retained_transition_reports_acceptance_independently_of_effects() {
        let core = core(writable_session());
        let response = particular_response(&core);
        let first = fold_particular(&core, response.clone()).unwrap();
        let second = fold_particular(&core, response.clone()).unwrap();
        assert_eq!(first.next_mount_id, second.next_mount_id);
        assert_eq!(first.template_ids, second.template_ids);
        assert!(first.pending.is_empty());
        let mut contradicted = response;
        contradicted["particular_view_set"]["composition"]["title"] =
            serde_json::json!("Different");
        assert!(fold_particular(&core, contradicted).is_err());
    }

    #[test]
    fn refused_retained_transition_does_not_partially_mount_a_surface() {
        let core = core(writable_session());
        let original = core.active_surface().unwrap().id().clone();
        let attachments = core.binding_attachments.len();
        let sets = core.view_sets().count();
        let mut response = particular_response(&core);
        response["ui_transition"]["presentation_id"] = serde_json::json!("foreign");
        assert!(fold_particular(&core, response).is_err());
        assert_eq!(core.active_surface().unwrap().id(), &original);
        assert_eq!(core.binding_attachments.len(), attachments);
        assert_eq!(core.view_sets().count(), sets);
    }

    #[test]
    fn particular_resume_installs_resolved_work_before_its_first_refresh() {
        let core = core(writable_session());
        let projection = fold_particular(&core, particular_response(&core)).unwrap();
        let resumed = projection.composition.active_view_set().unwrap().id;
        assert_eq!(
            projection.work_selections[&resumed],
            serde_json::json!({"thread":"T-head","chain_root":"T-root"})
        );
        assert_eq!(projection.template_ids[&resumed], "current");
        assert_eq!(
            projection.insertion_bindings[&resumed].binding_attachment_id,
            "resume-attachment"
        );
        assert!(projection.composition.view_sets().count() > core.view_sets().count());
        assert!(!core.seat.events().iter().any(|event| matches!(&event.kind,crate::ui::seat::SeatEventKind::Facet {key,..} if key.contains("selection.work"))));
    }

    #[test]
    fn malformed_or_foreign_particular_resume_never_partially_admits() {
        let core = core(writable_session());
        let original = core.active_surface().unwrap().id().clone();
        let count = core.view_sets().count();
        for mutation in 0..3 {
            let mut response = particular_response(&core);
            match mutation {
                0 => response["ui_transition"] = serde_json::Value::Null,
                1 => response["ui_transition"]["presentation_id"] = serde_json::json!("foreign"),
                _ => {
                    response["particular_view_set"]["relationships"][0]["source"] =
                        serde_json::json!({"mode":"follow_set","saved_view_set_id":"foreign"})
                }
            }
            assert!(fold_particular(&core, response).is_err());
            assert_eq!(core.active_surface().unwrap().id(), &original);
            assert_eq!(core.view_sets().count(), count);
            assert!(!core.binding_attachments.contains_key("resume-attachment"));
        }
    }

    #[test]
    fn stale_source_response_is_dropped_by_the_freshness_guard() {
        use crate::ui::effect::{RyeOsEffectResult, RyeOsEffectResultKind};
        let mut core = source_core(
            "view:test/source",
            serde_json::json!({"widget":"rows", "sources":{"default":{"ref":"service:test/source","params":{}}}}),
        );
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
        let mut core = source_core(
            "view:test/slow",
            serde_json::json!({"widget":"rows", "sources":{"default":{"ref":"service:test/source","params":{},"collection":"rows"}}}),
        );
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
        let mut core = source_core(
            "view:test/slow",
            serde_json::json!({"widget":"rows", "sources":{"default":{"ref":"service:test/source","params":{},"collection":"rows"}}}),
        );
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
        let mut core = source_core(
            "view:test/slow",
            serde_json::json!({"widget":"rows", "sources":{"default":{"ref":"service:test/source","params":{},"collection":"rows"}}}),
        );
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
        let mut core = source_core(
            "view:test/detail",
            serde_json::json!({
                "widget": "rows",
                "refresh": { "on_facet": "selection" },
                "sources": { "default": { "ref": "service:test/detail", "params": {}, "collection": "rows" } }
            }),
        );
        let instance_key = test_instance();
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
        let mut core = source_core(
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
        let mut core = source_core(
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
        let instance_key = test_instance();
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
        let mut core = input_core();
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
        let mut core = input_core();
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
        let mut core = input_core();
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
        let mut core = input_core();
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
        let mut core = source_core(
            "view:ryeos/items/space",
            serde_json::json!({"widget":"rows", "sources":{"default":{"ref":"service:test/source","params":{}}}}),
        );
        let effects =
            core.emit_fetch_source_for_instance(test_instance(), "view:ryeos/items/space");
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
                    "session": { "session_id": "session-1", "root_ref": "config:ryeos/test/presentation", "read_only": true },
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
        let mut core = input_core();
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
                .any(|n| n.message.contains("Input context changed since submit")
                    && n.tone == RyeOsTone::Warn),
            "a changed thread invalidates the exact input address before the sequence guard"
        );
    }

    #[test]
    fn ratchet_skips_retarget_when_thread_refuses_operator_followup() {
        let mut core = input_core();
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
        let mut core = input_core();
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
        let mut core = input_core();
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
        let mut core = input_core();
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
        let mut core = input_core();
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
        let mut core = input_core();
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
