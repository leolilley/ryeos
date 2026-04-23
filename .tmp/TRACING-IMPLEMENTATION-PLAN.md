# Observability Roadmap: Structured Tracing for Rye OS

**Date:** 2025-04-23  
**Status:** Draft  
**Scope:** Tracing instrumentation across the Rust workspace  
**Out of Scope:** Benchmarks, OpenTelemetry export, custom subscriber implementations

---

## 1. Current State Audit

### 1.1 Workspace at a Glance

```
8 crates, 11 binaries, 150 .rs files
```

### 1.2 What We Have

| Metric | Value |
|---|---|
| `tracing` dependency | 7 of 8 crates (all except `lillux`) |
| `tracing-subscriber` dependency | 4 crates (ryeosd, ryeos-graph-runtime, ryeos-directive-runtime, ryeos-tools) |
| Total tracing macro calls | 119 (`info!` 50, `debug!` 31, `warn!` 24, `error!` 11, `trace!` 3) |
| Other logging frameworks | None — tracing is the sole framework |

### 1.3 What We're Missing

| Gap | Severity | Impact |
|---|---|---|
| Zero `#[tracing::instrument]` annotations | **Critical** | No structured spans anywhere. All 119 calls are bare events with no parent-child hierarchy. Impossible to correlate events across the execution path of a single directive or thread. |
| `trace!` nearly empty (3 calls) | **High** | Finest-grained debug level is essentially dead. No way to trace hot loops, interpolation, signature verification, or other low-level operations without modifying code. |
| No shared subscriber configuration | **High** | Each binary initializes its own subscriber independently. No unified filter config, no structured output format, no way for users to control verbosity. |
| 5 of 7 `rye-tools` binaries have no tracing | **Medium** | `rye-status`, `rye-verify`, `rye-sign`, `rye-rebuild`, `rye-bundle` are silent. When a CLI tool misbehaves, there's nothing to look at. |
| `ryeos-tools` lib has 1 test | **Low** | Testing gap, not tracing gap, but relevant — hard to verify tracing works if the test harness is thin. |

### 1.4 Per-Crate Distribution

Tracing calls are heavily skewed toward `ryeosd`:

```
ryeosd              ████████████████████████████████████████████  62
ryeos-engine        █████████████████                            23
ryeos-state         ███████                                      10
ryeos-directive-runtime ██████                                    9
ryeos-graph-runtime ████                                          5
ryeos-tools         █████                                         6
lillux                                                            0
```

### 1.5 Actual Dependency Graph

```
                    lillux  (leaf — no internal deps, no tracing)
                   /  /  \     \  \      \
                  /  /    \     \  \      \
    ryeos-engine  ryeos-state  ryeos-runtime  (leaf siblings, all → lillux only)
          \            \           /     \
           \            \         /       \
            ryeos-tools (→ engine + state)  ryeos-graph-runtime (→ runtime)
                                          ryeos-directive-runtime (→ runtime)

    ryeosd (→ engine + state + lillux)
```

Key fact: `ryeos-engine` and `ryeos-state` are **parallel leaf-level crates** with no path between them. Neither can host a shared module the other can reach. This constrains where shared tracing infrastructure can live.

---

## 2. Span Architecture

### 2.1 The Core Problem

Right now, a directive execution produces a flat stream of uncorrelated events:

```
INFO directive resolved: id=my-directive
INFO loading tool: name=rye/bash/bash
INFO state stored: key=thread_123
WARN tool timeout: name=rye/bash/bash
INFO directive complete: id=my-directive
```

There's no way to answer: "How long did the tool execution within directive X take?" or "What did the full execution tree for thread Y look like?" Every event is an orphan.

### 2.2 Target Span Hierarchy

