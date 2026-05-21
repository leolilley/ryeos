# ryeos:signed:2026-05-20T11:41:17Z:fad54ff41aec43f1073f32a5b90e4093e4a1663928891b96c97192c9cc245e58:uimnd29X4ghxEDI8KwexjeJ3lVYsYf8UTcy3Apvbz2GiNEwIriC8QNaSHCKMwqE39LlqfEjR2/owzmh9Erk0Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea

---
category: ryeos/core
tags: [fundamentals, mcp, agent-interface, tools]
version: "2.0.0"
description: >
  How AI agents interact with Rye OS via MCP — the single `cli`
  tool, argument passing, and routing patterns.
---

# MCP Agent Interface

AI agents (Claude Code, Cursor, amp, etc.) interact with Rye OS through
**MCP (Model Context Protocol)**. The MCP server is a thin wrapper
that exposes a single tool, `cli`, which shells out to the `ryeos`
CLI binary.

## Threat Model

The MCP server is intended for **local single-user use**:

- Transport: stdio over a process owned by the operator's OS user
- Caller authentication: assumed (the OS user IS the operator)
- Capability gating: none at the MCP layer — every CLI verb the
  `ryeos` binary exposes is available

Do NOT expose the MCP server over the network without a separate
auth-terminating proxy.

## The `cli` Tool

The server exposes one tool:

```json
{
  "tool": "cli",
  "args": ["execute", "tool:ryeos/core/sign"],
  "project_path": "/path/to/project"
}
```

### Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `args` | string[] | Yes | argv passed to `ryeos`. Do NOT include `ryeos` as the first element. |
| `project_path` | string | No | Sets the subprocess working directory. Defaults to the MCP server's cwd. |
| `timeout_s` | number | No | Seconds before the subprocess is killed. Default 60. Minimum 1. |

### Returns

A JSON object with `exit_code`, `stdout`, `stderr`, and (if stdout is
valid JSON) a parsed `json` field. On validation errors or timeouts, a
typed error is returned with `error` and `type` fields.

### Discovery

```json
{"tool": "cli", "args": ["help"]}
```

Lists all available verbs in the current project. Verbs are loaded
data-driven from `.ai/config/cli/*.yaml` — installing a new bundle
makes new verbs immediately callable with no MCP redeploy.

### Binary Discovery

The server finds the `ryeos` binary via:

1. `RYE_BIN` environment variable
2. `shutil.which("ryeos")` on PATH

## Example Invocations

```json
// Execute a directive
{"tool": "cli", "args": ["execute", "directive:my/workflow"], "project_path": "/path"}

// Fetch an item
{"tool": "cli", "args": ["fetch", "tool:ryeos/core/sign", "--with-content"], "project_path": "/path"}

// Sign items
{"tool": "cli", "args": ["sign", "directive:*"], "project_path": "/path"}

// Push to a remote
{"tool": "cli", "args": ["remote", "push", "--remote", "prod", "--project", "/abs/path"]}

// Thread operations
{"tool": "cli", "args": ["thread", "list"], "project_path": "/path"}
{"tool": "cli", "args": ["thread", "tail", "T-abc123"], "project_path": "/path"}
```

## Workflow Patterns

### Pattern 1: Execute and Follow
```json
{"tool": "cli", "args": ["execute", "directive:deploy"], "project_path": "/path"}
```
The response contains the execution result. For directives, follow the
returned directions.

### Pattern 2: Discover First
```json
{"tool": "cli", "args": ["help"], "project_path": "/path"}
```
List all available verbs, then execute the appropriate one.

### Pattern 3: Read-Only Inspection
```json
{"tool": "cli", "args": ["fetch", "tool:my/helper", "--with-content"], "project_path": "/path"}
```
Inspect an item without executing it.
