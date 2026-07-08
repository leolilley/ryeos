<!-- ryeos:signed:2026-06-24T04:51:58Z:d7cce1ea3f8ae2bc2a97eb9c5e85d9dd01174c007333a2d5063cbab1fe1dc3aa:0kbujKEJ/SX7xLzr1otzGrTd8d6bpnizMHPKdzpoCo4YnEdaEDnSHDBWekBlXPCxMgoOhhETZhtTNKcU1xAVAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/daemon
tags: [daemon, startup, shutdown, lifecycle, state-lock, uds]
version: "2.0.0"
description: >
  Daemon process lifecycle: strict startup ordering, local lifecycle RPC,
  daemon.json metadata, and shutdown cleanup.
---

# Daemon Process Lifecycle

`ryeosd` is the long-running runtime process. It owns the HTTP API, UDS
runtime callbacks, execution state, scheduler, CAS projection, and
service registry. The local user lifecycle verbs are owned by
`ryeos-node`; see [Local Node Lifecycle](../node/lifecycle.md).

## Strict startup order

Daemon startup is fail-closed and side-effect-minimal until operator
initialization has been verified:

1. `Cli::parse` and `Config::load` — side-effect-free config.
2. `bootstrap::verify_initialized` — requires signed bundle
   registrations and bails with `Run: ryeos init` guidance if absent.
3. Subcommand dispatch — `run-service` standalone takes its own state
   lock immediately after init verification.
4. Acquire daemon state lock before removing any socket.
5. Initialize tracing/file sink only after init verification and state
   lock acquisition.
6. `bootstrap::repair_daemon_local` — repair only daemon-local artifacts
   and fail when operator init artifacts are missing.
7. Remove stale configured socket and ensure runtime paths.
8. Two-phase node-config bootstrap, engine construction, service
   self-check, route table build, listeners, scheduler, and metadata
   write.

The state-lock-before-socket-unlink ordering prevents a second daemon
from unlinking the first daemon's live socket.

## State lock

The daemon holds `<system>/.ai/state/operator.lock` for its lifetime.
`StateLock` opens with `truncate(false)`, acquires `flock` first, and
writes its PID only after winning, so a losing process can read the
holder PID.

## Daemon-local repair

`repair_daemon_local` is not init. It verifies operator-owned artifacts
created by `ryeos init` and repairs only daemon-local files. See
[Daemon Bootstrap](bootstrap.md).

## Local lifecycle UDS RPC

The daemon UDS server exposes local lifecycle methods:

- `lifecycle.status` — returns `status: "running"`, PID, version,
  started timestamp, bind address, and UDS path.
- `lifecycle.shutdown` — accepts graceful shutdown.

These are local UDS control messages, not public HTTP routes.

## `daemon.json`

After listeners bind, the daemon writes `<system>/daemon.json` with PID,
UDS path, HTTP bind address, start time, version, and system space. It is
a discovery hint only; lifecycle code confirms liveness through
`lifecycle.status`. On normal shutdown the daemon removes `daemon.json`
and the configured UDS socket best-effort.

## Runtime shutdown

Shutdown can be triggered by Ctrl-C/SIGTERM or `lifecycle.shutdown`. The
daemon stops serving, drains running threads, applies configured process
shutdown actions, removes metadata/socket files best-effort, and exits.
