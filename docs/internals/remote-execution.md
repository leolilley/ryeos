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

The remote server is a FastAPI app at `services/ryeos-remote/` deployed on Modal. The engine serves REST, MCP (`/mcp`), and webhooks from a single Modal deployment. The separate Railway proxy service (`ryeos-remote-mcp`) has been removed. The server also mounts a FastMCP server at `/mcp` with 3 tools (fetch, execute, sign) that call the engine directly — no proxy. The client is a bundled tool at `.ai/tools/rye/core/remote/remote.py`.

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

5b. POST /push ────────────────────────→  Deep manifest graph validation
    {project_path, manifest hashes,        Create ProjectSnapshot
     system_version}                       Advance HEAD (optimistic CAS)

6. POST /execute ─────────────────────────→  Resolve HEAD snapshot
   {project_path, item_type,                 Create mutable checkout
    item_id, params}                          from snapshot cache
                                              Run executor
                                              Fold-back via three-way merge
                     ←─────────────────────  Return snapshot + new hashes

7. POST /objects/get ─────────────────────→  Fetch by hash
   (new execution results)
                     ←─────────────────────  Return objects

8. Render human-readable views locally
   (knowledge markdown, transcripts)
   Optional: re-sign for user provenance
```

Note: `/push` performs deep manifest graph validation via `_validate_manifest_graph()` — verifying the full transitive object graph before creating the snapshot.

Steps 1–3 happen locally before any network call. Steps 4–5 are the sync phase — only missing objects cross the wire. Step 5b creates a `ProjectSnapshot` with parent lineage and advances HEAD. Step 6 resolves the HEAD snapshot, creates a mutable checkout, runs the executor, and folds back changes via three-way merge. Steps 7–8 pull results back and render them.

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
| `/push` | POST | Yes | Validate manifest graph, create ProjectSnapshot, advance HEAD |
| `/push/user-space` | POST | Yes | Push user space independently (optimistic CAS) |
| `/user-space` | GET | Yes | Get current user space ref |
| `/execute` | POST | Yes (dual) | Execute via snapshot checkout (bearer or webhook HMAC) |
| `/fetch` | POST | Yes | Fetch/search/inspect items (wraps execute with rye/fetch) |
| `/sign` | POST | Yes | Sign items (wraps execute with rye/sign) |
| `/threads` | GET | Yes | List user's executions (optional `project_path` filter) |
| `/threads/{thread_id}` | GET | Yes | Get specific thread status |
| `/history` | GET | Yes | Walk first-parent snapshot chain from project HEAD |
| `/secrets` | POST | Yes | Upsert user secrets |
| `/secrets` | GET | Yes | List secret names |
| `/secrets/{name}` | DELETE | Yes | Delete a secret |
| `/webhook-bindings` | POST | Yes | Create webhook binding (returns hook_id + hmac_secret) |
| `/webhook-bindings` | GET | Yes | List user's webhook bindings |
| `/webhook-bindings/{hook_id}` | DELETE | Yes | Revoke a webhook binding |

### `/execute` Flow (Snapshot-Based)

`/execute` uses dual-auth via `resolve_execution()`:

- **Bearer API key** → caller controls `item_type`, `item_id`, `project_path`, `parameters`
- **Webhook HMAC** → binding controls what executes, caller provides only `parameters`

Execution via `_execute_from_head()`:

1. **Resolve HEAD** — look up `project_refs` for latest `snapshot_hash` + `snapshot_revision`.
2. **Resolve user space** — look up `user_space_refs` independently (handles `None` gracefully).
3. **Create execution space** — `create_execution_space()` produces a mutable checkout from the snapshot cache.
4. **Cache user space** — `ensure_user_space_cached()` for user-level items.
5. **Inject secrets** — fetch from vault, inject as env vars. Validates names against `RESERVED_ENV_NAMES` and `RESERVED_ENV_PREFIXES` (blocks PATH, PYTHONPATH, SUPABASE_*, MODAL_*, AWS_*, etc.).
6. **Run executor** — `ExecuteTool` against the materialized checkout.
7. **Promote CAS objects** — `_copy_cas_objects()` re-ingests execution-local CAS into user CAS (integrity-verified).
8. **Ingest runtime outputs** — transcripts, knowledge, refs stored as `RuntimeOutputsBundle`.
9. **Build post-execution manifest** — compare to base manifest.
10. **No-op check** — if manifest unchanged, skip fold-back.
11. **Create execution ProjectSnapshot** — parent = base snapshot.
12. **Fold-back** — merge into HEAD via `_fold_back()`.
13. **Cleanup** — remove execution space, restore env vars.

### Fold-Back / Three-Way Merge

After execution, changes must be merged back into HEAD. `_fold_back()` implements a bounded retry loop:

- **Fast-forward** — HEAD hasn't moved since checkout → advance HEAD directly via optimistic CAS on `snapshot_revision`.
- **Three-way merge** — HEAD moved → `three_way_merge(base, head, exec, cas_root)` resolves changes:
  - Both sides agree → take the change
  - One side changed → take that change
  - Both deleted → accept
  - Delete vs modify → conflict
  - Both modified differently → attempt text merge (UTF-8 only, <1MB)
- **Conflict** — unresolvable conflicts stored as `conflict_record` on the thread row.
- **Retry exhaustion** — `MAX_FOLD_BACK_RETRIES=5` with exponential jitter (`FOLD_BACK_BASE_JITTER_MS=50`).

Merge commits create a `ProjectSnapshot` with two parents: `[0]` = current HEAD, `[1]` = execution snapshot.

## Trust Model

Remote executor has its own Ed25519 keypair. It never receives the user's private key.

- Remote generates keypair on first boot (same `ensure_keypair()` pattern as local)
- Public key exposed via `/public-key` endpoint
- Client pins remote key via TOFU (Trust On First Use) into user trust store. TOFU pins are keyed by `remote:{name}:{url_host}` — this prevents two remotes with the same name but different URLs from sharing a pin.
- Key verification happens before push AND pull operations — hard-fails on fingerprint mismatch or fetch failure
- Remote signs execution artifacts with its own key
- User can optionally re-sign pulled results for "this is mine" provenance

The trust chain: you trust the remote by pinning its key. The remote proves identity by signing results. If you want your own provenance on those results, re-sign them locally.

## Authentication

Bearer token auth via `services/ryeos-remote/ryeos_remote/auth.py`. Two methods:

**API key:** SHA256-hashed keys looked up in Supabase `api_keys` table. Checks revocation and expiry.

**JWT:** Supabase JWT decoded with HS256, audience `"authenticated"`. Extracts `user_id` from `sub` claim.

Both methods resolve to a `user_id` that scopes all CAS operations.

### Webhook Authentication

Webhook auth uses HMAC-SHA256 verification via `webhook_bindings`:

1. **Binding lookup** — `hook_id` from request body → look up in `webhook_bindings` table (must be active, not revoked).
2. **Timestamp verification** — `X-Webhook-Timestamp` header must be within 5 minutes (not stale) and not >30s in the future.
3. **HMAC verification** — `X-Webhook-Signature` header (`sha256=<hex>`) verified against `HMAC-SHA256(timestamp.raw_body, hmac_secret)`.
4. **Replay protection** — `X-Webhook-Delivery-Id` checked against `webhook_deliveries_replay` table (persistent, DB-backed). Unique constraint handles concurrent races.

Webhook callers can only provide `parameters` — the binding controls `item_type`, `item_id`, and `project_path`.

## Quotas and Limits

| Limit | Default | Enforced At |
|-------|---------|-------------|
| `max_request_bytes` | 50 MB | Request middleware (Content-Length header + stream-based) |
| `max_user_storage_bytes` | 1 GB | Before `put_objects` and after execution |

Per-user CAS isolation: each user's objects live at `{cas_base_path}/{user_id}/.ai/objects/`. One user cannot read or write another user's objects.

## Secrets

Secrets are injected as environment variables per-request. They never appear in CAS — not in manifests, not in objects, not in blobs. They exist only in memory during execution.

Managed via the remote tool's secret actions: `secrets_set`, `secrets_import`, `secrets_list`, `secrets_delete`. Stored server-side, scoped to the authenticated user.

Secret names are validated before injection via `_is_safe_secret_name()`:

- Must be a valid Python identifier
- Blocked: `RESERVED_ENV_NAMES` (PATH, HOME, PYTHONPATH, TMPDIR, RYE_SIGNING_KEY_DIR, RYE_KERNEL_PYTHON, RYE_REMOTE_NAME, etc.)
- Blocked: `RESERVED_ENV_PREFIXES` (SUPABASE_, MODAL_, LD_, SSL_, AWS_, GOOGLE_, AZURE_, GITHUB_, CI_, DOCKER_)
- Unsafe names are logged and skipped (not rejected — other secrets still inject)

## Client Tool

Bundled at `.ai/tools/rye/core/remote/remote.py`.

| Action | Description |
|--------|-------------|
| `push` | Build manifests, sync missing objects, publish project ref on remote |
| `pull` | Fetch new objects from remote (execution results) |
| `execute` | Push → trigger remote execution → pull results (end-to-end) |
| `status` | Show local manifest hashes, system version, and configured remotes |
| `threads` | List remote executions from the server |
| `thread_status` | Get status of a specific remote thread by thread_id |

Remotes are configured as named entries in `cas/remote.yaml` (under `.ai/config/`). Use `resolve_remote(name, project_path)` to resolve a named remote to its URL and API key. The default remote name is `"default"`. All remotes must be declared in config.

## Named Remotes

Remotes are configured in `cas/remote.yaml` under `.ai/config/`:

```yaml
remotes:
  default:
    url: "https://ryeos-remote--execute.modal.run"
    key_env: "RYE_REMOTE_API_KEY"

  gpu:
    url: "https://gpu-worker--execute.modal.run"
    key_env: "GPU_REMOTE_API_KEY"
