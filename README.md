> **EXPERIMENTAL**: Under active development. Features may be unstable and subject to change.

# RYE OS

> _"In Linux, everything is a file. In RYE, everything is data."_

RYE (Rye Your Execution) is a portable operating system for AI agents. It gives any LLM a `.ai/` directory with **directives** (workflows), **tools** (executables), and **knowledge** (domain data) — all cryptographically signed, capability-controlled, and orchestratable across processes.

Four MCP tools. The framework provides the scaffolding. The LLM is the execution engine.

## The Problem

Agent frameworks today hardcode their runtimes, trap workflows inside individual projects, and treat tool execution as a black box. Want to add a new language runtime? Rewrite framework code. Want to share a workflow? Copy-paste and pray. Want to know if a tool was tampered with? You can't.

MCP itself provides binary access — granting callers full tool utility — without support for semantic permission attenuation, deep delegation chains, or reasoning traces. It's stateless regarding internal reasoning, exposing only results rather than intent. RYE is the policy and orchestration layer that MCP is missing.

## The Architecture

RYE inverts the relationship between code and data. The system is built on three principles:

### Everything Is Data

Runtimes, error classification, retry policies, provider configs, hook conditions — all loaded from swappable YAML/Python files, not hardcoded.

Adding a new language runtime is a YAML file:

```yaml
tool_type: runtime
executor_id: rye/core/primitives/subprocess
env_config:
  interpreter:
    type: venv_python
    var: RYE_PYTHON
    fallback: python3
config:
  command: "${RYE_PYTHON}"
  args: ["{tool_path}", "--params", "{params_json}"]
```

No code changes to RYE. No recompilation. No pull request. Just a file.

### The Runtime Runs on Itself

RYE's own agent system — the LLM loop, safety harness, orchestrator, thread system — lives inside `.ai/tools/` as signed items. They're subject to the same integrity checks, space precedence, and override mechanics as user-authored tools.

Want to modify how the orchestrator waits for threads? Override the file in your project space. The system resolves `project → user → system` — your version wins.

### Three-Layer Executor Chain

Every tool call follows a deterministic chain from agent request to OS operation:

```
Tool (.py)  →  Runtime (.yaml)  →  Primitive (Lilux)
   ↑               ↑                    ↑
 your code    how to run it      OS-level execution
```

Each element is independently signed and verified. The chain is cached, lockfile-pinned, and validated for space compatibility before anything executes.

## What You Get

### Cryptographic Integrity

Every item is Ed25519-signed. Every chain element is verified before execution. Lockfiles pin exact versions with SHA256 hashes. Bundle manifests cover multi-file dependencies. TOFU pinning for registry items.

**Unsigned or tampered items are rejected. No fallback. No bypass.**

```
# rye:signed:2026-02-14T00:27:54Z:8e27c5f8...:WOclUqjr...:440443d0
```

### Multi-Agent Orchestration

Spawn child threads as separate OS processes via `os.fork()`. Each child gets its own LLM loop, model selection, budget, and transcript:

```
Root Orchestrator (sonnet, $3.00 budget)
  ├── discover_leads × 5  (haiku, $0.10 each, parallel)
  ├── qualify_leads        (sonnet, $1.00)
  │   ├── scrape_website × N  (haiku, $0.05 each)
  │   └── score_lead × N      (haiku, $0.05 each)
  └── prepare_outreach     (haiku, $0.20)
```

- **Budget cascades** — children can never spend more than the parent allocated. A SQLite-backed ledger tracks reservations atomically across concurrent forks.
- **Capabilities attenuate** — each level can only have equal or fewer permissions. A leaf that scores leads can execute exactly one tool. Nothing else.
- **Adaptive coordination** — cancel threads, kill unresponsive processes (SIGTERM → SIGKILL), resume failed threads with new instructions, cascade cancellation policies to children. All configurable via YAML.
- **Automatic continuation** — when a thread hits 90% of its context window, it hands off to a new thread with a summary, preserving the full execution chain.

### Declarative State Graphs

Define deterministic workflows as YAML — no LLM calls for routing, no code:

```yaml
tool_type: graph
executor_id: rye/core/runtimes/state_graph_runtime

config:
  start: count_files
  max_steps: 10
  nodes:
    count_files:
      action:
        primary: execute
        item_type: tool
        item_id: rye/bash/bash
        params:
          command: "find . -name '*.py' | wc -l"
      assign:
        file_count: "${result.stdout}"
      next: done
    done:
      type: return
```

