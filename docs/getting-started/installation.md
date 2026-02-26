```yaml
id: installation
title: Installation
description: Install and configure Rye OS as an MCP server for your AI agent
category: getting-started
tags: [install, setup, mcp, configuration]
version: "1.0.0"
```

# Installation

Rye OS is distributed as pip packages. Install to give your AI agent access to the `.ai/` directory system.

## Install the packages

```bash
pip install ryeos-mcp
```

This pulls in the full dependency chain:

- **`ryeos-mcp`** — the MCP server that exposes Rye OS to any MCP-compatible AI agent.
- **`ryeos`** — the standard bundle (~3MB) with agent, bash, file-system, MCP, primary, core, authoring, and guide items. Depends on `ryeos-core`.
- **`ryeos-core`** — the core runtimes, primitives, and extractors (`rye/core/*` items). Depends on `ryeos-engine`.
- **`ryeos-engine`** — the orchestration layer with the resolver, executor, signing, and metadata.
- **`lillux`** — the microkernel with stateless primitives (subprocess, HTTP, signing, integrity hashing). Depends on `lillux-proc` (Rust binary for process lifecycle management).

### Optional extras

```bash
# Add web tools (browser automation, fetch, search)
pip install ryeos[web]    # or: pip install ryeos-web

# Add code tools (git, npm, typescript, LSP, diagnostics)
pip install ryeos[code]   # or: pip install ryeos-code

# Everything
pip install ryeos[all]
```

> **Without MCP:** Install just `ryeos` to call the executor directly from Python — useful for scripting, CI, or wrapping in a CLI.
>
> **Minimal install:** Install `ryeos-core` for the core runtimes, primitives, and extractors (`rye/core/*` items) without the full standard library. `ryeos` depends on `ryeos-core`, so you get core items either way.
>
> **Engine only:** Install `ryeos-engine` for the engine with no `.ai/` data bundles at all. `ryeos-core` depends on `ryeos-engine`.
>
> See [Packages and Bundles](../internals/packages-and-bundles.md) for the full breakdown.

## Configure your MCP client

Add `ryeos-mcp` as an MCP server in your AI client's configuration. The server runs over stdio.

### OpenCode

Add to your OpenCode MCP configuration (`.opencode/opencode.json` or via the UI):

```json
{
  "mcp": {
    "rye": {
      "command": "ryeos-mcp",
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
      "command": "ryeos-mcp"
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
      "command": "ryeos-mcp",
      "env": {
        "USER_SPACE": "/home/you"
      }
    }
  }
}
```

> **Tip:** If you installed into a virtual environment, use the full path to the `ryeos-mcp` binary (e.g., `/home/you/.venv/bin/ryeos-mcp`) or activate the venv before launching your MCP client.

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
ryeos-mcp
```

## Verify the installation

After configuring your MCP client, verify that Rye OS is running by having your agent call `rye_search`:

```
rye_search(scope="directive", query="create", project_path="/path/to/your/project")
```

If the installation is correct, this will return results from the system space — the built-in directives that ship with `ryeos`, such as `rye/core/create_directive`, `rye/core/create_tool`, and `rye/core/create_knowledge`.

You can also search for tools:

```
rye_search(scope="tool", query="bash", project_path="/path/to/your/project")
```

This should find the built-in `rye/bash/bash` tool, confirming that the system bundles are discoverable.

## What's next

- [Quickstart](quickstart.md) — Create your first directive, tool, and knowledge entry in under 5 minutes.
- [The .ai/ Directory](ai-directory.md) — Understand the directory structure, item IDs, and the 3-tier space system.
