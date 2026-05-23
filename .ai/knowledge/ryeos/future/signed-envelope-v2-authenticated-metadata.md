<!-- rye:signed:2026-05-23T08:22:20Z:bfc6b9211ec4b2bdd91940469348a6d56eb575e654167a88a7fd2b1aab9c7149:TLSei3NQ1ykNPh0u1zOTXHnd3dstwGj2GY1RUtmtFgfvOfWZslK3W1cffhm7hXn0FSidtfQEJTk7___EZ7N-Bg:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: signed-envelope-v2-authenticated-metadata
title: Signed Envelope V2 Authenticated Metadata
entry_type: design-note
version: "1.0.0"
author: amp
created_at: 2026-05-23T00:00:00Z
description: Future signature-envelope design that authenticates timestamp and signature metadata instead of signing only the content hash
tags:
  - signatures
  - lillux
  - signed-envelope
  - metadata-authentication
  - future-hardening
```

# Signed Envelope V2 Authenticated Metadata

## Purpose

This note records the advanced signature-envelope path that came up while diagnosing `bundle publish` timestamp churn.

The current fix should keep publish idempotent by preserving existing valid signatures and avoiding unnecessary strip/delete/re-sign cycles. That does not require changing signature semantics. However, the current signature format signs only the body content hash. The timestamp and other signature-line metadata are visible but not themselves authenticated.

If RyeOS later needs stronger audit or tamper-evidence semantics for signature metadata, introduce a new signed-envelope version instead of overloading the current format.

## Current behavior

The existing signature line is shaped like:

```text
<prefix> ryeos:signed:<timestamp>:<content_hash>:<signature_b64>:<signer_fingerprint> <suffix?>
```

The signature covers the content hash:

```text
signature = Ed25519.sign(content_hash)
```

Implications:

- unchanged body content produces the same content hash;
- unchanged body content produces the same signature bytes;
- a re-sign of unchanged body content can still produce a different timestamp;
- timestamp and fingerprint are not part of the signed payload.

That behavior is acceptable for current publish idempotence once the pipeline stops re-signing unchanged files. It is not a full authenticated-metadata envelope.

## Trigger for this future path

Consider this design when one or more of these become requirements:

- the signature timestamp must be tamper-evident;
- the signer fingerprint or key id must be covered by the signature payload;
- audit history needs to distinguish original signing time from later validation time;
- signature rotation needs explicit envelope-version semantics;
- multiple signers or threshold signatures are introduced;
- bundle publication needs stronger provenance claims than “this body hash validates under this key”.

Do not implement this just to fix timestamp-only publish diffs. The smaller publish fix is to make signing idempotent and stop stripping/deleting signatures before they can be validated.

## Proposed V2 payload

Introduce a canonical payload that signs both the body hash and normalized metadata.

Example payload:

```text
ryeos:signed:v2
timestamp:<rfc3339_utc>
content_hash:<sha256_hex_body_hash>
signer_fingerprint:<fingerprint>
algorithm:ed25519-sha256
scope:<optional_scope_or_empty>
```

Then compute:

```text
signature = Ed25519.sign(sha256(canonical_payload))
```

or, if the crypto layer already standardizes signing pre-hashed content:

```text
payload_hash = sha256(canonical_payload)
signature = Ed25519.sign(payload_hash)
```

The exact choice should match the rest of `lillux` signing conventions, but the key property is that the timestamp and metadata are included in the canonical payload.

## Compatibility approach

Support both formats during migration:

```text
ryeos:signed:...       # v1, signs content hash only
ryeos:signed:v2:...    # v2, signs canonical metadata payload
```

Verifier behavior:

1. Parse the signature line.
2. Detect the envelope version.
3. For v1, verify the existing content-hash-only payload.
4. For v2, reconstruct the canonical metadata payload and verify it.
5. Preserve existing v1 signatures unless a command explicitly upgrades or re-signs.

Avoid an implicit rewrite of every valid v1 signature during normal publish or validation. Otherwise the V2 migration would recreate the timestamp churn problem at a larger scale.

## Design constraints

- Canonicalization must be byte-stable across platforms and Rust versions.
- RFC3339 timestamps should be normalized to UTC with a single accepted representation.
- Metadata key ordering must be fixed.
- Unknown metadata fields should either be rejected or included in a canonical extension block; do not silently ignore signed-looking fields.
- Prefix/suffix handling for YAML, Markdown, and other comment styles must remain unambiguous.
- Parsing should avoid delimiter ambiguity; prefer a structured encoded payload if colon-separated fields become brittle.

## Possible line format

If a single-line envelope remains desirable, use an encoded canonical payload rather than piling on colon-separated fields:

```text
<prefix> ryeos:signed:v2:<payload_b64url_no_pad>:<signature_b64url_no_pad> <suffix?>
```

Where `payload_b64url_no_pad` decodes to the canonical metadata payload. This keeps parsing simple and avoids edge cases where timestamps or future metadata contain delimiters.

## Migration sketch

1. Add V2 parser and verifier alongside the existing V1 parser.
2. Add tests that verify V1 and V2 fixtures.
3. Add a focused `resign --format v2` or equivalent explicit upgrade path.
4. Keep normal validation idempotent for both V1 and V2.
5. Only consider default V2 emission after all bundle and item paths can read both versions.

## Non-goals

- Do not make this part of the immediate `bundle publish` idempotence fix.
- Do not invalidate existing V1 signatures by default.
- Do not rewrite all repository signatures merely to upgrade the format.
- Do not introduce multiple signers until a separate trust and policy model exists.
