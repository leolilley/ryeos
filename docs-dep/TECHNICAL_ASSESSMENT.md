# Technical Assessment: RYE OS Architecture

**Date:** 2026-02-18  
**Scope:** Complete technical review of RYE OS design, implementation, and production readiness  
**Perspective:** Neutral, evidence-based assessment

---

## Executive Summary

RYE OS is a **self-hosting, data-driven framework** for building portable agent workflows with cryptographic integrity, multi-process orchestration, and a registry ecosystem. The system is organized as a microkernel (Lilux) with a 4-tool MCP surface (Rye), where the agent runtime itself — the LLM loop, safety harness, orchestrator, hooks, loaders, providers — lives inside the same `.ai/tools/` directory structure it manages. This self-hosting property is architecturally distinctive: RYE runs on its own abstractions.

**Key distinction:** RYE is not an LLM execution engine — it's infrastructure for agents to manage, execute, and share workflows. The LLM is the execution engine; RYE provides the scaffolding, integrity guarantees, and orchestration primitives.

---

## 1. Core Architecture: Four Layers

### Layer 1: Lilux (Microkernel)

**Location:** `lilux/lilux/`

The lowest layer. Pure I/O primitives with no business logic:

| Primitive             | Purpose                                              |
| --------------------- | ---------------------------------------------------- |
| `SubprocessPrimitive` | Async subprocess execution with two-stage templating |
| `HttpClientPrimitive` | HTTP requests with auth, retry, templating           |
| `signing.py`          | Ed25519 keypair generation, sign, verify             |
| `integrity.py`        | Content hashing (SHA256)                             |
| `lockfile.py`         | Lockfile I/O (read/write/manage)                     |
| `env_resolver.py`     | Environment variable resolution with fallbacks       |
| `auth.py`             | Authentication storage                               |
| `schema_validator.py` | JSON Schema validation                               |

Lilux has no knowledge of `.ai/`, spaces, or items. It only does what it's told.

### Layer 2: Rye (MCP Server + Orchestration Engine)

**Location:** `rye/rye/`

The MCP server exposes exactly **4 tools** to agents:

| Tool        | Purpose                                   |
| ----------- | ----------------------------------------- |
| **search**  | Find items across spaces/registry         |
| **load**    | Read item content or copy between spaces  |
| **execute** | Run directives, tools, or knowledge items |
| **sign**    | Validate and Ed25519-sign items           |

Core subsystems:

- **`tools/`** — The 4 MCP tool implementations (search, load, execute, sign)
- **`executor/`** — PrimitiveExecutor (chain resolution), ChainValidator, LockfileResolver
- **`handlers/`** — Per-type item handlers (directive, tool, knowledge)
- **`utils/`** — MetadataManager, ParserRouter, TrustStore, integrity, validators, signature formats, path resolution — all data-driven via extractors

### Layer 3: The `.ai/` Data Bundle (Self-Hosted Standard Library)

**Location:** `rye/rye/.ai/`

This is the architecturally significant layer. RYE ships its own agent runtime, tools, configs, runtimes, and directives **inside** the same `.ai/` directory structure that user items live in. The agent system runs on itself.

**116 Python files, 30+ YAML configs** organized into:

| Namespace                | Contents                                                                       | File Count |
| ------------------------ | ------------------------------------------------------------------------------ | ---------- |
| `rye/agent/threads/`     | LLM loop, orchestrator, safety harness, loaders, persistence, adapters, events | ~40 files  |
| `rye/core/`              | Extractors, parsers, runtimes, primitives, bundler, sinks, registry, telemetry | ~30 files  |
| `rye/file-system/`       | read, write, edit_lines, glob, grep, ls                                        | 6 files    |
| `rye/web/`               | websearch, webfetch                                                            | 2 files    |
| `rye/mcp/`               | MCP server management (add, list, refresh, remove)                             | 3 files    |
| `rye/lsp/`               | Language Server Protocol integration                                           | 1 file     |
| `rye/bash/`              | Shell command execution                                                        | 1 file     |
| `rye/primary/`           | In-thread wrappers for the 4 MCP tools                                         | 4 files    |
| `rye/agent/providers/`   | LLM provider configs (OpenAI, Anthropic)                                       | 2 files    |
| `rye/agent/permissions/` | Capability definitions, capability tokens                                      | ~10 files  |

