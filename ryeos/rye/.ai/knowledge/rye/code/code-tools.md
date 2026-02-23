<!-- rye:signed:2026-02-23T02:07:54Z:eaae71aae2b29d35339f30f1e3f0ac6b5e1052103319759a0a76b3d6778261cf:69yq6YTR1hK1UnsX492ol6AIMYhLXCs3K_f1HV4_LYioN_emSNsU7sOfow4OZ-h65Gav3N25qi0rjwEv1Ou3Cw==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->

```yaml
id: code-tools
title: Code Intelligence Tools
entry_type: reference
category: rye/code
version: "1.0.0"
author: rye-os
created_at: 2026-02-23T00:00:00Z
tags:
  - code
  - lsp
  - diagnostics
  - typescript
  - npm
  - linters
  - type-checking
references:
  - "docs/standard-library/tools/index.md"
```

# Code Intelligence Tools

Four tools for code analysis, linting, type checking, and package management. All run as Node.js tools.

## Namespace & Runtime

| Field       | Value                               |
| ----------- | ----------------------------------- |
| Namespace   | `rye/code/`                         |
| Runtime     | `javascript`                        |
| Executor ID | `rye/core/runtimes/node/node`       |

---

## `diagnostics`

**Item ID:** `rye/code/diagnostics/diagnostics`

Run linters and type checkers on source files. Auto-detects file type and available linters.

### Parameters

| Name        | Type    | Required | Default       | Description                                                                 |
| ----------- | ------- | -------- | ------------- | --------------------------------------------------------------------------- |
| `file_path` | string  | ✅       | —             | Path to file to get diagnostics for                                         |
| `linters`   | array   | ❌       | auto-detected | Linter names to run (overrides auto-detection)                              |
| `timeout`   | integer | ❌       | `30`          | Timeout per linter in seconds                                               |

### Supported Languages & Linters

| File Type   | Extensions                           | Linters                          |
| ----------- | ------------------------------------ | -------------------------------- |
| Python      | `.py`                                | ruff, mypy, pylint, flake8       |
| JavaScript  | `.js`, `.jsx`, `.mjs`, `.cjs`        | eslint, tsc                      |
| TypeScript  | `.ts`, `.tsx`                        | eslint, tsc                      |
| Go          | `.go`                                | go vet                           |
| Rust        | `.rs`                                | cargo clippy                     |

### Linter Discovery

1. Checks system PATH via `which`
2. Only available linters are executed
3. Diagnostics are deduplicated by `(line, column, message)` tuple across all linters

### Diagnostic Format

```json
{
  "line": 42,
  "column": 8,
  "severity": "error",
  "message": "Undefined variable 'x'",
  "code": "F821"
}
```

Severity values: `error`, `warning`, `info`

### Limits

| Limit              | Value                  |
| ------------------ | ---------------------- |
| Max output         | 32,768 bytes (32 KB)   |
| Linter timeout     | 30 seconds per linter  |

### Return

```json
{
  "success": true,
  "output": "src/main.py:42:8: error: Undefined variable [F821]",
  "diagnostics": [...],
  "linters_checked": ["ruff", "mypy"],
  "file_type": "python"
}
```

### Invocation

```python
rye_execute(item_type="tool", item_id="rye/code/diagnostics/diagnostics",
    parameters={"file_path": "src/main.py"})

rye_execute(item_type="tool", item_id="rye/code/diagnostics/diagnostics",
    parameters={"file_path": "src/main.py", "linters": ["ruff", "mypy"]})
```

---

## `lsp`

**Item ID:** `rye/code/lsp/lsp`

Real LSP client — spawns a language server, sends a request, and returns the result. Supports go-to-definition, find-references, hover, symbols, call hierarchy, and more.

### Parameters

