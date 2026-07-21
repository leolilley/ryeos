<!-- ryeos:signed:2026-07-21T00:24:30Z:1ca3fce6dbb862d6a33c702f767598a9e3b2d8c133e64cccf68c2ffccfbcc590:XRwZW/EasOXP+eXcuhhIMw4k7vNEIpuqXlZ2OSDQESamQoE42P8feGXPzorOPct0imt1qCuQkdAHac75UurXCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, isolation, security, subprocess, node-policy]
version: "1.6.0"
description: >
  Node contract for the node-owned subprocess isolation: strict policy
  schema, startup pickup, enforcement behavior, diagnostics, and limits.
---

# Execution Isolation

RyeOS can launch executable tools and runtimes through a node-owned isolation
policy and a selected signed backend bundle. The only policy source is
`<app-root>/.ai/node/isolation.yaml`. `ryeos init` creates it once; later edits
belong to the node owner. Items, bundles, requests, and environment variables
cannot activate the isolation or weaken its controls.

The engine resolves typed isolation requirements against signed backend
declarations and live inspected capabilities. It emits a strict backend-neutral
plan; the selected adapter owns backend-specific inspection and launch
compilation. RyeOS does not ship or select an isolation backend by default.

The engine also keeps node trust separate from project/request trust. The
`node_trust_store` is loaded only from persistent node configuration and is the
authority for installed bundle schemas, parsers, handlers, protocols,
runtimes, and native executor manifests. Project keys and caller-scoped trust
overlays may authorize project items, but they cannot make a new host binary or
installed runtime node-trusted.

The policy has two modes:

- `mode: disabled` does not wrap the subprocess in an isolation adapter. This is the
  default and does not require the selected bundle. Node-owned stdout/stderr retention
  caps, signature, trust, authorization, and capability checks still apply,
  but there is no OS confinement or verification-to-exec path pinning.
  Daemon-owned processes still use attachment-before-execution; Lillux supplies
  the direct target hold without an isolation backend.
- `mode: enforce` applies the complete policy and refuses the launch if any
  requested control cannot be enforced.

## Default policy

```yaml
version: 1
mode: disabled
backend: null
filesystem:
  readable:
    - "{node_public_identity}"
    - "{daemon_socket}"
    - "{bundle_roots}"
    - "{node_trusted_keys}"
    - "{verified_code}"
  writable:
    - "{project}"
    - "{checkpoint_dir}"
network:
  mode: host
environment:
  allow:
    - "*"
limits:
  open_files: 1024
  stdout_bytes: 8388608
  stderr_bytes: 8388608
  verified_artifact_file_bytes: 67108864
  verified_artifact_total_bytes: 268435456
  verified_artifact_files: 4096
```

The default is deliberately inert. To opt in, install a signed backend bundle,
select its bundle and implementation in the policy, change the node-owned mode
to `enforce`, run
`ryeos node doctor`, and restart the node.
The daemon loads one immutable policy generation at startup; editing the file
does not change a running daemon. The daemon-backed `ryeos daemon status`
surface (`service:node/status`) reports the loaded mode, version, source, and
source digest together with the exact backend selection, signed bundle-manifest
digest, signer fingerprint, adapter content digest and build, declared and
effective capabilities, and inspected artifact versions and digests. Backend
status is the typed value `disabled`, `available`, `unavailable`, or
`incompatible`. `ryeos node status` is the narrower local lifecycle probe.
Doctor derives the same facts from the shared immutable runtime snapshot.

## Strict schema

- `version` must be `1`, the first published strict-policy schema. Other
  versions and unknown fields are rejected without aliases or translation.
- `backend` is null when no backend is selected and must be present in enforce
  mode.
- `backend.bundle` names one registered signed bundle.
- `backend.implementation` names one backend declaration in that bundle's
  signed manifest. Enforce mode captures the exact signed adapter and artifact
  executables into sealed anonymous executable files, requires signer
  continuity with the bundle manifest, refuses symlinks, privilege bits, and
  Linux file capabilities, and runs the adapter's strict live inspection
  before accepting the node generation. The effective capability set is the
  intersection of the signed declaration and live inspection.
