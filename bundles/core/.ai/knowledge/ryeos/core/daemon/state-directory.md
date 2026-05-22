<!-- ryeos:signed:2026-05-22T07:21:24Z:71d59deaab20211d0fd8f5737244c3d651ad112c788378da757b2a18d09eca50:flfrCEZn8xR9soJhTdB4hM7P3H5psipk51rbu9wScYcXQ4UbPknmlWW+92BVypl0wZ8cB9wnhqHWG1kFO9ZfDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/daemon
tags: [daemon, state, cas, sqlite, vault, locks, ownership]
version: "2.0.0"
description: >
  Daemon state directory layout, file ownership, lifecycle locks, and what
  persists across restarts.
---

# Daemon State Directory

Daemon-owned state lives under the configured system space, not inside
ephemeral execution working directories. Operator-owned user state lives
under the user root.

## Ownership split

User-space artifacts owned by `ryeos init`:

- `<user>/.ai/config/keys/signing/private_key.pem` — user/CLI key.
- `<user>/.ai/config/keys/trusted/<fp>.toml` — publisher, user, and node
  trust docs.

System-space artifacts installed or registered by `ryeos init`:

- `<system>/.ai/bundles/<name>/.ai/` — installed bundle content.
- `<system>/.ai/node/bundles/<name>.yaml` — signed registrations.
- `<system>/.ai/node/identity/private_key.pem` — node key.
- `<system>/.ai/node/vault/private_key.pem` — vault X25519 key.
- `<system>/.ai/node/ingest/ignore.yaml` — ingest-ignore config.

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
- `.ai/node/identity/` — node key and public identity.
- `.ai/node/auth/authorized_keys/` — node-signed authorized callers.
- `.ai/node/vault/` — vault key material.
- `.ai/node/bundles/` — signed bundle registrations.
- `.ai/node/ingest/ignore.yaml` — ingest-ignore rules.
- `.ai/state/runtime.sqlite3` — thread, event, and projection database.
- `.ai/state/scheduler.sqlite3` — scheduler database.
- `.ai/state/objects/` and `.ai/state/refs/` — CAS.
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
