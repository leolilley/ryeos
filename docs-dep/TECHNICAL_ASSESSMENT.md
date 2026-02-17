# Technical Assessment: RYE OS Architecture

**Date:** 2026-02-18  
**Scope:** Complete technical review of RYE OS design, implementation, and production readiness  
**Perspective:** Neutral, evidence-based assessment

---

## Executive Summary

RYE OS is a **well-architected, partially complete framework** for building portable agent workflows with cryptographic integrity and registry-aware execution. The system successfully integrates data-driven tool composition, LLM-native orchestration, and multi-process thread spawning.

**Key distinction:** RYE is not an LLM execution engine — it's infrastructure for agents to manage, execute, and share workflows. Execution happens in the agent loop; RYE provides the scaffolding.

---

## 1. Core Architecture Overview

### Design Philosophy

RYE operates on three foundational principles:

1. **Everything is data** — Directives, tools, knowledge stored as versioned files in `.ai/` with cryptographic signatures
2. **Data-driven composition** — Tool executors, parsers, extractors, hooks, and loaders are swappable implementations loaded from filesystem
3. **LLM-native workflows** — Directives are instructions for LLMs (free-form XML), not parsed specifications. The LLM is the execution engine

This inverts typical agent frameworks:

- **CrewAI/LangGraph**: LLM calls functions; framework orchestrates
- **RYE**: Framework provides tools; LLM orchestrates. "Execute this directive" = "here's an instruction to follow"

### Three-Tier Space System

Items resolve through precedence:

```
project/.ai/{type}/ (highest priority)
→ ~/.ai/{type}/
→ system bundles / installed packages (lowest priority)
```

This enables:

- **Local overrides** — Project directives shadow user/system versions
- **Portable workflows** — Same project works anywhere (same `.ai/` structure)
- **Shared libraries** — System bundles provide standard tools

---

## 2. MCP Server & Core Tools

### Implementation

