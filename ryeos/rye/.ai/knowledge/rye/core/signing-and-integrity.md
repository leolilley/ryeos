<!-- rye:signed:2026-02-22T23:37:08Z:93079dfe1ea072e100fb2833c185c18c597184fc44130eaf5f22b3697378904d:38TgeWa6wCpOROW3edlPT-EX7KNDkLy69mFWviuALk_dkUEe7m0SCKdeDGlwNVC3HUJv9Y1mLNf8sqaa_zamBw==:9fbfabe975fa5a7f -->

```yaml
id: signing-and-integrity
title: Signing & Integrity
entry_type: reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - signing
  - integrity
  - security
  - hashing
  - ed25519
  - lockfile
  - trust
  - verification
  - rye-sign
  - content-hash
references:
  - "docs/internals/integrity-and-signing.md"
```

# Signing & Integrity

Content hashing, Ed25519 signatures, lockfile pinning, and trust management.

## Signature Format

Signatures are embedded as comments on line 1 of every item file.

### By File Type

| File Type        | Format                                                         |
| ---------------- | -------------------------------------------------------------- |
| Python / YAML    | `# rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP`   |
| Markdown         | `<!-- rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP -->` |

### Field Breakdown

| Field          | Format                                     | Example                  |
| -------------- | ------------------------------------------ | ------------------------ |
| `TIMESTAMP`    | ISO 8601 UTC                               | `2026-02-14T00:27:54Z`   |
| `CONTENT_HASH` | SHA-256 hex (64 chars)                     | `8e27c5f8c129cde3...`    |
| `ED25519_SIG`  | Base64url-encoded                          | `WOclUqjrz1dhuk6C...`    |
| `PUBKEY_FP`    | First 16 hex chars of SHA-256(public_key_pem) | `440443d0858f0199`    |

### Registry Provenance

Items published through the registry get a suffix:

```
# rye:signed:TIMESTAMP:HASH:SIG:FP|registry@username
```

## Content Hash Computation

SHA-256 digest of the file content **excluding** the signature line itself.

```python
def compute_hash(item_type, content, file_path=None, project_path=None):
    # 1. Strip signature line from content
    # 2. SHA-256 of remaining content
    return hashlib.sha256(content.encode()).hexdigest()
```

Canonical JSON serialization (sorted keys, no whitespace) ensures deterministic hashing for structured data.

## Ed25519 Signing Flow

```python
from lilux.primitives.signing import generate_keypair, sign_hash, verify_signature

# 1. Generate keypair (one-time)
private_pem, public_pem = generate_keypair()
save_keypair(private_pem, public_pem, key_dir=~/.ai/keys/)
# → private_key.pem (mode 0600)
# → public_key.pem  (mode 0644)
# → key directory    (mode 0700)

# 2. Compute content hash
content_hash = MetadataManager.compute_hash(item_type, content)

# 3. Sign the hash
signature = sign_hash(content_hash, private_key_pem)  # → Base64url

# 4. Compute key fingerprint
fingerprint = compute_key_fingerprint(public_key_pem)  # → 16 hex chars

# 5. Format signature line
# → "# rye:signed:2026-02-14T00:27:54Z:{hash}:{sig}:{fp}"
```

## Verification Flow

`verify_item()` in `rye/utils/integrity.py` — runs on every `execute` and `load`:

```python
def verify_item(file_path, item_type, project_path=None):
    content = file_path.read_text()

    # 1. Extract signature from line 1
    sig_info = MetadataManager.get_signature_info(item_type, content)
    if not sig_info:
        raise IntegrityError(f"Unsigned item: {file_path}")

    # 2. Verify content hash
    actual = MetadataManager.compute_hash(item_type, content)
    if actual != sig_info["hash"]:
        raise IntegrityError(f"Integrity failed: expected {sig_info['hash']}, got {actual}")

    # 3. Verify Ed25519 signature against trust store
    public_key_pem = TrustStore().get_key(sig_info["pubkey_fp"])
    if not public_key_pem:
        raise IntegrityError(f"Untrusted key {sig_info['pubkey_fp']}")

    if not verify_signature(sig_info["hash"], sig_info["ed25519_sig"], public_key_pem):
        raise IntegrityError("Ed25519 signature verification failed")
```

## IntegrityError Conditions

| Condition                 | Error Message                             |
| ------------------------- | ----------------------------------------- |
| No signature line         | `Unsigned item: {path}`                   |
| Content hash mismatch     | `Integrity failed: expected {X}, got {Y}` |
| Unknown key fingerprint   | `Untrusted key {fingerprint}`             |
| Invalid Ed25519 signature | `Ed25519 signature verification failed`   |

## When Re-Signing Is Required

