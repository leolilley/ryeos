---
id: installation
title: Installation
description: Install and configure Rye OS as an MCP server for your AI agent
category: getting-started
tags: [install, setup, mcp, configuration]
version: "1.0.0"
---

# Installation

Rye OS is distributed as three pip packages. Install to give your AI agent access to the `.ai/` directory system.

## Install the packages

```bash
pip install rye-mcp
```

This pulls in the full dependency chain:

- **`rye-mcp`** — the MCP server that exposes Rye OS to any MCP-compatible AI agent.
- **`rye-os`** — the orchestration layer with the resolver, executor, signing, and metadata. Registers the `rye-os` bundle (all `rye/*` items).
- **`lilux`** — the microkernel with stateless primitives (subprocess, HTTP, signing, integrity hashing).

> **Without MCP:** Install just `rye-os` to call the executor directly from Python — useful for scripting, CI, or wrapping in a CLI.
>
> **Minimal install:** Install `rye-core` instead of `rye-os` for only the core runtimes, primitives, and extractors (`rye/core/*` items) without the full standard library. Note: `rye-core` and `rye-os` are mutually exclusive — install one or the other.
>
> See [Packages and Bundles](../internals/packages-and-bundles.md) for the full breakdown.

## Configure your MCP client

Add `rye-mcp` as an MCP server in your AI client's configuration. The server runs over stdio.

### OpenCode

Add to your OpenCode MCP configuration (`.opencode/opencode.json` or via the UI):

```json
{
  "mcp": {
    "rye": {
      "command": "rye-mcp",
      "env": {
        "USER_SPACE": "/home/you/my-ai-workspace"
      }
    }
  }
}
```

### Amp

Add to your Amp MCP configuration:

```json
{
  "amp.mcpServers": {
    "rye": {
      "command": "rye-mcp"
    }
  }
}
```

### Claude Desktop

Add to `~/.config/claude/claude_desktop_config.json` (Linux) or `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS):

```json
{
  "mcpServers": {
    "rye": {
      "command": "rye-mcp",
      "env": {
        "USER_SPACE": "/home/you"
      }
    }
  }
}
```

> **Tip:** If you installed into a virtual environment, use the full path to the `rye-mcp` binary (e.g., `/home/you/.venv/bin/rye-mcp`) or activate the venv before launching your MCP client.

## Exposed MCP tools

Once connected, the server registers four tools that your AI agent can call:

| MCP Tool      | Purpose                                                        |
| ------------- | -------------------------------------------------------------- |
| `rye_search`  | Find directives, tools, or knowledge by scope and query        |
| `rye_load`    | Load item content for inspection, or copy items between spaces |
| `rye_execute` | Run a directive, tool, or knowledge item                       |
| `rye_sign`    | Validate and cryptographically sign an item file               |

These are the only four tools — every interaction with the `.ai/` directory goes through them.

## Environment variables

| Variable     | Default              | Description                                                                                                   |
| ------------ | -------------------- | ------------------------------------------------------------------------------------------------------------- |
| `USER_SPACE` | `~` (home directory) | Base path for the user-level `.ai/` directory. The system looks for `$USER_SPACE/.ai/` for user-scoped items. |
| `RYE_DEBUG`  | `false`              | Set to `true` to enable debug-level logging from the Rye OS server and core library.                          |

### Example: custom user space

```bash
export USER_SPACE="/home/you/my-ai-config"
# Rye OS will look for items in /home/you/my-ai-config/.ai/
```

### Example: enable debug logging

```bash
export RYE_DEBUG=true
rye-mcp
```

## Verify the installation

After configuring your MCP client, verify that Rye OS is running by having your agent call `rye_search`:

```
rye_search(scope="directive", query="create", project_path="/path/to/your/project")
```

If the installation is correct, this will return results from the system space — the built-in directives that ship with `rye-os`, such as `rye/core/create_directive`, `rye/core/create_tool`, and `rye/core/create_knowledge`.

You can also search for tools:

```
rye_search(scope="tool", query="bash", project_path="/path/to/your/project")
```

This should find the built-in `rye/bash/bash` tool, confirming that the system bundles are discoverable.

## What's next

- [Quickstart](quickstart.md) — Create your first directive, tool, and knowledge entry in under 5 minutes.
- [The .ai/ Directory](ai-directory.md) — Understand the directory structure, item IDs, and the 3-tier space system.
