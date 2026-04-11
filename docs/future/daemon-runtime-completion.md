```yaml
id: daemon-runtime-completion
title: "Daemon Runtime Completion"
description: Remaining work to make ryeosd a fully capable production runtime — command delivery, process supervision, auth enforcement, remote forwarding, and MCP unification.
category: future
tags: [ryeosd, daemon, runtime, commands, supervision, auth, remote, mcp]
version: "0.1.0"
status: planned
```

# Daemon Runtime Completion

> **Status:** Planned — these are the concrete gaps between the current inline-capable daemon and a fully supervised, multi-node production runtime.

> **Prerequisite:** [Rust Engine Rewrite](rust-engine-rewrite.md) resolves the process ownership issue that blocks several items below.

---

## What Works Today

The daemon is the sole authority for thread lifecycle, events, budgets, and execution on a single local node:

- **Inline execution** — `POST /execute` blocks and returns result
- **Detached execution** — `POST /execute` with `launch_mode=detached` returns immediately, background task completes
- **Thread lifecycle** — create, running, finalize, continuation with successor threads and `continued` edges
- **Event store** — append, replay, SSE streaming with replay-to-live handoff
- **Kill safety** — PGID check prevents daemon self-kill
- **Sibling modules** — CAS, push/refs, vault, webhooks, registry all native Rust
- **Auth middleware** — Ed25519 signed-request verification (disabled by default)
- **CLI cutover** — all CLI verbs use daemon HTTP, not direct ExecuteTool
- **Bootstrap** — two-phase init with stable public identity documents

---

## What's Deferred

### 1. Runtime Command Delivery

**Status:** 501 for cancel/interrupt/continue

**Problem:** Commands are queued in the database but no runtime polls `commands.claim` during execution. The Python runtime runs synchronously inside the PyO3 bridge — there is no concurrent command polling loop.

**What's needed:**

The directive runtime needs a command polling loop that runs concurrently with execution. During a directive run, the runtime should periodically call `commands.claim` via UDS RPC and act on received commands:

- **cancel** — cooperative shutdown, finalize as `cancelled`
- **interrupt** — save state, request continuation, finalize as `continued`
- **continue** — (submitted to a terminal thread) triggers a new continuation

This requires the Python runtime to be restructured from a single synchronous `execute_item()` call into a loop that interleaves execution steps with command polling. The natural implementation:

```python
while not done:
    # Check for pending commands
    commands = lifecycle.claim_commands(thread_id, timeout_ms=0)
    for cmd in commands:
        if cmd["command_type"] == "cancel":
            return _cancelled()
        if cmd["command_type"] == "interrupt":
            return _interrupted_with_continuation()

    # Execute next step
    done = execute_next_step()
```

**Trigger:** When directive runtimes need external interruption — e.g., user cancels a long-running agent from the TUI, or budget enforcement needs to stop a runaway thread.

**Effort:** M — restructure directive runtime loop, add command polling, test cancel/interrupt flows.

---

### 2. Process Supervision

