# Lockfile Format

Specification of the lockfile format for pinning resolved tool chains with per-element integrity verification.

## Overview

Lockfiles capture the exact resolution of a tool's executor chain at execution time. They enable:

- **Reproducibility**: Same chain resolution every time
- **Security**: Integrity hash per chain element detects changes since lockfile creation
- **Caching**: Skip chain resolution when lockfile matches
- **Audit**: Complete execution trace with provenance

Lockfiles are stored as JSON in `{USER_SPACE}/lockfiles/` (default) or `{PROJECT}/lockfiles/` (opt-in).

## File Naming

```
{tool_id}@{version}.lock.json
```

Example: `web/scraper@1.0.0.lock.json`

## Format

```json
{
  "lockfile_version": 1,
  "generated_at": "2026-02-11T00:00:00+00:00",
  "root": {
    "tool_id": "web/scraper",
    "version": "1.0.0",
    "integrity": "a1b2c3d4...64 hex chars"
  },
  "resolved_chain": [
    {
      "item_id": "web/scraper",
      "space": "project",
      "tool_type": "python",
      "executor_id": "rye/core/runtimes/python_runtime",
      "integrity": "a1b2c3d4...64 hex chars"
    },
    {
      "item_id": "rye/core/runtimes/python_runtime",
      "space": "system",
      "tool_type": "runtime",
      "executor_id": "rye/core/primitives/subprocess",
      "integrity": "e5f6a7b8...64 hex chars"
    },
    {
      "item_id": "rye/core/primitives/subprocess",
      "space": "system",
      "tool_type": "primitive",
      "executor_id": null,
      "integrity": "c9d0e1f2...64 hex chars"
    }
  ],
  "registry": null
}
```

## Fields

### Top-level

| Field | Type | Required | Description |
|---|---|---|---|
| `lockfile_version` | integer | Yes | Format version (currently `1`) |
| `generated_at` | string | Yes | ISO 8601 timestamp of generation |
| `root` | object | Yes | Root tool metadata |
| `resolved_chain` | array | Yes | Ordered chain elements (tool → primitive) |
| `registry` | object | No | Registry metadata if pulled from registry |

### `root`

| Field | Type | Description |
|---|---|---|
| `tool_id` | string | Tool identifier (relative path without extension) |
| `version` | string | Semver version of the tool |
| `integrity` | string | SHA256 content hash of the root tool |

### `resolved_chain[n]`

| Field | Type | Description |
|---|---|---|
| `item_id` | string | Tool identifier (relative path without extension) |
| `space` | string | Resolution space: `"project"`, `"user"`, or `"system"` |
| `tool_type` | string | Tool type from metadata (e.g., `"python"`, `"runtime"`, `"primitive"`) |
| `executor_id` | string \| null | Next executor in chain, `null` for primitives |
| `integrity` | string | SHA256 content hash of this chain element |

## Portability

Chain elements store `item_id` + `space` instead of absolute file paths. Paths are re-resolved at verification time using the 3-tier space precedence system:

- **project**: `{project_path}/.ai/tools/{item_id}{ext}`
- **user**: `{user_space}/tools/{item_id}{ext}`
- **system**: `{system_space}/tools/{item_id}{ext}`

This ensures lockfiles remain valid across machines and project relocations.

## Integrity Verification

On lockfile load, every element is verified:

1. **Root integrity**: `root.integrity` compared against current hash of the root tool
2. **Chain integrity**: Each `resolved_chain[n].integrity` compared against the current hash of that chain element
3. **Mismatch action**: Execution blocked with error instructing user to re-sign and delete stale lockfile

Integrity hashes are computed via `MetadataManager.compute_hash(ItemType.TOOL, content, file_path=..., project_path=...)` which hashes the content after stripping the signature line.

This is **separate from** Ed25519 signature verification, which happens independently for every chain element before execution.

## Three-Tier Precedence

Lockfile resolution follows the same 3-tier precedence as tools:

| Priority | Space | Location | Access |
|---|---|---|---|
| 1 (highest) | project | `{project}/lockfiles/` | read-write |
| 2 | user | `{user_space}/lockfiles/` | read-write |
| 3 (lowest) | system | `{system_space}/lockfiles/` | read-only |

First match wins on read. Write location depends on configured scope (`user` by default).
