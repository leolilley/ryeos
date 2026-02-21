<!-- rye:signed:2026-02-21T05:56:40Z:28491686f11dd779965576ac8ceccebeb28bada81efd682e4e98d7cc89db5200:vKQAEfxullcbheiH2Jc2_aDFInmXzEqYKu4MkY6ZYaekYZiGSWi1qp14ysAM_HIBnGRldIyOOZ0oCD6sWDcWCQ==:9fbfabe975fa5a7f -->

```yaml
id: file-operations
title: File System Operations
entry_type: reference
category: rye/file-system
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - file-system
  - read
  - write
  - edit
  - glob
  - grep
references:
  - "docs/standard-library/tools/file-system.md"
```

# File System Operations

Six tools for file operations, all sandboxed to the project workspace. Paths outside the project root are rejected.

## Namespace & Runtime

| Field       | Value                                      |
| ----------- | ------------------------------------------ |
| Namespace   | `rye/file-system/`                         |
| Runtime     | `python_function_runtime`                  |
| Executor ID | `rye/core/runtimes/python_function_runtime` |

## Path Resolution (All Tools)

1. Relative paths → resolved against `project_path`
2. Absolute paths → used directly
3. All resolved paths must satisfy `is_relative_to(project)` or error

---

## Line ID (LID) System

All read output includes persistent line IDs:

```
[LID:a3f2c1] def execute(params, project_path):
[LID:b7e4d9]     project = Path(project_path).resolve()
```

- **Content-based**: SHA-256 hash of `{line_num}:{content}`, truncated to 6 hex chars
- **Cached**: `.ai/cache/tools/read/line_index/<path_hash>.json`
- **Reconciled**: unchanged lines keep old IDs when file changes
- **Invalidated**: `write` and `edit_lines` delete the cache after modification

---

## `read`

**Item ID:** `rye/file-system/read`

### Parameters

| Name        | Type    | Required | Default | Description                      |
| ----------- | ------- | -------- | ------- | -------------------------------- |
| `file_path` | string  | ✅       | —       | Path to file                     |
| `offset`    | integer | ❌       | `1`     | Starting line number (1-indexed) |
| `limit`     | integer | ❌       | `2000`  | Max lines to read                |

### Limits

| Limit           | Value                    |
| --------------- | ------------------------ |
| Max line length  | 2,000 characters         |
| Max total output | 51,200 bytes (50 KB)     |

### Return

```json
{
  "success": true,
  "output": "[LID:a3f2c1] line content...\n...",
  "line_count": 50,
  "total_lines": 200,
  "truncated": false,
  "offset": 1
}
```

### Invocation

```python
rye_execute(item_type="tool", item_id="rye/file-system/read",
    parameters={"file_path": "src/main.py"})

rye_execute(item_type="tool", item_id="rye/file-system/read",
    parameters={"file_path": "src/main.py", "offset": 100, "limit": 50})
```

---

## `write`

**Item ID:** `rye/file-system/write`

Creates or overwrites a file. Parent directories created automatically. **Invalidates LID cache.**

### Parameters

| Name        | Type   | Required | Description    |
| ----------- | ------ | -------- | -------------- |
| `file_path` | string | ✅       | Path to file   |
| `content`   | string | ✅       | Content to write |

### Return

```json
{
  "success": true,
  "output": "--- a/path\n+++ b/path\n@@ ...",
  "file_path": "relative/path",
  "bytes_written": 1234,
  "created": false
}
```

- `output` is a unified diff (or creation message for new files)
- `created` is `true` when file didn't exist before

---

## `edit_lines`

**Item ID:** `rye/file-system/edit_lines`

Edit files by line ID. **Requires a prior `read`** to populate the LID cache.

### Parameters

| Name        | Type   | Required | Description               |
| ----------- | ------ | -------- | ------------------------- |
| `file_path` | string | ✅       | Path to file              |
| `changes`   | array  | ✅       | List of change operations |

### Change Modes

