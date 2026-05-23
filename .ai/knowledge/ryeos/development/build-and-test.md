```yaml
category: "ryeos/development"
name: "build-and-test"
description: "Build the Rust workspace, populate bundles, run the test gate"
```

# Build and Test

## Prerequisites

- Rust stable (1.80+)
- `cargo-nextest` installed (`cargo install cargo-nextest`)
- Linux (x86_64 or aarch64)
- `HOSTNAME` env var set (most desktops set this automatically)

## One-command gate

```bash
./scripts/gate.sh
```

This is the canonical gate. It does three things:
1. Populates bundles (build binaries + sign + rebuild CAS manifest)
2. Runs `cargo nextest run --workspace --no-fail-fast`

Skip tests with `--no-tests`:
```bash
./scripts/gate.sh --no-tests
```

Forward nextest args:
```bash
./scripts/gate.sh -p ryeosd
```

## Step by step

### 1. Build release binaries

```bash
cargo build --release
```

Produces these binaries in `target/release/`:

| Binary | Purpose |
|---|---|
| `ryeosd` | The daemon |
| `ryeos` | The CLI |
| `ryeos-directive-runtime` | Directive execution runtime |
| `ryeos-graph-runtime` | State graph execution runtime |
| `ryeos-knowledge-runtime` | Knowledge composition runtime |
| `ryeos-core-tools` | Core tools binary (sign, verify, identity, fetch) |
| Parser/composer binaries | `rye-parser-*`, `rye-composer-*` |

### 2. Populate bundles

```bash
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev
```

This stages binaries into `bundles/{core,standard}/.ai/bin/<triple>/`, then runs `ryeos-core-tools build` on both bundles to sign all items, rebuild CAS manifests, and emit publisher trust docs.

**Idempotent.** Safe to re-run. Required after any `cargo build` that changes binary hashes.

What it actually does:

1. Builds the release binaries that bundles own.
2. Wipes derived bundle state: `.ai/bin/`, `.ai/objects/`, `.ai/refs/`, and stale `PUBLISHER_TRUST.toml`.
3. Installs fresh release binaries into `bundles/core/.ai/bin/<triple>/` and `bundles/standard/.ai/bin/<triple>/`.
4. Runs the offline publisher binary directly:
   - `target/release/ryeos-core-tools build bundles/core --registry-root bundles/core --owner <owner>`
   - `target/release/ryeos-core-tools build bundles/standard --registry-root bundles/core --owner <owner>`

`ryeos-core-tools build` signs and publishes an already-staged bundle tree; it does **not** compile Rust binaries. Do not manually copy a single binary into a bundle as a recovery step — that bypasses manifest/CAS/signature regeneration.

After a source merge that changes bundle schemas, parsers, composers, or bundled binaries, use the full refresh + init sequence:

```bash
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev

target/release/ryeos-core-tools bundle-verify bundles/core --registry-root bundles/core
target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core

# Use the same system space your daemon/CLI actually uses.
target/release/ryeos init \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml

# Repo-local dev install, matching scripts/dev-up.sh:
target/release/ryeos init \
  --system-space-dir .local/ryeos \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

For source-tree verification, pass `--registry-root` explicitly. Omitting it lets verification consult installed bundle registrations, which may be stale when you are repairing the bundle source tree.

### 3. Run tests

```bash
cargo nextest run --workspace --no-fail-fast
```

Or use `gate.sh` which auto-syncs manifests first.

## Common build failures

| Symptom | Cause | Fix |
|---|---|---|
| `hash mismatch` in tests | Binary changed but manifest is stale | Run `populate-bundles.sh` or `gate.sh --no-tests` |
| `no kind schema roots found` | Missing core bundle in system space | Run `ryeos init` |
| `signature from fingerprint X not in trust store` | Signed with wrong key | Use `.dev-keys/PUBLISHER_DEV.pem`, never `--seed 42` |
| `failed to acquire state lock` | Another daemon instance running | Stop it first |
| `unknown variant ... expected ...` while publishing/verifying a new kind schema | The merged bundle descriptor language is newer than the built binaries, or the source code lacks support for the new descriptor term | Fix/build the Rust support first, then rerun `populate-bundles.sh`; do not add raw YAML fallbacks or hardcoded registries |

## Debug builds

`cargo build` (without `--release`) produces debug binaries. These work but:
- Bundle `.ai/bin/<triple>/` contents are release binaries staged by `populate-bundles.sh`, not automatically refreshed by debug builds
- The bundle manifest will be stale until `populate-bundles.sh` runs after any binary change that should be packaged
- Tests that verify binary hashes will fail

For development iteration, use `cargo build` for compile speed, then run `gate.sh --no-tests` before testing.

## The dev key

`.dev-keys/PUBLISHER_DEV.pem` is the development publisher key. It is intentionally checked into version control. **Never trust this key in production.**

The engine test harness trusts exactly one key: the official publisher fingerprint (`OFFICIAL_PUBLISHER_FP` in `crates/tools/core-tools/src/actions/init.rs`). For dev bundles, that's the dev key's fingerprint.
