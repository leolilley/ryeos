---
category: ryeos/core/daemon
tags: [daemon, state, cas, sqlite, vault]
version: "1.0.0"
description: >
  Daemon state directory layout and what persists across restarts.
---

# Daemon State Directory

Invariant: daemon-owned state lives under the configured system space, not inside ephemeral execution working directories.

## Major directories

- `.ai/node/config.yaml` — node bind address, database path, and daemon config.
- `.ai/node/identity/` — node Ed25519 identity keys and public identity document.
- `.ai/node/auth/authorized_keys/` — trusted operator keys.
- `.ai/node/vault/` — vault key material.
- `.ai/node/bundles/` — installed bundle registrations.
- `.ai/state/runtime.sqlite3` — thread, event, and projection database.
- `.ai/state/objects/` and `.ai/state/refs/` — CAS object store and refs.
- `.ai/state/secrets/` — sealed vault data.
- `.ai/state/audit/` — append-only audit trail.
- `.ai/state/schedules/` — scheduler fire history and schedule state.

## Execution state

Per-thread state directories are daemon-owned. This lets checkpoints survive working-directory cleanup and daemon restart, which is especially important for pushed-head and resume flows.
