# Execute-Only Architecture

**Date:** 2026-04-23  
**Status:** Draft  
**Prerequisite:** [Tracing Implementation Plan](./TRACING-IMPLEMENTATION-PLAN.md)

---

## 1. Problem

The daemon currently has two code paths:

1. **Direct state access** — `/threads`, `/chains`, `/events`, `/public-key` hit `ThreadLifecycleService` and `StateStore` directly, bypassing the engine entirely. These are purpose-built HTTP handlers that know the domain model.

2. **Full engine pipeline** — `/execute` goes through resolve → verify → trust → build_plan → spawn. This is the generic path for directives and tools.

Two paths means two auth patterns, two tracing stories, two places to maintain, and the daemon has to know about domain concepts (threads, chains, events) as first-class HTTP endpoints. Adding a new capability means writing a handler, wiring it into the router, adding auth checks — all outside the engine's item system.

---

## 2. Solution

Collapse the external HTTP API to a single endpoint:

```
POST /execute
GET  /health    ← connectivity check, no engine involvement
```

Everything else — listing threads, getting chain events, submitting commands, fetching public keys — becomes an item execution. The caller says "execute this item" and the daemon runs the engine pipeline. No special cases.

---

## 3. Current API → Execute Mapping

### 3.1 Endpoints That Become Item Executions

| Current Endpoint | Item to Execute | Notes |
|---|---|---|
| `GET /status` | `tool:rye/status` | Daemon status info |
| `GET /public-key` | `tool:rye/public_key` | Node identity public key |
| `GET /threads` | `tool:rye/list_threads` | List all threads |
| `GET /threads/:id` | `tool:rye/get_thread` | Get single thread detail |
| `GET /threads/:id/children` | `tool:rye/list_children` | List child threads |
| `GET /threads/:id/chain` | `tool:rye/get_chain` | Get chain events |
| `POST /threads/:id/commands` | `tool:rye/submit_command` | Submit command to thread |
| `GET /threads/:id/events` | `tool:rye/get_events` | Get thread events |
| `GET /threads/:id/events/stream` | `tool:rye/stream_events` | Stream thread events (SSE) |
| `GET /chains/:id/events` | `tool:rye/get_chain_events` | Get chain events |
| `GET /chains/:id/events/stream` | `tool:rye/stream_chain_events` | Stream chain events (SSE) |
| `POST /execute` | *(unchanged)* | Already the generic path |

### 3.2 Endpoints That Stay

| Endpoint | Why |
|---|---|
| `GET /health` | Pure connectivity check. No engine, no state, no auth. Returns 200 if the daemon is alive. |

### 3.3 Internal Endpoints (Move to UDS Only)

| Current Endpoint | Purpose | New Home |
|---|---|---|
| `POST /runtime/{method}` | Subprocess callbacks from spawned runtimes | UDS only (already partially there) |

The `/runtime/{method}` endpoint is used by `ryeos-directive-runtime` and `ryeos-graph-runtime` to call back into the daemon during execution. These are internal — they should never be exposed on the HTTP port. Move entirely to UDS.

---

## 4. The Execute Request

Uniform for all operations:

```json
POST /execute
{
  "item_id": "tool:rye/list_threads",
  "project_path": "/home/leo/project",
  "params": {}
}
```

```json
POST /execute
{
  "item_id": "directive:my/workflow",
  "project_path": "/home/leo/project",
  "params": { "niche": "plumbers" }
}
```

```json
POST /execute
{
  "item_id": "tool:rye/stream_events",
  "project_path": "/home/leo/project",
  "params": { "thread_id": "abc123" }
}
```

Every request has the same shape: `item_id`, `project_path`, `params`. The engine resolves what kind of item it is (tool, directive, graph), builds the appropriate plan, and executes it.

---

## 5. No Builtins

This is the key design constraint: **there are no builtin tools.** Every operation — including state queries — is an item that lives in the `.ai/` directory as a regular tool or directive.

`tool:rye/list_threads` is not a special case hardcoded into the daemon. It's a tool definition at `.ai/tools/rye/list_threads` (a Python script, a Rust binary, whatever the item format is). It goes through the exact same engine pipeline as `directive:my/workflow` or `tool:rye/bash/bash`.

### 5.1 How State Query Tools Access the Daemon

Tools need to read the daemon's state store (threads, chains, events). They do this through the daemon's **UDS RPC interface** — the same internal API that `ryeos-directive-runtime` and `ryeos-graph-runtime` already use for callbacks.

