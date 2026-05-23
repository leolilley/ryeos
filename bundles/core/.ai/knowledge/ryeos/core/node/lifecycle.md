---
category: ryeos/core/node
tags: [node, lifecycle, init, start, stop, status, ryeos-node]
version: "1.0.0"
description: >
  Local node lifecycle semantics owned by the ryeos-node crate: init,
  start, stop, status, liveness, daemon metadata, and CLI preflight.
---

# Local Node Lifecycle (`ryeos-node`)

`crates/core/node` (`ryeos-node`) is the single owner of local-node
lifecycle and bootstrap semantics. The supported user lifecycle surface
is exactly four verbs:

```bash
ryeos init
ryeos start
ryeos stop
ryeos status
```

There is no `restart`, no enable/disable command, no init-system
integration, and no separate probe command. `ryeos status` is the only
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

## `ryeos status`

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

`stop` sends shutdown to the UDS that just proved the daemon is live; it
never blind-fires at stale `daemon.json` paths. Default graceful timeout
is 10 seconds.

With `--force`, after graceful timeout, stop re-confirms live PID through
a fresh `lifecycle.status` RPC immediately before signalling. It fails
closed if no live daemon responds, response status is not `running`, or
no PID is present. On Unix it verifies `/proc/<pid>/comm == "ryeosd"` or
`/proc/<pid>/exe` basename `ryeosd` before `SIGTERM`. `ESRCH` is benign.

Generation tokens and pidfd/SO_PEERCRED hardening were deliberately not
added; fresh-RPC reconfirm plus `/proc` verification is the chosen
portable floor.

## Lifecycle RPC timeout

`LocalLifecycleEnv::RPC_TIMEOUT` is a single 750 ms bound covering the
whole UDS round trip: connect, write, read, and decode.

## CLI daemon-backed preflight

Normal daemon-backed CLI dispatch first checks local lifecycle status
unless `RYEOSD_URL` is set. If not `Running`, it fails before signing with
guidance to run `ryeos init` or `ryeos start`. `RYEOSD_URL` bypasses this
preflight for normal dispatch only; lifecycle verbs still ignore it.
