# RYE OS

> _"In Linux, everything is a file. In RYE, everything is data."_

**RYE is a portable operating system for AI agents.** It gives any LLM — through any MCP client — a `.ai/` directory with signed workflows, scoped permissions, and a community registry. Four tools. Any model. The agent is the interpreter.

There is no other system that combines cryptographic trust, capability-scoped multi-agent orchestration, and portable declarative workflows in a single, model-agnostic package.

```bash
pip install ryeos-mcp
```

```json
{
  "mcpServers": {
    "rye": { "command": "ryeos-mcp" }
  }
}
```

Works in Claude Desktop, Cursor, Windsurf, Amp, or any MCP client. Same workflows, same trust, same permissions — regardless of which LLM runs them.

## Why RYE

Every agent framework solves pieces of the puzzle in isolation. Codex, Claude Code, LangChain — each a walled garden with no way to share solutions between them.

MCP gave us a universal tool protocol. But MCP is a pipe — it doesn't answer who can do what, how far trust extends, or whether what you're running has been tampered with.

**RYE is the policy and orchestration layer that MCP is missing.**

|                         | Codex / Claude Code | LangChain / CrewAI | **RYE**                                  |
| ----------------------- | ------------------- | ------------------ | ---------------------------------------- |
| **Portable workflows**  | ✗ Platform-locked   | ✗ Code, not data   | ✓ Declarative data files                 |
| **Model agnostic**      | ✗ Single vendor     | ✓                  | ✓ Any LLM via MCP                        |
| **Cryptographic trust** | ✗                   | ✗                  | ✓ Ed25519 signed, chain-verified         |
| **Permission model**    | Human-in-the-loop   | None               | ✓ Declarative capability attenuation     |
| **Cross-client**        | ✗ One client only   | ✗ One framework    | ✓ Any MCP client                         |
| **Community registry**  | ✗                   | ✗ Unsigned         | ✓ Signed, TOFU-pinned, author-attributed |

## Features

### Cryptographic Trust — No Exceptions

Every item is Ed25519-signed. Every chain element is verified before execution. Lockfiles pin exact versions with SHA256 hashes. **Unsigned or tampered items are rejected — including RYE's own system tools.**

```
# rye:signed:2026-02-14T00:27:54Z:8e27c5f8...:WOclUqjr...:440443d0
```

### Multi-Agent Orchestration

Spawn autonomous child threads as separate OS processes. Each gets its own LLM, budget, and transcript:

```
Root Orchestrator (sonnet, $3.00 budget)
  ├── discover_leads × 5  (haiku, $0.10 each, parallel)
  ├── qualify_leads        (sonnet, $1.00)
  │   ├── scrape_website × N  (haiku, $0.05 each)
  │   └── score_lead × N      (haiku, $0.05 each)
  └── prepare_outreach     (haiku, $0.20)
```

- **Budget cascades** — children can never exceed parent allocation
- **Capabilities attenuate** — each level can only narrow permissions, never escalate
- **Adaptive coordination** — cancel, kill, resume, cascade policies. All via YAML
- **Lossless context chains** — threads hand off with summaries; full history is searchable

### Fail-Closed Capability System

No capabilities declared = all actions denied. Permissions use fnmatch patterns with full attenuation down delegation chains:

```
rye.execute.tool.rye.bash.bash       — one specific tool
rye.execute.tool.rye.file-system.*   — any file-system tool
rye.load.knowledge.my-project.*      — project knowledge only
```

Capabilities are Ed25519-signed tokens with audience binding and expiry. Children can only subset parent permissions.

### Declarative State Graphs

Deterministic workflows as YAML — no LLM calls for routing:

