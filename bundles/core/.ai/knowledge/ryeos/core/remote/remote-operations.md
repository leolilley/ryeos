---
category: ryeos/core
tags: [remote, operations, trust, security, networking]
version: "3.5.0"
description: >
  Remote execution and bundle synchronization — trust model,
  operator workflows, fail-closed semantics, and security requirements.
  Covers the per-request engine overlay, user-space sync, hybrid binary
  resolution, symmetric pull-back, single-flight cache builds, and
  live system roots.
---

# Remote Operations

Remote operations allow one ryeos node to execute items, push/pull
CAS objects, and synchronize bundles with another node over HTTPS.

See also:

- [Remote Command Reference](remote-command-reference.md) for command syntax, examples, capabilities, scopes, routes, and failure modes.
- [Identity Model](../identity-model.md) for the four trust layers and request authentication flow.
- [Spaces](../spaces.md) for project/user/system resolution.
- [Execution Provenance](../execution/provenance.md) for local vs pushed-head execution source invariants.
- [HTTP API](../node/http-api.md) and [Services: remote](../services/remote.md) for route/service mappings.

## v1 Trust Boundary

v1 remote execution is for **operator-trusted remotes**, not mutually
untrusted tenants.

- CAS is shared/global within a node; capability checks protect access,
  not storage partitioning.
- Vault is a single shared store in v1; capability checks protect
  mutation/listing, not per-principal isolation.
- All remote requests are signed with the **caller's node Ed25519 key**
  (not the user/CLI key); the remote node verifies the signature against
  its authorized-keys trust store.
- Granting the local `remote.admin` capability is operator-level access
  to high-impact remote orchestrators in v1. It does not replace
  target-node authorized-key scopes.

## Identity Requirements

Remote outbound requests are signed with the **caller's node key**, not
the user CLI key. This means:

- The key that must be authorized on the remote is the caller's **node
  public key** (`<system>/.ai/node/identity/public-identity.json`).
- To display your node's public key for sharing with a remote operator:

  ```bash
  ryeos identity public-key
  ```

- The remote operator authorizes the caller's **node key fingerprint**,
  not the user key fingerprint.

## Prerequisites

### On the remote node

1. **Bootstrap**: run `ryeos init` so the remote has node identity,
   vault key material, local operator authorization, and installed
   bundles.

2. **Authorize the caller node key**: the remote operator needs the
   caller node public key in `ed25519:<base64>` form. The caller gets it
   with:

   ```bash
   ryeos identity public-key
   ```

   The remote operator can then grant explicit scopes locally:

   ```bash
   ryeos authorize-key \
     --public-key "ed25519:<caller_node_pubkey_b64>" \
     --label "dev-machine" \
     --scopes "ryeos.execute.service.objects.has,ryeos.execute.service.objects.put,ryeos.execute.service.objects.get,ryeos.execute.service.push.head"
   ```