```
session                                                    ← top-level binary invocation
  ├── directive:resolve                                    ← parsing + validation
  │     fields: directive_id, path, kind
  │
  ├── directive:execute                                    ← the main execution span
  │     fields: directive_id, model, thread_id
  │     │
  │     ├── thread:spawn                                   ← creating a new thread
  │     │     fields: thread_id, parent_thread_id, model
  │     │
  │     ├── thread:execute                                 ← running a thread's LLM loop
  │     │     fields: thread_id, turn, model
  │     │     │
  │     │     ├── tool:resolve                             ← resolving a tool from disk
  │     │     │     fields: tool_name, kind, source
  │     │     │
  │     │     ├── tool:execute                             ← executing a tool
  │     │     │     fields: tool_name, elapsed_ms
  │     │     │
  │     │     └── state:persist                            ← writing transcript/state
  │     │           fields: key, artifact_type
  │     │
  │     ├── graph:step                                     ← single graph node execution
  │     │     fields: node_id, node_type, status
  │     │
  │     └── provider:request                               ← outbound LLM/provider call
  │           fields: adapter_type, model, stream
  │
  └── engine:lifecycle                                     ← startup/shutdown
        fields: event
```

### 2.3 How This Works

Once `#[tracing::instrument]` is on the hot-path functions, every event emitted inside them automatically inherits the span context. The existing 119 `info!`/`debug!`/`warn!`/`error!` calls become *enriched* — they gain directive_id, thread_id, tool_name without any changes to the event calls themselves.

This is why instrument annotations are Tier 1: they multiply the value of every existing tracing call at zero marginal cost per event.

### 2.4 Span Field Conventions

- **No prefix.** Span names provide the namespace. A field called `directive_id` inside `directive::execute` is unambiguous.
- **Use `snake_case`** for all field names (Rust convention, matches tracing ecosystem).
- **Standard fields per span type:**

| Span Type | Required Fields |
|---|---|
| `directive::*` | `directive_id` |
| `thread::*` | `thread_id`, optionally `parent_thread_id` |
| `tool::*` | `tool_name` |
| `graph::*` | `node_id`, `node_type` |
| `state::*` | `key` or `artifact_type` |
| `provider::*` | `adapter_type`, `model` |

- **`elapsed_ms`** on spans that represent measurable work (tool:execute, provider:request, graph:step).
- **`skip(self)`** on all `#[tracing::instrument]` annotations — never log the entire struct.

---

## 3. The `ryeos-tracing` Crate

### 3.1 Why a New Crate

Given the dependency graph, there's no existing crate that both `ryeos-engine` and `ryeos-state` can depend on for shared tracing infrastructure (other than `lillux`, which intentionally has no observability). A new leaf crate solves this:

```
                    lillux          ryeos-tracing  ← NEW (leaf, no internal deps)
                   /  /  \         /  /  /  \  \
                  /  /    \       /  /  /    \  \
    ryeos-engine  ryeos-state  ryeos-runtime  (all → lillux + ryeos-tracing)
          \            \           /     \
           ... (rest of graph unchanged)
```

Every crate in the workspace can pull in `ryeos-tracing` without cycles.

### 3.2 Crate Structure

```
ryeos-tracing/
  Cargo.toml
  src/
    lib.rs          ← re-exports
    subscriber.rs   ← init_subscriber(), output format config
    fields.rs       ← shared field name constants
    test.rs         ← mock subscriber for asserting spans in tests
```

### 3.3 `subscriber.rs` — Unified Initialization

```rust
pub struct SubscriberConfig {
    pub filter: String,           // e.g. "ryeosd=debug,ryeos_engine=info"
    pub json_output: bool,        // structured JSON vs pretty-human
    pub with_file: bool,          // include file:line in output
    pub with_target: bool,        // include module path
}

impl Default for SubscriberConfig {
    fn default() -> Self {
        Self {
            filter: std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info".into()),
            json_output: std::env::var("RYE_TRACE_JSON").is_ok(),
            with_file: false,
            with_target: true,
        }
    }
}

pub fn init_subscriber(config: SubscriberConfig) { ... }
```

