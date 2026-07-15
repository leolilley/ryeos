<!-- ryeos:signed:2026-07-14T01:54:46Z:ed1ed79f3b2ec6eb2cd0f3c1039c2a0d6ce78da94856e3b22240b95fa5773e2e:h/ZhRqj6wpEmY4CXaUrI+FOWHdSQLyfb+Ky8d2OonfulZEbvmBtCKYCVNY107RFbe7bVGwKznNOoAppcCFxhBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/daemon
tags: [daemon, state, cas, sqlite, vault, locks, ownership]
version: "2.1.0"
description: >
  Daemon state directory layout, file ownership, lifecycle locks, and what
  persists across restarts.
---

# Daemon State Directory

Daemon-owned state and operator-owned state both live under the
configured system space (the app root), not inside ephemeral execution
working directories.

## Ownership split

Operator artifacts owned by `ryeos init`:

- `<system>/.ai/config/keys/signing/private_key.pem` — operator/CLI key.
- `<system>/.ai/config/keys/trusted/<fp>.toml` — publisher, operator, and
  node trust docs.

System-space artifacts installed or registered by `ryeos init`:

- `<system>/.ai/bundles/<name>/.ai/` — installed bundle content.
- `<system>/.ai/node/bundles/<name>.yaml` — signed registrations.
- `<system>/.ai/node/identity/private_key.pem` — node key.
- `<system>/.ai/node/vault/private_key.pem` — vault X25519 key.
- `<system>/.ai/node/ingest/ignore.yaml` — ingest-ignore config.
- `<system>/.ai/node/sandbox.yaml` — create-once strict subprocess policy.

Daemon-local artifacts `ryeosd` may repair after init verification:

- `<system>/.ai/node/config.yaml`
- `<system>/.ai/node/identity/public-identity.json`
- `<system>/.ai/node/vault/public_key.pem`
- `<system>/.ai/node/auth/authorized_keys/<user-fp>.toml`
- daemon-local layout directories

The daemon must not write user trust docs or regenerate the node key.

## Major directories

- `.ai/bundles/` — installed bundles.
- `.ai/node/config.yaml` — daemon config.
- `.ai/node/sandbox.yaml` — immutable-at-runtime sandbox policy source.
- `.ai/node/identity/` — node key and public identity.
- `.ai/node/auth/authorized_keys/` — node-signed authorized callers.
- `.ai/node/vault/` — vault key material.
- `.ai/node/bundles/` — signed bundle registrations.
- `.ai/node/ingest/ignore.yaml` — ingest-ignore rules.
- `.ai/state/runtime.sqlite3` — thread, event, and projection database.
- `.ai/state/scheduler.sqlite3` — scheduler database.
- `.ai/state/objects/` and `.ai/state/refs/` — CAS.
- `.ai/state/cache/executions/` — request-owned pushed-head and no-project
  workspaces; guards remove them when their request/cache ownership ends.
- `.ai/state/secrets/` — sealed vault data.
- `.ai/state/audit/` — append-only audit trail.
- `.ai/state/schedules/` — scheduler state.

## Locks and metadata

- `.ai/state/operator.lock` — daemon/standalone state lock, opened with
  `truncate(false)`, flocked first, then populated with holder PID.
- `.ai/state/lifecycle-start.lock` — `ryeos start` coordination lock;
  flock-backed and self-clearing on process exit.
- `<system>/daemon.json` — daemon metadata hint written after listeners
  bind and removed best-effort on shutdown. It is not liveness truth.

## Execution state

Per-thread state directories are daemon-owned so checkpoints survive
working-directory cleanup and daemon restart.
