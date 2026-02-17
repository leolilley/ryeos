> **EXPERIMENTAL**: Under active development. Features may be unstable and subject to change.

# RYE OS

> _"In Linux, everything is a file. In RYE, everything is data."_

RYE (RYE Your Execution)is an MCP server that gives AI agents a portable `.ai/` directory system for managing **directives** (workflow instructions), **tools** (executable scripts), and **knowledge** (domain information) — with cryptographic signing, capability-based permissions, and multi-process orchestration.

The agent interacts through 4 MCP tools. The framework provides the scaffolding. The LLM is the execution engine.

## Why RYE

Agent frameworks today hardcode their runtimes, trap workflows inside individual projects, and treat tool execution as a black box. RYE inverts this:

- **Everything is data.** Runtimes, error classification, retry policies, provider configs, hook conditions — all loaded from swappable YAML/Python files, not hardcoded. Adding a new language runtime is a YAML file, not a code change.
- **The runtime runs on itself.** RYE's own agent system (LLM loop, safety harness, orchestrator) lives inside `.ai/tools/` as signed items — subject to the same integrity checks, space precedence, and override mechanics as user-authored tools.
- **Workflows are portable, signed artifacts.** Directives carry Ed25519 signatures. A signed workflow runs anywhere, pulls dependencies from a shared registry, and rejects tampered items at execution time.

## How It Works

```
.ai/
├── directives/     # Workflow instructions (XML metadata + free-form process steps)
├── tools/          # Executable scripts (Python, JS, Bash, YAML runtimes)
└── knowledge/      # Domain information with YAML frontmatter
```

Items resolve through three spaces: **project** (`.ai/`) → **user** (`~/.ai/`) → **system** (bundled). Four MCP tools are the entire agent-facing surface:

| Tool      | Purpose                                       |
| --------- | --------------------------------------------- |
| `search`  | Find items across all spaces and the registry |
| `load`    | Read item content or copy between spaces      |
| `execute` | Run a directive, tool, or knowledge item      |
| `sign`    | Cryptographically sign items with Ed25519     |

### Orchestration

Directives can spawn child threads as separate OS processes (`os.fork`), each with its own LLM loop, budget, and capabilities. Capabilities attenuate down the tree — children can only do what parents allow. Budget cascades upward. Depth is tracked and limited. All configurable via YAML.

### Integrity

Every tool in the execution chain is Ed25519-verified before running. Lockfiles pin versions with hash verification. Bundle manifests cover non-signable assets. The trust store supports TOFU pinning for registry items.

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
- **[Registry](docs/registry/sharing-items.md)** — Sharing items, trust model, agent integration
- **[Internals](docs/internals/architecture.md)** — Architecture, executor chain, spaces, signing

## License

MIT
