<!-- rye:signed:2026-02-26T06:42:50Z:e4cc8eec79c6651ddbcadf46966c8f9448b3f214b055974a9fe7c0def01b4aa3:K-_dW7LyWlpfJir95PR3gFMFZu42O1fscgvKD_P0ET1I7esa4Rar2LN4LAX5eAS6dj_3yYYHjK99672ygmpABw==:4b987fd4e40303ac -->

```yaml
name: trust-model
title: Cryptographic Trust Model
entry_type: reference
category: rye/core/registry
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - trust
  - security
  - keys
  - signatures
  - registry
references:
  - registry-api
  - "docs/registry/trust-model.md"
```

# Cryptographic Trust Model

Ed25519 signing, key pinning, TOFU bootstrap, and verification for Rye OS items.

## Ed25519 Signing Primitives

**Implementation:** `lillux/lillux/primitives/signing.py` (uses `cryptography` library — no custom crypto).

### Keypair Storage

```
~/.ai/config/keys/signing/
├── private_key.pem   # Ed25519 private key (mode 0600)
└── public_key.pem    # Ed25519 public key (mode 0644)
```

Key directory: mode `0700`. Serialized as PEM (PKCS8 private, SubjectPublicKeyInfo public). Keys are managed via the `rye/core/keys/keys` tool (actions: `generate`, `info`, `trust`, `list`, `remove`, `import`). Signing no longer auto-generates keypairs — `MetadataManager.create_signature()` uses `load_keypair()` and raises `RuntimeError` if no keypair exists.

### Signature Format

```
rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
```

| Field          | Value                                                    |
| -------------- | -------------------------------------------------------- |
| `TIMESTAMP`    | ISO 8601 UTC (e.g., `2026-02-14T00:44:49Z`)             |
| `CONTENT_HASH` | SHA256 hex digest of normalized content                 |
| `ED25519_SIG`  | Base64url-encoded Ed25519 signature of the content hash |
| `PUBKEY_FP`    | First 16 hex chars of `SHA256(public_key_pem)`          |

### Comment Syntax per File Type

| File Type             | Format                                      |
| --------------------- | ------------------------------------------- |
| Markdown (directives) | `<!-- rye:signed:TIMESTAMP:HASH:SIG:FP -->` |
| Python (tools)        | `# rye:signed:TIMESTAMP:HASH:SIG:FP`        |
| YAML (tools/configs)  | `# rye:signed:TIMESTAMP:HASH:SIG:FP`        |

### Registry Provenance Suffix

Registry-signed items append `|rye-registry@{username}`:

```
rye:signed:TIMESTAMP:HASH:SIG:FP|rye-registry@leolilley
```

For markdown (HTML comment), provenance goes before closing `-->`:

```html
<!-- rye:signed:2026-02-14T...:HASH:SIG:FP|rye-registry@leolilley -->
```

### Signing Steps

1. `MetadataManager` extracts content to hash (strips existing signature if present)
2. `SHA256(normalized_content)` → `content_hash`
3. `sign_hash(content_hash, private_key_pem)` → base64url Ed25519 signature
4. `compute_key_fingerprint(public_key_pem)` → 16-char hex fingerprint
5. Signature comment inserted at line 1

```python
def sign_hash(content_hash: str, private_key_pem: bytes) -> str:
    private_key = serialization.load_pem_private_key(private_key_pem, password=None)
    signature = private_key.sign(content_hash.encode("utf-8"))
    return base64.urlsafe_b64encode(signature).decode("ascii")
```

## Trust Store

**Location:** `~/.ai/config/keys/trusted/`
**Implementation:** `rye/rye/utils/trust_store.py`

Every item in Rye — including Rye's own system tools — must pass Ed25519 signature verification against a trusted key. There are no exceptions and no bypass for system items.

### Identity Document Format

Trusted keys are TOML identity documents that bind a key to an owner:

```toml
# .ai/config/keys/trusted/{fingerprint}.toml
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
project/.ai/config/keys/trusted/{fingerprint}.toml  →  (highest priority)
user/.ai/config/keys/trusted/{fingerprint}.toml     →
system/.ai/config/keys/trusted/{fingerprint}.toml   →  (lowest priority, shipped with bundle)
```

First match wins. The system bundle ships the author's key at `rye/.ai/config/keys/trusted/{fingerprint}.toml` — resolved automatically via standard 3-tier lookup, with no special bootstrap logic.

### TrustStore Operations

| Operation                     | Method                        | Behavior                                                                    |
| ----------------------------- | ----------------------------- | --------------------------------------------------------------------------- |
| Check trust                   | `is_trusted(fingerprint)`     | Delegates to `get_key()`, returns True if key found                         |
| Get key by fingerprint        | `get_key(fingerprint)`        | 3-tier search: project → user → system `.ai/config/keys/trusted/{fp}.toml`        |
| Add key                       | `add_key(public_key_pem)`     | Writes `{fingerprint}.toml` identity document, returns fingerprint          |
| Remove key                    | `remove_key(fingerprint)`     | Deletes `{fingerprint}.toml` from user store                               |
| Pin registry key              | `pin_registry_key(pem)`       | Adds key with `owner="rye-registry"` (**no-op if already exists**)          |
| Get registry key              | `get_registry_key()`          | Scans all keys for `owner=="rye-registry"`                                  |
| List all keys                 | `list_keys()`                 | Returns all `.toml` identity documents across all spaces                    |

