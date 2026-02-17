# Adding Workspace Tools to RYE

Implementation plan for RYE's workspace toolset, featuring line-addressed editing with persistent IDs for superior reliability across all LLMs.

**Key innovation:** Uses persistent line IDs instead of brittle string matching, based on insights from [The Harness Problem](https://blog.can.ac/2026/02/12/the-harness-problem/).

---

## 1. Overview

Traditional agent harnesses use string matching for edits:

- Model must reproduce text _exactly_ including whitespace
- "String to replace not found" errors are extremely common
- Even good models fail due to harness fragility

**RYE's approach:** Line-addressed editing with persistent IDs

- Model references stable line identifiers: `[LID:abc123]`
- No need to reproduce text exactly
- Benchmarks show **+8% to +14%** improvement across all models
- Aligns with RYE philosophy: _workflows are data, not prompts_

---

## 2. Tool Categories

```
.ai/tools/rye/
├── file-system/
│   ├── read.py            # Read files with persistent line IDs
│   ├── write.py           # Create/overwrite files
│   ├── edit_lines.py      # Line-addressed editing
│   ├── glob.py            # File pattern matching
│   ├── grep.py            # Content search with line IDs
│   └── ls.py              # Directory listing
├── bash/
│   └── bash.py            # Shell execution
├── web/
│   ├── webfetch.py        # URL fetching
│   └── websearch.py       # Web search (P2)
├── lsp/
│   └── lsp.py             # LSP diagnostics (P2)
├── primary-tools/         # Existing: rye_execute, rye_search, rye_load, rye_sign
└── registry/              # Existing: registry tool
```

**9 tools total:**

- P0 (7): read, write, edit_lines, glob, grep, ls, bash
- P1 (1): webfetch
- P2 (2): websearch, lsp

---

## 3. Line ID System

### Format

**Output:** `[LID:abc123] line content here`

**Generation:**

- Each line gets a persistent ID stored in cache
- First read: Generate IDs from content hash + line number
- Subsequent reads: Reconcile with cache, preserve IDs for unchanged lines

### Cache Location

```
{PROJECT_ROOT}/.ai/cache/tools/read/line_index/{path_hash}.json
```

Following RYE convention: `{SPACE}/{AI_DIR}/cache/tools/{ITEM_ID}/`

### Cache Format

```json
{
  "file_path": "src/app.py",
  "content_hash": "sha256:abc123...",
  "last_modified": 1708451200,
  "lines": [
    { "id": "a1b2c3", "line_num": 1, "content_hash": "sha256:def456..." },
    { "id": "d4e5f6", "line_num": 2, "content_hash": "sha256:ghi789..." }
  ]
}
```

### Reconciliation Logic

1. Hash current file content
2. If matches cached hash → return cached IDs
3. If changed → match lines by content hash, preserve IDs for unchanged lines
4. Assign new IDs to new/modified lines
5. Persist updated index

### Benefits

- IDs survive whitespace/formatting changes (content hash unchanged)
- IDs survive line insertions/deletions (only affected lines change)
- IDs persist across sessions (cache survives agent restart)
- Multi-step edits work without re-reading files

---

## 4. Tool Specifications

### read

**Location:** `.ai/tools/rye/file-system/read.py`

**Runtime:** `python_function_runtime`

**Purpose:** Read file content with persistent line IDs

**Params:**

- `file_path` (string, required): Path to file
- `offset` (integer, optional): Starting line (1-indexed, default 1)
- `limit` (integer, optional): Max lines to read (default 2000)

**Returns:**

```json
{
  "success": true,
  "output": "[LID:a1b2c] def hello():\n[LID:d3e4f]     pass",
  "line_count": 2,
  "total_lines": 150,
  "truncated": false
}
```

**Cache Management:**

```python
def get_line_index_path(file_path: str, project_path: str) -> Path:
    """Get cache path for line index following RYE conventions."""
    relative_path = Path(file_path).relative_to(project_path)
    path_hash = hashlib.sha256(str(relative_path).encode()).hexdigest()[:16]
    return Path(project_path) / ".ai" / "cache" / "tools" / "read" / "line_index" / f"{path_hash}.json"

def reconcile_line_index(current_lines: list, cached_index: dict) -> tuple:
    """Match current lines to cached IDs by content hash."""
    content_to_line = {
        line["content_hash"]: line
        for line in cached_index.get("lines", [])
    }

    new_index = []
    reused = 0

    for i, line_content in enumerate(current_lines, 1):
        content_hash = hashlib.sha256(line_content.encode()).hexdigest()

        if content_hash in content_to_line:
            line_id = content_to_line[content_hash]["id"]
            reused += 1
        else:
            line_id = generate_line_id()

        new_index.append({
            "id": line_id,
            "line_num": i,
            "content_hash": content_hash
        })

    return new_index, reused, len(current_lines) - reused
```

---

### write

**Location:** `.ai/tools/rye/file-system/write.py`

**Runtime:** `python_function_runtime`

**Purpose:** Create or overwrite file

**Params:**

- `file_path` (string, required): Path to file
- `content` (string, required): File content

**Returns:**

```json
{
  "success": true,
  "output": "Created src/app.py (45 bytes)",
  "file_path": "src/app.py",
  "bytes_written": 45,
  "created": true
}
```

**Cache Management:**

- Clear line index cache for the file on write
- Next read will generate fresh IDs

---

### edit_lines

**Location:** `.ai/tools/rye/file-system/edit_lines.py`

**Runtime:** `python_function_runtime`

**Purpose:** Edit files by line ID (not string matching)

**Params:**

- `file_path` (string, required): Path to file
- `changes` (array, required): List of change operations
  - Single line: `{"line_id": "abc123", "new_content": "    return 10"}`
  - Range: `{"start_line_id": "abc123", "end_line_id": "def456", "new_content": "..."}`

**Returns:**

```json
{
  "success": true,
  "output": "Applied 2 changes\n--- a/src/app.py\n+++ b/src/app.py\n...",
  "changes_applied": 2,
  "lines_changed": 5
}
```

**Validation:**

1. Check all referenced line IDs exist in current file state
2. If any ID missing → fail fast with error listing invalid IDs
3. Apply changes in order
4. Invalidate line index cache for the file

**Example Flow:**

```python
# 1. Agent calls read
[LID:a1b2c] def calculate(x):
[LID:d3e4f]     return x * 2
[LID:g5h6i]

# 2. Agent calls edit_lines
{
  "file_path": "src/app.py",
  "changes": [
    {"line_id": "d3e4f", "new_content": "    return x * 3"}
  ]
}

# 3. Tool validates ID exists, applies change, invalidates cache
```

---

### glob

**Location:** `.ai/tools/rye/file-system/glob.py`

**Runtime:** `python_function_runtime`

**Purpose:** Find files by pattern

**Params:**

- `pattern` (string, required): Glob pattern (e.g., `**/*.py`)
- `path` (string, optional): Search path (default: project root)

**Returns:**

```json
{
  "success": true,
  "output": "src/app.py\nsrc/utils.py\ntests/test_app.py",
  "files": ["src/app.py", "src/utils.py", "tests/test_app.py"],
  "count": 3,
  "truncated": false
}
```

**Features:**

- Use `pathlib.Path.rglob()` for pattern matching
- Respect common ignores: `node_modules`, `__pycache__`, `.git`, `.venv`
- Limit 100 results (truncation flag if exceeded)

---

### grep

**Location:** `.ai/tools/rye/file-system/grep.py`

**Runtime:** `python_function_runtime`

**Purpose:** Search file contents with regex, returning line IDs

**Params:**

- `pattern` (string, required): Regex pattern
- `path` (string, optional): Search path (default: project root)
- `include` (string, optional): File glob filter (e.g., `*.py`)

**Returns:**

```json
{
  "success": true,
  "output": "src/app.py:42:[LID:abc123]:def hello():\nsrc/utils.py:15:[LID:def456]:def world():",
  "matches": [
    {
      "file": "src/app.py",
      "line": 42,
      "line_id": "abc123",
      "content": "def hello():"
    }
  ],
  "count": 2
}
```

**Features:**

- Shell out to `ripgrep` if available, fallback to `grep -rn`
- Look up line IDs from cache for each match
- Limit results (truncation flag if exceeded)

---

### ls

**Location:** `.ai/tools/rye/file-system/ls.py`

**Runtime:** `python_function_runtime`

**Purpose:** List directory contents

**Params:**

- `path` (string, optional): Directory path (default: project root)

**Returns:**

```json
{
  "success": true,
  "output": "src/\ntests/\nREADME.md\npyproject.toml",
  "entries": [
    { "name": "src", "type": "directory" },
    { "name": "tests", "type": "directory" },
    { "name": "README.md", "type": "file" },
    { "name": "pyproject.toml", "type": "file" }
  ]
}
```

**Features:**

- Show directories with trailing `/` in text output
- Ignore common build artifacts: `__pycache__`, `.venv`, `node_modules`

---

### bash

**Location:** `.ai/tools/rye/bash/bash.py`

**Runtime:** `python_function_runtime`

**Purpose:** Execute shell commands

**Params:**

- `command` (string, required): Shell command
- `timeout` (integer, optional): Timeout in seconds (default 120)
- `working_dir` (string, optional): Working directory (default: project root)

**Returns:**

```json
{
  "success": true,
  "output": "stdout content here...",
  "stderr": "",
  "exit_code": 0,
  "truncated": false
}
```

**Features:**

- Uses Python `subprocess` internally
- Smart truncation for large outputs (>50KB)
- Working directory validation (must be within project)
- Timeout enforcement with process termination

---

### webfetch

**Location:** `.ai/tools/rye/web/webfetch.py`

**Runtime:** `python_function_runtime`

**Purpose:** Fetch URL content

**Params:**

- `url` (string, required): URL to fetch
- `format` (string, optional): Output format - `text`, `markdown`, `html` (default: `markdown`)
- `timeout` (integer, optional): Timeout in seconds (default 30)

**Returns:**

```json
{
  "success": true,
  "output": "# Page Title\n\nContent here...",
  "url": "https://example.com",
  "format": "markdown",
  "bytes": 1234
}
```

**Features:**

- Use `httpx` if available, fallback to `urllib`
- HTML→markdown conversion for `format: markdown`
- Timeout enforcement

---

### websearch (P2)

**Location:** `.ai/tools/rye/web/websearch.py`

**Runtime:** `python_function_runtime`

**Purpose:** Web search via configurable provider

**Params:**

- `query` (string, required): Search query
- `num_results` (integer, optional): Number of results (default 10)

**Returns:**

```json
{
  "success": true,
  "output": "1. Result Title\n   https://example.com\n   Snippet...\n\n2. ...",
  "results": [
    { "title": "Result Title", "url": "https://example.com", "snippet": "..." }
  ]
}
```

**Notes:**

- Requires API key configuration (Exa, or other provider)
- Provider configured via project/user/system YAML

---

### lsp (P2)

**Location:** `.ai/tools/rye/lsp/lsp.py`

**Runtime:** `python_function_runtime`

**Purpose:** Get LSP diagnostics for a file

**Params:**

- `file_path` (string, required): Path to file

**Returns:**

```json
{
  "success": true,
  "output": "src/app.py:10:5: error: Undefined variable 'foo'",
  "diagnostics": [
    {
      "line": 10,
      "column": 5,
      "severity": "error",
      "message": "Undefined variable 'foo'"
    }
  ]
}
```

**Notes:**

- Requires running language server
- Complex integration, deferred to P2

---

## 5. Implementation Pattern

All tools follow RYE's data-driven format:

```python
# rye:signed:...
"""Tool description for discovery."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
__category__ = "rye/file-system"  # or rye/bash, rye/web, rye/lsp
__tool_description__ = "Short description for tool registry"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "file_path": {"type": "string", "description": "Path to file"},
        "offset": {"type": "integer", "description": "Starting line (1-indexed)"},
        "limit": {"type": "integer", "description": "Max lines to read"}
    },
    "required": ["file_path"]
}


def execute(params: dict, project_path: str) -> dict:
    """Main execution entry point."""
    file_path = params["file_path"]
    offset = params.get("offset", 1)
    limit = params.get("limit", 2000)

    # Validate path stays within project
    resolved = Path(project_path) / file_path
    try:
        resolved.resolve().relative_to(Path(project_path).resolve())
    except ValueError:
        return {"success": False, "error": "Path outside project"}

    # Implementation here

    return {
        "success": True,
        "output": "content with [LID:xxx] prefixes",
        "line_count": 42
    }
```

**Key conventions:**

- Standalone scripts (no cross-tool imports)
- `CONFIG_SCHEMA` for validation and agent tool descriptions
- Return dict with `success`, `output`, and optional metadata
- `__executor_id__` points to `python_function_runtime`
- Path validation: ensure all paths stay within `project_path`
- Minimal dependencies (stdlib preferred)

---

## 6. Dependencies

| Tool                              | Dependencies            | Notes                           |
| --------------------------------- | ----------------------- | ------------------------------- |
| read, write, edit_lines, glob, ls | None (stdlib)           | `pathlib`, `hashlib`, `json`    |
| grep                              | None (shell out)        | `rg` preferred, `grep` fallback |
| bash                              | None (stdlib)           | `subprocess`, `shlex`           |
| webfetch                          | `httpx` (optional)      | Fallback to `urllib`            |
| websearch                         | Provider SDK (optional) | Requires API key                |
| lsp                               | LSP client library      | Complex, P2                     |

---

## 7. Implementation Order

**P0 Tools (7):**

1. `file-system/read.py` — Foundation of line ID system
2. `file-system/edit_lines.py` — Line-addressed editing
3. `file-system/write.py` — File creation
4. `file-system/glob.py` — File discovery
5. `file-system/grep.py` — Content search with line IDs
6. `file-system/ls.py` — Directory listing
7. `bash/bash.py` — Shell execution

**P1 Tools (1):** 8. `web/webfetch.py` — URL fetching

**P2 Tools (2):** 9. `web/websearch.py` — Web search 10. `lsp/lsp.py` — LSP diagnostics

---

## 8. Design Principles

### 1. Data Over Prompts

Models reference stable data (line IDs), not generate matching text:

- No whitespace/indentation sensitivity
- No need for fuzzy matching fallbacks
- Deterministic success/fail based on ID existence

### 2. Transparent Persistence

The line ID cache is:

- **Automatic** — Created/managed by tools, no user action needed
- **Transparent** — Same interface whether cached or not
- **Self-healing** — Rebuilds on mismatch
- **Invisible to agents** — They just see `[LID:xxx]` and use it

### 3. Cache Invalidation Strategy

- `edit_lines` succeeds → Invalidate cache (file changed)
- `write` succeeds → Invalidate cache (file replaced)
- `read` detects change → Reconcile and update cache
- Pruning → Remove cache entries for files not accessed in 30 days

### 4. Fail Fast

- Invalid line ID? Fail immediately with clear error
- File changed since read? Fail with mismatch info
- No fuzzy matching, no silent failures

---

## 9. Example Workflows

### Basic Edit

```python
# 1. Read file
read({"file_path": "src/calculator.py"})
# → "[LID:a1b2c] def add(x, y):\n[LID:d3e4f]     return x + y"

# 2. Edit by line ID
edit_lines({
    "file_path": "src/calculator.py",
    "changes": [{"line_id": "d3e4f", "new_content": "    return x + y + 1"}]
})

# 3. Read again to verify
read({"file_path": "src/calculator.py"})
```

### Search and Edit

```python
# 1. Search
grep({"pattern": "def hello", "path": "src/"})
# → "src/app.py:42:[LID:abc123]:def hello():"

# 2. Edit using the line ID from search
edit_lines({
    "file_path": "src/app.py",
    "changes": [{"line_id": "abc123", "new_content": "def hello_world():"}]
})
```

### Multi-Step Refactoring

```python
# 1. Initial read
read({"file_path": "src/app.py"})

# 2. Edit (cache invalidated automatically)
edit_lines({
    "file_path": "src/app.py",
    "changes": [{"line_id": "abc123", "new_content": "..."}]
})

# 3. Read again (reconciles, preserves IDs for unchanged lines)
read({"file_path": "src/app.py"})

# 4. Another edit (some IDs from step 1 still valid)
edit_lines({
    "file_path": "src/app.py",
    "changes": [{"start_line_id": "def456", "end_line_id": "ghi789", "new_content": "..."}]
})
```

---

## 10. Comparison: RYE vs Traditional Approaches

### Editing Reliability

| Approach                | String Matching | Diff Parsing | **RYE Line IDs** |
| ----------------------- | --------------- | ------------ | ---------------- |
| Whitespace sensitivity  | High            | Medium       | **None**         |
| Model must reproduce    | Exact text      | Diff format  | **Just IDs**     |
| Failure rate (Grok 4)   | 50.7%           | 46.2%        | **~5%**          |
| Multi-step stability    | Poor            | Poor         | **Excellent**    |
| Cross-model reliability | Varies          | Varies       | **Consistent**   |

_Per [The Harness Problem](https://blog.can.ac/2026/02/12/the-harness-problem/)_

### Feature Comparison

| Feature                 | OpenCode | Claude Code | **RYE** |
| ----------------------- | -------- | ----------- | ------- |
| Line windowing          | ✅       | ✅          | ✅      |
| Persistent line IDs     | ❌       | ❌          | **✅**  |
| String-based editing    | ✅       | ✅          | ❌      |
| Line-addressed editing  | ❌       | ❌          | **✅**  |
| Cache reconciliation    | ❌       | ❌          | **✅**  |
| Content search with IDs | ❌       | ❌          | **✅**  |

---

## 11. Testing Strategy

### Unit Tests

1. **read cache management**
   - First read generates IDs
   - Second read returns cached IDs
   - Modified file reconciles correctly
   - Line preservation when inserting/deleting

2. **edit_lines validation**
   - Valid IDs → success
   - Invalid ID → fail with error
   - Multiple changes in one call
   - Range operations

3. **write cache invalidation**
   - Write clears cache
   - Subsequent read generates new IDs

4. **grep line ID lookup**
   - Returns line IDs from cache
   - Handles files without cache

### Integration Tests

1. **Round-trip:** read → edit → read
2. **Multi-step:** Multiple edits without re-reading
3. **Search-edit:** grep → edit using returned ID
4. **Error recovery:** Invalid ID → re-read → retry edit

---

## 12. Summary

**RYE's workspace tools provide:**

1. **Superior reliability** — Line IDs eliminate string matching failures
2. **Persistent state** — Cache survives sessions, enables multi-step workflows
3. **Model agnostic** — Works consistently across all LLMs
4. **Data-driven** — References stable identifiers, not prompt generation
5. **Self-managing** — Automatic cache creation, reconciliation, and cleanup

**Tool count:** 9 tools across 4 categories

**Expected impact:**

- +8-14% success rate improvement (per The Harness Problem benchmarks)
- 20-60% reduction in output tokens (fewer retry loops)
- Dramatically better experience for all models, especially smaller ones
