<!-- ryeos:signed:2026-07-21T00:24:30Z:c3c2bd6024dcdcd8bd55ea5244b5aee957f2ccb1433f77dc0b1b3f038cb981bc:V/yFCvTEJ1OAE0ABVD9wLSX02u5CN2ofSKres12rEExHBc3w44hnc5xKmWzRFFqIuGWf3rzU993vT0jBI0lJBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/execution
tags: [execution, process, lifecycle, attachment, recovery, lillux]
version: "1.0.0"
description: >
  The attachment-before-execution contract that makes every daemon-owned
  process durably nameable before target code can run.
---

# Attachment Before Execution

RyeOS creates every daemon-owned executable process at an explicit lifecycle
boundary:

```text
spawn awaiting attachment
  -> obtain the exact target and process-group identity
  -> persist that identity under the thread's launch owner
  -> authorize release against stop and shutdown state
  -> release the target to execute
```

Target code cannot execute before the durable attachment succeeds. This closes
the former local crash window between kernel process creation and publication
of process ownership: a daemon crash before attachment keeps the target held
and causes it to die with its parent; a crash after attachment leaves an exact
durable identity for startup reconciliation.

## Lifecycle and isolation are independent

Attachment owns process lifecycle. Isolation owns the target's filesystem,
network, device, environment, and resource authority. Neither substitutes for
the other.

- With node isolation disabled, Lillux provides a native pre-exec attachment
  boundary for the direct target.
- With isolation enforced, the selected signed backend holds its actual target
  and reports that target's exact host identity through the strict isolation
  protocol.
- A backend that cannot prove an attachment boundary is rejected for an
  attachment-required launch. RyeOS never silently falls back to a different
  process or execution mode.

Bubblewrap is optional. No bundle name, backend name, helper binary, binary
reference, PATH lookup, or package layout is foundational to durable process
ownership.

## Ownership split

Lillux owns the process mechanics:

- process creation, sessions, groups, and exact birth identity;
- the pre-exec hold and consuming release transition;
- output capture, timeout, termination, group quiescence, and reaping;
- fail-closed cleanup when attachment or release fails.

RyeOS owns the durable policy:

- whether a launch requires attachment;
- the thread and launch owner allowed to attach it;
- persistence of the exact PID, PGID, boot identity, and start-time ticks;
- the stop/shutdown check immediately before release;
- cancellation, recovery, and terminal settlement.

The public type state mirrors the real lifecycle. A pending process can be
released or aborted; it cannot be waited as though it were running. Release
consumes the pending value and produces a running process, so waiting before
attachment, releasing twice, and treating a missing hold as normal are not
representable transitions.

## Direct launch sequence

For a direct host launch, Lillux builds the ordinary authoritative command and
runs `Command::spawn` on a short-lived worker. A final audited pre-exec hook
reports readiness and waits on a private release channel. The daemon opens and
retains a pidfd while the held child is provably alive, validates the reported
PID/PGID/session identity, and commits that identity to the runtime store.

After attachment, the daemon rechecks that the exact launch owner still holds
authority, the thread is nonterminal, no stop intent exists, and shutdown has
not closed process release. Only then does Lillux release the child to complete
exec. Control descriptors are close-on-exec and do not enter target code.

For an isolated launch, the same RyeOS transition applies to the exact target
reported by the adapter. Lillux retains wrapper and process-group ownership
until target exit, group quiescence, and leader reap are all proven.

## Failure and recovery

Attachment failure aborts the held process and proves exact target exit,
process-group quiescence, and leader reap before durable ownership can be
cleared. Cleanup is fail-closed: an unkillable process may delay its owner or
graceful shutdown, but RyeOS never reports the identity as cleared while it can
still execute.

After an unclean daemon exit, the exclusive node-state lock establishes the
previous daemon's death. Startup checks each durable process identity with
pidfds and recorded birth facts, terminates exactly matched orphan groups, and
only then performs recovery launch. A same-boot identity that cannot be proven
is quarantined rather than guessed, cleared, or signalled by numeric PID alone.

The contract is kind-agnostic. Graphs, directives, tools, callbacks, follow and
fanout children, continuation successors, recovered roots, synchronous
requests, and detached requests all use the same boundary when the daemon owns
their process lifecycle.

## Exact persisted contracts

Process attachment and recovery authority are exact current-format durable
data. Readers inspect the enclosing object kind and schema epoch before typed
decoding; unknown fields, predecessor epochs, and noncanonical bytes fail
closed. RyeOS has no compatibility decoder or in-place reinterpretation for
these contracts. A clean-cut change requires the explicit offline
thread-history and project-head reset described by the node diagnostic.

See also [Execution Isolation](../node/execution-isolation.md),
[Daemon Process Lifecycle](../daemon/lifecycle.md), and
[CAS Architecture](../state/cas-architecture.md).
