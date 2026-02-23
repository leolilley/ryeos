```yaml
id: tools-index
title: "Tools Reference"
description: Detailed documentation for every tool in the Rye OS standard library
category: standard-library/tools
tags: [tools, standard-library, reference]
version: "1.0.0"
```

# Tools Reference

This section documents every tool that ships with Rye OS. Tools are organized into two tiers:

- **Agent-facing tools** — tools that users and directives interact with directly
- **Infrastructure tools** — internal components that power the system

## Agent-Facing Tools

| Page                          | Namespace          | Tools | Description                       |
| ----------------------------- | ------------------ | ----- | --------------------------------- |
| [File System](file-system.md) | `rye/file-system/` | 6     | Read, write, edit, glob, grep, ls |
| [Bash](bash.md)               | `rye/bash/`        | 1     | Shell command execution           |
| [Web](web.md)                 | `rye/web/`         | 3     | Web search, fetch, and browser    |
| [Code](code.md)               | `rye/code/`        | 4     | NPM, diagnostics, TypeScript, LSP |
| [MCP Client](mcp.md)          | `rye/mcp/`         | 3     | Connect to external MCP servers   |
| [Primary Tools](primary.md)   | `rye/primary/`     | 4     | Search, load, execute, sign items |
| [Agent System](agent.md)      | `rye/agent/`       | 40+   | Thread orchestration engine       |

## Infrastructure Tools

| Page                                | Namespace   | Description                                                                |
| ----------------------------------- | ----------- | -------------------------------------------------------------------------- |
| [Infrastructure](infrastructure.md) | `rye/core/` | Parsers, runtimes, extractors, sinks, bundler, registry, system, telemetry |

## Invocation Pattern

All tools are invoked via the same interface:

```python
rye_execute(
    item_type="tool",
    item_id="<namespace>/<tool_name>",
    parameters={...}
)
```

Every tool returns a dict with at minimum `success: bool`. On failure, an `error` string is included. Most tools also return an `output` string with human-readable results.
