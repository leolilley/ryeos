```yaml
id: tools-file-system
title: "File System Tools"
description: "Read, write, edit, search, and list files — with persistent line IDs for stable editing"
category: standard-library/tools
tags: [tools, file-system, read, write, edit, glob, grep, ls]
version: "1.0.0"
```

# File System Tools

**Namespace:** `rye/file-system/`
**Runtime:** `python/function`

Six tools for file operations, all sandboxed to the project workspace. Paths outside the project root are rejected.

---

## Line ID System

The file system tools use a **persistent line ID (LID)** system that provides stable references for editing. When you read a file, each line is tagged with a short hash-based ID:

```
[LID:a3f2c1] def execute(params, project_path):
[LID:b7e4d9]     project = Path(project_path).resolve()
```

These IDs are:

- **Content-based** — generated from a SHA-256 hash of the line number and content
- **Cached** — stored in `.ai/cache/tools/read/line_index/<hash>.json`
- **Reconciled** — when a file changes, unchanged lines keep their old IDs
- **Invalidated** — `write` and `edit_lines` clear the cache, forcing re-generation on next read

This means `edit_lines` uses line IDs instead of string matching, avoiding the ambiguity problems of text-based editing.

---

## `read`

**Item ID:** `rye/file-system/read`

Read file contents with persistent line IDs.

### Parameters

| Name        | Type    | Required | Default | Description                                         |
| ----------- | ------- | -------- | ------- | --------------------------------------------------- |
| `file_path` | string  | ✅       | —       | Path to file (relative to project root or absolute) |
| `offset`    | integer | ❌       | `1`     | Starting line number (1-indexed)                    |
| `limit`     | integer | ❌       | `2000`  | Maximum number of lines to read                     |

### Limits

- **Max line length:** 2,000 characters
- **Max total output:** 51,200 bytes (50 KB) — output is truncated beyond this

### Output

```json
{
  "success": true,
  "output": "[LID:a3f2c1] line content...\n[LID:b7e4d9] ...",
  "line_count": 50,
  "total_lines": 200,
  "truncated": false,
  "offset": 1
}
```

### Example

```python
rye_execute(item_type="tool", item_id="rye/file-system/read",
    parameters={"file_path": "src/main.py"})

# Read lines 100-150
rye_execute(item_type="tool", item_id="rye/file-system/read",
    parameters={"file_path": "src/main.py", "offset": 100, "limit": 50})
```

---

## `write`

**Item ID:** `rye/file-system/write`

Create or overwrite a file. Parent directories are created automatically. Invalidates the line ID cache for the file.

### Parameters

| Name        | Type   | Required | Description                                         |
| ----------- | ------ | -------- | --------------------------------------------------- |
| `file_path` | string | ✅       | Path to file (relative to project root or absolute) |
| `content`   | string | ✅       | Content to write to the file                        |

### Output

Returns a unified diff showing the changes made, plus metadata:

```json
{
  "success": true,
  "output": "--- a/src/main.py\n+++ b/src/main.py\n@@ ...",
  "file_path": "src/main.py",
  "bytes_written": 1234,
  "created": false
}
```

### Example

```python
rye_execute(item_type="tool", item_id="rye/file-system/write",
    parameters={"file_path": "config/settings.json", "content": '{"debug": true}'})
```

---

## `edit_lines`

**Item ID:** `rye/file-system/edit_lines`

Edit files using line IDs instead of string matching. **You must read the file first** to generate line IDs — editing without a cache returns an error.

### Parameters

| Name        | Type   | Required | Description               |
| ----------- | ------ | -------- | ------------------------- |
| `file_path` | string | ✅       | Path to file              |
| `changes`   | array  | ✅       | List of change operations |

Each change object supports two modes:

**Single line replacement:**

```json
{ "line_id": "a3f2c1", "new_content": "replacement text" }
```

**Range replacement:**

```json
{
  "start_line_id": "a3f2c1",
  "end_line_id": "b7e4d9",
  "new_content": "replaces all lines in range"
}
```

### Safety