**Why this matters:** Every file listed above is itself a signed tool or YAML config. The agent runtime is subject to the same integrity verification, space precedence, and signing rules as user-authored tools. A project can override any of these by placing a file at the same path in its own `.ai/tools/`. This is not just "convention" — it's the core architectural property.

### Layer 4: Registry API

**Location:** `services/registry-api/`

Separate FastAPI service with Supabase backend. Handles push/pull/search with server-side validation and registry signing. Documented in `docs/registry/`.

---

## 2. Design Philosophy

### Everything Is Data

This phrase is used loosely in many frameworks. In RYE, it's literal:

| Component              | Data Format            | Location                                        | Overridable? |
| ---------------------- | ---------------------- | ----------------------------------------------- | ------------ |
| Runtimes               | YAML                   | `.ai/tools/rye/core/runtimes/`                  | Yes          |
| Parsers                | Python                 | `.ai/tools/rye/core/parsers/`                   | Yes          |
| Extractors             | YAML/Python            | `.ai/tools/rye/core/extractors/`                | Yes          |
| Signature formats      | Loaded from extractors | Via AST parsing at startup                      | Yes          |
| Error classification   | YAML                   | `.ai/tools/rye/agent/threads/config/`           | Yes          |
| Resilience config      | YAML                   | `.ai/tools/rye/agent/threads/config/`           | Yes          |
| Hook conditions        | YAML                   | `.ai/tools/rye/agent/threads/config/`           | Yes          |
| Coordination config    | YAML                   | `.ai/tools/rye/agent/threads/config/`           | Yes          |
| LLM provider configs   | YAML                   | `.ai/tools/rye/agent/providers/`                | Yes          |
| Capability definitions | YAML                   | `.ai/tools/rye/agent/permissions/capabilities/` | Yes          |
| Search field weights   | Loaded from extractors | Via AST parsing at startup                      | Yes          |
| Budget ledger schema   | YAML                   | `.ai/tools/rye/agent/threads/config/`           | Yes          |

Every entry above is loaded at runtime via a loader pattern (AST parsing, YAML parsing, or `importlib`) and resolved through the 3-tier space system. None is hardcoded.

### Three-Tier Space Precedence

Items resolve through:

```
project/.ai/{type}/  (highest priority — local overrides)
→ ~/.ai/{type}/      (user defaults)
→ site-packages/rye/.ai/{type}/  (system bundles — lowest priority)
```

This applies to tools, directives, knowledge, runtimes, parsers, extractors, configs, and lockfiles. The precedence is enforced consistently across search, load, execute, and sign.

### LLM-Native Execution

Directives are **not** parsed into a DAG or state machine. The directive's process steps are free-form natural language + pseudo-code that the LLM reads and follows. RYE's infrastructure consumes the metadata (limits, permissions, model tier, hooks, inputs/outputs) and provides the scaffolding; the LLM decides how to execute the steps.

This is a deliberate inversion:

| Framework | Who orchestrates? | Who provides tools? |
| --------- | ----------------- | ------------------- |
| LangGraph | Framework (DAG)   | Framework           |
| CrewAI    | Framework (roles) | Framework           |
| **RYE**   | **LLM**           | **Framework**       |

---

## 3. Tool Execution Engine

### PrimitiveExecutor: Three-Layer Routing

**File:** `rye/rye/executor/primitive_executor.py` (1232 lines)

Tools declare their execution target via `__executor_id__`:

```
Layer 3: Tool (e.g., websearch.py)
  __executor_id__ = "rye/core/runtimes/python_script_runtime"
         ↓
Layer 2: Runtime (python_script_runtime.yaml)
  executor_id: rye/core/primitives/subprocess
         ↓
Layer 1: Primitive (SubprocessPrimitive)
  → Lilux async execution
```

**Chain resolution:**

1. Load tool metadata via AST parsing (cached with hash-based invalidation)
2. Follow `__executor_id__` pointers recursively (max depth: 10)
3. At each level, resolve ENV_CONFIG (interpreter paths, env vars, anchor paths)
4. Validate chain: space compatibility, I/O types, version constraints, no cycles
5. If lockfile exists, verify integrity hashes for every chain element
6. Verify Ed25519 signature on every chain element before execution
7. Execute via the terminal Lilux primitive

### Runtimes Are Data

Adding a new language runtime is a YAML file, not code:

