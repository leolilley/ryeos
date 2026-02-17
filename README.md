> **EXPERIMENTAL**: Under active development. Features may be unstable and subject to change.

# RYE OS

> _"In Linux, everything is a file. In RYE, everything is data."_

RYE (RYE Your Execution) is an MCP server that gives AI agents a portable `.ai/` directory system. Agents search, load, execute, and sign three item types — **directives** (workflow instructions), **tools** (executable scripts), and **knowledge** (domain information) — across projects, users, and a shared registry.

Built on **Lilux**, a microkernel providing pure execution primitives.

## The Problem

Workflows, tools, and context are trapped inside individual projects. Your agent can't pull the scraper it built yesterday, can't reuse the deployment pipeline from another project, and can't share what it learned. Every new project starts from scratch.

RYE breaks that loop. Agents self-serve from a searchable, cryptographically-signed item system — locally and across a shared registry.

## How It Works

```
.ai/
├── directives/     # XML workflow instructions
├── tools/          # Executable scripts (Python, JS, Bash, YAML)
└── knowledge/      # Domain information with YAML frontmatter
```

Items live in three spaces with precedence: **project** (`.ai/`) → **user** (`~/.ai/`) → **system** (bundled with the package). Your agent interacts through four MCP tools:

| Tool      | Purpose                                       |
| --------- | --------------------------------------------- |
| `search`  | Find items across all spaces and the registry |
| `load`    | Read item content or copy between spaces      |
| `execute` | Run a directive, tool, or knowledge item      |
| `sign`    | Cryptographically sign items with Ed25519     |

Every item is Ed25519 signed. Unsigned or tampered items are rejected at execution time.

## Install

```bash
pip install rye-mcp
```

This installs the full stack: `rye-mcp` (MCP transport) → `rye-os` (executor, resolver, standard library) → `lilux` (microkernel primitives).

Configure your MCP client:

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

| Package    | What it provides                                              | Bundle                         |
| ---------- | ------------------------------------------------------------- | ------------------------------ |
| `lilux`    | Microkernel — subprocess, HTTP, signing, integrity hashing    | —                              |
| `rye-os`   | Executor, resolver, signing, metadata + full standard library | `rye-os` (all `rye/*` items)   |
| `rye-core` | Same code as `rye-os`, minimal bundle                         | `rye-core` (only `rye/core/*`) |
| `rye-mcp`  | MCP server transport (stdio/SSE)                              | —                              |

`rye-os` and `rye-core` are mutually exclusive — install one or the other. `rye-mcp` depends on `rye-os`.

## Documentation

Full documentation at [`docs/`](docs/index.md):

- **[Getting Started](docs/getting-started/installation.md)** — Installation, quickstart, `.ai/` directory structure
- **[Authoring](docs/authoring/directives.md)** — Writing directives, tools, and knowledge
- **[MCP Tools Reference](docs/tools-reference/execute.md)** — The four agent-facing tools
- **[Orchestration](docs/orchestration/overview.md)** — Thread-based multi-agent workflows
- **[Internals](docs/internals/architecture.md)** — Architecture, executor chain, spaces, signing, packages

## License

MIT
