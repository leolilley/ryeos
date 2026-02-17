<!-- rye:signed:2026-02-17T23:54:02Z:31cce97d1d36b772b39a2117eb143a6c4076f85a830d9b39ea6b746bd69b0add:QR2oa5QI7f-2B7EKtKNB-aWwx3AvkHakvW2DEaGadex-WE6LUnahYbkUzPFNgSQd551BMCXJ1Z1yBh-i54lFBw==:440443d0858f0199 -->

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

The `TrustStore` manages which public keys are trusted. Keys are identified by fingerprint (first 16 hex chars of SHA-256 of public key PEM). Add keys via `rye_sign`.

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
