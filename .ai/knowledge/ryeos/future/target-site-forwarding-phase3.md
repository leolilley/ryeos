<!-- rye:signed:2026-05-24T14:55:07Z:5cc091417f5dcdf7495aef242497e48998ae451a8992a16490faccc93eb9221d:PBnU1VEG-7iJWqdppiaRPZoL7bEhDnZMMOJBAxIHSYtx7IBa4rEuwtJ6WmHAQNw-MykAHAjzxcHP97KgLN0lCQ:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: target-site-forwarding-phase3
title: Target-Site Forwarding Future Advanced Paths
entry_type: implementation_guide
version: "1.1.0"
author: amp
created_at: 2026-05-25T00:00:00Z
updated_at: 2026-05-25T00:00:00Z
description: Future-only reference for optional advanced target-site forwarding work to revisit if product needs require it.
tags:
  - target-site-forwarding
  - remote-execute
  - execute-mode
  - future-work
  - advanced-paths
```

# Target-Site Forwarding Future Advanced Paths

## Purpose

This note is **not** a record of the Phase 2/3 implementation that already landed.

It captures future advanced paths that were intentionally deferred. Come back to this only if a product need appears that exceeds the current unary target-site forwarding contract.

Current completed baseline, for orientation only:

- Phase 2 shared unary helper: `7309fbd6 Refine unary remote forwarding helper`
- Phase 3 unary `/execute` forwarding: `315caa99 Wire target-site unary forwarding`

The completed baseline is intentionally narrow:

- unary inline only
- local preflight before remote I/O
- remote target resolved by strict `site_id`
- no-project uses `NO_PROJECT_SENTINEL`
- project execution requires explicit full-project binding
- no remote `operation` / `inputs`
- no streaming, detached, mirror-thread, scheduler, or remote-to-remote behavior

The future paths below should be considered only when those constraints become insufficient.

## Future path 1: rich unary parity

### When to do this

Implement this path if target-site users need remote unary execution to support richer `/execute` features while preserving local daemon authority.

Concrete triggers:

- remote target-site execution needs `operation` and `inputs`
- clients need `validate_only + target_site_id` to validate more than local composition
- clients need remote `/execute` structured error details preserved rather than wrapped
- target-site unary responses need richer local/remote provenance metadata

### Scope

This path extends the existing unary forwarding flow without adding streaming or detached execution.

Likely work:

1. Extract reusable local op/input preflight.
   - Validate requested op exists for the resolved item/kind.
   - Validate required inputs and input types.
   - Apply defaults if local dispatch would apply them.
   - Run this before push or remote execute.

2. Allow forwarding of `operation` and `inputs` only after preflight passes.
   - `RemoteClient::execute_with_options()` already has the transport shape.
   - Do not silently drop either field.
   - Do not let the remote become the first place op/input validity is discovered.

3. Define `validate_only + target_site_id` semantics.
   - Option A: keep it local-only but return an explicit target-site-aware validation response.
   - Option B: perform remote reachability/config validation without remote execution.
   - Option C: remote dry-run if the remote daemon grows a safe validate-only contract.
   - Avoid a behavior where validation unexpectedly pushes or mutates remote state.

4. Preserve or translate remote structured errors more precisely.
   - Detect structured remote `/execute` error payloads.
   - Preserve stable `code` fields where possible.
   - Wrap with target-site context without hiding the remote cause.
   - Keep local preflight errors distinguishable from remote execution errors.

5. Add response provenance if needed.
   - Examples: `target_site_id`, remote config key, pushed snapshot hash, result snapshot hash, pull summary.
   - Decide whether this belongs in the normal `/execute` result or a debug/metadata envelope.

### Guardrails

- Local daemon must still authorize the original caller before forwarding.
- Local preflight must still run before remote I/O.
- Remote target failures must not be reported as local descriptor validation failures.
- `target_site_id` must still not be forwarded to the remote `/execute` body, to avoid loops.
- Do not add lower-crate extraction unless multiple API surfaces truly need the same logic.

### Tests to add

- remote `operation` passes only after local op preflight succeeds
- bad op fails locally before push
- bad inputs fail locally before push
- defaults are applied consistently with local dispatch
- remote structured error payload is preserved/wrapped predictably
- `validate_only + target_site_id` has a pinned response contract
- loop prevention remains true: forwarded request body has no `target_site_id`

## Future path 2: non-unary / long-running forwarding

### When to do this

Implement this path if target-site execution needs interactive, streaming, detached, or scheduled behavior.

Concrete triggers:

- remote target-site execution must stream events back to the caller
- detached remote target execution is required
- local UI needs to show a remote thread as if it were local
- scheduled jobs need target-site placement
- fleet or capability-based site selection becomes product-facing
- remote-to-remote forwarding is explicitly required

### Scope

This path is larger than rich unary parity. It changes the execution model from “one local request pushes, waits, pulls” to “local daemon brokers or records a remote run over time.”

Likely work:

1. Streaming bridge.
   - Start remote execution in a streaming mode.
   - Subscribe to remote events.
   - Re-emit them through the local route/event stream contract.
   - Preserve ordering, terminal events, keepalives, and reconnect semantics.

2. Mirror thread model.
   - Decide whether the local daemon creates a mirror thread ID.
   - Store remote thread ID, target site, pushed snapshot, result snapshot, and last mirrored event.
   - Decide whether cancellation/resume are local commands forwarded to the remote or local-only views.

3. Detached target-site execution.
   - Define when pull-back happens: on remote completion, explicit sync, resume, or user action.
   - Define conflict behavior if local files change while detached remote work runs.
   - Persist enough metadata for recovery after local daemon restart.

4. WebSocket or frontend transport, if needed.
   - This should remain a frontend transport layer on top of daemon event semantics.
   - Do not make WebSocket the core forwarding primitive.

5. Scheduler and fleet placement.
   - Add policy for selecting a site from capabilities, load, labels, or task metadata.
   - Keep explicit `target_site_id` resolution strict.
   - Avoid fallback to arbitrary sites unless policy explicitly says so.

6. Remote-to-remote forwarding, only if explicitly required.
   - Current loop prevention intentionally makes forwarded requests execute locally on the target.
   - Any multi-hop design needs hop limits, origin tracking, and clear trust semantics.

### Guardrails

- Do not weaken strict site identity requirements.
- Do not infer a remote site from a name, URL, or path when `site_id` lookup fails.
- Do not use local project paths as remote paths without an explicit binding.
- Keep conflict detection and snapshot lineage checks mandatory.
- Make cancellation/resume semantics explicit before implementing detached mode.
- Avoid remote-to-remote chains until there is a real product requirement.

### Tests to add

- remote stream events are re-emitted locally with stable ordering
- reconnect/replay behavior works across a dropped client connection
- local mirror thread records remote terminal state
- cancellation reaches the remote and local mirror records it
- detached remote completion can pull results safely
- local conflict during delayed pull-back is actionable and does not overwrite files
- daemon restart can recover mirror metadata
- scheduler never silently falls back to an unrequested site

## Keep deferring unless needed

These remain out of scope until a concrete need appears:

- lower-crate extraction of forwarding
- exact parity for every remote structured error variant
- browser/WebSocket UX before daemon event semantics are stable
- remote-to-remote forwarding
- automatic site selection without explicit scheduler policy
- speculative compatibility for old remote configs

## Decision rule

Before starting either path, write down the product need in one sentence.

If the need is “run this item remotely and get the result back,” the completed unary target-site path is enough.

If the need is “run a richer unary op remotely with local validation parity,” use Future path 1.

If the need is “watch, resume, cancel, schedule, or mirror a remote run over time,” use Future path 2.
