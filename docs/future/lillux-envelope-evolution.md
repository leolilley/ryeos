```yaml
id: lillux-envelope-evolution
title: "Lillux Envelope Evolution"
description: Future directions for the sealed envelope system — HPKE formalization, sender authentication, multi-recipient, and validation unification
category: future
tags: [lillux, envelope, crypto, identity, hpke]
version: "0.1.0"
status: exploratory
```

# Lillux Envelope Evolution

> **Status:** Exploratory — the current v1 envelope format is working and shipped. These are directions to consider if the format becomes a stable cross-language protocol or crosses more hostile trust boundaries.

## Current State (v1)

The sealed envelope system lives entirely in Lillux's Identity primitive. It uses a "HPKE-lite" construction:

- **Key agreement:** X25519 ephemeral Diffie-Hellman
- **Key derivation:** HKDF-SHA256 with `info = b"execution-secrets/v1"`
- **Encryption:** ChaCha20Poly1305 AEAD with zero nonce (safe because the derived key is single-use)
- **AAD:** Canonical JSON of `{ kind, recipient }` — binds envelope purpose and target
- **Validation:** Rust is the strict implementation; Python opener is looser

The Rust implementation handles: `seal`, `open`, `validate`, `inspect`. Seal rejects unsafe env names at seal-time. Open verifies the recipient fingerprint. All operations are available as both CLI commands and a typed library API.

## Future Directions

### 1. Formal HPKE Adoption

**When:** If envelopes become a public/stable cross-language protocol or are consumed by independent implementations.

The current construction is a manual assembly of the same primitives that [RFC 9180 (HPKE)](https://www.rfc-editor.org/rfc/rfc9180) standardizes. Moving to a formal HPKE mode would:

- Provide a well-analyzed, peer-reviewed construction instead of hand-rolled crypto
- Use HPKE's `DHKEM(X25519, HKDF-SHA256)` + `ChaCha20Poly1305` (mode_base) which is functionally equivalent to what we do now
- Give us a standard `enc` format and key schedule
- Make the construction legible to external auditors

**Migration path:** The wire format would change (new version field), but the key material (X25519 keypairs) is reusable. A v2 envelope could coexist with v1 via version dispatch in `open`.

### 2. Extended KDF Context Binding

**When:** If envelopes cross more hostile or internet-exposed boundaries.

Currently the KDF info string is just `b"execution-secrets/v1"`. Additional context could be bound into the derivation:

- **Sender identity** — if we add sender authentication (see below)
- **Timestamp / nonce** — to prevent replay of old envelopes
- **Envelope purpose** — beyond just `kind`, e.g. execution ID, node ID
- **Key rotation epoch** — to enforce key freshness

This is essentially moving from HPKE Base mode to Auth or AuthPSK mode, or adding an application-level context binding.

### 3. Sender Authentication

**When:** If we need to prove _who_ sealed an envelope, not just _to whom_.

Currently envelopes are anonymous — anyone with the recipient's public key can seal. This is fine for the current use case (client seals secrets for a known node), but doesn't prove sender identity.

Options:

- **HPKE Auth mode** — sender uses their static X25519 key in the DH, providing implicit sender authentication
- **Sign-then-encrypt** — Ed25519 signature over the plaintext, then sealed. Simpler but adds a signature to every envelope
- **Signed envelope wrapper** — the outer envelope is signed by the sender's Ed25519 key, inner envelope is the current sealed format

### 4. Multi-Recipient Envelopes

**When:** If secrets need to reach multiple nodes simultaneously (e.g. cluster deployments).

Currently each envelope targets exactly one recipient. Multi-recipient would:

- Generate one ephemeral key and symmetric key
- Encrypt the symmetric key separately to each recipient's X25519 public key
- Include multiple `enc` entries (one per recipient)
- Single ciphertext, multiple key encapsulations

This is essentially HPKE's multi-recipient extension or a simplified version of age's multi-recipient format.

### 5. Validation Unification

**When:** Soon — this is the most practical near-term improvement.

Currently validation logic exists in two places:

| Location                           | Input type                 | What it checks                      |
| ---------------------------------- | -------------------------- | ----------------------------------- |
| `validate_env_map()` (private)     | `serde_json::Map`          | sizes, NUL bytes, non-string values |
| `validate_envelope_env()` (public) | `BTreeMap<String, String>` | sizes, NUL bytes, unsafe names      |

The constants (`MAX_VARIABLE_COUNT`, `MAX_VALUE_LENGTH`, `MAX_TOTAL_ENV_BYTES`, `RESERVED_ENV_NAMES`, `RESERVED_ENV_PREFIXES`) are shared, but the check paths could drift. Additionally, Python's `sealed_envelope.py` has its own copy of these constants.

**Target state:** One canonical validation path in Rust, called by both `seal` and `open`. Python callers route through `lillux identity envelope validate` instead of maintaining their own validation code. Constants live in exactly one place.

### 6. Retire the Python Crypto Path

**When:** As part of the broader Python deprecation effort.

Currently `ryeos/rye/primitives/sealed_envelope.py` contains:

- `seal_secrets()` — Python-native sealing (uses `cryptography` library)
- `open_envelope()` — Python-native opening (alternative to shelling out to Lillux)
- `validate_env_map()` — duplicated validation
- `seal_secrets_for_identity()` — identity-doc-based sealing

All of these now have Rust equivalents in Lillux. The migration path:

1. Route `seal_secrets()` callers through `lillux identity envelope seal`
2. Route `open_envelope()` callers through `lillux identity envelope open` (already done for server-side via `decrypt_envelope()`)
3. Remove the `cryptography` library dependency from this path
4. Keep Python wrappers as thin subprocess calls to Lillux

### 7. Nonce-Explicit AEAD

**When:** Only if the single-use key invariant becomes hard to maintain.

The current zero-nonce approach is safe because each envelope generates a fresh ephemeral key, producing a unique derived symmetric key. If we ever reuse a symmetric key across multiple encryptions (e.g. streaming envelope chunks), we'd need explicit nonces.

This is unlikely for the current use case but would matter if envelopes evolve beyond single-shot secret injection.

## Non-Goals

- **General-purpose encryption** — envelopes are specifically for subprocess secret injection, not a generic crypto API
- **Key management** — key generation, storage, and rotation stay in the Identity primitive's `keypair` subsystem
- **Transport security** — envelopes assume a transport layer (HTTPS, mTLS) handles in-flight protection; they provide at-rest confidentiality and recipient binding