Key design decisions:
- **`RUST_LOG`** as the primary filter mechanism — standard Rust ecosystem convention.
- **`RYE_TRACE_JSON`** env var toggle for structured output — useful for log aggregation.
- Each binary calls `init_subscriber()` early in `main()`.
- The function is idempotent — safe to call multiple times in tests.

### 3.4 `fields.rs` — Shared Constants

```rust
pub mod field {
    pub const DIRECTIVE_ID: &str = "directive_id";
    pub const THREAD_ID: &str = "thread_id";
    pub const PARENT_THREAD_ID: &str = "parent_thread_id";
    pub const TOOL_NAME: &str = "tool_name";
    pub const NODE_ID: &str = "node_id";
    pub const NODE_TYPE: &str = "node_type";
    pub const ARTIFACT_KEY: &str = "key";
    pub const ARTIFACT_TYPE: &str = "artifact_type";
    pub const ADAPTER_TYPE: &str = "adapter_type";
    pub const MODEL: &str = "model";
    pub const ELAPSED_MS: &str = "elapsed_ms";
}
```

These prevent field name typos across crates and make it easy to grep for all uses of a given field.

### 3.5 `test.rs` — Test Harness

```rust
/// Captures all spans and events created during a test closure.
/// Returns a recorded trace that can be asserted against.
pub fn capture_traces<F, R>(f: F) -> (R, Vec<RecordedSpan>)
where
    F: FnOnce() -> R,
{ ... }

pub struct RecordedSpan {
    pub name: String,
    pub fields: HashMap<String, String>,
    pub children: Vec<RecordedSpan>,
    pub events: Vec<RecordedEvent>,
}

pub struct RecordedEvent {
    pub level: Level,
    pub fields: HashMap<String, String>,
}
```

Usage in tests:

```rust
#[test]
fn directive_execute_creates_expected_spans() {
    let (_, spans) = ryeos_tracing::test::capture_traces(|| {
        execute_directive(/* ... */);
    });

    let root = spans.iter().find(|s| s.name == "directive::execute").unwrap();
    assert_eq!(root.fields["directive_id"], "my-directive");
    assert!(root.children.iter().any(|s| s.name == "tool::execute"));
}
```

### 3.6 Cargo.toml

```toml
[package]
name = "ryeos-tracing"
version = "0.1.0"
edition = "2021"

[dependencies]
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }

[dev-dependencies]
# test harness has no extra dependencies — uses tracing-subscriber's
# internal mechanisms for span capture
```

---

## 4. Instrumentation Plan — Three Tiers

### 4.1 Tier 1: Critical Path (est. 25-30 annotations)

These are the functions on the execution hot path. Adding instrument here enriches *all* downstream events.

**Priority order — do these first:**

#### `ryeosd` — Execution Runner & Bootstrap

| File | Function | Span Name | Notes |
|---|---|---|---|
| `execution/runner.rs` | `run_thread_loop` | `thread:execute` | The core LLM loop — highest value single annotation |
| `execution/runner.rs` | `invoke_tool` | `tool:execute` | Tool invocation boundary |
| `execution/runner.rs` | `resolve_tool` | `tool:resolve` | Tool lookup from disk |
| `execution/mod.rs` | `execute_directive` | `directive:execute` | Top-level directive entry |
| `bootstrap.rs` | `bootstrap` | `engine:lifecycle` | Startup sequence |
| `engine_init.rs` | `init_engine` | `engine:lifecycle` | Engine initialization |
| `reconcile.rs` | `reconcile_state` | `state:reconcile` | State reconciliation loop |

#### `ryeos-directive-runtime` — Provider Adapter Boundary

| File | Function | Span Name | Notes |
|---|---|---|---|
| `dispatcher.rs` | `dispatch` | `directive:execute` | Directive dispatch entry |
| `provider_adapter/http.rs` | `send_request` (impl) | `provider:request` | HTTP adapter — highest-value trait impl |
| `provider_adapter/streaming.rs` | `stream_response` (impl) | `provider:request` | Streaming adapter |
| `provider_adapter/messages.rs` | `build_messages` (impl) | `provider:build_messages` | Message construction |