- `filesystem.readable` accepts absolute paths plus `{project}`, `{cwd}`,
  `{node_public_identity}`, `{daemon_socket}`, `{bundle_roots}`,
  `{node_trusted_keys}`, and `{verified_code}`.
- `filesystem.writable` accepts absolute paths plus `{project}`, `{cwd}`, and
  `{checkpoint_dir}`.
- `network.mode` is `host` or `isolated`.
- `environment.allow` entries are exact names, `*`, or prefix patterns ending
  in one `*`.
- `limits.open_files` is an optional per-spawn file-descriptor limit in
  `mode: enforce`. Disabled mode preserves a tighter limit already owned by a
  caller but does not install the isolation policy's `RLIMIT_NOFILE`.
- `limits.stdout_bytes` and `limits.stderr_bytes` are mandatory positive
  bounds on bytes retained from each output stream. The node continues
  draining after a bound is crossed and terminates the supervised workload.
- `limits.verified_artifact_file_bytes`,
  `limits.verified_artifact_total_bytes`, and
  `limits.verified_artifact_files` are mandatory positive bounds on each exact
  artifact, the aggregate bytes, and the unique artifact count in one runtime
  generation. The aggregate byte limit must be at least the per-file limit.

Missing required sections, malformed YAML, invalid paths or wildcard forms,
and unsupported values are errors even when disabled. Disabled mode skips
backend availability and OS-confinement controls, not node-owned output caps.

## Generation admission

Daemon bootstrap holds the node-wide bundle-registry mutation lock from its
first signed registration read through manifest admission, backend capture,
engine-registry construction, and the full node-config snapshot. Every
component consumes the same immutable node-trust snapshot; inner manifest and
engine builders do not reload trust. Phase one captures a verified generation
record for every bundle: canonical root directory identity, signed bundle
manifest body digest and signer, and signed executor-manifest hash and signer.
The root directory handles remain pinned, and every path-based phase-one reader
checks the exact root and signed identities before and after its read. An
out-of-band root replacement therefore refuses the generation instead of
mixing independently valid bundle versions. Daemon item resolution, plan
construction, and spawn preparation additionally hold the registry mutation
lock as a read-side generation guard, so a cooperative replacement cannot
enter between identity checks and path consumption. The running daemon
retains sealed adapter and payload handles; a later atomic bundle replacement
cannot change that runtime until restart.

Standalone doctor, inspection, signing, and offline execution retain the same
generation lock together with the exact trust snapshot and registered root set
for the lifetime of their isolation runtime. A caller-provided earlier snapshot
is reverified or compared against that retained generation before execution.

Install, replacement, removal, and re-init independently prove the exact
prospective generation before activation. With enforcement enabled this means
capturing and inspecting the backend selected from the post-operation roots,
then constructing prospective registries with that prospective runtime. The
currently running runtime confines candidate verification but is never used as
the prospective registry runtime. Removing the selected bundle or replacing it
with an incompatible generation fails before mutation. Disabled policy has no
artificial dependency on the selected bundle.

Re-init validates the complete source generation even when ordinary preflight
is skipped. It also re-resolves the selected backend from its completed staging
tree before that tree can be atomically activated. First init has no policy and
uses the compiled disabled default until it creates the policy once.

## Filesystem authority

The policy source and all later app-root authority are associated with one
canonical app-root identity. RyeOS opens the policy below that canonical root
and re-resolves the configured app root after reading it; a changed
association refuses startup. The original app-root spelling is retained only
as its namespace destination.

Every host mount source is canonicalized for validation. Its destination keeps
the absolute spelling supplied by the launch context or policy so projects and
app roots reached through a symlink still occupy the namespace path expected by
the process. Every policy, app, project, working-directory, state, checkpoint,
socket, bundle, trusted-key, and generated-code namespace destination must be
an absolute root followed only by normal path components. Parent traversal is
rejected before an adapter can interpret it. Enforce mode then pins system,
policy-selected, identity, socket, bundle, writable, and code mount sources
with `O_PATH` descriptors and passes only typed descriptor authorities to the
selected adapter. Caller-supplied inherited descriptors are refused.
The descriptors remain close-on-exec in the
multithreaded daemon and are made inheritable only in the forked child, so a
pathname replacement cannot redirect a validated mount and concurrent spawns
cannot inherit another launch's authority. Before the adapter executes its
launcher it marks every ambient non-stdio descriptor close-on-exec, then clears
that flag only for the signed plan's authorities, sealed argument file, and
target-status channel. The adapter descriptor itself closes in the launcher
image.

