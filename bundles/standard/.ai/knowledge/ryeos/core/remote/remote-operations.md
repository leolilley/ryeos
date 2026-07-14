<!-- ryeos:signed:2026-07-14T10:12:30Z:32aa040113a899e23106a7f3ab811958ba13ffb3ca4241aa8124c34f52bd416d:he4O+CfC+gtzmD6nX1Z/zgrmpSWadZH8utJtrJVk/jnEUGy6xHzCZKFTaA8BSAH8Ynuv2PzcU8F8sI00nwLXAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [remote, operations, trust, security, networking]
version: "3.7.0"
description: >
  Remote execution and bundle synchronization — trust model,
  operator workflows, fail-closed semantics, and security requirements.
  Covers the per-request engine, hybrid binary resolution, project
  pull-back, single-flight cache builds, and live bundle roots.
---

# Remote Operations

Remote operations allow one ryeos node to execute items, push/pull
CAS objects, and synchronize bundles with another node over HTTPS.

See also:

- [Remote Command Reference](remote-command-reference.md) for command syntax, examples, capabilities, scopes, routes, and failure modes.
- [Identity Model](../identity-model.md) for the four trust layers and request authentication flow.
- [Spaces](../spaces.md) for project/system resolution.
- [Execution Provenance](../execution/provenance.md) for local vs pushed-head execution source invariants.
- [Execution Sandbox](../node/execution-sandbox.md) for the target node's immutable subprocess boundary.
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
ryeos identity
  ```

- The remote operator authorizes the caller's **node key fingerprint**,
  not the user key fingerprint.

## Prerequisites

### On the remote node

1. **Bootstrap**: run `ryeos init` so the remote has node identity,
   vault key material, local operator authorization, and installed
   bundles.

2. **Authorize the caller node key**: either mint a one-time admission
   token on the target node, or directly authorize the caller node key.
   For direct authorization, the remote operator needs the caller node
   public key in `ed25519:<base64>` form. The caller gets it with:

   ```bash
  ryeos identity
   ```

   Preferred bootstrap is token based: the remote operator runs a local
   offline tool on the target node and sends the cleartext token to the
   caller out of band:

   ```bash
   ryeos admission-token \
     --label "dev-machine" \
     --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head" \
     --ttl-secs 600
   ```

   The caller then configures the remote and claims the grant with a
   self-signed admission request:

   ```bash
   ryeos remote configure --remote production --url https://ryeos.example.com
   ryeos remote admit \
     --remote production \
     --token "<one-time-token>" \
     --label "dev-machine" \
     --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head"
   ```

   Direct local authorization remains available when the operator has the
   caller's node public key:

   ```bash
   ryeos authorize-key \
     --public-key "ed25519:<caller_node_pubkey_b64>" \
     --label "dev-machine" \
     --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head"
   ```

3. **Choose scopes by operation**. Do not mix local remote-service caps
   with remote authorized-key scopes. The complete matrix is in
   [Remote Command Reference](remote-command-reference.md#authority-matrix).

   Common remote-side scopes:

   | Operation | Remote scopes on target |
   |-----------|-------------------------|
   | `remote push` | `ryeos.execute.service.objects/has`, `ryeos.execute.service.objects/put`, `ryeos.execute.service.system/push-head` |
   | `remote pull` | `ryeos.execute.service.objects/get` |
   | `remote execute` | push scopes + `ryeos.execute.service.objects/get` + whatever caps the executed item requires |
   | `remote authorize` | `ryeos.execute.service.identity/authorize-key` |
   | `remote bundle-install` | `ryeos.execute.service.bundle/export`, `ryeos.execute.service.objects/get` |
   | `remote vault-set/list/delete` | matching `ryeos.execute.service.vault.*` cap |
   | `remote threads/thread-status` | signed auth; no additional thread service cap in v1 |

   Short-form scopes like `bundle.install` will never authorize a
   handler — the matcher does not auto-prefix and the daemon's auth
   loader refuses to load TOMLs that contain them.

### On the caller node

1. **Configure the remote**:

   ```bash
   ryeos remote configure --remote production --url https://ryeos.example.com
   # or, with a provider/operator descriptor pin:
   ryeos remote configure --descriptor ./production.remote.yaml
   ```

   This discovers the remote's public key, principal id, vault
   fingerprint, and ingest-ignore rules. Descriptor import is a trust pin
   only; it is verified against the live `/public-key` document before
   local config is written.

### Publishing a descriptor

Any node operator can export a descriptor locally:

```bash
ryeos remote-descriptor \
  --name production \
  --url https://ryeos.example.com \
  --capabilities "remote-execute,bundle-install" \
  --output ./production.remote.yaml