#### `ryeos-graph-runtime` — Graph Execution

| File | Function | Span Name | Notes |
|---|---|---|---|
| `dispatch.rs` | `dispatch_node` | `graph:step` | Single graph node execution |
| `hooks.rs` | `run_hook` | `graph:hook` | Hook execution within a step |
| `main.rs` | `run_graph` | `graph:execute` | Top-level graph entry |

#### `ryeos-state` — State Operations

| File | Function | Span Name | Notes |
|---|---|---|---|
| `chain_state.rs` | `apply_event` | `state:apply` | Event application to chain |
| `chain.rs` | `append` | `state:append` | Chain append operation |

### 4.2 Tier 2: State & CLI Tools (est. 30-40 annotations)

#### `ryeos-state` — Deeper Instrumentation

| File | Function | Span Name |
|---|---|---|
| `head_cache.rs` | `get` / `invalidate` | `state:cache_get` / `state:cache_invalidate` |
| `thread_snapshot.rs` | `snapshot` / `restore` | `state:snapshot` / `state:restore` |
| `thread_event.rs` | `record` | `state:record_event` |
| `artifact.rs` | `store` / `load` | `state:artifact_store` / `state:artifact_load` |

#### `ryeos-engine` — Lifecycle & Trust

| File | Function | Span Name |
|---|---|---|
| `lifecycle.rs` | `start` / `stop` / `reload` | `engine:lifecycle` |
| `trust.rs` | `verify_signature` / `check_trust_chain` | `engine:trust_verify` |
| `canonical_ref.rs` | `resolve` | `engine:resolve_ref` |
| `kind_registry.rs` | `register` / `lookup` | `engine:registry_op` |

#### `ryeos-tools` — CLI Binary Instrumentation

All 7 binaries need the same pattern:

```rust
fn main() {
    ryeos_tracing::init_subscriber(SubscriberConfig::default());
    // ... existing code
}
```

Plus instrument on the primary work function in each:

| Binary | Function to Instrument | Span Name |
|---|---|---|
| `rye-fetch` | `fetch_item` | `tool:fetch` |
| `rye-sign` | `sign_item` | `tool:sign` |
| `rye-verify` | `verify_item` | `tool:verify` |
| `rye-status` | `show_status` | `tool:status` |
| `rye-gc` | `collect_garbage` | `tool:gc` |
| `rye-rebuild` | `rebuild_state` | `tool:rebuild` |
| `rye-bundle` | `bundle_items` | `tool:bundle` |

### 4.3 Tier 3: Deep Observability — `trace!` Fill (est. 40-50 calls)

The `trace!` level should be the "turn it on only when debugging something specific" layer. Right now it has 3 calls. Target areas:

#### `ryeos-runtime`

| File | Where to Add `trace!` | What to Log |
|---|---|---|
| `verified_loader.rs` | Every file load attempt | path, hash_result, trust_level |
| `capability_tokens.rs` | Token creation + validation | token_id, capabilities, valid |
| `interpolation.rs` | Each interpolation step | template, result |
| `condition.rs` | Condition evaluation | expression, result |
| `paths.rs` | Path resolution steps | input, resolved, source |

#### `ryeos-engine`

| File | Where to Add `trace!` | What to Log |
|---|---|---|
| `trust.rs` | Each signature verification step | key_id, verified, error |
| `canonical_ref.rs` | Ref resolution path | ref, intermediate, final |
| `kind_registry.rs` | Registry lookup misses | requested, available |

#### `ryeos-state`

| File | Where to Add `trace!` | What to Log |
|---|---|---|
| `chain.rs` | Each link traversal | index, hash |
| `head_cache.rs` | Cache hit/miss | key, hit |
| `projection.rs` | Projection rebuild steps | from_version, to_version |

#### `ryeosd`