```yaml
# python_script_runtime.yaml
version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess

env_config:
  interpreter:
    type: venv_python
    venv_path: .venv
    var: RYE_PYTHON
    fallback: python3

anchor:
  enabled: true
  mode: auto
  markers_any: ["__init__.py", "pyproject.toml"]
  root: tool_dir
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"]

verify_deps:
  enabled: true
  scope: anchor
  extensions: [".py", ".yaml", ".yml", ".json"]

config:
  command: "${RYE_PYTHON}"
  args:
    [
      "{tool_path}",
      "--params",
      "{params_json}",
      "--project-path",
      "{project_path}",
    ]
  timeout: 300
```

Six runtimes ship: `python_script`, `python_function`, `node`, `bash`, `mcp_stdio`, `mcp_http`. Each is a signed YAML file. Users can add more by dropping a YAML file in the right place.

### Anchor System

Runtimes support an anchor system for multi-file tools. When `anchor.enabled: true` and marker files are present (e.g., `__init__.py`), the executor:

1. Resolves the anchor root directory
2. Prepends anchor paths to env vars (PYTHONPATH, NODE_PATH)
3. Verifies integrity of **all dependency files** within the anchor scope before execution

This enables tools that are Python packages (with `__init__.py`, submodules, `lib/` directories) — not just single scripts.

### Lockfile Pinning

**File:** `rye/rye/executor/lockfile_resolver.py`

Lockfiles pin tool versions with integrity hashes, following the same 3-tier precedence (project → user → system). When a lockfile exists for a tool:

1. Root tool integrity hash is verified
2. Every chain element's integrity hash is verified
3. If any hash mismatches, execution is refused

---

## 4. Multi-Agent Threading

### Thread Architecture

All orchestration flows through one tool: `rye/agent/threads/thread_directive`. The full lifecycle:

1. **Load directive** — Parse XML metadata, extract limits/permissions/hooks/model
2. **Resolve limits** — defaults → directive → overrides → parent caps via `min()`; depth decrements per level
3. **Check spawn limit** — Parent's spawn count checked against limit
4. **Build SafetyHarness** — Fail-closed capability enforcement + limit checking + hook evaluation
5. **Reserve budget** — Ledger tracks spend per thread with parent cascade
6. **Resolve LLM provider** — Data-driven: model tier → provider config YAML → HttpProvider
7. **Run LLM loop** — `runner.py`: call LLM → parse tool calls → permission check → dispatch → hooks → repeat

### Process Spawning with os.fork()

When `async=true`, the thread forks:

```python
child_pid = os.fork()
if child_pid == 0:
    os.setsid()  # Detach from parent process group
    # Redirect stdio to /dev/null
    result = asyncio.run(runner.run(...))
    # Finalize: report spend, cascade to parent, update registry
    os._exit(0)
else:
    return {"success": True, "thread_id": thread_id, "pid": child_pid}
```

Each child is a **separate OS process** with its own memory, interpreter, and event loop. True parallelism — not async/await, not threading.

### Capability Attenuation

Capabilities flow downward and can only narrow:

```
Root thread: [rye.execute.tool.rye.file-system.*, rye.execute.tool.rye.web.*, rye.search.tool]
  ↓ Child spawned with subset
Child thread: [rye.execute.tool.rye.file-system.read, rye.search.tool]
  ↓ Grandchild inherits child's caps (or less)
Grandchild: [rye.execute.tool.rye.file-system.read]
```

Permission check is **fail-closed**: no capabilities declared → all actions denied. Enforcement happens at dispatch time in `runner.py` before every tool call, using `fnmatch` against capability strings.

### Capability Tokens

**File:** `rye/rye/.ai/tools/rye/agent/permissions/capability_tokens/capability_tokens.py` (683 lines)

Full Ed25519-signed capability tokens with:

- **Delegation chains** — Tokens track parent token IDs
- **Audience binding** — Prevents cross-service replay
- **Expiry** — UTC-based time-to-live
- **Cryptographic verification** — Signed with Ed25519, verified before use

### Budget Hierarchy

- Each thread has a spend ledger entry
- Children **reserve** budget from parent's remaining allowance
- Actual spend **cascades upward** on completion
- Ledger tracks: reserved, actual, released, final_status

### Orchestrator Operations

**File:** `rye/rye/.ai/tools/rye/agent/threads/orchestrator.py`

