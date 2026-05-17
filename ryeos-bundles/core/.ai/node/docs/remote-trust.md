# Remote Execution Trust Model

## Overview

Remote execution in ryeos is built on a **single shared vault** trust boundary:
every node has exactly one vault identity (X25519 key pair) and all authorized
keys share the same vault access level.

## Trust Prerequisites

Before a node can execute remote commands on another node:

1. **Both nodes must be bootstrapped** — each has a node signing key and vault
   key pair generated during `ryeos init`.

2. **Vault fingerprint exchange** — the caller must know the remote's vault
   fingerprint (SHA-256 of the remote's vault X25519 public key). This is
   obtained out-of-band (e.g., `ryeos identity public-key` on the remote).

3. **Key authorization** — the remote node must have authorized the caller's
   public key via `ryeos remote-authorize`. This creates a node-signed
   `authorized-key.toml` entry on the remote.

4. **Scope restriction** — authorized keys declare a **strict subset** of the
   granting node's own scopes. Wildcard scopes (`*`) are **rejected** in v1.
   This prevents privilege escalation.

5. **HMAC request signing** — every authenticated request is signed with the
   caller's Ed25519 signing key. The remote verifies the signature against
   the authorized key's fingerprint.

## Trust Boundary: Single Shared Vault

All authorized keys on a node share the same vault. There is no per-key
vault isolation in v1. This means:

- Any authorized key can read, set, list, or delete any vault secret.
- Any authorized key can execute any service within its authorized scopes.
- Any authorized key can list and inspect any thread.

This is a deliberate v1 simplification. Per-key vault isolation is deferred
to a future release.

## Scope Delegation Rules

| Rule | Description |
|------|-------------|
| **No wildcards** | `*` is forbidden in authorized scope sets |
| **Subset only** | Granted scopes must be a subset of the granter's scopes |
| **No escalation** | A key with `vault/get` cannot grant `vault/set` |
| **Explicit list** | Every scope must be enumerated explicitly |

## Key Rotation

To rotate an authorized key:

1. Generate a new key pair on the caller node.
2. Run `ryeos remote-authorize` with the new public key from the granting node.
3. Update the caller's remote configuration to use the new key.
4. (Optional) Remove the old key from the remote's `authorized-keys/`.

The granting node signs the new authorization entry, so no manual TOML editing
is needed.

## Revocation

Remove the authorized key's TOML file from `<system>/.ai/node/authorized-keys/`
on the remote node and restart the daemon. There is no CRL/OCSP mechanism in v1.