3. **Choose scopes by operation**. Do not mix local remote-service caps
   with remote authorized-key scopes. The complete matrix is in
   [Remote Command Reference](remote-command-reference.md#authority-matrix).

   Common remote-side scopes:

   | Operation | Remote scopes on target |
   |-----------|-------------------------|
   | `remote push` | `ryeos.execute.service.objects.has`, `ryeos.execute.service.objects.put`, `ryeos.execute.service.push.head` |
   | `remote pull` | `ryeos.execute.service.objects.get` |
   | `remote execute` | push scopes + `ryeos.execute.service.objects.get` + whatever caps the executed item requires |
   | `remote authorize` | `ryeos.execute.service.authorize.key` |
   | `remote bundle-install` | `ryeos.execute.service.bundle.export`, `ryeos.execute.service.objects.get` |
   | `remote vault-set/list/delete` | matching `ryeos.execute.service.vault.*` cap |
   | `remote threads/thread-status` | signed auth; no additional thread service cap in v1 |

   Short-form scopes like `bundle.install` will never authorize a
   handler — the matcher does not auto-prefix and the daemon's auth
   loader refuses to load TOMLs that contain them.

### On the caller node

1. **Configure the remote**:

   ```bash
   ryeos remote configure --remote production --url https://ryeos.example.com
   ```

   This discovers the remote's public key, principal id, vault
   fingerprint, and ingest-ignore rules.

## End-to-End Workflow

```bash
# ── On the CALLER node ──

# 1. Display your node public key (share this with the remote operator)
ryeos identity public-key

# 2. Configure the remote
ryeos remote configure --remote prod --url https://ryeos.example.com

# ── On the REMOTE node ──

# 3. Authorize the caller's node key (use the output from step 1)
ryeos authorize-key \
  --public-key "ed25519:<caller_node_pubkey_b64>" \
  --label "dev-machine" \
  --scopes "ryeos.execute.service.objects.has,ryeos.execute.service.objects.put,ryeos.execute.service.objects.get,ryeos.execute.service.push.head"

# ── Back on the CALLER node ──

# 4. Execute on the remote
ryeos remote execute --remote prod --item-ref tool:my/heavy-compute
```

The synchronous `remote execute` command:

1. **Push** — ingests the local project, uploads missing CAS objects
   to the remote, advances the remote's HEAD ref.
2. **Execute** — runs the specified item on the remote using the
   pushed content.
3. **Pull** — fetches the resulting snapshot, diffs against the
   pre-push manifest, and applies changes to the local workspace.

### Clean-Base Guarantee

If the pull detects that any local tracked file has changed since the
push, the entire apply is **aborted** — no partial writes. The local
HEAD ref is rolled back to its pre-push state.

## No Wildcard Delegation

Bootstrap may create `["*"]` keys locally for the node operator.
Remote authorization (`ryeos authorize-key`) rejects any request that
includes `"*"` in the scopes list. All remote keys must enumerate
their capabilities explicitly.

## Node-Key Rotation

When a node's signing key is compromised or needs rotation:

1. Rotate the node signing key (`ryeosd --init-only --force`).
2. Reissue the bootstrap local operator key.
3. Reissue every remotely granted authorized key.
4. All existing authorized-key TOMLs become invalid immediately.

This is an intentional break-glass: node-key rotation invalidates all
delegated authority.

### Key rotation on the caller side

1. Rotate the node key on the caller.
2. On the remote: `ryeos authorize-key` with the new node public key.
3. On the caller: `ryeos remote configure` to pick up changes.
4. On the remote: remove the old authorized-key TOML from
   `.ai/node/auth/authorized_keys/`.

## Remote Ignore Cache

`ryeos remote configure` caches the remote node's ingest-ignore rules
in the local remotes config. Subsequent push/execute operations use
these cached rules to build the push manifest.

If the remote changes its ignore rules, re-run `ryeos remote configure`.

If no cached rules are available, the push handler fetches them inline
and persists them. If the inline fetch fails, the push is **aborted** —
the handler does not silently fall back to local ignore rules.

## HTTPS Requirement

All remote communication uses Ed25519 request signing for authentication
and integrity. However, vault `set` operations send plaintext secret
values under the signing layer — they are not end-to-end encrypted by
the vault protocol itself.

**Production deployments must terminate TLS in front of `ryeosd`.**
HTTP without TLS is only acceptable for loopback addresses.

## Async Limitations

v1 is **synchronous only**. Remote execute blocks until completion and
results are pulled back. Asynchronous execution is deferred to a future
release.

## Bundle Synchronization

### Remote Pull

Fetch CAS objects by hash from a remote node, store in local CAS,
optionally materialize to a directory.

```bash
ryeos remote pull --remote production --hashes abc123 def456
ryeos remote pull --remote production --hashes abc123 --output-dir /tmp/objects
```

**Fail-closed**: aborts if any requested hash is missing. No partial fetches.
Local capability and remote target scope: `ryeos.execute.service.objects.get`

### Remote Bundle Install

Install a complete bundle from a remote node via CAS pipeline:

```bash
ryeos remote bundle-install --remote production --bundle-name standard
```

**Fail-closed**:
- Missing blobs → abort before materialization.
- Preflight failure → clean up partial directory.
- No registration written unless preflight passes.

Local capability: `ryeos.execute.service.bundle.install`.
Remote scopes on the target: `ryeos.execute.service.bundle.export`,
`ryeos.execute.service.objects.get`.

### Bundle Export (server-side)

Walk an installed bundle's tree, ingest files into CAS, return manifest.
Called automatically by `remote bundle-install` — not invoked directly.
Remote target scope: `ryeos.execute.service.bundle.export`

## Additional Remote Commands

```bash
ryeos remote push --remote production
ryeos remote vault-set --remote production --name API_KEY --value "sk-..."
ryeos remote vault-list --remote production
ryeos remote vault-delete --remote production --name API_KEY
ryeos remote threads --remote production
ryeos remote thread-status --remote production --thread-id abc123
```

## Using `--input` for Complex Parameters

When a handler requires arrays, nested objects, or typed parameters
that the CLI flag binder cannot express, use `--input` to pass a JSON
file (or `-` for stdin):

```bash
# From a file
ryeos execute service:identity/authorize-key --input params.json

# From stdin
echo '{"public_key":"ed25519:abc","label":"test","scopes":["cap1","cap2"]}' | \
  ryeos execute service:identity/authorize-key --input -
```

## Per-Request Engine Overlay

When `remote execute` pushes a snapshot, the remote daemon builds a
**per-request engine** that resolves items against the caller's pushed
content rather than the remote operator's own user space.

### How it works

1. The caller pushes a **project manifest** (project files) and
   optionally a **user manifest** (user-space items from `~/.ryeos/.ai/`).
2. The remote daemon checks the **engine cache** keyed by
   `(system_install_generation, snapshot_hash)`. On a miss, it:
   - Materialises the user overlay into a temp directory.
   - Loads any pushed trust pins into an in-memory overlay.
   - Reads the live signed bundle registry with
     `BootstrapLoader::load_bundle_section()`.
   - Builds a fresh `Engine` against registered bundle roots + the
     project checkout + the user overlay.
3. The project checkout is **per-request** (each request gets its own
   directory, cleaned up when the request completes). The user overlay
   temp dir is **shared** across concurrent requests on the same
   snapshot and lives as long as the cache entry.
4. All dispatch paths — top-level routes, scheduler, callbacks, and
   resume — construct a single type-state `ExecutionProvenance` value
   at the entry point. Its four variants encode the legal combinations
   of role and source: `RootLiveFs`, `RootPushedHead`,
   `BorrowedChildLiveFs`, and `BorrowedChildPushedHead`. Pushed roots
   carry the snapshot hash and a non-optional `Arc<TempDirGuard>`;
   borrowed children carry no snapshot hash, so they cannot own lineage.
   It flows through `DispatchRequest`, `ExecutionParams`,
   `BuildAndLaunchParams`, and `CallbackCapability` unchanged.
   Callback children clone via `clone_for_borrowed_child()` — they
   never reconstruct provenance from other fields, and there is no
   "infer from temp_dir/snapshot_hash" heuristic anywhere in the
   codebase. Provenance is required on every callback token; there is
   no `Option`, no fallback to the daemon engine, and no deploy-window
   migration path.
5. All materialised directories in `resolve_project_context` are
   wrapped in `Arc<TempDirGuard>` immediately. Failures anywhere in
   the build path drop those guards and clean up through the existing
   `Drop` implementation. Cache-owned guards survive as long as the
   cache entry; request-owned guards survive as long as the request's
   execution guard.

### Cache invalidation

- `bundle.install` and `bundle.remove` write persistent registry YAMLs
  at `<system_space_dir>/.ai/node/bundles/<name>.yaml` and bump the
  `system_install_generation` counter. Subsequent per-request engine
  rebuilds call `BootstrapLoader::load_bundle_section()` to read the
  live registry, so the new bundle set takes effect on the next
  `pushed_head` request without a daemon restart.
- Existing cached engines continue using the old bundle set until
  evicted by LRU or the idle threshold.
- The cache uses single-flight builds: concurrent first requests for
  the same snapshot serialise on one `EngineCache::get_or_insert_with`
  build. Late arrivals receive a clone of the first-built
  `Arc<Engine>`; there is no double materialisation and no same-key
  overwrite race.
- LRU eviction with a **strong-count guard**: entries whose engine
  `Arc` is still held by a running request are never evicted, even
  if that means temporarily exceeding capacity.
- Idle entries past the configured threshold (default: 30 minutes) are
  evicted on the next access, subject to the same strong-count guard.

### Isolation guarantee

A `pushed_head` request NEVER resolves against the remote operator's
user space. If the pushed snapshot has no user manifest, user-tier
resolution returns "not found" rather than falling through to the
operator's `~/.ryeos/.ai/`.

### Borrowed workspace for callbacks

Borrowed callback children carry a borrowed variant tag in their
provenance. The runner's lifecycle gates read this directly:
`provenance.is_borrowed_child()` returns true iff the execution is
borrowed. Borrowed children:

- skip `pin_localpath_snapshot` and `post_execution_foldback` (parent
  owns the snapshot lineage);
- do not track the workspace temp dir on their `ExecutionGuard` (the
  parent's `Arc<TempDirGuard>` pins it; the callback token's borrowed
  pushed provenance variant keeps it alive across parent crash or
  token-TTL grace period);
- have no snapshot hash field at all (the parent owns lineage; the
  child has nothing to pin).

Nested callback children (callback within a callback) preserve the
parent's provenance via `clone_for_borrowed_child`, so the grandchild's
variant remains `BorrowedChildPushedHead` and its lifeline Arc still
pins the original Root's temp dir. This closes the regression flagged
by oracle review on the previous heuristic-based implementation.

Resuming a `pushed_head` thread under the per-request overlay is
tracked separately; the resume path falls back to the daemon engine.
Detached children from callbacks are disallowed at the callback
dispatcher (inline only).

## User-Space Sync

### New user root: `~/.ryeos/`

The user-space root is `~/.ryeos/` (historically `~/.ai/`). All user-tier items
(directives, tools, knowledge, trust pins) live under
`~/.ryeos/.ai/`.

### Allow-list for user-space push

Only these subdirectories under `~/.ryeos/.ai/` are ingested during a
remote push:

| Directory | Contents |
|---|---|
| `directives/` | User-authored directive YAMLs |
| `tools/` | User-authored tool YAMLs + binaries |
| `knowledge/` | User knowledge entries |
| `config/` | User config items (including `config/keys/trusted/`) |

All other content under `~/.ryeos/` is ignored by the push pipeline.

### Symlink protection

User-space ingest **skips all symlinks** — a symlink at
`~/.ryeos/.ai/directives/exfil.md` pointing at `~/.ssh/id_rsa` will
NOT be followed, read, hashed, or uploaded. Project-space ingest
does not have this restriction (operators own their project data).

### `--no-project` mode

When the caller passes `--no-project`, only user-space content is
pushed. The snapshot's `project_manifest_hash` is set to a sentinel
empty manifest. User-space sync still happens normally.

### Trust-pin overlay semantics

Pushed trust pins (from the user manifest's `config/keys/trusted/`
section) are loaded into an in-memory overlay and unioned with the
remote's persistent trust store for **that request only**. They are
never written to the remote's persistent trust directory.

## Symmetric Pull-Back

After remote execution, the pull phase applies changes to **both** the
local project and the local user space:

- **Project pull-back**: diffs the pre-push manifest against the
  remote's result snapshot, applies updated/deleted files to the
  local project directory.
- **User pull-back**: diffs the pre-push user manifest against the
  result's user manifest, applies updated/deleted files to the
  local `~/.ryeos/.ai/`.

The JSON response from `remote execute` includes both:

```json
{
  "pull": {
    "files_updated": 3,
    "files_deleted": 1,
    "user_files_updated": 2,
    "user_files_deleted": 0
  }
}
```

On a fresh node where `~/.ryeos/.ai/` does not exist, the pull-back
creates it automatically.

## Hybrid Binary Resolution

When a pushed user-tier handler descriptor references a binary that is
not installed on the remote node, the handler is recorded as
`VerifiedHandler::Unresolved`. `build_engine_for_roots` logs a warning
and proceeds.

Boot-time subprocess validation for parser and composer handlers
downgrades `HandlerBinaryMissing` specifically to a warning. Other
validation errors — non-zero exit, spawn failure, protocol violation —
still produce a `BootIssue` and fail boot. Actual **invocation** of an
`Unresolved` handler returns `EngineError::HandlerBinaryMissing` with
the handler id, reason, and structured remediation:

```
handler binary missing: 'bin/x86_64-unknown-linux-gnu/my_tool'
for handler 'handler:my/custom-tool' — binary not found in any bundle.
binary 'bin/...' not installed on this node — install the bundle
containing it or push it as a project-tier item.
```

Other boot issues (schema mismatch, signature mismatch, untrusted
publisher) still cause boot to fail. Only `HandlerBinaryMissing` is
downgraded.

This allows pushed snapshots that reference tools the remote doesn't
have to build successfully and pass boot validation, while still
producing a clear error if the operator actually tries to invoke one.

## Configuration

Remotes are stored in `<system_space_dir>/.ai/config/remotes/remotes.yaml`:

```yaml
remotes:
  production:
    name: production
    url: https://ryeos.example.com
    principal_id: principal_abc123
    vault_fingerprint: sha256:def456...
    ingest_ignore:
      patterns:
        - ".git/"
        - "target/"
        - "*.pyc"
```
