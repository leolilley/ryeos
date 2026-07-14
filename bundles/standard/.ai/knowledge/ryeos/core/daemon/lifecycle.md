<!-- ryeos:signed:2026-07-14T10:12:30Z:6cf84c9cc834f0b858bff75218abb0adb972ef6237830a5bb690d37fff0bef04:bWlTu9VkDMJCwzOHQoOH8oTbZuHOkMa6f3iZxq58R6+S1Qz/QU9W5yy6kZ4mopCQO/tSko3NHhcXjfR+FZ2lAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/daemon
tags: [daemon, startup, shutdown, lifecycle, state-lock, uds]
version: "2.2.0"
description: >
  Daemon process lifecycle: strict startup ordering, local lifecycle status,
  exact signal control, daemon.json metadata, and shutdown cleanup.
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
2. Prove Linux pidfd process-group signalling and `SO_PEERPIDFD` support;
   unsupported hosts fail before initialization repair or executable work.
3. `bootstrap::verify_initialized` — requires signed bundle
   registrations and bails with `Run: ryeos init` guidance if absent.
4. Subcommand dispatch — `run-service` standalone takes its own state
   lock immediately after init verification.
5. Acquire daemon state lock before removing any socket.
6. Initialize tracing/file sink only after init verification and state
   lock acquisition.
7. `bootstrap::repair_daemon_local` — repair only daemon-local artifacts
   and fail when operator init artifacts are missing.
8. Remove stale configured socket and ensure runtime paths.
9. Two-phase node-config bootstrap, engine construction, and sandbox-policy
   snapshot resolution. Invalid policy fails startup; disabled mode does not
   require or inspect Bubblewrap.
10. Service self-check, route table build, listeners, scheduler, and metadata
   write.

The state-lock-before-socket-unlink ordering prevents a second daemon
from unlinking the first daemon's live socket.

The sandbox snapshot is immutable for the process lifetime. Operators validate
edits with `ryeos node doctor` and restart before expecting a new policy
generation. See [Execution Sandbox](../node/execution-sandbox.md).

## State lock

The daemon holds `<system>/.ai/state/operator.lock` for its lifetime.
`StateLock` opens with `truncate(false)`, acquires `flock` first, and
writes its PID only after winning, so a losing process can read the
holder PID.

## Daemon-local repair

`repair_daemon_local` is not init. It verifies operator-owned artifacts
created by `ryeos init` and repairs only daemon-local files. See
[Daemon Bootstrap](bootstrap.md).

## Local lifecycle UDS surface

The daemon UDS server exposes one read-only lifecycle method:

- `lifecycle.status` — returns `status: "running"`, PID, version,
  started timestamp, bind address, and UDS path.

It is a local UDS status message, not a public HTTP route and not signal
authority. There is deliberately no unauthenticated shutdown RPC because
sandboxed runtimes may receive the callback socket. Local `ryeos stop` captures
the connected socket peer through `SO_PEERCRED` and `SO_PEERPIDFD` and signals
that exact process incarnation.

## `daemon.json`

After listeners bind, the daemon writes `<system>/daemon.json` with PID,
UDS path, HTTP bind address, start time, version, and system space. It is
a discovery hint only; lifecycle code confirms liveness through
`lifecycle.status`. On normal shutdown the daemon removes `daemon.json`
and the configured UDS socket best-effort.

## Runtime shutdown

Graceful shutdown is triggered by Ctrl-C or `SIGTERM`. An early signal watcher
is installed before slow bootstrap work. Once application state exists, the
coordinator first closes the durable shutdown gate so no new root thread,
callback mutation, continuation, or scheduler launch can be admitted. It then
stops the HTTP and UDS listeners, lets already-decoded UDS requests finish under
the same absolute listener deadline, drains attached process groups within the
node-owned bound, removes metadata/socket files best-effort, and exits. A forced
wire-task abort does not abort an admitted request's execution owner; the closed
attachment gate and exact-identity drain still own any process it can spawn. A
clean lifecycle marker is written only when every required drain step completes.

`ryeos stop --force` may escalate to pidfd `SIGKILL` after its timeout. That
terminates the daemon immediately and therefore cannot promise graceful drain;
the durable exact process identities and exclusive startup lock drive the next
startup's orphan teardown and reconciliation.
