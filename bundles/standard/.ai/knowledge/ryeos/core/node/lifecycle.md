<!-- ryeos:signed:2026-09-13T09:38:49Z:c193781f9f085963b8022341400a103093d1c2edee5254058674dc878fe6646a:MOMManE0+uMjg2vz6pyTqMa3orkTT81MMWhmiDBAfvA6cxofbrUQAII90GVC956Z/IijeYvQ+KLvGo73y2SNDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, lifecycle, init, start, stop, status, ryeos-node]
version: "1.5.0"
description: >
  Local node lifecycle semantics owned by the ryeos-node crate: init,
  start, stop, status, liveness, daemon metadata, and CLI preflight.
---

# Local Node Lifecycle (`ryeos-node`)

`crates/daemon/ryeos-node` (`ryeos-node`) is the single owner of local-node
lifecycle and bootstrap semantics. The supported user lifecycle surface
is the ordinary four-verb surface below, plus one explicit administrator
transition for nodes that require host supervision:

```bash
ryeos init --node-profile full
ryeos start
ryeos stop
ryeos node status
ryeos node host setup --confirm
```

There is no `restart`, no enable/disable command, and no separate probe
command. `ryeos node status` is the only lifecycle read operation. Lifecycle
operations are local-node operations and intentionally ignore `RYEOSD_URL`;
that variable only steers normal daemon-backed dispatch. Native service-manager
and process-scope mechanics belong wholly to Lillux. RyeOS retains only the
generic protected association and lifecycle/upgrade testimony.

`full` above is the native-package distribution selector. Every fresh node
requires one explicit publisher-signed init profile whose exact bundle inventory
matches its distribution; image and package entrypoints select their mapped
profile rather than asking Rust to infer policy from bundle presence. An already
initialized node may rerun init without a selector only when its signed policy
generation already exists.

## Public crate surface

`ryeos-node` exposes:

- `NodeConfig` — side-effect-free local config wrapper around
  `ryeos_app::config::Config::load`.
- `LocalLifecycleEnv` — UDS candidate ordering, best-effort
  `daemon.json` hint reading, lifecycle RPC timeout, and start-lock
  acquisition.
- `LifecycleController` — controller for `init`, `init_state`,
  `require_initialized`, `status`, `start`, and `stop`, including optional
  progress-observer variants for startup and shutdown.
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

For a node without a host association, `start` spawns `ryeosd` directly with
resolved local config. For an explicitly supervised node, it publishes Up
through its exact protected association and asks Lillux to submit that intent
to the selected native supervisor. Missing, changed, or unhealthy supervision
fails closed; it never falls back to a direct process. Both paths wait for
readiness through the same `status` liveness contract. A directly launched
child that exits early is reported immediately after one concurrency probe.
The readiness timeout is 15 minutes so verified projection recovery can finish.

When stdout and stderr are interactive terminals, the CLI consumes the typed
lifecycle observer and redraws one compact `RYE/OS BOOT` line. The bar follows
the real `StartupPhase` enum and uses real chain counters when the daemon
publishes them. Redirected and non-interactive calls do not emit cursor control
sequences or timing-dependent progress lines.

## `ryeos stop`

`stop` first establishes that the local daemon is live, then connects to a
configured UDS candidate and asks Lillux to authenticate and pin that exact
local peer. `daemon.json` and the PID returned by `lifecycle.status` are never
signal authority. Platform-specific peer credentials and process handles do
not appear in RyeOS lifecycle code.

The normal path sends `SIGTERM` through the peer pidfd. That enters the daemon's
graceful shutdown coordinator, closes new runtime authoring, stops listeners,
and drains attached workloads. The default wait is 10 seconds.

With `--force`, expiry of that wait causes a fresh socket connection and fresh
Lillux peer/process capture before native force termination. Stop then waits
another two seconds for disappearance. It fails closed when no configured
socket has a verifiable live `ryeosd` peer. There is no numeric-PID or
stale-metadata fallback. A supervised stop publishes Down before terminating
the exact process, preventing the native supervisor from bouncing it.

Interactive shutdown uses the same presentation boundary: `RYE/OS HALT`
animates while verified lifecycle probes still see the daemon and completes
only after the process/state authority is gone. This does not alter the
pidfd-based stop contract.

The first qualified Lillux backend uses Linux pidfd and peer-credential
primitives; other platforms must provide the equivalent Lillux contract before
RyeOS claims support. See [Platform Support](../platform-support.md).

## Host setup and package upgrades

`ryeos node host setup --confirm` is a one-time administrator transition for an
existing node that needs a supervised Lillux process-scope delegation. The app
root remains account-owned and may remain under the user's normal data root.
The administrator-owned association pins its exact app root, node identity,
account, daemon executable, native supervisor, and scope provider. Projects,
workers, bundles, and node policy cannot select any of those host authorities.

Package installation stages and validates its prospective generation before it
creates durable installation inhibition or stops a supervised node. The
installer must prove that the association's pinned daemon path is the exact
package-owned path it will replace. A service pinned to a different prefix is a
different installation authority and is refused before lifecycle mutation; the
installer neither copies into that prefix nor claims it upgraded the service.
The retained journal binds the expected image digest and original Up/Down
intent, and is removed only after the installed image proves the restored
state. Interrupted upgrades remain inhibited and retryable.

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
