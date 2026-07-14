<!-- ryeos:signed:2026-07-14T02:13:36Z:43d0f7d6322fce777a27bca0273ed35cc3c6edf5fa0f53cb6126645b376ef0ba:3nexAzSpp9LU+Bu4XG0hbmDubxr7CpNthLxm86d20744sy/XGxt8shxt0dkiiHvoZjHWKkQTpgAip3ODELqdDQ==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
```yaml
category: ryeos/development
name: sandbox-runtime
title: Node Sandbox Runtime Architecture
entry_type: implementation_guide
version: "1.0.0"
description: Deep implementation map for the immutable RyeOS strict sandbox, exact-byte execution boundary, launch coverage, path authority, and state durability.
tags:
  - sandbox
  - bubblewrap
  - execution
  - node-policy
  - security
```

# Node Sandbox Runtime Architecture

## Contract and ownership

The node owns one strict `version: 1` policy at
`<app-root>/.ai/node/sandbox.yaml`. `ryeos init` creates the disabled default
only when the file is absent. This is the sole schema and activation source;
other versions, unknown fields, item-authored profiles, and per-request
overrides are rejected.

`SandboxRuntime::load` in `ryeos-engine/src/sandbox.rs` first resolves one
canonical app-root identity, opens the fixed source below that canonical root
as a regular non-symlink file, strictly parses it, hashes the exact source, and
re-resolves the supplied app root before returning an immutable typed snapshot.
The operator spelling is retained only as the namespace destination; a changed
canonical association refuses startup. Daemon startup uses `load_for_daemon`
to pin the configured callback UDS path into the same snapshot. Standalone and
offline callers use `load`. Disabled snapshots still validate the full schema,
absolute backend spelling, and mandatory artifact limits, but do not inspect
backend availability or modify a subprocess request. Enforced snapshots resolve and
read the backend once, require unprivileged executable regular-file metadata
(no setuid, setgid, or file capabilities), materialize its exact bytes in the
runtime's private artifact generation, and validate the Lillux open-file limit
before startup succeeds. Later launches execute the captured inode through
`/proc/self/fd`, not the configured pathname. Inspection exposes its captured
SHA-256 and version. Startup invokes the exact capture with `--version` and
`--help`, requiring Bubblewrap 0.11.0+ and the fd-bind/argv0 features used by
request construction. The configured executable is a node-approved host
dependency; the probe establishes compatibility, while the node-owned policy
is the authority selecting those bytes.

```text
ryeos init
  -> create-once .ai/node/sandbox.yaml

ryeosd main
  -> SandboxRuntime::load_for_daemon(app_root, uds_path)
  -> Arc<SandboxRuntime>
  -> AppState / EngineContext / runtime launch parameters

standalone service / offline dispatch
  -> SandboxRuntime::load(app_root)
  -> one immutable runtime for that process or command

all node-workload executable-item subprocess paths
  -> SandboxRuntime::apply(request, launch context)
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

The strict sandbox requires these artifact controls in `limits`:

```yaml
verified_artifact_file_bytes: 67108864
verified_artifact_total_bytes: 268435456
verified_artifact_files: 4096
```

They bound a single exact artifact, aggregate exact-artifact bytes, and unique
artifact count in one runtime generation. Values must be positive, and the
aggregate byte bound cannot be below the per-file bound. `open_files` remains
the optional per-process descriptor limit.

## Launch context and coverage

`SandboxLaunchContext` carries engine/daemon-derived facts only: canonical item
reference, one-component thread identifier, project authority and path,
optional state root and checkpoint directory, verified bundle roots, operator
trust directory, and zero or more `SandboxVerifiedCode` exact-byte identities.
None of these fields can activate or relax the policy.

The shared runtime is applied at:

- execution-plan subprocess dispatch;
- normal, streaming, and managed runtime spawn;
- compose-context child runtime bootstrap;
- inline, background, detached-child, and follow-child execution;
- HTTP handler executable dispatch;
- tool environment import probes; and
- offline executable tool and service dispatch.

Installed CLI client launchers intentionally remain outside this boundary: they
are local user-interface processes that require the invoking terminal/desktop
authority, not workloads launched by the node. Hosted execution never selects
that client path.

Internal parser/composer helpers are part of the trusted engine implementation,
not caller-selected executable items. Their output still feeds the same
signature, plan, authorization, and sandboxed launch boundary before item code
can run.

## Exact-byte execution invariant

Resolution records a whole-file SHA-256. `build_plan` rereads the requested
root only if its bytes still match that digest. Every executor-chain hop uses
the real Ed25519 verification path rather than trusting a claimed fingerprint.

When an executable plan reaches enforced `apply`, each `SandboxVerifiedCode`
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
symlink. Required visibility checks pair canonical source containment with
lexical destination containment, rejecting `..` and symlink escapes.

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
cannot inherit another sandbox's mounts. Enforced apply refuses any inherited
descriptor not created by the sandbox runtime itself.

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
HTTP layer no longer creates a caller-selected state-root path before sandbox
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
to namespace-local `/tmp`.

Writable binds are installed before read-only binds. Verified-code mirrors,
exact artifact overlays, and captured non-system commands are pinned after
writable ancestors, making the read-only authority dominant. The outer Lillux
request executes the exact startup-captured Bubblewrap artifact and uses the
canonical cwd for host spawn; Bubblewrap changes to the preserved namespace
destination.

`limits.open_files` is merged with a request limit using the lower value and
installed as `RLIMIT_NOFILE` before exec. `RLIMIT_NPROC` is deliberately absent
because it is scoped to the daemon's real UID, not one sandbox.

## Workspace and CAS durability

No-project requests, handler invocations, and pushed-head checkouts use narrow
request-owned workspaces. Execution roots and workspaces are mode `0700` on
Unix, and `TempDirGuard` lifelines are retained through every blocking,
background, detached-child, follow-child, validation, and compose execution
path. A process therefore cannot outlive cleanup of its cwd.

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
backend and reports filesystem, network, environment, and open-file posture.
Release smoke keeps separate default-disabled and explicit-enforcement profiles;
both execute through normal signature and authorization paths.

## Deferred extensions

- delegated cgroup v2 CPU, memory, and per-sandbox `pids.max` quotas;
- more production isolation backends in a future schema version; and
- signed native runtime distributions for additional host triples.

Do not add item-authored profiles, secondary activation sources, implicit path
creation, or per-spawn policy reads. Those would break node ownership, path
authority, and policy-generation observability.
