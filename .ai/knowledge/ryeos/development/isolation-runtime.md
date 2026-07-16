<!-- ryeos:signed:2026-07-16T02:18:49Z:1fa00e4d244007145f53b632480292df9555fe7ff9930273161db83366343ce2:T26/Acaek0D2/0MqZQ+ImB7jJkDDO+sJORwl4vo3ErkYEyOnq1HlJBGGZ0XWjghxZ3JHnKT0Oazrsx1LP7KqBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/development
name: isolation-runtime
title: Node Isolation Runtime Architecture
entry_type: implementation_guide
version: "1.2.0"
description: Deep implementation map for the immutable RyeOS strict isolation, exact-byte execution boundary, launch coverage, path authority, and state durability.
tags:
  - isolation
  - bubblewrap
  - execution
  - node-policy
  - security
```

# Node Isolation Runtime Architecture

## Contract and ownership

The node owns one strict `version: 1` policy at
`<app-root>/.ai/node/isolation.yaml`. `ryeos init` creates the disabled default
only when the file is absent. This is the sole schema and activation source;
other versions, unknown fields, item-authored profiles, and per-request
overrides are rejected.

`IsolationRuntime::load` in `ryeos-engine/src/isolation.rs` first resolves one
canonical app-root identity, opens the fixed source below that canonical root
as a regular non-symlink file, strictly parses it, hashes the exact source, and
re-resolves the supplied app root before returning an immutable typed snapshot.
The supplied spelling is retained only as the namespace destination; a changed
canonical association refuses startup. Daemon startup uses `load_for_daemon`
to pin the configured callback UDS path into the same snapshot. Standalone and
offline callers use the shared registered-isolation composition path. Disabled
snapshots validate the strict schema and mandatory output/artifact limits but
do not require the selected bundle. Enforced snapshots resolve the selected
declaration from a registered signed bundle, capture the exact adapter and
launcher descriptors through the normal executor-manifest trust chain, require
signer continuity with the bundle manifest, and perform strict live protocol,
capability, version, feature, and artifact-digest inspection. Later launches
execute only those retained descriptors. The engine derives a backend-neutral
authority plan; backend-specific command construction belongs to the adapter.

```text
ryeos init
  -> create-once .ai/node/isolation.yaml

ryeosd main
  -> signed bundle resolution and adapter inspection
  -> IsolationRuntime::load_for_daemon(app_root, uds_path, backend)
  -> Arc<IsolationRuntime>
  -> AppState / EngineContext / runtime launch parameters

standalone service / offline dispatch
  -> IsolationRuntime::load(app_root)
  -> one immutable runtime for that process or command

all node-workload executable-item subprocess paths
  -> IsolationRuntime::apply(request, launch context)
  -> Lillux spawn/run