The narrow readable placeholders mean:

- `{node_public_identity}` exposes only the exact regular, non-symlink
  `<app-root>/.ai/node/identity/public-identity.json`, never the private key.
- `{daemon_socket}` exposes only the daemon-pinned, non-symlink Unix socket
  when a typed launch fact requests callback IPC. RyeOS does not infer this
  authority from an environment-variable name. The requested path must equal
  the socket pinned at daemon startup, and the exact placeholder mount is
  required; a surrounding directory is not substituted for it.
- `{bundle_roots}` expands to bundle roots verified for this execution.
- `{node_trusted_keys}` conditionally exposes the node owner's trusted-key
  directory. The placeholder name matches the existing launch-envelope root.
- `{verified_code}` is mandatory for executable-item launches. RyeOS rechecks
  the verified whole-file SHA-256, writes those exact bytes to node-owned
  content-addressed storage, mirrors the code's project or bundle authority at
  a synthetic `/run/ryeos/verified-code/<authority-hash>` root, and overlays
  the artifact at its matching relative path. Arguments and environment values
  that name the host source are rewritten to that synthetic path. Any remaining
  lexical, canonical, or resolvable path reference to the live source is
  refused rather than passed through. A mutable project therefore cannot
  replace the entry code that was authorized.

The node-private exact-artifact store is per immutable runtime. One mutex
serializes content-address checks, quota accounting, and publication, so
concurrent launches cannot race the configured unique-file, per-file-byte, or
aggregate-byte bounds. The default bounds are 4,096 artifacts, 64 MiB per
artifact, and 256 MiB total. The backend capture consumes the same budget.

Exact-byte authority applies to the verified entry file and any captured
non-system executable. The surrounding project or bundle mirror remains a live
read-only view for transitive imports, libraries, assets, and interpreter
lookups; those transitive contents are not content-pinned by this boundary.

`{checkpoint_dir}` is accepted only for the exact daemon-owned path for the
current thread. Thread identifiers must be one normal path component, and
checkpoint paths that traverse a symlink are rejected.

An explicit runtime state root is not a wildcard mount. It must already exist
and be contained by `{project}` or an absolute writable root chosen in this
policy. This prevents a request from turning a path parameter into new host
write authority.

Writable roots may not overlap `/`, protected system roots, the app root, the
selected backend artifacts, or the daemon socket. The resolved host home itself and
its ancestors are also rejected. The only app-root exceptions are a
daemon-provenance execution workspace directly beneath
`.ai/state/cache/executions` and the exact daemon-owned checkpoint directory.
Absolute paths in this file are node-owner authority and should be kept narrow.

## Process boundary

Enforce mode canonicalizes the command, project, working directory, mount
sources, and complete constructed environment before spawn. The adapter receives
a strict typed plan and no ambient target environment. The request is stored in
an immutable sealed anonymous file, and `TMPDIR` is normalized to `/tmp` in the
target environment.

The namespace uses a private root and new user, IPC, and UTS namespaces. An
isolated network policy also creates a network namespace; host mode deliberately
keeps host networking. RyeOS retains the host PID namespace because managed
runtimes attach their PID to the daemon and host-side PGID cancellation and
restart reconciliation must address the same process identifiers. The namespace
contains an empty `/proc` directory rather than the host procfs: exposing the
host mount would let a same-UID workload traverse another process's `root`,
`cwd`, or `fd` links and bypass the selected filesystem surface. PID syscalls
still use host identifiers, and the isolation does not claim PID or same-UID
signal isolation.