**Single line:**
```json
{"line_id": "a3f2c1", "new_content": "replacement text"}
```

**Range replacement (inclusive):**
```json
{
  "start_line_id": "a3f2c1",
  "end_line_id": "b7e4d9",
  "new_content": "replaces all lines in range"
}
```

### Safety Rules

| Rule                 | Behavior                                                  |
| -------------------- | --------------------------------------------------------- |
| No cache             | Error: `"Read the file first to generate line IDs"`       |
| Stale cache          | Error: `"File has changed since last read. Re-read..."`   |
| Invalid line ID      | Error with list of available IDs                          |
| Application order    | Changes applied bottom-up to preserve line numbers        |
| Post-edit            | LID cache invalidated — must re-read before next edit     |

### Return

```json
{
  "success": true,
  "output": "--- a/path\n+++ b/path\n@@ ...",
  "changes_applied": 2,
  "lines_changed": 5
}
```

---

## `glob`

**Item ID:** `rye/file-system/glob`

Find files by glob pattern.

### Parameters

| Name      | Type   | Required | Default      | Description                    |
| --------- | ------ | -------- | ------------ | ------------------------------ |
| `pattern` | string | ✅       | —            | Glob pattern (e.g., `**/*.py`) |
| `path`    | string | ❌       | project root | Base search directory          |

### Limits & Behavior

- Max results: **100** matches
- Results sorted alphabetically
- Uses `Path.rglob()` internally
- Files only (directories excluded)

### Auto-Ignored Directories

`node_modules`, `__pycache__`, `.git`, `.venv`, `venv`, `.tox`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `dist`, `build`, `*.egg-info`, `.eggs`, `.nox`, `.hg`, `.svn`

### Return

```json
{
  "success": true,
  "output": "src/main.py\nsrc/utils.py",
  "files": ["src/main.py", "src/utils.py"],
  "count": 2,
  "truncated": false
}
```

---

## `grep`

**Item ID:** `rye/file-system/grep`

Regex search across files. Uses **ripgrep** (`rg`) when available, Python fallback otherwise.

### Parameters

| Name      | Type   | Required | Default      | Description                     |
| --------- | ------ | -------- | ------------ | ------------------------------- |
| `pattern` | string | ✅       | —            | Regex pattern                   |
| `path`    | string | ❌       | project root | Directory to search             |
| `include` | string | ❌       | —            | File glob filter (e.g., `*.py`) |

### Limits

| Limit       | Value            |
| ----------- | ---------------- |
| Max results | 100 matches      |
| Timeout     | 30 seconds (rg)  |

### Return

```json
{
  "success": true,
  "output": "file.py:42:[LID:a3f2c1]:content",
  "matches": [
    {"file": "file.py", "line": 42, "content": "...", "line_id": "a3f2c1"}
  ],
  "count": 1,
  "truncated": false
}
```

- `line_id` is included only when the file has a cached LID index
- Ignores same directories as `glob`

---

## `ls`

**Item ID:** `rye/file-system/ls`

List directory contents (non-recursive).

### Parameters

| Name   | Type   | Required | Default      | Description       |
| ------ | ------ | -------- | ------------ | ----------------- |
| `path` | string | ❌       | project root | Directory to list |

### Behavior

- Sorted: directories first (alphabetical), then files (alphabetical)
- Excludes noise directories (same set as `glob`)

### Return

```json
{
  "success": true,
  "output": "src/\ntests/\nREADME.md",
  "entries": [
    {"name": "src", "type": "directory"},
    {"name": "README.md", "type": "file"}
  ]
}
```

---

## Common Error Conditions

| Error                          | Returned by                  |
| ------------------------------ | ---------------------------- |
| Path outside project workspace | all tools                    |
| File/directory not found       | read, write, edit_lines, ls  |
| Path is a directory (not file) | read, edit_lines             |
| Invalid regex pattern          | grep                         |
| No line ID cache               | edit_lines                   |
| Stale cache (file changed)     | edit_lines                   |
