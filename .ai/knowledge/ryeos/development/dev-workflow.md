<!-- ryeos:signed:2026-08-23T23:14:59Z:7c7956fe7c4e99ec5593fbc376828553e6d195c7f2b5e8ffb1896a59fb0b872c:i+X3Aze0bi8/tSNbVaHgj4laAcggXP1WH1BbatxPC1Vh36jPjSLNSV7WuMvtv5lhPo1z6ldR5KrL3XYQJIsWAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "dev-workflow"
title: "Development Workflow"
description: "Short LLM-facing guide for choosing the right RyeOS dev workflow"
entry_type: reference
version: "1.3.1"
```

# Development Workflow

This directory is repository-contributor knowledge: it exists to help people
and coding agents change, test, sign, review, and release the RyeOS project.
Installed product architecture and operator runbooks belong in bundle knowledge,
normally under `bundles/standard/.ai/knowledge/`.

Use this when an agent needs orientation before changing code. For exact build,
signing, and install commands, prefer `development/build-and-test.md`.

## Pick the loop

| Change type | Loop |
|---|---|
| Rust-only, compile feedback | `cargo build` or a targeted `cargo test -p <crate>` |
| Rust that affects bundled binaries | `./scripts/gate.sh --refresh-bundles --no-tests`, then targeted/full tests |
| Anything under `bundles/` | `./scripts/gate.sh --refresh-bundles` unless intentionally skipping tests |
| Daemon/CLI behavior with installed bundles | initialize/start a repo-local app root with `--app-root .local/ryeos` |
| System packaged-layout repair from already-built artifacts | `./scripts/pkg/install-local-direct.sh --trust-source-publishers` |

Default rule: if a test or runtime loads bundle items, refresh/sign bundles
first. Stale bundle bin/CAS/signature state is the most common false failure.

## Repo-local app root

```bash
target/release/ryeos init \
  --app-root .local/ryeos \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
target/release/ryeos start --app-root .local/ryeos
```

This initializes `.local/ryeos` from the already-built source bundles and
starts a daemon against that isolated app root. It does not populate/rebuild
bundles or touch the normal user app root.

## Day-to-day examples

Targeted Rust edit:

```bash
cargo build
cargo test -p ryeos-engine
```

Bundle-aware edit:

```bash
./scripts/gate.sh --refresh-bundles --no-tests
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
- If unsure which command to run for bundle-affecting work, use
  `./scripts/gate.sh --refresh-bundles --no-tests` before targeted tests.
