```yaml
id: trust-model
title: "Cryptographic Trust Model"
description: End-to-end integrity verification for Rye OS items — Ed25519 signing, trust store, TOFU pinning, and registry provenance
category: registry
tags: [registry, security, signing, ed25519, trust, integrity, tofu]
version: "1.0.0"
```

# Cryptographic Trust Model

Rye OS uses Ed25519 digital signatures to guarantee that every item (directive, tool, knowledge) is authentic and untampered before execution. This document covers the full trust chain from local signing through registry distribution to runtime verification.

## Ed25519 Signing

All cryptographic primitives live in `lillux/kernel/lillux/primitives/signing.py`. Rye uses the `cryptography` library's Ed25519 implementation — no custom crypto.

### Keypair Generation

When a user first runs the `rye_sign` tool, Ed25519 keys are generated and stored:

```
~/.ai/keys/
├── private_key.pem   # Ed25519 private key (mode 0600)
└── public_key.pem    # Ed25519 public key (mode 0644)
```

The key directory itself is set to mode `0700`. Keys are generated via `Ed25519PrivateKey.generate()` and serialized to PEM format (PKCS8 for private, SubjectPublicKeyInfo for public).

### How Signing Works

The sign tool produces a signature comment embedded in the item file:

```
rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
```

| Field          | Value                                                   |
| -------------- | ------------------------------------------------------- |
| `TIMESTAMP`    | ISO 8601 UTC timestamp (e.g., `2026-02-14T00:44:49Z`)   |
| `CONTENT_HASH` | SHA256 hex digest of normalized content                 |
| `ED25519_SIG`  | Base64url-encoded Ed25519 signature of the content hash |
| `PUBKEY_FP`    | First 16 hex characters of SHA256(public_key_pem)       |

The signature is placed as a comment on line 1, using the file type's comment syntax:

| File Type             | Format                                      |
| --------------------- | ------------------------------------------- |
| Markdown (directives) | `<!-- rye:signed:TIMESTAMP:HASH:SIG:FP -->` |
| Python (tools)        | `# rye:signed:TIMESTAMP:HASH:SIG:FP`        |
| YAML (tools/configs)  | `# rye:signed:TIMESTAMP:HASH:SIG:FP`        |

### Signing Steps

1. The `MetadataManager` extracts the content to hash (strips existing signature if present)
2. SHA256 of the normalized content is computed → `content_hash`
3. `sign_hash(content_hash, private_key_pem)` signs the hash with Ed25519, returning a base64url-encoded signature
4. `compute_key_fingerprint(public_key_pem)` produces a 16-char hex fingerprint
5. The formatted signature comment is inserted at line 1 of the file

```python
# From lillux/kernel/lillux/primitives/signing.py
def sign_hash(content_hash: str, private_key_pem: bytes) -> str:
    private_key = serialization.load_pem_private_key(private_key_pem, password=None)
    signature = private_key.sign(content_hash.encode("utf-8"))
    return base64.urlsafe_b64encode(signature).decode("ascii")
```

## Trust Store

**Implementation:** `ryeos/rye/utils/trust_store.py`

The trust store manages which Ed25519 public keys are trusted for signature verification. Every item in Rye — including Rye's own system tools — must pass signature verification against a trusted key. There are no exceptions.

### Zero Exceptions

Rye ships pre-signed by its author, Leo Lilley. The system bundle includes the author's public key as a TOML identity document at `.ai/trusted_keys/{fingerprint}.toml`, and every system item is signed with this key. When you install Rye, you are trusting Leo Lilley's signing key — the same key used for registry publishing. There is no bypass for system items. The verification flow is identical regardless of which space the item lives in.

### Identity Document Format

Trusted keys are TOML identity documents that bind a key to an owner:

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

| Field         | Description                                          |
| ------------- | ---------------------------------------------------- |
| `fingerprint` | First 16 hex chars of SHA256(public_key_pem)         |
| `owner`       | Registry username or `"local"` for self-generated    |
| `attestation` | Registry attestation signature (optional)            |
| `pem`         | Ed25519 public key in PEM format                     |

### 3-Tier Resolution

The trust store uses the same 3-tier resolution as directives, tools, and knowledge:

```
project/.ai/trusted_keys/{fingerprint}.toml  →  (highest priority)
user/.ai/trusted_keys/{fingerprint}.toml     →
system/.ai/trusted_keys/{fingerprint}.toml   →  (lowest priority, shipped with bundle)
```

First match wins. The system bundle ships the author's key at `rye/.ai/trusted_keys/{fingerprint}.toml` — it is resolved automatically via the standard 3-tier lookup, with no special bootstrap logic.

### Key Operations

