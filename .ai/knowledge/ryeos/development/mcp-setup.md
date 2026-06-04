<!-- ryeos:signed:2026-05-25T06:47:54Z:5398860bad2ef054084a3781888ada3827337b9258da43b2e7039c4b18ab6a41:NWTP1aISZmhCY2bcYrKOPA1yLLtlc0ZdKFg6IaHcLBdw4fAgtyQaMIyhkBhvdyIBaOFH4luiGXisIAGC+AUZCA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: "ryeos/development"
name: "mcp-setup"
title: "MCP Setup"
description: "Short setup and verification guide for the RyeOS MCP adapter"
entry_type: reference
version: "1.1.0"
---

# MCP Setup

The MCP adapter is `integrations/mcp/ryeosd`. It exposes one MCP tool named
`cli`, which shells out to the `ryeos` binary.

## Contract

| Field | Value |
|---|---|
| Package | `ryeosd-mcp` |
| Module | `ryeosd_mcp.server` |
| Tool | `cli` |
| Wrapped binary | `ryeos` |
| Binary override | `RYE_BIN` |
| Daemon needed | yes for daemon-backed commands; no for CLI-offline commands |

Tool input shape:

```json
{
  "args": ["execute", "tool:ryeos/core/identity/public_key"],
  "project_path": "/home/leo/projects/ryeos-next",
  "timeout_s": 60
}
```

Do not include `ryeos` in `args`; the adapter prepends the binary.

## Setup

```bash
cargo build --release -p ryeos-cli --bin ryeos

cd integrations/mcp/ryeosd
uv venv .venv
source .venv/bin/activate
uv pip install -e ".[dev]"
```

Run manually:

```bash
RYE_BIN=/home/leo/projects/ryeos-next/target/release/ryeos \
  /home/leo/projects/ryeos-next/integrations/mcp/ryeosd/.venv/bin/ryeosd-mcp
```

If `RYE_BIN` is unset, the adapter resolves `ryeos` from `PATH`.

## Client config pattern

Configure your MCP client to launch the `ryeosd-mcp` executable and set
`RYE_BIN` when the release binary is not on PATH.

```json
{
  "command": ["/home/leo/projects/ryeos-next/integrations/mcp/ryeosd/.venv/bin/ryeosd-mcp"],
  "environment": {
    "RYE_BIN": "/home/leo/projects/ryeos-next/target/release/ryeos"
  }
}
```

## Verify

```bash
cd integrations/mcp/ryeosd
uv run pytest tests
```

The tests build `ryeos` unless `RYE_BIN` already points at a usable binary.
