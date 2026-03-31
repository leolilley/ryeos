```yaml
id: node-vault
title: "Node Vault — Per-Principal Encrypted Secret Store"
description: Per-principal encrypted secret storage on ryeos-node. File-per-secret sealed with X25519 + ChaCha20Poly1305, decrypted only at execution time. Vault keys referenced by name in webhook bindings and signed execute requests. Progression from simple file-backed store to DEK-based model to federated cluster vaults.
category: future
tags:
  [
    secrets,
    vault,
    encryption,
    x25519,
    chacha20poly1305,
    node,
    webhooks,
    security,
  ]
version: "0.1.0"
status: in-progress
```

# Node Vault — Per-Principal Encrypted Secret Store

> **Status:** V1 in progress. V2/V3 exploratory.

## Current State

Secrets reach executions via `secret_envelope` — sealed with X25519, decrypted at exec time by the node. This works for signed `/execute` requests where the caller constructs the envelope per-call.

Problems:

- **Webhook bindings bake the envelope at creation time.** The sealed secret is stored in the binding. Rotating a secret means recreating every binding that uses it.
- **Hosted nodes (Railway, Render) require manual env var management.** Users set secrets on the hosting platform's dashboard. No programmatic access from the agent.
- **No secret management API.** An agent can't set, list, or rotate secrets on a node it controls.

---

## V1 — File-Per-Secret (Implementing Now)

Simple file-backed store reusing existing crypto infrastructure.

### Storage

```
<cas_base>/<fingerprint>/vault/<NAME>.json
```

Each file contains a sealed envelope — the same `secret_envelope` format used today (X25519 + ChaCha20Poly1305). Secrets are encrypted at rest with the node's public key, decrypted only at execution time.

### Server API

| Endpoint        | Method   | Description                      |
| --------------- | -------- | -------------------------------- |
| `/vault/set`    | `POST`   | Store a sealed secret by name    |
| `/vault/list`   | `GET`    | List secret names (never values) |
| `/vault/delete` | `DELETE` | Remove a secret                  |

### Usage in Executions

Webhook bindings gain a `vault_keys` field:

```yaml
vault_keys: ["OPENAI_API_KEY", "DATABASE_URL"]
```

At execution time, the node reads the named secrets from the vault, decrypts them, and injects them into the execution environment. No more baked-in envelopes — rotating a secret is a single `/vault/set` call; all bindings referencing that name pick up the new value on next execution.

Signed `/execute` requests can also specify `vault_keys` alongside or instead of inline `secret_envelope`.

### Client Side

New actions on the remote tool:

- `vault_set` — seal and upload a secret
- `vault_list` — list secret names on a node
- `vault_delete` — remove a secret

### Validation

Reuses existing `is_safe_secret_name()` for name validation and `validate_env_map()` for blocked name enforcement.

---

## V2 — Data Encryption Key Model (Future)

V1 performs one X25519 + ChaCha20Poly1305 unseal per secret per execution. With 10 secrets, that's 10 public-key decryptions. V2 introduces a per-principal DEK to make this O(1).

### Architecture

```
Node X25519 key
  └─ seals → DEK envelope (one per principal)
                └─ DEK (random symmetric key)
                      └─ encrypts → individual secrets (fast symmetric crypto)
```

- **One public-key decrypt per execution** — unseal the DEK envelope, then decrypt N secrets symmetrically.
- **Cleaner key rotation** — rotating the node's X25519 key only requires re-sealing the DEK envelope, not every individual secret.
- **Bulk operations** — import/export and vault replication across nodes become practical (re-seal the DEK to the target node's key, ship all secrets as-is).

### Additional Capabilities

- **Secret versioning** — history of values with timestamps and principal attribution.
- **Audit log** — who set what, when. Queryable per-principal.
- **Vault backup/restore** — re-seal the DEK to a new node's key, transfer all secrets without re-encrypting each one.
- **Secret leasing / TTL** — auto-expire secrets after a duration. Node garbage-collects expired entries.

---

## V3 — Cluster Vault (Future)

Vault federation across node clusters.

- **Replication policies** — declare which secrets sync to which nodes. A production database credential replicates to all worker nodes; a staging key stays on the staging node.
- **Centralized management node** — single vault authority with satellite distribution. Secrets flow outward, never inward.
- **External backend integration** — HashiCorp Vault, AWS Secrets Manager, GCP Secret Manager as storage backends behind the same `/vault/*` API surface. The node vault becomes an adapter layer.

---

## Security Model

**Authentication.** All vault endpoints require signed-request auth — same mechanism as `/execute`. No unauthenticated vault access.

**Capability gating.** Vault operations are gated by `rye.vault.*` capabilities. A principal can be granted `rye.vault.set` without `rye.vault.delete`, or scoped to specific secret name patterns.

**Secrets never leak.** Secret values are never logged, never returned in API responses. `/vault/list` returns names only. Error messages reference secret names, never contents.

**File permissions.** Vault files are `0600`, vault directories `0700`. Owned by the node process user.

**Atomic writes.** Secrets are written to a temp file and renamed into place — no partial reads on concurrent access.

**Blocked names.** `PATH`, `PYTHONPATH`, `LD_LIBRARY_PATH`, `HOME`, and other dangerous env names are rejected at set time via the existing blocked-name list.
