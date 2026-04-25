# native_resume Advanced Path: Closing the Spawn/Attach Gap

**Date**: 2026-04-25
**Status**: Future reference — not for current implementation.
**Prerequisite**: Phase 6 `native_resume` (`runner::run_existing_detached` +
`ResumeIntent` + DB-backed `resume_attempts` counter + snapshot-pinned
`ResumeContext`) must be implemented and proven insufficient before any
of this applies.

---

## When to evolve

The Phase 6 design accepts one well-documented limitation: the
**spawn/attach gap**.

```
                  daemon                          OS
─────────────────────────────────────────────────────────
   1. fork/exec subprocess  ─────────►  child running, pid=P
   2. attach_process(pid=P)
       writes pid/pgid into runtime_db
   ─────────────────────────────────────────────────────
   ▲ if daemon dies in this window:
     • child (pid=P) is alive but unknown to runtime_db
     • on next reconcile, daemon has no record of P
     • a `Resume` decision will start a SECOND child
     • → duplicate work, possibly double side effects
```

This is **pre-existing** in the detached path; `native_resume` only
makes it more visible because resume is the codepath where “did the
last spawn actually attach?” matters most.

The Phase 6 mitigation is pre-bumping the `resume_attempts` counter so
the failure mode is *bounded retries*, not *unbounded duplicate
spawns*. That is sufficient for tools whose work is idempotent or
tolerant of being run twice (the common case for `native_resume`
targets, which by declaration are already replay-aware).

Only evolve beyond Phase 6 when one of these conditions holds:

| Condition                                           | Symptom                                                                                                                            |
| --------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| Tools with **non-idempotent external side effects** | A duplicate spawn after a daemon crash causes user-visible double-billing, double-publish, double-write, etc.                       |
| Resumed tools must call back to the daemon **before producing side effects** | Tool semantics require “tell me my real thread_id and I'll commit only then”; loose attach is a wedge for race conditions.         |
| Operator-visible resume state across machines       | Federation / cluster path needs an authoritative “is this thread currently executing somewhere” answer that survives daemon crash. |
| Cross-version resume robustness                     | Daemon binary upgrades during in-flight execution must preserve resume semantics; pre-bump-and-pray is no longer acceptable.       |
| Strict exactly-once semantics                       | Resume must be observably exactly-once for downstream consumers (e.g. ledgered side effects, exactly-once event publication).       |

If none of these are true, Phase 6's pre-bump model is the right
answer. Do not over-engineer.

---

## Evolution 1: Wrapper-launcher with self-registration

**Trigger**: First condition above (non-idempotent side effects on
duplicate spawn) starts causing real incidents.

The daemon stops `exec()`-ing the tool directly. Instead it spawns a
tiny `ryeos-resume-launcher` binary that:

1. Receives a one-shot **launch token** (signed, short-lived) from the
   daemon as an env var or fd.
2. Connects back to the daemon over UDS BEFORE executing the tool.
3. Submits its own `pid` and `pgid` (from `getpid()` / `getpgid(0)`)
   together with the launch token. Daemon validates the token and
   atomically writes the pid/pgid into `runtime_db`.
4. Only after the daemon ACKs the registration does the launcher
   `execve()` into the actual tool binary.
5. If the launcher cannot register (daemon dead, token expired) it
   exits non-zero before any tool code runs.

```
   daemon                       launcher                    tool
─────────────────────────────────────────────────────────────────
   spawn(launcher, token)  ──►  recv(token)
                                connect UDS  ──►  daemon
                                                  validate token
                                                  attach_process
                                ◄── ACK
                                execve(tool)  ──►  real work begins
```

Key property: **after daemon crash + restart, no live child exists
that wasn't `attach_process`'d.**

Either:

- the launcher registered → the daemon knows about it; reconcile sees
  a live or dead pid and routes correctly;
- the launcher failed to register → no tool code ever ran → no side
  effects → safe to retry from scratch.

### Required pieces

- A `ryeos-resume-launcher` binary in this workspace that the daemon
  ships alongside `ryeosd`.