```
1. Client sends: POST /execute { item_id: "tool:rye/list_threads" }
2. Daemon: resolve → verify → trust → build_plan
3. Plan: DispatchSubprocess { script: "rye/list_threads", ... }
4. Daemon spawns tool as child process
5. Tool calls daemon over UDS: { "method": "threads.list", "params": {} }
6. Daemon queries StateStore, returns result
7. Tool formats and returns result to daemon
8. Daemon returns HTTP response
```

The tool is a separate process that uses the daemon's API. The daemon is a generic execution engine + state store. It doesn't need to know what `list_threads` means.

### 5.2 Implications

- The UDS RPC interface becomes the **public API for tools and directives**. It needs to be stable and well-documented.
- New capabilities (new tools that access state) are added by writing a tool, not by modifying the daemon.
- The daemon's HTTP surface is just `/execute` + `/health`. Adding new operations never requires daemon changes.

---

## 6. PlanNode: No Changes Needed

The current plan IR is sufficient:

```
enum PlanNode {
    DispatchSubprocess { ... }  ← run a binary (ALL items use this)
    SpawnChild { ... }          ← fork a child thread
    Complete { ... }            ← terminal node
}
```

No `Inline` variant. No builtin fast path. Every item is a `DispatchSubprocess` — the engine spawns a child process, the process runs, the daemon collects the result. Uniform.

The `Inline` launch mode still exists as a scheduling hint (run synchronously vs detach), but the plan node type is always `DispatchSubprocess`.

---

## 7. Daemon Architecture After Collapse

### 7.1 Before

```
ryeosd
  ├── HTTP API (many endpoints)
  │     ├── /threads, /chains, /events    ← direct StateStore access
  │     ├── /execute                      ← engine pipeline
  │     └── /runtime/{method}             ← subprocess callbacks
  ├── UDS RPC
  │     ├── commands
  │     ├── attach_process
  │     └── runtime callbacks
  ├── StateStore (CAS-backed)
  ├── Engine (resolve/verify/trust/build_plan/spawn)
  └── Process Manager (PGID tracking, kill, reconcile)
```

Two code paths into state. HTTP handlers that know the domain model. Authentication split across two patterns.

### 7.2 After

```
ryeosd
  ├── HTTP API
  │     ├── POST /execute                  ← everything
  │     └── GET /health                    ← connectivity
  ├── UDS RPC
  │     ├── threads.*                      ← state queries (tools call this)
  │     ├── chains.*                       ← state queries (tools call this)
  │     ├── events.*                       ← state queries (tools call this)
  │     ├── commands.*                     ← command submission (tools call this)
  │     ├── runtime.*                      ← subprocess callbacks
  │     └── attach_process                 ← subprocess self-registration
  ├── StateStore (CAS-backed)
  ├── Engine (resolve/verify/trust/build_plan/spawn)
  └── Process Manager (PGID tracking, kill, reconcile)
```

One code path into everything. The daemon is a **pure execution engine + state store + process manager**. It has no domain-specific HTTP handlers. It doesn't know what "list threads" means. It just executes items.

### 7.3 Process Orchestration (unchanged)

The daemon still spawns `ryeos-graph-runtime` and `ryeos-directive-runtime` as child processes. It still tracks PGIDs, kills on shutdown, and reconciles orphans on restart. The `/runtime/{method}` callback just moves from HTTP to UDS.

---

## 8. UDS RPC Surface

The UDS interface becomes the primary API for all state operations. Tools and directives use it to interact with the daemon. This is the existing interface, but its role changes: from "internal daemon plumbing" to "the API that everything uses."

### 8.1 Existing UDS Methods (keep as-is)

These already exist and are used by spawned runtimes:

| Method | Purpose |
|---|---|
| `threads.attach_process` | Subprocess self-registration (PID/PGID) |
| `commands.submit` | Submit a command to a thread |
| `commands.claim` | Claim pending commands |
| `commands.complete` | Mark command as complete |
| `runtime.*` | Runtime callbacks (dispatch_action, etc.) |

### 8.2 New UDS Methods Needed

These currently live as HTTP handlers. They need UDS equivalents so tools can call them:

