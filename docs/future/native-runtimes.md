```yaml
id: native-runtimes
title: "Native Runtimes: directive-runtime & graph-runtime"
description: Rewrite the Python directive LLM loop and state graph walker as compiled Rust binaries, deployed as data-driven tools dispatched by the existing engine pipeline.
category: future
tags: [rust, runtimes, directive, graph, llm, walker, performance]
version: "0.1.0"
status: planned
```

# Native Runtimes: directive-runtime & graph-runtime

> **Status:** Planned — the engine and daemon are already Rust. These are the last two large Python subsystems below userspace tools.

> **Prerequisite:** [Rust Engine Rewrite](rust-engine-rewrite.md) (complete). Engine resolves, verifies, builds plans, and dispatches via Lillux. Daemon owns thread lifecycle, events, CAS.

---

## Scope

Two Python tools are being replaced:

| Current Python tool                       | Lines | Bundle       | Replacement         |
| ----------------------------------------- | ----- | ------------ | ------------------- |
| `rye/agent/threads/thread_directive.py`   | ~930  | `ryeos_std`  | `directive-runtime` |
| `rye/core/runtimes/state-graph/walker.py` | ~2860 | `ryeos_core` | `graph-runtime`     |

Both are **data-driven tools** dispatched by the engine as subprocesses. They receive params on stdin, produce JSON on stdout, and call back to `ryeosd` via HTTP/UDS for execute/fetch/sign/lifecycle operations. The Rust replacements maintain this exact contract — no engine or daemon changes required.

---

## Architecture: What Doesn't Change

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

The engine dispatches these as it would any tool binary. The `runtime.yaml` for the state-graph runtime already specifies a command + args pattern — the Rust binary slots into the same shape, replacing the Python interpreter line with a native binary path.

---

## Workspace Structure

```
ryeos/
├── Cargo.toml                    # workspace: add rye_runtime, directive-runtime, graph-runtime
├── rye_engine/                   # existing — resolution, trust, plans, dispatch
├── ryeosd/                       # existing — daemon
├── lillux/                       # existing — process isolation
│
├── rye_runtime/                  # NEW — shared library crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── client.rs             # ryeosd HTTP/UDS client (execute, fetch, sign, lifecycle)
│       ├── condition.rs          # condition evaluator (matches, resolve_path, apply_operator)
│       ├── interpolation.rs      # ${state.foo} template interpolation
│       ├── hooks.rs              # hook loading, evaluation, dispatch
│       ├── permissions.rs        # capability fnmatch, check_permission
│       ├── transcript.rs         # JSONL event log + knowledge markdown rendering
│       ├── cas.rs                # CAS state/execution snapshot persistence
│       └── config.rs             # config loader, resilience loader, provider resolver
│
├── directive-runtime/            # NEW — binary crate
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs               # argparse + stdin JSON → execute() → stdout JSON
│       ├── directive.rs          # directive parsing, extends chain, input validation
│       ├── prompt.rs             # LLM prompt builder
│       ├── provider.rs           # HTTP LLM provider (reqwest + SSE streaming)
│       ├── runner.rs             # core LLM loop (turn cycle, tool dispatch, limits)
│       ├── harness.rs            # safety harness (limits, capabilities, cancellation)
│       ├── tools.rs              # tool schema loading, preload, directive_return
│       └── dispatcher.rs         # tool call dispatch (route to ryeosd execute/fetch/sign)
│
├── graph-runtime/                # NEW — binary crate
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs               # argparse + stdin JSON → execute() → stdout JSON
│       ├── walker.rs             # main graph traversal loop
│       ├── nodes.rs              # node type handlers (action, return, foreach, gate)
│       ├── edges.rs              # edge evaluation, on_error routing
│       ├── foreach.rs            # foreach sequential + parallel (tokio::spawn bounded)
│       ├── validation.rs         # graph validation, static analysis, reachability
│       ├── resume.rs             # CAS-backed resume support
│       └── cache.rs              # node-level result caching
```

---

## Shared Library: `rye_runtime`

Both runtimes share substantial infrastructure. This crate provides it.

### `client.rs` — ryeosd API Client

HTTP/UDS client for calling back to the daemon during execution.

