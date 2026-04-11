```yaml
id: rust-engine-rewrite
title: "Rust Engine Rewrite"
description: Plan for rewriting the Python resolution/execution engine in Rust, eliminating the Python dependency below userspace and collapsing daemon + engine + Lillux into a single binary.
category: future
tags: [rust, engine, rewrite, performance, deployment, architecture]
version: "0.1.0"
status: planned
```

# Rust Engine Rewrite

> **Status:** Planned — triggered when PyO3 bridge latency becomes visible in metrics or when shipping a single static binary becomes a deployment priority.

> **Read first:** [ryeosd v3 Overview](../../.tmp/ryeosd-v3/00-overview.md), [Future Evolution](../../.tmp/ryeosd-v2/future-evolution.md)

---

## Current Architecture

```
Agents / Clients           (userspace — Python tools, directives, graphs)
    │
    ▼  HTTP / UDS
ryeosd                     [RUST]  (control plane — threads, events, budgets, auth)
    │
    ▼  worker process (stdin/stdout JSON)
rye engine                 [PYTHON]  (execution policy — resolution, trust, chain building)
    │
    ▼  execute(file, params)
Lillux                     [RUST]  (process isolation — fork/exec, IPC)
    │
    ▼  fork/exec
OS subprocess              (tool/directive execution)
```

The daemon owns lifecycle state. Python owns execution policy. The boundary is a worker process that receives an `ExecutionRequest` on stdin and writes an `ExecutionCompletion` to stdout. The worker calls four conceptual operations:

1. **resolve** — walk project → user → system spaces, find the item file
2. **verify_integrity** — check Ed25519 signatures, trust store, content hashes
3. **build_chain** — construct the executor chain from the resolved item
4. **dispatch** — hand off to Lillux for isolated execution

These four operations are the rewrite target.

---

## Target Architecture

```
Agents / Clients           (userspace — Python tools, directives, graphs)
    │
    ▼  canonical refs (syscall interface)
ryeosd                     [RUST]  (OS: threads, events, scheduling, networking)
    │
    ▼  resolve → verify → build_chain → dispatch
rye engine                 [RUST]  (kernel: resolution, trust, integrity, chain building)
    │
    ▼  execute(file, params)
Lillux                     [RUST]  (microkernel: process isolation, IPC)
    │
    ▼  fork/exec
OS subprocess              (hardware)
```

Daemon + kernel + microkernel all Rust, all in-process. No Python runtime below userspace. Bundles and userspace stay Python — tools, directives, runtimes, graphs are all data interpreted by the engine, not compiled into it.

---

## The Contract Is Already Defined

The worker process boundary from the hardening phase defines the exact interface:

```rust
trait ExecutionEngine {
    fn resolve(&self, ctx: &ResolutionContext, item_ref: &str) -> Result<ResolvedItem>;
    fn verify_integrity(&self, item: &ResolvedItem) -> Result<()>;
    fn build_chain(&self, item: &ResolvedItem, params: &Value) -> Result<ExecutorChain>;
    fn dispatch(&self, chain: &ExecutorChain) -> Result<ExecutionCompletion>;
}
```

The swap is mechanical: implement the same trait in Rust, remove the worker process spawn, call the trait directly. No userspace changes. No bundle changes. The syscall interface (canonical refs) is unchanged.

---

## Module-by-Module Rewrite Map

### Resolution (`resolve`)

**What it does today (Python):** Walks the three-tier space (project `.ai/` → user `~/.ai/` → system bundles) searching for a matching item file by canonical ref. Handles extension-based dispatch (`.py`, `.yaml`, `.sh`, `.js`, `.ts`).

**Rust equivalent:** Directory walking with `std::fs`. The search roots and extension priority are already implemented in Rust in `thread_lifecycle.rs` (`search_roots()`, `find_item_path()`). This is ~80% done — the Rust daemon already resolves items for the `/execute` endpoint.

**Remaining work:**

- Move resolution into a standalone engine module
- Support all item types (tools, directives, knowledge, configs)
- Handle `.pth`-style editable installs for bundle discovery
- Parse YAML frontmatter for directive metadata

### Integrity Verification (`verify_integrity`)

**What it does today (Python):** Reads the `# rye:signed:...` header from item files, verifies Ed25519 signatures against the trust store, checks content hashes.

**Rust equivalent:** The daemon's `auth.rs` already implements Ed25519 signature verification for HTTP requests using `ed25519-dalek`. The same crypto library verifies item signatures. The signature format is identical — `# rye:signed:<timestamp>:<content_hash>:<sig_b64>:<signer_fp>`.

**Remaining work:**

- Port `SignatureFormats` parser (Python `rye/utils/signature_formats.py`)
- Port trust store loading (Python `rye/utils/trust_store.py`)
- Implement content hash verification
- Unify with existing `auth.rs` crypto — one Ed25519 verification path

### Chain Building (`build_chain`)

**What it does today (Python):** Constructs the executor chain — resolves the executor ref, loads tool metadata (`__executor_id__`, `__tool_type__`), builds the parameter injection chain, resolves nested dependencies.

**Rust equivalent:** Partially implemented in `thread_lifecycle.rs` (`resolve_tool_item()` parses `__executor_id__` and `__tool_type__`). Full chain building needs:

**Remaining work:**

- Parse all tool metadata formats (Python assignments, YAML frontmatter)
- Build executor chains for directives (the `thread_directive` executor)
- Build executor chains for state graphs
- Handle parameter interpolation and condition evaluation
- Resolve nested executor dependencies

