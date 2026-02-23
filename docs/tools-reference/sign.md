```yaml
id: sign
title: "rye_sign"
description: Validate and sign items with integrity hashes
category: tools-reference
tags: [sign, mcp-tool, api, integrity]
version: "1.0.0"
```

# rye_sign

Validate item structure using schema-driven extractors and sign the file with an integrity hash. Supports batch signing via glob patterns.

## Parameters

| Parameter      | Type   | Required | Default     | Description                                                              |
| -------------- | ------ | -------- | ----------- | ------------------------------------------------------------------------ |
| `item_type`    | string | yes      | —           | `"directive"`, `"tool"`, or `"knowledge"`                                |
| `item_id`      | string | yes      | —           | Item identifier or glob pattern (e.g., `"rye/core/*"` for batch signing) |
| `project_path` | string | yes      | —           | Absolute path to the project root                                        |
| `source`       | string | no       | `"project"` | Where to find the item: `"project"`, `"user"`, or `"system"`             |

## Signing Process

1. **Load and parse** — The file is read and routed to the appropriate parser based on item type and file extension:
   - Directives: `markdown/xml` parser
   - Tools (`.py`): `python/ast` parser
   - Tools (`.yaml`/`.yml`): `yaml/yaml` parser
   - Tools (`.js`/`.ts`/`.mjs`/`.cjs`): `javascript/javascript` parser
   - Knowledge: `markdown/frontmatter` parser

2. **Schema-driven validation** — Parsed data is validated against the extractor schema for the item type. Checks include:
   - Required fields are present
   - Field types are correct
   - Filename matches expected patterns
   - Tool field mappings are applied (e.g., `__executor_id__` → `executor_id`)

3. **Compute integrity hash** — A content hash is computed from the file contents including metadata.

4. **Write signature** — The signature comment is written to the file via `MetadataManager`.

## Signature Format

Each file type uses its own comment syntax for the signature line:

| Item Type | Extension      | Signature Format                                     |
| --------- | -------------- | ---------------------------------------------------- |
| Directive | `.md`          | `<!-- rye:signed:<timestamp>:<hash>:<signature> -->` |
| Tool      | `.py`          | `# rye:signed:<timestamp>:<hash>:<signature>`        |
| Tool      | `.yaml`/`.yml` | `# rye:signed:<timestamp>:<hash>:<signature>`        |
| Tool      | `.ts`/`.js`/`.mjs`/`.cjs` | `// rye:signed:<timestamp>:<hash>:<signature>` |
| Knowledge | `.md`          | `<!-- rye:signed:<timestamp>:<hash>:<signature> -->` |

Signature formats are data-driven — configured per-extension in `tool_extractor.yaml` via the `signature_formats` field. Each item type (tool, directive, knowledge) has its own extractor with independent format mappings.

The signature contains:

- **Timestamp** — When the item was signed
- **Content hash** — Integrity hash of the file contents
- **Signature** — Ed25519 cryptographic signature

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

Use glob patterns in `item_id` to sign multiple items at once. Patterns containing `*` or `?` trigger batch mode.

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

## Validation Errors

When validation fails, the response includes specific issues:

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

For items not found, a hint is provided:

```json
{
  "status": "error",
  "error": "Directive not found: my-project/deploy",
  "hint": "Create file at .ai/directives/my-project/deploy.md"
}
```

## Verification

When items are loaded or executed via `rye_load` or `rye_execute`, their integrity is verified against the stored signature. Verification fails if:

- The file content has been modified since signing
- The file was moved to a different path without re-signing
- The signature is missing or malformed

Re-sign the item after any modification to restore integrity.

## Examples

### Sign a single directive

```python
rye_sign(
    item_type="directive",
    item_id="my-project/workflows/deploy",
    project_path="/home/user/my-project"
)
```

### Sign a tool from the project space

```python
rye_sign(
    item_type="tool",
    item_id="my-project/scraper/fetch_page",
    project_path="/home/user/my-project",
    source="project"
)
```

### Batch sign all directives in a namespace

```python
rye_sign(
    item_type="directive",
    item_id="my-project/workflows/*",
    project_path="/home/user/my-project"
)
```

### Batch sign all project knowledge entries

```python
rye_sign(
    item_type="knowledge",
    item_id="*",
    project_path="/home/user/my-project"
)
```

### Sign a knowledge entry

```python
rye_sign(
    item_type="knowledge",
    item_id="my-project/patterns/error-handling",
    project_path="/home/user/my-project"
)
```