| File | Where to Add `trace!` | What to Log |
|---|---|---|
| `execution/runner.rs` | Each LLM response chunk (if streaming) | chunk_index, delta_length |
| `execution/runner.rs` | Token counting | input_tokens, output_tokens |
| `write_barrier.rs` | Barrier check | thread_id, allowed |

#### Target: 50+ `trace!` calls distributed across these files.

---

## 5. Implementation Sequence

### Phase 1: Foundation (est. 1-2 days)

1. Create `ryeos-tracing` crate with `subscriber.rs`, `fields.rs`, `test.rs`
2. Add it to workspace `Cargo.toml`
3. Add dependency to all 7 non-lillux crates
4. Migrate subscriber initialization in `ryeosd`, `ryeos-graph-runtime`, `ryeos-directive-runtime`, `ryeos-tools` to use `ryeos_tracing::init_subscriber()`
5. Verify existing behavior unchanged — all 119 events still emit correctly

**Exit criterion:** `cargo test` passes across workspace. No behavioral change.

### Phase 2: Tier 1 Instrumentation (est. 2-3 days)

1. Add `#[tracing::instrument(skip(self))]` annotations to all Tier 1 functions
2. Add span field values at each annotation site
3. Add `ryeos_tracing::init_subscriber()` to the 5 `rye-tools` binaries that lack it
4. Add 3-5 trace-capture tests using `ryeos_tracing::test::capture_traces()` for critical paths
5. Manual validation: run `ryeosd` with `RUST_LOG=debug` and verify span hierarchy in output

**Exit criterion:** A single directive execution produces a complete span tree from `directive:execute` down to `tool:execute` and `provider:request`. Verify with `RYE_TRACE_JSON=1` and inspect the structured output.

### Phase 3: Tier 2 Instrumentation (est. 2-3 days)

1. Instrument remaining `ryeos-state` operations
2. Instrument `ryeos-engine` lifecycle and trust functions
3. Instrument all 7 `rye-tools` primary work functions
4. Add trace-capture tests for state operations and trust verification

**Exit criterion:** Every binary produces structured spans. State mutations are traceable through the span hierarchy.

### Phase 4: Tier 3 Deep Observability (est. 1-2 days)

1. Add `trace!` calls across all target files
2. Test with `RUST_LOG=trace` to verify output volume is useful but not overwhelming
3. Document expected `RUST_LOG` configurations for common debugging scenarios

**Exit criterion:** `RUST_LOG=trace,ryeos_runtime::verified_loader=trace` shows full file load resolution without flooding unrelated modules.

---

## 6. Testing the Tracing

### 6.1 Unit-Level: Span Assertions

Using the `capture_traces()` harness from `ryeos-tracing`:

```rust
#[cfg(test)]
mod tests {
    use ryeos_tracing::test::capture_traces;

    #[test]
    fn tool_execution_creates_expected_span_tree() {
        let (result, spans) = capture_traces(|| {
            execute_tool("rye/bash/bash", params)
        });

        let tool_span = spans.iter()
            .find(|s| s.name == "tool:execute")
            .expect("tool:execute span should exist");

        assert_eq!(tool_span.fields["tool_name"], "rye/bash/bash");
        assert!(tool_span.fields.contains_key("elapsed_ms"));
    }

    #[test]
    fn directive_execution_contains_thread_and_tool_spans() {
        let (_, spans) = capture_traces(|| {
            execute_directive(directive)
        });

        let dir_span = find_span(&spans, "directive:execute");
        let thread_span = find_child(dir_span, "thread:execute");
        let tool_span = find_child(thread_span, "tool:execute");

        assert!(tool_span.is_some(), "tool span should nest under thread span");
    }
}
```

### 6.2 Integration-Level: Full Execution Trace

Add to `ryeosd/tests/`:

```rust
#[test]
fn full_directive_execution_trace_structure() {
    let (_, spans) = ryeos_tracing::test::capture_traces(|| {
        // Run a minimal directive that spawns a thread and calls a tool
        run_test_directive("test-tracing-directive")
    });

    // Verify the full expected span tree:
    // directive:execute → thread:execute → tool:execute
    //                         \→ state:persist
    let root = find_span(&spans, "directive:execute").unwrap();
    assert!(find_child(root, "thread:execute").is_some());
    assert!(find_child(root, "state:persist").is_some());
}
```

