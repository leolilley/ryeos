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
status: done
```

# Node Vault — Per-Principal Encrypted Secret Store

> **Status:** V1 done. V2/V3 exploratory.

## V1 — File-Per-Secret (Done)

Simple file-backed store reusing existing crypto infrastructure. Implemented in `ryeos-node/ryeos_node/vault.py` and `server.py`.

### Storage

```
<cas_base>/<fingerprint>/vault/<NAME>.json
```

Each file contains a sealed envelope — the same `secret_envelope` format used today (X25519 + ChaCha20Poly1305). Secrets are encrypted at rest with the node's public key, decrypted only at execution time.

### Server API

| Endpoint        | Method | Description                      |
| --------------- | ------ | -------------------------------- |
| `/vault/set`    | `POST` | Store a sealed secret by name    |
| `/vault/list`   | `GET`  | List secret names (never values) |
| `/vault/delete` | `POST` | Remove a secret                  |

### Usage in Executions

Webhook bindings support a `vault_keys` field:

```yaml
vault_keys: ["OPENAI_API_KEY", "DATABASE_URL"]
```

At execution time, the node reads the named secrets from the vault, decrypts them, and injects them into the execution environment (`server.py:1225-1232`). Rotating a secret is a single `/vault/set` call; all bindings referencing that name pick up the new value on next execution. Inline `secret_envelope` on `/execute` overrides vault-resolved values.

Signed `/execute` requests can also specify `vault_keys` alongside or instead of inline `secret_envelope`.

### Client Side

Actions on the remote tool (`remote/remote.py`):

- `vault_set` — seal with node identity and upload a secret
- `vault_list` — list secret names on a node
- `vault_delete` — remove a secret

### Validation

Reuses existing `is_safe_secret_name()` for name validation and `validate_env_map()` for blocked name enforcement.

### Security

All vault endpoints require signed-request auth (`get_current_principal`). File permissions enforced: vault directories `0700`, secret files `0600`. Atomic writes via temp file + `os.replace` + `fsync`. Secret values never logged or returned in API responses.

### Known Gaps

- **`/vault/delete` uses `POST` instead of `DELETE`.** Doc originally specified `DELETE` method; implementation settled on `POST` for consistency with signed-request auth payload handling. Low priority to change.
- **No `rye.vault.*` capability gating on endpoints.** The original spec called for fine-grained LLM capability gating (e.g. `rye.vault.set` without `rye.vault.delete`). Since vault operations are node-side (not LLM-initiated), capability gating is handled differently — via node-level auth and principal identity rather than LLM caps. No change planned here.

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
