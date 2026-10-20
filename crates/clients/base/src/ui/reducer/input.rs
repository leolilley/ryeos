use super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::event::RyeOsFilterField;
use super::model::RyeOsCore;
use super::parse_tile_id;
use super::view_model::RyeOsTone;

impl RyeOsCore {
    pub(crate) fn dispatch_addressed_input(
        &mut self,
        address: super::model::RyeOsInputAddress,
        action: super::event::RyeOsInputAction,
    ) -> Vec<RyeOsEffect> {
        use super::event::{RyeOsInputAction, RyeOsUiEvent};
        use super::model::RyeOsFocusTarget;
        if !self.input_address_is_live(&address)
            || self.move_freeze_restricts_instance(&address.buffer.view_instance_key)
        {
            return Vec::new();
        }
        let Some(input) = self
            .binding_for_instance(&address.buffer.view_instance_key, &address.buffer.view_ref)
            .and_then(|view| view.input.as_ref())
        else {
            return Vec::new();
        };
        let expected = self.input_key_for(
            address.buffer.view_instance_key.clone(),
            &address.buffer.view_ref,
            input,
        );
        if expected.as_ref() != Some(&address.buffer) {
            return Vec::new();
        }
        if matches!(
            &action,
            RyeOsInputAction::NavigateSelection { .. } | RyeOsInputAction::ActivateSelection
        ) && !input.is_live_filter()
        {
            return Vec::new();
        }
        if matches!(&action, RyeOsInputAction::NavigateSelection { delta } if !matches!(*delta, -1 | 1))
        {
            return Vec::new();
        }
        // Resolve and compare before changing any focus or buffer state. A stale
        // callback must not retarget a replacement view or a changed seat route.
        let instance = address.buffer.view_instance_key.clone();
        let tile_id = self.view_set_for_instance(&instance).and_then(|set| {
            set.tiles
                .iter()
                .find_map(|(id, tile)| (tile.instance_key == instance).then_some(*id))
        });
        if let Some(tile_id) = tile_id {
            let Some(set) = self.active_view_set_mut() else {
                return Vec::new();
            };
            set.focus_tile(tile_id);
            set.focus_target = Some(RyeOsFocusTarget::ViewSetTile {
                tile_id: tile_id.0.to_string(),
            });
        }
        // A shared input has no tab. Interaction focus names its exact mount,
        // and never rewrites another View's execution route.
        self.focused_input = Some(instance);
        let event = match action {
            RyeOsInputAction::Focus => {
                self.bump_generation();
                return Vec::new();
            }
            RyeOsInputAction::SetText { text, cursor } => {
                RyeOsUiEvent::SetInputText { text, cursor }
            }
            RyeOsInputAction::Complete => RyeOsUiEvent::CompleteInput,
            RyeOsInputAction::Submit { interrupt: false } => RyeOsUiEvent::SubmitInput,
            RyeOsInputAction::Submit { interrupt: true } => RyeOsUiEvent::SubmitInputInterrupt,
            RyeOsInputAction::NavigateSelection { delta } => {
                return self.move_focused_row(delta).1;
            }
            RyeOsInputAction::ActivateSelection => RyeOsUiEvent::ActivateFocused,
        };
        self.dispatch_ui(event)
    }

    /// Refetch the focused instance's source when its input declares
    /// `feeds` (the buffer is a writer of one source param). Debounce is a
    /// renderer/transport concern; the reducer emits the refetch and the
    /// binding carries `debounce_ms` for the renderer to honour.
    /// Whether the focused input is a live filter (feeds a source, no submit).
    pub(crate) fn focused_input_is_live_filter(&self) -> bool {
        self.focused_input_instance()
            .and_then(|(key, view_ref)| {
                self.binding_for_instance(&key.view_instance_key, &view_ref)
            })
            .and_then(|binding| binding.input.as_ref())
            .is_some_and(|input| input.is_live_filter())
    }

    /// Feed-refetch effects for a buffer edit, EXCEPT for a live filter — those
    /// are debounced by the client loop (which calls [`Self::refresh_focused_feeds`]
    /// on its tick) so typing a filter doesn't block on a daemon round-trip per
    /// keystroke. The edit itself already applied; this only defers the fetch.
    pub(crate) fn feeds_effects_unless_live_filter(
        &mut self,
        live_filter: bool,
    ) -> Vec<RyeOsEffect> {
        if live_filter {
            Vec::new()
        } else {
            self.effects_for_focused_feeds()
        }
    }

    /// Public entry for the debounced feed refetch: re-derives the focused
    /// input's source fetch from the current buffer (and resets the table
    /// cursor). The client loop calls this once typing settles.
    pub fn refresh_focused_feeds(&mut self) -> Vec<RyeOsEffect> {
        self.effects_for_focused_feeds()
    }

