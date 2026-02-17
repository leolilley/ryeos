# TOFU Registry Pinning

RYE uses Trust On First Use (TOFU) to pin the registry's Ed25519 public key. Once pinned, the key cannot be replaced — preventing key rotation attacks from a compromised registry.

## Pin Lifecycle

### First Pull

On the first `registry pull`, the client:

1. Fetches the registry's public key via `GET /v1/public-key`
2. Calls `TrustStore.pin_registry_key(public_key_pem)`
3. Key is written to `~/.ai/trusted_keys/registry.pem`
4. Fingerprint is logged

### Subsequent Pulls

On every pull after the first:

1. `pin_registry_key()` detects `registry.pem` already exists
2. Returns the fingerprint of the existing key (no-op — file is not overwritten)
3. Pulled items are verified against the pinned key

If the registry rotates its key, `pin_registry_key()` silently ignores the new key. The existing pinned key remains authoritative.

## Registry Signing on Push

When an item is pushed to the registry:

1. The registry strips the author's local signature
2. Re-signs the content with the registry's Ed25519 private key
3. Appends provenance metadata to the signature's fingerprint field: `|provider@username`

The resulting signature looks like:

```
rye:signed:2026-02-11T00:00:00Z:a1b2c3...64chars:base64url_sig:registry_fp|provider@username
```

This binds the item to both the registry identity and the original author's account.

## Verification on Pull

When verifying a registry-signed item:

1. `verify_item()` extracts `PUBKEY_FP` from the signature
2. `TrustStore.get_key(fingerprint)` checks `{fp}.pem`, then checks if `registry.pem` matches
3. If the registry key's fingerprint matches, verification proceeds using `registry.pem`
4. Ed25519 signature is verified against the pinned key
5. `registry_provider` and `registry_username` fields are available from `MetadataManager.get_signature_info()` for provenance display

## Key Replacement

To trust a different registry key (e.g., after a legitimate key rotation), manually delete `~/.ai/trusted_keys/registry.pem` and pull again. This is intentionally a manual operation — automated key rotation is a security risk in this model.
