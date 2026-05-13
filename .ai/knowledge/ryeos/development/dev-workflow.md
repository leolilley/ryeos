---
category: "ryeos/development"
name: "dev-workflow"
description: "Day-to-day development workflow: edit, build, sign, test, iterate"
---

# Development Workflow

## The dev loop

```
edit code → cargo build → populate bundles → start daemon → test → repeat
```

### Quick iteration (no bundle changes)

If you only changed Rust code (not bundle YAML):

```bash
cargo build
# Tests that need bundle binaries will fail with hash mismatch.
# Run just the non-bundle tests:
cargo nextest run -p ryeos-engine
```

### Full iteration (bundle changes)

If you changed anything in `ryeos-bundles/`:

```bash
./scripts/gate.sh
```

This rebuilds binaries, re-signs bundles, and runs all tests.

### Daemon iteration

```bash
# Terminal 1: start daemon
cargo run --release -p ryeosd

# Terminal 2: test against daemon
curl http://127.0.0.1:7400/health
cargo run --release -p ryeos-cli -- execute tool:ryeos/core/identity/public_key
```

## One-command bootstrap

For a fresh checkout:

```bash
./scripts/dev-up.sh
```

This runs:
1. `populate-bundles.sh` (build + sign)
2. `ryeos init` (create node identity, install bundles)
3. Starts the daemon

Note: `dev-up.sh` uses `--system-space-dir .local/ryeos` for isolation from any system install.

## Key file locations

| What | Where |
|---|---|
| Daemon source | `ryeosd/src/` |
| CLI source | `ryeos-cli/src/` |
| Engine core | `ryeos-engine/src/` |
| Test support | `ryeos-engine/src/test_support.rs` |
| CLI actions | `ryeos-tools/src/actions/` |
| Core bundle items | `ryeos-bundles/core/.ai/` |
| Standard bundle items | `ryeos-bundles/standard/.ai/` |
| Dev publisher key | `.dev-keys/PUBLISHER_DEV.pem` |
| Gate script | `scripts/gate.sh` |
| Bundle populator | `scripts/populate-bundles.sh` |

## Testing patterns

### Unit tests

Most crates have unit tests. Run individually:

```bash
cargo nextest run -p ryeos-engine
cargo nextest run -p ryeosd
```

### Integration tests with live daemon

Tests in `tests/` spin up a real daemon process. These require:
- Built binaries (`cargo build --release`)
- Populated bundles (`populate-bundles.sh`)
- `HOSTNAME` env var set

### Test support

`ryeos_engine::test_support::live_trust_store()` provides a trust store that trusts only the dev publisher key. Use this in tests that load bundle items.

## Git workflow

- `ryeos-bundles/{core,standard}/.ai/bin/` — `.gitignored` (derived, rebuilt by scripts)
- `ryeos-bundles/{core,standard}/.ai/objects/` — `.gitignored` (CAS objects, regenerated)
- `ryeos-bundles/{core,standard}/.ai/refs/` — `.gitignored` (CAS refs, regenerated)
- `ryeos-bundles/{core,standard}/PUBLISHER_TRUST.toml` — committed (deterministic from key)
- `ryeos-bundles/{core,standard}/.ai/**/*.yaml` — committed (signed bundle items)
- `target/` — `.gitignored`

## CI

The canonical gate is `./scripts/gate.sh`. CI should invoke it directly. It auto-syncs manifests and runs the full workspace test suite.
