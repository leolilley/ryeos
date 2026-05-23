```yaml
category: "ryeos/development"
name: "signing"
description: "How signing works in this project, what to sign, what key to use, common pitfalls"
```

# Signing in This Project

## The one rule

**Sign at bundle granularity, not file granularity.** There is no per-file signing CLI.

```bash
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev
```

This builds release binaries, stages them into the bundle `.ai/bin/<triple>/` directories, signs every signable item in both bundles, rebuilds CAS manifests, and emits `PUBLISHER_TRUST.toml` files.

## When you must re-sign

- After editing any YAML under `bundles/`
- After `cargo build` (binaries change, manifest hashes go stale)
- After a fresh checkout (binaries are `.gitignored`)
- Before running tests that load the bundle tree

## What "signed" means

Every YAML file under `bundles/<bundle>/.ai/` that loads through `VerifiedLoader` carries an inline `# ryeos:signed:...` header. The header is an Ed25519 signature over the file's content hash, plus a publisher fingerprint. When the runtime loads the file, it:

1. Strips the signature line
2. Hashes the remaining content
3. Verifies the signature against the fingerprint's pubkey from the trust store
4. Accepts (trusted), warns (unknown signer), or fails (invalid)

You cannot edit a signed YAML and reuse the existing signature. Any byte change invalidates it.

## The dev publisher key

The keypair lives in `.dev-keys/`:

| File | Purpose |
|---|---|
| `PUBLISHER_DEV.pem` | Private Ed25519 key (PKCS#8 PEM) |
| `PUBLISHER_DEV_TRUST.toml` | Public key + fingerprint + owner label |

The dev key fingerprint is `741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea`.

Verify after signing:
```bash
head -1 bundles/standard/.ai/config/ryeos-runtime/model-providers/zen.yaml
# Trailing fingerprint should be 741a8bc...
```

## Common mistakes

### Using `--seed 42`

Older docs may reference `--seed 42`. **Do not use it for bundle artifacts.** The seed-42 key is NOT in the engine test trust store. It breaks ~130 tests with "signature not in trust store."

Recovery:
```bash
git checkout bundles/
rm -rf bundles/core/.ai/objects/blobs/*/
```
Then rebuild with `--key .dev-keys/PUBLISHER_DEV.pem`.

### Trying to sign individual files

No `--file` or `--single` flag exists. Use `populate-bundles.sh` (or `ryeos-core-tools build` after binaries are already staged) at bundle granularity.

### Stale bundle binaries or manifests after a merge/build

Bundle publishing packages the binaries already present under `bundles/{core,standard}/.ai/bin/<triple>/`. The publish step does **not** compile Rust or magically refresh those binaries. After source merges or Rust changes, stale bundle binaries can show up as hash mismatches or as parser/schema errors such as `unknown variant ... expected ...` when a new descriptor term landed with the merge.

Fix with the canonical rebuild + republish path:
```bash
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
```

Then reinstall the refreshed bundle tree into the system space you use:

```bash
target/release/ryeos init \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml

# Or for the repo-local dev system space:
target/release/ryeos init \
  --system-space-dir .local/ryeos \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

Do not manually copy `target/release/ryeos-core-tools` or any other single binary into a bundle as the fix. That leaves manifests, CAS sidecars, and signatures out of sync.

When verifying the source tree during recovery, make dependency roots explicit:

```bash
target/release/ryeos-core-tools bundle-verify bundles/core --registry-root bundles/core
target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core
```

## Signing project-level items

For items in a project's `.ai/` (directives, tools, knowledge in a user's project — not this repo's bundles), use `ryeos sign`:

```bash
ryeos sign .ai/knowledge/my/entry.md
```

This is different from bundle signing. Project items are signed with the operator's user key, not the publisher key.
