```yaml
id: cas
title: "Content-Addressed Store"
description: Content-addressed storage for immutable objects, blobs, and mutable refs
category: internals
tags: [cas, content-addressed, objects, blobs, integrity]
version: "1.0.0"
```

# Content-Addressed Store

CAS is the foundational storage primitive in Rye OS. Every meaningful object is immutable and keyed by its content hash. Write-once semantics — if a path exists, skip. No overwrites, no conflicts, no corruption from concurrent writes.

CAS lives in two layers:

- **Lillux primitives** (`lillux/kernel/lillux/primitives/cas.py`) — kernel-level, type-agnostic blob and object storage
- **Rye CAS operations** (`ryeos/rye/cas/store.py`) — Rye-level layer that knows about item types, spaces, and manifests

## Storage Layout

```
.ai/objects/
  blobs/
    ab/cd/<sha256>              — raw bytes (file contents, large outputs)
  objects/
    ab/cd/<sha256>.json         — canonical JSON (typed objects)
  cache/
    nodes/<cache_key>.json      — node execution cache pointers
  refs/
    graphs/<run_id>.json        — mutable pointers to latest execution snapshots
```

Directory sharding uses 2 levels: first 2 chars of the hash, then next 2 chars. Same pattern as git objects. This keeps directory listing sizes manageable at scale.

## Lillux Primitives

**Location:** `lillux/kernel/lillux/primitives/cas.py`

Kernel-level, type-agnostic. Same layer as `integrity.py` and `signing.py`. Lillux has no knowledge of Rye item types, spaces, or `.ai/` conventions — it stores and retrieves bytes and dicts by hash.

### Interface

| Function                                                     | Description                                                                        |
| ------------------------------------------------------------ | ---------------------------------------------------------------------------------- |
| `store_blob(data: bytes, root: Path) -> str`                 | Store raw bytes, return SHA256 hex digest. Skip if exists. Atomic (tmp + rename).  |
| `store_object(data: dict, root: Path) -> str`                | Canonical JSON via `compute_integrity()`, store as `.json`. Return integrity hash. |
| `get_blob(hash: str, root: Path) -> bytes \| None`           | Read blob by hash.                                                                 |
| `get_object(hash: str, root: Path) -> dict \| None`          | Read object by hash.                                                               |
| `has(hash: str, root: Path) -> bool`                         | Check existence (blob or object). Delegates to `has_blob()` and `has_object()`.    |
| `has_blob(hash: str, root: Path) -> bool`                    | Check if hash exists as a blob specifically (not objects).                         |
| `has_object(hash: str, root: Path) -> bool`                  | Check if hash exists as an object specifically (not blobs).                        |
| `has_many(hashes: list[str], root: Path) -> dict[str, bool]` | Batch existence check.                                                             |

### Rules

- **Write-once** — if the target path exists, skip. Content-addressed = idempotent.
- **Atomic writes** — write to a tmp file, then `os.rename()` into place. No partial writes visible.
- **Hashing** — blobs use `hashlib.sha256` on raw bytes. Objects use `compute_integrity()` (canonical JSON → SHA256).

### Write Path

```python
from rye.primitives.cas import store_blob, store_object

# Store raw bytes → SHA256 hex
blob_hash = store_blob(file_bytes, root=objects_dir)
# Writes to: {root}/blobs/ab/cd/{sha256}

# Store typed object → integrity hash
obj_hash = store_object({"kind": "item_source", ...}, root=objects_dir)
# Writes to: {root}/objects/ab/cd/{sha256}.json
```

### Read Path

```python
from rye.primitives.cas import get_blob, get_object, has

if has(some_hash, root=objects_dir):
    data = get_blob(some_hash, root=objects_dir)  # bytes | None
    obj = get_object(some_hash, root=objects_dir)  # dict | None
```

## Rye CAS Operations

**Location:** `ryeos/rye/cas/store.py`

Rye-level layer. Knows about item types, spaces, and manifests. Built on Lillux CAS primitives.

### Interface

| Function                                                      | Description                                                                                                                         |
| ------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `cas_root(project_path) -> Path`                              | Returns `{project}/.ai/objects/`                                                                                                    |
| `user_cas_root() -> Path`                                     | Returns `{USER_SPACE}/.ai/objects/`                                                                                                 |
| `ingest_item(item_type, file_path, project_path) -> ItemRef`  | Read file → store as blob + create `item_source` object → return `ItemRef(blob_hash, object_hash, integrity, signature_info)`       |
| `ingest_directory(base_path, project_path) -> dict[str, str]` | Walk `.ai/` tree → ingest all items → return `{relative_path: object_hash}`. Skips `.ai/objects/` and `.ai/agent/` (runtime state). |
| `materialize_item(object_hash, target_path, root) -> Path`    | Read `item_source` object → extract blob → write to target_path                                                                     |
| `write_ref(ref_path, hash_hex) -> None`                       | Atomically write a mutable ref pointer                                                                                              |
| `read_ref(ref_path) -> str \| None`                           | Read a ref pointer                                                                                                                  |

### Ingest Flow

`ingest_item` is the primary entry point for getting files into the store:

1. Read the file as raw bytes
2. `store_blob(bytes)` → `blob_hash`
3. Extract signature info and integrity from the file content
4. Build an `item_source` object dict
5. `store_object(item_source_dict)` → `object_hash`
6. Return `ItemRef(blob_hash, object_hash, integrity, signature_info)`

`ingest_directory` walks the `.ai/` tree and calls `ingest_item` for each file, building a mapping of `{relative_path: object_hash}`. It skips `.ai/objects/` (the store itself) and `.ai/agent/` (runtime state like threads and transcripts).

### Materialization

`materialize_item` reverses the ingest: given an `object_hash`, it reads the `item_source` object, extracts the `content_blob_hash`, reads the blob, and writes it to `target_path`.