| Action                            | Needs Re-Sign? | Notes                                      |
| --------------------------------- | -------------- | ------------------------------------------ |
| Editing file content              | **Yes**        | Content hash will mismatch                 |
| Moving file to different path     | No             | Hash is content-based, not path-based      |
| Deleting and recreating           | **Yes**        | Lockfile will also need deletion           |
| Changing signing key              | **Yes**        | Old signature won't verify with new key    |

## Trust Store

The `TrustStore` manages which Ed25519 public keys are trusted for signature verification. Trusted keys are TOML identity documents at `.ai/trusted_keys/{fingerprint}.toml`. There are no exceptions — every item that executes must pass signature verification against a trusted key, including Rye's own system tools.

### Zero Exceptions Policy

Rye ships pre-signed by its author, Leo Lilley. The system bundle includes the author's public key as a TOML identity document at `.ai/trusted_keys/{fingerprint}.toml`, and every system item — every directive, tool, runtime, parser, extractor, and knowledge entry — is signed with this key. When you install Rye, you are trusting Leo Lilley's signing key. The same key is used for registry publishing.

There is no bypass for system items. There is no "trusted by default" escape hatch. The verification flow is identical whether the item lives in project space, user space, or system space:

1. Extract signature from line 1
2. Recompute content hash, compare to signed hash
3. Verify Ed25519 signature
4. Look up signing key in trust store — **reject if untrusted**

### Identity Document Format

```toml
# .ai/trusted_keys/{fingerprint}.toml
fingerprint = "bc8e267dadcce3a4"
owner = "leo"
attestation = ""

[public_key]
pem = """
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEA...
-----END PUBLIC KEY-----
"""
```

### 3-Tier Resolution

The trust store uses the same 3-tier resolution as directives, tools, and knowledge:

```
project/.ai/trusted_keys/{fingerprint}.toml  →  (highest priority)
user/.ai/trusted_keys/{fingerprint}.toml     →
system/.ai/trusted_keys/{fingerprint}.toml   →  (lowest priority, shipped with bundle)
```

First match wins. The system bundle ships the author's key at `rye/.ai/trusted_keys/{fingerprint}.toml` — it is resolved automatically via the standard 3-tier lookup, with no special bootstrap logic.

### Key Sources

| Source | How Trusted | Scope |
| --- | --- | --- |
| Own key | Auto-trusted on first keygen (`owner="local"`) | Signs your project/user items |
| Bundle author key | Shipped as `.toml` in system bundle, resolved via 3-tier lookup | Verifies system items |
| Registry key | TOFU-pinned on first pull (`owner="rye-registry"`) | Verifies registry-pulled items |
| Peer key | Manually trusted via `rye_sign` | Verifies collaborator items |

## Lockfile Format

Stored as `{tool_id}@{version}.lock.json`:

```json
{
  "lockfile_version": 1,
  "generated_at": "2026-02-15T12:00:00+00:00",
  "root": {
    "tool_id": "rye/bash/bash",
    "version": "1.0.0",
    "integrity": "a1b2c3d4e5f6..."
  },
  "resolved_chain": [
    {
      "item_id": "rye/bash/bash",
      "space": "system",
      "tool_type": "python",
      "executor_id": "rye/core/runtimes/python_script_runtime",
      "integrity": "a1b2c3d4e5f6..."
    },
    {
      "item_id": "rye/core/runtimes/python_script_runtime",
      "space": "system",
      "tool_type": "runtime",
      "executor_id": "rye/core/primitives/subprocess",
      "integrity": "f6e5d4c3b2a1..."
    }
  ]
}
```

## Lockfile Resolution

Three-tier precedence for reading:

```
Read:  project/.ai/lockfiles/ → user/.ai/lockfiles/ → system/.ai/lockfiles/
Write: Always to project space (if available), else user space
```

System lockfiles are read-only.

## Lockfile Verification Flow

During `PrimitiveExecutor.execute()`:

1. Look up lockfile via `LockfileResolver.get_lockfile(item_id, version)`
2. Verify root integrity (compute hash, compare to `lockfile.root.integrity`)
3. Verify each chain element (resolve path, compute hash, compare to pinned `integrity`)
4. **Any mismatch → execution fails** with "Re-sign and delete stale lockfile"
5. After successful execution (no lockfile existed), auto-create one

## Batch Signing

```
rye_sign(item_type="tool", glob_pattern="rye/**/*.py")
```

Re-signs all matching files with fresh timestamp and hash. Used after bulk edits or key rotation.

## Dependency Verification

Runtimes can enable `verify_deps` to walk the tool's anchor directory before execution:

```yaml
verify_deps:
  enabled: true
  scope: anchor    # anchor, tool_dir, tool_siblings, or tool_file
  recursive: true
  extensions: [".py", ".yaml", ".yml", ".json"]
  exclude_dirs: ["__pycache__", ".venv", "node_modules", ".git"]
```

Catches tampered dependencies outside the executor chain. Symlink escapes are detected and raise `IntegrityError`.
