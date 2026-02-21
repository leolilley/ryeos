> **EXPERIMENTAL**: Under active development. Features may be unstable and subject to change.

# RYE OS

> _"In Linux, everything is a file. In RYE, everything is data."_

RYE is a portable operating system for AI agents. It gives any LLM a `.ai/` directory with **directives** (workflows), **tools** (executables), and **knowledge** (domain data) — all cryptographically signed, capability-scoped, and shareable through a community registry.

Four MCP tools. Any model. Any client. The agent is the interpreter. The workflows are the commons.

## Contents

- [The Problem](#the-problem)
- [Why Not Just Use Codex / Claude Code / LangChain?](#why-not-just-use-codex--claude-code--langchain)
- [The Architecture](#the-architecture)
- [What You Get](#what-you-get)
  - [Cryptographic Trust](#cryptographic-trust)
  - [Multi-Agent Orchestration](#multi-agent-orchestration)
  - [Fail-Closed Security with Capability Attenuation](#fail-closed-security-with-capability-attenuation)
  - [Declarative State Graphs](#declarative-state-graphs)
  - [White-Box Observability](#white-box-observability)
  - [Three-Tier Space System](#three-tier-space-system)
  - [Community Registry](#community-registry)
- [MCP Interface](#mcp-interface)
- [Install](#install)
- [Packages](#packages)
- [Documentation](#documentation)

## The Problem

The industry is converging on a hard truth: multi-agent orchestration, delegation, and trust are unsolved problems.

Research on [intelligent AI delegation](https://arxiv.org/abs/2503.18175) maps the failure modes — diffusion of responsibility across delegation chains, privilege escalation through unchecked sub-agents, opacity that makes it impossible to distinguish incompetence from malice, and the complete absence of cryptographic verification for agent-to-agent trust. These aren't edge cases. They're structural gaps in every major agent system shipping today.

Every framework has tried to solve pieces of this independently. Codex built a polished harness — tightly coupled to their runtime, not portable. Claude Code optimized for one model — not interoperable. LangChain, CrewAI, AutoGen all converged on multi-agent support — but workflows live in code, not in a shareable format. Each one is a walled garden solving the same problems in isolation, with no way to share the solutions between them.

None of them have a community registry. None have cryptographic trust. None have declarative permission attenuation. None have portable workflows.

MCP gave us a universal tool protocol — adopted by every major harness and now under the Linux Foundation. But MCP is a pipe: it connects agents to tools without answering who can do what, how far trust extends, whether what you're running has been tampered with, or how to scope permissions down a delegation chain.

RYE is the policy and orchestration layer that MCP is missing. Portable agent workflows, cryptographically signed and capability-scoped, executable by any LLM through any MCP client. The harness becomes optional. The workflows become the commons.

## Why Not Just Use [Codex / Claude Code / LangChain]?

Those are harnesses — runtime environments optimized for a specific model or framework. RYE is the layer underneath.

|                         | Codex | Claude Code | LangChain | RYE |
| ----------------------- | ----- | ----------- | --------- | --- |
| **Portable workflows**  | No | No | No | Yes — directives are data files |
| **Model agnostic**      | Limited — OpenAI + OSS via Ollama | Limited — Claude only | Yes | Yes |
| **Community registry**  | No | No | Partial — Hub for prompts, unsigned | Yes — push, pull, signed, TOFU-pinned |
| **Cryptographic trust** | No | No | No | Yes — Ed25519 signed, chain-verified |
| **Permission model**    | OS sandbox + interactive approval | Configurable approval policies | None | Declarative capability attenuation per delegation level |
| **Cross-client**        | Codex only | Claude only | LangChain only | Any MCP client |

Codex and Claude Code have sophisticated permission models — OS-level sandboxing, configurable approval policies — but they're designed for human-in-the-loop sessions. RYE's capability attenuation solves a different problem: scoping permissions across autonomous multi-agent delegation chains where no human is in the loop to approve.

A directive written in RYE works in Claude Desktop, Cursor, Windsurf, Amp, or any MCP-compatible client. The same workflow, the same trust guarantees, the same permission model — regardless of which LLM executes it.

You could run RYE _inside_ Codex or Claude Code. You could also replace them entirely.

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

### Cryptographic Trust

Every item is Ed25519-signed. Every chain element is verified before execution. Lockfiles pin exact versions with SHA256 hashes. Trusted keys are identity-bound TOML documents resolved through the same three-tier system as everything else. The registry uses TOFU key pinning with author provenance.

**Unsigned or tampered items are rejected. No fallback. No bypass. No exceptions — including RYE's own system tools.**

```
# rye:signed:2026-02-14T00:27:54Z:8e27c5f8...:WOclUqjr...:440443d0
```

This directly addresses the delegation trust gap identified in the [research](https://arxiv.org/abs/2503.18175) — every item in a delegation chain is cryptographically attributable to a specific author, and trust is verifiable without requiring a central authority.

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
- **Lossless context chains** — when a thread hits its context window, it hands off to a new thread with a generated summary. The full chain is searchable — the model can retrieve any detail from any previous thread. No compression, no information loss.

### Fail-Closed Security with Capability Attenuation

No capabilities declared? All actions denied. Capabilities use fnmatch patterns with full attenuation down the thread hierarchy:

```
rye.execute.tool.rye.bash.bash       — execute exactly one tool
rye.execute.tool.rye.file-system.*   — execute any file-system tool
rye.load.knowledge.my-project.*      — load project knowledge only
```

A scoring leaf can call one scoring tool. An orchestrator can spawn threads and load knowledge. Nothing gets implicit access. Capabilities are Ed25519-signed tokens with audience binding and expiry — children can only subset their parent's permissions, never escalate.

This is the [privilege attenuation](https://arxiv.org/abs/2503.18175) that MCP and A2A protocols lack — scoped, declarative, cryptographically enforced permissions that attenuate at every delegation boundary.

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

### White-Box Observability

Every thread is fully transparent. Parents can read child transcripts — full reasoning traces, tool calls, and results. The orchestrator provides `get_status`, `read_transcript`, `get_chain`, and `chain_search` (regex across an entire delegation tree). Per-token streaming writes to both JSONL transcripts and knowledge markdown in real-time:

```bash
tail -f .ai/threads/<thread_id>/transcript.jsonl
```

No opaque delegation. No hidden reasoning. Every step in every chain is auditable — addressing the [accountability vacuum](https://arxiv.org/abs/2503.18175) that emerges in multi-agent systems.

### Three-Tier Space System

Items resolve through three spaces with shadow-override semantics:

| Space       | Path                     | Purpose                            |
| ----------- | ------------------------ | ---------------------------------- |
| **Project** | `.ai/`                   | Your project's tools and workflows |
| **User**    | `{USER_PATH}/.ai/`       | Cross-project personal items       |
| **System**  | `site-packages/rye/.ai/` | Immutable standard library         |

Project shadows user shadows system. Override any system behavior by placing a file with the same ID in your project.

### Community Registry

Push signed items to a shared registry. Pull them with TOFU key pinning. Items carry registry provenance (`|rye-registry@username`) so you know who published what. Trust is cryptographically provable and author-attributed — not implicit like npm.

```python
rye_execute(item_type="tool", item_id="rye/core/registry/registry",
    parameters={"action": "push", "item_type": "tool", "item_id": "my-tool"})
```

The registry is essentially a package manager for agent cognition. Search, load, execute, and share workflows the same way you `npm install` a package — except every item is signed and every author is verifiable.

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
pip install ryeos-mcp
```

Installs the full stack: `ryeos-mcp` (MCP transport) → `ryeos` (executor + standard library) → `lilux` (microkernel primitives).

```json
{
  "mcpServers": {
    "rye": {
      "command": "ryeos-mcp"
    }
  }
}
```

> **From source:**
>
> ```bash
> git clone https://github.com/leolilley/rye-os.git
> cd rye-os
> pip install -e lilux -e ryeos -e ryeos-mcp
> ```

## Packages

| Package      | What it provides                                              |
| ------------ | ------------------------------------------------------------- |
| `lilux`      | Microkernel — subprocess, HTTP, signing, integrity primitives |
| `ryeos`      | Executor, resolver, signing, metadata + full standard library |
| `ryeos-core` | Same engine, minimal bundle (only `rye/core/*` items)         |
| `ryeos-bare` | Same engine, no bundle (for services like registry-api)       |
| `ryeos-mcp`  | MCP server transport (stdio/SSE)                              |

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
