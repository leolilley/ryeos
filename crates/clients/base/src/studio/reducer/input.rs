use super::effect::{StudioEffect, StudioEffectKind};
use super::event::StudioFilterField;
use super::model::StudioCore;
use super::view_model::StudioTone;
use super::parse_tile_id;

impl StudioCore {
    /// Refetch the focused instance's source when its input declares
    /// `feeds` (the buffer is a writer of one source param). Debounce is a
    /// renderer/transport concern; the reducer emits the refetch and the
    /// binding carries `debounce_ms` for the renderer to honour.
    /// Whether the focused input is a live filter (feeds a source, no submit).
    pub(crate) fn focused_input_is_live_filter(&self) -> bool {
        self.focused_input_instance()
            .and_then(|(_, view_ref)| self.views.get(&view_ref))
            .and_then(|binding| binding.input.as_ref())
            .is_some_and(|input| input.is_live_filter())
    }

    /// Feed-refetch effects for a buffer edit, EXCEPT for a live filter — those
    /// are debounced by the client loop (which calls [`Self::refresh_focused_feeds`]
    /// on its tick) so typing a filter doesn't block on a daemon round-trip per
    /// keystroke. The edit itself already applied; this only defers the fetch.
    pub(crate) fn feeds_effects_unless_live_filter(&mut self, live_filter: bool) -> Vec<StudioEffect> {
        if live_filter {
            Vec::new()
        } else {
            self.effects_for_focused_feeds()
        }
    }

