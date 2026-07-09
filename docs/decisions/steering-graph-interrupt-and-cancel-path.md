# Steering decisions: graph interrupt & the one cancel path

Decision record for the two steering-maturity items in
`.tmp/implementation/05-frontend-buildout.md` §5c.2 and §5c.3. Written on the
`steering` branch (Wave 2). These are design decisions, not a feature; the code
that enacts them is small and lives beside this doc.

## 1. Graph interrupt semantics (§5c.2)

**Decision: a graph declares `supports_interrupt: false`.** The graph kind
profile withholds interrupt; no graph offers an interrupt control.

### Why (a), not a walker interrupt contract

`kind_profiles::allowed_actions(kind, status, has_process)`
(`crates/daemon/ryeos-app/src/kind_profiles.rs`) adds `interrupt` to a running
thread's allowed actions only when its profile has `supports_interrupt: true`.
`service:commands/submit` then gates on that list: an action not in
`allowed_actions` is refused (`command_service.rs`).

The directive runtime honors an interrupt: SIGUSR1 seals the partial cognition
as `cognition_out { interrupted: true }`, refunds the turn, and folds any queued
input (the braid even renders an `interrupted` seam). **The graph walker has no
equivalent.** A running graph is a segment-walk over a frontier; there is no
in-flight signal it catches and reseals as a partial turn, and
`crates/runtimes/graph/` implements no interrupt handling at all. Declaring
`supports_interrupt: true` (the prior value) therefore advertised a capability
the walker could not service: `allowed_actions` would offer `interrupt`,
`commands/submit` would accept it, and the command would reach a walker with
undefined behavior. That is precisely the "don't ship an interrupt button that
signals a walker with undefined behavior" hazard the spec names.

A full walker interrupt contract (checkpoint-and-reseal on signal) is a real
runtime feature, not a flag flip, and nothing depends on it today. Withholding
the flag is the honest, safe default and can be revisited if a graph ever needs
mid-walk interruption.

### What enforces it

- **Daemon (authoritative):** with `supports_interrupt: false`,
  `allowed_actions(graph_run, running, …)` omits `interrupt`, so
  `commands/submit` refuses an `interrupt` command on a graph thread. This is
  the enforcement point — it holds for every caller (TUI, web, CLI, other
  nodes), not just the ryeos-ui.
- **Client:** there is nothing to grey out, because the ryeos-ui never surfaces a
  command-style interrupt control. The only "interrupt" in the ryeos-ui is
  Alt+Enter (`SubmitInputInterrupt`) — a *text-bearing live redirect* through
  `service:threads/input`, not a `commands/submit` control command — and it is
  already gated on `supports_operator_followup`, which a graph declares `false`
  (`reducer/input.rs`), so a graph never accepts that either. The base client's
  `ExecutionFacts` (`dto.rs`) carry `supports_continuation` and
  `supports_operator_followup` but no `supports_interrupt`, precisely because no
  client control keys off it.

To stop a graph, use **cancel** (or **kill** when it has a process). Those stay
in `allowed_actions` for a running graph.

## 2. One cancel path for the ryeos-ui (§5c.3)

**Decision: the ryeos-ui's operator-facing cancel affordance routes through
`service:commands/submit` with `command_type: cancel`.** That is the single
audited cancel path.

Three cancel services exist:

- `service:threads/cancel` (core) — the raw cancel; CLI / delegate target.
- `service:ui/ryeos-ui/thread/cancel` (ryeos-ui) — a daemon-only shim that delegates
  to `threads/cancel` under the verified session principal.
- `service:commands/submit { command_type: cancel }` (standard) — the command
  channel: per-thread **ownership check** (`ctx.require_owner`) **and a durable
  command record** (audit), plus the `allowed_actions` state-machine gate.

`commands/submit` is the winner: it is the only one that both checks ownership
and writes an audit record, and it is the same channel the operator uses for
`continue` / `kill`. Routing cancel there makes the four control verbs one
mechanism.

### What changed

- `bundles/ryeos-ui/.ai/views/ryeos/threads/list.yaml` — the row `cancel`
  affordance now invokes `service:commands/submit` with
  `{ thread_id: "{record.thread_id}", command_type: cancel }` instead of
  `service:ui/ryeos-ui/thread/cancel`. It rides the generic Service-invoke path
  (`AffordanceInvoke::Service` → `RyeOsEffectKind::Invoke { intent: Service }`),
  so args reach the daemon as `/execute` parameters and target the row, not the
  route head. The authored `notice:` template resolves against the returned
  `CommandRecord` (which carries `thread_id`). This is the exact shape the
  base-restructure test `service_ref_affordance_emits_execute_invoke_with_row_args`
  already asserts.
- Guard test `shipped_threads_list_cancel_uses_the_single_command_submit_path`
  (`reducer/affordances.rs`) parses the shipped list.yaml and asserts the cancel
  affordance targets `commands/submit` with `command_type: cancel`, and that the
  killed service refs appear in no affordance.

The ryeos-ui launcher steering item "Cancel thread" already routes through
`commands/submit` (via the typed `RyeOsEffectKind::SubmitThreadCommand`), so the
operator-facing affordance and the launcher now share the one path.

### The other routes' remaining use sites

- `service:threads/cancel` stays as the raw core cancel (CLI `thread cancel`,
  and the delegate target of the ryeos-ui shim). Not a ryeos-ui affordance.
- `service:ui/ryeos-ui/thread/cancel` is no longer a client effect target. The
  client reducer rejects the removed `cancel_thread` effect wire form; operator
  cancel flows through `SubmitThreadCommand { command_type: "cancel" }` and the
  shared command-submit path.

## Republish

Both YAML edits invalidate signatures and ride the single Wave 3 republish:

- `bundles/standard/.ai/node/engine/kinds/graph/graph.kind-schema.yaml`
- `bundles/ryeos-ui/.ai/views/ryeos/threads/list.yaml`

This decision record is a repo doc; it needs no signing or republish.
