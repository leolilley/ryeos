---
category: "ryeos/development"
name: "mcp-setup"
description: "MCP setup for opencode and amp"
---

# MCP Setup

The RyeOS MCP adapter lives at `integrations/mcp/ryeosd`. It exposes a
single MCP tool named `cli` that runs the `ryeos` CLI binary in a
subprocess.

## Architecture

| Component | Value |
|---|---|
| MCP package | `ryeosd-mcp` |
| Python module | `ryeosd_mcp.server` |
| Tool name | `cli` |
| Wrapped binary | `ryeos` |
| Binary override env | `RYE_BIN` |
| Required daemon | `ryeosd` for daemon-backed commands |

The MCP tool accepts:

```json
{
  "args": ["execute", "tool:ryeos/core/identity/public_key"],
  "project_path": "/home/leo/projects/ryeos-next",
  "timeout_s": 60
}
```

Do not include `ryeos` as the first argument; the MCP server prepends
the binary path.

## Setup steps

### 1. Build the ryeos CLI binary

```bash
cargo build --release -p ryeos-cli --bin ryeos
```

### 2. Create a venv for ryeosd-mcp

```bash
cd integrations/mcp/ryeosd
uv venv .venv
source .venv/bin/activate
uv pip install -e ".[dev]"
```

### 3. Run the MCP server

```bash
RYE_BIN=/home/leo/projects/ryeos-next/target/release/ryeos \
  /home/leo/projects/ryeos-next/integrations/mcp/ryeosd/.venv/bin/ryeosd-mcp
```

If `RYE_BIN` is unset, the adapter resolves `ryeos` from `PATH`.

## opencode / amp configuration

Configure an MCP server entry that launches `ryeosd-mcp` and sets
`RYE_BIN` when the release binary is not on `PATH`.

Example command payload:

```json
{
  "command": ["/home/leo/projects/ryeos-next/integrations/mcp/ryeosd/.venv/bin/ryeosd-mcp"],
  "environment": {
    "RYE_BIN": "/home/leo/projects/ryeos-next/target/release/ryeos"
  }
}
```

## Verification

```bash
cd integrations/mcp/ryeosd
uv run pytest tests
```

The test suite builds `ryeos` unless `RYE_BIN` is already set.
