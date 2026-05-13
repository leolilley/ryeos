---
category: "ryeos/development"
name: "signing"
description: "How signing works in this project, what to sign, what key to use, common pitfalls"
---

# Signing in This Project

## The one rule

**Sign at bundle granularity, not file granularity.** There is no per-file signing CLI.

```bash
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev
```

This signs every signable item in both bundles, rebuilds CAS manifests, and emits `PUBLISHER_TRUST.toml` files.

## When you must re-sign

- After editing any YAML under `ryeos-bundles/`
- After `cargo build` (binaries change, manifest hashes go stale)
- After a fresh checkout (binaries are `.gitignored`)
- Before running tests that load the bundle tree

## What "signed" means

Every YAML file under `ryeos-bundles/<bundle>/.ai/` that loads through `VerifiedLoader` carries an inline `# ryeos:signed:...` header. The header is an Ed25519 signature over the file's content hash, plus a publisher fingerprint. When the runtime loads the file, it:

1. Strips the signature line
2. Hashes the remaining content
3. Verifies the signature against the fingerprint's pubkey from the trust store
4. Accepts (trusted), warns (unknown signer), or fails (invalid)

You cannot edit a signed YAML and reuse the old signature. Any byte change invalidates it.

## The dev publisher key

The keypair lives in `.dev-keys/`:

| File | Purpose |
|---|---|
| `PUBLISHER_DEV.pem` | Private Ed25519 key (PKCS#8 PEM) |
| `PUBLISHER_DEV_TRUST.toml` | Public key + fingerprint + owner label |

The dev key fingerprint is `741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea`.

Verify after signing:
```bash
head -1 ryeos-bundles/standard/.ai/config/ryeos-runtime/model-providers/zen.yaml
# Trailing fingerprint should be 741a8bc...
```

## Common mistakes

### Using `--seed 42`

Older docs may reference `--seed 42`. **Do not use it for bundle artifacts.** The seed-42 key is NOT in the engine test trust store. It breaks ~130 tests with "signature not in trust store."

Recovery:
```bash
git checkout ryeos-bundles/
rm -rf ryeos-bundles/core/.ai/objects/blobs/*/
```
Then rebuild with `--key .dev-keys/PUBLISHER_DEV.pem`.

### Trying to sign individual files

No `--file` or `--single` flag exists. Use `populate-bundles.sh` or `ryeos publish` at bundle granularity.

### Stale manifest after cargo build

The dev tree symlinks `ryeos-bundles/core/.ai/bin/<triple>/ryeos-core-tools` to `target/debug/ryeos-core-tools`. Any rebuild changes the binary hash. Fix:
```bash
./scripts/gate.sh --no-tests
```

## Signing project-level items

For items in a project's `.ai/` (directives, tools, knowledge in a user's project — not this repo's bundles), use `ryeos sign`:

```bash
ryeos sign .ai/knowledge/my/entry.md
```

This is different from bundle signing. Project items are signed with the operator's user key, not the publisher key.