```yaml
config:
  start: count_files
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

Resumable, auditable, parallelizable. Foreach nodes fan out work. Error edges route to recovery. Hooks fire on graph events.

### White-Box Observability

Every thread is fully transparent. Parents read child transcripts — full reasoning traces, tool calls, results. Regex search across entire delegation trees. Per-token streaming to JSONL and knowledge markdown in real-time.

### Three-Tier Space System

Items resolve `project → user → system` with shadow-override semantics:

| Space       | Path                     | Purpose                    |
| ----------- | ------------------------ | -------------------------- |
| **Project** | `.ai/`                   | Your project's items       |
| **User**    | `~/.ai/`                 | Personal cross-project     |
| **System**  | `site-packages/rye/.ai/` | Immutable standard library |

Override any system behavior by placing a file with the same ID in your project. RYE's own orchestrator, safety harness, and agent system are all overridable `.ai/` items.

### Community Registry

Push signed items. Pull with TOFU key pinning. Every item carries author provenance — trust is cryptographically provable, not implicit.

```bash
rye execute directive rye/core/registry/push    # publish a tool
rye execute directive rye/core/registry/pull    # install a tool
rye execute directive rye/core/registry/search  # find tools
```

A package manager for agent cognition.

### Everything Is Data

Runtimes, error classification, retry policies, provider configs — all YAML/Python files, not hardcoded. Adding a new language runtime is a single YAML file:

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

No code changes. No recompilation. Just a file.

## MCP Interface

Four tools are the entire agent-facing surface:

| Tool      | Purpose                             |
| --------- | ----------------------------------- |
| `search`  | Find items across all spaces        |
| `load`    | Read content or copy between spaces |
| `execute` | Run directives, tools, or knowledge |
| `sign`    | Cryptographically sign items        |

## Install

```bash
pip install ryeos-mcp          # full stack with MCP transport
```

### Optional bundles

```bash
pip install ryeos[web]         # + browser automation, fetch, search
pip install ryeos[code]        # + git, npm, typescript, LSP, diagnostics
pip install ryeos[all]         # everything
```

### Minimal installs

```bash
pip install ryeos              # standard bundle (no MCP transport)
pip install ryeos-core         # engine + core only (runtimes, primitives)
pip install ryeos-engine       # engine only, no .ai/ data (for embedding)
```

### From source

```bash
git clone https://github.com/leolilley/ryeos.git
cd ryeos
pip install -e lillux/kernel -e ryeos -e ryeos-mcp
```

## Packages

```
lillux/
  kernel/        → pip: lillux          Microkernel (subprocess, signing, HTTP)
  proc/          → pip: lillux-proc     Process lifecycle manager (Rust)
  watch/         → pip: lillux-watch    Push-based file watcher (Rust)

ryeos/           → pip: ryeos-engine   Execution engine, no .ai/ data
  bundles/
    core/        → pip: ryeos-core     Minimal: rye/core only
    standard/    → pip: ryeos          Standard .ai/ data bundle
    web/         → pip: ryeos-web      Browser, fetch, search tools
    code/        → pip: ryeos-code     Git, npm, typescript, LSP tools
ryeos-mcp/       → pip: ryeos-mcp      MCP server transport (stdio/SSE)
```

## Platform Support

Linux and macOS. Multi-agent orchestration uses `lillux-proc` for cross-platform process management. Windows support is untested.

## Documentation

Full docs at [`docs/`](docs/index.md):

- **[Getting Started](docs/getting-started/installation.md)** — Install, quickstart, `.ai/` directory
- **[Authoring](docs/authoring/directives.md)** — Writing directives, tools, and knowledge
- **[MCP Tools Reference](docs/tools-reference/execute.md)** — The four agent-facing tools
- **[Orchestration](docs/orchestration/overview.md)** — Multi-agent workflows and threading
- **[State Graphs](docs/orchestration/state-graphs.md)** — Declarative YAML workflow graphs
- **[Registry](docs/registry/sharing-items.md)** — Sharing, trust model, agent integration
- **[Internals](docs/internals/architecture.md)** — Architecture, executor chain, spaces, signing
- **[Future Work](docs/future/index.md)** — Encrypted shared intelligence, continuous streams, CLI, HTTP server

## License

MIT
