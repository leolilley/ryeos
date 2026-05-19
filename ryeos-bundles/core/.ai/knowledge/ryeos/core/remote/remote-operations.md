---
category: ryeos/core
tags: [remote, operations, trust, security, networking]
version: "2.0.0"
description: >
  Remote execution and bundle synchronization — trust model,
  operator workflows, fail-closed semantics, and security requirements.
  Revised to accurately document the node-key identity model and the
  actual CLI bootstrap flow.
---

# Remote Operations

Remote operations allow one ryeos node to execute items, push/pull
CAS objects, and synchronize bundles with another node over HTTPS.

See also: [Identity Model](../identity-model.md) for the four trust
layers and request authentication flow.

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
- Granting the `remote.admin` capability is operator-level access in
  v1 — there is no per-action isolation between remote subcommands.

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

1. **Bootstrap**: `ryeos init` (generates node signing key, vault key,
   bootstrap authorized key for the local operator).

2. **Authorize the caller's node key**: The remote operator needs the
   caller's node public key (in `ed25519:<base64>` format). This is
   obtained by running `ryeos identity public-key` on the caller node.

   ```bash
   ryeos authorize-key \
     --public-key "ed25519:<caller_node_pubkey_b64>" \
     --label "dev-machine" \
     --scopes "ryeos.execute.service.objects.has,ryeos.execute.service.objects.put,ryeos.execute.service.objects.get,ryeos.execute.service.push_head,ryeos.execute.service.authorize_key"
   ```

   The scopes listed above are the minimum required for the `remote
   exec` push → execute → pull pipeline. The remote's
   `objects.has`, `objects.put`, `objects.get`, and `push_head`
   endpoints each check their own capability.

 3. **(Optional) Narrower scopes for specific operations**:

   | Operation | Required scopes (canonical) |
   |-----------|-----------------------------|
   | `remote execute` (full pipeline) | `ryeos.execute.service.objects.has`, `ryeos.execute.service.objects.put`, `ryeos.execute.service.objects.get`, `ryeos.execute.service.push_head`, `ryeos.execute.service.authorize_key` |
   | `remote push` | `ryeos.execute.service.objects.has`, `ryeos.execute.service.objects.put`, `ryeos.execute.service.push_head` |
   | `remote pull` | `ryeos.execute.service.objects.get` |
   | `remote vault-set/list/delete` | `ryeos.execute.service.vault.set`, `ryeos.execute.service.vault.list`, `ryeos.execute.service.vault.delete` |
   | `remote threads/thread-status` | (authenticated; threads are per-principal isolated) |
   | `remote bundle-install` | `ryeos.execute.service.bundle.export`, `ryeos.execute.service.objects.get` |

   Short-form scopes like `bundle.install` will never authorize a
   handler — the matcher does not auto-prefix and the daemon's
   auth loader refuses to load TOMLs that contain them.

### On the caller node

1. **Configure the remote**:

   ```bash
   ryeos remote configure --name production --url https://ryeos.example.com
   ```

   This discovers the remote's public key, vault fingerprint, and
   caches its ingest-ignore rules.

## End-to-End Workflow

```bash
# ── On the CALLER node ──

# 1. Display your node public key (share this with the remote operator)
ryeos identity public-key

# 2. Configure the remote
ryeos remote configure --name prod --url https://ryeos.example.com

# ── On the REMOTE node ──

# 3. Authorize the caller's node key (use the output from step 1)
ryeos authorize-key \
  --public-key "ed25519:<caller_node_pubkey_b64>" \
  --label "dev-machine" \
  --scopes "ryeos.execute.service.objects.has,ryeos.execute.service.objects.put,ryeos.execute.service.objects.get,ryeos.execute.service.push_head,ryeos.execute.service.authorize_key"

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
Required capability: `ryeos.execute.service.objects.get`

### Remote Bundle Install

Install a complete bundle from a remote node via CAS pipeline:

```bash
ryeos remote bundle-install --remote production --bundle-name standard
```

**Fail-closed**:
- Missing blobs → abort before materialization.
- Preflight failure → clean up partial directory.
- No registration written unless preflight passes.

Required capabilities: `ryeos.execute.service.bundle.install`,
`ryeos.execute.service.bundle.export`, `ryeos.execute.service.objects.get`

### Bundle Export (server-side)

Walk an installed bundle's tree, ingest files into CAS, return manifest.
Called automatically by `remote bundle-install` — not invoked directly.
Required capability: `ryeos.execute.service.bundle.export`

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
