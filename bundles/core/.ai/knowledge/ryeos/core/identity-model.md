<!-- ryeos:signed:2026-05-22T03:35:36Z:0c0103c047810f05b05d67c9f42264479dd76c1daf595c090c875c534148cfe4:oShSJxFWyPEXFdpk+lpp9c0fbI5vABWPzw95BrKOhRM29kbd7VqdRqZ1ANiOMMv8UkIC0KirQEHvLji7qy7tDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [identity, trust, keys, security, fundamentals]
version: "1.0.0"
description: >
  The four identity layers in ryEOS — what each key is for, where it
  lives, and which key signs which kind of request. This is the
  authoritative reference for all trust and auth decisions.
---

# Identity Model

ryeOS has four distinct identity/trust layers. They are **never
interchangeable** — each layer has a specific purpose, storage location,
and lifecycle.

## The Four Layers

| Layer                 | Purpose                                                                  | Storage                                            | Created by                       |
| --------------------- | ------------------------------------------------------------------------ | -------------------------------------------------- | -------------------------------- |
| **Publisher trust**   | Verify signed bundle items                                               | `~/.ryeos/.ai/config/keys/trusted/<fp>.toml`       | `ryeos trust pin`                |
| **User (CLI) key**    | Sign local HTTP requests from CLI to your own daemon                     | `~/.ryeos/.ai/config/keys/signing/private_key.pem` | `ryeos init`                     |
| **Node (daemon) key** | Sign outbound HTTP requests to remote daemons; sign authorized-key TOMLs | `<system>/.ai/node/identity/private_key.pem`       | `ryeos init` or daemon auto-init |
| **Vault X25519**      | Seal/unseal vault secrets (XChaCha20-Poly1305 envelopes)                 | `<system>/.ai/node/vault/private_key.pem`          | `ryeos init` or daemon auto-init |

### Publisher trust

The operator trust store at `~/.ryeos/.ai/config/keys/trusted/` holds the public
keys of publishers whose signed bundle items will be accepted. Each
entry is a TOML file named by fingerprint, signed by the key it
declares (self-signature).

Managed by `ryeos trust pin`. The `ryeos init` command auto-pins the
official publisher key and any `--trust-file` entries.

### User (CLI) key

The operator's persistent Ed25519 identity. The CLI uses this key to
sign every request to the **local** daemon (`POST /execute`, etc.).

- **Never force-regenerated** — `ryeos init --force` rotates the node
  key but preserves the user key.
- Rotation requires explicit operator action (manual key replacement).

The daemon authorizes this key at bootstrap by writing a node-signed
authorized-key TOML to
`<system>/.ai/node/auth/authorized_keys/<user-fp>.toml`
with scopes `["*"]`.

### Node (daemon) key

The daemon's Ed25519 identity. Used for:

1. **Signing outbound remote requests** — when your daemon calls a
   remote daemon, it signs with the **node key**, not the user key.
2. **Signing authorized-key TOMLs** — the daemon signs new
   authorized-key entries with its node key so they can be verified
   on next boot.
3. **Self-trust** — the node key's public half is pinned in
   `~/.ryeos/.ai/config/keys/trusted/` so daemon-written node-config items
   verify on subsequent boots.

**Critical for remote operations**: when a remote daemon receives an
authenticated request, it verifies the signature against its own
authorized-keys store. The key that must be authorized on the remote is
the **caller's node key**, NOT the caller's user key.

```
CLI ──[user key signs]──> local daemon ──[node key signs]──> remote daemon
                                                      ↑
                                    remote authorizes this fingerprint
```

### Vault X25519

A separate X25519 keypair used exclusively for sealing/unsealing
secret values in the vault. Separate from the Ed25519 node identity so
that node-key rotation does NOT brick the vault.

Rotation is via `ryeos vault rewrap` (generates new keypair, re-seals
all entries).

## Lifecycle

### Fresh install

```
ryeos init --source <bundles>
```

Creates all four layers:

1. User key (load-or-create)
2. Node key (load-or-create)
3. Vault X25519 (load-or-create)
4. Publisher trust (pin official key + `--trust-file` entries)
5. Self-trust entries for both user and node keys
6. User authorized-key TOML (node-signed, scopes `["*"]`)

### Daemon startup

```
ryeosd
```

The daemon verifies initialization (keys, bundles, registrations exist)
and loads the two-phase node config. If any key artefacts are missing,
the daemon auto-initializes by running `bootstrap::init` idempotently.

### Remote bootstrap

To authorize a caller on a remote node, the **remote operator** must
authorize the caller's **node key**:

```bash
# On the remote node:
ryeos authorize-key \
  --public-key "ed25519:<CALLER_NODE_PUBKEY_B64>" \
  --label "dev-machine" \
  --scopes "ryeos.execute.service.objects.has,ryeos.execute.service.objects.put,ryeos.execute.service.objects.get,ryeos.execute.service.push.head"
```

The fingerprint to authorize is the SHA-256 of the caller's **node**
public key (not the user key). To display the node public key:

```bash
# On the caller node:
ryeos identity public-key
```

## Key Rotation

| Key             | How to rotate                                                             | Side effects                                                             |
| --------------- | ------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| User key        | Manual replacement + re-pin trust                                         | CLI auth breaks until new key authorized                                 |
| Node key        | Regenerate node key + re-sign node-config items + re-trust + re-authorize | All remote authorizations invalidated; must re-authorize on every remote |
| Vault key       | `ryeos vault rewrap`                                                      | None — re-seals all entries under new key                                |
| Publisher trust | `ryeos trust pin`                                                         | Items signed by untrusted keys fail verification                         |

## Request Authentication Flow

### Local (CLI → daemon)

1. CLI resolves user signing key from `~/.ryeos/.ai/config/keys/signing/`
2. CLI signs `POST /execute` with Ed25519(user_key, sha256(canonical_request))
3. Daemon verifies against `authorized_keys/<fp>.toml`
4. Scopes from the TOML are the caller's effective capabilities

### Remote (daemon → daemon)

1. Local daemon resolves node identity from `<system>/.ai/node/identity/`
2. Local daemon signs outbound request with Ed25519(node_key, sha256(canonical_request))
3. Remote daemon verifies against its own `authorized_keys/<fp>.toml`
4. The authorized fingerprint must be the **caller's node key fingerprint**