```rust
pub struct DaemonClient {
    socket_path: Option<PathBuf>,  // UDS preferred
    base_url: String,              // HTTP fallback
    client: reqwest::Client,
}

impl DaemonClient {
    /// Dispatch rye_execute through the daemon
    pub async fn execute(&self, item_ref: &str, project_path: &str, params: Value) -> Result<Value>;

    /// Dispatch rye_fetch through the daemon
    pub async fn fetch(&self, item_ref: &str, project_path: &str) -> Result<Value>;

    /// Dispatch rye_sign through the daemon
    pub async fn sign(&self, item_ref: &str, project_path: &str) -> Result<Value>;

    /// Thread lifecycle: register, mark_running, finalize, attach_process
    pub async fn thread_create(&self, params: &ThreadCreateParams) -> Result<Value>;
    pub async fn thread_finalize(&self, params: &ThreadFinalizeParams) -> Result<Value>;

    /// Event store: append events, replay
    pub async fn append_event(&self, thread_id: &str, event_type: &str, payload: Value) -> Result<()>;
}
```

Socket path resolved from `RYE_DAEMON_SOCKET` env var (same as the Python `daemon_rpc.py`).

### `condition.rs` — Condition Evaluator

Direct port of `condition_evaluator.py`. Pure functions, no I/O.

```rust
pub fn matches(doc: &Value, condition: &Value) -> bool;
pub fn resolve_path(doc: &Value, path: &str) -> Option<Value>;
pub fn apply_operator(actual: &Value, op: &str, expected: &Value) -> bool;
```

Supports: `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `contains`, `regex`, `exists`, `any`, `all`, `not`.
Dotted path resolution with array index support (`state.items.0.name`, `state.items[0].name`).

### `interpolation.rs` — Template Engine

Port of `interpolation.py`. Resolves `${state.foo}` references in action params and assign expressions.

```rust
pub fn interpolate(template: &Value, context: &Value) -> Value;
pub fn interpolate_action(action: &Value, context: &Value) -> Value;
```

Handles nested dicts, arrays, string templates with multiple refs, and full-value substitution when the entire string is a single `${...}` ref.

### `hooks.rs` — Hook System

Port of hook loading and evaluation from `hooks_loader.py` and the hook dispatch in both `safety_harness.py` and `walker.py`.

```rust
pub struct HookEngine {
    hooks: Vec<Hook>,
    client: Arc<DaemonClient>,
}

impl HookEngine {
    /// Load and merge hooks: user → directive/graph → builtin → context → project → infra
    pub fn load(project_path: &Path, item_hooks: Vec<Hook>) -> Result<Self>;

    /// Evaluate hooks for an event. Returns control action or None.
    pub async fn run_hooks(&self, event: &str, context: &Value, project_path: &str) -> Option<Value>;

    /// Evaluate context hooks (thread_started, build_system_prompt). Returns concatenated context.
    pub async fn run_hooks_context(&self, context: &Value, event: &str, suppress: &[String]) -> HookContext;
}
```

Hook conditions evaluated via `condition.rs`. Actions dispatched via `client.rs`.

### `permissions.rs` — Capability Enforcement

Port of permission checking from both `safety_harness.py` and `walker.py`.

```rust
/// Check if an action is permitted by capabilities. Returns None if allowed, error if denied.
pub fn check_permission(capabilities: &[String], primary: &str, item_id: &str) -> Option<PermissionError>;

