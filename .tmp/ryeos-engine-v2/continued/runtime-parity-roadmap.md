# Runtime Parity Roadmap

Post-CAS/GC work. The daemon (ryeosd) is becoming the node — it needs
to auto-resolve project snapshots, vault secrets, and kind schemas the
same way ryeos-node does today. The directive LLM loop and graph walker
are being **fully rewritten in Rust** as compiled binaries dispatched
by the engine via Lillux.

No backward-compat shim. No Python fallback track. No CLI rewrite
(deferred to separate Rust CLI plan at `.tmp/nl-cli-plan-clean/`).

---

## 1. Daemon-Node Convergence + Bundle Access — L (2–3d)

The daemon's current execution path is live-FS only: take a
`project_path`, resolve items against it, execute against it. This
works for local development.

The node (ryeos-node) has a second path: pushed projects in CAS.
It looks up the HEAD ref, checks out from CAS into an execution
space, builds an `ExecutionContext` pointing at that checkout, then
resolves and executes against it. See `_execute_from_head` in
`ryeos-node/ryeos_node/server.py`.

The daemon needs both paths. The work is porting the node's
CAS-backed execution pipeline into the daemon as a second code path,
not redesigning the existing live-FS path.

### 1a. Port `_execute_from_head` into the daemon

Add a CAS-backed execution path to `ryeosd/src/execution/runner.rs`:

```
1. Receive (acting_principal, project_path, item_ref)
2. Look up project ref for (acting_principal, project_path)
3. If ref found:
   - Get HEAD snapshot hash from ref
   - CAS checkout into execution space
   - Resolve and execute against checkout dir
   - Fold back changes after execution
4. If no ref:
   - Use live FS project_path directly (current behavior, unchanged)
   - No checkout, no fold-back
```

The node's `ExecutionContext` (`ryeos/rye/utils/execution_context.py`)
is the model: immutable container for per-execution paths (project
root, user space, signing key dir, system spaces). No globals, no
env-var fallbacks.

### 1b. Fix principal bug in ref lookup

