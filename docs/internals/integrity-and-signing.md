---
id: integrity-and-signing
title: "Integrity and Signing"
description: Content hashing, Ed25519 signatures, and lockfile-based pinning
category: internals
tags: [integrity, signing, security, lockfiles, ed25519]
version: "1.0.0"
---

# Integrity and Signing

Every item in Rye OS is signed with Ed25519. Unsigned or tampered items are rejected at execution time. This page covers the signature format, verification flow, lockfile pinning, and trust management.

## Signature Format

Signatures are embedded as comments on the first line of every item file. The format depends on file type:

**Python and YAML files:**

```
# rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
```

**Markdown files (directives, knowledge):**

```
<!-- rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP -->
```

### Field Breakdown

| Field          | Format                                       | Example                |
| -------------- | -------------------------------------------- | ---------------------- |
| `TIMESTAMP`    | ISO 8601 UTC                                 | `2026-02-14T00:27:54Z` |
| `CONTENT_HASH` | SHA256 hex (64 chars)                        | `8e27c5f8c129cde3...`  |
| `ED25519_SIG`  | Base64url-encoded                            | `WOclUqjrz1dhuk6C...`  |
| `PUBKEY_FP`    | First 16 hex chars of SHA256(public_key_pem) | `440443d0858f0199`     |

### Registry Provenance

Items pushed through the registry get an additional suffix:

```
# rye:signed:TIMESTAMP:HASH:SIG:FP|registry@username
```

The `|provider@username` suffix records who published the item and through which registry.

## Content Hash Computation

The content hash is a SHA256 digest of the file content (including metadata but excluding the signature line itself).

```python
# From MetadataManager:
def compute_hash(item_type, content, file_path=None, project_path=None):
    # 1. Extract content portion (strip signature line)
    # 2. Compute SHA256 of remaining content
    return hashlib.sha256(content.encode()).hexdigest()
```

For Lilux-level integrity (used in lockfiles and bundle manifests), `lilux/primitives/integrity.py` provides a generic `compute_integrity(data)` function. Lilux is type-agnostic — callers construct the data dict with whatever fields are relevant:

```python
from lilux.primitives.integrity import compute_integrity

# Caller structures the dict for their item type:
tool_hash = compute_integrity({
    "tool_id": tool_id, "version": version,
    "manifest": manifest, "files": files,
})

directive_hash = compute_integrity({
    "directive_name": name, "version": version,
    "xml_content": xml_content, "metadata": metadata,
})
```

Canonical JSON serialization (sorted keys, no whitespace) ensures the same input always produces the same hash regardless of dict ordering or formatting.

## Ed25519 Signing

Signing uses the Ed25519 algorithm via `lilux/primitives/signing.py`:

### Key Generation

```python
from lilux.primitives.signing import generate_keypair, save_keypair
from rye.utils.path_utils import get_user_space, AI_DIR

private_pem, public_pem = generate_keypair()
key_dir = get_user_space() / AI_DIR / "keys"  # respects $USER_SPACE env var
save_keypair(private_pem, public_pem, key_dir=key_dir)
# → private_key.pem (mode 0600)
# → public_key.pem (mode 0644)
# → key directory (mode 0700)
```

### Signing Flow

```python
from lilux.primitives.signing import sign_hash, compute_key_fingerprint

# 1. Compute content hash
content_hash = MetadataManager.compute_hash(item_type, content)

# 2. Sign the hash with private key
signature = sign_hash(content_hash, private_key_pem)
# → Base64url-encoded Ed25519 signature

# 3. Compute key fingerprint
fingerprint = compute_key_fingerprint(public_key_pem)
# → First 16 hex chars of SHA256(public_key_pem)

# 4. Format signature line
# → "# rye:signed:2026-02-14T00:27:54Z:{content_hash}:{signature}:{fingerprint}"
```

### Verification Flow

```python
from lilux.primitives.signing import verify_signature

# Verify Ed25519 signature
is_valid = verify_signature(content_hash, signature_b64, public_key_pem)
# → True if signature matches, False otherwise
```

## Verification on Execute and Load

`verify_item()` in `rye/utils/integrity.py` is the single entry point for all integrity checks. It runs automatically on every `execute` and `load` call.

### Verification Steps

