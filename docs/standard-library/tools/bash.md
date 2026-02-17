```yaml
id: tools-bash
title: "Bash Tool"
description: Execute shell commands with timeout and output truncation
category: standard-library/tools
tags: [tools, bash, shell, subprocess]
version: "1.0.0"
```

# Bash Tool

**Namespace:** `rye/bash/`
**Runtime:** `python_script_runtime`

Execute shell commands via `subprocess.run()` with `shell=True`. Commands run in the project root by default and are sandboxed to the project workspace.

---

## `bash`

**Item ID:** `rye/bash/bash`

### Parameters

| Name          | Type    | Required | Default      | Description                                |
| ------------- | ------- | -------- | ------------ | ------------------------------------------ |
| `command`     | string  | ✅       | —            | Shell command to execute                   |
| `timeout`     | integer | ❌       | `120`        | Timeout in seconds                         |
| `working_dir` | string  | ❌       | project root | Working directory (must be within project) |

### Limits

- **Max output:** 51,200 bytes (50 KB) per stream (stdout and stderr independently)
- **Default timeout:** 120 seconds
- **Working directory** must be within the project workspace

### Output

```json
{
  "success": true,
  "output": "combined stdout + stderr",
  "stdout": "stdout only",
  "stderr": "stderr only",
  "exit_code": 0,
  "truncated": false
}
```

`success` is `true` when `exit_code == 0`. Stderr is included in `output` prefixed with `[stderr]`.

On timeout:

```json
{
  "success": false,
  "error": "Command timed out after 120 seconds",
  "timeout": 120
}
```

### Examples

```python
# Simple command
rye_execute(item_type="tool", item_id="rye/bash/bash",
    parameters={"command": "git status --short"})

# With timeout and working directory
rye_execute(item_type="tool", item_id="rye/bash/bash",
    parameters={
        "command": "npm test",
        "timeout": 300,
        "working_dir": "frontend/"
    })
```