```

The descriptor is intentionally generic core data. RyeOS Cloud or another
hosted/provider bundle may distribute it through a web UI later, but the
authority model does not depend on that distribution channel. The caller
still verifies the descriptor against the live node identity during
`remote configure --descriptor`.

## End-to-End Operator Workflow

This is the copy-pasteable two-node bootstrap path. It uses only generic
core primitives; hosted/provider bundles can later wrap the same flow in
UI or provisioning automation.

### Target node / remote operator

Export a descriptor trust pin for the node and mint a one-time admission
token. Deliver both files/values to the caller out of band.

```bash
ryeos remote-descriptor \
  --name prod \
  --url https://ryeos.example.com \
  --output ./prod.remote.yaml

ryeos admission-token \
  --label "dev-machine" \
  --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head" \
  --ttl-secs 600
```

### Caller node

Import the descriptor, claim the grant, verify setup, then execute.

```bash
ryeos remote configure --descriptor ./prod.remote.yaml

ryeos remote admit \
  --remote prod \
  --token "<one-time-token>" \
  --label "dev-machine" \
  --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head"

ryeos remote doctor --remote prod

ryeos remote execute --remote prod --item-ref tool:my/heavy-compute
```

Operational notes:

- The descriptor is a trust/discovery pin, not a credential. Importing it
  still verifies the live `/public-key` response before local config is
  written.
- The admission token is one-time bootstrap material. It must be
  delivered out of band, is shown only once by the token tool, and is
  consumed by `/admission/claim`.
- Non-loopback admission should use HTTPS. Request signing authenticates
  claims, but it does not encrypt the token in transit.
- Admission tokens and admission claims must use concrete scopes only;
  wildcard scopes are rejected.
- Runtime authority after admission is the normal target-node-local
  `authorized_keys` grant for the caller node key.

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
Remote authorization (`ryeos authorize-key`) and admission bootstrap
reject any requested or token-file scope containing `"*"`. All remote
keys must enumerate their capabilities explicitly.

## Node-Key Rotation

When a node's signing key is compromised or needs rotation, treat it as a
break-glass operation. The daemon no longer has an `--init-only` path and
will not auto-regenerate the node key, because doing so would invalidate
the node trust doc pinned in the node trust store.

Safe rotation requires explicit operator action:

1. Stop the daemon.
2. Replace/regenerate the node signing key under
   `<system>/.ai/node/identity/private_key.pem`.
3. Recreate and pin the matching node trust doc in the node trust store.
4. Re-sign or recreate node-signed local config items as needed.
5. Reissue every remotely granted authorized key.

This intentionally invalidates delegated authority until remotes
authorize the new node key.

### Key rotation on the caller side

1. Rotate the node key on the caller.
2. On the remote: `ryeos authorize-key` with the new node public key.
3. On the caller: `ryeos remote configure` to pick up changes.
4. On the remote: remove any superseded authorized-key TOML from
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
Local capability and remote target scope: `ryeos.execute.service.objects/get`

### Remote Bundle Install

Install a complete bundle from a remote node via CAS pipeline:

```bash
ryeos remote bundle-install --remote production --bundle-name standard
```

**Fail-closed**:

- The export is limited to 10,000 files, 20,000 total tree entries, 64 levels,
  4 KiB paths, 32 MiB per file, and 256 MiB of declared file content.
- Object responses are streamed into bounded buffers and fetched in bounded
  batches; the installer never retains a whole-bundle blob map. The export and
  each object batch have a five-minute total request bound covering both the
  response and bounded body read.
- Missing, duplicate, malformed, wrong-sized, or hash-mismatched blobs abort
  the operation and remove the hidden staging generation.
- Network transfer uses a request-unique hidden generation while no bundle
  mutation lock is held. The installer reacquires that lock only to reconcile,
  run prospective admission, journal, and durably activate the completed tree;
  it rechecks for a concurrent install before activation. A bounded scavenger
  removes only same-owner, real-directory UUID transfer generations that have
  remained stale for at least 24 hours; current transfers are not age-eligible.
- The completed staging tree must pass signed-manifest preflight and the same
  prospective engine/node-config admission used at node boot. A bundle that
  would introduce a registry, protocol, runtime, command, or native-executor
  collision never becomes live.
- No registration is written unless activation succeeds. Transaction recovery
  completes an interrupted committed install and invalidates cached engines.

Local capability: `ryeos.execute.service.bundle/install`.
Remote scopes on the target: `ryeos.execute.service.bundle/export`,
`ryeos.execute.service.objects/get`.

### Bundle Export (server-side)

Walk an installed bundle's tree, ingest files into CAS, return manifest.
Called automatically by `remote bundle-install` — not invoked directly.
Remote target scope: `ryeos.execute.service.bundle/export`

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
content rather than the remote operator's own local workspace.

### How it works

1. The caller pushes a **project manifest** (project files). Legacy
   snapshots carrying a `user_manifest_hash` are rejected — re-push the
   project.
2. The remote daemon checks the **engine cache** keyed by
   `(system_install_generation, snapshot_hash)`. On a miss, it:
   - Reads the live signed bundle registry with
     `BootstrapLoader::load_bundle_section()`.
   - Builds a fresh `Engine` against registered bundle roots + the
     project checkout.
3. The project checkout is **per-request** (each request gets its own
   directory, cleaned up when the request completes).
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
- Existing cached engines continue using the bundle set they were built
  with until evicted by LRU or the idle threshold.
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

A `pushed_head` request resolves only against the pushed project
checkout plus the remote's registered bundle roots. There is no
operator user space to fall through to — user-tier resolution does not
exist in the single-app-root model.

Executable tool/runtime launches then pass through the target node's sandbox
snapshot exactly like local launches. Remote content cannot enable or weaken
that policy. The default readable placeholders expose only the verified bundle
roots, resolved pushed item source, node trusted-key directory, and any exact
callback socket required by the managed launch; the request-owned checkout is
the writable project.

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
pins the original Root's temp dir.

Resuming a `pushed_head` thread under the per-request overlay is
tracked separately; the resume path falls back to the daemon engine.
Detached children from callbacks are disallowed at the callback
dispatcher (inline only).

## Pull-Back

After remote execution, the pull phase applies changes to the local
project: it diffs the pre-push manifest against the remote's result
snapshot and applies updated/deleted files to the local project
directory.

The JSON response from `remote execute` reports the counts:

```json
{
  "pull": {
    "files_updated": 3,
    "files_deleted": 1
  }
}
```

The response also carries `user_files_updated` and `user_files_deleted`,
retained in the wire contract as always-`0` in the single-app-root
model — there is no user-space pull-back.

## Hybrid Binary Resolution

When a pushed handler descriptor references a binary that is
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
    principal_id: fp:<remote-signing-key-fingerprint>
    signing_key: ed25519:<base64-remote-public-key>
    site_id: site:production
    vault_fingerprint: sha256:def456...
    ingest_ignore:
      patterns:
        - ".git/"
        - "target/"
        - "*.pyc"
```