```

The daemon never watches or reopens the policy at spawn time. Status serializes
the running snapshot's mode, version, source path, and raw-source SHA-256, so an
edited on-disk generation is distinguishable from the generation in use.

Engine bootstrap similarly loads two explicit trust snapshots. `trust_store`
combines persistent node keys with project keys and any caller-scoped overlay
for item resolution. `node_trust_store` contains persistent node keys only and
is used to build installed kind/parser/handler/protocol/runtime registries and
to authorize native executor manifests. `Engine::with_trust_store` never
implicitly expands node trust; full-node constructors must pass
`with_node_trust_store` deliberately.

The strict isolation requires these runtime controls in `limits`:

```yaml
stdout_bytes: 8388608
stderr_bytes: 8388608
verified_artifact_file_bytes: 67108864
verified_artifact_total_bytes: 268435456
verified_artifact_files: 4096
```

The first two fields bound daemon-retained output for each stream. The
remaining fields bound a single exact artifact, aggregate exact-artifact bytes,
and unique artifact count in one runtime generation. Values must be positive,
and the aggregate artifact byte bound cannot be below the per-file bound.
`open_files` remains the optional per-process descriptor limit.

## Launch context and coverage

`IsolationLaunchContext` carries engine/daemon-derived facts only: canonical item
reference, one-component thread identifier, project authority and path,
optional state root and checkpoint directory, an optional typed daemon callback
socket, verified bundle roots, node trusted-key directory, and zero or more
`IsolationVerifiedCode` exact-byte identities. Socket authority is never inferred
from a child environment-variable name: an IPC-capable launch must carry the
exact socket fact and enforced apply compares it with the path pinned at daemon
startup. None of these fields can activate or relax the policy.

The shared runtime is applied at:

- execution-plan subprocess dispatch;
- normal, streaming, and managed runtime spawn;
- compose-context child runtime bootstrap;
- inline, background, detached-child, and follow-child execution;
- HTTP handler executable dispatch;
- callback-free streaming protocol executors, which now receive a real durable
  thread row and exact attached process identity rather than running as an
  untracked blocking subprocess;
- external parser/composer handler dispatch and boot-time handler validation;
- tool environment import probes; and
- offline executable tool and service dispatch.

Installed CLI client launchers intentionally remain outside this boundary: they
are local user-interface processes that require the invoking terminal/desktop
authority, not workloads launched by the node. Hosted execution never selects
that client path.

Maintainer-only bundle signing is also an explicit authoring boundary. It uses
`HandlerRegistry::load_base_for_authoring` with the compiled disabled runtime
because it may run before any node policy exists; it receives the maintainer's
local process authority and makes no OS-confinement claim. Node boot, preflight,
admission, doctor, item signing, and runtime handler dispatch do not use that
exception.

External parser/composer handlers are node-trusted infrastructure rather than
caller-selected executable items, but they still pass through the immutable
runtime. The registry retains each signed executor-manifest content hash, the
isolation captures those exact bytes, presents installed bundle roots read-only,
and suppresses every configured host writable mount through a no-project launch
authority. Their output then feeds the same signature, plan, and authorization
boundary before item code can run.

## Exact-byte execution invariant

Resolution records a whole-file SHA-256. `build_plan` rereads the requested
root only if its bytes still match that digest. Every executor-chain hop uses
the real Ed25519 verification path rather than trusting a claimed fingerprint.

When an executable plan reaches enforced `apply`, each `IsolationVerifiedCode`
provides host provenance and a whole-file digest. The runtime:

1. requires an absolute regular non-symlink source;
2. rereads it and requires the whole-file SHA-256 to match;
3. validates or creates a node-private per-runtime generation below
   `<app-root>/.ai/state/cache/verified-code`, then materializes the bytes at a
   content-addressed artifact path;
4. mirrors the containing project or bundle authority read-only beneath
   `/run/ryeos/verified-code/<authority-hash>`, overlays the exact artifact at
   the corresponding relative path after writable mounts, rewrites command
   arguments and environment values that name the host source, and refuses any
   remaining lexical, canonical, or resolvable source-path reference; and
5. refuses launch unless policy contains the exact `{verified_code}` surface.

This makes verification-to-exec mutation fail closed. The source path is
host-side provenance, not the execution authority. Verified node and bundle
commands without an exact content identity are refused. Non-system project or
node-selected commands are read once, copied into the same immutable store, and
executed only from their synthetic namespace path. Graph runtimes follow the
same invariant at the protocol layer: they parse
`LaunchEnvelope.resolution.root.raw_content` and never reopen a path supplied on
the command line.

The exact guarantee is deliberately scoped to the verified entry file and a
captured non-system executable. Its project or bundle authority mirror remains
a live read-only view, so transitive imports, shared libraries, interpreters,
and assets are not content-pinned by this mechanism.

One mutex protects each runtime artifact generation's content-address map,
unique-file and byte accounting, limit checks, and atomic publication. Backend,
verified-entry, and captured-command artifacts share the same 64 MiB per-file,
256 MiB aggregate, and 4,096-unique-file default budget. Concurrent applies
therefore cannot both pass against stale accounting or publish conflicting
content under one artifact name.

## Mount and path authority

Host sources and namespace destinations are deliberately separate. Sources are
canonicalized for overlap and containment checks. Destinations retain the
absolute spelling supplied by the policy or launch context, preserving a
process's expected path even when an app or project root was reached through a
symlink. Every app, project, cwd, state, checkpoint, socket, bundle, trusted-key,
policy, and generated-code destination must be an absolute root followed only
by normal components. Parent traversal is rejected before Bubblewrap can
reinterpret a destination inside its new root.

The generated readable surface consists of the exact public identity document,
the pinned callback socket when requested, verified bundle roots, the node
owner's trust directory when present, and the verified-code authority mirrors plus
their exact artifact overlays. Public identity, callback socket, policy source,
and checkpoint validations reject final-component symlinks. The callback socket
must be the exact socket pinned at daemon bootstrap; mounting an ancestor does
not satisfy its postcondition.

After canonical validation, every system or policy-selected readable or
writable source and every code mirror/artifact source is opened as an
`O_PATH|O_NOFOLLOW`
descriptor. Bubblewrap receives the descriptor number through `--ro-bind-fd`
or `--bind-fd`, binding the validated kernel object even if its pathname is
replaced before spawn. These handles keep `FD_CLOEXEC` in the multithreaded
parent. Lillux clears that bit only in the forked child's `pre_exec` hook, then
retains the handles through `Command::spawn`; an unrelated concurrent spawn
cannot inherit another isolation's mounts. Enforced apply refuses any inherited
descriptor not created by the isolation runtime itself.

Writable validation rejects filesystem root, protected host system roots, the
app root and sensitive state, backend/socket overlap, and the resolved host home
itself or an ancestor. Two app-root exceptions are provenance-bound:

- an exact request-owned directory directly below
  `.ai/state/cache/executions` with `RuntimeWorkspace` authority; and
- the exact daemon-derived `threads/<thread-id>/checkpoints` directory requested
  through `{checkpoint_dir}`.

Thread IDs must be one normal path component. The expected checkpoint path is
canonicalized and compared with its canonical-app-root spelling, so a symlink
in the descendant chain cannot redirect checkpoint authority.

There is no state-root placeholder. A state override must already exist and be
contained by the project or a node-policy absolute writable root. The
HTTP layer no longer creates a caller-selected state-root path before isolation
validation.

## Bubblewrap request construction

Enforced apply validates and filters the authoritative Lillux environment, then
constructs a Bubblewrap request with a private root, new user/IPC/UTS
namespaces, `/dev`, an empty `/proc` directory, and a private `/tmp`. `network.mode: isolated`
adds a network namespace; host mode does not. The host PID namespace is retained
intentionally because managed runtimes attach their PID over the pinned UDS and
the daemon uses host PID/PGID values for cancellation and restart
reconciliation. A new procfs cannot be mounted over a PID namespace owned by an
ancestor user namespace, while binding the host procfs would expose other
same-UID processes' `root`, `cwd`, and open descriptors. The empty directory
avoids both failure modes. Retaining host PIDs is still an explicit
cancellation/operations tradeoff: RyeOS strict does not claim PID or same-UID
signal isolation.

Bubblewrap receives no target environment. `--clearenv` and explicit
`--setenv` arguments construct it inside the namespace, preventing variables
such as `LD_PRELOAD` from affecting Bubblewrap's loader. `TMPDIR` is normalized
to namespace-local `/tmp`. The complete option vector, including all target
environment values and arguments, is NUL-separated in a sealed anonymous file.
Bubblewrap's host-visible argv contains only `--args <fd>`.

A private `--json-status-fd` pipe reports the target host PID for accounting.
Before executing Bubblewrap, Lillux creates a new session whose leader is the
retained Bubblewrap wrapper; the target inherits that wrapper-led process
group. Lillux keeps the wrapper unreaped while it terminates the group, so the
PGID cannot be recycled even when the initial target exits before one of its
descendants. Timeout, cancellation, output overflow, and wait failure all use
that stable group ownership. Deliberate descendant `setsid` escape remains
outside this local guarantee; hosted workers add cgroup ownership and
`cgroup.kill`.

Before durable attachment, the daemon captures a version-1
`ExecutionProcessIdentity`: boot ID, target PID/start ticks, and retained group
leader PID/start ticks. In-process spawns require the captured target to remain
in the wrapper-led group. UDS self-attachment requires the reported PID to
equal kernel `SO_PEERCRED` and captures the exact task through
`SO_PEERPIDFD`. Later target/group signals reopen pidfds, revalidate both birth
tuples, and use `pidfd_send_signal`; no stored numeric PID or PGID becomes raw
signal authority.

The exclusive daemon state lock is also the recovery ownership proof. On
startup, a live attachment whose exact target and group leader still match is a
previous-daemon orphan: reconciliation hard-kills that exact group, clears the
attachment with compare-and-clear semantics, then resumes or finalizes the
thread. A same-boot identity with a dead or mismatched leader is quarantined so
PID reuse cannot turn cleanup into an unrelated signal. Old-boot dead identities
can be cleared because their numeric IDs cannot name the recorded incarnation.

There is still a process-creation-to-durable-attachment crash window. If the
daemon receives `SIGKILL` after spawn but before publication, the process group
can survive without a state row that identifies it. Parent-death signalling is
not used as a substitute because the actual spawning-thread/child relationship
does not provide the required daemon-lifetime ownership proof. Hosted workers
must close this window at an outer cgroup/worker boundary and use `cgroup.kill`
or equivalent whole-worker teardown.

Writable binds are installed before read-only binds. Verified-code mirrors,
exact artifact overlays, and captured non-system commands are pinned after
writable ancestors, making the read-only authority dominant. The outer Lillux
request executes the exact startup-captured Bubblewrap artifact and uses the
canonical cwd for host spawn; Bubblewrap changes to the preserved namespace
destination.

In `mode: enforce`, `limits.open_files`, `limits.stdout_bytes`, and
`limits.stderr_bytes` are each merged with request limits using the lower value,
and the open-file cap is installed as `RLIMIT_NOFILE` before exec. In
`mode: disabled`, isolation policy does not introduce an open-file limit; an
explicit caller-owned `RLIMIT_NOFILE` is preserved. Output byte caps remain
active in both modes because they protect the daemon's pipe drainers, which
retain only the configured prefix, continue draining to avoid deadlock, and
terminate the supervised workload with an explicit truncation outcome on
overflow. `RLIMIT_NPROC` is deliberately absent because it is scoped to the
daemon's real UID, not one isolation.

## Workspace and CAS durability

No-project requests and pushed-head checkouts use narrow request-owned
workspaces. External handler invocations instead use their verified bundle root
as a read-only cwd; `IsolationProjectAuthority::ReadOnly` suppresses all writable
policy mounts, including `{project}` and `{cwd}`. Execution roots and writable
workspaces are mode `0700` on Unix, and `TempDirGuard` lifelines are retained
through every blocking, background, detached-child, follow-child, validation,
and compose execution path. A process therefore cannot outlive cleanup of a
request-owned cwd.

Live-project and manifest snapshot publication returns a
`PendingProjectSnapshot` that owns the state-store write permit. The caller
holds that permit from CAS ingest through durable launch-metadata or runtime
attachment publication. Online GC cannot quiesce between writing a new object
and publishing the root that makes it reachable.

Online GC also adds active resume-context hashes as transient roots under one
state-store lock. It retains both local `original_snapshot_hash` and pushed-head
snapshot hashes for Created and Running threads. Deep cache cleanup preserves
`cache/executions` because live subprocess cwd lifelines are not reconstructible
from ordinary CAS reachability. It also preserves `cache/verified-code`:
per-runtime generation locks and startup cleanup own those files independently
of CAS reachability. Other rebuildable cache children may still be removed.

## Diagnostics and operational proof

`ryeos node doctor` calls the production loader. Disabled mode is a healthy
inactive opt-out and does not require Bubblewrap. Enforce mode validates the
backend and reports filesystem, network, environment, open-file, and bounded
output posture.
Release smoke keeps separate default-disabled and explicit-enforcement profiles;
both execute through normal signature and authorization paths.

## Deferred extensions

- delegated cgroup v2 CPU, memory, and per-isolation `pids.max` quotas;
- more production isolation backends in a future schema version; and
- signed native runtime distributions for additional host triples.

The current implementation is not hostile multi-tenancy: CPU, memory, and
process-count quotas remain deferred; host PIDs are visible to syscalls;
same-UID signal isolation is not claimed; and transitive imports, libraries,
and assets remain live read-only. Hostile hosted workloads require cgroups plus
a VM, microVM, or dedicated outer worker.

Do not add item-authored profiles, secondary activation sources, implicit path
creation, or per-spawn policy reads. Those would break node ownership, path
authority, and policy-generation observability.
