<!-- ryeos:signed:2026-05-22T03:35:35Z:b9bc61c81874fe2dfa851103d6128859c028438b91f58f4dab885cfe0419be47:3UUqtuzIGuxF/vDED1wUtAw/lnglBcH59XlqAdNgvD3X6CQU/3MQFrITd/T3JE96M7txHWk5wAXncMtc4iMhBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