**Current bug**: `prepare_cas_context` uses `state.identity.fingerprint()`
(the daemon's own key) for project ref lookup. Project refs are
per-principal — must use the **acting principal's** fingerprint.

Fix: pass `acting_principal` through to ref lookup instead of using
the daemon identity.

### 1c. Fix trust bootstrap path disconnection

**Current bug**: `bootstrap.rs` seeds trust at
`state_dir/trust/trusted_keys/`, but `engine_init.rs` loads trust
from `.ai/config/keys/trusted/` under system roots. These are
different paths — bootstrap trust doesn't participate in engine
verification.

**Fix**: make bootstrap write to the system root's
`.ai/config/keys/trusted/`. One canonical path, no dual scanning.

### 1d. Bundle discovery contract

Define how the daemon discovers installed bundles. This must land
before the execution path changes so kind schemas and trust are
loadable from the right places.

- Support **multiple system roots/bundle roots**, not just one
  `system_data_dir` — roots are an **ordered list**, first match wins
  for kind schema conflicts
- Load kind schemas from all discovered bundle roots
- Load trust from `.ai/config/keys/trusted/` across project > user >
  all system roots (in order)
- **Core + standard**: installed as system roots, always available
- **Node-only bundles** (code, email, web): installed on disk,
  discovered via configured bundle roots
- Validate bundle roots exist and are readable at bootstrap/startup

Bootstrap (`bootstrap.rs`) should persist `system_data_dir` and
bundle roots in the config. The default config writer should include
`system_data_dir` so kind loading doesn't depend on implicit defaults.

### 1e. Structured secret declaration format

Tools declare what secrets they need via `required_secrets` in tool
YAML/frontmatter. The metadata parser is extended to support array
values for this field. The daemon resolves them for the acting
principal and injects as `RYE_VAULT_*` env vars.

This must land **before** removing caller-supplied `vault_keys` from
the API (1f), otherwise tools lose secrets.

The daemon resolves secrets **per executable item**, not as a blanket
request-level bag. This prevents over-injection — directives/graphs
don't get secrets they don't need.

Rules:

- Items declare secret IDs only (e.g. `["openai-api-key"]`)
- Daemon resolves secrets for the acting principal
- Daemon injects only canonical env vars (`RYE_VAULT_{NAME}`)
- Items cannot choose arbitrary env var names
- Missing required secrets fail fast before execution starts

### 1f. Remove `vault_keys` and `project_snapshot_hash` from `/execute` API

**File**: `ryeosd/src/api/execute.rs`

Remove both fields from `ExecuteRequest`. The daemon resolves them
internally — snapshot from project refs, vault from tool declarations.

**Transition**: keep both fields temporarily as deprecated on
`ExecuteRequest` — deserialize but ignore values, log a warning if
present. This makes silent breakage visible. Delete fields entirely
in a follow-up.

`validate_only` must use the **same root-selection logic** as real
execution (CAS checkout when a ref exists, live FS otherwise). It
performs resolution, authorization, and required-secret checks but
skips spawn and fold-back.

### 1g. Migrate webhook binding `vault_keys`

**Files**: `ryeosd/src/webhooks.rs`, `ryeosd/src/api/webhooks.rs`

Current webhook bindings store `vault_keys` — needs migration to
the `required_secrets` model from 1e.

- Old bindings with `vault_keys` must still **deserialize** (don't
  break persisted state on upgrade)
- Daemon **ignores** the field on read — execution uses only tool
  `required_secrets`
- Rewritten storage **omits** the field
- Remove `vault_keys` from `CreateWebhookRequest` (new bindings
  can't set it)

---

## 2. Native Runtime Rewrite — XL (multi-week, centerpiece)

Rewrite the Python directive LLM loop (`thread_directive.py`, ~930 lines)
and graph walker (`walker.py`, ~2860 lines) as compiled Rust binaries.
Full plan at `docs/future/native-runtimes.md`.

Both are **data-driven tools** dispatched by the engine as subprocesses.
They receive params on stdin, produce JSON on stdout, and call back to
ryeosd via HTTP/UDS. The Rust replacements maintain this exact contract —
**no engine or daemon changes required**.

### Architecture (unchanged)

```
Agents / Clients
    │
    ▼  MCP tool call (rye_execute)
ryeosd                     [RUST]  (thread lifecycle, events, CAS, auth)
    │
    ▼  resolve → verify → build_plan → dispatch (Lillux subprocess)
rye_engine                 [RUST]  (resolution, trust, plan building)
    │
    ▼  fork/exec
directive-runtime          [RUST binary]  ← NEW (replaces thread_directive.py)
graph-runtime              [RUST binary]  ← NEW (replaces walker.py)
    │
    ▼  HTTP/UDS callbacks
ryeosd                     (execute, fetch, sign, thread lifecycle)
```

### Workspace changes

Add three crates to `Cargo.toml` workspace members:

```toml
members = ["ryeosd", "rye_engine", "lillux/lillux", "rye_runtime", "directive-runtime", "graph-runtime"]
```

### Callback surface freeze

The Rust runtimes call back to ryeosd via these existing paths only —
no new daemon APIs are needed:

- `threads.create` / `threads.attach_process` / `threads.mark_running` / `threads.finalize`
- `threads.set_facets` / `threads.get_facets`
- `events.append` / `events.append_batch` / `events.replay`
- `commands.claim` / `commands.complete`
- `budgets.reserve` / `budgets.report` / `budgets.release`
- `POST /execute` (tool dispatch from within runtimes)
- `rye_fetch` / `rye_sign` (Python-direct, called via daemon relay)

### Phase 2a: `rye_runtime` — shared library crate

Build bottom-up. Each module is a port of existing Python with identical
semantics.

| Module             | Ports                                                  | Dependencies       |
| ------------------ | ------------------------------------------------------ | ------------------ |
| `condition.rs`     | `condition_evaluator.py` — pure functions, no I/O      | none               |
| `interpolation.rs` | `interpolation.py` — `${state.foo}` templates          | none               |
| `permissions.rs`   | capability fnmatch, `check_permission`, `attenuate`    | `glob`             |
| `client.rs`        | `daemon_rpc.py` — HTTP/UDS client to ryeosd            | `reqwest`, `tokio` |
| `hooks.rs`         | `hooks_loader.py` — hook loading, evaluation, dispatch | condition, client  |
| `transcript.rs`    | `transcript.py` — JSONL events + knowledge markdown    | client             |
| `cas.rs`           | state/execution snapshot persistence via daemon CAS    | client             |
| `config.rs`        | config/resilience/provider loading                     | filesystem         |

**Key design point**: bundle-root discovery in `config.rs` uses the
contract defined in step 1g.

### Phase 2b: `graph-runtime` — binary crate

Build first — self-contained (no LLM loop), clear test fixtures (graph
YAML files).

1. `validation.rs` — structural checks, reachability, state flow analysis
2. `edges.rs` + `nodes.rs` — node type handlers (action, return, foreach, gate)
3. `foreach.rs` — sequential + parallel (`tokio::sync::Semaphore`)
4. `cache.rs` + `resume.rs` — CAS-backed result caching and resume
5. `walker.rs` — main graph traversal loop
6. `main.rs` — argparse + stdin JSON → execute() → stdout JSON

**Validation**: Run existing graph YAML test suites through both Python
and Rust walkers, diff outputs.

### Phase 2c: `directive-runtime` — binary crate

More complex due to LLM provider integration.

1. `directive.rs` — directive parsing, extends chain, input validation
2. `prompt.rs` — LLM prompt builder
3. `harness.rs` — safety harness (limits, capabilities, cancellation)
4. `tools.rs` — tool schema loading, preload, directive_return
5. `provider.rs` — HTTP LLM provider with SSE streaming (reqwest + eventsource-stream)
6. `dispatcher.rs` — tool call dispatch (route to ryeosd execute/fetch/sign)
7. `runner.rs` — core LLM loop (turn cycle, tool dispatch, limits)

**Validation**: Run existing directive test suites through both Python
and Rust runtimes, diff outputs. Token-level streaming output may differ
in chunking but final results must match.

### Phase 2d: Runtime YAML cutover

Update `runtime.yaml` to point at Rust binaries:

```yaml
# graph-runtime
config:
  command: "${RYE_GRAPH_RUNTIME}"
  args: ["--graph-path", "{tool_path}", "--project-path", "{project_path}"]
  stdin: "{params_json}"

# directive-runtime
config:
  command: "${RYE_DIRECTIVE_RUNTIME}"
  args: ["--project-path", "{project_path}", "--thread-id", "{thread_id}"]
  stdin: "{params_json}"
```

Binary discovery uses the bundle contract from step 1g.

### Facet vocabulary

Rust runtimes emit these facets via `threads.set_facets`:

| Key                    | Source            | Example                    |
| ---------------------- | ----------------- | -------------------------- |
| `llm.model`            | directive-runtime | `claude-sonnet-4-20250514` |
| `llm.provider`         | directive-runtime | `anthropic`                |
| `cost.input_tokens`    | directive-runtime | `12450`                    |
| `cost.output_tokens`   | directive-runtime | `3200`                     |
| `cost.total_usd`       | directive-runtime | `0.0234`                   |
| `graph.nodes_executed` | graph-runtime     | `7`                        |
| `graph.status`         | graph-runtime     | `completed`                |

### Exit criteria (no fallback — parity gates only)

- [ ] Graph fixture parity: all existing graph YAMLs produce identical output
- [ ] Directive fixture parity: all existing directives produce identical final results
- [ ] CAS/transcript parity: same JSONL event format, same knowledge markdown
- [ ] Hook behavior parity: identical hook dispatch and context injection
- [ ] Facet emission: Rust runtimes emit all defined facets

---

## 3. Webhook Replay Protection — M (0.5–1d)

Security hardening. Separate from custom routes.

### 3a. Outbound webhook headers

**File**: `ryeosd/src/webhooks.rs`

When delivering webhooks, add:

- `X-Rye-Timestamp` — Unix timestamp of delivery
- `X-Rye-Delivery-Id` — UUID per delivery
- `X-Rye-Signature` — HMAC-SHA256 over `{timestamp}.{delivery_id}.{body}`

Update HMAC to sign over the canonical string, not just the body.

### 3b. Inbound webhook verification

**File**: `ryeosd/src/api/webhooks.rs`

On inbound webhook receipt:

- Require `X-Rye-Timestamp` header
- Reject if timestamp is older than 5 minutes
- Require `X-Rye-Delivery-Id` header
- Check `X-Rye-Signature` over `{timestamp}.{delivery_id}.{body}`
- Reject duplicate delivery-ids via **durable** storage

### 3c. Durable delivery-id persistence

Store delivery IDs in a sliding-window file or SQLite table alongside
the daemon DB. Must survive daemon restarts. Prune entries older than
the timestamp window (5 minutes + buffer).

### 3d. Rollout

Existing webhook bindings: support legacy (body-only HMAC) temporarily
behind a version flag on the binding. New bindings default to v2
(timestamp + delivery-id + signature).

---

## 4. Admin Tooling Contract — M (0.5–1d)

Define the contract, don't over-model.

### Principles

- Admin tooling lives in bundles as **config + handlers**
- Control plane is daemon-owned
- Execution plane can trigger normal tool execution via daemon runner
- No new item type
- Config lives at `.ai/config/node/` (routes, webhooks, etc.)
- Daemon scans installed bundles + project config

### Admin config discovery

Daemon startup:

1. Scan project `.ai/config/node/` for admin configs
2. Scan each installed bundle's `.ai/config/node/` for admin configs
3. Merge additively (bundle configs namespaced by bundle prefix)
4. Invalid configs logged as warnings, not fatal
5. Re-scan on project push (when project space updates)

### Config locations

| Config Type      | Path                            | Scope                    |
| ---------------- | ------------------------------- | ------------------------ |
| Routes           | `.ai/config/node/routes/*.yaml` | Per-project + per-bundle |
| Webhook bindings | Runtime-created, CAS-stored     | Per-deployment           |
| Daemon config    | `~/.ai/config/daemon.yaml`      | User-space only          |

---

## 5. Custom Routes V1 — L (1–2d)

Build on the admin tooling contract from step 4. Full design at
`.tmp/node-custom-routes.md`.

### Scope (v1 only)

- **YAML-declared routes only** — no runtime registration API
- **Auth modes**: `none`, `hmac`
- **Response modes**: `static`, `tool_output`
- No `signed`/`token` auth modes yet
- No passthrough response mode yet

### Implementation

#### 5a. Route config parser

Parse `.ai/config/node/routes/*.yaml` per the schema in the
node-custom-routes doc. Validate required fields, reject malformed.

#### 5b. Dynamic route registration

At startup, register parsed routes as HTTP endpoints. Routes from
bundles are namespaced under the bundle prefix.

#### 5c. Request handler

```
Incoming HTTP → match route → auth check → response mode:
  static: return fixed response, spawn tool async
  tool_output: execute tool sync, return tool's response
```

#### 5d. Refresh on push

When project space updates (push), re-scan route configs and update
registered routes.

---

## 6. Facet Thread Queries — M (0.5–1d)

Comes naturally after Rust runtimes emit facets natively (step 2).

### 6a. Facet-based thread listing

**File**: `ryeosd/src/api/threads.rs`, `ryeosd/src/db.rs`

Add facet filters to `GET /threads`:

```
GET /threads?facet.llm.model=sonnet&facet.cost.provider=anthropic
```

- Parse query params with `facet.` prefix
- AND semantics for multiple filters
- Use `EXISTS` subquery per filter against `thread_facets`
- Use the existing `idx_thread_facets_key_value` index

```sql
SELECT t.* FROM threads t
WHERE EXISTS (
    SELECT 1 FROM thread_facets f
    WHERE f.thread_id = t.thread_id
    AND f.facet_key = ?1 AND f.facet_value = ?2
)
AND EXISTS (...)
ORDER BY t.created_at DESC
LIMIT ?
```

---

## Not In Scope

- CLI rewrite — deferred to separate Rust CLI plan at `.tmp/nl-cli-plan-clean/`
- NL input layer — deferred, depends on Rust CLI
- Private bundle distribution/sync — solve later, not a runtime concern
- Schema-driven facet projections — nice-to-have, not blocking
- Generic resource accounting beyond current facets — not needed yet
- Route registration API — YAML-only in v1
- `signed`/`token` auth modes for routes — add when needed
- Python ThreadLifecycleClient facet methods — Rust runtimes have their own DaemonClient
- Historical execution against arbitrary snapshots — HEAD only for now

---

## Dependency Graph

```
1. Daemon-Node Convergence + Bundle Access
    │
    ├── 1c+1d first: trust bootstrap fix, bundle discovery contract
    ├── 1a+1b next:  CAS-backed execution path, principal fix
    ├── 1e next:     required_secrets on tool metadata
    └── 1f+1g last:  remove /execute fields, migrate webhook bindings
    │
    ▼
2. Native Runtime Rewrite ──────────────────┐
    │                                        │
    │ (runtimes emit facets)                 │
    ▼                                        ▼
3. Webhook Replay ──→ 4. Admin Tooling ──→ 5. Custom Routes
                                                     │
                      6. Facet Thread Queries ◄──────┘
                           (after runtimes ship)
```

Step 1 is the prerequisite — daemon needs both execution paths
(live FS + CAS-backed) before runtimes can be rewritten against it.
Steps 3–5 are a parallel track that can proceed alongside step 2.
Step 6 depends on step 2 (facet emission).

Within step 1: 1c+1d are the foundation (trust and bundle discovery
must be correct first), 1a+1b add the CAS execution path, 1e adds
secret declarations (must land before 1f), 1f+1g remove the old
caller-supplied fields and migrate webhooks.

---

## Design Decision: Why Multiple System Roots (bundle_roots)

The daemon uses `bundle_roots` — an ordered list of directories beyond
`system_data_dir` — for bundle discovery. This was debated and kept for
structural reasons, not Python packaging nostalgia.

### The bundle architecture is inherently multi-root

Each bundle is a self-contained directory tree with its own `.ai/`
hierarchy. This isn't a pip artifact — it's fundamental to the bundle
integrity model:

1. **Manifest integrity**: each bundle has a `manifest.yaml` with
   SHA-256 hashes of all its files, relative to the bundle's own root.
   Merging bundles into a single directory breaks manifest paths.

2. **Independent versioning**: bundles version independently. A single
   merged directory can't represent "core v0.1.44, standard v0.2.1,
   code v0.1.3" — each needs its own root to be a coherent unit.

3. **Self-contained signing**: bundle manifests are signed. The
   signature covers files relative to the bundle root. No shared root.

### What changes post-Python is discovery, not layout

Python discovered bundles via `importlib.metadata.entry_points(group=
"rye.bundles")`. Each entry point returned a `root_path` from
`Path(__file__).parent`. The Rust daemon can't do that — it needs
explicit configuration.

`bundle_roots` is the Rust equivalent: an ordered list of bundle root
directories in the daemon config. The daemon scans each root for kind
schemas, trust keys, tools, and directives. First match wins for
conflicts (same semantics as Python's sorted entry point list).

### Development workflow

During bundle development, `bundle_roots` lets you point the daemon at
a local checkout without an install step. Without it, every edit would
require copying files into `system_data_dir`.

### What we're NOT doing

- No runtime entry-point discovery (no plugin scanning, no dlopen)
- No bundle dependency resolution (bundles are independent)
- No automatic bundle installation — `bundle_roots` is configured
  explicitly, either via CLI `--bundle-root` or config file

---

## Future: Per-request engine view (deferred)

Currently `effective_kinds(ctx)` and `effective_trust_store(ctx)` each
scan the project's filesystem on every call. A single request can hit
these 3–4 times across `resolve`, `verify`, and `build_plan`.

If this becomes a measurable bottleneck, introduce a cached
`EffectiveEngineView` keyed by `(project_root | snapshot_hash,
base_registry_fingerprint)` that carries:

- effective `KindRegistry` (base + project overlay)
- effective `TrustStore` (startup + project keys)
- effective fingerprint for plan/cache identity

`resolve`, `verify`, and `build_plan` would all accept this view
instead of re-deriving overlays independently.

**Don't build this yet.** Only revisit if:
- plan caching is real and `plan_id` stability matters
- per-request filesystem loads show up in profiling
- richer trust delegation is needed (cross-signed project keys, org trust)
- multi-workspace or remote clients make overlay identity more complex
