<!-- rye:signed:2026-02-23T05:24:41Z:ffda66d6378ed78ef5c12b25c3dde150b828c47f157b5e91e853c5be7bbe2cb5:d7tgzK8h50BEGbagCYdnGTDIln1iX8HfmXyj7wanM6SNufcD229_TKQwQyvxp_UJJ_lWzk3TWOguUZWfWmMdCw==:9fbfabe975fa5a7f -->

```yaml
name: bash-execution
title: Bash Command Execution
entry_type: reference
category: rye/bash
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - bash
  - shell
  - subprocess
  - execution
references:
  - "docs/standard-library/tools/bash.md"
```

# Bash Command Execution

Execute shell commands via `subprocess.run()` with `shell=True`, sandboxed to the project workspace.

## Tool Identity

| Field         | Value                                  |
| ------------- | -------------------------------------- |
| Item ID       | `rye/bash/bash`                        |
| Namespace     | `rye/bash/`                            |
| Runtime       | `python/script`                |
| Executor ID   | `rye/core/runtimes/python/script` |

## Parameters

| Name          | Type    | Required | Default      | Description                                |
| ------------- | ------- | -------- | ------------ | ------------------------------------------ |
| `command`     | string  | ✅       | —            | Shell command to execute                   |
| `timeout`     | integer | ❌       | `120`        | Timeout in seconds                         |
| `working_dir` | string  | ❌       | project root | Working directory (must be within project) |

## Invocation

```python
rye_execute(item_type="tool", item_id="rye/bash/bash",
    parameters={"command": "git status --short"})

rye_execute(item_type="tool", item_id="rye/bash/bash",
    parameters={
        "command": "npm test",
        "timeout": 300,
        "working_dir": "frontend/"
    })
```

## Working Directory Resolution

1. If `working_dir` is provided and relative → resolved against `project_path`
2. If `working_dir` is absolute → used directly
3. Resolved path must be within project workspace (`is_relative_to(project)`)
4. If path is outside project → error: `"Working directory is outside the project workspace"`
5. If path does not exist → error with path shown
6. If omitted → defaults to project root

## Timeout Behavior

- Default: **120 seconds** (`DEFAULT_TIMEOUT`)
- On timeout: process is terminated via `subprocess.TimeoutExpired`
- Returns timeout-specific error response (see below)

## Output Limits

| Limit              | Value          |
| ------------------ | -------------- |
| Max output per stream | 51,200 bytes (50 KB) |
| Streams truncated independently | stdout and stderr each capped |

When truncated, appends: `\n... [output truncated, {total_bytes} bytes total]`

## Return Format

### Success (`exit_code == 0`)

```json
{
  "success": true,
  "output": "combined stdout + [stderr] prefix",
  "stdout": "stdout only",
  "stderr": "stderr only",
  "exit_code": 0,
  "truncated": false
}
```

- `output` combines stdout and stderr; stderr lines are prefixed with `[stderr]\n`
- `truncated` is `true` if either stream exceeded 50 KB

### Non-Zero Exit

Same structure as success but `"success": false` and `exit_code` reflects the actual code.

### Timeout

```json
{
  "success": false,
  "error": "Command timed out after {timeout} seconds",
  "timeout": 120
}
```

### General Error

```json
{
  "success": false,
  "error": "error message string"
}
```

## Security Constraints

- `shell=True` — commands run in a shell subprocess
- Working directory is sandboxed: must resolve within `project_path`
- No explicit environment variable filtering — inherits the process environment
- `capture_output=True` — stdout and stderr are captured, not passed through

## Implementation Notes

- Uses `subprocess.run()` with `text=True` for string output
- Truncation is byte-based (`utf-8` encoded), decoded back with `errors="replace"`
- CLI entry point accepts `--params` (JSON) and `--project-path` arguments