| Operation           | Purpose                                           |
| ------------------- | ------------------------------------------------- |
| `wait_threads`      | Wait for child threads (asyncio.Event, zero-poll) |
| `cancel_thread`     | Cancel a running thread                           |
| `kill_thread`       | Kill thread process (SIGKILL)                     |
| `get_status`        | Read thread.json for status/cost                  |
| `list_active`       | List all active threads                           |
| `aggregate_results` | Collect results from multiple threads             |
| `get_chain`         | Get parent→child chain for a thread               |
| `chain_search`      | Search across thread transcripts                  |
| `read_transcript`   | Read a thread's execution transcript              |
| `resume_thread`     | Resume a stopped thread with new context          |
| `handoff_thread`    | Context-limited continuation (summarize + resume) |

### Thread Resumption and Handoff

When a thread approaches context limits (configurable via `coordination.yaml`):

1. A **summary directive** is spawned to summarize the conversation so far
2. Summary + trailing turns + new message are assembled under a token ceiling
3. The thread resumes with the compressed context

Configuration is data-driven:

```yaml
continuation:
  trigger_threshold: 0.9
  summary_directive: "rye/agent/threads/thread_summary"
  summary_model: "fast"
  resume_ceiling_tokens: 16000
  summary_max_tokens: 4000
```

---

## 5. Data-Driven Composition: The Loader Pattern

### How Config Loading Works

**File:** `rye/rye/.ai/tools/rye/agent/threads/loaders/config_loader.py`

Base class loads YAML configs with project-override support:

```python
class ConfigLoader:
    def load(self, project_path: Path) -> Dict:
        # 1. Load system config (shipped with rye)
        config = self._load_yaml(system_path)
        # 2. Load project override (.ai/config/{name})
        if project_config_path.exists():
            config = self._merge(config, project_config)
        return config
```

Deep merge with `extends` support and list-by-id merging. This means projects can:

- Override error classification patterns
- Add custom retry policies
- Change default limits
- Add hook conditions
- Modify coordination behavior

All without touching RYE source code — just drop a YAML file at `.ai/config/`.

### What's Configurable via Data

| Loader                | Config File                 | Controls                                                           |
| --------------------- | --------------------------- | ------------------------------------------------------------------ |
| `resilience_loader`   | `resilience.yaml`           | Default limits, retry policies, child policy, concurrency caps     |
| `error_loader`        | `error_classification.yaml` | Error patterns (regex/path matching), retry strategies, categories |
| `hooks_loader`        | `hook_conditions.yaml`      | Built-in and infra hooks, condition DSL                            |
| `events_loader`       | `events.yaml`               | Event types, emission rules                                        |
| `coordination_loader` | `coordination.yaml`         | Wait timeouts, continuation/resume config, orphan detection        |
| `condition_evaluator` | Inline DSL                  | Hook condition evaluation (any/all/not combinators)                |

### Error Classification Is Declarative

```yaml
patterns:
  - id: "http_429"
    category: "rate_limited"
    retryable: true
    match:
      any:
        - path: "status_code"
          op: "eq"
          value: 429
        - path: "error.message"
          op: "regex"
          value: "rate limit|too many requests"
    retry_policy:
      type: "use_header"
      header: "retry-after"
      fallback:
        type: "exponential"
        base: 2.0
        max: 60.0
```

Error classification, retry policy selection, and delay calculation are all driven by this YAML. The error_loader evaluates patterns using the condition DSL (operators: eq, ne, gt, in, contains, regex, exists; combinators: any, all, not). No code changes needed to add a new error pattern.

---

## 6. LLM Provider System

### Providers Are Data

**Files:** `rye/rye/.ai/tools/rye/agent/providers/openai.yaml`, `anthropic.yaml`

Each provider config is a signed YAML file that declares:

- **Tier mapping** — Model name aliases (e.g., `general` → `gpt-4o-mini`)
- **Pricing** — Per-million-token costs for cost tracking
- **API config** — URL, auth, headers, body template, timeout, retry
- **Tool use config** — Native vs. text-parsed, tool definition format, response parsing

The `provider_resolver` maps model names/tiers to provider configs, the `provider_adapter` normalizes the interface, and `http_provider` executes calls using the config. Adding a new LLM provider is a YAML file.

---

## 7. Cryptographic Integrity