| Name        | Type    | Required | Default | Description                        |
| ----------- | ------- | -------- | ------- | ---------------------------------- |
| `operation` | string  | ✅       | —       | LSP operation to perform (see below) |
| `file_path` | string  | ✅       | —       | Path to the file                   |
| `line`      | integer | ✅       | —       | Line number (1-based)              |
| `character` | integer | ✅       | —       | Character offset (1-based)         |
| `timeout`   | integer | ❌       | `15`    | Timeout in seconds                 |

### Operations

| Operation                | LSP Method                              | Description                          |
| ------------------------ | --------------------------------------- | ------------------------------------ |
| `goToDefinition`         | `textDocument/definition`               | Jump to symbol definition            |
| `findReferences`         | `textDocument/references`               | Find all references to a symbol      |
| `hover`                  | `textDocument/hover`                    | Get type info / docs at position     |
| `documentSymbol`         | `textDocument/documentSymbol`           | List all symbols in the file         |
| `workspaceSymbol`        | `workspace/symbol`                      | Search symbols across the workspace  |
| `goToImplementation`     | `textDocument/implementation`           | Find implementations of an interface |
| `prepareCallHierarchy`   | `textDocument/prepareCallHierarchy`     | Get call hierarchy item at position  |
| `incomingCalls`          | `callHierarchy/incomingCalls`           | Who calls this function?             |
| `outgoingCalls`          | `callHierarchy/outgoingCalls`           | What does this function call?        |

### Supported Language Servers

| Server ID              | Extensions                                               | Command                              |
| ---------------------- | -------------------------------------------------------- | ------------------------------------ |
| `typescript`           | `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`, `.mts`, `.cts` | `typescript-language-server --stdio`  |
| `pyright`              | `.py`                                                    | `pyright-langserver --stdio`          |
| `gopls`                | `.go`                                                    | `gopls serve`                         |
| `rust-analyzer`        | `.rs`                                                    | `rust-analyzer`                       |

### Return

```json
{
  "success": true,
  "output": "[{\"uri\": \"src/auth.ts\", \"range\": {...}}]",
  "operation": "goToDefinition",
  "server": "typescript",
  "results": [...]
}
```

File URIs in results are converted to project-relative paths.

### Invocation

```python
rye_execute(item_type="tool", item_id="rye/code/lsp/lsp",
    parameters={"operation": "goToDefinition", "file_path": "src/main.ts", "line": 10, "character": 5})

rye_execute(item_type="tool", item_id="rye/code/lsp/lsp",
    parameters={"operation": "findReferences", "file_path": "src/auth.py", "line": 42, "character": 8})

rye_execute(item_type="tool", item_id="rye/code/lsp/lsp",
    parameters={"operation": "hover", "file_path": "src/utils.ts", "line": 15, "character": 12})

rye_execute(item_type="tool", item_id="rye/code/lsp/lsp",
    parameters={"operation": "incomingCalls", "file_path": "src/api.ts", "line": 20, "character": 10})
```

---

## `typescript`

**Item ID:** `rye/code/typescript/typescript`

TypeScript type checker — runs `tsc --noEmit` for type checking without producing build output.

### Parameters

| Name          | Type    | Required | Default | Description                                    |
| ------------- | ------- | -------- | ------- | ---------------------------------------------- |
| `action`      | string  | ✅       | —       | `check` (whole project) or `check-file` (single file) |
| `file_path`   | string  | ❌       | —       | File to check (required for `check-file`)      |
| `working_dir` | string  | ❌       | project root | Directory containing `tsconfig.json`       |
| `strict`      | boolean | ❌       | `false` | Enable strict mode                             |
| `timeout`     | integer | ❌       | `60`    | Timeout in seconds                             |

### Diagnostic Format

```json
{
  "file": "src/main.ts",
  "line": 10,
  "column": 5,
  "severity": "error",
  "message": "Property 'x' does not exist on type 'Y'",
  "code": "TS2339"
}
```

### Limits

| Limit              | Value                  |
| ------------------ | ---------------------- |
| Max output         | 51,200 bytes (50 KB)   |
| Default timeout    | 60 seconds             |

### Return

