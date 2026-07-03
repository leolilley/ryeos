---
category: ryeos/core/protocols
tags: [callbacks, auth, uds, runtime, tokens, capabilities, audit, boundary]
version: "1.0.0"
description: >
  Per-route audit of the UDS runtime-callback surface: every runtime.*
  method, the access tier that gates it, and the exact prelude check it
  passes before its handler runs. Companion to callback-auth.md, which
  covers the token mechanism itself.
---

# UDS Callback Surface Audit

The daemon's Unix-domain-socket RPC dispatcher gates every `runtime.*`
method at the transport boundary — before the handler runs — in
`crates/bin/daemon/src/uds/server.rs::dispatch_runtime_method`
(the prelude, lines ~136-202). This document records which tier gates
each route. It is the route-level companion to
[callback-auth.md](callback-auth.md), which documents the token types,
TTLs, env injection, and revocation.

Invariant: no `runtime.*` handler executes without first clearing its
tier's prelude check. A chain read never widens into a chain write; an
exact-thread token never acts on another thread.

## The four tiers

| Tier | Prelude check | Selector | server.rs |
|---|---|---|---|
| thread-auth | `thread_auth.validate(tat, thread_id)`; handler re-validates the callback token and derives principal/provenance/caps from server-side state | `is_thread_auth_method` | validate at ~:149; selector ~:278-283 |
| two-proof | `thread_auth.validate(tat, thread_id)` **and** `callback_tokens.validate_token_and_thread(token, thread_id)` — both proofs required | literal match on the method | ~:159-179 |
| chain-read | `callback_tokens.validate_token_only(token)` then `authorize_chain_read` (cap thread and target must share a chain root) | `is_chain_read_method` | ~:180-187; selector ~:287-292; authz ~:298-329 |
| exact-thread write | `callback_tokens.validate_token_and_thread(token, thread_id)` — the token's thread must equal the target thread | else branch (default) | ~:188-202 |

## Per-route disposition

| Method | Tier | Notes |
|---|---|---|
| `runtime.dispatch_action` | thread-auth | Handler enforces `effective_caps` before the schema loop; empty caps = deny-all. |
| `runtime.spawn_follow_child` | thread-auth | Same prelude as dispatch_action; admission checks in the handler. |
| `runtime.poll_input` | two-proof | Durable `cognition_in` write for a running thread; both proofs required. |
| `runtime.author_item` | two-proof | Validated `ThreadAuthState` is retained and passed to the author service, which additionally requires LiveFs provenance + path-traversal checks. |
| `runtime.get_thread` | chain-read | Rehydrate a predecessor within the same chain. |
| `runtime.replay_events` | chain-read | Accepts `thread_id` or `chain_root_id`; both resolve to a chain root. |
| `runtime.get_thread_events` | chain-read | Alias of the replay handler. |
| `runtime.append_event` | exact-thread write | |
| `runtime.append_events` | exact-thread write | Batch append. |
| `runtime.bundle_events_append` | exact-thread write | Handler receives the capability and enforces the bundle scope. |
| `runtime.bundle_events_read_chain` | exact-thread write | A *read* by name, but gated exact-thread — not a chain read — and bundle-scoped in the service. |
| `runtime.bundle_events_scan` | exact-thread write | Bundle-scoped in the service. |
| `runtime.vault_put` / `vault_get` / `vault_delete` / `vault_list` | exact-thread write | Vault refs rejected on bundle mismatch against the token's `effective_bundle_id`. |
| `runtime.finalize_thread` | exact-thread write | |
| `runtime.mark_running` | exact-thread write | |
| `runtime.request_continuation` | exact-thread write | |
| `runtime.publish_artifact` | exact-thread write | |
| `runtime.get_facets` | exact-thread write | |
| `runtime.submit_command` / `claim_commands` / `complete_command` | exact-thread write | |
| `runtime.attach_process` | exact-thread write | The runtime self-reports only its pid; the process group is always derived daemon-side (`pgid_of`, server.rs ~:348) — never trusted from the runtime. |

## Deeper (post-prelude) checks

The prelude authenticates the token to the route; the service layer then
authorizes the *content* of the request:

- **Empty `effective_caps` = deny-all**, enforced at `dispatch_action`
  (runtime_dispatch) and again at the service layer.
- **Bundle scope**: vault and bundle-event references are rejected when
  they do not match the token's `effective_bundle_id`
  (`ryeos_app::runtime_vault_service`, `ryeos_app::bundle_event_service`).
- **author_item provenance**: `ryeos_app::runtime_item_author_service`
  requires LiveFs provenance and applies path-traversal checks.

## Ungated / local-control surface

Only `system.health` is ungated. `lifecycle.status` and
`lifecycle.shutdown` are local UDS control methods on the bare namespace
(no `runtime.` prefix, no token) and carry no thread authority. Every
other bare-namespace method returns `unknown_method`.

## Boundary guards

The tier boundaries are pinned by unit tests in
`crates/bin/daemon/src/uds/server.rs` (`mod tests`):

- `successor_token_can_read_predecessor_in_chain` — a chain-read is
  accepted for a predecessor in the token's own chain.
- `token_cannot_read_another_chain` — the same read is rejected against a
  thread in a different chain.
- `successor_token_cannot_write_predecessor` — an exact-thread write
  route rejects a token whose thread differs from the target `thread_id`.
- `dispatch_action_without_thread_auth_token_is_rejected`,
  `dispatch_action_with_wrong_thread_auth_token_is_rejected`,
  `spawn_follow_child_rejects_*` — the thread-auth prelude fails closed.