**Auto-trust:** User's own public key is automatically added to the trust store on keygen (with `owner="local"`). Keys are generated and managed via the `rye/core/keys/keys` tool. For bundles, use `action: trust, space: project` to provision trusted keys.

## TOFU (Trust On First Use)

Registry key bootstrap flow on first-ever pull:

```
1. Pull returns signed content with pubkey_fp in signature
2. TrustStore.get_registry_key() → None (no pinned key)
3. Client fetches GET {REGISTRY_API_URL}/v1/public-key → Ed25519 PEM
4. TrustStore.pin_registry_key(registry_key_pem) → writes identity document with owner="rye-registry"
5. All subsequent pulls verify against this pinned key
```

```python
if registry_key is None:
    # TOFU: fetch and pin registry key on first pull
    key_url = f"{REGISTRY_API_URL}/v1/public-key"
    req = urllib.request.Request(key_url)
    with urllib.request.urlopen(req, timeout=10) as resp:
        registry_key = resp.read()
    trust_store.pin_registry_key(registry_key)
```

**Key immutability:** `pin_registry_key()` is a no-op if key already exists. Once pinned, the registry key cannot be silently replaced.

## Manual Key Trust

For items signed by individual users (not registry):

```python
rye_sign(
    action="trust",
    public_key_pem="<PEM content>"
)
# Writes to ~/.ai/config/keys/trusted/{fingerprint}.toml
```

## Verification on Execute

**Implementation:** `rye/rye/utils/integrity.py`

### Four Verification Steps

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

**No exceptions.** Every item — system, user, or project — goes through the same verification. Any check failure → `IntegrityError` → execution denied.

### What Triggers Verification

| Trigger              | Verified                                       |
| -------------------- | ---------------------------------------------- |
| `rye_execute` a tool | Every element in the executor chain            |
| Pull from registry   | Registry signature + content hash              |
| Bundle verify        | Manifest signature + per-file SHA256 hashes    |
| Lockfile check       | Root integrity hash + all chain element hashes |

## Chain Integrity

**Implementation:** `rye/rye/executor/primitive_executor.py`

`PrimitiveExecutor` verifies **every element** in the execution chain:

```python
for element in chain:
    verify_item(element.path, ItemType.TOOL, project_path=self.project_path)
```

Example chain:

```
my-tool.py → python/script.yaml → subprocess.yaml
```

All three must be signed and verified before `my-tool.py` executes.

### Lockfile Integrity

When a lockfile exists:
1. **Root integrity** — Lockfile's recorded hash must match current file's computed hash
2. **Chain element integrity** — Every recorded chain element must still exist with same hash

Hash mismatch → execution fails → re-sign and delete stale lockfile.

### Dependency Verification

`_verify_tool_dependencies()` walks a tool's anchor directory tree, verifying every file matching configured extensions (`.py`) via `verify_item()`. Runs before subprocess spawn.

## Bundle Integrity

Manifest verification via `validate_bundle_manifest()`:

| Layer                  | Check                                                              |
| ---------------------- | ------------------------------------------------------------------ |
| **Manifest signature** | `verify_item(manifest_path, ItemType.TOOL)` — Ed25519 on manifest |
| **Per-file SHA256**    | Compute hash of each file, compare to manifest's recorded hash     |
| **Inline signatures**  | If `inline_signed: true`, also `verify_item()` on that file       |
| **Missing files**      | Files in manifest but not on disk → flagged                        |

## API Key Authentication

The Rye Registry uses `rye_sk_...` API keys as the primary authentication mechanism. JWTs are used only once during the initial device auth flow to bootstrap the first API key.

### Auth Flow

```
Device flow → OAuth → temporary JWT → create rye_sk_... API key → store API key → use everywhere
```

### Token Resolution (Client)

1. `RYE_REGISTRY_API_KEY` env var — primary for CI/serverless
2. Keyring (AuthStore) — stores API key from device flow

### API Key Format

```
rye_sk_{secrets.token_urlsafe(32)}
```

Only the SHA256 hash is stored server-side. The raw key is returned once on creation.

### Registry API Auth Detection

- `Bearer rye_sk_...` → API key path (primary)
- Other Bearer tokens → JWT validation (bootstrap only, for initial API key creation)

### Management Actions

| Action | Description |
| --- | --- |
| `create_api_key` | Create new API key (requires existing session) |
| `list_api_keys` | List active keys (prefix + name + created) |
| `revoke_api_key` | Revoke by name |

## Threat Model

### Protected Against

- **Tampered items** — SHA256 + Ed25519 catches any modification
- **Unauthorized items** — Trust store rejects unknown key signatures
- **Registry impersonation** — TOFU pinning prevents key substitution after first pull
- **Chain attacks** — Every chain element verified, not just entry point
- **Bundle tampering** — Per-file hashes in signed manifests

### Known Gaps

| Gap                       | Description                                                              |
| ------------------------- | ------------------------------------------------------------------------ |
| Advisory permissions      | Tool permissions enforced at Python level, not OS-level sandbox          |
| No key revocation         | No mechanism to revoke compromised keys; manual trust store removal only |
| TOFU limitations          | First connection MITM can pin wrong key; no out-of-band verification    |
| No supply chain audit     | Compromised registry re-signing attacks not formally modeled            |
| Single registry key       | One keypair for all ops; rotation requires all clients to re-pin        |
| No signature expiry       | Signatures never expire; trusted indefinitely                           |
| System key visibility     | System bundle author keys are trusted by default via 3-tier resolution; users implicitly trust any installed bundle's author key |
