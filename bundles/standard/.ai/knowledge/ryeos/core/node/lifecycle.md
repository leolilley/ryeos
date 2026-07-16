<!-- ryeos:signed:2026-07-16T02:18:48Z:1f61b3edfe8ca830196410da6285d243d74c5833e90848bc6cebd9dd3a1d4dce:AkvB+0WY32i6pAKd3/niISOJ3UH209BgRgwE4UCHO4J3U7qXTxZJgiBAI4HefToGF5lgxJCl9cPugA7AZbA/DQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, lifecycle, init, start, stop, status, ryeos-node]
version: "1.2.0"
description: >
  Local node lifecycle semantics owned by the ryeos-node crate: init,
  start, stop, status, liveness, daemon metadata, and CLI preflight.
---

# Local Node Lifecycle (`ryeos-node`)

`crates/daemon/ryeos-node` (`ryeos-node`) is the single owner of local-node
lifecycle and bootstrap semantics. The supported user lifecycle surface
is exactly four verbs:

```bash
ryeos init
ryeos start
ryeos stop
ryeos node status
```

There is no `restart`, no enable/disable command, no init-system
integration, and no separate probe command. `ryeos node status` is the only
lifecycle read operation. Lifecycle operations are local-node operations
and intentionally ignore `RYEOSD_URL`; that variable only steers normal
daemon-backed dispatch.

## Public crate surface

`ryeos-node` exposes:

- `NodeConfig` — side-effect-free local config wrapper around
  `ryeos_app::config::Config::load`.
- `LocalLifecycleEnv` — UDS candidate ordering, best-effort
  `daemon.json` hint reading, lifecycle RPC timeout, and start-lock
  acquisition.
- `LifecycleController` — controller for `init`, `init_state`,
  `require_initialized`, `status`, `start`, and `stop`.
- `init::run_init` — authoritative operator init.
- `init_check::{init_state, require_initialized}` — initialized-state
  checks based on signed bundle registrations.
- `DaemonMetadata` — `<system>/daemon.json` hint contract.
- `LifecycleStartLock` — flock-backed start coordination.

## Initialization state

A node is initialized when the system space exists and
`<system>/.ai/node/bundles/` contains at least one signed YAML bundle
registration. Missing system space, missing registration directory, or no
signed registrations returns `NotInitialized` with `Run: ryeos init`
guidance. Bundle names are not hardcoded.

## `ryeos node status`

`status` is strictly read-only: no directory creation, no metadata
writes, no repair, no socket cleanup.

Status flow:

1. Check init state; if missing, return `NotInitialized`.
2. Read `<system>/daemon.json` as a best-effort hint. Missing,
   unreadable, or malformed metadata is treated as no hint; malformed
   metadata is logged at debug and never fatal.
3. Probe UDS candidates in order: metadata `uds_path` first, configured
   `uds_path` second, deduped.
4. Call `lifecycle.status` on each candidate within the lifecycle RPC
   timeout.
5. Trust only responses that explicitly report `status: "running"`.
   Off-contract responses fail closed.
6. Live response fields override stale metadata fields.
7. If no candidate responds and metadata exists, return `Stale`;
   otherwise return `Stopped`.

`daemon.json` is a discovery hint, not liveness truth.

## `ryeos start`

`start` is idempotent and concurrent-safe. It fails if not initialized,
succeeds immediately if already `Running`, and coordinates concurrent
starters with `<system>/.ai/state/lifecycle-start.lock` using
`flock(LOCK_EX | LOCK_NB)`. The lock is released on process exit, so a
crashed starter cannot wedge future starts.

`start` spawns `ryeosd` directly with resolved local config and waits for
readiness via the same `status` liveness contract. If the child exits
early, it re-probes once for concurrent-starter convergence, then
surfaces stderr immediately instead of holding the lock to the deadline.
Default readiness timeout is 15 seconds.

## `ryeos stop`

`stop` first establishes that the local daemon is live, then connects to a
configured UDS candidate and asks the kernel for that socket peer's
`SO_PEERCRED` PID and `SO_PEERPIDFD`. The pidfd is the process identity;
`daemon.json` and the PID returned by `lifecycle.status` are never signal
authority. The numeric peer PID is used only to verify that `/proc/<pid>/comm`
or `/proc/<pid>/exe` identifies `ryeosd`.

The normal path sends `SIGTERM` through the peer pidfd. That enters the daemon's
graceful shutdown coordinator, closes new runtime authoring, stops listeners,
and drains attached workloads. The default wait is 10 seconds.

With `--force`, expiry of that wait causes a fresh socket connection, fresh
kernel peer credentials, and fresh peer pidfd capture before `SIGKILL`
escalation. Stop then waits another two seconds for disappearance. It fails
closed when no configured socket has a verifiable live `ryeosd` peer. There is
no numeric-PID or stale-metadata fallback.

This contract requires the Linux pidfd and `SO_PEERPIDFD` primitives in the
supported-node baseline; see [Platform Support](../platform-support.md).

## Lifecycle RPC timeout

`LocalLifecycleEnv::RPC_TIMEOUT` is a single 750 ms bound covering the
whole UDS round trip: connect, write, read, and decode.

## CLI daemon-backed preflight

Normal daemon-backed CLI dispatch first checks local lifecycle status
unless `RYEOSD_URL` is set. If not `Running`, it fails before signing with
guidance to run `ryeos init` or `ryeos start`. `RYEOSD_URL` bypasses this
preflight for normal dispatch only; lifecycle verbs still ignore it.

Sandbox policy is not a live-reload surface. Validate edits with
`ryeos node doctor`, then stop/start the node so startup resolves a new immutable
snapshot. See [Execution Isolation](execution-isolation.md).
