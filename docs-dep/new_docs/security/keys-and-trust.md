# Keys and Trust

RYE uses a single Ed25519 keypair per user for both content signing and capability token signing. Keys are managed through two modules:

- [`lilux/lilux/primitives/signing.py`](lilux/lilux/primitives/signing.py) — pure cryptographic primitives
- [`rye/rye/utils/trust_store.py`](rye/rye/utils/trust_store.py) — trust store management

## Keypair Storage

```
~/.ai/keys/
├── private_key.pem   (0600 — owner read/write only)
└── public_key.pem    (0644 — owner read/write, others read)
```

The key directory itself is set to `0700`.

## Key Generation

Keypairs are generated automatically on first use. When `ensure_keypair(key_dir)` is called (during any sign operation), it:

1. Attempts to load existing keys from `key_dir`
2. If `FileNotFoundError`, generates a new Ed25519 keypair
3. Saves with proper file permissions
4. Returns `(private_pem, public_pem)`

The user's own public key is auto-trusted on first sign — `MetadataManager.create_signature()` checks if the key's fingerprint is in the trust store and adds it if missing.

## Fingerprints

A key fingerprint is the first 16 hex characters of `SHA256(public_key_pem)`:

```python
hashlib.sha256(public_key_pem).hexdigest()[:16]
# e.g., "0a3f9b2c1d4e5f67"
```

Fingerprints identify keys in signatures (`PUBKEY_FP` field) and in the trust store (filename).

## Trust Store

```
~/.ai/trusted_keys/
├── 0a3f9b2c1d4e5f67.pem   # Self key (auto-trusted)
├── 8c7d6e5f4a3b2c1d.pem   # Peer key (manually added)
└── registry.pem            # Registry key (TOFU-pinned)
```

Keys are stored as PEM files named by fingerprint. The registry key uses the fixed name `registry.pem`.

### TrustStore API

```python
from rye.utils.trust_store import TrustStore

store = TrustStore()  # defaults to ~/.ai/trusted_keys/
```

| Method                                | Description                                                                                                       |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `is_trusted(fingerprint)`             | Returns `True` if `{fp}.pem` exists or `registry.pem` matches the fingerprint                                     |
| `get_key(fingerprint)`                | Returns PEM bytes for the fingerprint, checking both `{fp}.pem` and `registry.pem`. Returns `None` if not found.  |
| `add_key(public_key_pem, label=None)` | Writes key as `{fingerprint}.pem`. Returns fingerprint.                                                           |
| `remove_key(fingerprint)`             | Deletes `{fingerprint}.pem`. Returns `True` if removed, `False` if not found.                                     |
| `pin_registry_key(public_key_pem)`    | TOFU pin — writes `registry.pem` only if it does not already exist. No-op if already pinned. Returns fingerprint. |
| `get_registry_key()`                  | Returns registry PEM bytes, or `None` if not pinned.                                                              |
| `list_keys()`                         | Returns list of dicts with `fingerprint`, `path`, `is_registry`, and `label` fields.                              |

### Trust Lookup Order

When `is_trusted()` or `get_key()` is called:

1. Check `~/.ai/trusted_keys/{fingerprint}.pem`
2. If not found, check if `registry.pem` exists and its fingerprint matches
3. If neither matches, return `False` / `None`

## Lilux Signing Primitives

[`lilux/lilux/primitives/signing.py`](lilux/lilux/primitives/signing.py) provides pure cryptographic operations with no policy logic:

| Function                                                        | Signature                     | Description                                        |
| --------------------------------------------------------------- | ----------------------------- | -------------------------------------------------- |
| `generate_keypair()`                                            | `→ (private_pem, public_pem)` | Generate new Ed25519 keypair as PEM bytes          |
| `sign_hash(content_hash, private_key_pem)`                      | `→ str`                       | Sign SHA256 hex digest, return base64url signature |
| `verify_signature(content_hash, signature_b64, public_key_pem)` | `→ bool`                      | Verify signature against content hash              |
| `compute_key_fingerprint(public_key_pem)`                       | `→ str`                       | First 16 hex chars of SHA256(PEM)                  |
| `ensure_keypair(key_dir)`                                       | `→ (private_pem, public_pem)` | Load or generate keypair at path                   |
| `save_keypair(private_pem, public_pem, key_dir)`                | `→ None`                      | Write keys with `0600`/`0644` permissions          |
| `load_keypair(key_dir)`                                         | `→ (private_pem, public_pem)` | Load existing keys or raise `FileNotFoundError`    |

All signing operations use the `cryptography` library's `Ed25519PrivateKey` and `Ed25519PublicKey` classes. Private keys are PKCS8-encoded PEM without encryption.