- **Stale cache detection** — if the file has changed since the last read, the edit is rejected with an error asking you to re-read
- **Invalid ID detection** — if any line ID doesn't exist, the edit fails and shows available IDs
- **Reverse application** — changes are applied bottom-up to preserve line numbers
- **Cache invalidation** — after successful edit, the line ID cache is cleared

### Output

Returns a unified diff of the changes applied:

```json
{
  "success": true,
  "output": "--- a/src/main.py\n+++ b/src/main.py\n@@ ...",
  "changes_applied": 2,
  "lines_changed": 5
}
```

### Example

```python
# Replace a single line
rye_execute(item_type="tool", item_id="rye/file-system/edit_lines",
    parameters={
        "file_path": "src/main.py",
        "changes": [{"line_id": "a3f2c1", "new_content": "    return True"}]
    })

# Replace a range of lines
rye_execute(item_type="tool", item_id="rye/file-system/edit_lines",
    parameters={
        "file_path": "src/main.py",
        "changes": [{
            "start_line_id": "a3f2c1",
            "end_line_id": "d5f8e2",
            "new_content": "    # refactored\n    return process(data)"
        }]
    })
```

---

## `glob`

**Item ID:** `rye/file-system/glob`

Find files matching a glob pattern. Results are sorted alphabetically and capped at 100 matches.

### Parameters

| Name      | Type   | Required | Default      | Description                            |
| --------- | ------ | -------- | ------------ | -------------------------------------- |
| `pattern` | string | ✅       | —            | Glob pattern (e.g., `**/*.py`, `*.md`) |
| `path`    | string | ❌       | project root | Base directory to search from          |

### Ignored Directories

The following are automatically excluded: `node_modules`, `__pycache__`, `.git`, `.venv`, `venv`, `.tox`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `dist`, `build`, `*.egg-info`, `.eggs`, `.nox`, `.hg`, `.svn`.

### Output

```json
{
  "success": true,
  "output": "src/main.py\nsrc/utils.py\n...",
  "files": ["src/main.py", "src/utils.py"],
  "count": 2,
  "truncated": false
}
```

### Example

```python
rye_execute(item_type="tool", item_id="rye/file-system/glob",
    parameters={"pattern": "**/*.py", "path": "src/"})
```

---

## `grep`

**Item ID:** `rye/file-system/grep`

Search file contents with regex. Uses **ripgrep** (`rg`) when available for performance, with a pure-Python fallback. Results include line IDs when the file has been previously read.

### Parameters

| Name      | Type   | Required | Default      | Description                     |
| --------- | ------ | -------- | ------------ | ------------------------------- |
| `pattern` | string | ✅       | —            | Regex pattern to search for     |
| `path`    | string | ❌       | project root | Directory to search in          |
| `include` | string | ❌       | —            | File glob filter (e.g., `*.py`) |

### Limits

- **Max results:** 100 matches
- **Timeout:** 30 seconds (ripgrep mode)

### Output

```json
{
  "success": true,
  "output": "src/main.py:42:[LID:a3f2c1]:    # TODO: fix this",
  "matches": [
    {
      "file": "src/main.py",
      "line": 42,
      "content": "    # TODO: fix this",
      "line_id": "a3f2c1"
    }
  ],
  "count": 1,
  "truncated": false
}
```

### Example

```python
# Search for TODO comments in Python files
rye_execute(item_type="tool", item_id="rye/file-system/grep",
    parameters={"pattern": "TODO:", "path": "src/", "include": "*.py"})
```

---

## `ls`

**Item ID:** `rye/file-system/ls`

List directory contents. Entries are sorted with directories first (alphabetically), then files (alphabetically). Common noise directories are excluded.

### Parameters

| Name   | Type   | Required | Default      | Description       |
| ------ | ------ | -------- | ------------ | ----------------- |
| `path` | string | ❌       | project root | Directory to list |

### Output

```json
{
  "success": true,
  "output": "src/\ntests/\nREADME.md\nsetup.py",
  "entries": [
    { "name": "src", "type": "directory" },
    { "name": "tests", "type": "directory" },
    { "name": "README.md", "type": "file" },
    { "name": "setup.py", "type": "file" }
  ]
}
```

### Example

```python
rye_execute(item_type="tool", item_id="rye/file-system/ls",
    parameters={"path": "src/"})
```