    /// Public entry for the debounced feed refetch: re-derives the focused
    /// input's source fetch from the current buffer (and resets the table
    /// cursor). The client loop calls this once typing settles.
    pub fn refresh_focused_feeds(&mut self) -> Vec<StudioEffect> {
        self.effects_for_focused_feeds()
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
    pub(crate) fn submit_focused_input(&mut self, interrupt: bool) -> Vec<StudioEffect> {
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
        // explicit `Some(false)`. A suspended follow-parent is never
        // continuation-eligible while suspended: the daemon owns its resume via
        // the followed child, so the instance follow fact vetoes the kind fact.
        let facts: super::dto::ExecutionFacts =
            serde_json::from_value(row.get("execution")?.clone()).ok()?;
        Some(facts.supports_continuation && !row_is_suspended_parent(row))
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
        // A suspended follow-parent takes no operator input regardless of kind
        // policy — its resume successor (not it) is the live target. Gating here
        // means every consumer of this predicate (the input-target list, the
        // foot-input "continuing" label, the Continue launcher item) excludes a
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
            .and_then(|(_, view_ref)| self.views.get(&view_ref))
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
    pub(crate) fn cycle_filter_field(&mut self, forward: bool) -> Vec<StudioEffect> {
        let Some((key, view_ref)) = self.focused_input_instance() else {
            return Vec::new();
        };
        let count = self
            .views
            .get(&view_ref)
            .and_then(|binding| binding.input.as_ref())
            .and_then(|input| input.feeds.as_ref())
            .map(|feeds| feeds.field_count())
            .unwrap_or(0);
        if count < 2 {
            return Vec::new();
        }
        let buffer = self.ui.input_buffers.entry(key.storage_key()).or_default();
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
    pub(crate) fn cycle_input_target(&mut self, forward: bool) -> Vec<StudioEffect> {
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
    pub(crate) fn submit_route(&mut self, text: &str, interrupt: bool) -> Vec<StudioEffect> {
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
                    intent: super::effect::InvokeIntent::Launch,
                    success_notice: None,
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
                            intent: super::effect::InvokeIntent::Launch,
                            success_notice: None,
                            route_seq,
                            ratchet_on_thread_id,
                        })]
                    }
                    super::seat::InvokeTemplate::Command { mut tokens } => {
                        tokens.push(plain);
                        vec![self.emit(StudioEffectKind::Invoke {
                            target: super::effect::InvokeRef::Tokens { tokens },
                            params: serde_json::json!({}),
                            intent: super::effect::InvokeIntent::Launch,
                            success_notice: None,
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
                drill,
            }) => {
                let effects = self.apply_ui_affordance(facet, value, merge, open_view, drill);
                self.clear_focused_input();
                effects
            }
            Some(super::content::AffordanceInvoke::Rye {
                tokens,
                args,
                notice,
            }) => {
                vec![self.emit(StudioEffectKind::Invoke {
                    target: super::effect::InvokeRef::Tokens { tokens },
                    params: args,
                    intent: super::effect::InvokeIntent::Service,
                    success_notice: notice,
                    route_seq: None,
                    ratchet_on_thread_id: false,
                })]
            }
            Some(super::content::AffordanceInvoke::Service {
                item_ref,
                args,
                notice,
            }) => {
                vec![self.emit(StudioEffectKind::Invoke {
                    target: super::effect::InvokeRef::Ref { item_ref },
                    params: args,
                    intent: super::effect::InvokeIntent::Service,
                    success_notice: notice,
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

    pub(crate) fn clear_focused_input(&mut self) {
        if let Some(buffer) = self.focused_input_buffer_mut() {
            buffer.clear();
        }
    }

    pub(crate) fn set_tile_filter(
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
    use crate::studio::reducer::test_support::*;

    #[test]
    fn cycling_the_filter_field_switches_the_fed_param_and_clears_text() {
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/threads/list"],
                "views": { "view:ryeos/threads/list": {
                    "widget": "table",
                    "source": { "ref": "service:ui/studio/threads/list", "params": { "sort": "watch" }, "collection": "threads" },
                    "input": { "id": "filter", "feeds": { "fields": [
                        { "param": "status", "label": "status" },
                        { "param": "requested_by", "label": "source" }
                    ] } }
                }}
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = StudioCore::new(session, BrowserViewport::default(), 0);
        // Type into the first field (status).
        core.dispatch(StudioEvent::Ui { event: StudioUiEvent::InsertInputChar { ch: 'r' } });
        assert_eq!(
            core.focused_input_buffer().map(|b| b.text.clone()),
            Some("r".to_string())
        );

        // Tab cycles to the next field (source): the buffer clears and the
        // refetch now feeds requested_by, not status.
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleFilterField { forward: true },
        });
        assert_eq!(
            core.focused_input_buffer().map(|b| b.text.clone()),
            Some(String::new()),
            "switching field clears the prior field's text"
        );
        assert!(
            effects.iter().any(|e| matches!(&e.kind,
                StudioEffectKind::FetchSource { params, .. }
                    if params.get("requested_by").is_some() && params.get("status").is_none())),
            "cycled fetch feeds requested_by, not status; got {effects:?}"
        );
    }

    #[test]
    fn editing_a_live_filter_resets_the_table_cursor_to_the_top() {
        use crate::studio::view_model::action_for_focused_row;
        use crate::workspace::ViewLocalState;
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/threads/list"],
                "views": {
                    "view:ryeos/threads/list": {
                        "widget": "table",
                        "source": { "ref": "service:ui/studio/threads/list", "params": {}, "collection": "threads" },
                        "projections": { "columns": [ { "label": "thread", "field": "thread_id" } ] },
                        "selection": { "activate": "watch" },
                        "affordances": [{
                            "id": "watch",
                            "invoke": { "plane": "ui", "facet": "selection", "value": { "thread": "{record.thread_id}" } }
                        }],
                        "input": { "id": "filter", "feeds": { "param": "status" } }
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = StudioCore::new(session, BrowserViewport::default(), 0);
        let tile = core.workspace.focused_tile;
        let key = tile.0.to_string();
        // A long list the operator has scrolled down into.
        core.data.sources.insert(
            key.clone(),
            serde_json::json!({
                "threads": (0..60)
                    .map(|i| serde_json::json!({ "thread_id": format!("T-{i}") }))
                    .collect::<Vec<_>>()
            }),
        );
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetTileCursor { tile_id: key.clone(), index: 50 },
        });
        let cursor = |core: &StudioCore| match &core.workspace.tiles.get(&tile).unwrap().local {
            ViewLocalState::GenericList { cursor, .. } => *cursor,
            other => panic!("expected generic-list local, got {other:?}"),
        };
        assert_eq!(cursor(&core), 50);

        // Typing into the live filter narrows the list; the cursor must reset to
        // the top so Enter (activate) hits the first narrowed row, not a no-op
        // pointing past the end. The refetch (and reset) is debounced — the
        // client loop calls refresh_focused_feeds once typing settles.
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::InsertInputChar { ch: 'r' },
        });
        let _ = core.refresh_focused_feeds();
        assert_eq!(cursor(&core), 0);

        // With the narrowed result, the reset cursor resolves the first row.
        core.data.sources.insert(
            key.clone(),
            serde_json::json!({ "threads": [ { "thread_id": "T-only" } ] }),
        );
        match action_for_focused_row(&core).expect("first narrowed row activates") {
            StudioAction::InvokeAffordance { record, .. } => {
                assert_eq!(record["thread_id"], "T-only")
            }
            other => panic!("expected affordance invoke, got {other:?}"),
        }
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
    fn suspended_follow_parent_gates_the_continuation_predicates() {
        // A suspended parent carries `execution.supports_continuation: true` (a
        // graph continues by machine) yet must report NOT continuation-eligible
        // and NOT operator-followup-eligible while suspended — the instance
        // follow fact vetoes the kind facts.
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.data.threads = Some(StudioThreadsDto {
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
            core.thread_supports_continuation("T-parent"),
            Some(false),
            "a suspended parent is not continuation-eligible while suspended"
        );
        assert_eq!(
            core.thread_supports_operator_followup("T-parent"),
            Some(false),
            "a suspended parent takes no operator input"
        );
    }

    #[test]
    fn suspended_follow_parent_is_never_an_input_target() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_service_route(&mut core);
        // Two single-thread chains: one is a normal operator-followup directive,
        // the other is a suspended follow-parent (its successor is not yet in the
        // list, so the parent is the chain head).
        core.data.threads = Some(StudioThreadsDto {
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
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(core.seat.fold().input_route().chain_root.as_deref(), Some("T-ok"));
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::CycleInputTarget { forward: true },
        });
        assert_eq!(
            core.seat.fold().input_route().chain_root,
            None,
            "wraps back to new conversation — the suspended parent was never a slot"
        );
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
        // A live-filter keystroke is debounced: no synchronous refetch.
        assert!(edit.is_empty());
        // The deferred refetch is what the client loop drives; read-only.
        assert!(
            core.refresh_focused_feeds()
                .iter()
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
}
