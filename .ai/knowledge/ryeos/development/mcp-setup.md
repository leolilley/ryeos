---
category: "ryeos/development"
name: "mcp-setup"
description: "Dual MCP setup for coexistence of old Python system and new Rust system in opencode and amp"
---

# MCP Setup: Dual System Coexistence

## The situation

Two Rye OS systems coexist:

| | Old (Python) | New (Rust) |
|---|---|---|
| **Project** | `/home/leo/projects/ryeos/` | `/home/leo/projects/ryeos-cas-as-truth/` |
| **MCP server** | `ryeos-mcp` (3 tools: execute, fetch, sign) | `ryeosd-mcp` (1 tool: cli) |
| **Binary** | Python package, in-process | `ryeos` CLI binary, subprocess |
| **Daemon** | None (MCP IS the server) | `ryeosd` running separately |
| **MCP name in config** | `rye` | `ryeos` |
| **Tools** | `mcp__rye__execute`, `mcp__rye__fetch`, `mcp__rye__sign` | `mcp__ryeos__cli` |

## opencode configuration

Both MCP servers run simultaneously. The agent knows which project uses which server.

```json
{
  "mcp": {
    "rye": {
      "type": "local",
      "command": ["/home/leo/projects/ryeos/.venv/bin/ryeos-mcp"],
      "environment": {
        "USER_SPACE": "/home/leo/rye-stable"
      },
      "enabled": true
    },
    "ryeos": {
      "type": "local",
      "command": ["/path/to/ryeosd-mcp-venv/bin/ryeosd-mcp"],
      "environment": {
        "RYE_BIN": "/home/leo/projects/ryeos-cas-as-truth/target/release/ryeos"
      },
      "enabled": true
    }
  }
}
```

## Setup steps for new MCP server

### 1. Build the ryeos CLI binary

```bash
cd /home/leo/projects/ryeos-cas-as-truth
cargo build --release -p ryeos-cli
```

### 2. Create venv for ryeosd-mcp

```bash
cd /home/leo/projects/ryeos-cas-as-truth/ryeosd-mcp
uv venv .venv
source .venv/bin/activate
uv pip install -e ".[dev]"
```

### 3. Verify

```bash
RYE_BIN=../target/release/ryeos .venv/bin/ryeosd-mcp
# In another terminal, test via MCP client
```

### 4. Update opencode config

Add the `ryeos` entry alongside the existing `rye` entry.

## Agent prompt design

The agent prompt needs to be aware of both systems:

- When working in `/home/leo/projects/ryeos/` → use `mcp__rye__*` tools
- When working in `/home/leo/projects/ryeos-cas-as-truth/` → use `mcp__ryeos__cli` tool
- The project path determines which system to use

For the new system, the single `cli` tool accepts:
```json
{
  "args": ["execute", "tool:ryeos/core/identity/public_key"],
  "project_path": "/home/leo/projects/ryeos-cas-as-truth"
}
```

## Current ryeosd-mcp status

The `ryeosd-mcp` Python server wraps the `ryeos` CLI binary via subprocess. It exposes one tool (`cli`) that passes args through to the binary.

**Known issues to address:**
1. Binary name defaults to `rye` (old name) — use `RYE_BIN` env var to override to `ryeos`
2. Tests reference `cargo build -p rye-cli --bin rye` — needs updating to `ryeos-cli`/`ryeos`
3. No venv exists yet in `ryeosd-mcp/`

## amp configuration

amp uses a similar MCP config format. The same dual-server pattern applies:
- Old `rye` MCP for old projects
- New `ryeos` MCP for this project

Check amp's config format for specifics.