### Signing Pipeline

1. Content normalized (strip existing signature)
2. SHA256 hash computed
3. Hash signed with Ed25519 private key
4. Signature embedded as format-appropriate comment: `# rye:signed:TIMESTAMP:HASH:SIG:FINGERPRINT`

Signature format (comment prefix) is loaded from extractors — Python uses `#`, Markdown uses `<!--`, YAML uses `#`. Data-driven, not hardcoded.

### Trust Store

**File:** `rye/rye/utils/trust_store.py`

- Own pubkey auto-trusted on keygen
- Registry pubkey pinned on first pull (TOFU — Trust On First Use)
- Peer keys manually trusted via sign tool
- Lookup: fingerprint → PEM file in `~/.ai/trusted_keys/`

### Integrity Enforcement

**File:** `rye/rye/utils/integrity.py`

`verify_item()` runs 4 checks:

1. Signature exists
2. Content hash matches embedded hash
3. Ed25519 signature valid
4. Signing key is in trust store

This runs on **every chain element** before tool execution, and on lockfile entries when lockfiles are present. Failed verification raises `IntegrityError` and blocks execution.

---

## 8. Bundle System

### What Bundles Are

**File:** `rye/rye/.ai/tools/rye/core/bundler/bundler.py` (759 lines)

Bundles are **signed manifests** that cover groups of items — directives, tools, knowledge, and non-signable assets (images, data files). A bundle manifest:

```yaml
bundle:
  id: rye-core
  version: 0.1.0
  type: package
  created_at: "2026-02-15T05:39:45Z"
files:
  .ai/directives/rye/core/create_directive.md:
    sha256: c7deaec3...
    inline_signed: false
  .ai/tools/rye/agent/threads/runner.py:
    sha256: 31a5fb48...
    inline_signed: true
```

The manifest itself is signed (line 1). Each file has a SHA256 hash. Files that support inline signatures have `inline_signed: true` — these get dual protection (manifest hash + inline Ed25519).

Bundle operations: `create` (walk directories, hash files, sign manifest), `create-package` (for pip-installable packages), `verify` (check manifest signature + all file hashes), `inspect` (parse without verification), `list`.

### Why This Matters

Bundles solve the "assets can't have inline signatures" problem. A tool that includes a `data/` directory with JSON fixtures or a `templates/` directory with HTML files can have those files integrity-verified through the manifest — even though they can't carry `rye:signed:` comments.

---

## 9. MCP Meta-Layer

### MCP Server Management

**File:** `rye/rye/.ai/tools/rye/mcp/manager.py` (581 lines)

RYE doesn't just consume MCP — it manages other MCP servers:

| Action    | Purpose                                 |
| --------- | --------------------------------------- |
| `add`     | Register an MCP server (stdio or HTTP)  |
| `list`    | List configured servers and their tools |
| `refresh` | Re-discover tools from a server         |
| `remove`  | Deregister a server                     |

When a server is added, the manager:

1. Connects to the server and discovers its tools
2. **Auto-generates tool stubs** as `.ai/tools/mcp/{server_name}/{tool_name}.yaml`
3. Each stub has `executor_id: rye/core/runtimes/mcp_stdio_runtime` (or `mcp_http_runtime`)
4. Stubs are signed with placeholder signatures (must be re-signed)

This means external MCP servers' tools become first-class RYE items — searchable, executable through the chain, subject to permissions and integrity checks.

---

## 10. Search System

### Implementation

**File:** `rye/rye/tools/search.py` (1153 lines)

Search features:

- Boolean operators (AND, OR, NOT)
- Wildcards, phrase search (quotes)
- Field-specific search with configurable weights
- Fuzzy matching (Levenshtein distance)
- Proximity search
- BM25-inspired field-weighted scoring
- Namespace filtering via capability-format scopes
- 3-tier space resolution with source-priority tie-breaking

**Data-driven field weights** — Search field weights and extraction rules are loaded from extractor files via AST parsing. New item types are auto-discovered. Search behavior changes by modifying extractor data, not search code.

---

## 11. Self-Hosting Property

This is the system's most distinctive architectural property and deserves explicit treatment.

RYE's agent runtime — the LLM loop (`runner.py`), safety harness, orchestrator, thread directive, all loaders, all adapters, all persistence modules — lives inside `.ai/tools/rye/agent/`. These are signed Python tools with `__executor_id__`, `__version__`, `__category__` metadata. They are resolved through the same 3-tier space system, verified via the same integrity checks, and subject to the same override mechanics as any user-authored tool.

