<!-- ryeos:signed:2026-07-16T10:54:56Z:2728e996c06f802fb1335054bac5e1c0419303c7524e88829c5d0a4e19841e77:3Lj51TOYzoo+zyAC6L7WexsHHpmPttwesmPkLSwPjaguEQLUhpBXYIibtmTR6XtXWUHyBWEZLfjmh0NygQjkDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [callbacks, auth, uds, runtime, tokens, capabilities, audit, boundary]
version: "1.1.0"
description: >
  Per-route audit of the UDS runtime-callback surface: every runtime.*
  method, the access tier that gates it, and the exact prelude check it
  passes before its handler runs. Companion to callback-auth.md, which
  covers the token mechanism itself.
---

# UDS Callback Surface Audit

The daemon's Unix-domain-socket RPC dispatcher gates every `runtime.*`
method at the transport boundary — before the handler runs — in
`crates/bin/daemon/src/uds/server.rs::dispatch_runtime_method`. This document
records which tier gates each route. It is the route-level companion to
[callback-auth.md](callback-auth.md), which documents the token types,
TTLs, env injection, and revocation.

Invariant: no `runtime.*` handler executes without first clearing its
tier's prelude check. A chain read never widens into a chain write; an
exact-thread token never acts on another thread.

## The four tiers

| Tier | Prelude check | Selector |
|---|---|---|
| thread-auth | `thread_auth.validate(tat, thread_id)`; handler re-validates the callback token and derives principal/provenance/caps from server-side state | `is_thread_auth_method` |
| two-proof | `thread_auth.validate(tat, thread_id)` **and** `callback_tokens.validate_token_and_thread(token, thread_id)` — both proofs required | literal match on the method |
| chain-read | `callback_tokens.validate_token_only(token)` then `authorize_chain_read` (cap thread and target must share a chain root) | `is_chain_read_method` |
| exact-thread | `callback_tokens.validate_token_and_thread(token, thread_id)` — the token's thread must equal the target thread | else branch (default) |

Authentication is followed by a lifecycle admission check under the
authoritative state-store lock. Authoring and sensitive reads require the
thread to be Running, without durable stop intent, while the daemon shutdown
gate is open. Stop completion retains only the narrow claim/complete/finalize
surface. Terminal state immediately revokes both callback credential classes;
requests already past token validation still meet the locked lifecycle check.

## Per-route disposition

| Method | Tier | Notes |
|---|---|---|
| `runtime.dispatch_action` | thread-auth | Handler enforces `effective_caps` before the schema loop; empty caps = deny-all. |
| `runtime.spawn_follow_child` | thread-auth | Same prelude as dispatch_action; admission checks in the handler. |
| `runtime.poll_input` | two-proof | Durable `cognition_in` write for a running thread; both proofs required. |
| `runtime.author_item` | two-proof | Validated `ThreadAuthState` is retained and passed to the author service, which additionally requires LiveFs provenance + path-traversal checks. |
| `runtime.get_thread` | chain-read | Rehydrate a predecessor within the same chain. Returns the slim thread + result shape; artifacts and facets are not embedded. |
| `runtime.replay_events` | chain-read | Accepts `thread_id` or `chain_root_id`; both resolve to a chain root. |
| `runtime.get_thread_events` | chain-read | Alias of the replay handler. |
| `runtime.append_event` | exact-thread write | |
| `runtime.append_events` | exact-thread write | Batch append. |
| `runtime.bundle_events_append` | exact-thread write | Handler receives the capability and enforces the bundle scope. |
| `runtime.bundle_events_read_chain` | exact-thread | A *read* by name, but gated to the exact executing thread — not a chain-wide token — and bundle-scoped in the service. |
| `runtime.bundle_events_scan` | exact-thread | Bundle-scoped in the service. |
| `runtime.bundle_events_materialize_attachment` | exact-thread write | Requires bundle-event scan authority; reads a retained CAS attachment and atomically writes the caller-selected project-relative destination without following symlinks. |
| `runtime.vault_put` / `vault_get` / `vault_delete` / `vault_list` | exact-thread write | Vault refs rejected on bundle mismatch against the token's `effective_bundle_id`. List uses an exclusive lexical cursor, defaults to 64 keys, and accepts at most 128. |
| `runtime.finalize_thread` | exact-thread write | |
| `runtime.mark_running` | exact-thread write | |
| `runtime.request_continuation` | exact-thread write | |
| `runtime.publish_artifact` | exact-thread write | |
| `runtime.get_facets` | exact-thread write | |
| `runtime.submit_command` / `claim_commands` / `complete_command` | exact-thread write | |
| `runtime.attach_process` | exact-thread | The runtime reports only its own PID. It must equal the accepted socket's kernel `SO_PEERCRED` PID; `SO_PEERPIDFD` pins that exact incarnation, then the daemon derives and records the target/group birth tuple. Runtime-supplied PGIDs are never accepted. |

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

## Ungated / local-status surface

Only `system.health` and read-only `lifecycle.status` are ungated bare methods.
They carry no thread authority. There is no UDS shutdown method: sandboxed
runtimes may receive this socket, so local lifecycle control uses a
kernel-authenticated socket peer pidfd and OS signals. Every other
bare-namespace method returns `unknown_method`.

The transport caps frames and responses at 10 MiB, holds a 32 MiB aggregate
in-flight request budget, limits the server to 32 connections, and times out
frame I/O. Runtime thread-event replay is capped at 32 records and a 6 MiB
conservative serialized page; bundle-event reads are capped at 16 records and
8 MiB of serialized records. These service-level cursors and byte budgets
prevent valid small requests from materializing unbounded event histories
before response framing.

Runtime-vault list responses are independently capped at 64 KiB and return
`{namespace, keys, next_cursor}`. Its cursor bounds service response
materialization only: the current sealed backend opens and validates the whole
bounded vault map (at most 1,024 entries, 256-byte physical keys, 256 KiB
values, 4 MiB plaintext, and a 6 MiB sealed envelope) before choosing a page.
Narrow per-scope storage reads require the deferred sharded/scoped backend.

During coordinated shutdown the listener stops accepting immediately and idle
persistent streams exit before reading another frame. A request that already
decoded owns its frame-memory permit and any peer pidfd in an independent task,
so it can finish while connection tasks drain under the daemon's shared
deadline. If that deadline forces the wire server to abort, the admitted owner
remains fenced by closed process admission and the exact-identity process drain;
dropping a socket waiter cannot orphan a spawned workload.

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