```python
def verify_item(file_path, item_type, project_path=None):
    content = file_path.read_text()

    # 1. Extract signature from first line
    sig_info = MetadataManager.get_signature_info(item_type, content)
    if not sig_info:
        raise IntegrityError(f"Unsigned item: {file_path}")

    # 2. Verify content hash
    expected = sig_info["hash"]
    actual = MetadataManager.compute_hash(item_type, content)
    if actual != expected:
        raise IntegrityError(f"Integrity failed: expected {expected}, got {actual}")

    # 3. Verify Ed25519 signature
    # 4. Check trust store for the signing key
    trust_store = TrustStore()
    public_key_pem = trust_store.get_key(sig_info["pubkey_fp"])
    if public_key_pem is None:
        raise IntegrityError(f"Untrusted key {sig_info['pubkey_fp']}")

    if not verify_signature(expected, sig_info["ed25519_sig"], public_key_pem):
        raise IntegrityError("Ed25519 signature verification failed")

    return actual  # verified hash
```

### What Triggers IntegrityError

| Condition                 | Error                                     |
| ------------------------- | ----------------------------------------- |
| No signature line         | `Unsigned item: {path}`                   |
| Content hash mismatch     | `Integrity failed: expected {X}, got {Y}` |
| Unknown key fingerprint   | `Untrusted key {fingerprint}`             |
| Invalid Ed25519 signature | `Ed25519 signature verification failed`   |

## Trust Store

The `TrustStore` manages which public keys are trusted for signature verification. Keys are identified by their fingerprint (first 16 hex chars of SHA256 of the public key PEM).

To trust a new key, add it via the `sign` MCP tool. The trust store allows the system to verify items signed by different authors or the registry.

## Lockfile Pinning

Lockfiles pin exact versions of an entire executor chain. They are checked **before** chain building during execution.

### Lockfile Structure

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

### Lockfile Resolution

`LockfileResolver` uses three-tier precedence for reading lockfiles:

```
Read:  project/.ai/lockfiles/ → user/.ai/lockfiles/ → system/.ai/lockfiles/
Write: Always to project space (if available), else user space
```

System lockfiles are read-only (bundled with the package).

### Lockfile Verification Flow

During `PrimitiveExecutor.execute()`:

1. Look up lockfile via `LockfileResolver.get_lockfile(item_id, version)`
2. If found, verify root integrity:
   - Compute current hash of the root tool file
   - Compare to `lockfile.root.integrity`
   - **Mismatch → execution fails** with "Re-sign and delete stale lockfile"
3. Verify each chain element:
   - Resolve each `item_id` to its current path
   - Compute current hash
   - Compare to pinned `integrity` value
   - **Any mismatch → execution fails**
4. If all checks pass, proceed with execution using the cached chain

### Lockfile Creation

After successful execution (when no lockfile existed), a new lockfile is automatically created:

```python
lockfile = lockfile_resolver.create_lockfile(
    tool_id=item_id,
    version=version,
    integrity=root_hash,
    resolved_chain=chain_dicts,
)
lockfile_resolver.save_lockfile(lockfile, space=chain[0].space)
```

## What Breaks Integrity

| Action                            | Result                                                                                |
| --------------------------------- | ------------------------------------------------------------------------------------- |
| Editing a file without re-signing | Content hash mismatch → `IntegrityError`                                              |
| Moving a file to a different path | Signature still valid (hash is content-based), but lockfile will fail if paths change |
| Deleting and recreating a file    | New hash won't match lockfile → must delete lockfile                                  |
| Using a key not in trust store    | `Untrusted key` error                                                                 |

## Batch Signing

The `sign` MCP tool supports batch signing via glob patterns:

```
sign(item_type="tool", glob_pattern="rye/**/*.py")
```

This re-signs all matching files with the current Ed25519 key, updating the signature line with fresh timestamp and hash. Used after bulk edits or when onboarding a new signing key.

## Dependency Verification

The `verify_deps` config in runtimes extends integrity checking beyond the chain:

```yaml
verify_deps:
  enabled: true
  scope: anchor # anchor, tool_dir, tool_siblings, or tool_file
  recursive: true
  extensions: [".py", ".yaml", ".yml", ".json"]
  exclude_dirs: ["__pycache__", ".venv", "node_modules", ".git"]
```

When enabled, `_verify_tool_dependencies()` walks the anchor directory tree **before** subprocess spawn and calls `verify_item()` on every matching file. This catches tampered dependencies that aren't part of the executor chain itself.

Symlink escapes are detected: if a file's resolved path is outside the anchor directory, an `IntegrityError` is raised.
