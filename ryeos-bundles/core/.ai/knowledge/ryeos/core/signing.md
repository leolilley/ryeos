---
category: ryeos/core
tags: [fundamentals, signing, trust, integrity]
version: "1.0.0"
description: >
  How signing and trust work in Rye OS — Ed25519 signatures,
  content hashing, path anchoring, and the trust store.
---

# Signing and Trust

Rye OS uses Ed25519 cryptographic signatures to ensure item integrity.
Every executable item must be signed before the daemon will run it.

## How Signing Works

When you run `ryeos sign <ref>`:

1. The engine resolves the item by canonical ref
2. The file content is hashed (SHA-256)
3. The hash is signed with the operator's Ed25519 private key
4. A signature header is prepended to the file:

```yaml
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

The trust store lives in `config/keys/trusted/` and contains Ed25519
public keys indexed by fingerprint:

```toml
version = "1.0.0"
fingerprint = "741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea"
owner = "ryeos-dev"
attestation = ""

[public_key]
pem = "MCowBQYDK2VwAyEA..."
```

Bundles ship their author key in the trust store. When a bundle is
installed, its key is added to the daemon's trusted keys.

## The Node Key

Each daemon instance has its own **node key pair** used for:
- Signing node-internal items (verbs, aliases, routes)
- Authenticating HTTP API requests
- Establishing daemon identity

The node key is generated at `ryeos init` time and stored in the
daemon's state directory. It is separate from the operator's signing
key.

## System Space Is Immutable

Bundle items (system space) cannot be signed with the node key — they
carry the author's signature. If you need to customize a system item,
copy it to project or user space first, then sign with your key.

## Key Fingerprints

The key fingerprint is the SHA-256 hash of the public key bytes,
hex-encoded. It appears in:
- Signature headers (`keyfp=...`)
- Trust store filenames (`<fingerprint>.toml`)
- `ryeos identity public-key` output