## Object Model

**Location:** `ryeos/rye/cas/objects.py`

All objects are JSON dicts with a `schema` version (currently `1`) and a `kind` field. All hashing uses `compute_integrity()` — canonical JSON serialization (sorted keys, compact separators) → SHA256.

### Object Kinds

| Kind                     | Key Fields                                                                                                                     | Purpose                                                                    |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------- |
| `item_source`            | `item_type, item_id, content_blob_hash, integrity, signature_info`                                                             | Versioned snapshot of a tool/directive/knowledge file                      |
| `source_manifest`        | `space, items: {path: item_source_hash}, files: {path: blob_hash}`                                                             | Filesystem closure — everything needed to materialize a space              |
| `config_snapshot`        | `resolved_config: {agent.yaml: {...}, ...}`                                                                                    | Merged config state after 3-tier resolution                                |
| `node_input`             | `graph_hash, node_name, interpolated_action, lockfile_hash, config_snapshot_hash`                                              | Cache key for node execution — must be deterministic                       |
| `node_result`            | `result: {...}`                                                                                                                | Cached execution output                                                    |
| `node_receipt`           | `node_input_hash, node_result_hash, cache_hit, elapsed_ms, timestamp`                                                          | Audit record for a single node execution                                   |
| `execution_snapshot`     | `graph_run_id, graph_id, project_manifest_hash, user_manifest_hash, system_version, step, status, state_hash, node_receipts[]` | Immutable run checkpoint                                                   |
| `state_snapshot`         | `state: {...}`                                                                                                                 | Graph state at a point in time                                             |
| `artifact_index`         | `thread_id, entries: {call_id: {blob_hash, ...}}`                                                                              | Per-thread artifact mapping                                                |
| `project_snapshot`       | `project_manifest_hash, user_manifest_hash, parent_hashes[], source, source_detail, timestamp, metadata`                       | Point-in-time project state commit with parent lineage (like a git commit) |
| `runtime_outputs_bundle` | `remote_thread_id, execution_snapshot_hash, files: {path: blob_hash}`                                                          | Maps runtime-produced files to CAS blobs for remote output sync            |

### Example: item_source

```json
{
  "schema": 1,
  "kind": "item_source",
  "item_type": "tool",
  "item_id": "rye/bash/bash",
  "content_blob_hash": "a1b2c3d4e5f6...",
  "integrity": "f6e5d4c3b2a1...",
  "signature_info": {
    "timestamp": "2026-02-14T00:27:54Z",
    "hash": "a1b2c3d4e5f6...",
    "ed25519_sig": "WOclUqjrz1dhuk6C...",
    "pubkey_fp": "440443d0858f0199"
  }
}
```

### Example: execution_snapshot

```json
{
  "schema": 1,
  "kind": "execution_snapshot",
  "graph_run_id": "run_abc123",
  "graph_id": "my/pipeline",
  "project_manifest_hash": "...",
  "user_manifest_hash": "...",
  "system_version": "0.7.0",
  "step": 3,
  "status": "completed",
  "state_hash": "...",
  "node_receipts": ["receipt_hash_1", "receipt_hash_2", "receipt_hash_3"]
}
```

### Example: project_snapshot

```json
{
  "schema": 1,
  "kind": "project_snapshot",
  "project_manifest_hash": "a1b2c3...",
  "user_manifest_hash": "d4e5f6...",
  "parent_hashes": ["prev_head_hash"],
  "source": "push",
  "source_detail": "",
  "timestamp": "2026-03-10T12:00:00Z",
  "metadata": {}
}
```

Parent conventions: `[0]` = previous HEAD (mainline), `[1]` = merged branch (for merge commits). Zero parents = initial push. `get_history()` follows `parent_hashes[0]` for mainline traversal.

## Refs (Mutable Pointers)

```
.ai/objects/refs/
  graphs/<graph_run_id>.json    → latest execution_snapshot hash
```

Only refs are mutable. Everything they point to is immutable. Refs are stored as simple JSON:

```json
{ "hash": "a1b2c3d4e5f6..." }
```

`write_ref` uses atomic writes (tmp + rename) to prevent partial pointer updates. `read_ref` returns `None` if the ref doesn't exist.

Refs act as the "latest" cursor for graph execution. After each step, the runner writes an `execution_snapshot` object to the store, then updates the ref to point to it. Resumption reads the ref → loads the snapshot → continues from where it left off.

## Integrity Guarantees

- **Blobs:** SHA256 of raw bytes
- **Objects:** `compute_integrity()` — canonical JSON (sorted keys, compact separators) → SHA256
- **Atomic writes:** All writes go through tmp file + `os.rename()`. No partial writes visible to readers.
- **Content-addressed = idempotent = write-once:** Storing the same content twice is a no-op. The hash is the identity.

The CAS integrity model composes with Rye's signing system. `item_source` objects record the original file's `integrity` and `signature_info`, so signature verification can be performed against materialized files without re-reading the original.

## Garbage Collection

CAS objects accumulate indefinitely. The GC system prunes derived caches, compacts ProjectSnapshot DAG history, and mark-and-sweeps unreachable objects. See [Garbage Collection](./garbage-collection.md) for the full architecture, pipeline, configuration, and server integration.

## Implementation Files

| Component            | File                                           |
| -------------------- | ---------------------------------------------- |
| CAS primitives       | `lillux/kernel/lillux/primitives/cas.py`       |
| Rye CAS store        | `ryeos/rye/cas/store.py`                       |
| Object model         | `ryeos/rye/cas/objects.py`                     |
| GC engine            | `ryeos/rye/cas/gc.py`                          |
| Integrity primitives | `lillux/kernel/lillux/primitives/integrity.py` |
