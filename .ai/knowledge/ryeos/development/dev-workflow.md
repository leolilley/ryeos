<!-- ryeos:signed:2026-05-25T06:47:54Z:8a82caf3ef35a380c1f1317369654565106a00b532d27277cd4cc560578cce88:ie23g/dCmoCaEroKfpNLAAeS8J7md5jK4vw+oTYilMqa8xMLSFVUwkaN5czVo1Fz8vNlX7PuDJaqE/bjVusNCQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
```yaml
category: "ryeos/development"
name: "dev-workflow"
title: "Development Workflow"
description: "Short LLM-facing guide for choosing the right RyeOS dev workflow"
entry_type: reference
version: "1.2.0"
```

# Development Workflow

Use this when an agent needs orientation before changing code. For exact build,
signing, and install commands, prefer `development/build-and-test.md`.

## Pick the loop

| Change type | Loop |
|---|---|
| Rust-only, compile feedback | `cargo build` or a targeted `cargo test -p <crate>` |
| Rust that affects bundled binaries | `./scripts/gate.sh --no-tests`, then targeted/full tests |
| Anything under `bundles/` | `./scripts/gate.sh` unless intentionally skipping tests |
| Daemon/CLI behavior with installed bundles | `./scripts/dev-up.sh` for repo-local `.local/ryeos` |
| System packaged-layout repair | `./scripts/pkg/install-local-direct.sh` |

Default rule: if a test or runtime loads bundle items, refresh/sign bundles
first. Stale bundle bin/CAS/signature state is the most common false failure.

## Fresh checkout

```bash
./scripts/dev-up.sh
```

This populates bundles, initializes `.local/ryeos`, and starts a daemon against
that isolated system space. It does not touch the normal user/system install.

## Day-to-day examples

Targeted Rust edit:

```bash
cargo build
cargo test -p ryeos-engine
```

Bundle-aware edit:

```bash
./scripts/gate.sh --no-tests
cargo test -p ryeos-cli
```

Full confidence:

```bash
./scripts/gate.sh
```

## Key locations

| Area | Path |
|---|---|
| CLI | `crates/bin/cli/src/` |
| Daemon | `crates/bin/daemon/src/` |
| Engine | `crates/engine/ryeos-engine/src/` |
| Core tools/actions | `crates/tools/core-tools/src/actions/` |
| TUI shared model | `crates/clients/base/src/` |
| TUI terminal client | `crates/clients/terminal/src/` |
| Core bundle | `bundles/core/.ai/` |
| Standard bundle | `bundles/standard/.ai/` |
| Dev publisher key | `.dev-keys/PUBLISHER_DEV.pem` |
| Main runbook | `.ai/knowledge/ryeos/development/build-and-test.md` |

## Git/derived state

Derived and safe to regenerate:

- `bundles/{core,standard}/.ai/bin/`
- `bundles/{core,standard}/.ai/objects/`
- `bundles/{core,standard}/.ai/refs/`
- `target/`

Committed and meaningful:

- `bundles/{core,standard}/PUBLISHER_TRUST.toml`
- signed YAML under `bundles/{core,standard}/.ai/`
- Rust source and scripts

## Guardrails for agents

- Prefer smallest code changes; do not paper over stale bundle state with code.
- Do not add raw YAML fallback parsers or hardcoded registries to pass tests.
- Do not copy bundle-owned binaries to `/usr/bin`; bundle resolution must go
  through signed bundle bin trees.
- If a daemon is running while bundles are reinitialized, restart it so the
  in-memory engine matches disk.
- If unsure which command to run, use `./scripts/gate.sh --no-tests` before
  targeted tests.