| Method | Purpose |
|---|---|
| `threads.list` | List threads (with optional filters) |
| `threads.get` | Get single thread detail |
| `threads.list_children` | List child threads |
| `threads.get_chain` | Get chain for a thread |
| `events.get` | Get events for a thread or chain |
| `events.stream` | Subscribe to event stream |
| `status.get` | Daemon status |
| `identity.public_key` | Node identity public key |

### 8.3 Auth Model

UDS calls from spawned subprocesses are already authenticated via callback tokens. For tool processes spawned by `/execute`, the same mechanism applies — the daemon injects a callback token into the subprocess environment, and the subprocess presents it on each UDS call.

No new auth mechanism needed. Same model, just more methods behind it.

---

## 9. Streaming

Event streaming is the trickiest part. Currently the daemon has SSE endpoints (`/events/stream`, `/chains/:id/events/stream`) that hold HTTP connections open.

With the collapsed architecture, streaming works like this:

```
1. Client sends: POST /execute { item_id: "tool:rye/stream_events", params: { thread_id: "..." } }
2. Daemon spawns tool as child process
3. Tool opens UDS connection, subscribes to event stream via events.stream
4. Tool receives events from daemon over UDS
5. Tool writes events to stdout (the daemon captures subprocess stdout)
6. Daemon forwards subprocess stdout to the HTTP response as SSE
```

The tool is a streaming bridge: it subscribes to the daemon's UDS event stream and pipes events to its stdout. The daemon captures subprocess stdout and forwards it as the HTTP response body.

This keeps the streaming model simple: the daemon doesn't need to know about SSE. It just captures subprocess output. The tool handles the formatting.

---

## 10. Tracing Impact

This is where the collapse pays off massively for observability. See [Tracing Implementation Plan](./TRACING-IMPLEMENTATION-PLAN.md) for the full tracing spec.

### 10.1 Before: Two Tracing Stories

```
# Direct state access (no spans, flat events)
INFO GET /threads
INFO listing threads: count=42

# Engine execution (would have spans, once instrumented)
INFO directive resolved: id=my-directive
INFO loading tool: name=rye/bash/bash
```

Different shapes, different fields, no correlation.

### 10.2 After: One Tracing Story

```
# Everything is an execution — same span hierarchy
execute { item_id: "tool:rye/list_threads", project: "/foo" }  ← root span
  └── engine.resolve { item_id: "rye/list_threads" }
  └── engine.verify
  └── engine.trust
  └── engine.build_plan
  └── subprocess:spawn { runtime: "tool", pid: 5678 }
  └── subprocess:wait { pid: 5678, exit: Ok, elapsed: 2ms }
  └── execute.complete { status: "ok" }
```

State queries get the same span hierarchy as directive executions. The trace tells you the full story regardless of what was executed.

### 10.3 Cross-Process Trace Propagation

For spawned processes (tools, directives, runtimes), the daemon propagates the trace context:

```
# Parent (ryeosd)
execute { item_id: "directive:my/workflow" }
  └── subprocess:spawn { pid: 1234 }
        [trace_id propagated via env var to child process]
        └── directive:execute { item_id: "..." }               ← child process
              └── provider:http { url: "..." }
              └── tool:execute { name: "rye/bash/bash" }
  └── subprocess:wait { pid: 1234, exit: Ok }
```

The daemon injects a trace ID (and optional parent span ID) into the subprocess environment. The child process's `ryeos-tracing` subscriber picks it up, so the child's spans are logically nested under the parent's `subprocess:spawn` span.

This works for ALL subprocesses — tools, directive-runtime, graph-runtime — because they all go through the same `DispatchSubprocess` path.

---

## 11. Overhead Consideration

A state query like `list_threads` currently takes a direct path through `StateStore::list_threads()`. Under the collapsed architecture, it goes through resolve → verify → trust → build_plan → spawn subprocess → UDS call → StateStore::list_threads() → return.

In Rust, the resolve/verify/trust/build_plan path is microseconds. The subprocess spawn is the dominant cost — fork + exec + process startup. For a local UDS call, this adds single-digit milliseconds of latency.

This overhead is acceptable because:
- The network latency of the HTTP request already dominates
- The architectural simplicity (one code path, one auth model, one tracing story) is worth more than shaving a few milliseconds off state queries
- If it ever becomes a problem, we optimize the subprocess spawn (pre-forked process pool, keep-alive workers) — but that's a future concern, not a design constraint

---

## 12. Migration Path

### Phase 1: UDS API Expansion