### 6.3 Regression Prevention

Add a CI check that fails if any crate in the workspace has zero `#[tracing::instrument]` annotations on its public API surface. This prevents new modules from being added without instrumentation.

```bash
# .github/workflows/tracing-coverage.sh (conceptual)
# For each crate, verify at least N instrument annotations exist
# Fail if any crate regresses below its Tier target
```

---

## 7. Expected Outcomes

### 7.1 By the Numbers

| Metric | Before | After (Target) |
|---|---|---|
| `#[tracing::instrument]` | 0 | 80-100 |
| `trace!` calls | 3 | 50+ |
| Crates with no tracing | 1 (`lillux` — intentional) | 1 (same) |
| Binaries with subscriber init | 4 of 11 | 11 of 11 |
| Trace-capture tests | 0 | 15-20 |
| Unified subscriber config | No | Yes |
| Structured JSON output | No | Yes (`RYE_TRACE_JSON=1`) |

### 7.2 Developer Experience Improvements

**Before:** "Something went wrong with directive X. I'll sprinkle `println!` statements around."

**After:** `RUST_LOG=debug,ryeosd::execution=trace RYE_TRACE_JSON=1 ryeosd` and get a full structured trace showing every function entry/exit, tool call, state mutation, and provider request — correlated by directive_id and thread_id.

### 7.3 Production Observability

With `RYE_TRACE_JSON=1` and log aggregation (future: OTel export):
- **P99 latency breakdown:** Which step in directive execution is slow?
- **Error correlation:** Which tool failures correlate with which provider?
- **State mutation audit:** Full trace of every chain append, projection rebuild, artifact store.

---

## Appendix A: Annotation Template

Copy-paste template for adding instrument annotations:

```rust
// Simple — no fields, just timing and entry/exit
#[tracing::instrument(skip(self))]
pub fn my_function(&self, arg: String) -> Result<()> { ... }

// With fields — log identifying info in the span
#[tracing::instrument(
    skip(self),
    fields(
        directive_id = %directive.id(),
        kind = ?directive.kind(),
    )
)]
pub fn execute_directive(&self, directive: &Directive) -> Result<()> { ... }

// Async function — instrument handles .await points automatically
#[tracing::instrument(skip(self, params))]
pub async fn invoke_tool(&self, name: &str, params: Value) -> Result<Value> { ... }
```

## Appendix B: RUST_LOG Cheat Sheet

```bash
# Default — info and above
ryeosd

# Debug everything
RUST_LOG=debug ryeosd

# Trace a specific module (e.g., debugging tool resolution)
RUST_LOG=info,ryeosd::execution::runner=trace ryeosd

# Trace all engine internals
RUST_LOG=info,ryeos_engine=trace ryeosd

# JSON output for log aggregation
RYE_TRACE_JSON=1 RUST_LOG=debug ryeosd

# CLI tools with tracing
RUST_LOG=debug rye-fetch directive:my-directive
RUST_LOG=debug rye-verify --path .ai/
```

## Appendix C: What This Document Does NOT Cover

| Topic | Why Excluded | When to Address |
|---|---|---|
| Benchmarks (Criterion) | Different concern, different cadence | Separate roadmap doc |
| OpenTelemetry export | Infrastructure dependency, premature optimization | Phase 2 observability (post-instrumentation) |
| Custom subscriber implementation | `tracing-subscriber` covers current needs | Only if OTel or custom routing required |
| `log` compatibility facade | Project is pure tracing — no migration needed | Never, unless external deps require it |
| Distributed tracing (cross-process) | Single-process architecture currently | If daemon splitting occurs |
| `tracing-chrome` / flame graphs | Nice-to-have profiling tool | After instrumentation is complete |
