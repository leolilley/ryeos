```yaml
id: remote-execution
title: "Remote Execution"
description: CAS-native remote execution — sync protocol, materializer, server endpoints, and trust model
category: internals
tags: [remote, execution, sync, cas, materializer, trust]
version: "1.0.0"
```

# Remote Execution

Remote execution lets you run tools and state graphs on a remote server without exposing your private signing key. The system uses content-addressed storage (CAS) for sync — no git, no file diffs. Objects are synced by hash, execution happens in a temp-materialized `.ai/` directory, and results flow back as immutable CAS objects.

The remote server is a FastAPI app at `services/ryeos-remote/` deployed on Modal. The client is a bundled tool at `.ai/tools/rye/core/remote/remote.py`.

## End-to-End Flow

```
LOCAL                                          REMOTE
─────                                          ──────

1. Build project manifest
   (walk .ai/, ingest items to CAS)

2. Build user manifest
   (config, trusted keys, public key)

3. Compute transitive object set
   from both manifests

4. POST /objects/has ──────────────────────→  Check user CAS
                     ←─────────────────────  Return missing[]

5. POST /objects/put ──────────────────────→  Store in user CAS
   (only missing objects)                     Verify hashes

6. POST /execute ─────────────────────────→  Auth + load secrets
   {project_manifest, user_manifest,          Materialize temp .ai/
    item_type, item_id, params}               Run executor/walker
                                              Per-node cache check
                                              Store results in CAS
                                              Sign with remote key
                     ←─────────────────────  Return snapshot + new hashes

7. POST /objects/get ─────────────────────→  Fetch by hash
   (new execution results)
                     ←─────────────────────  Return objects

8. Render human-readable views locally
   (knowledge markdown, transcripts)
   Optional: re-sign for user provenance
```

Steps 1–3 happen locally before any network call. Steps 4–5 are the sync phase — only missing objects cross the wire. Step 6 is the actual execution. Steps 7–8 pull results back and render them.

## Sync Protocol

Three operations. Hash set reconciliation. No git.

Source: `ryeos/rye/cas/sync.py`

| Operation | Request | Response |
|-----------|---------|----------|
| `has_objects` | `{hashes: string[]}` | `{present: string[], missing: string[]}` |
| `put_objects` | `{entries: [{hash, kind, data}]}` | `{stored: string[]}` |
| `get_objects` | `{hashes: string[]}` | `{entries: [{hash, kind, data}]}` |

- `kind` is `"blob"` or `"object"`
- `data` is base64-encoded
- Server verifies claimed hash matches recomputed hash on `put_objects` — rejects on mismatch
- Gzip compression for responses

### Client Helpers

`collect_object_hashes(manifest, root)` — walks items (`item_source` → `content_blob_hash`) and files (blob hashes), returns a deduplicated list of all hashes transitively referenced by the manifest.

`export_objects(hashes, root)` — reads objects from local CAS, returns base64-encoded entries ready for `put_objects`.

`import_objects(entries, root)` — imports entries into local CAS. **Raises on integrity errors** — if the claimed hash doesn't match the recomputed hash, the object is rejected and the import fails.

## Materializer

Compatibility bridge. Reconstitutes a `.ai/` filesystem from CAS so existing executor runs unmodified. System space = installed `ryeos` package (unchanged, NOT materialized).

Source: `ryeos/rye/cas/materializer.py`

### `ExecutionPaths` Dataclass

```python
@dataclass
class ExecutionPaths:
    project_path: Path    # /tmp/rye-exec-<id>/project/
    user_space: Path      # /tmp/rye-exec-<id>/user/
    cas_root: Path        # shared CAS (not copied)
```

### Functions

`materialize(project_manifest_hash, user_manifest_hash, cas_root, tmp_base) -> ExecutionPaths` — reads source manifest objects from CAS, iterates items (`item_source` → blob) and files (raw blob → write directly), writes them to temp directories that mirror the `.ai/` layout.

`cleanup(paths)` — removes temp dirs.

`_safe_target(root, rel_path)` — validates relative path, rejects absolute paths and `..` escapes. This is the path traversal protection — a manifest with `../../etc/passwd` as a relative path will be caught here.

## Server Endpoints

FastAPI app at `services/ryeos-remote/ryeos_remote/server.py`.

