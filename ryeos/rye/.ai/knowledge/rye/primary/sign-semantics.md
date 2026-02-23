<!-- rye:signed:2026-02-23T00:43:10Z:42d8f7ce10aa1e8789a3df4fe92c32417092b4f1e0d04ba19ecb7950ec30e42d:Od6Lqc7hXFvhYMHxQQXM4Y6zuyY4DHKNLDImRZOtVLQX1RIKj1yQMkaOVuV2qK-FE2zOLOKb3iCF37G-WT6GAA==:9fbfabe975fa5a7f -->

```yaml
id: sign-semantics
title: "rye_sign — MCP Tool Semantics"
entry_type: reference
category: rye/primary
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - sign
  - mcp-tool
  - api
  - integrity
references:
  - execute-semantics
  - search-semantics
  - load-semantics
  - "docs/tools-reference/sign.md"
```

# rye_sign — MCP Tool Semantics

Validate item structure using schema-driven extractors and sign the file with an integrity hash. Supports batch signing via glob patterns.

## Parameters

| Parameter      | Type   | Required | Default     | Description                                                              |
| -------------- | ------ | -------- | ----------- | ------------------------------------------------------------------------ |
| `item_type`    | string | yes      | —           | `"directive"`, `"tool"`, or `"knowledge"`                                |
| `item_id`      | string | yes      | —           | Item identifier or glob pattern (e.g., `"rye/core/*"` for batch)         |
| `project_path` | string | yes      | —           | Absolute path to the project root                                        |
| `source`       | string | no       | `"project"` | Where to find the item: `"project"`, `"user"`, or `"system"`             |

## Signing Process

1. **Load and parse** — File read and routed to parser by item type and extension
2. **Schema-driven validation** — Parsed data validated against extractor schema
3. **Compute integrity hash** — Content hash computed from file contents including metadata
4. **Write signature** — Signature comment written to file via `MetadataManager`

### Parser routing

| Item Type   | Extension      | Parser                 |
| ----------- | -------------- | ---------------------- |
| `directive` | `.md`          | `markdown_xml`         |
| `tool`      | `.py`          | `python_ast`           |
| `tool`      | `.yaml`/`.yml` | `yaml`                 |
| `knowledge` | `.md`          | `markdown_frontmatter` |

### Validation checks

- Required fields are present
- Field types are correct
- Filename matches expected patterns
- Tool field mappings applied (e.g., `__executor_id__` → `executor_id`)

## Signature Format

Each file type uses its own comment syntax:

| Item Type   | Extension      | Format                                               |
| ----------- | -------------- | ---------------------------------------------------- |
| `directive` | `.md`          | `<!-- rye:signed:<timestamp>:<hash>:<signature> -->` |
| `tool`      | `.py`          | `# rye:signed:<timestamp>:<hash>:<signature>`        |
| `tool`      | `.yaml`/`.yml` | `# rye:signed:<timestamp>:<hash>:<signature>`        |
| `knowledge` | `.md`          | `<!-- rye:signed:<timestamp>:<hash>:<signature> -->` |

### Signature components

| Component     | Description                        |
| ------------- | ---------------------------------- |
| `timestamp`   | When the item was signed           |
| `hash`        | Integrity hash of file contents    |
| `signature`   | Ed25519 cryptographic signature    |

## When to Sign

**Re-sign after any content change.** The integrity check compares current file content against the stored hash. Any modification — edits, moves, renames — invalidates the signature.

Rule: **edit → sign → execute/load**. Unsigned or stale-signed items fail integrity verification on `rye_execute` and `rye_load`.

## Response (Single Item)

```json
{
  "status": "signed",
  "item_id": "rye/core/create_directive",
  "path": "/home/user/my-project/.ai/directives/rye/core/create_directive.md",
  "location": "project",
  "signature": {
    "timestamp": "2026-02-17T10:30:00Z",
    "hash": "a1b2c3d4...",
    "valid": true
  },
  "warnings": [],
  "message": "Directive validated and signed."
}
```

## Batch Signing

Glob patterns in `item_id` (`*` or `?`) trigger batch mode.

### Glob pattern examples

| Pattern          | Matches                                     |
| ---------------- | ------------------------------------------- |
| `rye/core/*`     | All items directly under `rye/core/`        |
| `*`              | All items in the type directory (recursive) |
| `rye/agent/*`    | All items directly under `rye/agent/`       |
| `my-project/*/*` | Items two levels deep under `my-project/`   |

### Batch response

```json
{
  "status": "completed",
  "signed": [
    "rye/core/create_directive",
    "rye/core/create_tool",
    "rye/core/create_knowledge"
  ],
  "failed": [
    {
      "item": "rye/core/broken_directive",
      "error": "Validation failed",
      "details": ["Missing required field: description"]
    }
  ],
  "total": 4,
  "summary": "Signed 3/4 items, 1 failed"
}
```

## Verification Flow

When `rye_load` or `rye_execute` is called, items are verified:

1. Read stored signature from file
2. Recompute content hash from current file contents
3. Compare hashes — mismatch → `IntegrityError`

### IntegrityError conditions

| Condition                           | Cause                                   |
| ----------------------------------- | --------------------------------------- |
| Content modified since signing      | File edited without re-signing          |
| File moved to different path        | Path changed without re-signing         |
| Signature missing                   | File never signed or signature stripped  |
| Signature malformed                 | Corrupt or truncated signature line     |

## Validation Error Responses

```json
{
  "status": "error",
  "error": "Validation failed",
  "issues": [
    "Missing required field: description",
    "Field 'version' expected string, got int"
  ],
  "path": "/home/user/my-project/.ai/directives/my-project/deploy.md"
}
```

Item not found:

```json
{
  "status": "error",
  "error": "Directive not found: my-project/deploy",
  "hint": "Create file at .ai/directives/my-project/deploy.md"
}
```

## Usage Examples

```python
# Sign a single directive
rye_sign(
    item_type="directive",
    item_id="my-project/workflows/deploy",
    project_path="/home/user/my-project"
)

# Sign a tool
rye_sign(
    item_type="tool",
    item_id="my-project/scraper/fetch_page",
    project_path="/home/user/my-project",
    source="project"
)

# Batch sign all directives in a namespace
rye_sign(
    item_type="directive",
    item_id="my-project/workflows/*",
    project_path="/home/user/my-project"
)

# Batch sign all project knowledge
rye_sign(
    item_type="knowledge",
    item_id="*",
    project_path="/home/user/my-project"
)

# Sign a knowledge entry
rye_sign(
    item_type="knowledge",
    item_id="my-project/patterns/error-handling",
    project_path="/home/user/my-project"
)
```
