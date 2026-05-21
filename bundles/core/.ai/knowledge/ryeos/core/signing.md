
---
category: ryeos/core
tags: [fundamentals, signing, trust, integrity, compilation]
version: "3.0.0"
description: >
  How signing and trust work in Rye OS — Ed25519 signatures,
  content hashing, path anchoring, the trust store, the signed
  envelope format, and signing as a compilation step.
---

# Signing and Trust

Rye OS uses Ed25519 cryptographic signatures to ensure item integrity.
Every executable item must be signed before the daemon will run it.

## Three Keys, Three Roles

| Key | Purpose | Location |
|-----|---------|----------|
| **User key** | Operator identity — CLI auth, signing user/project items | `~/.ryeos/.ai/config/keys/signing/private_key.pem` |
| **Node key** | Daemon identity — bundle registrations, node config | `<system>/.ai/node/identity/private_key.pem` |
| **Publisher key** | Bundle author identity — signs items in published bundles | Pinned in `~/.ryeos/.ai/config/keys/trusted/` |

The user key and node key are generated at `ryeos init` time. The
publisher key is **hardcoded in the binary** and pinned during init
without trusting any on-disk file. For development, `--trust-file`
pins a dev publisher key instead.

## The Signed Envelope Format

Every item carries a signature in a comment header. The format is
four colon-delimited fields:

```
```

| Field | Format | Example |
|---|---|---|
| `timestamp` | ISO 8601 UTC | `2026-05-20T05:57:09Z` |
| `content_hash` | SHA-256 hex (64 chars) | `fb60141f8b9e...` |
| `signature` | Ed25519 base64url-padded | `mjlXVP85DZLrMf...` |
| `fingerprint` | SHA-256 of the public key (64 chars) | `741a8bc609b3...` |

The envelope prefix and suffix vary by file type:

| File type | Prefix | Suffix |
|---|---|---|
| YAML (`*.yaml`) | `#` | (none) |
| Markdown (`*.md`) | `<!--` | `-->` |
| Other | Configurable via `SignatureEnvelope` | (none) |

### Concrete examples

YAML:
```
```

Markdown:
```
```

The Ed25519 signature covers `SHA256(body)` — the hash bytes, not the
body bytes directly. Verification accepts standard base64, URL-safe-no-pad
base64, and URL-safe-padded base64 for the signature.

## How Signing Works

When you run `ryeos sign <ref>`:

1. The engine resolves the item by canonical ref
2. The file content (below the signature line) is hashed (SHA-256)
3. The hash is signed with the operator's Ed25519 private key
4. The signature line is written as the first line of the file

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

Unsigned or modified items produce an error with a clear
message telling you exactly what to fix.

## Trust Store

The trust store lives in `~/.ryeos/.ai/config/keys/trusted/` and contains
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

## Signing as Compilation

The signing pipeline gives you properties analogous to a traditional
compiler, plus several that a compiler normally cannot provide.

### What a compiler gives you

| Property | Compiler | Signing Pipeline |
|---|---|---|
| **Integrity** | Compiled binary matches source | Content hash verified at every load |
| **Provenance** | (Usually absent) | Ed25519 signature identifies the author |
| **Trust gating** | (Link-time, implicit) | Trust store checked — untrusted keys rejected |
| **Structural validation** | Type checking | `deny_unknown_fields` YAML deserialization |
| **Cross-reference validation** | Missing symbol errors | Boot validator walks the registry for dangling refs |
| **Immutability** | Binary is frozen | Content-addressed hash locks the content |

### What makes it stronger than traditional compilation

1. **It runs every time** — A compiler runs once. The signing pipeline
   verifies every item at every daemon startup and at every request
   resolution. There is no stale compiled binary.

2. **It applies to configuration, not just code** — Routes, verbs,
   aliases, kind schemas, parsers, handlers, trust pins, and service
   endpoints are all verified. A misconfigured route can't sneak in any
   more than a syntax error can.

3. **It spans the trust boundary** — A compiler trusts its own standard
   library. The signing pipeline explicitly does NOT trust bundles —
   they are verified against the operator's trust store. The "standard
   library" is subject to the same verification as any extension.

4. **It composes across trust levels** — The weakest-link trust fold
   across extends chains means an untrusted ancestor taints the entire
   chain. A system-signed binary reached through a user-tier descriptor
   is capped at TrustedUser. This is dependency trust transitivity,
   computed dynamically per item.

5. **It is incremental by design** — Content addressing means only
   changed files need re-verification. But every load is still fully
   verified — there is no caching of trust decisions.

### What it is NOT

The signing pipeline is not static analysis in the program verification
sense. It cannot tell you "this tool will always terminate" or "this
directive will never call an API you don't have scope for." Runtime
capability enforcement handles behavioral correctness. The signing
pipeline is about integrity, provenance, and structural validity.
