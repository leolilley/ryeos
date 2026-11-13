<!-- ryeos:signed:2026-11-13T05:04:32Z:b7709bf29f71b9d6254e8d5f8c8f4c5e287528b430221d6890a85625c1cbe768:FaxJkV4WI7Oxn+LiqPtpEYFwKSw5bMNZu/j3VpquAJ/l53WNplAB71YjQWdBiGgFWjlySMfR4b4EhRFwBSCRBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/daemon
tags: [daemon, state, cas, sqlite, vault, locks, ownership]
version: "2.1.1"
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
- `<system>/.ai/node/trust/signers/<fp>.yaml` — node-signed signer
  records (publisher, operator, project author).
- `<system>/.ai/node/trust/events/` — node-signed hash-chained key events.

System-space artifacts installed or registered by `ryeos init`:

- `<system>/.ai/bundles/<name>/.ai/` — installed bundle content.
- `<system>/.ai/node/bundles/<name>.yaml` — signed registrations.
- `<system>/.ai/node/identity/private_key.pem` — node key.
- `<system>/.ai/node/vault/private_key.pem` — vault X25519 key.
- `<system>/.ai/node/policies/` — one complete node-signed semantic policy
  generation, seeded from the selected init profile on first publication.

Daemon-local artifacts `ryeosd` may repair after init verification:

- `<system>/.ai/node/config.yaml`
- `<system>/.ai/node/identity/public-identity.json`
- `<system>/.ai/node/vault/public_key.pem`
- `<system>/.ai/node/auth/authorized_keys/<user-fp>.toml`
- daemon-local layout directories

The daemon must not write signer records or regenerate the node key.

## Major directories

- `.ai/bundles/` — installed bundles.
- `.ai/node/config.yaml` — daemon config.
- `.ai/node/policies/` — atomic node-owned policy generation, including
  isolation, execution, ingest, retention, maintenance, and hosted authority.
- `.ai/node/identity/` — node key and public identity.
- `.ai/node/auth/authorized_keys/` — node-signed authorized callers.
- `.ai/node/vault/` — vault key material.
- `.ai/node/bundles/` — signed bundle registrations.
- `.ai/state/runtime.sqlite3` — exact-current execution/session state and the
  live credential-profile lease projection.
- `.ai/state/operational.sqlite3` — retained source-of-truth operational state,
  including provider-neutral credential-profile lifecycle authority.
- `.ai/state/private-artifact-homes/` — daemon-owned opaque workload homes;
  credential bytes remain here and are never copied into profile metadata.
- `.ai/state/scheduler.sqlite3` — scheduler database.
- `.ai/state/objects/` and `.ai/state/refs/` — CAS.
- `.ai/state/cache/executions/` — request-owned pushed-head and no-project
  workspaces; guards remove them when their request/cache ownership ends.
- `.ai/state/secrets/` — sealed vault data.
- `.ai/state/audit/` — append-only audit trail.
- `.ai/state/schedules/` — scheduler state.
- `.ai/state/daemon/` — daemon-process-local state, including the default
  app-root-scoped `ryeosd.sock` control endpoint, its callback-only `ryeosd-runtime.sock` sibling, and the distinct `lifecycle/`
  operation-lock inode. The live StateStore retains `.ai/state` itself, so the
  lifecycle controller must never reuse that parent lock.

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
