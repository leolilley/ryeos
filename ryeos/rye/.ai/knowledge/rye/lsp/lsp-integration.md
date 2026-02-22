<!-- rye:signed:2026-02-22T02:41:03Z:64533928c79cb18d21c9ad300af433ad834193e63f8535ad19d7942421715621:jSRHEix25yD0fVFdAXnncHqqOqOxfqwNRzDLUXDEtmpi9rJ7jh-JTE4_5-bsjD4P10Xdr_CrMNln2Kf-GgQYDQ==:9fbfabe975fa5a7f -->

```yaml
id: lsp-integration
title: LSP Diagnostics Integration
entry_type: reference
category: rye/lsp
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - lsp
  - language-server
  - diagnostics
  - code-intelligence
references:
  - "docs/standard-library/tools/index.md"
```

# LSP Diagnostics Integration

Run linters on a file and return structured diagnostics. Auto-detects file type and available linters.

## Tool Identity

| Field         | Value                                      |
| ------------- | ------------------------------------------ |
| Item ID       | `rye/lsp/lsp`                              |
| Namespace     | `rye/lsp/`                                 |
| Runtime       | `python_function_runtime`                  |
| Executor ID   | `rye/core/runtimes/python_function_runtime` |

## Parameters

| Name        | Type   | Required | Default       | Description                                    |
| ----------- | ------ | -------- | ------------- | ---------------------------------------------- |
| `file_path` | string | ✅       | —             | Path to file to get diagnostics for            |
| `linters`   | array  | ❌       | auto-detected | Linter names to run (overrides auto-detection) |

## Invocation

```python
rye_execute(item_type="tool", item_id="rye/lsp/lsp",
    parameters={"file_path": "src/main.py"})

rye_execute(item_type="tool", item_id="rye/lsp/lsp",
    parameters={"file_path": "src/main.py", "linters": ["ruff", "mypy"]})
```

## Supported Languages & Linters

| File Type   | Extensions                           | Linters                          |
| ----------- | ------------------------------------ | -------------------------------- |
| Python      | `.py`                                | ruff, mypy, pylint, flake8       |
| JavaScript  | `.js`, `.jsx`                        | eslint, tsc                      |
| TypeScript  | `.ts`, `.tsx`                        | eslint, tsc                      |
| Go          | `.go`                                | golint, go vet                   |
| Rust        | `.rs`                                | cargo clippy, rustc              |
| C/C++       | `.c`, `.h`, `.cpp`, `.hpp`           | —                                |
| Ruby        | `.rb`                                | —                                |
| Java        | `.java`                              | —                                |
| Kotlin      | `.kt`                                | —                                |
| Swift       | `.swift`                             | —                                |

## Linter Priority

Checked in order: `ruff` → `mypy` → `pylint` → `eslint` → `tsc`

## Linter Discovery

1. Checks system PATH via `shutil.which()`
2. Also checks project `.venv/bin/` for venv-installed linters
3. Only available linters are executed

## Linter Output Parsing

| Linter  | Output Format            | Parsing Method                      |
| ------- | ------------------------ | ----------------------------------- |
| ruff    | `--output-format=json`   | JSON array of issue objects         |
| mypy    | `--no-error-summary`     | Regex: `file:line: severity: msg`   |
| pylint  | `--output-format=json`   | JSON array                          |
| flake8  | `--format=default`       | Regex: `file:line:col: CODE msg`    |
| eslint  | `--format=json`          | JSON with messages array            |
| tsc     | `--noEmit --pretty false`| Text output                         |

## Diagnostic Format

Each diagnostic object:

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

## Deduplication

Diagnostics are deduplicated by `(line, column, message)` tuple across all linters.

## Output Limits

| Limit              | Value                  |
| ------------------ | ---------------------- |
| Max output         | 32,768 bytes (32 KB)   |
| Linter timeout     | 30 seconds per linter  |

## Return Format

### Success (with diagnostics)

```json
{
  "success": true,
  "output": "src/main.py:42:8: error: Undefined variable [F821]",
  "diagnostics": [...],
  "linters_checked": ["ruff", "mypy"],
  "file_type": "python"
}
```

### Success (no issues)

```json
{
  "success": true,
  "output": "No issues found in src/main.py",
  "diagnostics": [],
  "linters_checked": ["ruff"],
  "file_type": "python"
}
```

### No linters available

```json
{
  "success": true,
  "output": "No linters available for python files",
  "diagnostics": [],
  "linters_checked": []
}
```

## Formatted Output

Diagnostics are formatted as:

```
file_path:line:col: severity: message [code]
```

Sorted by `(line, column)` ascending.

## Directive Wrapper

The `rye/lsp/lsp` directive provides a thin orchestration layer:

- Validates `file_path` is non-empty
- Calls the LSP tool
- Returns diagnostics as output
- Model tier: `haiku` (lightweight)
- Max turns: 3, max tokens: 4096

## Error Conditions

| Error                         | Cause                                |
| ----------------------------- | ------------------------------------ |
| Path outside project          | Resolved path fails sandbox check    |
| File not found                | Path does not exist                  |
| Path is a directory           | Directories are not supported        |
| Linter timeout                | Individual linter exceeds 30 seconds |