1. Add the missing UDS methods (`threads.list`, `threads.get`, `events.get`, `events.stream`, etc.)
2. These are thin wrappers around the existing `ThreadLifecycleService` and `StateStore` methods
3. Add auth via callback tokens for all new methods
4. Test: existing UDS methods still work, new methods return correct data

### Phase 2: Tool Implementations

1. Create tool definitions for each current HTTP endpoint (`tool:rye/list_threads`, `tool:rye/get_thread`, etc.)
2. Each tool is a process that calls the daemon's UDS API and formats the result
3. These can be Python scripts, shell scripts, or small binaries — whatever the item format supports
4. Test: each tool produces the same output as the current HTTP endpoint

### Phase 3: Collapse HTTP API

1. Replace all HTTP handlers (except `/execute` and `/health`) with redirects or 410 Gone
2. `/execute` already handles the generic case — no changes needed to the execute handler itself
3. Move `/runtime/{method}` to UDS-only (remove HTTP route)
4. Test: all operations work through `POST /execute`

### Phase 4: Cleanup

1. Remove dead code: HTTP handler modules (`api/threads.rs`, `api/events.rs`, `api/commands.rs`, `api/runtime_callback.rs`)
2. Remove the multi-endpoint router setup in `main.rs`
3. Remove any auth middleware that was endpoint-specific
4. Test: daemon still starts, `/execute` works, `/health` works, nothing else accessible

### Phase 5: Tracing (see Tracing Implementation Plan)

1. Add `ryeos-tracing` crate
2. Instrument the execution pipeline
3. Implement cross-process trace propagation
4. Add trace-capture tests

---

## 13. What Changes

| Component | Before | After |
|---|---|---|
| HTTP endpoints | 14 routes | 2 routes (`/execute`, `/health`) |
| Code paths into state | 2 (direct + engine) | 1 (engine only) |
| Auth patterns | 2 (HTTP middleware + callback tokens) | 1 (callback tokens) |
| Domain knowledge in daemon | High (knows threads, chains, events) | Zero (generic execution engine) |
| Adding a new capability | Write handler + wire route + add auth | Write a tool |
| PlanNode variants | 3 (`DispatchSubprocess`, `SpawnChild`, `Complete`) | 3 (unchanged) |

## 14. What Doesn't Change

- **PlanNode IR** — no new variants needed
- **Engine pipeline** — resolve/verify/trust/build_plan unchanged
- **Process management** — PGID tracking, kill, reconcile unchanged
- **StateStore** — CAS-backed state, chains, projections unchanged
- **Subprocess spawning** — `DispatchSubprocess` unchanged
- **UDS RPC** — existing methods unchanged, just new methods added
- **lillux** — no tracing, no changes

---

## Appendix A: Dependency Graph (Unchanged)

```
                    lillux          ryeos-tracing  ← new (from tracing plan)
                   /  /  \         /  /  /  \  \
                  /  /    \       /  /  /    \  \
    ryeos-engine  ryeos-state  ryeos-runtime  (all → lillux + ryeos-tracing)
          \            \           /     \
           \            \         /       \
            ryeos-tools (→ engine + state)  ryeos-graph-runtime (→ runtime)
                                          ryeos-directive-runtime (→ runtime)
                  \          /
                   \        /
                    ryeosd  ← orchestrator
                    (→ engine + state + lillux + ryeos-tracing)
```

The dependency graph doesn't change. The collapse is an API-layer change, not a crate-structure change.

## Appendix B: Execute Request Schema

```rust
#[derive(Deserialize)]
pub struct ExecuteRequest {
    /// The item to execute (tool:*, directive:*, or graph:*)
    pub item_id: String,

    /// Project path containing the .ai/ directory
    pub project_path: String,

    /// Parameters passed to the item
    #[serde(default)]
    pub params: serde_json::Value,
}
```

## Appendix C: Execute Response Schema

```rust
#[derive(Serialize)]
pub struct ExecuteResponse {
    /// Execution status
    pub status: String,  // "ok" | "error"

    /// The result data from the item
    pub result: Option<serde_json::Value>,

    /// Error details if status != "ok"
    pub error: Option<String>,

    /// Thread ID if a thread was created
    pub thread_id: Option<String>,

    /// Execution metadata
    pub metadata: ExecuteMetadata,
}

#[derive(Serialize)]
pub struct ExecuteMetadata {
    pub elapsed_ms: u64,
    pub item_id: String,
    pub item_kind: String,  // "tool" | "directive" | "graph"
}
```
