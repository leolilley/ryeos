---
category: ryeos/core
tags: [fundamentals, signing, trust, integrity]
version: "2.0.0"
description: >
  How signing and trust work in Rye OS — Ed25519 signatures,
  content hashing, path anchoring, the trust store, and the
  three-key trust model.
---

# Signing and Trust

Rye OS uses Ed25519 cryptographic signatures to ensure item integrity.
Every executable item must be signed before the daemon will run it.

## Three Keys, Three Roles

| Key | Purpose | Location |
|-----|---------|----------|
| **User key** | Operator identity — CLI auth, signing user/project items | `~/.ai/config/keys/signing/private_key.pem` |
| **Node key** | Daemon identity — bundle registrations, node config | `<system>/.ai/node/identity/private_key.pem` |
| **Publisher key** | Bundle author identity — signs items in published bundles | Pinned in `~/.ai/config/keys/trusted/` |

The user key and node key are generated at `ryeos init` time. The
publisher key is **hardcoded in the binary** and pinned during init
without trusting any on-disk file. For development, `--trust-file`
pins a dev publisher key instead.

## How Signing Works

When you run `ryeos sign <ref>`:

1. The engine resolves the item by canonical ref
2. The file content is hashed (SHA-256)
3. The hash is signed with the operator's Ed25519 private key
4. A signature header is prepended to the file:

```yaml
# ryeos:signed:<timestamp>:<content_hash>:<signature_b64>:<signer_fingerprint>
```

The signature covers:
- The **content hash** — any change to the file body invalidates the sig
- The **path anchor** — moving the file to a different path invalidates the sig

This means signatures are **tamper-evident** and **location-bound**.

## When to Sign

Sign after creating or editing any item:
- `ryeos sign directive:my/workflow`
- `ryeos sign tool:my/helper`
- `ryeos sign knowledge:my/context`
- `ryeos sign directive:*` (batch sign all directives)
- `ryeos sign tool:my/project/*` (namespace glob)

## Verification

`ryeos verify <ref>` checks:
1. The signature header exists
2. The content hash matches the current file content
3. The signing key is in the trust store
4. The path anchor matches the item's actual location

Unsigned or modified items produce an `IntegrityError` with a clear
message telling you exactly what to fix.

## Trust Store

The trust store lives in `~/.ai/config/keys/trusted/` and contains
Ed25519 public keys indexed by SHA-256 fingerprint:

```toml
fingerprint = "741a8bc6..."
owner = "official-publisher"

[public_key]
pem = "ed25519:MCowBQYDK2VwAyEA..."
```

Each trust doc is self-signed (signed by the key it declares). During
`ryeos init`, three entries are created:
1. **User self-trust** — your own key
2. **Node self-trust** — the daemon's key
3. **Publisher trust** — the hardcoded official publisher key (or dev key)

Additional publisher keys can be pinned with `ryeos trust pin`.

**Bundles do NOT ship trust docs.** The operator must pin publisher
keys explicitly via `ryeos init` (official) or `ryeos trust pin`
(third-party). This prevents a bundle from injecting its own trust.

## Bundle Install Verification

When `ryeos init` installs a bundle (or `ryeos bundle install` is used),
every signable item is verified:

1. **Signature present** — unsigned items in a bundle are rejected
2. **Signature valid** — Ed25519 verification against content hash
3. **Signer trusted** — the signer's fingerprint must be in the
   operator's trust store
4. **Path anchored** — the item must be at its declared location
5. **Manifest verified** — if a signed manifest.yaml exists, its
   signature, identity, and provides_kinds are validated

If any check fails, the entire bundle install is refused with a clear
error listing every failed item.

## System Space Is Immutable

Bundle items (system space) cannot be signed with the node key — they
carry the publisher's signature. If you need to customize a system item,
copy it to project or user space first, then sign with your key.

## Key Fingerprints

The key fingerprint is the SHA-256 hash of the raw 32-byte public key,
hex-encoded (64 characters). It appears in:
- Signature headers (last field after the colon)
- Trust store filenames (`<fingerprint>.toml`)
- `ryeos identity public-key` output

## Vault (Sealed Secrets)

Separate from signing, the daemon has an X25519 keypair at
`<system>/.ai/node/vault/` used for sealing/unsealing secrets.
This key is independent from the Ed25519 node identity — rotating
the node key does not affect sealed secrets.