    /// Submit the focused instance's input buffer. Three modes: `feeds`
    /// (no submit — buffer is live), `submit: <affordance>` (fire it with
    /// `{value}`), `submit: route` (the engine route-fold: classification
    /// + route_seq + ratchet, unchanged).
    ///
    /// `interrupt` selects the live-delivery intent for a submit that routes at a
    /// RUNNING thread: `true` cuts the in-flight cognition (forceful redirect),
    /// `false` steers (folds at the next turn boundary). It is carried as the
    /// `intent` param and ignored by the daemon on non-running targets (fresh
    /// launch / settled continuation land at boundaries by construction).
    pub(crate) fn submit_focused_input(&mut self, interrupt: bool) -> Vec<RyeOsEffect> {
        let Some((key, view_ref)) = self.focused_input_instance() else {
            return Vec::new();
        };
        if self.move_freeze_restricts_instance(&key.view_instance_key) {
            return Vec::new();
        }
        if self.instance_has_unresolved_required_subject(&key.view_instance_key) {
            self.notice(
                "This view requires a subject before input can be submitted.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let Some(input) = self
            .binding_for_instance(&key.view_instance_key, &view_ref)
            .and_then(|binding| binding.input.clone())
        else {
            return Vec::new();
        };
        // `feeds`-only inputs have no submit — Enter does nothing durable.
        if input.submit.is_none() {
            return Vec::new();
        }
        let text = self
            .input_buffers
            .get(&key.storage_key())
            .map(|buffer| buffer.text.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            self.notice("Input is empty.", RyeOsTone::Warn);
            return Vec::new();
        }

        if let Some(affordance_id) = input.submit_affordance() {
            // Mode 2: Enter fires a content affordance with `{value}`.
            if self.refuse_blocked_mutation_for_instance(&key.view_instance_key) {
                return Vec::new();
            }
            return self.invoke_input_affordance(
                &key.view_instance_key,
                &view_ref,
                affordance_id,
                &text,
            );
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
    /// The daemon-authored `execution.supports_operator_followup` for a thread.
    /// Gates OPERATOR-input targeting/labels: a graph is continuation-capable but
    /// machine-only, so it accepts no operator input even though it continues.
    /// `None` = the thread isn't in the fetched data or carries no execution
    /// facts — callers treat unknown optimistically, distrusting only an
    /// explicit `Some(false)`.
    pub(crate) fn thread_supports_operator_followup(&self, thread_id: &str) -> Option<bool> {
        let row = self.data.threads.as_ref()?.threads.iter().find(|row| {
            row.get("thread_id").and_then(serde_json::Value::as_str) == Some(thread_id)
        })?;
        let facts: super::dto::ExecutionFacts =
            serde_json::from_value(row.get("execution")?.clone()).ok()?;
        // A suspended follow-parent takes no operator input regardless of kind
        // policy — its resume successor (not it) is the live target. Gating here
        // means every consumer of this predicate (the input-target list, the
        // foot-input "continuing" label, the Continue command item) excludes a
        // suspended parent without each re-deriving the follow state.
        Some(facts.supports_operator_followup && !row_is_suspended_parent(row))
    }

    /// Whether a thread row is a graph follow SUSPENDED PARENT — issued a
    /// `follow:` and settled `continued`, awaiting its child chain. Read from the
    /// typed daemon-authored [`FollowFact`](super::dto::FollowFact) on the row, not
    /// raw JSON: this is a behavior gate (a suspended parent is never an
    /// input/interrupt target), exactly the no-stringly rule. Absent `follow`
    /// object → `false`.
    pub(crate) fn thread_is_suspended_parent(&self, thread_id: &str) -> bool {
        self.data
            .threads
            .as_ref()
            .and_then(|threads| {
                threads.threads.iter().find(|row| {
                    row.get("thread_id").and_then(serde_json::Value::as_str) == Some(thread_id)
                })
            })
            .is_some_and(row_is_suspended_parent)
    }

    /// Route-cycle candidate (not built): while a parent is suspended on a
    /// follow, its CHILD chain (`follow.child_chain_root_id` on the parent
    /// row) is the natural steering target — the child is what's running.
    /// Today the suspended parent is only excluded; offering the child in the
    /// cycle needs the child's row to be in the fetched page and a de-dup
    /// against its own chain entry. Build when follow steering demands it.
    pub(crate) fn input_target_chains(&self) -> Vec<(String, String)> {
        let Some(threads) = self.data.threads.as_ref() else {
            return Vec::new();
        };
        let rows = &threads.threads;
        let upstreams: std::collections::HashSet<&str> = rows
            .iter()
            .filter_map(|t| {
                t.get("upstream_thread_id")
                    .and_then(serde_json::Value::as_str)
            })
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
                .filter(|x| {
                    x.get("chain_root_id").and_then(serde_json::Value::as_str) == Some(root)
                })
                .filter_map(|x| x.get("thread_id").and_then(serde_json::Value::as_str))
                .find(|id| !upstreams.contains(id))
                .unwrap_or(root);
            // Only offer chains whose head accepts OPERATOR input — gate on
            // `execution.supports_operator_followup`, not `supports_continuation`
            // (a graph continues by machine but takes no operator input).
            // Distrust only an explicit `false`; unknown stays offered (the
            // daemon refuses a real non-followup submit anyway). A suspended
            // follow-parent is excluded outright (its successor, not it, is the
            // live target): the follow fact vetoes even an unknown execution fact.
            if self.thread_supports_operator_followup(head) == Some(false)
                || self.thread_is_suspended_parent(head)
            {
                continue;
            }
            out.push((root.to_string(), head.to_string()));
        }
        out
    }

    /// The focused input's declared targeting capability, if any. The cycle
    /// only acts when the FOCUSED input owns a `target` — the capability is
    /// content-declared, never assumed for every route-input.
    pub(crate) fn focused_input_target_cycle(&self) -> Option<super::content::InputTargetCycle> {
        self.focused_input_instance()
            .and_then(|(key, view_ref)| {
                self.binding_for_instance(&key.view_instance_key, &view_ref)
            })
            .and_then(|binding| binding.input.as_ref())
            // Defense-in-depth: targeting retargets the ROUTE, so it is only
            // meaningful on a route-submit input. Content validation degrades
            // a target on a non-route input, but don't rely on that alone.
            .filter(|input| input.submits_to_route())
            .and_then(|input| input.target.as_ref())
            .map(|target| target.cycle)
    }

    /// Cycle a live-filter box to its next/previous target field (e.g. status →
    /// kind → source). Clears the buffer — the previous field's text doesn't
    /// apply to the new one — and refetches on the new field. No-op unless the
    /// focused input declares more than one filter field.
    pub(crate) fn cycle_filter_field(&mut self, forward: bool) -> Vec<RyeOsEffect> {
        let Some((key, view_ref)) = self.focused_input_instance() else {
            return Vec::new();
        };
        if self.move_freeze_restricts_instance(&key.view_instance_key) {
            return Vec::new();
        }
        let count = self
            .binding_for_instance(&key.view_instance_key, &view_ref)
            .and_then(|binding| binding.input.as_ref())
            .and_then(|input| input.feeds.as_ref())
            .map(|feeds| feeds.field_count())
            .unwrap_or(0);
        if count < 2 {
            return Vec::new();
        }
        let buffer = self.input_buffers.entry(key.storage_key()).or_default();
        buffer.filter_field = if forward {
            (buffer.filter_field + 1) % count
        } else {
            (buffer.filter_field + count - 1) % count
        };
        buffer.text.clear();
        buffer.cursor = 0;
        self.bump_generation();
        self.effects_for_focused_feeds()
    }

    /// Cycle the input's route target through `[new conversation]
    /// + [synthetic current if not yet fetched] + [fetched chain heads]`.
    /// New = no `thread`/`chain_root` (spawns a fresh chain); a chain slot
    /// retargets the head so the next submit braids onto it. Gated on the
    /// focused input declaring `target.cycle: route_chains`.
    pub(crate) fn cycle_input_target(&mut self, forward: bool) -> Vec<RyeOsEffect> {
        // Capability gate (decision: content-declared). The keymap shouldn't
        // emit this without a declaration; if a direct/stale event arrives
        // anyway, no-op silently (no mutation, no user notice) rather than
        // panic — the reducer never trusts the caller was correct.
        let Some(super::content::InputTargetCycle::RouteChains) = self.focused_input_target_cycle()
        else {
            return Vec::new();
        };

        let Some((input_key, _)) = self.focused_input_instance() else {
            return Vec::new();
        };
        if self.move_freeze_restricts_instance(&input_key.view_instance_key) {
            return Vec::new();
        }
        let mut route = self.input_route_for(&input_key);
        // The input declared route-chain targeting (the author's assertion
        // that this route continues conversations). The only thing the engine
        // can't paper over is a route with no invoke at all — there's nothing
        // to submit onto. Surface that (deduped), don't silently no-op.
        if route.invoke.is_none() {
            self.notice_deduped(
                "This input has no route to target a conversation on.",
                RyeOsTone::Warn,
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
        if self.set_input_route(&input_key, &route).is_none() {
            return Vec::new();
        }
        self.bump_generation();
        let mut effects = self.effects_for_view_instance(&input_key.view_instance_key);
        effects.extend(self.effects_for_hint("thread"));
        effects
    }

    /// Build the ordered target slots for route-chain cycling:
    /// `[NewConversation] + [synthetic current if its root isn't fetched]
    /// + [fetched chain heads]`, deduped by `chain_root` (preferring the
    ///   fetched head when the current chain is also present in fetched data).
    pub(crate) fn route_chain_slots(&self, route: &super::seat::InputRoute) -> Vec<TargetSlot> {
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
    pub(crate) fn submit_route(&mut self, text: &str, interrupt: bool) -> Vec<RyeOsEffect> {
        let Some((origin_key, _)) = self.focused_input_instance() else {
            return Vec::new();
        };
        if self.move_freeze_restricts_instance(&origin_key.view_instance_key) {
            return Vec::new();
        }
        if self.instance_has_unresolved_required_subject(&origin_key.view_instance_key) {
            self.notice(
                "This view requires a subject before input can be submitted.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        if self.refuse_blocked_mutation_for_instance(&origin_key.view_instance_key) {
            return Vec::new();
        }
        let Some(input_origin) = self.input_address_for(origin_key.clone()) else {
            return Vec::new();
        };
        let line = match super::tokenize::classify_line(text) {
            Ok(line) => line,
            Err(error) => {
                self.notice(format!("Input parse error: {error}"), RyeOsTone::Warn);
                return Vec::new();
            }
        };
        match line {
            super::tokenize::InputLine::SlashEmpty => {
                self.notice(
                    "Type command tokens after / (e.g. /thread list).",
                    RyeOsTone::Neutral,
                );
                Vec::new()
            }
            super::tokenize::InputLine::Slash(tokens) => {
                // Explicit grammar: tokens resolve + bind daemon-side (one
                // invocation path for all clients). The interactive surface
                // signs the command-dispatch affordance; an observation child
                // erases it with `affordances: []`.
                let Some(coordinate) = self.focused_command_submit_coordinate() else {
                    self.notice(
                        "The focused input declares no command affordance.",
                        RyeOsTone::Warn,
                    );
                    return Vec::new();
                };
                let Some((request, request_bounds)) = self.compiled_binding_operation(
                    &origin_key.view_instance_key,
                    coordinate,
                    crate::ui::binding::UiBindingPayload::Tokens {
                        tokens,
                        arguments: serde_json::json!({}),
                    },
                ) else {
                    return Vec::new();
                };
                vec![self.emit(RyeOsEffectKind::InvokeBinding {
                    request,
                    request_bounds,
                    intent: super::effect::InvokeIntent::Launch,
                    success_notice: None,
                    invocation_origin: Some(origin_key.view_instance_key.clone()),
                    input_origin: Some(input_origin),
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
                let route = self.input_route_for(&origin_key);
                let route_seq = self.route_seq_for_instance(&origin_key.view_instance_key);
                let Some(invoke) = route.invoke.clone() else {
                    self.notice(
                        "Input has no target — the surface declares no route.",
                        RyeOsTone::Warn,
                    );
                    return Vec::new();
                };
                match invoke {
                    super::seat::InvokeTemplate::Service { .. }
                    | super::seat::InvokeTemplate::Command { .. } => {
                        // The effective surface owns the executable route. The
                        // client supplies only input plus the current seat
                        // continuation coordinate; the daemon lowers both
                        // against the session's compiled SurfaceRoute entry.
                        let Some((request, request_bounds)) = self.compiled_binding_operation(
                            &origin_key.view_instance_key,
                            crate::ui::binding::UiBindingCoordinate::SurfaceRoute,
                            crate::ui::binding::UiBindingPayload::Input {
                                value: plain,
                                route: Some(crate::ui::binding::UiBindingRouteContext {
                                    thread_id: route.thread.clone(),
                                    chain_root_id: route.chain_root.clone(),
                                    interrupt,
                                }),
                            },
                        ) else {
                            return Vec::new();
                        };
                        vec![self.emit(RyeOsEffectKind::InvokeBinding {
                            request,
                            request_bounds,
                            intent: super::effect::InvokeIntent::Launch,
                            success_notice: None,
                            invocation_origin: Some(origin_key.view_instance_key.clone()),
                            input_origin: Some(input_origin),
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
    pub(crate) fn invoke_input_affordance(
        &mut self,
        invocation_origin: &crate::ids::RyeOsViewInstanceKey,
        view_ref: &str,
        affordance_id: &str,
        value: &str,
    ) -> Vec<RyeOsEffect> {
        if self.move_freeze_restricts_instance(invocation_origin) {
            return Vec::new();
        }
        if self.instance_has_unresolved_required_subject(invocation_origin) {
            self.notice(
                "This view requires a subject before input can be submitted.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let Some(binding) = self.binding_for_instance(invocation_origin, view_ref) else {
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
            Some(super::content::AffordanceInvoke::OpenSavedViewSet { .. })
            | Some(super::content::AffordanceInvoke::SaveActiveViewSet { .. })
            | Some(super::content::AffordanceInvoke::SaveParticularViewSet { .. })
            | Some(super::content::AffordanceInvoke::SaveActiveSurface { .. })
            | Some(super::content::AffordanceInvoke::SaveParticularSurface { .. }) => {
                self.notice(
                    "View-set library actions require a selected library record.",
                    RyeOsTone::Warn,
                );
                Vec::new()
            }
            Some(super::content::AffordanceInvoke::Ui {
                facet,
                value,
                merge,
                open_view,
                drill,
            }) => {
                let effects = self.apply_ui_affordance(facet, value, merge, open_view, drill);
                self.clear_focused_input();
                effects
            }
            Some(super::content::AffordanceInvoke::Rye { notice, .. })
            | Some(super::content::AffordanceInvoke::Service { notice, .. }) => {
                if self.refuse_blocked_mutation_for_instance(invocation_origin) {
                    return Vec::new();
                }
                let Some((request, request_bounds)) = self.compiled_binding_operation(
                    invocation_origin,
                    crate::ui::binding::UiBindingCoordinate::Affordance {
                        view_ref: view_ref.to_string(),
                        affordance_id: affordance_id.to_string(),
                    },
                    crate::ui::binding::UiBindingPayload::Input {
                        value: value.to_string(),
                        route: None,
                    },
                ) else {
                    return Vec::new();
                };
                vec![self.emit(RyeOsEffectKind::InvokeBinding {
                    request,
                    request_bounds,
                    intent: super::effect::InvokeIntent::Service,
                    success_notice: notice,
                    invocation_origin: Some(invocation_origin.clone()),
                    input_origin: None,
                    route_seq: None,
                    ratchet_on_thread_id: false,
                })]
            }
            None => {
                self.notice(
                    "Input affordance cannot be supplied by {value}.",
                    RyeOsTone::Warn,
                );
                Vec::new()
            }
        }
    }

    pub(crate) fn clear_focused_input(&mut self) {
        if let Some(buffer) = self.focused_input_buffer_mut() {
            buffer.clear();
        }
    }

    pub(crate) fn clear_addressed_input(
        &mut self,
        address: &super::model::RyeOsInputAddress,
    ) -> bool {
        if !self.input_address_is_live(address)
            || self.move_freeze_restricts_instance(&address.buffer.view_instance_key)
        {
            return false;
        }
        let Some(buffer) = self.input_buffers.get_mut(&address.buffer.storage_key()) else {
            return false;
        };
        buffer.clear();
        true
    }

    pub(crate) fn set_tile_filter(
        &mut self,
        tile_id: String,
        field: RyeOsFilterField,
        value: String,
    ) -> Vec<RyeOsEffect> {
        let Some(tile_id) = parse_tile_id(&tile_id) else {
            return Vec::new();
        };
        let Some(tile) = self
            .active_view_set_mut()
            .and_then(|set| set.tiles.get_mut(&tile_id))
        else {
            return Vec::new();
        };
        // Item/file tiles are content-bound now; only the services
        // filter remains engine-local.
        let _ = tile;
        if matches!(field, RyeOsFilterField::ServicesQuery) {
            self.ui.filters.services_query = value;
            self.bump_generation();
        }
        Vec::new()
    }
}

/// Whether a thread projection row carries a follow fact marking it a suspended
/// parent. Reads the typed [`FollowFact`](super::dto::FollowFact) off the row's
/// `follow` object; a missing/odd `follow` is not a suspended parent.
fn row_is_suspended_parent(row: &serde_json::Value) -> bool {
    row.get("follow")
        .and_then(|follow| serde_json::from_value::<super::dto::FollowFact>(follow.clone()).ok())
        .is_some_and(|fact| fact.is_suspended_parent())
}

/// A route-chain target the input can cycle onto.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TargetSlot {
    /// No target thread/root — a submit starts a fresh chain.
    NewConversation,
    /// Braid onto an existing chain: `head` is the turn the next submit
    /// continues, `root` is the conversation identity the feed follows.
    Chain { root: String, head: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::core as test_support_core;
    use crate::ui::reducer::test_support::*;

    fn seed_input_view(core: &mut RyeOsCore) {
        crate::ui::reducer::test_support::seed_input_view(core);
        if core.focused_input_instance().is_none() {
            core.focused_input =
                core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom);
        }
        assert!(core.focused_input_instance().is_some());
    }

    fn seed_service_route(core: &mut RyeOsCore) {
        seed_input_view(core);
        crate::ui::reducer::test_support::seed_service_route(core);
    }

    fn mount_fixture_view(core: &mut RyeOsCore, view_ref: &str) -> crate::ids::TileId {
        let producer = core
            .presentation_operations
            .producer
            .clone()
            .unwrap_or_else(|| claim_fixture_producer(core));
        settle_pending_prefix(core, &producer);
        let before = core.active_view_set().unwrap().tile_ids();
        core.add_center_tile(ViewSpec::bound(view_ref));
        settle_pending_prefix(core, &producer);
        let tile = core
            .active_view_set()
            .unwrap()
            .tile_ids()
            .into_iter()
            .find(|tile| !before.contains(tile))
            .expect("admitted fixture mount");
        focus_tile(core, tile);
        tile
    }

    fn filter_core() -> RyeOsCore {
        let session = session_with_surface(serde_json::json!({
            "name":"Filter fixture","tiles":["view:test/filter"],
            "views":{"view:test/filter":{
            "widget":"rows",
            "sources":{"default":{"ref":"service:test/items","params":{"limit":50},"collection":"items"}},
            "input":{"id":"q","placeholder":"filter…","feeds":{"param":"query","debounce_ms":120}}
        }}}));
        let mut core = test_support_core(session);
        let tile = core.active_view_set().unwrap().focused_tile;
        focus_tile(&mut core, tile);
        core
    }

    fn surface_route_core() -> RyeOsCore {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"Surface route fixture","tiles":["view:ryeos/input"],
            "input":{"invoke":{"type":"service","ref":"service:threads/input"}},
            "views":{"view:ryeos/input":{"widget":"text","input":{
                "id":"line","submit":"route","target":{"cycle":"route_chains"}
            }}}
        })));
        let tile = core.active_view_set().unwrap().focused_tile;
        focus_tile(&mut core, tile);
        core
    }

    #[test]
    fn shared_draft_edits_reject_whole_over_budget_unicode_inputs() {
        let mut session = shared_session();
        session.binding_attachments[0]
            .binding_request_bounds
            .max_input_bytes = 4;
        let mut core = test_support_core(session);
        let address = build_view_model(&core)
            .docks
            .bottom
            .unwrap()
            .input
            .unwrap()
            .address;
        core.dispatch_addressed_input(
            address.clone(),
            crate::ui::event::RyeOsInputAction::SetText {
                text: "é".into(),
                cursor: 2,
            },
        );
        core.dispatch_ui(RyeOsUiEvent::InsertInputChar { ch: 'λ' });
        let before = core.focused_input_buffer().unwrap().clone();
        assert_eq!(before.text, "éλ");
        assert_eq!(before.cursor, 4);
        core.dispatch_ui(RyeOsUiEvent::InsertInputChar { ch: 'x' });
        assert_eq!(core.focused_input_buffer(), Some(&before));
        core.insert_focused_input_text("\nλ");
        assert_eq!(core.focused_input_buffer(), Some(&before));
        core.dispatch_addressed_input(
            address.clone(),
            crate::ui::event::RyeOsInputAction::SetText {
                text: "abcé".into(),
                cursor: 5,
            },
        );
        assert_eq!(core.focused_input_buffer(), Some(&before));
        core.dispatch_addressed_input(
            address,
            crate::ui::event::RyeOsInputAction::SetText {
                text: "aé".into(),
                cursor: 2,
            },
        );
        assert_eq!(core.focused_input_buffer().unwrap().cursor, 1);
        assert!(core.active_surface().is_none());
    }

    #[test]
    fn local_filter_draft_uses_its_mounted_binding_budget() {
        let mut core = filter_core();
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
            .max_input_bytes = 3;
        core.insert_focused_input_text("aé");
        let before = core.focused_input_buffer().unwrap().clone();
        core.insert_focused_input_text("x");
        assert_eq!(core.focused_input_buffer(), Some(&before));
        core.binding_attachments
            .get_mut(&id)
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_input_bytes = 0;
        core.dispatch_ui(RyeOsUiEvent::SetInputText {
            text: "x".into(),
            cursor: 1,
        });
        assert_eq!(core.focused_input_buffer(), Some(&before));
    }

    #[test]
    fn paste_inserts_unicode_and_newlines_into_exact_shared_input() {
        let mut core = test_support_core(shared_session());
        let address = build_view_model(&core)
            .docks
            .bottom
            .unwrap()
            .input
            .unwrap()
            .address;
        let storage = address.buffer.storage_key();
        core.dispatch_addressed_input(
            address,
            crate::ui::event::RyeOsInputAction::SetText {
                text: "aéz".into(),
                cursor: 3,
            },
        );
        core.input_buffers.insert(
            "unrelated".into(),
            crate::ui::model::RyeOsInputState::default(),
        );
        core.insert_focused_input_text("\nλ");
        let buffer = &core.input_buffers[&storage];
        assert_eq!(buffer.text, "aé\nλz");
        assert_eq!(buffer.cursor, 6);
        assert!(core.input_buffers["unrelated"].text.is_empty());
        assert!(core.active_surface().is_none());
    }

    #[test]
    fn paste_live_filter_defers_fetch_and_blocked_replay_rejects_edit() {
        let mut core = filter_core();
        assert!(core.insert_focused_input_text("query\ntext").is_empty());
        assert_eq!(focused_input_text(&core), "query\ntext");
        core.presentation_operations.replay_blocked = true;
        assert!(core.insert_focused_input_text("ignored").is_empty());
        assert_eq!(focused_input_text(&core), "query\ntext");
    }

    #[test]
    fn paste_without_live_input_or_with_invalid_cursor_is_inert() {
        let mut core = test_support_core(session_with_surface_and_docks(
            serde_json::json!({"name":"No input","tiles":["view:test/text"],"views":{"view:test/text":{"widget":"text","body":"Text"}}}),
            serde_json::json!({}),
        ));
        assert!(!core.has_focused_input());
        let generation = core.generation;
        assert!(core.insert_focused_input_text("ignored").is_empty());
        assert_eq!(core.generation, generation);
        let mut core = test_support_core(shared_session());
        let address = build_view_model(&core)
            .docks
            .bottom
            .unwrap()
            .input
            .unwrap()
            .address;
        core.dispatch_addressed_input(address, crate::ui::event::RyeOsInputAction::Focus);
        let buffer = core.focused_input_buffer_mut().unwrap();
        buffer.text = "é".into();
        buffer.cursor = 1;
        assert!(core.insert_focused_input_text("ignored").is_empty());
        assert_eq!(focused_input_text(&core), "é");
    }

    #[test]
    fn shared_input_remains_addressable_without_any_surface() {
        let mut core = test_support_core(shared_session());
        assert!(core.active_view_set().is_none());
        let address = build_view_model(&core)
            .docks
            .bottom
            .unwrap()
            .input
            .unwrap()
            .address;
        assert!(matches!(
            &address.owner,
            crate::ui::model::RyeOsInputOwner::Shared { .. }
        ));
        core.dispatch_addressed_input(
            address,
            crate::ui::event::RyeOsInputAction::SetText {
                text: "shared draft".into(),
                cursor: 12,
            },
        );
        assert_eq!(focused_input_text(&core), "shared draft");
        assert!(core.active_surface().is_none());
    }

    #[test]
    fn addressed_live_filter_navigates_its_mount_and_rejects_stale_addresses() {
        use crate::ui::event::RyeOsInputAction;
        let session = session_with_surface_and_docks(
            serde_json::json!({
                "name":"Filter fixture", "views": {
                    "view:test/filter": {
                        "title":"Filter", "widget":"rows",
                        "sources":{"default":{"ref":"service:test/read","collection":"rows"}},
                        "projections":{"primary":"title"},
                        "input":{"id":"filter","feeds":{"fields":[{"param":"query","label":"query"}]}}
                    }
                }
            }),
            serde_json::json!({"left":{"content":"view:test/filter","open":true,"size":32}}),
        );
        let mut core = test_support_core(session);
        let address = build_view_model(&core)
            .docks
            .left
            .unwrap()
            .input
            .unwrap()
            .address;
        let key = address.buffer.view_instance_key.clone();
        core.data.sources.insert(
            crate::ui::source_key::RyeOsSourceInstanceKey::named(key.clone(), "default").encode(),
            serde_json::json!({"rows":[{"title":"One"},{"title":"Two"}]}),
        );
        assert!(
            core.dispatch_addressed_input(
                address.clone(),
                RyeOsInputAction::NavigateSelection { delta: 1 }
            )
            .is_empty()
        );
        assert!(matches!(
            core.mounted_local_state(&key),
            Some(crate::view_set::ViewLocalState::GenericList { cursor: 1, .. })
        ));
        let mut stale = address.clone();
        stale.binding_generation += 1;
        core.dispatch_addressed_input(stale, RyeOsInputAction::NavigateSelection { delta: -1 });
        core.dispatch_addressed_input(
            address,
            RyeOsInputAction::NavigateSelection { delta: i32::MIN },
        );
        assert!(matches!(
            core.mounted_local_state(&key),
            Some(crate::view_set::ViewLocalState::GenericList { cursor: 1, .. })
        ));
    }

    #[test]
    fn live_filter_dialog_activation_never_uses_the_underlying_surface_row() {
        use crate::ui::event::{RyeOsInputAction, RyeOsUiIntent};
        for widget in ["rows", "table"] {
            let view = serde_json::json!({
                "title":"Exact list", "widget":widget,
                "sources":{"default":{"ref":"service:test/read","collection":"rows"}},
                "projections":{"primary":"title","columns":[{"label":"Title","field":"title"}]},
                "input":{"id":"filter","feeds":{"fields":[{"param":"query","label":"query"}]}},
                "selection":{"activate":"choose"},
                "affordances":[{"id":"choose","invoke":{"plane":"ui","facet":"fixture.choice","value":"{record.title}"}}]
            });
            let mut session = session_with_surface(serde_json::json!({
                "name":"Underlying", "tiles":["view:test/underlying"],
                "views":{"view:test/underlying":view.clone()}
            }));
            session
                .binding_attachments
                .last_mut()
                .unwrap()
                .presentation
                .views
                .insert("view:ryeos/launcher".into(), view);
            let mut core = test_support_core(session);
            let set = core.active_view_set().unwrap();
            let underlying = set.tiles[&set.focused_tile].instance_key.clone();
            let key = |instance| {
                crate::ui::source_key::RyeOsSourceInstanceKey::named(instance, "default").encode()
            };
            core.data.sources.insert(
                key(underlying.clone()),
                serde_json::json!({"rows":[{"title":"Wrong Surface row"}]}),
            );
            core.focused_input = None;
            assert!(matches!(
                crate::ui::view_model::intent_for_focused_row(&core),
                Some(RyeOsUiIntent::InvokeAffordance { instance_key, .. }) if instance_key == underlying
            ));
            core.open_dialog("launcher");
            let address = build_view_model(&core).dialogs[0]
                .input
                .as_ref()
                .unwrap()
                .address
                .clone();
            let dialog = address.buffer.view_instance_key.clone();
            core.data.sources.insert(
                key(dialog.clone()),
                serde_json::json!({"rows":[{"title":"Exact dialog row"}]}),
            );
            assert!(matches!(
                crate::ui::view_model::intent_for_focused_row(&core),
                Some(RyeOsUiIntent::InvokeAffordance { instance_key, record, .. })
                    if instance_key == dialog && record["title"] == "Exact dialog row"
            ));
            core.dispatch_addressed_input(address.clone(), RyeOsInputAction::ActivateSelection);
            assert_eq!(
                core.seat.fold().get("fixture.choice"),
                Some(&serde_json::json!("Exact dialog row"))
            );
            // An empty dialog must not fall through to the still-actionable tile.
            core.data
                .sources
                .insert(key(dialog.clone()), serde_json::json!({"rows":[]}));
            assert!(crate::ui::view_model::intent_for_focused_row(&core).is_none());
            core.close_dialog(&dialog);
            core.dispatch_addressed_input(address, RyeOsInputAction::ActivateSelection);
            assert_ne!(core.focused_input.as_ref(), Some(&dialog));
            assert_eq!(
                core.seat.fold().get("fixture.choice"),
                Some(&serde_json::json!("Exact dialog row"))
            );
        }
    }

    #[test]
    fn list_actions_do_not_reinterpret_an_ordinary_composer_or_change_focus() {
        use crate::ui::event::RyeOsInputAction;
        let mut core = test_support_core(session());
        let address = build_view_model(&core)
            .docks
            .bottom
            .unwrap()
            .input
            .unwrap()
            .address;
        core.focused_input = None;
        assert!(
            core.dispatch_addressed_input(address.clone(), RyeOsInputAction::ActivateSelection)
                .is_empty()
        );
        assert!(
            core.dispatch_addressed_input(
                address,
                RyeOsInputAction::NavigateSelection { delta: 1 }
            )
            .is_empty()
        );
        assert!(core.focused_input.is_none());
    }

    #[test]
    fn addressed_input_rejects_stale_scope_and_edits_the_named_instance() {
        use crate::ui::event::RyeOsInputAction;
        let mut core = test_support_core(session());
        let address = build_view_model(&core)
            .docks
            .bottom
            .unwrap()
            .input
            .unwrap()
            .address;
        // Browser focus may still be on a different region; address, not that
        // focus, decides which admitted input receives the edit.
        core.active_view_set_mut().unwrap().focus_target = None;
        let edit = RyeOsInputAction::SetText {
            text: "exact draft".into(),
            cursor: 11,
        };
        assert!(
            core.dispatch_addressed_input(address.clone(), edit.clone())
                .is_empty()
        );
        assert_eq!(core.focused_input_buffer().unwrap().text, "exact draft");
        for fence in 0..3 {
            // Each independent fence rejects before changing focus or buffers.
            let mut stale = address.clone();
            match fence {
                0 => stale.binding_digest.push('x'),
                1 => stale.session_id.push('x'),
                _ => stale.buffer.view_ref.push('x'),
            }
            assert!(
                core.dispatch_addressed_input(stale, RyeOsInputAction::Submit { interrupt: false })
                    .is_empty()
            );
        }
        let mut stale = address.clone();
        stale.buffer.target_scope = Some("different-route".into());
        core.dispatch_addressed_input(
            stale,
            RyeOsInputAction::SetText {
                text: "wrong".into(),
                cursor: 5,
            },
        );
        assert_eq!(core.focused_input_buffer().unwrap().text, "exact draft");
        core.new_view_set();
        core.dispatch_addressed_input(address.clone(), edit);
        assert_eq!(core.input_buffers.len(), 1);
        assert_eq!(
            core.input_buffers[&address.buffer.storage_key()].text,
            "exact draft"
        );
    }

    fn source_request(effect: &RyeOsEffect) -> Option<(&str, &str, &str, &serde_json::Value)> {
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
    fn cycling_the_filter_field_switches_the_fed_param_and_clears_text() {
        let session = session_with_surface(serde_json::json!({
            "name": "t",
            "tiles": ["view:ryeos/threads/list"],
            "views": { "view:ryeos/threads/list": {
                "widget": "table",
                "sources": { "default": { "ref": "service:ui/ryeos-ui/threads/list", "params": { "sort": "watch" }, "collection": "threads" } },
                "input": { "id": "filter", "feeds": { "fields": [
                    { "param": "status", "label": "status" },
                    { "param": "requested_by", "label": "source" }
                ] } }
            }}
        }));
        let mut core = test_support_core(session);
        let tile = core.active_view_set().unwrap().focused_tile;
        focus_tile(&mut core, tile);
        // Type into the first field (status).
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::InsertInputChar { ch: 'r' },
        });
        assert_eq!(
            core.focused_input_buffer().map(|b| b.text.clone()),
            Some("r".to_string())
        );

        // Tab cycles to the next field (source): the buffer clears and the
        // refetch now feeds requested_by, not status.
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleFilterField { forward: true },
        });
        assert_eq!(
            core.focused_input_buffer().map(|b| b.text.clone()),
            Some(String::new()),
            "switching field clears the prior field's text"
        );
        assert!(
            effects
                .iter()
                .filter_map(source_request)
                .any(|(_, _, _, params)| params.get("requested_by").is_some()
                    && params.get("status").is_none()),
            "cycled fetch feeds requested_by, not status; got {effects:?}"
        );
    }

    #[test]
    fn editing_a_live_filter_resets_the_table_cursor_to_the_top() {
        use crate::ui::view_model::intent_for_focused_row;
        use crate::view_set::ViewLocalState;
        let session = session_with_surface(serde_json::json!({
            "name": "t",
            "tiles": ["view:ryeos/threads/list"],
            "views": {
                "view:ryeos/threads/list": {
                    "widget": "table",
                    "sources": { "default": { "ref": "service:ui/ryeos-ui/threads/list", "params": {}, "collection": "threads" } },
                    "projections": { "columns": [ { "label": "thread", "field": "thread_id" } ] },
                    "selection": { "activate": "watch" },
                    "affordances": [{
                        "id": "watch",
                        "invoke": { "plane": "ui", "facet": "selection", "value": { "thread": "{record.thread_id}" } }
                    }],
                    "input": { "id": "filter", "feeds": { "param": "status" } }
                }
            }
        }));
        let mut core = test_support_core(session);
        let tile = core.active_view_set().unwrap().focused_tile;
        focus_tile(&mut core, tile);
        let tile_key = tile.0.to_string();
        let source_key = crate::ui::source_key::RyeOsSourceInstanceKey::named(
            core.active_view_set().unwrap().tiles[&tile]
                .instance_key
                .clone(),
            "default",
        )
        .encode();
        // A long list the operator has scrolled down into.
        core.data.sources.insert(
            source_key.clone(),
            serde_json::json!({
                "threads": (0..60)
                    .map(|i| serde_json::json!({ "thread_id": format!("T-{i}") }))
                    .collect::<Vec<_>>()
            }),
        );
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetTileCursor {
                tile_id: tile_key,
                index: 50,
            },
        });
        let cursor = |core: &RyeOsCore| match &core
            .active_view_set()
            .unwrap()
            .tiles
            .get(&tile)
            .unwrap()
            .local
        {
            ViewLocalState::GenericList { cursor, .. } => *cursor,
            other => panic!("expected generic-list local, got {other:?}"),
        };
        assert_eq!(cursor(&core), 50);

        // Typing into the live filter narrows the list; the cursor must reset to
        // the top so Enter (activate) hits the first narrowed row, not a no-op
        // pointing past the end. The refetch (and reset) is debounced — the
        // client loop calls refresh_focused_feeds once typing settles.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::InsertInputChar { ch: 'r' },
        });
        let _ = core.refresh_focused_feeds();
        assert_eq!(cursor(&core), 0);

        // With the narrowed result, the reset cursor resolves the first row.
        core.data.sources.insert(
            source_key,
            serde_json::json!({ "threads": [ { "thread_id": "T-only" } ] }),
        );
        match intent_for_focused_row(&core).expect("first narrowed row activates") {
            RyeOsUiIntent::InvokeAffordance { record, .. } => {
                assert_eq!(record["thread_id"], "T-only")
            }
            other => panic!("expected affordance invoke, got {other:?}"),
        }
    }

    #[test]
    fn tab_cycles_input_target_through_new_then_chains() {
        let mut core = test_support_core(writable_session());
        seed_service_route(&mut core);
        // Two conversations in list order (most-recent first): chain B is a
        // single thread; chain A has a follow-up (head T-a2 braids on T-a1).
        core.data.threads = Some(RyeOsThreadsDto {
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
        let route = core.focused_input_route();
        assert_eq!(route.thread, None);
        assert_eq!(route.chain_root, None);

        // Tab → first chain (B, a single thread: head == root).
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.focused_input_route();
        assert_eq!(route.chain_root.as_deref(), Some("T-b1"));
        assert_eq!(route.thread.as_deref(), Some("T-b1"));

        // Tab → chain A, targeting its HEAD (T-a2, the turn nothing continues).
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.focused_input_route();
        assert_eq!(route.chain_root.as_deref(), Some("T-a1"));
        assert_eq!(route.thread.as_deref(), Some("T-a2"));

        // Tab → wraps back to "new conversation".
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.focused_input_route();
        assert_eq!(route.thread, None);
        assert_eq!(route.chain_root, None);

        // Shift+Tab from "new" wraps backward to the last chain (A).
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: false },
        });
        let route = core.focused_input_route();
        assert_eq!(route.chain_root.as_deref(), Some("T-a1"));
        assert_eq!(route.thread.as_deref(), Some("T-a2"));
    }

    #[test]
    fn thread_execution_facts_accessors() {
        let mut core = test_support_core(writable_session());
        core.data.threads = Some(RyeOsThreadsDto {
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
        assert_eq!(core.thread_supports_operator_followup("T-a"), Some(true));
        assert_eq!(
            core.thread_supports_operator_followup("T-g"),
            Some(false),
            "graph is machine-only"
        );
        assert_eq!(core.thread_supports_operator_followup("T-c"), None);
        assert_eq!(core.thread_supports_operator_followup("T-missing"), None);
    }

    #[test]
    fn suspended_follow_parent_gates_the_continuation_predicates() {
        // A suspended parent carries `execution.supports_continuation: true` (a
        // graph continues by machine) yet must report NOT continuation-eligible
        // and NOT operator-followup-eligible while suspended — the instance
        // follow fact vetoes the kind facts.
        let mut core = test_support_core(writable_session());
        core.data.threads = Some(RyeOsThreadsDto {
            threads: vec![serde_json::json!({
                "thread_id": "T-parent",
                "status": "continued",
                "execution": { "supports_continuation": true, "supports_operator_followup": false },
                "follow": { "role": "suspended_parent", "phase": "waiting",
                            "child_chain_root_id": "T-child", "child_terminal_status": null }
            })],
        });
        assert!(core.thread_is_suspended_parent("T-parent"));
        assert_eq!(
            core.thread_supports_operator_followup("T-parent"),
            Some(false),
            "a suspended parent takes no operator input"
        );
    }

    #[test]
    fn suspended_follow_parent_is_never_an_input_target() {
        let mut core = test_support_core(writable_session());
        seed_service_route(&mut core);
        // Two single-thread chains: one is a normal operator-followup directive,
        // the other is a suspended follow-parent (its successor is not yet in the
        // list, so the parent is the chain head).
        core.data.threads = Some(RyeOsThreadsDto {
            threads: vec![
                serde_json::json!({ "thread_id": "T-ok", "chain_root_id": "T-ok",
                    "execution": { "supports_continuation": true, "supports_operator_followup": true } }),
                serde_json::json!({ "thread_id": "T-parent", "chain_root_id": "T-parent",
                    "status": "continued",
                    "execution": { "supports_continuation": true, "supports_operator_followup": false },
                    "follow": { "role": "suspended_parent", "child_chain_root_id": "T-child" } }),
            ],
        });
        let targets = core.input_target_chains();
        assert!(
            targets.iter().any(|(root, _)| root == "T-ok"),
            "the ordinary chain is offered: {targets:?}"
        );
        assert!(
            !targets.iter().any(|(root, _)| root == "T-parent"),
            "the suspended follow-parent is never offered as an input target: {targets:?}"
        );
        // Cycling the input target skips straight past the suspended parent.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(
            core.focused_input_route().chain_root.as_deref(),
            Some("T-ok")
        );
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(
            core.focused_input_route().chain_root,
            None,
            "wraps back to new conversation — the suspended parent was never a slot"
        );
    }

    #[test]
    fn cycle_input_target_excludes_machine_only_chain() {
        let mut core = test_support_core(writable_session());
        seed_service_route(&mut core);
        // Two single-thread chains: one accepts operator follow-up, one is
        // machine-only (a graph — continuation-capable but no operator input).
        // Only the operator-followup chain is a valid input target.
        core.data.threads = Some(RyeOsThreadsDto {
            threads: vec![
                serde_json::json!({ "thread_id": "T-yes", "chain_root_id": "T-yes",
                    "execution": { "supports_continuation": true, "supports_operator_followup": true } }),
                serde_json::json!({ "thread_id": "T-no", "chain_root_id": "T-no",
                    "execution": { "supports_continuation": true, "supports_operator_followup": false } }),
            ],
        });
        // New → the continuation-capable chain (T-no is never offered).
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(
            core.focused_input_route().chain_root.as_deref(),
            Some("T-yes")
        );
        // Forward again → wraps straight back to "new conversation".
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(core.focused_input_route().chain_root, None);
    }

    #[test]
    fn cycle_input_target_is_noop_with_no_chains() {
        let mut core = test_support_core(writable_session());
        seed_service_route(&mut core);
        // No threads fetched yet → only "new conversation" exists.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.focused_input_route();
        assert_eq!(route.thread, None);
        assert_eq!(route.chain_root, None);
    }

    #[test]
    fn cycle_input_target_noop_without_declaration() {
        let mut core = test_support_core(writable_session());
        seed_service_route(&mut core);
        // Replace the input with one that does NOT declare targeting.
        seed_view_value(
            &mut core,
            "view:ryeos/input",
            serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "submit": "route" }
            }),
        );
        core.data.threads = Some(RyeOsThreadsDto {
            threads: vec![serde_json::json!({ "thread_id": "T-x", "chain_root_id": "T-x" })],
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.focused_input_route();
        assert_eq!(route.thread, None, "no declaration → no mutation");
        assert_eq!(route.chain_root, None);
        assert!(
            core.ui.notices.is_empty(),
            "no declaration → silent (no notice)"
        );
    }

    #[test]
    fn cycle_input_target_notices_when_route_has_no_invoke() {
        let mut core = test_support_core(writable_session());
        seed_input_view(&mut core);
        // Route facet with no invoke template at all.
        set_focused_route_value(&mut core, serde_json::json!({ "thread": "T-x" }));
        core.data.threads = Some(RyeOsThreadsDto {
            threads: vec![serde_json::json!({ "thread_id": "T-x", "chain_root_id": "T-x" })],
        });
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        let n = core.ui.notices.len();
        assert!(n > 0, "no invoke → notice, not silent");
        // Deduped: a repeat press doesn't spam an identical notice.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(core.ui.notices.len(), n, "notice deduped on repeat press");
    }

    #[test]
    fn cycle_input_target_dedupes_current_chain_and_prefers_fetched_head() {
        let mut core = test_support_core(writable_session());
        seed_service_route(&mut core);
        // Route already on chain A with a stale head (T-a1).
        set_focused_route_value(
            &mut core,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "thread": "T-a1", "chain_root": "T-a1"
            }),
        );
        // Fetched data shows chain A advanced to head T-a2.
        core.data.threads = Some(RyeOsThreadsDto {
            threads: vec![
                serde_json::json!({ "thread_id": "T-a2", "chain_root_id": "T-a1", "upstream_thread_id": "T-a1" }),
                serde_json::json!({ "thread_id": "T-a1", "chain_root_id": "T-a1" }),
            ],
        });
        // Slots = [New, Chain(A)] — A appears once. Current is A → forward → New.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(core.focused_input_route().chain_root, None);
        // Forward again → chain A using the FETCHED head, not the stale T-a1.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        let route = core.focused_input_route();
        assert_eq!(route.chain_root.as_deref(), Some("T-a1"));
        assert_eq!(
            route.thread.as_deref(),
            Some("T-a2"),
            "prefers fetched head"
        );
    }

    #[test]
    fn cycle_input_target_keeps_synthetic_current_before_refresh() {
        let mut core = test_support_core(writable_session());
        seed_service_route(&mut core);
        // Route aimed at a freshly-launched chain not yet in the thread list.
        set_focused_route_value(
            &mut core,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "thread": "T-new", "chain_root": "T-new"
            }),
        );
        core.data.threads = Some(RyeOsThreadsDto { threads: vec![] }); // refresh not landed
        // Slots = [New, SyntheticChain(T-new)]. The guarantee: you can move
        // AWAY from the unfetched current chain before the refresh lands —
        // forward from the synthetic current reaches "new conversation".
        // (Returning to it relies on the refresh, which lands quickly.)
        assert_eq!(
            core.focused_input_route().chain_root.as_deref(),
            Some("T-new"),
            "starts on the unfetched current chain"
        );
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(
            core.focused_input_route().chain_root,
            None,
            "synthetic current did not trap the cycle — moved to new conversation"
        );
    }

    #[test]
    fn shared_input_cannot_borrow_the_active_surface_route() {
        let mut core = surface_route_core();
        core.focused_input = core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom);
        seed_service_route(&mut core);
        set_focused_input(&mut core, "private shared draft");
        let buffers = core.input_buffers.clone();
        assert!(
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput
            })
            .is_empty()
        );
        assert_eq!(core.input_buffers, buffers);
    }

    #[test]
    fn writable_input_submit_emits_surface_route_coordinate() {
        let mut core = surface_route_core();
        seed_service_route(&mut core);
        set_focused_input(&mut core, "  run this  ");

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SubmitInput,
        });

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::InvokeBinding {
                request: crate::ui::binding::UiBindingRequest {
                    coordinate: crate::ui::binding::UiBindingCoordinate::SurfaceRoute,
                    payload: crate::ui::binding::UiBindingPayload::Input { value, route },
                    ..
                },
                route_seq: Some(_),
                ..
            }) if value == "run this" && route.as_ref().is_some_and(|route| route.thread_id.is_none())
        ));
        // Buffer survives until delivery succeeds.
        assert_eq!(focused_input_text(&core), "  run this  ");
    }

    #[test]
    fn plain_submit_carries_no_interrupt_intent() {
        // Steer is the daemon default, so the wire omits `intent` entirely.
        let mut core = surface_route_core();
        seed_service_route(&mut core);
        set_focused_input(&mut core, "steer me");

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SubmitInput,
        });
        let Some(RyeOsEffectKind::InvokeBinding { request, .. }) = effects.first().map(|e| &e.kind)
        else {
            panic!("expected a bound invocation effect");
        };
        let crate::ui::binding::UiBindingPayload::Input { value, route } = &request.payload else {
            panic!("expected input payload");
        };
        assert_eq!(value, "steer me");
        assert!(!route.as_ref().is_some_and(|route| route.interrupt));
    }

    #[test]
    fn interrupt_submit_sets_interrupt_intent() {
        // Alt+Enter → SubmitInputInterrupt injects intent=interrupt so a running
        // thread cuts its current cognition.
        let mut core = surface_route_core();
        seed_service_route(&mut core);
        set_focused_input(&mut core, "stop, do X");

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SubmitInputInterrupt,
        });
        let Some(RyeOsEffectKind::InvokeBinding { request, .. }) = effects.first().map(|e| &e.kind)
        else {
            panic!("expected a bound invocation effect");
        };
        let crate::ui::binding::UiBindingPayload::Input { value, route } = &request.payload else {
            panic!("expected input payload");
        };
        assert_eq!(value, "stop, do X");
        assert!(route.as_ref().is_some_and(|route| route.interrupt));
    }

    #[test]
    fn submit_without_route_warns_and_emits_nothing() {
        let mut core = test_support_core(writable_session());
        seed_input_view(&mut core);
        set_focused_input(&mut core, "hello");
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SubmitInput,
        });
        assert!(effects.is_empty());
        assert!(
            core.ui
                .notices
                .last()
                .is_some_and(|notice| notice.message.contains("no target"))
        );
    }

    #[test]
    fn launch_does_not_ratchet_a_non_targeting_input() {
        // The ratchet keys off the input's `target` declaration, not the
        // invoke ref: an input that does NOT declare conversation targeting is
        // never retargeted onto the produced thread (no false "continuing").
        let mut core = surface_route_core();
        seed_service_route(&mut core);
        seed_view_value(
            &mut core,
            "view:ryeos/input",
            serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "submit": "route" }
            }),
        );
        set_focused_input(&mut core, "go");
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
                data: Some(serde_json::json!({ "thread_id": "T-9", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.focused_input_route();
        assert_eq!(route.thread, None, "non-targeting input is not retargeted");
        assert_eq!(route.chain_root, None);
    }

    #[test]
    fn ratchet_eligibility_is_captured_at_issue_time_not_result_time() {
        // A targeting input submits → eligibility captured TRUE on the effect.
        // Focus then moves to a non-targeting input before the async result
        // lands. The launch must STILL ratchet (issue-time decision), proving
        // the result handler doesn't recompute from current focus.
        let mut core = surface_route_core();
        seed_service_route(&mut core); // targeting input focused
        set_focused_input(&mut core, "hi");
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("submit effect");

        // Focus moves to a NON-targeting input while the launch is in flight.
        seed_view_value(
            &mut core,
            "view:ryeos/input",
            serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "submit": "route" }
            }),
        );
        assert!(
            core.focused_input_target_cycle().is_none(),
            "focus now resolves a non-targeting input"
        );

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(serde_json::json!({ "thread_id": "T-9", "delivery": "launched" })),
                error: None,
            },
        });
        let route = core.focused_input_route();
        assert_eq!(
            route.thread.as_deref(),
            Some("T-9"),
            "ratcheted on the issue-time decision, not the moved focus"
        );
        assert_eq!(route.chain_root.as_deref(), Some("T-9"));
    }

    #[test]
    fn delayed_submit_result_cannot_mutate_an_inactive_view_set_or_current_input() {
        let mut core = surface_route_core();
        seed_service_route(&mut core);
        set_focused_input(&mut core, "first draft");
        let effect = core
            .dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SubmitInput,
            })
            .pop()
            .expect("first view-set submit");
        let origin = match &effect.kind {
            RyeOsEffectKind::InvokeBinding {
                input_origin: Some(origin),
                ..
            } => origin.clone(),
            other => panic!("route submit must retain its origin, got {other:?}"),
        };

        // A separate Surface tab mounts an ordinary input View. The shared
        // bottom dock remains one mount across tab switches, not a copied dock.
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core.new_view_set();
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.active_surface().unwrap().view_sets().len(), 2);
        let second_tile = mount_fixture_view(&mut core, "view:ryeos/input");
        focus_tile(&mut core, second_tile);
        seed_service_route(&mut core);
        set_focused_route_value(
            &mut core,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "thread": "T-second", "chain_root": "T-second"
            }),
        );
        set_focused_input(&mut core, "second draft");
        let second_instance = core.focused_view_instance_key().unwrap();
        assert_ne!(origin.buffer.view_instance_key, second_instance);

        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(serde_json::json!({
                    "thread_id": "T-first-result",
                    "delivery": "launched"
                })),
                error: None,
            },
        });

        assert_eq!(focused_input_text(&core), "second draft");
        assert_eq!(
            core.route_for_instance(&second_instance).thread.as_deref(),
            Some("T-second")
        );
        assert_eq!(
            core.route_for_instance(&origin.buffer.view_instance_key)
                .thread
                .as_deref(),
            None,
            "an inactive mount is not a live input address for late retargeting"
        );
        assert!(
            core.input_buffers
                .get(&origin.buffer.storage_key())
                .is_some_and(|buffer| buffer.text == "first draft"),
            "inactive source drafts remain intact when a late result is rejected"
        );
    }

    #[test]
    fn feeds_input_drives_its_source_param() {
        let mut core = filter_core();
        let tile_id = core.active_view_set().unwrap().focused_tile.0.to_string();
        // The focused tile declares `input.feeds`, so it owns input.
        assert!(core.has_focused_input());

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetInputText {
                text: "wid".to_string(),
                cursor: 3,
            },
        });

        // Editing a feeds buffer refetches the source with the buffer text
        // injected into the named param.
        let fetch = effects.iter().find_map(source_request);
        let (fetched, view_ref, channel, params) = fetch.expect("feeds edit refetches source");
        assert_eq!(
            fetched,
            crate::ui::source_key::RyeOsSourceInstanceKey::named(
                crate::ids::RyeOsViewInstanceKey::surface_tile(
                    core.presentation.as_ref().unwrap().id(),
                    core.active_surface().unwrap().id(),
                    crate::ids::TileId::new(tile_id.parse().unwrap())
                ),
                "default",
            )
            .encode()
        );
        assert_eq!(
            view_ref, "view:test/filter",
            "the source coordinate names the authored view, never its service ref"
        );
        assert_eq!(channel, "default");
        assert_eq!(params["query"], "wid");
        assert_eq!(params["limit"], 50);
    }

    #[test]
    fn feeds_input_has_no_submit_and_allows_read_only() {
        // `feeds` works in an observation-only binding (no durable write); Enter
        // does nothing.
        let mut core = filter_core();
        for binding in core.binding_attachments.values_mut() {
            binding.descriptor.posture = crate::ui::binding::UiEffectivePosture::ObservationOnly;
        }
        let edit = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::InsertInputChar { ch: 'x' },
        });
        // A live-filter keystroke is debounced: no synchronous refetch.
        assert!(edit.is_empty());
        // The deferred refetch is what the client loop drives; read-only.
        assert!(
            core.refresh_focused_feeds()
                .iter()
                .any(|e| matches!(e.kind, RyeOsEffectKind::FetchSource { .. })),
            "feeds refetch is allowed read-only"
        );
        let submit = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SubmitInput,
        });
        assert!(submit.is_empty());
        // No read-only notice: a feeds input has no submit to block.
        assert!(core.ui.notices.is_empty());
    }

    #[test]
    fn submit_affordance_fires_with_value_payload() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"Palette","tiles":["view:test/palette"],"views":{"view:test/palette":{
                "widget": "text",
                "input": { "id": "line", "submit": "run" },
                "affordances": [{
                    "id": "run",
                    "invoke": { "plane": "rye", "tokens": ["thread", "input"], "args": { "line": "{value}" } }
                }]
            }}
        })));
        let tile_id = core.active_view_set().unwrap().focused_tile;
        focus_tile(&mut core, tile_id);
        set_focused_input(&mut core, "do the thing");

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SubmitInput,
        });

        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::InvokeBinding {
                request: crate::ui::binding::UiBindingRequest {
                    coordinate: crate::ui::binding::UiBindingCoordinate::Affordance { affordance_id, .. },
                    payload: crate::ui::binding::UiBindingPayload::Input { value, route: None },
                    ..
                },
                route_seq: None,
                ..
            }) if affordance_id == "run" && value == "do the thing"
        ));
    }

    #[test]
    fn submit_affordance_blocked_without_compiled_binding() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"Palette","tiles":["view:test/palette"],"views":{"view:test/palette":{
                "widget": "text",
                "input": { "id": "line", "submit": "run" },
                "affordances": [{
                    "id": "run",
                    "invoke": { "plane": "rye", "tokens": ["x"], "args": { "line": "{value}" } }
                }]
            }}
        })));
        let tile_id = core.active_view_set().unwrap().focused_tile;
        focus_tile(&mut core, tile_id);
        set_focused_input(&mut core, "blocked");
        let buffers = core.input_buffers.clone();
        let history = core.seat.events().to_vec();
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .binding_digest
            .clear();
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SubmitInput,
        });
        assert!(effects.is_empty());
        assert!(
            core.focused_input_instance().is_none(),
            "a stale descriptor cannot resolve the input address"
        );
        assert_eq!(core.input_buffers, buffers);
        assert_eq!(core.seat.events(), history);
    }

    #[test]
    fn duplicate_view_instances_have_independent_buffers() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"Independent inputs","view_sets":[{"schema":2,"id":"work","title":"Work","root":{
                "type":"split","axis":"horizontal","ratio":0.5,
                "first":{"type":"group","views":["view:test/filter"],"active":0},
                "second":{"type":"group","views":["view:test/filter"],"active":0}
            }}],"views":{"view:test/filter":{
                "widget": "rows",
                "sources": { "default": { "ref": "service:test/items", "params": {}, "collection": "items" } },
                "input": { "id": "q", "feeds": { "param": "query" } }
            }}
        })));
        let tiles = core.active_view_set().unwrap().tile_ids();
        let first = tiles[0];
        let second = tiles[1];
        assert_ne!(first, second);

        focus_tile(&mut core, first);
        set_focused_input(&mut core, "first-buffer");
        set_focused_route_value(&mut core, serde_json::json!({ "thread": "T-first" }));
        focus_tile(&mut core, second);
        set_focused_input(&mut core, "second-buffer");
        set_focused_route_value(&mut core, serde_json::json!({ "thread": "T-second" }));

        // The same `view:` rendered twice keeps independent buffers and
        // subjects. Its authored ref is content identity, not instance state.
        focus_tile(&mut core, first);
        assert_eq!(focused_input_text(&core), "first-buffer");
        assert_eq!(
            core.focused_input_route().thread.as_deref(),
            Some("T-first")
        );
        focus_tile(&mut core, second);
        assert_eq!(focused_input_text(&core), "second-buffer");
        assert_eq!(
            core.focused_input_route().thread.as_deref(),
            Some("T-second")
        );
    }
}