Process attachment is orthogonal to this isolation mode. Every daemon-owned
launch is created awaiting attachment, its exact target identity is persisted,
and only then is the target released to execute. In disabled mode Lillux holds
the direct target in its native pre-exec boundary. In enforce mode the adapter
holds its actual target and reports that target's host PID through the strict
isolation protocol. A supervised request without the requested target hold is
rejected; there is no fallback to wrapper identity or direct execution.

Lillux creates a new session before it executes the adapter, and the target
inherits the retained wrapper's process group. The wrapper remains unreaped
while Lillux terminates that group, which keeps the PGID reserved even if the
initial target exits while descendants are still running. Timeout,
cancellation, output overflow, attachment failure, release failure, and wait
failure all use that stable group ownership. A descendant that deliberately
creates another session remains outside this local process-group guarantee;
hostile hosted workers additionally use `cgroup.kill` at the outer worker
boundary.

Offline tools that inherit terminal stdin/stdout/stderr use the same Lillux
session, target-status, timeout, group-cleanup, and refusal contract. They do
not drop the supervised target channel to execute through a raw host command.
Because terminal output is not retained by RyeOS, retained-output byte caps are
explicitly removed at that composition boundary; Lillux rejects an inherited-
stdio request that still claims captured-output limits. Open-file and timeout
limits remain enforced.

Each managed launch persists secret-free provenance in its runtime launch
metadata: policy digest, selected backend, manifest and signer identities,
adapter and payload digests, protocol version, effective capabilities, and a
compiled-plan digest. The digest redacts target argument and environment
plaintext before canonical hashing; changes to authority-bearing plan
structure still change it. Non-managed infrastructure launches emit the same
provenance to the diagnostic log surface.

Managed launches pin the exact target and retained group leader before durable
attachment. The current process identity records the boot ID, numeric target
and leader IDs, and both `/proc` start-time ticks. Every later target or group
signal first opens a pidfd, proves the stored incarnation, and uses pidfd
signalling; RyeOS never turns a stored PID/PGID into raw `kill` authority.
Self-attaching runtimes must match their accepted UDS `SO_PEERCRED` PID and are
pinned through that socket's `SO_PEERPIDFD`.

Normal shutdown first closes process release and authoring, then tears down
attached identities within a shared node-owned grace bound. A stop that wins
before release leaves the target held and aborts it; a stop that wins after
release observes the exact durable identity and terminates it. After an unclean
daemon exit, the exclusive state lock proves that any still-live, exactly
matched attachment belongs to the previous daemon; startup kills that group
before recovery launches a replacement. A same-boot attachment whose leader
birth identity can no longer be proven is quarantined rather than cleared or
signalled.

The native pre-exec hold removes the former local spawn-to-attachment crash
window. Parent death before durable attachment kills the still-held direct
target; after attachment, recovery has its exact identity. Hosted deployments
still require cgroups plus a VM, microVM, or dedicated outer worker for quotas,
cross-session whole-workload teardown, and a hostile-tenant kernel boundary;
those are separate guarantees, not compensation for missing local attachment.

See [Attachment Before Execution](../execution/attachment-before-execution.md)
for the complete lifecycle contract and ownership split.

Configured writable binds are installed first. Read-only policy mounts, the
verified-code authority mirror and exact artifact, and any non-system command
are installed afterward, so a broad writable ancestor cannot hide them.
Verified node or bundle commands without a content identity are refused;
project and node-selected commands are copied from one opened byte sequence and
run from the same synthetic read-only namespace. `/usr`, `/bin`, `/lib`, and
`/lib64` are descriptor-pinned read-only; the small required `/etc` runtime
surface is pinned separately, `/dev` is backend-provided, `/proc` is empty,
and `/tmp` is private tmpfs.

`limits.open_files` becomes `RLIMIT_NOFILE` before exec. Output is retained only
up to `limits.stdout_bytes` and `limits.stderr_bytes`; pipes continue to be
drained so the child cannot deadlock, and overflow terminates the supervised
workload with an explicit truncated-output result. When a request already has
a lower cap, the lower value wins. Any validation, mount, backend, or limit
failure refuses the spawn.

## Launch coverage