| Endpoint | Method | Auth | Description |
|----------|--------|------|-------------|
| `/health` | GET | No | Health check, returns `{status, version}` |
| `/public-key` | GET | No | Remote executor's Ed25519 public key PEM |
| `/objects/has` | POST | Yes | Batch existence check in user's CAS |
| `/objects/put` | POST | Yes | Upload objects to user's CAS (quota-checked) |
| `/objects/get` | POST | Yes | Download objects from user's CAS |
| `/execute` | POST | Yes | Execute from manifest hashes |

### `/execute` Flow

1. **Version check** — verify system version compatibility (major.minor match required). Returns 409 on mismatch so the client knows to upgrade.
2. **Materialize** — build temp project + user space from manifest hashes via `materialize()`.
3. **Wire executor** — create `ExecuteTool` against materialized paths.
4. **Run** — execute the tool or walk the state graph.
5. **Ingest results** — re-ingest execution outputs into user CAS via `_copy_cas_objects()`. This is integrity-verified (recomputes hashes), not a raw file copy.
6. **Quota check** — post-execute quota enforcement.
7. **Return** — `{status, execution_snapshot_hash, new_object_hashes[], result, system_version}`.
8. **Cleanup** — remove temp dirs. Always runs, even on error.

## Trust Model

Remote executor has its own Ed25519 keypair. It never receives the user's private key.

- Remote generates keypair on first boot (same `ensure_keypair()` pattern as local)
- Public key exposed via `/public-key` endpoint
- Client pins remote key via TOFU (Trust On First Use) into user trust store
- Key verification happens before push — hard-fails on fingerprint mismatch or fetch failure
- Remote signs execution artifacts with its own key
- User can optionally re-sign pulled results for "this is mine" provenance

The trust chain: you trust the remote by pinning its key. The remote proves identity by signing results. If you want your own provenance on those results, re-sign them locally.

## Authentication

Bearer token auth via `services/ryeos-remote/ryeos_remote/auth.py`. Two methods:

**API key:** SHA256-hashed keys looked up in Supabase `api_keys` table. Checks revocation and expiry.

**JWT:** Supabase JWT decoded with HS256, audience `"authenticated"`. Extracts `user_id` from `sub` claim.

Both methods resolve to a `user_id` that scopes all CAS operations.

## Quotas and Limits

| Limit | Default | Enforced At |
|-------|---------|-------------|
| `max_request_bytes` | 50 MB | Request middleware (Content-Length header + stream-based) |
| `max_user_storage_bytes` | 1 GB | Before `put_objects` and after execution |

Per-user CAS isolation: each user's objects live at `{cas_base_path}/{user_id}/.ai/objects/`. One user cannot read or write another user's objects.

## Secrets

Secrets are injected as environment variables per-request. They never appear in CAS — not in manifests, not in objects, not in blobs. They exist only in memory during execution.

Managed via the remote tool's secret actions: `secrets_set`, `secrets_import`, `secrets_list`, `secrets_delete`. Stored server-side, scoped to the authenticated user.

## Client Tool

Bundled at `.ai/tools/rye/core/remote/remote.py`.

| Action | Description |
|--------|-------------|
| `init` | Initialize remote CAS, create remote signing key, return public key |
| `push` | Build manifests, sync missing objects |
| `pull` | Fetch new objects (execution results, updated state) |
| `execute` | Push → trigger remote execution → pull results (end-to-end) |
| `status` | Show local vs remote manifest hashes |
| `pin_remote_key` | Pin remote executor's public key (TOFU) |
| `secrets_set` | Store a user secret |
| `secrets_import` | Bulk import from `.env` file |
| `secrets_list` | List secret names (not values) |
| `secrets_delete` | Delete a secret |

`REMOTE_URL` is read from the `RYE_REMOTE_URL` environment variable. No default — must be set explicitly.

## Implementation Files

| Component | File |
|-----------|------|
| Server | `services/ryeos-remote/ryeos_remote/server.py` |
| Server config | `services/ryeos-remote/ryeos_remote/config.py` |
| Server auth | `services/ryeos-remote/ryeos_remote/auth.py` |
| Sync protocol | `ryeos/rye/cas/sync.py` |
| Materializer | `ryeos/rye/cas/materializer.py` |
| Client tool | `.ai/tools/rye/core/remote/remote.py` |
| CAS primitives | `lillux/kernel/lillux/primitives/cas.py` |
