<!-- rye:signed:2026-03-10T04:07:14Z:927a35c17acd7d5d544cc62e4add3826394602c98072c57b16585225d413d248:dQXRLiRXzDWgRMx_aXBNfv0yrgHyGb4Qc-w-pqwgNKkeBiXDR4NbS8XuEs91ZKb9fyujgDlvsHYjgHZbJJiGBg==:4b987fd4e40303ac -->
```yaml
name: object-kinds
title: CAS Object Kinds
entry_type: reference
category: rye/core/cas
version: "1.0.0"
author: rye-os
created_at: 2026-03-10T00:00:00Z
tags:
  - cas
  - objects
  - storage
  - data-model
```

# CAS Object Kinds

The Content-Addressable Store (CAS) uses frozen dataclasses to represent
immutable objects. Every object includes `schema` (version) and `kind`
(discriminator) for evolution and routing.

Defined in `ryeos/rye/cas/objects.py`.

## Reference Objects

### ItemRef
Return type from `ingest_item()`. Not stored in CAS — it holds references:
- `blob_hash` — raw file content in CAS blobs
- `object_hash` — the `ItemSource` object hash
- `integrity` — SHA-256 of raw bytes
- `signature_info` — signing metadata (None for unsigned files)

## Stored Object Kinds

### 1. ItemSource (`item_source`)
Versioned snapshot of a signed or unsigned `.ai/` file.
- `item_type` — directive, tool, or knowledge
- `item_id` — filename stem
- `content_blob_hash` — pointer to raw content blob
- `integrity` — SHA-256 of content
- `signature_info` — signing metadata or None

### 2. SourceManifest (`source_manifest`)
Filesystem closure — everything needed to materialize a space.
- `space` — "project" or "user"
- `items` — `{relative_path: item_source_hash}` for `.ai/` files
- `files` — `{relative_path: blob_hash}` for non-`.ai/` project files

### 3. ConfigSnapshot (`config_snapshot`)
Merged config state after 3-tier resolution.
- `resolved_config` — the fully merged config dict

### 4. NodeInput (`node_input`)
Cache key for node execution — must be deterministic.
- `graph_hash`, `node_name` — identify the node
- `interpolated_action` — resolved action after template substitution
- `lockfile_hash` — optional lockfile reference
- `config_snapshot_hash` — config at time of execution

### 5. NodeResult (`node_result`)
Cached execution output.
- `result` — full unwrapped result dict

### 6. NodeReceipt (`node_receipt`)
Audit record for a single node execution.
- `node_input_hash`, `node_result_hash` — links input to output
- `cache_hit` — whether result came from cache
- `elapsed_ms` — execution time
- `timestamp` — ISO 8601

### 7. ExecutionSnapshot (`execution_snapshot`)
Immutable run checkpoint — captures full execution state.
- `graph_run_id`, `graph_id` — run identity
- `project_manifest_hash`, `user_manifest_hash` — source state
- `system_version` — installed ryeos version
- `step`, `status` — progress tracking
- `state_hash` — pointer to StateSnapshot
- `node_receipts` — list of NodeReceipt hashes

### 8. StateSnapshot (`state_snapshot`)
Graph state at a point in time.
- `state` — arbitrary state dict

### 9. ArtifactIndex (`artifact_index`)
Per-thread mapping for artifact retrieval.
- `thread_id` — owning thread
- `entries` — `{call_id: {name: blob_hash}}` mapping

## Schema Evolution

All objects carry a `schema` version field (currently `1`). The engine
checks this on deserialization. Future versions increment the schema and
handle migration in `to_dict()` / factory methods.

## Storage

Objects are stored as canonical JSON (sorted keys, no whitespace) via
`cas.store_object()`. The hash is SHA-256 of the canonical form.
Blobs are stored as raw bytes via `cas.store_blob()`.
Both use 2-level sharding: `{root}/{type}/{hash[:2]}/{hash[2:4]}/{hash}`.