**File:** [rye-mcp/server.py#140-160](file:///home/leo/projects/rye-os/rye-mcp/rye_mcp/server.py#L140-L160)

The server exposes exactly **4 tools** via MCP:

| Tool        | Purpose                                   | Implementation                                                               |
| ----------- | ----------------------------------------- | ---------------------------------------------------------------------------- |
| **search**  | Find items across spaces/registry         | Keyword search with fuzzy matching, field weighting, BM25-inspired scoring   |
| **load**    | Read item content or copy between spaces  | File I/O with metadata extraction                                            |
| **execute** | Run directives, tools, or knowledge items | Routes to PrimitiveExecutor (tools) or returns parsed structure (directives) |
| **sign**    | Validate and Ed25519-sign items           | Schema-driven validation + cryptographic signing                             |

### What Makes Search Non-Trivial

**File:** [search.py#53-126](file:///home/leo/projects/rye-os/rye/rye/tools/search.py#L53-L126)

Search field weights and extraction rules are **loaded from extractor files via AST parsing**:

```python
def _load_extractor_data(project_path: Optional[Path] = None):
    """Load SEARCH_FIELDS, EXTRACTION_RULES, and PARSER from all extractors via AST."""
```

This means:

- New item types auto-discovered (no hardcoding)
- Search behavior is data-driven
- Extractors define how to parse their own file types

**Not novel by itself**, but rare pattern in agent frameworks.

### What Execute Actually Does

**File:** [execute.py#134-186](file:///home/leo/projects/rye-os/rye/rye/tools/execute.py#L134-L186)

```python
# For directives: parse and return
result = {
    "status": "success",
    "type": ItemType.DIRECTIVE,
    "item_id": item_id,
    "data": parsed,        # ← Parsed XML structure
    "inputs": inputs,
    "instructions": DIRECTIVE_INSTRUCTION,  # ← Generic execution instruction
}

# For tools: route to PrimitiveExecutor
result: ExecutionResult = await executor.execute(...)
```

**Critical insight:**

- **Directives are returned as data** — the LLM reads the parsed XML and follows the instructions
- **Tools are executed deterministically** — routed through executor chain (tool → runtime → primitive)
- This is intentional: "Directives are free-form so LLMs can optimize prompting"

---

## 3. Directive System

### Parsing: Data-Driven, Not Prescriptive

**XML Structure Example:**

```xml
<directive name="scrape_website" version="1.0.0">
  <metadata>
    <description>Scrape a website and extract structured data</description>
    <limits>
      <turns>20</turns>
      <tokens>50000</tokens>
      <spend>2.00</spend>
    </limits>
    <permissions>
      <execute><tool>rye.file-system.fs_write</tool></execute>
      <execute><tool>rye.web.websearch</tool></execute>
    </permissions>
  </metadata>
  <inputs>
    <input name="url" type="string" required="true">Website URL to scrape</input>
  </inputs>
  <process>
    <step name="fetch">Fetch the page HTML</step>
    <step name="parse">Parse and extract structured data</step>
    <step name="save">Save results to file</step>
  </process>
</directive>
```

**What RYE does:**

1. Extracts metadata: limits, model tier, permissions, inputs
2. Validates inputs against declared schema
3. **Returns the full XML to the LLM** with instruction: "Execute these steps"

**What RYE does NOT do:**

- Parse the `<process>` steps and execute them sequentially
- Enforce that steps run in declared order
- Parse natural language descriptions

**Why:** "The whole point is that [the directive] is free-form so the LLM can optimize its own prompting. There's no point in any deterministic parsing outside the metadata inputs and outputs."

### Hooks: Event-Driven, Conditionally Evaluated

**Locations:** [thread_directive.py#312-322](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/thread_directive.py#L312-L322), [safety_harness.py#141-150](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/safety_harness.py#L141-L150)

Hooks execute on events:

- `thread_started` — load context (identity, rules, knowledge)
- `error` — classify error, decide retry/abort
- `limit` — respond to cost/time/token limits
- `after_step` — evaluate conditions, emit telemetry

**Implementation:**

```python
async def run_hooks(
    self,
    event: str,
    context: Dict,
    dispatcher: Any,
    thread_context: Dict,
) -> Optional[Dict]:
    """Evaluate hooks for error/limit/after_step events."""
```

Hooks can:

- Load knowledge/tools via dispatcher
- Evaluate conditions (custom DSL)
- Return control actions (retry, abort, continue)
- Emit telemetry

Not a complete workflow engine, but solid event system for orchestration.

### Signing: Integrity Verification, Not Execution

**Concept:** "Signing is like a compile step. The LLM running the directives IS the execution."

**How it works:**

1. File content parsed and validated against schema
2. Ed25519 signature computed over normalized content
3. Signature appended as metadata comment: `<!-- rye:signed:... -->`
4. On load: signature verified, tampered items rejected

**Effect:** Directives and tools carry cryptographic proof of origin and integrity, enabling registry trust model.

---

## 4. Tool Execution Engine: PrimitiveExecutor

### Three-Layer Routing

**File:** [primitive_executor.py#80-88](file:///home/leo/projects/rye-os/rye/rye/executor/primitive_executor.py#L80-L88)

Tools declare execution via `__executor_id__`:

```python
class Tool:
    __executor_id__ = None           # Layer 1: I am a primitive
    # OR
    __executor_id__ = "subprocess"  # Layer 2: delegate to subprocess runtime
    # OR
    __executor_id__ = "python_runtime"  # Layer 3: delegate to python runtime
```

**Routing algorithm:**

```
Tool A (python_runtime)
  ↓
Runtime B (subprocess runtime)
  ↓
Primitive C (SubprocessPrimitive)
  ↓ execute async command
```

**Chain Validation:**

- I/O types match between layers
- Space compatibility (project tools can't call system-only runtimes)
- Recursive loop detection (max depth: 10)

**Advantage:** Tools are composable without hardcoding dependencies. A tool just declares what executor it needs; RYE resolves the chain at runtime.

**Status:** Implemented. Chain building, validation, caching all present.

### Lilux Primitives

#### SubprocessPrimitive

**File:** [subprocess.py#36-100](file:///home/leo/projects/rye-os/lilux/lilux/primitives/subprocess.py#L36-L100)

Two-stage templating:

1. Environment variable expansion: `${VAR:-default}`
2. Runtime parameter substitution: `{param_name}`

Example config:

```json
{
  "command": "python",
  "args": ["{script_path}"],
  "cwd": "${WORKSPACE}",
  "env": { "PYTHONPATH": "/usr/lib/python3" },
  "timeout": 300
}
```

Fully async, timeout support, no complex shell features (pipes, redirects).

#### HttpClientPrimitive

Referenced in code but implementation not shown. Likely handles HTTP requests with similar templating.

---

## 5. Multi-Agent Threading: Real Process Spawning

### Async Execution with os.fork()

**File:** [thread_directive.py#399-450](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/thread_directive.py#L399-L450)

```python
if params.get("async_exec"):
    child_pid = os.fork()
    if child_pid == 0:
        # Child process
        os.setsid()  # Detach from parent TTY
        # Redirect stdout/stderr to /dev/null
        devnull = os.open(os.devnull, os.O_RDWR)
        os.dup2(devnull, 0)
        os.dup2(devnull, 1)
        os.dup2(devnull, 2)

        # Run LLM loop in child
        result = asyncio.run(runner.run(
            thread_id, user_prompt, harness, provider,
            dispatcher, emitter, transcript, proj_path,
        ))
        # Finalize: report spend, update registry
        os._exit(0)
    else:
        # Parent returns immediately with thread_id
        return {"success": True, "thread_id": thread_id, "pid": child_pid}
```

**What this enables:**

- Parent spawns child, returns immediately
- Child runs full LLM loop asynchronously
- Each child is a separate OS process (own memory, interpreter, state)
- True parallelism (not async/await, not threading — actual processes)

**Limitations:**

- **Unix/Linux only** — `os.fork()` doesn't work on Windows
- **No resource limits** — processes consume memory/CPU freely
- **No container isolation** — processes share filesystem, network, OS

### Parent Context Injection

**File:** [runner.py#291-295](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/runner.py#L291-L295)

When a child spawns grandchildren, parent context is auto-injected:

```python
if resolved_id == "rye/agent/threads/thread_directive":
    dispatch_params.setdefault("parent_thread_id", thread_id)
    dispatch_params.setdefault("parent_depth", orchestrator.get_depth(thread_id))
    dispatch_params.setdefault("parent_limits", harness.limits)
    dispatch_params.setdefault("parent_capabilities", harness._capabilities)
```

This enables:

- **Depth tracking** — prevent infinite spawn loops
- **Capability attenuation** — children inherit parent's capabilities (or less)
- **Budget cascade** — spend tracked up the tree

### Permission Enforcement in the Harness

**File:** [runner.py#262-285](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/runner.py#L262-L285)

Permission check happens **before every tool call**:

```python
# Permission check: extract the inner action from tool input
inner_primary = tc_name.replace("rye_", "", 1)
inner_item_type = tc_input.get("item_type", "tool")
inner_item_id = tc_input.get("item_id", "")

denied = harness.check_permission(inner_primary, inner_item_type, inner_item_id)
if denied:
    emitter.emit("tool_call_result", {"error": denied["error"]})
    messages.append({"role": "tool", "tool_call_id": id, "content": str(denied)})
    continue  # Skip execution
```

**How it works:**

Directive declares capabilities:

```xml
<permissions>
  <execute><tool>rye.file-system.*</tool></execute>
  <execute><tool>rye.web.websearch</tool></execute>
  <search><type>tool</type></search>
</permissions>
```

These become capability strings: `rye.execute.tool.rye.file-system.*`

At tool dispatch, RYE checks if the requested tool matches:

```python
def check_permission(self, primary: str, item_type: str, item_id: str) -> Optional[Dict]:
    if not self._capabilities:
        return {"error": "Permission denied: no capabilities declared"}

    required = f"rye.{primary}.{item_type}.{item_id_dotted}"

    for cap in self._capabilities:
        if fnmatch.fnmatch(required, cap):
            return None  # Allowed

    return {"error": "Permission denied: ..."}
```

**Why this design:** "Permissions are fully enforced in the harness, by intention. Permission enforcement happens through the harness, not at the MCP level."

**Isolation model:**

- **Advisory, not OS-level** — enforced in Python, not seccomp/containers
- **Fail-closed** — no capabilities declared = all actions denied
- **Capability attenuation** — children inherit parent's capabilities, can't escalate

---

## 6. LLM Loop: The Agent Orchestrator

### Core Loop

**File:** [runner.py#38-334](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/runner.py#L38-L334)

```
while True:
  1. Check limits (pre-turn)
  2. Call LLM with current messages
  3. Parse response for tool calls
  4. For each tool call:
     a. Check permission
     b. Dispatch to tool via ToolDispatcher
     c. Append result to messages
  5. Run hooks (after_step)
  6. Check cancellation
  7. Loop or finalize
```

### Message Construction

**First message:** Assembled from hooks + user prompt

```python
hook_context = await harness.run_hooks_context(
    {"directive": ..., "model": ..., "limits": ...},
    dispatcher,
)

first_message_parts = []
if hook_context:
    first_message_parts.append(hook_context)
first_message_parts.append(user_prompt)
messages.append({"role": "user", "content": "\n\n".join(first_message_parts)})
```

Hook context can load knowledge items (identity, rules, context) that get prepended to the prompt.

### Tool Dispatch Mapping

**File:** [tool_dispatcher.py#25-132](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/adapters/tool_dispatcher.py#L25-L132)

LLM calls tools by name: `rye_search`, `rye_execute`, `rye_load`, `rye_sign`

ToolDispatcher maps to item_ids and calls RYE's core tools:

```python
async def dispatch(self, action: Dict) -> Dict:
    primary = action.get("primary", "execute")
    tool = self._tools.get(primary)
    item_type = action.get("item_type", "tool")
    item_id = action.get("item_id", "")

    if primary == Action.EXECUTE:
        return await tool.handle(
            item_type=item_type,
            item_id=item_id,
            project_path=...,
            parameters=...,
        )
```

---

## 7. Registry: Real Backend, Incomplete Client

### Server Implementation

**File:** [registry-api/main.py](file:///home/leo/projects/rye-os/services/registry-api/registry_api/main.py)

Real FastAPI service with Supabase backend:

#### Endpoints

| Endpoint                          | Purpose                                  |
| --------------------------------- | ---------------------------------------- |
| `POST /v1/push`                   | Validate & publish item to registry      |
| `GET /v1/pull/{item_id}`          | Download item from registry              |
| `GET /v1/search`                  | Search registry items                    |
| `POST /v1/bundle/push`            | Upload versioned bundle                  |
| `GET /v1/bundle/pull/{bundle_id}` | Download bundle                          |
| `GET /v1/public-key`              | Get registry's Ed25519 public key (TOFU) |

#### Validation & Signing

**File:** [main.py#208-224](file:///home/leo/projects/rye-os/services/registry-api/registry_api/main.py#L208-L224)

```python
# Strip existing signature and validate
content_clean = strip_signature(content, item_type)
is_valid, validation_result = validate_content(content_clean, item_type, name)

if not is_valid:
    raise HTTPException(status_code=400, detail={"issues": validation_result["issues"]})

# Sign with registry provenance
signed_content, signature_info = sign_with_registry(content_clean, item_type, user.username)

# Upsert to database
await _upsert_item(...)
```

**Validation uses RYE's schema validators** — same ones used locally.

**Signing adds registry provenance** — Ed25519 signature with `|registry@username` suffix.

### Client Tool

**File:** [rye/core/registry/registry.py](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/core/registry/registry.py) — **2100+ lines**

Tool provides actions:

| Action                  | Purpose                         |
| ----------------------- | ------------------------------- |
| `signup`                | Create account (email/password) |
| `login` / `login_poll`  | OAuth PKCE flow                 |
| `logout`                | Clear local auth session        |
| `whoami`                | Show authenticated user         |
| `search`                | Query registry                  |
| `pull`                  | Download item to `.ai/{type}/`  |
| `push`                  | Upload local item               |
| `publish` / `unpublish` | Control visibility              |
| `delete`                | Remove item                     |

**Implementation:** Full OAuth flow with PKCE, device auth, local auth storage.

### Gap: Integration Not Documented

The registry **exists and works**, but:

- No examples of agents actually pulling from registry
- Client/server integration tested in unit tests, not integrated into agent loop
- Search tool doesn't query the registry (only local filesystem)

---

## 8. Data-Driven Composition: The Key Pattern

### What "Data-Driven" Means

Tools can declare their own loaders, validators, hooks, permissions. These are **loaded from the filesystem**, not hardcoded:

```
.ai/tools/
├── rye/
│   └── agent/
│       └── threads/
│           ├── runner.py (the LLM loop)
│           ├── loaders/ (pluggable data loaders)
│           │   ├── condition_evaluator.py
│           │   ├── hooks_loader.py
│           │   ├── resilience_loader.py
│           │   └── ...
│           ├── adapters/ (provider/tool integrations)
│           │   ├── tool_dispatcher.py
│           │   ├── http_provider.py
│           │   └── ...
│           └── persistence/ (storage backends)
│               ├── transcript.py
│               ├── thread_registry.py
│               └── budgets.py
```

Each loader is a **data tool** — it loads configuration and rules from YAML/JSON/Python files in `.ai/`.

### Example: Hooks Loader

**Pattern:** Define hooks in directive YAML, loader instantiates them at runtime.

```yaml
# .ai/directives/scrape_website.md
<hooks>
<hook event="thread_started" name="load_identity">
<tool>rye.primary.load</tool>
<params>
<item_type>knowledge</item_type>
<item_id>agent_identity</item_id>
</params>
</hook>

<hook event="error" condition="error.code == 'rate_limit'">
<action>retry</action>
<delay>5</delay>
</hook>
</hooks>
```

Loader parses this and instantiates hook handlers at runtime. If you add a new hook type, it's automatically available (no code changes needed).

---

## 9. What's Complete, What's Incomplete

### ✅ Complete & Solid

| Component                | Status | Evidence                                                          |
| ------------------------ | ------ | ----------------------------------------------------------------- |
| **MCP server (4 tools)** | ✅     | Fully functional, all tests pass                                  |
| **Search system**        | ✅     | Keyword search, fuzzy match, field weighting, metadata extraction |
| **File-based signing**   | ✅     | Ed25519 signatures, TOFU pinning, integrity verification          |
| **Tool execution chain** | ✅     | 3-layer routing, validation, caching                              |
| **LLM loop**             | ✅     | Full event loop, limit checking, permission enforcement, hooks    |
| **Registry server**      | ✅     | FastAPI, Supabase, validation, signing                            |
| **Process spawning**     | ✅     | os.fork(), daemonization, environment inheritance                 |
| **Permission system**    | ✅     | Capability-based, enforced at dispatch time                       |

### ⚠️ Partial

| Component                       | Status | Notes                                                                       |
| ------------------------------- | ------ | --------------------------------------------------------------------------- |
| **Cost tracking**               | ⚠️     | Ledger framework exists; actual cost calculation delegated to provider      |
| **Error recovery**              | ⚠️     | Hooks system present; error classification incomplete                       |
| **Hook system**                 | ⚠️     | thread_started, error, limit, after_step implemented; extensibility partial |
| **Registry client integration** | ⚠️     | Tool exists; not integrated into agent search/load flow                     |

### ❌ Missing/Not Implemented

| Component                   | Status | Notes                                                      |
| --------------------------- | ------ | ---------------------------------------------------------- |
| **Windows support**         | ❌     | Uses `os.fork()`                                           |
| **Resource limits**         | ❌     | No CPU/memory/disk quotas (except cost-based)              |
| **Deterministic workflows** | ❌     | Intentional: directives are free-form for LLM optimization |
| **Container/VM isolation**  | ❌     | Permissions are advisory (Python level)                    |

---

## 10. Compared to Existing Frameworks

### LangGraph

**LangGraph specializes in:**

- Deterministic state graphs
- Exactly reproducible execution paths
- Strong typing for state transitions

**RYE specializes in:**

- Portable, versioned, signed workflows
- Multi-process concurrency with forking
- Data-driven tool composition
- Registry integration

**Verdict:** Orthogonal designs. LangGraph for deterministic orchestration; RYE for portable, registry-driven, LLM-native workflows.

### CrewAI

**CrewAI emphasizes:**

- Role-based agent design (Manager, Researcher, etc.)
- Inter-agent communication
- Specialized tool access per agent

**RYE emphasizes:**

- Workflow instructions (directives)
- Hierarchical thread spawning
- Capabilities-based permission model

**Verdict:** CrewAI is more opinionated about agent behavior; RYE is more flexible about what agents can do.

### AutoGen

**AutoGen focuses on:**

- Human-in-the-loop workflows
- Agent conversation patterns

**RYE focuses on:**

- Signed, versioned, shareable workflows
- Cryptographic provenance
- Registry ecosystem

**Verdict:** Different layers — AutoGen is conversation-level; RYE is artifact-level.

### Raw MCP

**Raw MCP provides:**

- Transport layer (stdio, HTTP, SSE)
- Tool schema definitions

**RYE provides:**

- Search/load/execute/sign abstraction over MCP
- Spaces system (project/user/system)
- Signing & registry
- Multi-process orchestration

**Verdict:** RYE is built on top of MCP; adds significant layers on top.

---

## 11. Production Readiness Checklist

### Deploy Local Development

- ✅ Install from source
- ✅ Create `.ai/directives`, `.ai/tools`, `.ai/knowledge`
- ✅ Sign items
- ✅ Execute directives via MCP client
- ✅ Spawn child threads
- ✅ Enforce permissions

**Ready for:** Single-user, single-machine development.

### Deploy Multi-User Shared Registry

- ⚠️ Registry server exists (needs Supabase)
- ⚠️ Auth flow works (OAuth PKCE)
- ❌ Agent integration not documented (no example of agent pulling from registry)
- ❌ Trust model untested in production (TOFU pinning works, but ecosystem trust not proven)

**Needed:**

- Example: agent searches registry, pulls directive, executes locally
- Documentation: trust model, threat model, revocation
- Testing: supply-chain attack scenarios

### Deploy Production Agent Farm

- ❌ Resource limits missing (no CPU/memory/disk quotas)
- ⚠️ Cost control framework exists but untested at scale
- ❌ Windows support missing
- ❌ No disaster recovery / backup strategy documented

**Needed:**

- Resource limit enforcement (maybe via containers)
- Cost tracking validation against LLM provider APIs
- Windows compatibility (replace `os.fork()`)
- Graceful shutdown, recovery procedures

---

## 12. Architectural Insights

### Signing as Compile Step

"Signing is like a compile step. The LLM running the directives IS the execution."

This reframes what "execution" means:

- **Compile step** = signature verification (integrity & trust)
- **Execution** = LLM reading the instruction and deciding actions

Consequences:

- Directives don't need to be deterministically parseable (they're free-form)
- Trust is cryptographic (signed directives can be safely shared)
- Agents can optimize their own prompting (no hardcoded execution semantics)

### Permission Model: Capabilities, Not Roles

Traditional RBAC: "Alice is an Admin"

RYE's model: "This directive can search tools AND execute file-system tools, but NOT spawn threads"

Benefits:

- Fine-grained, per-directive control
- Attenuation as you spawn children
- No centralized role database

Drawbacks:

- Harder to revoke permissions (need to redeploy directives)
- Not OS-level sandboxing

### Data-Driven vs. Hardcoded

Most agent frameworks hardcode behavior:

- If hook type = "retry", retry using hardcoded logic
- If item type = "tool", execute using hardcoded executor

RYE loads behavior from data:

- Hook handlers are loaded from `.ai/tools/`
- Executor chains are resolved from tool metadata
- This means: new hook types don't require code changes

Trade-off: Flexibility vs. predictability. RYE chooses flexibility.

---

## 13. Final Assessment

### Novelty Score: 7/10

**Novel aspects:**

1. ✅ Portable item system (directives + tools + knowledge as first-class)
2. ✅ Registry-aware with cryptographic provenance
3. ✅ MCP-native (is an MCP server, not a client library)
4. ✅ Data-driven composition pattern
5. ✅ Multi-process thread spawning with parent context injection

**Repackaged aspects:**

- Directive system ≈ LangGraph state graphs + prompting
- Tool chaining ≈ function composition (standard in many frameworks)
- Permission system ≈ capability-based security (Unix-era concept)

### Completeness Score: 7/10

**What works:**

- MCP tools (search, load, execute, sign)
- LLM loop with permissions & limits
- Process spawning with budget tracking
- Registry backend
- Signing & integrity

**What doesn't:**

- Windows support
- Resource limits (OS-level)
- Registry client integration (tool exists, not used)
- Documentation of trust model

### Production Readiness: 6/10

**Safe for:**

- Local development (single user, single machine)
- Small agent teams (shared `.ai/` directory)
- Proof-of-concept multi-agent workflows

**Risky for:**

- Production agent farms (cost control untested, resource limits missing)
- Multi-OS deployment (Windows unsupported)
- Large-scale registry (trust model untested)

### Recommendation

**Best current use:** Local development environment where teams build and version directives/tools/knowledge, leverage signing for trust, and experiment with multi-threaded orchestration.

**For production:** Needs:

1. Windows support (async/multiprocessing instead of `os.fork()`)
2. Resource limit enforcement (containers or OS-level quotas)
3. Cost tracking validation against provider APIs
4. Documented trust & threat model
5. Registry integration examples

---

## References

### Key Files

- **MCP Server:** [rye-mcp/rye_mcp/server.py](file:///home/leo/projects/rye-os/rye-mcp/rye_mcp/server.py)
- **Core Tools:** [rye/rye/tools/](file:///home/leo/projects/rye-os/rye/rye/tools/)
- **Executor:** [rye/rye/executor/primitive_executor.py](file:///home/leo/projects/rye-os/rye/rye/executor/primitive_executor.py)
- **LLM Loop:** [rye/.ai/tools/rye/agent/threads/runner.py](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/runner.py)
- **Permission Enforcement:** [rye/.ai/tools/rye/agent/threads/safety_harness.py](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/agent/threads/safety_harness.py)
- **Registry Client:** [rye/.ai/tools/rye/core/registry/registry.py](file:///home/leo/projects/rye-os/rye/rye/.ai/tools/rye/core/registry/registry.py)
- **Registry Server:** [services/registry-api/registry_api/main.py](file:///home/leo/projects/rye-os/services/registry-api/registry_api/main.py)
