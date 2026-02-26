```yaml
id: integrity-and-signing
title: "Integrity and Signing"
description: Content hashing, Ed25519 signatures, and lockfile-based pinning
category: internals
tags: [integrity, signing, security, lockfiles, ed25519]
version: "1.0.0"
```

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

**JavaScript and TypeScript files:**

```
// rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
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

For Lillux-level integrity (used in lockfiles and bundle manifests), `lillux/primitives/integrity.py` provides a generic `compute_integrity(data)` function. Lillux is type-agnostic — callers construct the data dict with whatever fields are relevant:

```python
from lillux.primitives.integrity import compute_integrity

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

Signing uses the Ed25519 algorithm via `lillux/primitives/signing.py`:

### Key Generation

```python
from lillux.primitives.signing import generate_keypair, save_keypair
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
from lillux.primitives.signing import sign_hash, compute_key_fingerprint

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
from lillux.primitives.signing import verify_signature

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

The `TrustStore` manages which Ed25519 public keys are trusted for signature verification. Trusted keys are TOML identity documents at `.ai/trusted_keys/{fingerprint}.toml`. There are no exceptions — every item that executes must pass signature verification against a trusted key, including Rye's own system tools.

### Zero Exceptions

Rye ships pre-signed by its author, Leo Lilley. The system bundle includes the author's public key as a TOML identity document at `.ai/trusted_keys/{fingerprint}.toml`, and every system item — every directive, tool, runtime, parser, extractor, and knowledge entry — is signed with this key. When you install Rye, you are trusting Leo Lilley's signing key. The same key is used for registry publishing.

There is no bypass for system items. The verification flow is identical whether the item lives in project space, user space, or system space.

### Identity Document Format

```toml
# rye:signed:2026-02-14T00:27:54Z:a1b2c3d4...:WOclUqjr...:9fbfabe975fa5a7f
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

### Trusted Key Integrity

Trusted key files are themselves signed, using TOML `#` comment syntax on line 1:

```
# rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
```

This ensures the trust store cannot be silently tampered with.

**Write path:** `TrustStore.add_key()` signs the `.toml` file on write using the caller's Ed25519 keypair. The signature covers all content below line 1 (fingerprint, owner, attestation, and the PEM block).

**Read path:** `TrustStore.get_key()` verifies integrity on load. Unsigned files produce a debug-level warning but are still loaded (for backward compatibility). Files where the content hash or Ed25519 signature do not match are rejected with an `IntegrityError`.

**Self-signed keys:** When the signing key fingerprint matches the file's key fingerprint (i.e., the key signs itself), verification uses the PEM embedded in the file itself. This handles the bootstrap case — the bundle author's key in the system bundle signs itself, so no external key is needed to verify it.

**Cross-signed keys:** When the signing fingerprint differs from the file's fingerprint, `TrustStore` performs a bounded recursive lookup to resolve the signing key. A recursion guard prevents infinite loops (e.g., two keys that each claim to be signed by the other).

**Bundler integration:** The bundler collects `trusted_keys/` files into bundle manifests alongside directives, tools, and knowledge entries. Each key file's SHA256 hash is recorded in the manifest for independent verification during bundle extraction.

### Key Sources

| Source | How Trusted | Scope |
| --- | --- | --- |
| Own key | Auto-trusted on first keygen (`owner="local"`) | Signs your project/user items |
| Bundle author key | Shipped as self-signed `.toml` in system bundle, resolved via 3-tier lookup | Verifies system items |
| Registry key | TOFU-pinned on first pull (`owner="rye-registry"`) | Verifies registry-pulled items |
| Peer key | Manually trusted via `rye_sign` | Verifies collaborator items |

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
      "executor_id": "rye/core/runtimes/python/script",
      "integrity": "a1b2c3d4e5f6..."
    },
    {
      "item_id": "rye/core/runtimes/python/script",
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

## Transcript Integrity

Thread transcripts (JSONL event logs) are signed at turn boundaries using inline checkpoint events. This provides crash-resilient integrity — partial transcripts are still verifiable up to the last checkpoint.

### Checkpoint Signing

`TranscriptSigner` appends checkpoint events to the same JSONL stream as all other events:

```python
signer = TranscriptSigner(thread_id, thread_dir)
signer.checkpoint(turn=3)
# Appends: {"event_type": "checkpoint", "payload": {"turn": 3, "byte_offset": ..., "hash": ..., "sig": ..., "fp": ...}}
```

Each checkpoint:
1. Reads all bytes of `transcript.jsonl` up to the current file size
2. Computes SHA256 of those bytes
3. Signs the hash with Ed25519
4. Appends a checkpoint event with the hash, signature, byte offset, and key fingerprint

The runner calls `signer.checkpoint()` at the start of each turn (after the first) and at finalization.

### Checkpoint Verification

```python
result = signer.verify()
# {"valid": True, "checkpoints": 5}
# or {"valid": False, "error": "Content hash mismatch at turn 3", "failed_at_turn": 3}
```

Verification reads the JSONL, extracts all checkpoint events, and for each one:
1. Computes SHA256 of file content up to `byte_offset`
2. Compares to the stored hash
3. Verifies the Ed25519 signature against the trust store
4. Checks for unsigned trailing content after the last checkpoint

The `transcript_integrity` setting in `coordination.yaml` controls strictness:
- `strict` (default): Refuses on any integrity failure, including unsigned trailing events
- `lenient`: Allows unsigned trailing events with a warning

### thread.json Signing

Thread metadata files use a `_signature` field with canonical JSON serialization:

```python
from transcript_signer import sign_json, verify_json

data = {"thread_id": "...", "status": "running", "limits": {...}}
signed = sign_json(data)
# Adds: data["_signature"] = "rye:signed:TIMESTAMP:HASH:SIG:FP"

is_valid = verify_json(signed)  # True
```

The hash is computed over canonical JSON (sorted keys, compact separators) of all fields except `_signature`. This protects thread capabilities and limits from tampering.

### Knowledge Entry Export

Transcripts are also exported as signed knowledge entries at `.ai/knowledge/threads/{thread_id}.md`. These use cognition-style framing (`## > Turn N`, `### < Cognition`) and standard knowledge metadata with `entry_type: thread_transcript`. The knowledge entry is updated at each checkpoint and at finalization, replacing the legacy `transcript.md` file in the thread directory.

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
