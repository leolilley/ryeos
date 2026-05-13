---
category: "ryeos/development"
name: "build-and-test"
description: "Build the Rust workspace, populate bundles, run the test gate"
---

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

This stages binaries into `ryeos-bundles/{core,standard}/.ai/bin/<triple>/`, then runs `ryeos publish` on both bundles to sign all items and rebuild CAS manifests.

**Idempotent.** Safe to re-run. Required after any `cargo build` that changes binary hashes.

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

## Debug builds

`cargo build` (without `--release`) produces debug binaries. These work but:
- Debug `ryeos-core-tools` symlinked into the bundle will have a different hash
- The manifest will be stale until `populate-bundles.sh` runs
- Tests that verify binary hashes will fail

For development iteration, use `cargo build` for compile speed, then run `gate.sh --no-tests` before testing.

## The dev key

`.dev-keys/PUBLISHER_DEV.pem` is the development publisher key. It is intentionally checked into version control. **Never trust this key in production.**

The engine test harness trusts exactly one key: the official publisher fingerprint (`OFFICIAL_PUBLISHER_FP` in `ryeos-tools/src/actions/init.rs`). For dev bundles, that's the dev key's fingerprint.