**Implications:**

1. **Overridable runtime** — A project can replace `runner.py` or `safety_harness.py` by placing a file at the same relative path in its `.ai/tools/`
2. **Self-verifying** — The agent runtime verifies its own integrity on execution
3. **Versionable** — The runtime is covered by the `rye-core` bundle manifest with per-file hashes
4. **Bootstrapping** — RYE's standard library of directives (create_tool, create_directive, create_knowledge) are themselves directives managed by the system they define

No other agent framework in production has this property. LangGraph, CrewAI, AutoGen, and raw MCP all have hardcoded runtimes that users cannot override through the framework's own mechanisms.

---

## 12. What's Complete, What's Incomplete

### ✅ Complete & Solid

| Component                     | Evidence                                                                                |
| ----------------------------- | --------------------------------------------------------------------------------------- |
| MCP server (4 tools)          | Fully functional, all tests pass                                                        |
| Search system                 | Boolean, fuzzy, proximity, BM25 scoring, data-driven field weights                      |
| Ed25519 signing + trust store | Signing, verification, TOFU pinning, trust management                                   |
| Tool execution chain          | 3-layer routing, chain validation, caching, lockfile pinning, anchor system             |
| LLM loop                      | Full event loop, limit checking, permission enforcement, hooks, error retry             |
| Process spawning              | os.fork(), daemonization, environment inheritance, parent context injection             |
| Permission system             | Fail-closed capabilities, fnmatch wildcards, attenuation, capability tokens             |
| Budget hierarchy              | Per-thread ledger, reservation, cascade, release                                        |
| Data-driven config            | 6+ loader classes, all configs overridable, deep merge with list-by-id                  |
| Provider system               | Data-driven YAML configs, tier mapping, pricing, tool_use modes                         |
| Bundle system                 | Create, verify, inspect manifests with per-file hashes                                  |
| Registry server               | FastAPI + Supabase, validation, registry signing, bundle push/pull                      |
| Registry client               | OAuth PKCE, push/pull/search/publish/unpublish/delete (2100+ lines)                     |
| Orchestrator operations       | wait, cancel, kill, status, list, aggregate, chain, search, transcript, resume, handoff |
| Thread resumption             | Summary directive spawning, context compression, configurable ceiling                   |
| MCP server management         | Add/list/refresh/remove external MCP servers, auto-stub generation                      |
| Error classification          | Declarative YAML patterns with condition DSL, retry policy resolution                   |

### ⚠️ Partial / Untested at Scale

| Component                   | Notes                                                                            |
| --------------------------- | -------------------------------------------------------------------------------- |
| Cost tracking               | Ledger framework complete; pricing from YAML configs, untested at scale          |
| Hook system                 | thread_started, error, limit, after_step implemented; custom hooks possible      |
| Registry client integration | Tool exists and works; no auto-search (agent must explicitly call registry tool) |

### ❌ Missing

| Component                | Notes                                                |
| ------------------------ | ---------------------------------------------------- |
| Windows support          | Uses `os.fork()` — Unix/Linux only                   |
| OS-level resource limits | No CPU/memory/disk quotas (cost-based limits only)   |
| Container/VM isolation   | Permissions are advisory (Python level, not seccomp) |
| Key revocation           | Trust store has no revocation mechanism              |

---

## 13. Compared to Existing Frameworks

### LangGraph

LangGraph enforces deterministic state graphs with typed transitions. Agents follow framework-defined execution paths. RYE rejects deterministic execution by design — the LLM reads free-form instructions and decides the path. LangGraph gives you reproducibility; RYE gives you adaptability.

Where they intersect: both handle multi-step workflows. Where they diverge: LangGraph has no signing, no registry, no space system, no capability attenuation, no self-hosting. RYE has no typed state graphs.

### CrewAI

CrewAI is role-based: agents have fixed roles (Manager, Researcher) with hardcoded communication patterns. RYE is directive-based: any thread can play any role depending on the directive it executes. CrewAI's tool access is per-agent; RYE's is per-directive with cryptographic capabilities.

CrewAI's runtime is hardcoded Python; RYE's is overridable data.

### AutoGen