| Operation             | Method                        | Behavior                                                                    |
| --------------------- | ----------------------------- | --------------------------------------------------------------------------- |
| **Check trust**       | `is_trusted(fingerprint)`     | Delegates to `get_key()`, returns True if key found                         |
| **Get key**           | `get_key(fingerprint)`        | 3-tier search: project → user → system `.ai/trusted_keys/{fp}.toml`        |
| **Add key**           | `add_key(public_key_pem)`     | Writes `{fingerprint}.toml` identity document, returns fingerprint          |
| **Remove key**        | `remove_key(fingerprint)`     | Deletes `{fingerprint}.toml` from user store                               |
| **Pin registry**      | `pin_registry_key(pem)`       | Adds key with `owner="rye-registry"` (no-op if already exists)             |
| **Get registry key**  | `get_registry_key()`          | Scans all keys for `owner=="rye-registry"`                                  |
| **List keys**         | `list_keys()`                 | Returns all `.toml` identity documents across all spaces                    |

The user's own public key is automatically added to the trust store when keys are first generated (with `owner="local"`).

## TOFU (Trust On First Use)

When an agent pulls an item from the registry for the first time and no registry key is pinned yet, the client performs Trust On First Use:

1. Pull request returns signed content with a `pubkey_fp` in the signature
2. Client checks `TrustStore.get_registry_key()` → returns `None` (no pinned key)
3. Client fetches `GET {REGISTRY_API_URL}/v1/public-key` → receives Ed25519 public key PEM
4. Client calls `TrustStore.pin_registry_key(registry_key_pem)` → writes identity document with `owner="rye-registry"`
5. All subsequent pulls verify against this pinned key

```python
# From registry.py pull flow
if registry_key is None:
    # TOFU: fetch and pin registry key on first pull
    key_url = f"{REGISTRY_API_URL}/v1/public-key"
    req = urllib.request.Request(key_url)
    with urllib.request.urlopen(req, timeout=10) as resp:
        registry_key = resp.read()
    trust_store.pin_registry_key(registry_key)
```

The `pin_registry_key` method is a **no-op if the same fingerprint already exists** — once pinned, the registry key cannot be silently replaced. This prevents key substitution attacks after initial trust establishment. The registry key is stored as a normal trusted key identity document with `owner="rye-registry"`.

## Registry Signing

**Implementation:** `services/registry-api/registry_api/validation.py`

When items are pushed to the registry, the server performs its own signing:

### Push Flow (Server-Side)

1. **Strip client signature** — `strip_signature(content, item_type)` removes any existing `rye:signed:` comment
2. **Validate content** — `validate_content()` parses and validates using the same rye validators as the client
3. **Registry signing** — `sign_with_registry(content_clean, item_type, username)`:
   - Extracts content for hashing via the item type's `MetadataManager` strategy
   - Computes SHA256 content hash
   - Loads the registry's Ed25519 keypair from `REGISTRY_KEY_DIR` (default: `/etc/rye-registry/keys/`)
   - Signs the hash with the registry's private key
   - Appends `|rye-registry@{username}` provenance suffix to the signature

### Registry Signature Format

```
rye:signed:TIMESTAMP:HASH:SIG:FP|rye-registry@username
```

The `|rye-registry@username` provenance suffix identifies:

- That this item was signed by the registry (not just a local user)
- Which user pushed it (the `username`)

For markdown items (HTML comment syntax), the provenance is inserted before the closing `-->`:

```html
<!-- rye:signed:2026-02-14T00:44:49Z:a66665d3...:bd7edl...:440443d0|rye-registry@leolilley -->
```

### Registry Keypair

The registry server maintains its own Ed25519 keypair at `REGISTRY_KEY_DIR`. The keypair is generated on first use via `ensure_keypair()`. The public key is exposed at `GET /v1/public-key` for client-side TOFU pinning.

## Integrity Verification on Execute

**Implementation:** `ryeos/rye/utils/integrity.py`

Before any tool is executed, the integrity system performs four checks:

```python
def verify_item(file_path, item_type, *, project_path=None) -> str:
    # 1. Signature exists
    sig_info = MetadataManager.get_signature_info(item_type, content, ...)
    if not sig_info:
        raise IntegrityError(f"Unsigned item: {file_path}")

    # 2. Content hash matches
    actual = MetadataManager.compute_hash(item_type, content, ...)
    if actual != sig_info["hash"]:
        raise IntegrityError(f"Integrity failed: {file_path}")

    # 3. Ed25519 signature is valid
    if not verify_signature(expected, ed25519_sig, public_key_pem):
        raise IntegrityError(f"Ed25519 signature verification failed: {file_path}")

    # 4. Signing key is in trust store
    public_key_pem = trust_store.get_key(pubkey_fp)
    if public_key_pem is None:
        raise IntegrityError(f"Untrusted key {pubkey_fp} for {file_path}")
```