```

Each entry specifies a `url` and `key_env` (the environment variable holding the API key). `resolve_remote(name, project_path)` reads the config, resolves the API key from the environment, and returns a `RemoteConfig` dataclass.

The `thread` parameter on `rye_execute` supports `"remote:name"` syntax to target a specific remote:

```python
rye_execute(
    item_type="tool",
    item_id="my/heavy-compute",
    project_path="/home/user/project",
    thread="remote:gpu"
)
```

The walker also supports per-node remote dispatch — individual graph nodes can specify `"remote": "gpu"` to run on different servers.

## Project Refs

The `/push` endpoint registers a project ref and creates a `ProjectSnapshot` with parent lineage. The ref tracks the latest state for a user × remote × project combination.

Project refs are stored in the `project_refs` Supabase table:
- Primary key: `(user_id, remote_name, project_path)`
- Columns: `project_manifest_hash`, `system_version`, `pushed_at`, `snapshot_hash`, `snapshot_revision` (bigint, optimistic CAS counter), `head_updated_at`

The `/push` endpoint performs deep manifest graph validation via `_validate_manifest_graph()` — verifying the full transitive object graph (manifest kind/schema/space, all item references point to valid `item_source` objects with existing content blobs, all file references point to existing blobs).

### User Space Refs

User space is managed independently from project refs via `user_space_refs`:
- Primary key: `(user_id, remote_name)`
- Columns: `user_manifest_hash`, `snapshot_revision`, `pushed_at`
- `/push/user-space` — optimistic CAS update (409 on revision conflict)
- `/user-space` — get current user space ref

## Snapshot History

The `/history` endpoint walks the first-parent chain from a project's HEAD snapshot:

```
GET /history?project_path=my-project&limit=50
```

Returns a list of `ProjectSnapshot` objects following `parent_hashes[0]` (mainline). Each entry includes its hash as `_hash`. Useful for auditing push and execution history.

## Implementation Files

| Component | File |
|-----------|------|
| Server | `services/ryeos-remote/ryeos_remote/server.py` |
| Server config | `services/ryeos-remote/ryeos_remote/config.py` |
| Server auth | `services/ryeos-remote/ryeos_remote/auth.py` |
| Sync protocol | `ryeos/rye/cas/sync.py` |
| Materializer | `ryeos/rye/cas/materializer.py` |
| Client tool | `.ai/tools/rye/core/remote/remote.py` |
| Remote config | `ryeos/bundles/core/ryeos_core/.ai/tools/rye/core/remote/remote_config.py` |
| Config schema | `ryeos/bundles/core/ryeos_core/.ai/tools/rye/core/remote.config-schema.yaml` |
| CAS primitives | `lillux/kernel/lillux/primitives/cas.py` |
| Checkout/spaces | `ryeos/rye/cas/checkout.py` |
| Three-way merge | `ryeos/rye/cas/merge.py` |
| MCP transport | `services/ryeos-remote/ryeos_remote/mcp.py` |
| Object model | `ryeos/rye/cas/objects.py` |
