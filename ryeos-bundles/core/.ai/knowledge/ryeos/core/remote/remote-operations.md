---
category: ryeos/core
tags: [remote, operations, trust, security, networking]
version: "1.0.0"
description: >
  Remote execution and bundle synchronization — trust model,
  operator workflows, fail-closed semantics, and security requirements.
---

# Remote Operations

Remote operations allow one ryeos node to execute items, push/pull
CAS objects, and synchronize bundles with another node over HTTPS.

## v1 Trust Boundary

v1 remote execution is for **operator-trusted remotes**, not mutually
untrusted tenants.

- CAS is shared/global within a node; capability checks protect access,
  not storage partitioning.
- Vault is a single shared store in v1; capability checks protect
  mutation/listing, not per-principal isolation.
- All remote requests are signed with Ed25519; the remote node verifies
  the signature against its authorized-keys trust store.

## Prerequisites

### On the remote node

1. Bootstrap: `ryeos init` (generates node signing key, vault key pair,
   bootstrap authorized key).
2. Trust the caller: `ryeos trust pin <caller_fingerprint>`.
3. Authorize a scoped key:
   ```
   ryeos remote authorize \
     --public-key "ed25519:<base64>" \
     --label "ci-pipeline" \
     --scopes '["ryeos.execute.service.remote.execute"]'
   ```
   Wildcard (`*`) delegation is rejected in v1 — all scopes must be
   enumerated explicitly.

### On the caller node

1. Configure the remote:
   ```
   ryeos remote configure --name production --url https://ryeos.example.com
   ```
   This discovers the remote's public key, vault fingerprint, and
   caches its ingest-ignore rules.

## End-to-End Workflow (Synchronous v1)

```
# 1. On the remote — trust the caller
ryeos trust pin <caller_fingerprint>

# 2. On the remote — authorize a scoped key
ryeos remote authorize \
  --public-key "ed25519:<caller_pubkey_b64>" \
  --label "dev-machine" \
  --scopes '["ryeos.execute.service.remote.execute"]'

# 3. On the caller — configure the remote
ryeos remote configure --name prod --url https://ryeos.example.com

# 4. Execute on the remote
ryeos remote execute tool:my/heavy-compute --remote prod
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
Remote authorization (`ryeos remote authorize`) rejects any request
that includes `"*"` in the scopes list. All remote keys must enumerate
their capabilities explicitly.

## Node-Key Rotation

When a node's signing key is compromised or needs rotation:

1. Rotate the node signing key (generate new Ed25519 key pair).
2. Reissue the bootstrap local operator key.
3. Reissue every remotely granted authorized key.
4. All existing authorized-key TOMLs become invalid immediately.

This is an intentional break-glass: node-key rotation invalidates all
delegated authority.

### Key rotation on the caller side

1. Generate a new key pair on the caller node.
2. On the remote: `ryeos remote authorize` with the new public key.
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

```
ryeos remote pull --remote production --hashes abc123 def456
ryeos remote pull --remote production --hashes abc123 --output-dir /tmp/objects
```

**Fail-closed**: aborts if any requested hash is missing. No partial fetches.
Required capability: `ryeos.execute.service.objects/get`

### Remote Bundle Install

Install a complete bundle from a remote node via CAS pipeline:

1. `bundle.export` on remote — walks bundle tree, ingests into remote CAS.
2. Fetches all blobs via `objects.get`.
3. Materializes to local bundle directory.
4. Runs preflight verification.
5. Writes signed node-config registration.

```
ryeos remote bundle-install --remote production --bundle-name standard
```

**Fail-closed**:
- Missing blobs → abort before materialization.
- Preflight failure → clean up partial directory.
- No registration written unless preflight passes.

Required capability: `ryeos.execute.service.bundle/install`

### Bundle Export (server-side)

Walk an installed bundle's tree, ingest files into CAS, return manifest.
Called automatically by `remote bundle-install` — not invoked directly.
Required capability: `ryeos.execute.service.bundle/export`

## Additional Remote Commands

```
ryeos remote push --remote production
ryeos remote vault-set --remote production --name API_KEY --value "sk-..."
ryeos remote vault-list --remote production
ryeos remote vault-delete --remote production --name API_KEY
ryeos remote threads --remote production
ryeos remote thread-status --remote production --thread-id abc123
```

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