/// Attenuate child capabilities against parent caps (fnmatch intersection).
pub fn attenuate(child_caps: &[String], parent_caps: &[String]) -> Vec<String>;
```

Uses `glob::Pattern` (or inline fnmatch) for capability matching. Fail-closed: empty capabilities deny all actions. Internal thread tools (`rye/agent/threads/internal/*`) always allowed.

### `transcript.rs` — Event Log & Knowledge Markdown

Port of `transcript.py` and `GraphTranscript`. Shared JSONL event writing and knowledge markdown rendering.

```rust
pub struct Transcript {
    thread_id: String,
    project_path: PathBuf,
    jsonl_path: PathBuf,
    client: Arc<DaemonClient>,
}

impl Transcript {
    /// Append event — daemon-backed when available, JSONL fallback
    pub async fn write_event(&self, event_type: &str, payload: Value);

    /// Checkpoint: sign transcript at step boundary
    pub fn checkpoint(&self, step: usize);

    /// Render signed knowledge markdown (for both threads and graphs)
    pub fn render_knowledge(&self, status: &str, step_count: usize, elapsed_s: f64) -> Option<PathBuf>;
}
```

### `cas.rs` — CAS Persistence

Port of state/execution snapshot storage. Used by both runtimes for state persistence.

```rust
/// Store a state snapshot in CAS. Returns hash.
pub fn store_state_snapshot(project_path: &Path, state: &Value) -> Result<String>;

/// Store an execution snapshot and update mutable ref.
pub fn persist_execution(project_path: &Path, params: &ExecutionPersistParams) -> Result<Option<String>>;

/// Store a node receipt (graph-specific).
pub fn store_node_receipt(project_path: &Path, receipt: &NodeReceipt) -> Result<Option<String>>;
```

### `config.rs` — Configuration Loading

Port of resilience loader, config loader, agent config, provider YAML discovery.

```rust
/// Load and merge agent config: system → user → project
pub fn load_agent_config(project_path: &Path) -> Result<Value>;

/// Load resilience config (limits defaults, tool preload settings)
pub fn load_resilience(project_path: &Path) -> Result<Value>;

/// Resolve provider: model/tier → (resolved_model, provider_item_id, provider_config)
pub fn resolve_provider(model: &str, project_path: &Path, provider_hint: Option<&str>)
    -> Result<(String, String, Value)>;
```

---

## Binary: `directive-runtime`

Replaces `thread_directive.py` + `runner.py` + `safety_harness.py` + the `adapters/` and `loaders/` subtrees.

### Entry Point (`main.rs`)

Same interface as the Python tool:

```rust
fn main() {
    let args = Args::parse();  // --project-path, --thread-id, --pre-registered
    let params: Value = serde_json::from_reader(std::io::stdin())?;
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(execute(params, &args));
    println!("{}", serde_json::to_string(&result)?);
}
```

### `directive.rs` — Directive Resolution

- Parse directive markdown/XML via `ParserRouter` equivalent (YAML frontmatter + XML body extraction)
- Input validation and interpolation
- Extends chain resolution: walk `extends` field, fetch parent directives via `DaemonClient::fetch()`
- Context composition: collect `system`/`before`/`after` knowledge refs, materialize via `DaemonClient::execute()`
- Limits resolution: `defaults → directive → overrides → parent upper bounds`
- Capability risk assessment

### `prompt.rs` — LLM Prompt Builder

Port of `_build_prompt()`:

- Directive name + description
- Permissions (raw XML passthrough)
- Body (process steps with interpolated inputs)
- Returns section (output fields → directive_return instruction)

### `provider.rs` — HTTP LLM Provider

Port of `http_provider.py`. Data-driven provider adapter:

```rust
pub struct HttpProvider {
    model: String,
    client: reqwest::Client,
    config: ProviderConfig,  // from provider YAML
}

impl HttpProvider {
    /// Non-streaming completion
    pub async fn create_completion(&self, messages: &[Message], tools: &[ToolDef]) -> Result<LlmResponse>;

    /// SSE streaming completion
    pub async fn create_streaming_completion(&self, messages: &[Message], tools: &[ToolDef]) -> Result<LlmResponse>;
}
```

All provider-specific behavior driven by YAML schemas (`response_schema`, `message_schema`, `stream_schema`). No hardcoded provider formats. Uses `reqwest` + `eventsource-stream` for SSE.

### `runner.rs` — Core LLM Loop

Port of `runner.py::run()`. The main execution loop:

```
1. Register thread with orchestrator (in-process tracking)
2. Build system prompt from hooks (build_system_prompt event)
3. Build first user message from hooks (thread_started event) + directive prompt
4. Loop:
   a. Pre-turn limit check (turns, tokens, spend, elapsed)
   b. Cancellation check (SIGTERM flag)
   c. Checkpoint: sign transcript, render knowledge
   d. LLM call via provider (streaming or non-streaming)
   e. Track token usage and cost
   f. Parse tool calls from response (native tool_use or text-parsed)
   g. For each tool call:
      - Check permission via harness
      - Dispatch via DaemonClient::execute()
      - Unwrap result envelope
      - Guard result size
   h. Append assistant + tool result messages
   i. Run after_step hooks
   j. Check for directive_return (terminal tool call)
5. Finalize: write thread.json, render knowledge, sign transcript
```

### `harness.rs` — Safety Harness

Port of `SafetyHarness`:

- Limit checking: `turns`, `tokens`, `spend`, `elapsed_seconds`, `depth`, `spawns`
- Capability attenuation: child ∩ parent via fnmatch
- Tool schema storage: `available_tools`, `capabilities_tree`
- Hook dispatch delegation to `HookEngine`
- Cancellation flag (checked per-turn)

### `dispatcher.rs` — Tool Call Dispatcher

Port of `ToolDispatcher`. Routes tool calls from LLM responses:

```rust
pub struct ToolDispatcher {
    client: Arc<DaemonClient>,
    project_path: PathBuf,
}

impl ToolDispatcher {
    /// Dispatch a tool call. Routes to execute/fetch/sign based on the primary action.
    pub async fn dispatch(&self, action: &ToolAction) -> Result<Value>;
}
```

Internal tools (`rye/agent/threads/internal/*`) handled in-process. Everything else goes through `DaemonClient`.

### `tools.rs` — Tool Schema Loading

Port of `tool_schema_loader.py`:

- Resolve capabilities to concrete tool schemas for the LLM
- Build primary action schemas (rye_execute, rye_fetch, rye_sign)
- Build `directive_return` schema from directive `<outputs>`
- Token budget enforcement for preloaded schemas

---

## Binary: `graph-runtime`

Replaces `walker.py`. All 2860 lines consolidated into focused modules.

### Entry Point (`main.rs`)

Same interface as the Python tool:

```rust
fn main() {
    let args = Args::parse();  // --graph-path, --project-path, --graph-run-id, --pre-registered
    let params: Value = serde_json::from_reader(std::io::stdin())?;

    // SIGTERM handler for cooperative cancellation
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::SIGTERM, Arc::clone(&shutdown))?;

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(execute(graph_config, params, &args, shutdown));
    println!("{}", serde_json::to_string(&result)?);
}
```

### `walker.rs` — Main Graph Traversal Loop

Port of `walker.py::execute()`. The core walk loop:

```
1. Parse graph YAML config (nodes, start, state, on_error, hooks, permissions)
2. Resolve execution context (capabilities from parent thread, graph YAML, or params)
3. Validate graph (structural checks, reachability analysis, state flow)
4. Preflight env check (env_requires on graph + nodes)
5. Initialize or resume state:
   - Fresh: merge initial state + inputs, fire graph_started hooks
   - Resume: load from CAS via execution_snapshot ref, verify transcript integrity
6. Loop (while current node exists and step_count < max_steps):
   a. Look up node definition
   b. Match node type:
      - return: interpolate output template, finalize, return result
      - foreach: delegate to foreach handler
      - gate: evaluate assign + edges, no action dispatch
      - action: interpolate params, check permission, dispatch, handle result
   c. On action nodes:
      - Cache lookup (opt-in via cache_result: true)
      - Dispatch via DaemonClient::execute()
      - Unwrap result envelope
      - Handle continuation chains for LLM nodes
      - Error handling: hooks → on_error edge → error_mode (fail/continue)
      - Assign result values to state
      - Evaluate edges for next node
   d. Store NodeReceipt in CAS
   e. Write transcript events (step_started, step_completed)
   f. Checkpoint + persist state + render knowledge
   g. Fire after_step hooks
   h. Check SIGTERM cancellation flag
7. Max steps exceeded: fire limit hooks, finalize as error
```

### `nodes.rs` — Node Type Handlers

Extracted handlers for each node type:

```rust
pub async fn handle_return(node: &Value, state: &Value, ctx: &WalkContext) -> WalkResult;
pub async fn handle_gate(node: &Value, state: &mut Value, ctx: &WalkContext) -> Option<String>;
pub async fn handle_action(node: &Value, state: &mut Value, ctx: &mut WalkContext) -> StepResult;
```

### `edges.rs` — Edge Evaluation

Port of `_evaluate_edges()` and `_find_error_edge()`:

```rust
/// Evaluate next node from edge spec. Returns None for terminal.
pub fn evaluate_edges(next_spec: &Value, state: &Value, result: &Value) -> Option<String>;

/// Find on_error target for a node.
pub fn find_error_edge(node: &Value) -> Option<String>;
```

Supports: string (unconditional), list of `{when, to}` (conditional, first match wins), None (terminal).

### `foreach.rs` — Foreach Support

Port of `_handle_foreach()`, `_foreach_sequential()`, `_foreach_parallel()`:

```rust
/// Handle a foreach node. Returns (next_node, updated_state).
pub async fn handle_foreach(
    node: &Value,
    state: &mut Value,
    ctx: &WalkContext,
    max_concurrency: usize,
) -> Result<Option<String>>;
```

Parallel mode uses `tokio::sync::Semaphore` for bounded concurrency (replaces `asyncio.Semaphore`).

### `validation.rs` — Graph Validation

Port of `_validate_graph()` and `_analyze_graph()`:

- Structural checks: start node exists, next/on_error references valid, known node keys
- Reachability analysis: BFS from start, warn on unreachable nodes
- State flow analysis: assigned-but-not-referenced, referenced-but-not-assigned
- Foreach structural checks: requires `over` and `action`
- Input validation against `config_schema`
- Env preflight: check `env_requires` vars

### `resume.rs` — Resume Support

Port of `_load_resume_state()`:

- Load execution snapshot from CAS via mutable ref
- Load state snapshot by hash
- Verify transcript JSONL integrity
- Extract current_node from last `state_checkpoint` event

### `cache.rs` — Node Result Caching

Port of node-level caching:

- `compute_cache_key()` from graph hash + node name + interpolated action + config snapshot
- `cache_lookup()` — check CAS for cached result
- `cache_store()` — store result in CAS on successful execution

---

## Dependencies

### `rye_runtime` (shared library)

```toml
[dependencies]
reqwest = { version = "0.12", features = ["json", "stream"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
sha2 = "0.10"
ed25519-dalek = { version = "2", features = ["pkcs8", "pem"] }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
glob = "0.3"       # fnmatch for capability matching
regex = "1"        # condition evaluator regex operator
chrono = "0.4"
```

### `directive-runtime` (binary)

```toml
[dependencies]
rye_runtime = { path = "../rye_runtime" }
clap = { version = "4", features = ["derive"] }
reqwest = { version = "0.12", features = ["json", "stream"] }
tokio = { version = "1", features = ["full"] }
eventsource-stream = "0.2"   # SSE parsing for LLM streaming
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
signal-hook = "0.3"
```

### `graph-runtime` (binary)

```toml
[dependencies]
rye_runtime = { path = "../rye_runtime" }
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
tracing = "0.1"
tracing-subscriber = "0.3"
signal-hook = "0.3"
```

---

## Runtime YAML Updates

The existing `runtime.yaml` for the state-graph walker points at a Python interpreter. The Rust binary replaces it:

```yaml
# Before (Python)
config:
  command: "${RYE_PYTHON}"
  args: ["-c", "...python bootstrap...", "{tool_path}", "{params_json}", "{project_path}"]

# After (Rust)
config:
  command: "${RYE_GRAPH_RUNTIME}"   # path to compiled graph-runtime binary
  args: ["--graph-path", "{tool_path}", "--project-path", "{project_path}"]
  stdin: "{params_json}"
```

The `env_config` section changes from resolving a Python interpreter to resolving the Rust binary location. The `anchor`, `verify_deps`, and `config_resolve` sections stay the same — they govern the graph YAML tool itself, not the runtime binary.

Similarly, the directive executor chain builder in `ryeos_core` currently specifies `python3 thread_directive.py`. The Rust binary replaces the interpreter + script with a single binary path.

---

## Migration Strategy

### Phase 0: Workspace Setup

Add `rye_runtime`, `directive-runtime`, `graph-runtime` to `Cargo.toml` workspace members. Scaffold crate structure, basic `main.rs` entry points that parse args and read stdin.

### Phase 1: `rye_runtime` — Shared Infrastructure

Build the shared library bottom-up:

1. `condition.rs` — pure functions, easy to test against Python equivalents
2. `interpolation.rs` — template engine, test with graph YAML fixtures
3. `permissions.rs` — capability matching, test with existing permission test cases
4. `client.rs` — ryeosd HTTP/UDS client, integration test against running daemon
5. `hooks.rs` — hook loading and evaluation, depends on condition + client
6. `transcript.rs` — JSONL + knowledge markdown, depends on client
7. `cas.rs` — state persistence, depends on daemon CAS API
8. `config.rs` — config/provider loading, depends on filesystem layout

### Phase 2: `graph-runtime` — Graph Walker

Build graph walker first — it's self-contained (no LLM loop) and has clear test fixtures (graph YAML files):

1. `validation.rs` — static analysis, test against existing graph YAMLs
2. `edges.rs` + `nodes.rs` — node type handling, unit tests
3. `foreach.rs` — sequential + parallel iteration
4. `cache.rs` + `resume.rs` — CAS integration
5. `walker.rs` — main loop, integration test with a simple graph
6. `main.rs` — wire up entry point

**Validation:** Run existing graph YAML test suites through both Python and Rust walkers, diff outputs.

### Phase 3: `directive-runtime` — LLM Loop

Build directive runtime — more complex due to LLM provider integration:

1. `directive.rs` — directive parsing and extends chain
2. `prompt.rs` — prompt builder
3. `harness.rs` — limits and capabilities
4. `tools.rs` — tool schema loading
5. `provider.rs` — HTTP LLM provider with SSE streaming
6. `dispatcher.rs` — tool call routing
7. `runner.rs` — core LLM loop, integration test with a simple directive

**Validation:** Run existing directive test suites through both Python and Rust runtimes, diff outputs. Token-level streaming output may differ in chunking but final results must match.

### Phase 4: Bundle Updates

1. Update `runtime.yaml` to point at compiled Rust binary
2. Update directive executor chain to use Rust binary
3. Python tools remain in bundles as fallback (executor chain can select either)
4. Feature-flag or env var (`RYE_NATIVE_RUNTIMES=1`) to opt into Rust runtimes

### Phase 5: Deprecate Python Runtimes

After validation period:

1. Rust runtimes become default
2. Python tools moved to a `compat/` directory
3. Eventually removed

---

## What This Enables

- **No Python dependency below userspace** — daemon + engine + runtimes are all Rust. Only user-authored tools need Python.
- **Startup latency** — Python import chain for `thread_directive.py` (~200ms) eliminated. Rust binary cold-starts in ~5ms.
- **Graph step overhead** — each walker step currently pays Python interpreter overhead. Rust eliminates per-step allocation and GC pressure.
- **Single binary distribution** — `ryeosd` + runtimes can be statically linked and shipped as one binary.
- **Concurrent graph execution** — Rust's async runtime handles parallel foreach without GIL contention.
- **Memory** — Python walker holds full graph state + all CAS objects in-process. Rust uses stack allocation and streaming where possible.

---

## What Stays Python

- **User-authored tools** — `.py` scripts in project/user `.ai/tools/`
- **Provider YAML schemas** — data files, not executed
- **Graph YAML tools** — data files interpreted by the runtime
- **Directives** — markdown/XML files interpreted by the runtime
- **Knowledge entries** — markdown files, not executed
- **MCP tool handlers** — Python tools exposed via MCP

The runtimes interpret these data files. The data files don't change.

---

## Risks

| Risk                            | Mitigation                                                                      |
| ------------------------------- | ------------------------------------------------------------------------------- |
| Provider YAML schema edge cases | Extensive test matrix against all provider YAMLs (Anthropic, OpenAI, etc.)      |
| SSE streaming parity            | Compare token-by-token output between Python httpx and Rust reqwest+eventsource |
| CAS format compatibility        | Shared CAS object schemas — Rust reads/writes same format as Python             |
| Hook behavior drift             | Run identical hook test suites against both runtimes                            |
| Interpolation edge cases        | Fuzz test interpolation engine with graph YAML corpus                           |
| Directive parsing               | Parser must handle same markdown/XML edge cases as Python `ParserRouter`        |

---

## Relationship to Other Documents

| Document                                                        | Relationship                                                            |
| --------------------------------------------------------------- | ----------------------------------------------------------------------- |
| [Rust Engine Rewrite](rust-engine-rewrite.md)                   | Engine is already Rust — runtimes are the next layer up                 |
| [Daemon Runtime Completion](daemon-runtime-completion.md)       | Command delivery (cancel/interrupt) integrates with runtime's turn loop |
| [Advanced Native Runtime Path](advanced-native-runtime-path.md) | Native runtimes enable the remote-correct-from-day-one contract         |
| [Execution Graph Scheduling](execution-graph-scheduling.md)     | Graph runtime is the execution target for scheduled graphs              |
| [Node Sandboxed Execution](node-sandboxed-execution.md)         | Rust runtimes run inside sandboxed environments more easily             |