AutoGen focuses on human-in-the-loop conversation patterns between agents. RYE focuses on autonomous, signed, budget-controlled workflows. Different layers: AutoGen is conversation-level; RYE is artifact-level with process-level isolation.

### Raw MCP

MCP provides transport (stdio, HTTP) and tool schema definitions. RYE builds an entire operating system layer on top: search/load/execute/sign abstraction, 3-tier space system, signing/integrity, multi-process orchestration, capability-based permissions, registry ecosystem, data-driven runtimes, and self-hosting.

### What No Other Framework Does

1. **Self-hosting runtime** — The agent system runs as items managed by the system it defines
2. **Cryptographic integrity on the execution path** — Every chain element verified before execution
3. **Data-driven runtimes** — New language runtimes are YAML files, not code
4. **Signed, portable, versioned workflows** — Directives + tools + knowledge as a cohesive, integrity-verified artifact system
5. **Declarative everything** — Error classification, retry policies, resilience, provider configs, capabilities, hook conditions — all YAML

---

## 14. Production Readiness

### Safe For

- **Local development** — Single user, single machine. Fully functional.
- **Small teams** — Shared `.ai/` directory with signing for trust.
- **Multi-agent workflows** — Thread spawning, budget control, capability attenuation all work.
- **Registry sharing** — Push/pull/sign flow works end-to-end.

### Needs Work For

- **Multi-OS deployment** — Windows support missing (replace `os.fork()` with `multiprocessing`)
- **Production agent farms** — OS-level resource limits, container isolation
- **Large-scale registry** — Formal threat model, key revocation, supply-chain testing
- **Cost tracking at scale** — Validate pricing accuracy against actual provider bills

---

## 15. Assessment

### Novelty

RYE's individual components — signing, capability-based security, subprocess execution, config-driven behavior — exist separately in other systems. What's novel is the combination and the self-hosting property:

- The **signing + chain verification + lockfile pinning** combination creates an integrity-verified execution path that has no equivalent in agent frameworks
- The **self-hosting** property (runtime as data items in the system it defines) is architecturally unique
- The **declarative runtime system** (YAML-defined language runtimes with anchor/verify_deps) is rare even outside agent frameworks
- The **data-driven depth** — where error classification, retry policies, provider configs, hook conditions, search weights, signature formats, and resilience configs are all swappable YAML — goes further than any comparable system

### Completeness

~21,000 lines of Python across 116 files, plus 30+ YAML configs, plus a full FastAPI registry service. The system is not a prototype — it has caching with hash-based invalidation, atomic file writes, deep merge with list-by-id semantics, context-aware tool result guarding, and configurable thread resumption via summary directives.

### What It Is

An operating system layer for AI agents. Not a chatbot framework. Not an LLM wrapper. Infrastructure for building, signing, sharing, and executing portable agent workflows with cryptographic integrity and hierarchical budget control.

---

## References

### Key Files

- **MCP Server:** `rye-mcp/rye_mcp/server.py`
- **Core Tools:** `rye/rye/tools/` (search, load, execute, sign)
- **Executor:** `rye/rye/executor/primitive_executor.py`
- **LLM Loop:** `rye/rye/.ai/tools/rye/agent/threads/runner.py`
- **Thread Directive:** `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py`
- **Safety Harness:** `rye/rye/.ai/tools/rye/agent/threads/safety_harness.py`
- **Orchestrator:** `rye/rye/.ai/tools/rye/agent/threads/orchestrator.py`
- **Permission Enforcement:** `rye/rye/.ai/tools/rye/agent/threads/runner.py#L262-L285`
- **Capability Tokens:** `rye/rye/.ai/tools/rye/agent/permissions/capability_tokens/capability_tokens.py`
- **Config Loaders:** `rye/rye/.ai/tools/rye/agent/threads/loaders/`
- **Provider Configs:** `rye/rye/.ai/tools/rye/agent/providers/`
- **Runtimes:** `rye/rye/.ai/tools/rye/core/runtimes/`
- **Bundler:** `rye/rye/.ai/tools/rye/core/bundler/bundler.py`
- **Trust Store:** `rye/rye/utils/trust_store.py`
- **Integrity:** `rye/rye/utils/integrity.py`
- **Registry Client:** `rye/rye/.ai/tools/rye/core/registry/registry.py`
- **Registry Server:** `services/registry-api/registry_api/main.py`