The immutable runtime covers engine plan subprocesses, managed and streaming
runtime launches, compose-context children, external parser/composer handlers
and their boot validation, tool environment import probes, and offline
executable dispatch. Handler binaries retain their signed executor-manifest
hash, execute from captured exact bytes, see installed bundle roots read-only,
and receive no configured host writable mounts. Graph runtimes parse the exact
verified bytes carried in their launch envelope rather than reopening a mutable
source path. Callback-free streaming protocol executors also receive a durable
thread row and exact attached process identity, so shutdown owns them rather
than leaving an untracked blocking subprocess.

Locally launched CLI client applications are not node workloads and do not pass
through this policy; they run with the invoking user's terminal and desktop
authority. Hosted execution paths do not launch those client applications.
Maintainer-only bundle signing is likewise an explicitly named authoring path
that may run before a node policy exists; it uses local maintainer authority and
does not claim OS confinement. Node boot, admission, preflight, doctor, item
signing, and runtime handler dispatch use the immutable node policy instead.

HTTP live-filesystem execution requires an explicit project path to name a real
project root containing `.ai`. No-project requests receive a private,
request-owned workspace. Pushed snapshots are materialized into private
execution workspaces; their lifelines are retained for the whole subprocess.

## Diagnostics and limits

`ryeos node doctor` uses the production policy loader. Disabled mode reports a
healthy inactive opt-out without resolving the selected backend. Enforce mode verifies
backend availability and capture digest, and reports filesystem, network,
environment, open-file, captured-output, and verified-artifact-limit posture.

The published container runs the default disabled profile without extra
capabilities. Enforce mode requires a separately installed selected signed
backend bundle and every host facility needed by its declared capabilities.
Setuid, setgid, and file-capability adapter or launcher artifacts are refused,
and verified bytes execute from sealed private captures.
The supported Docker profile adds `SYS_ADMIN` and uses unconfined seccomp and
AppArmor profiles for the required namespace and mount operations. A
purpose-built AppArmor profile may replace `apparmor=unconfined` when it grants
the same operations.

This boundary limits filesystem visibility and writes, network namespace
access, the target environment, and open file descriptors. It is not a virtual
machine, does not defend against kernel vulnerabilities, and does not yet set
CPU, memory, or per-isolation process quotas. Do not model a process quota with
`RLIMIT_NPROC`: it is scoped to the daemon's real UID rather than one isolation.
Host PIDs remain visible to syscalls, same-UID signal isolation is not claimed,
and transitive imports, libraries, and assets remain live read-only rather than
content-pinned.
Disabled means no OS isolation, not unverified execution; resolution, signature,
authorization, and capability checks remain active.

## Local use, hosted nodes, and Docker

For a trusted, single-user local node, keeping the default disabled mode is a
reasonable choice. It avoids namespace requirements while retaining RyeOS's
normal signed-item and capability model. Enforcement becomes important when a
node executes bundles for another person, accepts remotely supplied projects,
or shares one worker among workloads with different trust.

Docker and the RyeOS isolation protect different boundaries. A container isolates
the whole RyeOS node from its host, but the daemon and every tool inside that
container normally share the container filesystem, network, environment, and
Linux identity. The RyeOS policy creates a narrower boundary for each launched
process: only selected roots are visible, writes are limited, environment names
are filtered, networking can be detached, open files are capped, and verified
entry bytes are overlaid read-only. A compromised tool therefore receives less
of the node's authority than the daemon that launched it.

For hosted execution, use both layers: a container, VM, or dedicated worker as
the tenant/node boundary, and enforced RyeOS launches as the per-workload
least-authority boundary. This implementation makes node-owned policy pickup,
separate node executable trust, uniform launch coverage, exact entry-code
execution in enforce mode, and observable policy generations available now. It
is the base for safely evaluating third-party
bundles, remote project execution, and future workload tiers. It does not yet
provide CPU, memory, or process-count isolation; production multi-tenant hosting
still needs cgroup quotas plus a VM, microVM, or dedicated outer worker for
hostile code.

Do not generalize this policy by adding backend-specific fields to the current
schema. New implementations declare their adapter, artifacts, target triples,
and capability upper bound in a signed bundle and consume the existing typed
plan. No backend implementation is part of engine policy.