- A `runtime.attach_with_token` UDS RPC that takes
  `(launch_token, pid, pgid)` and returns OK only on first successful
  attach (idempotent on the same token).
- A signed token format with thread_id + nonce + expiry; reuse
  `lillux::crypto` and the node identity key.
- Token issuance side: `runner::run_detached` and
  `run_existing_detached` mint the token, persist it (alongside
  `RuntimeLaunchMetadata`?) so reconcile after a daemon crash can
  recognize a launcher trying to attach for an in-flight resume.

### Cost

- New binary, new RPC, new token plumbing.
- Slight per-spawn latency overhead (one extra UDS round-trip before
  exec).
- More moving parts to keep signed and trusted.

Worth it iff the “duplicate spawn” class of bugs is causing real
operator pain.

---

## Evolution 2: Daemon-side fork supervisor

**Trigger**: Wrapper-launcher works but the extra UDS round-trip is
showing up in latency-sensitive workloads, OR you need a single
authoritative point that holds the OS-level child handle.

Move spawning into a **dedicated daemon child process** — a long-lived
“supervisor” that the main daemon forks at startup. The supervisor:

- holds the only Unix-level wait()-able handle on every tool process,
- never accepts external network input,
- communicates with the main daemon over a private socketpair,
- survives main-daemon restarts (re-parented to PID 1 by `prctl(PR_SET_CHILD_SUBREAPER)` on the OS level if available).

This eliminates the spawn/attach gap structurally: the supervisor has
the pid in hand the moment fork() returns, before any UDS round-trip
or DB write. The DB write is then a journal of an already-known fact.

This is materially more complex (process model, supervisor lifecycle,
re-attach protocol after main-daemon restart) and is only justified
when both Evolution 1's overhead and its remaining failure modes
(token loss, launcher crash before register) are observably
problematic.

---

## Evolution 3: Cluster-aware resume

**Trigger**: Multi-node / federation; resume must work across machine
restarts AND across machines.

Out of scope for this document. Plug into `cluster-and-federation.md`
once that work begins. The relevant invariant to preserve:

- `runtime_db.thread_runtime.launch_metadata` becomes a
  cluster-replicated record;
- the `resume_attempts` counter must be bumped via the cluster's write
  barrier, not a local file;
- `ResumeContext.project_context` carries a cluster-resolvable
  `ProjectRef` (project hash + node-set), not a local
  `LocalPath { path }`.

---

## What the Phase 6 design deliberately does NOT promise

So that future implementers know what is in scope:

1. **Exactly-once spawn** — see "spawn/attach gap" above. Phase 6
   gives bounded-duplicate (≤ `max_auto_resume_attempts` extra spawns)
   in worst-case crash sequences.
2. **Cross-binary-version resume** — `RuntimeLaunchMetadata` carries a
   `schema_version` and decode failures are logged loudly, but
   migration of in-flight resume state across daemon binary versions
   is best-effort. Operators upgrading mid-flight should expect some
   threads to fall back to `Finalize` rather than auto-resume.
3. **Latest-head resume** — Phase 6 pins resume to the original
   project snapshot (`ResumeContext.project_context` carries the
   original `ProjectContext`, including snapshot hash for
   `LocalPathWithSnapshot`-style references). Resume against the
   current project head is a separate, opt-in policy that can be
   added later without changing the wire format (additive serde
   field on `NativeResumeSpec`).
4. **Side-effect rollback** — checkpoints record what the tool
   chose to record. The daemon does not snapshot the tool's external
   effects (filesystem, network, billing systems). That is the tool's
   responsibility per `native_resume` declaration.

---

## Bottom line

Phase 6's pre-bump-then-spawn-then-attach model is correct for the
target workload (replay-aware, idempotent-tolerant tools that opted
in to `native_resume`). The wrapper-launcher path closes the
spawn/attach gap **structurally** and is the right next step the
moment the assumption "duplicate spawn is acceptable" stops holding.

Do not implement Evolution 1 preemptively. Implement it the first
time a real `native_resume` tool reports a duplicate-side-effect
incident traceable to a daemon crash.