If any check fails, an `IntegrityError` is raised and execution is denied. There are no exceptions — system items go through the same verification as project and user items.

### What Triggers Verification

| Trigger              | Verified                                       |
| -------------------- | ---------------------------------------------- |
| `rye_execute` a tool | Every element in the executor chain            |
| Pull from registry   | Registry signature + content hash              |
| Bundle verify        | Manifest signature + per-file SHA256 hashes    |
| Lockfile check       | Root integrity hash + all chain element hashes |

## Chain Integrity

**Implementation:** `ryeos/rye/executor/primitive_executor.py`

The `PrimitiveExecutor` verifies **every element** in the execution chain before running any code:

```python
# Step 3 in PrimitiveExecutor.execute()
for element in chain:
    verify_item(
        element.path,
        ItemType.TOOL,
        project_path=self.project_path,
    )
```

A tool's execution chain includes the tool itself and all its runtime dependencies. For example, a Python tool's chain might be:

```
my-tool.py → python/script.yaml → subprocess.yaml
```

All three files must be signed and verified before `my-tool.py` executes.

### Lockfile Integrity

When a lockfile exists for a tool, the executor performs additional checks:

1. **Root integrity** — The lockfile's recorded hash for the root tool must match the current file's computed hash
2. **Chain element integrity** — Every chain element recorded in the lockfile must still exist and have the same hash

If any hash mismatches, execution fails with a message to re-sign and delete the stale lockfile.

### Dependency Verification

The `_verify_tool_dependencies` method can walk a tool's anchor directory tree, verifying every file matching configured extensions (e.g., `.py`) via `verify_item()`. This runs before subprocess spawn and catches tampered sibling files.

## Bundle Integrity

**Implementation:** `ryeos/rye/.ai/tools/rye/core/bundler/bundler.py`

Bundles group multiple items under a single signed manifest. The manifest itself is a YAML file with an inline `rye:signed:` signature on line 1.

### Manifest Structure

```yaml
# rye:signed:TIMESTAMP:HASH:SIG:FP
bundle:
  id: ryeos-core
  version: 1.0.0
  entrypoint: rye/core/create_directive
  description: Core Rye OS bundle
files:
  .ai/tools/rye/core/registry/registry.py:
    sha256: a66665d3ef686944...
    inline_signed: true
    item_type: tool
  .ai/directives/rye/core/create_directive.md:
    sha256: 7c8a91b2f3d40e...
    inline_signed: true
    item_type: directive
  .ai/tools/rye/core/bundler/collect.yaml:
    sha256: 3f2e1d0c9b8a...
    inline_signed: true
    item_type: tool
```

### Bundle Verification Flow

`validate_bundle_manifest()` performs:

1. **Manifest signature** — `verify_item(manifest_path, ItemType.TOOL)` checks the manifest's own Ed25519 signature
2. **Per-file hashes** — For each file in the manifest, compute `SHA256(file_content)` and compare to recorded hash
3. **Inline signatures** — If a file is marked `inline_signed: true`, also run `verify_item()` on it to verify its individual Ed25519 signature
4. **Missing files** — Any file listed in the manifest but not on disk is flagged

The bundle is valid only if the manifest signature passes, all files exist, and all hashes match.

## Threat Model & Known Gaps

### What's Protected

- **Tampered items** — SHA256 content hashing + Ed25519 signatures catch any modification
- **Unauthorized items** — Trust store rejects signatures from unknown keys
- **Registry impersonation** — TOFU pinning prevents key substitution after first pull
- **Chain attacks** — Every element in the executor chain is verified, not just the entry point
- **Bundle tampering** — Per-file hashes in signed manifests catch individual file modifications

### Known Limitations

| Gap                       | Description                                                                                                                                                                        |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Advisory permissions**  | Tool permissions declared in metadata are enforced at the Python level, not via OS-level sandboxing. A malicious tool with valid signature can still access the filesystem freely. |
| **No key revocation**     | There is no mechanism to revoke a compromised key. If a signing key is leaked, all items signed with it remain trusted until the key is manually removed from the trust store.     |
| **TOFU limitations**      | The registry key is trusted on first use. If the first connection is intercepted (MITM), a wrong key could be pinned. No out-of-band verification channel exists.                  |
| **No supply chain audit** | Supply-chain attack scenarios (e.g., compromised registry server re-signing malicious content) have not been formally tested or modeled.                                           |
| **Single registry key**   | The registry uses one keypair for all operations. Key rotation requires all clients to re-pin.                                                                                     |
| **No signature expiry**   | Signatures do not expire. A signed item remains trusted indefinitely regardless of when it was signed.                                                                             |
| **System key visibility**     | System bundle author keys are trusted by default via 3-tier resolution. Users implicitly trust any installed bundle's author key. |