### Dispatch (`dispatch`)

**What it does today (Python):** Hands the built chain to Lillux for isolated execution. Lillux is already Rust — it spawns a subprocess with the executor script and parameters.

**Rust equivalent:** Call Lillux directly. No FFI boundary. The executor chain specifies the script path and parameters; Lillux handles fork/exec/isolation.

**Remaining work:**

- Direct Rust API for Lillux (currently invoked via CLI or Python bindings)
- Pass executor chain as structured data instead of CLI args
- Handle stdio capture and result marshaling

---

## Crypto/Trust Unification

Until the engine rewrite, Ed25519 verification is duplicated:

| Layer                                       | Library         | Purpose                     |
| ------------------------------------------- | --------------- | --------------------------- |
| Rust daemon (`auth.rs`)                     | `ed25519-dalek` | HTTP request authentication |
| Python engine (`rye/primitives/signing.py`) | `cryptography`  | Item integrity verification |

Both use the same keys, the same trust store format, the same signature format. The duplication is mechanical, not architectural.

The engine rewrite collapses both into `ed25519-dalek` with a single trust store implementation:

```rust
struct TrustStore {
    trusted_keys: HashMap<String, VerifyingKey>,
}

impl TrustStore {
    fn verify_item(&self, path: &Path) -> Result<()> { ... }
    fn verify_request(&self, ...) -> Result<Principal> { ... }
}
```

---

## What Stays Python

Userspace stays Python. The engine rewrite does not change:

- **Tool scripts** — `.py`, `.sh`, `.js`, `.ts` files that Lillux executes as subprocesses
- **Directive bodies** — Markdown with YAML frontmatter, interpreted by the directive executor
- **State graph definitions** — YAML DAGs processed by the graph walker
- **Runtime runtimes** — Python code that runs inside Lillux subprocesses
- **MCP tool handlers** — Python tools exposed via the MCP protocol
- **Knowledge entries** — Markdown files, not executed

The boundary is clean: Rust resolves, verifies, builds chains, and dispatches. Python runs inside the dispatched subprocess. The subprocess communicates with the daemon via UDS RPC — this path is unchanged.

---

## Migration Path

### Phase 1: Extract engine trait

Define the `ExecutionEngine` trait in Rust. The current worker process bridge implements it by spawning Python. No behavior change.

```rust
// Current implementation: worker process bridge
struct WorkerBridge { python_path: PathBuf }
impl ExecutionEngine for WorkerBridge { ... }

// Future implementation: native Rust engine
struct RustEngine { trust_store: TrustStore, ... }
impl ExecutionEngine for RustEngine { ... }
```

### Phase 2: Port resolution

Move `resolve()` from Python to Rust. The daemon already does 80% of this. Port the remaining edge cases (editable installs, `.pth` discovery, extension loading order).

Test: both engines resolve the same items for the same refs.

### Phase 3: Port integrity verification

Port signature parsing and trust store loading to Rust. Unify with `auth.rs` crypto.

Test: both engines accept/reject the same items.

### Phase 4: Port chain building

Port executor chain construction. This is the most complex step — tool metadata parsing, parameter interpolation, condition evaluation, nested dependencies.

Test: both engines build the same chains for the same items.

### Phase 5: Direct Lillux dispatch

Replace the Python dispatch path with direct Rust Lillux calls. Remove the worker process spawn. The daemon calls the Rust engine in-process.

Test: end-to-end execution produces the same results.

### Phase 6: Remove Python dependency

Remove `pyo3` from Cargo.toml (if still present). Remove worker process code. The daemon is a single static binary.

---

## Triggers

Build this when:

1. **Metrics show bridge latency** — per-call overhead on the resolve → verify → build_chain → dispatch path becomes visible. Current workloads are in the noise.

2. **Single binary deployment** — shipping `ryeosd` without a Python runtime becomes a distribution priority. Docker images shrink from ~1GB to ~50MB.

3. **Worker process overhead** — spawning a Python process per execution adds measurable latency for high-frequency tool runs (graph walkers with 50+ steps).

4. **Trust unification** — maintaining two Ed25519 verification paths becomes a maintenance burden or security concern.

---

## What This Enables

- **Zero-dependency daemon** — single static Rust binary, no Python, no pip, no venv
- **Sub-millisecond resolution** — no process spawn, no Python import time
- **Unified crypto** — one Ed25519 path, one trust store, one signature verifier
- **JIT optimizations** — hot-path caching, chain fusion, speculative resolution (see future-evolution.md)
- **Capability refs** — `@cap:` suffix verification in the same binary, no duplication
- **Temporal resolution** — `@t:` suffix with CAS lookup, all in-process

---

## Relationship to Other Documents

| Document                                                     | Relationship                                                                    |
| ------------------------------------------------------------ | ------------------------------------------------------------------------------- |
| [ryeosd v3 Hardening](../../.tmp/ryeosd-v3/16-hardening.md)  | Worker process architecture defines the engine trait boundary                   |
| [Future Evolution](../../.tmp/ryeosd-v2/future-evolution.md) | JIT optimizations, capability refs, temporal resolution depend on native engine |
| [Sovereign Inference](sovereign-inference.md)                | Single-binary deployment enables edge node distribution                         |
| [Cluster Bootstrap](cluster-bootstrap.md)                    | Static binary simplifies multi-node deployment                                  |
