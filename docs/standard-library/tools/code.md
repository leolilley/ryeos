```yaml
id: tools-code
title: "Code Tools"
description: Development tools — NPM, diagnostics, TypeScript type checking, and LSP code intelligence
category: standard-library/tools
tags: [tools, code, npm, diagnostics, typescript, lsp]
version: "1.0.0"
```

# Code Tools

**Namespace:** `rye/code/`
**Runtime:** `node/node` (all four tools)

Four tools for development workflows — package management, linting, type checking, and language server code intelligence.

---

## `npm`

**Item ID:** `rye/code/npm/npm`

NPM and NPX operations — install packages, run scripts, build projects, and execute binaries.

### Parameters

| Name          | Type    | Required | Default | Description                                     |
| ------------- | ------- | -------- | ------- | ----------------------------------------------- |
| `action`      | string  | ✅       | —       | Action: `install`, `run`, `build`, `test`, `init`, `exec` |
| `args`        | array   | ❌       | `[]`    | Arguments for the action                        |
| `flags`       | object  | ❌       | `{}`    | CLI flags (e.g. `{"save_dev": true, "force": true}`) — single-char keys → `-k`, multi-char → `--key-name`, underscores → hyphens |
| `working_dir` | string  | ❌       | —       | Working directory (relative to project root or absolute) |
| `timeout`     | integer | ❌       | `120`   | Timeout in seconds                              |

### Actions

| Action    | Description                        | Example args              |
| --------- | ---------------------------------- | ------------------------- |
| `install` | Install packages                   | `["react", "react-dom"]`  |
| `run`     | Run an npm script                  | `["build"]`               |
| `build`   | Shortcut for `npm run build`       | —                         |
| `test`    | Shortcut for `npm test`            | —                         |
| `init`    | Initialize a new package.json      | —                         |
| `exec`    | Execute a binary via npx           | `["vite", "build"]`       |

### Example

```python
# Install dev dependencies
rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "install", "args": ["react", "react-dom"], "flags": {"save_dev": true}, "working_dir": "frontend"})

# Run a script
rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "run", "args": ["build"], "working_dir": "frontend"})

# Execute via npx
rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "exec", "args": ["vite", "build"], "working_dir": "frontend"})
```

---

## `diagnostics`

**Item ID:** `rye/code/diagnostics/diagnostics`

Run linters and type checkers against a file. Auto-detects available linters based on file extension.

### Parameters

| Name        | Type    | Required | Default | Description                                          |
| ----------- | ------- | -------- | ------- | ---------------------------------------------------- |
| `file_path` | string  | ✅       | —       | Path to the file to get diagnostics for              |
| `linters`   | array   | ❌       | auto    | Linter names to run (auto-detected from extension if omitted) |
| `timeout`   | integer | ❌       | `30`    | Timeout per linter in seconds                        |

### Supported Linters

| Linter        | Language       |
| ------------- | -------------- |
| `ruff`        | Python         |
| `mypy`        | Python         |
| `pylint`      | Python         |
| `flake8`      | Python         |
| `eslint`      | JavaScript/TS  |
| `tsc`         | TypeScript     |
| `go vet`      | Go             |
| `cargo clippy`| Rust           |

### Example

```python
# Auto-detect linters for a Python file
rye_execute(item_type="tool", item_id="rye/code/diagnostics/diagnostics",
    parameters={"file_path": "src/auth.py"})

# Run specific linters
rye_execute(item_type="tool", item_id="rye/code/diagnostics/diagnostics",
    parameters={"file_path": "src/auth.py", "linters": ["ruff", "mypy"]})
```

---

## `typescript`

**Item ID:** `rye/code/typescript/typescript`

TypeScript type checker — run `tsc --noEmit` against a whole project or a single file.

### Parameters

| Name          | Type    | Required | Default | Description                                      |
| ------------- | ------- | -------- | ------- | ------------------------------------------------ |
| `action`      | string  | ✅       | —       | `check` (whole project) or `check-file` (single file) |
| `file_path`   | string  | ❌       | —       | File to check (required for `check-file`)        |
| `working_dir` | string  | ❌       | —       | Directory containing `tsconfig.json`             |
| `strict`      | boolean | ❌       | `false` | Enable strict mode                               |
| `timeout`     | integer | ❌       | `60`    | Timeout in seconds                               |

### Example

```python
# Type-check the whole project
rye_execute(item_type="tool", item_id="rye/code/typescript/typescript",
    parameters={"action": "check", "working_dir": "frontend"})

# Type-check a single file in strict mode
rye_execute(item_type="tool", item_id="rye/code/typescript/typescript",
    parameters={"action": "check-file", "file_path": "src/auth.ts", "strict": true})
```

---

## `lsp`

**Item ID:** `rye/code/lsp/lsp`

A real Language Server Protocol client. Connects to language servers and provides code intelligence — go to definition, find references, hover info, symbols, call hierarchy, and more.

### Parameters

| Name        | Type    | Required | Default | Description                                      |
| ----------- | ------- | -------- | ------- | ------------------------------------------------ |
| `operation` | string  | ✅       | —       | LSP operation (see table below)                  |
| `file_path` | string  | ✅       | —       | Path to the file                                 |
| `line`      | integer | ✅       | —       | Line number (1-based)                            |
| `character` | integer | ✅       | —       | Character offset (1-based)                       |
| `timeout`   | integer | ❌       | `15`    | Timeout in seconds                               |

### Operations

| Operation               | Description                                    |
| ----------------------- | ---------------------------------------------- |
| `goToDefinition`        | Jump to where a symbol is defined              |
| `findReferences`        | Find all references to a symbol                |
| `hover`                 | Get type info and documentation                |
| `documentSymbol`        | List all symbols in a file                     |
| `workspaceSymbol`       | Search symbols across the workspace            |
| `goToImplementation`    | Find implementations of an interface/abstract  |
| `prepareCallHierarchy`  | Get call hierarchy item at position            |
| `incomingCalls`         | Find callers of a function                     |
| `outgoingCalls`         | Find functions called by a function            |

### Supported Servers

`typescript-language-server` · `pyright-langserver` · `gopls` · `rust-analyzer`

### Example

```python
# Go to definition
rye_execute(item_type="tool", item_id="rye/code/lsp/lsp",
    parameters={"operation": "goToDefinition", "file_path": "src/auth.ts", "line": 42, "character": 10})

# Find all references
rye_execute(item_type="tool", item_id="rye/code/lsp/lsp",
    parameters={"operation": "findReferences", "file_path": "src/auth.ts", "line": 42, "character": 10})

# List symbols in a file
rye_execute(item_type="tool", item_id="rye/code/lsp/lsp",
    parameters={"operation": "documentSymbol", "file_path": "src/auth.ts", "line": 1, "character": 1})

# Get hover info
rye_execute(item_type="tool", item_id="rye/code/lsp/lsp",
    parameters={"operation": "hover", "file_path": "src/auth.ts", "line": 42, "character": 10})
```