**Status:** Shared PGID (daemon's own), safety valve prevents damage

**Problem:** All threads share the daemon's PGID because Python executes in-process via PyO3. This means:

- `kill` is blocked by the safety valve (can't kill the daemon's own group)
- Restart reconciliation can't distinguish "this thread's process is dead" from "the daemon is alive"
- No true process isolation between concurrent executions

**Resolution path:** The [Rust Engine Rewrite](rust-engine-rewrite.md) eliminates this entirely. When the engine is native Rust, execution dispatches through Lillux which spawns isolated subprocesses with their own PGIDs. Kill, reconciliation, and supervision all work naturally.

**Interim option:** Worker process per execution (documented in `.tmp/ryeosd-v3/16-hardening.md` Workstream 1) could provide distinct PGIDs before the engine rewrite. This was deferred because it's throwaway scaffolding — the worker process boundary gets collapsed back into the daemon when the Rust engine lands.

**Trigger:** When kill/supervision semantics are needed in production before the Rust engine rewrite.

---

### 3. Auth Scope Enforcement

**Status:** Scopes loaded from authorized key files but not checked against endpoints

**Problem:** The auth middleware verifies signatures and loads scopes, but never checks whether the authenticated principal's scopes permit the requested operation. Every authenticated request has full access.

**What's needed:**

A scope-checking layer after authentication:

```rust
// In auth middleware, after verify_request succeeds:
let required_scope = scope_for_path(request.uri().path());
if !principal.scopes.contains(&"*".to_string())
    && !principal.scopes.iter().any(|s| scope_matches(s, required_scope))
{
    return Err("insufficient scope");
}
```

Scope mapping:

| Endpoint pattern           | Required scope    |
| -------------------------- | ----------------- |
| `POST /execute`            | `execute`         |
| `GET /threads/*`           | `threads.read`    |
| `POST /threads/*/commands` | `threads.command` |
| `POST /vault/*`            | `vault`           |
| `POST /registry/*`         | `registry.write`  |
| `GET /registry/*`          | `registry.read`   |
| `*` (catch-all)            | `*`               |

**Trigger:** When `require_auth=true` is enabled in production and multiple principals with different access levels need to coexist.

**Effort:** S — add scope map, check in middleware, test with scoped keys.

---

### 4. Remote Execution Forwarding

**Status:** `target_site_id` tags thread metadata but executes locally

**Problem:** When a client passes `target_site_id: "site:remote-node"`, the daemon records it on the thread but still executes locally. There is no HTTP forwarding to another node.

**What's needed:**

When `target_site_id` differs from the local node's site ID:

1. Resolve the target node's URL from configuration or discovery
2. Forward the `/execute` request to the target node's daemon
3. Create a local mirror thread with `mirrored_from` edge
4. Stream events from the remote node's SSE endpoint to the local event store
5. Finalize the local mirror when the remote thread completes

```rust
if request.target_site_id != state.current_site_id {
    let remote_url = state.config.resolve_remote(target_site_id)?;
    return forward_to_remote(remote_url, request).await;
}
```

**Dependencies:**

- Remote node discovery — either static config or registry-based
- Cross-node auth — the local node's signing key must be authorized on the remote
- Event correlation — `chain_root_id` shared across nodes, `site_id` disambiguates
- See [Cluster Bootstrap](cluster-bootstrap.md) for fleet enrollment

**Trigger:** When multi-node execution is needed — e.g., dispatching GPU work to a remote inference node.

**Effort:** L — node discovery, request forwarding, event mirroring, cross-node auth.

---

### 5. MCP Daemon Unification

**Status:** `ryeos-mcp` embeds the Python engine directly, bypassing the daemon

**Problem:** MCP-triggered executions don't create daemon threads, don't emit daemon events, aren't visible in the TUI, and don't participate in budget tracking. Two execution paths exist — daemon and direct engine.

**What's needed:**

Convert `ryeos-mcp` from an engine-embedding server to a daemon client:

| MCP Method       | Daemon Call                                       |
| ---------------- | ------------------------------------------------- |
| `tools/call`     | `POST /execute` → create thread, return result    |
| `tools/list`     | `GET /status` → map capabilities to MCP tools     |
| `resources/list` | `GET /threads` → expose threads as MCP resources  |
| `resources/read` | `GET /threads/{id}/events` → thread event history |

The MCP server becomes stateless — it translates JSON-RPC ↔ daemon HTTP. It doesn't import the engine, doesn't open SQLite, doesn't manage threads.

**Lifecycle:** The MCP client (Amp) manages the MCP server process. The MCP server connects to ryeosd on startup (auto-starts it if not running). The MCP server is ephemeral; the daemon is persistent.

**Trigger:** When MCP-triggered executions need to be visible in the TUI and participate in the unified thread/event model.

**Effort:** M — HTTP client in ryeos-mcp, tool list from status, execute routing, event streaming.

---

### 6. Budget Enforcement

**Status:** Budgets are bookkeeping only — no enforcement

**Problem:** Root budgets record `max_spend` in metadata. Child budgets have `reserved_spend` and `actual_spend`. But nothing actually stops execution when spend exceeds the budget. The runtime doesn't check budget during execution, and the daemon doesn't reject executions that would exceed available budget.

**What's needed:**

Two enforcement points:

1. **Pre-execution check** — before creating a child thread, verify parent budget has sufficient unreserved balance
2. **Runtime enforcement** — during execution, the runtime periodically reports spend via `budgets.report`; when `actual_spend >= max_spend`, the daemon submits a `cancel` command (requires command delivery, item 1 above)

**Dependencies:** Runtime command delivery (item 1) for mid-execution enforcement.

**Trigger:** When uncontrolled spend becomes a production concern — e.g., runaway directive loops burning API credits.

**Effort:** M — pre-execution check is S, runtime enforcement depends on command delivery.

---

### 7. Replay Protection Persistence

**Status:** In-memory replay guard only

**Problem:** The replay guard in `auth.rs` uses an in-memory `HashMap` that resets on daemon restart. An attacker could replay a captured request after a daemon restart.

**What's needed:**

Persist seen nonces to SQLite:

```sql
CREATE TABLE replay_nonces (
    fingerprint TEXT NOT NULL,
    nonce TEXT NOT NULL,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (fingerprint, nonce)
);
```

Prune entries older than `TIMESTAMP_MAX_AGE_SECS` (300s) on startup and periodically.

**Trigger:** When `require_auth=true` is enabled in a network-exposed deployment.

**Effort:** S — one table, insert on verify, prune on startup.

---

## Priority Order

For **single-node local use** (current target):

1. ~~Safety valves~~ ✅ Done
2. ~~SQLite hardening~~ ✅ Done
3. ~~UDS hardening~~ ✅ Done
4. ~~Config/bootstrap~~ ✅ Done
5. ~~Continuation from completion~~ ✅ Done
6. ~~Detached execution~~ ✅ Done
7. **Command delivery** — next, enables cancel/interrupt
8. **Budget enforcement** — after command delivery
9. **Auth scope enforcement** — when multi-principal needed

For **multi-node deployment**:

10. **Remote forwarding** — requires cluster bootstrap
11. **MCP unification** — single execution path
12. **Replay protection persistence** — network-exposed auth
13. **Rust engine rewrite** — eliminates Python dependency, enables true supervision

---

## Relationship to Other Documents

| Document                                                    | Relationship                                                  |
| ----------------------------------------------------------- | ------------------------------------------------------------- |
| [Rust Engine Rewrite](rust-engine-rewrite.md)               | Resolves process ownership, enables true supervision and kill |
| [Cluster Bootstrap](cluster-bootstrap.md)                   | Fleet enrollment for remote forwarding                        |
| [Sovereign Inference](sovereign-inference.md)               | Remote GPU dispatch depends on forwarding                     |
| [Mission Control](mission-control.md)                       | TUI depends on MCP unification for visibility                 |
| [Execution Graph Scheduling](execution-graph-scheduling.md) | Graph walkers depend on budget enforcement                    |