State graphs persist after each step as signed knowledge items — they're resumable, auditable, and can spawn LLM threads for steps that need reasoning. Foreach nodes fan out work in parallel. Error edges route to recovery nodes. Hooks fire on graph events.

### Fail-Closed Security with Capability Attenuation

No capabilities declared? All actions denied. Capabilities use fnmatch patterns with full attenuation down the thread hierarchy:

```
rye.execute.tool.rye.bash.bash       — execute exactly one tool
rye.execute.tool.rye.file-system.*   — execute any file-system tool
rye.load.knowledge.my-project.*      — load project knowledge only
```

A scoring leaf can call one scoring tool. An orchestrator can spawn threads and load knowledge. Nothing gets implicit access. Capabilities are Ed25519-signed tokens with audience binding and expiry — children can only subset their parent's permissions, never escalate.

### Data-Driven Error Handling

Error classification, retry policies, and resilience behavior are YAML configs, not hardcoded logic:

```yaml
# error_classification.yaml
patterns:
  - id: "http_429"
    category: "rate_limited"
    retryable: true
    match:
      any:
        - path: "error.message"
          op: "regex"
          value: "rate limit|too many requests"
    retry_policy:
      type: "exponential"
      base: 2.0
      max: 60.0
```

The hook system classifies errors by pattern, determines retryability, and calculates backoff — all swappable without touching framework code. Hooks fire on errors, limit violations, context exhaustion, and step completion.

### White-Box Observability

Every thread is fully transparent. Parents can read child transcripts — full reasoning traces, tool calls, and results. The orchestrator provides `get_status`, `read_transcript`, `get_chain`, and `chain_search` (regex across an entire delegation tree). Per-token streaming writes to both JSONL transcripts and knowledge markdown in real-time:

```bash
tail -f .ai/threads/<thread_id>/transcript.jsonl
```

### Three-Tier Space System

Items resolve through three spaces with shadow-override semantics:

| Space       | Path                     | Purpose                            |
| ----------- | ------------------------ | ---------------------------------- |
| **Project** | `.ai/`                   | Your project's tools and workflows |
| **User**    | `{USER_PATH}/.ai/`       | Cross-project personal items       |
| **System**  | `site-packages/rye/.ai/` | Immutable standard library         |

Project shadows user shadows system. Override any system behavior by placing a file with the same ID in your project.

### Shared Registry

Push signed items to a shared registry. Pull them with TOFU key pinning. Items carry registry provenance (`|rye-registry@username`) so you know who published what.

```python
rye_execute(item_type="tool", item_id="rye/core/registry/registry",
    parameters={"action": "push", "item_type": "tool", "item_id": "my-tool"})
```

## MCP Interface

Four tools are the entire agent-facing surface:

| Tool      | Purpose                                   |
| --------- | ----------------------------------------- |
| `search`  | Find items across all spaces              |
| `load`    | Read item content or copy between spaces  |
| `execute` | Run a directive, tool, or knowledge item  |
| `sign`    | Cryptographically sign items with Ed25519 |

## Install

```bash
pip install rye-mcp
```

Installs the full stack: `rye-mcp` (MCP transport) → `rye-os` (executor + standard library) → `lilux` (microkernel primitives).

```json
{
  "mcpServers": {
    "rye": {
      "command": "rye-mcp"
    }
  }
}
```

> **From source:**
>
> ```bash
> git clone https://github.com/leolilley/rye-os.git
> cd rye-os
> pip install -e lilux -e rye -e rye-mcp
> ```

## Packages

| Package    | What it provides                                              |
| ---------- | ------------------------------------------------------------- |
| `lilux`    | Microkernel — subprocess, HTTP, signing, integrity primitives |
| `rye-os`   | Executor, resolver, signing, metadata + full standard library |
| `rye-core` | Same engine, minimal bundle (only `rye/core/*` items)         |
| `rye-mcp`  | MCP server transport (stdio/SSE)                              |

## Documentation

Full documentation at [`docs/`](docs/index.md):

- **[Getting Started](docs/getting-started/installation.md)** — Installation, quickstart, `.ai/` directory structure
- **[Authoring](docs/authoring/directives.md)** — Writing directives, tools, and knowledge
- **[MCP Tools Reference](docs/tools-reference/execute.md)** — The four agent-facing tools
- **[Orchestration](docs/orchestration/overview.md)** — Thread-based multi-agent workflows
- **[State Graphs](docs/orchestration/state-graphs.md)** — Declarative YAML workflow graphs
- **[Registry](docs/registry/sharing-items.md)** — Sharing items, trust model, agent integration
- **[Internals](docs/internals/architecture.md)** — Architecture, executor chain, spaces, signing

## License

MIT
