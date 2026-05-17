# Remote Execution v1 — Operator Guide

## 1. Prerequisites

### On the remote node

1. **Bootstrap the node:**
   ```
   ryeos init
   ```
   This generates the node signing key, vault key pair, and bootstrap
   authorized key.

2. **Trust the caller's signing key:**
   ```
   ryeos trust pin <caller_fingerprint>
   ```
   This adds the caller's Ed25519 public key to the node's trust store.

3. **Authorize a scoped key for the caller:**
   ```
   ryeos remote authorize \
     --public-key "ed25519:<base64>" \
     --label "ci-pipeline" \
      --scopes '["ryeos.execute.service.remote.execute"]'
   ```
   This creates a node-signed authorized-key TOML with fine-grained
   scopes. Wildcard (`*`) delegation is rejected in v1.

### On the caller node

1. **Configure the remote:**
   ```
   ryeos remote configure --name production --url https://ryeos.example.com
   ```
   This discovers the remote's public key, vault fingerprint, and
   caches its ingest-ignore rules.

## 2. v1 Trust Boundary

v1 remote execution is for operator-trusted remotes, not mutually
untrusted tenants.

- CAS is shared/global within a node; capability checks protect access,
  not storage partitioning.
- Vault is a single shared store in v1; capability checks protect
  mutation/listing, not per-principal isolation.

## 3. No Wildcard Delegation

Bootstrap may create `["*"]` keys locally for the node operator.

Remote authorization (`ryeos remote authorize`) will reject any request
that includes `"*"` in the scopes list. All remote keys must enumerate
their capabilities explicitly.

## 4. Node-Key Rotation Procedure

When a node's signing key is compromised or needs rotation:

1. **Rotate the node signing key.** Generate a new Ed25519 key pair.
2. **Reissue the bootstrap local operator key.** The new node key
   re-signs the operator's authorized-key TOML.
3. **Reissue every remotely granted authorized key.** Each remote
   caller must obtain a new authorized-key TOML signed by the new node
   key.
4. **All existing authorized-key TOMLs become invalid immediately.**
   The old signing key is no longer trusted.

This is an intentional break-glass procedure: node-key rotation
invalidates all delegated authority.

## 5. Remote Ignore Cache

`ryeos remote configure` caches the remote node's ingest-ignore rules
(from `node/ingest/ignore.yaml`) in the local remotes config.

Subsequent `remote push` and `remote execute` operations use the cached
remote rules to build the push manifest — only files the remote would
accept are included.

If the remote operator changes their ignore rules, re-run:
```
ryeos remote configure --name production --url https://ryeos.example.com
```

If no cached rules are available (first push after config, or cache
miss), the push handler fetches the remote's ignore rules inline and
persists them to `remotes.yaml` for future use. If the inline fetch
fails, the push is **aborted** — the handler does not silently fall
back to local ignore rules. Run `ryeos remote configure` first or
ensure the remote is reachable.

## 6. End-to-End Workflow (Synchronous v1)

```
# Step 1: On the remote node — trust the caller
ryeos trust pin <caller_fingerprint>

# Step 2: On the remote node — authorize a scoped key
ryeos remote authorize \
  --public-key "ed25519:<caller_pubkey_b64>" \
  --label "dev-machine" \
  --scopes '["ryeos.execute.service.remote/execute"]'

# Step 3: On the caller node — configure the remote
ryeos remote configure --name prod --url https://ryeos.example.com

# Step 4: Execute on the remote
ryeos remote execute tool:my/heavy-compute --remote prod
```

The synchronous `remote execute` command:

1. **Push** — ingests the local project, uploads missing CAS objects to
   the remote, advances the remote's HEAD ref.
2. **Execute** — runs the specified item on the remote using the pushed
   content.
3. **Pull** — fetches the resulting snapshot, diffs against the pre-push
   manifest, and applies changes to the local workspace.

If the pull detects that any local tracked file has changed since the
push, the entire apply is aborted (clean-base policy). No partial writes.
On abort, the local HEAD ref is rolled back to its pre-push state.

## 7. Async Limitations

v1 is **synchronous only**. The `remote execute` command blocks until
the remote execution completes and results are pulled back.

Asynchronous remote execution with polling is deferred to a future
release. There is no `--async` flag.

## 8. HTTPS Requirement

All remote communication uses Ed25519 request signing for
authentication and integrity. However, vault `set` operations send
plaintext secret values under the signing layer — they are not
end-to-end encrypted by the vault protocol itself.

**Production deployments must terminate TLS in front of `ryeosd`.**
HTTP without TLS is only acceptable for loopback addresses
(`localhost`, `127.0.0.1`, `::1`).

## Additional Commands

### Push-only
```
ryeos remote push --remote production
```

### Vault operations
```
ryeos remote vault-set --remote production --name API_KEY --value "sk-..."
ryeos remote vault-list --remote production
ryeos remote vault-delete --remote production --name API_KEY
```

### Thread introspection
```
ryeos remote threads --remote production
ryeos remote thread-status --remote production --thread-id abc123
```

### Key rotation on the caller side
1. Generate a new key pair on the caller node.
2. On the remote: run `ryeos remote authorize` with the new public key.
3. On the caller: re-run `ryeos remote configure` to pick up any changes.
4. On the remote: remove the old authorized-key TOML from
   `.ai/node/auth/authorized_keys/`.

## Bundle Synchronization

### Remote pull — fetch CAS objects

Fetch arbitrary CAS objects (blobs or JSON objects) from a remote node
and store them in the local CAS. Optionally materialize to a directory.

```
ryeos remote pull --remote production --hashes abc123 def456
ryeos remote pull --remote production --hashes abc123 --output-dir /tmp/objects
```

Fail-closed: if **any** requested hash is missing on the remote, the
entire operation is aborted. No partial fetches.

Required capability: `ryeos.execute.service.objects/get`

### Remote bundle install

Install a complete bundle from a remote node via the CAS pipeline:

1. Calls `bundle.export` on the remote to walk the bundle tree and
   ingest every file into the remote's CAS.
2. Fetches all file blobs via `objects.get`.
3. Materializes them into the local bundle install directory.
4. Runs preflight verification on the materialized bundle.
5. Writes a signed node-config bundle registration.

```
ryeos remote bundle-install --remote production --bundle-name standard
```

Fail-closed guarantees:
- If any blob is missing, the install is aborted before materialization.
- If preflight verification fails, the partial directory is cleaned up.
- No signed registration is written unless preflight passes.

Required capability: `ryeos.execute.service.bundle/install`

### Bundle export (server-side only)

Walk an installed bundle's directory tree, ingest every file into the
node's CAS, and return a manifest of file hashes. This is the
server-side half of remote bundle install — callers don't invoke it
directly; `remote bundle-install` calls it on the remote automatically.

Required capability: `ryeos.execute.service.bundle/export`

## Configuration File

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
