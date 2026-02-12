# Content Signing

Every item in RYE carries an Ed25519 signature that binds its content to a specific keypair. The signing system is implemented across three modules:

- [`rye/rye/utils/integrity.py`](rye/rye/utils/integrity.py) — `verify_item()`, the central enforcement point
- [`rye/rye/utils/metadata_manager.py`](rye/rye/utils/metadata_manager.py) — `MetadataManager` with per-type strategies for hash extraction and signature embedding
- [`lilux/lilux/primitives/signing.py`](lilux/lilux/primitives/signing.py) — pure Ed25519 cryptographic primitives

## Signature Format

```
rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
```

| Field          | Format                                | Description                                                      |
| -------------- | ------------------------------------- | ---------------------------------------------------------------- |
| `TIMESTAMP`    | ISO 8601 UTC (`2026-02-11T00:00:00Z`) | When the signature was created                                   |
| `CONTENT_HASH` | 64-char hex SHA256 digest             | Hash of the content portion (excludes the signature line itself) |
| `ED25519_SIG`  | Base64url-encoded Ed25519 signature   | Signature over `CONTENT_HASH`                                    |
| `PUBKEY_FP`    | 16-char hex                           | First 16 hex characters of `SHA256(public_key_pem)`              |

Registry-attested items append a provenance suffix to the fingerprint field:

```
rye:signed:T:H:S:FP|provider@username
```

## Signature Embedding by Item Type

### Directives

HTML comment as the first line of the markdown file. Content hash is computed over the `<directive>...</directive>` XML body only.

```markdown
<!-- rye:signed:2026-02-11T00:00:00Z:a1b2c3...64chars:base64url_sig:0123456789abcdef -->

# My Directive

<directive name="example" version="1.0.0">
  ...
</directive>
```

### Tools

Language-appropriate comment prefix, placed after the shebang line (if present). Content hash covers everything except the signature line and shebang.

```python
#!/usr/bin/env python3
# rye:signed:2026-02-11T00:00:00Z:a1b2c3...64chars:base64url_sig:0123456789abcdef

def main():
    ...
```

The comment prefix is resolved per file extension via `get_signature_format()`. For example: `#` for Python/Shell, `//` for JavaScript/TypeScript/Go/Rust.

### Knowledge

HTML comment as the first line. Content hash excludes both the signature line and YAML frontmatter — only the body content after the closing `---` is hashed.

```markdown
## <!-- rye:signed:2026-02-11T00:00:00Z:a1b2c3...64chars:base64url_sig:0123456789abcdef -->

title: Example Entry
entry_type: reference

---

Actual knowledge content hashed here.
```

## Verification Flow

`verify_item()` executes these steps in order. Any failure raises `IntegrityError`.

```
1.  Read file content (UTF-8)
2.  Extract signature via MetadataManager.get_signature_info()
3.  REJECT if no signature found
      → IntegrityError: "Unsigned item: {path}"
4.  REJECT if signature lacks ed25519_sig field (legacy rye:validated: format)
      → IntegrityError: "Legacy signature format (rye:validated:) rejected"
5.  Recompute content hash via MetadataManager.compute_hash()
6.  Compare recomputed hash against embedded hash
      → IntegrityError if mismatch: "Integrity failed: {path} (expected ..., got ...)"
7.  Look up PUBKEY_FP in TrustStore
8.  REJECT if key not found
      → IntegrityError: "Untrusted key {fp} for {path}"
9.  Verify Ed25519 signature using public key from trust store
10. REJECT if signature invalid
      → IntegrityError: "Ed25519 signature verification failed: {path}"
```

On success, `verify_item()` returns the verified content hash.

## Enforcement Points

`verify_item()` is called at these locations — all before any content is used:

| Module                                                                                                               | When Called                               | Behavior                                                                 |
| -------------------------------------------------------------------------------------------------------------------- | ----------------------------------------- | ------------------------------------------------------------------------ |
| [`rye/rye/tools/execute.py`](rye/rye/tools/execute.py)                                                               | Before directive or knowledge execution   | Blocking — `IntegrityError` aborts execution                             |
| [`rye/rye/executor/primitive_executor.py`](rye/rye/executor/primitive_executor.py)                                   | Every element in the tool chain           | Blocking — every file in the resolution chain is verified                |
| [`rye/rye/tools/load.py`](rye/rye/tools/load.py)                                                                     | Before loading content for inspection     | Blocking — unsigned content cannot be read                               |
| [`rye/rye/tools/search.py`](rye/rye/tools/search.py)                                                                 | During metadata extraction for results    | Non-blocking — sets `signed: false` on failure, does not prevent listing |
| [`rye/rye/.ai/tools/rye/agent/threads/thread_directive.py`](rye/rye/.ai/tools/rye/agent/threads/thread_directive.py) | Before parsing directive for agent thread | Blocking — returns `None` on failure, thread cannot start                |

## Lockfile Chain Integrity

In addition to `verify_item()` (which checks Ed25519 signatures at execution time), lockfiles store a per-element integrity hash for change detection between lockfile creation and subsequent use.

When a lockfile is created after successful execution, `MetadataManager.compute_hash()` is called for every chain element, and the resulting SHA256 hashes are stored in each `resolved_chain` entry's `integrity` field.

On lockfile load, the executor re-resolves each chain element by `item_id` + `space` and recomputes the hash. If any element's hash differs from the stored value, execution is blocked:

```
Lockfile integrity mismatch for chain element {item_id}. Re-sign and delete stale lockfile.
```

This is a **separate check** from Ed25519 signature verification:

| Check              | When                        | What It Detects                                   | Module           |
| ------------------ | --------------------------- | ------------------------------------------------- | ---------------- |
| Ed25519 signature  | Every execution             | Tampering, unsigned content, untrusted keys       | `verify_item()`  |
| Lockfile integrity | When lockfile exists        | Any content change since lockfile was generated    | `execute()`      |

Both checks must pass. The lockfile check runs first (before chain building), and Ed25519 verification runs after chain building for every element.

See [Lockfile Format](../reference/file-formats/lockfile-format.md) for the full format specification.

## Dynamic Dependency Verification

`verify_item()` covers chain elements resolved via `__executor_id__`. However, signed tools often load dependencies dynamically — Python modules via `importlib`, JS files via `node_runtime`, YAML configs via `safe_load()`. These loads bypass the chain and are **not** verified by `PrimitiveExecutor`.

The `verify_dependency()` function closes this gap. It is language-agnostic — it calls `verify_item()` before any load or execution of files under `.ai/tools/`, using `get_signature_format()` to resolve the correct comment prefix per file extension (data-driven from the tool extractor's `SIGNATURE_FORMAT` dict). See [App Bundling and Orchestration — Verified Loader](../concepts/app-bundling-and-orchestration.md#layer-1-verified-loader-for-dynamic-dependencies) for the implementation.

| Verification Point | Module | What's Verified |
|-------------------|--------|-----------------|
| Chain resolution | `PrimitiveExecutor._build_chain()` | Files with `__executor_id__` |
| Dynamic dependency | `verify_dependency()` | Any file under `.ai/tools/` loaded at runtime (`.py`, `.js`, `.sh`, `.yaml`, etc.) |
| Directive load | `thread_directive._load_directive()` | Directive files |
| Bundle asset load | `verify_bundle_manifest()` | App source, assets, config (any file type including non-code) |

## Bundle Manifests

For app bundles that include files where inline signatures are not possible (JSX, CSS, images, minified JS, binaries), a **signed bundle manifest** provides integrity coverage. The manifest lists `{relative_path: sha256}` for every file in the bundle and is itself signed with Ed25519.

At load time, files are verified against either their inline signature or their manifest hash entry. Files covered by neither are rejected.

See [App Bundling and Orchestration — Bundle Manifest](../concepts/app-bundling-and-orchestration.md#layer-2-bundle-manifest-for-app-assets) for the full specification.

## Legacy Format Rejection

The old `rye:validated:` and `kiwi-mcp:validated:` signature formats are rejected entirely. When `verify_item()` finds a signature without the `ed25519_sig` field, it raises:

```
IntegrityError: Legacy signature format (rye:validated:) rejected: {path}.
Re-sign with Ed25519 via the sign tool.
```

There is no backwards compatibility. All items must be re-signed.