```json
{
  "success": true,
  "output": "No type errors found.",
  "diagnostics": [],
  "error_count": 0,
  "command": "tsc --noEmit --pretty false"
}
```

### Invocation

```python
rye_execute(item_type="tool", item_id="rye/code/typescript/typescript",
    parameters={"action": "check"})

rye_execute(item_type="tool", item_id="rye/code/typescript/typescript",
    parameters={"action": "check-file", "file_path": "src/main.ts", "strict": true})

rye_execute(item_type="tool", item_id="rye/code/typescript/typescript",
    parameters={"action": "check", "working_dir": "packages/core"})
```

---

## `npm`

**Item ID:** `rye/code/npm/npm`

NPM/NPX operations — install packages, run scripts, execute commands.

### Parameters

| Name          | Type    | Required | Default      | Description                                                         |
| ------------- | ------- | -------- | ------------ | ------------------------------------------------------------------- |
| `action`      | string  | ✅       | —            | `install`, `run`, `build`, `test`, `init`, or `exec`                |
| `args`        | array   | ❌       | `[]`         | Arguments (package names for install, script name for run, command for exec) |
| `flags`       | object  | ❌       | `{}`         | Flags to pass (e.g. `{ "save_dev": true, "force": true }`)         |
| `working_dir` | string  | ❌       | project root | Working directory (relative or absolute)                            |
| `timeout`     | integer | ❌       | `120`        | Timeout in seconds                                                  |

### Actions

| Action    | Command Built                  | Notes                                         |
| --------- | ------------------------------ | --------------------------------------------- |
| `install` | `npm install [packages] [flags]` | No args = install all from package.json       |
| `run`     | `npm run <script> [flags]`     | First arg is script name                      |
| `build`   | `npm run build [flags]`        | Shorthand for `run build`                     |
| `test`    | `npm test [flags]`             | Runs the test script                          |
| `init`    | `npm init -y [flags]`          | Initialize a new package.json                 |
| `exec`    | `npx <command> [flags]`        | Requires at least one arg (the command)       |

### Flag Handling

Flags object keys are converted to CLI flags: single-char keys become `-k`, multi-char keys become `--key-name` (underscores → hyphens). Boolean `true` adds the flag, string values add `--flag value`.

### Limits

| Limit              | Value                  |
| ------------------ | ---------------------- |
| Max output         | 51,200 bytes (50 KB)   |
| Default timeout    | 120 seconds            |

### Return

```json
{
  "success": true,
  "output": "added 42 packages...",
  "stdout": "...",
  "stderr": "",
  "exit_code": 0,
  "truncated": false,
  "command": "npm install express"
}
```

### Invocation

```python
rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "install", "args": ["express", "cors"]})

rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "install", "args": ["typescript"], "flags": {"save_dev": true}})

rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "run", "args": ["lint"]})

rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "exec", "args": ["prisma", "migrate", "dev"]})

rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "build", "working_dir": "packages/frontend"})
```

---

## Error Conditions

| Error                         | Tool         | Cause                                |
| ----------------------------- | ------------ | ------------------------------------ |
| File not found                | diagnostics, lsp, typescript | Path does not exist      |
| No linters available          | diagnostics  | No matching linter found in PATH     |
| No LSP server available       | lsp          | No server installed for file type    |
| Unknown operation             | lsp          | Invalid operation name               |
| Timeout                       | all          | Command exceeds timeout              |

## Usage Patterns

```python
# Run diagnostics then jump to definition of an error symbol
diag = rye_execute(item_type="tool", item_id="rye/code/diagnostics/diagnostics",
    parameters={"file_path": "src/main.py"})

defn = rye_execute(item_type="tool", item_id="rye/code/lsp/lsp",
    parameters={"operation": "goToDefinition", "file_path": "src/main.py", "line": 42, "character": 8})

# Full project type check
rye_execute(item_type="tool", item_id="rye/code/typescript/typescript",
    parameters={"action": "check"})

# Install deps then build
rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "install"})
rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "build"})
```
