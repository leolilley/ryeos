<!-- ryeos:signed:2026-09-10T07:15:52Z:cb6630e6dee13a82fdf7cfa931ea96426b47ed0641d4fa546c98768b80a11b65:7nlu58iRTn2JqMKoNNjEJNsYNs5sW+wjVAwp5Pfsws687d5AsXA+dDhlqQeJPsrGFjBbg4aYRSnPy7olFMb7Ag==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, isolation, security, subprocess, node-policy]
version: "1.22.0"
description: >
  Node contract for the node-owned subprocess isolation: strict policy
  schema, startup pickup, enforcement behavior, diagnostics, and limits.
---

# Execution Isolation

RyeOS can launch executable tools and runtimes through a node-owned isolation
policy and a selected signed backend bundle. Isolation is one mandatory member
of the complete node-signed generation under `<app-root>/.ai/node/policies/`.
The selected init profile makes the first explicit choice; later changes replace
the whole policy generation atomically. Items, bundles, requests, and environment
variables cannot activate isolation or weaken its controls.

The engine resolves typed isolation requirements against signed backend
declarations and live inspected capabilities. It emits a strict backend-neutral
plan; the selected adapter owns backend-specific inspection and launch
compilation. Core ships the self-contained `linux-lillux` implementation as
available signed data. The explicit development profile selects enforced
execution; the general installation profiles retain their explicit disabled choice.

Durable execution workspaces use the isolation-adapter v10 contract. RyeOS owns
one canonical private `project/` generation. Disabled/native execution creates
no other workspace directory and uses Lillux descriptor-relative filesystem
mechanics directly. Enforced execution additionally grants the selected signed
adapter one `backend-state/` directory as an opaque authority. Names and
representation below that directory—including any platform-specific overlay
layers—belong exclusively to the adapter and never enter the engine, executor,
workspace journal, or generic launch plan.

Create runs under the existing held-process attachment boundary. Its exact
creator identity is recorded before it can touch the workspace. One private
bounded descriptor-transfer receipt binds the canonical request and response;
the accepted view descriptor remains in the original workspace lifeline.
Launches borrow that exact view after durable workspace/launch-owner admission,
not by reconstructing mounts from path names. The native adapter creates one
detached overlay template; each borrower independently clones and attaches it
inside fresh namespaces. It never remounts the same upper/work pair or joins a
retained creator namespace. The actual backend probe checks these mechanics;
unsupported kernels refuse, without an alternate backend or ambient fallback.

The runtime journal fences every exact borrower, including nested child
launches. Exclusive workers additionally retain their existing process/boot
authority. Live immutable-input capture quiesces writers but retains the view.
Destroy requires settled borrowers and physical closure of the original view
owner; root terminal status, absent PID, reference count or a newly opened path
guard is not that proof. Cold restart verifies old process death before a new
view incarnation. Live retry transfers the existing original lifeline instead.
Uncertain pre-attachment contact stays quarantined rather than being silently
discarded or treated as an unused workspace.

Capture checks both the workspace borrower journal and the existing exclusive
worker/pool authority. Pending startup, unknown cleanup and extra unsettled
worker epochs block it. The root-operation drain is acquired before a scarce
capture permit; otherwise a capture can block the very operations it needs to
drain. A temporary drain gate does not authorize publication or remain held
while a candidate awaits its separate disposition.

Errors before a launcher reports its target identity may not carry exact
cleanup testimony. RyeOS retains their workspace/credential fences as uncertain;
it does not infer an unused workspace from an absent PID or parse stderr as
proof of death. This is quarantine, not automatic recovery of that failed start.

A projectless controller retains its separately confined worker workspace
through its existing scratch lifeline. The worker's private backing directory
is not placed under the controller's writable scratch mount. This distinction
does not give either process signing, publication or extra filesystem authority.

On freeze, an adapter may return normalized mutation facts plus a canonical
relative content-root name below its still-pinned opaque state authority. RyeOS
opens that returned root without following links and verifies each regular
file's mode, size, and content digest before admitting bytes to CAS. Thus
publication remains backend-neutral without granting the adapter CAS or project
HEAD authority. The current contract also permits a self-contained adapter
with no external artifact roles, requires an explicit PID-namespace choice,
and carries a bounded, sorted collection of typed target channels. Predecessor
workspace journals require the explicit runtime-history retirement ceremony and
are never reinterpreted. Each channel has one exact child descriptor and hidden
environment binding in both disabled and enforced modes; fd 0 is a deliberate
full-duplex primary channel, not a sentinel. Parent protocol code retains a
typed Lillux byte-stream endpoint and never recovers a raw Unix socket.

Argument entries and environment values are exact process data: an empty
string remains present and empty through plan validation, serialization and
native launch. It is not replaced with absence, an inherited value, or a
sentinel. Environment names, argv0, paths and authority identities still
require nonempty values. Per-string size, collection and NUL restrictions are
unchanged. This is validation of the existing process contract, not another
environment policy or an optional compatibility mode.

The native backend's kernel probe includes sources inherited from **before**
the namespace transition, not just files created inside the sandbox. Linux
cannot bind-clone an inherited source's former-namespace mount directly.
Lillux uses the descriptor's kernel-reported location only to find its exact
counterpart in the cloned namespace, then compares device, inode and file type
against the still-retained original descriptor. Missing, replaced or symlinked
sources refuse; the location does not independently authorize a mount. This
reproof happens before the private root obscures host locations.

### Confined live projects

Signed node isolation policy v5 selects `filesystem.live_project.mode:
fixed_parents` and supplies its construction `limits.max_entries` and
`limits.max_depth`. Protected relative paths come from State's existing central
project control-path classification, not another adapter-specific list.
The current protocol carries these compiled views and requires the separately probed
`filesystem.fixed_parent_views` capability. Missing policy, bounds, capability,
or source authority refuses execution; none selects a fallback.

The live source root remains a real source-backed mount. For each protected
path, Lillux constructs a private directory tree for its parent directories,
omits the protected leaf, mounts allowed siblings from their exact descriptors,
and seals only those parent directories read-only. Moving that completed tree
onto an existing ancestor does not create or modify source entries. Protected
leaves may be files, directories, or absent. Later positive mounts may not
replace these parents or expose protected paths; known raw-source aliases are
refused. Ordinary nested mounts must already have targets inside source-backed
ancestors, never create placeholders in a live source.

Immediate entry membership of these connector directories is fixed for the
launch: create, delete, or atomic replacement there refuses. Writes inside
allowed child directories, and content writes to allowed mounted files, still
reach the original source according to its read/write grant. The view is not
silent copy-on-write. Top protected-path ancestors must already exist as real
directories; a missing top ancestor refuses rather than freezing the entire
project root. Pinned-CoW worker workspaces keep their separate existing contract.

Symlinks resolve within the complete admitted execution namespace, including
other admitted mounts—not a project-only jail. Connector symlink entries are
fixed for the launch. Path protection does not revoke pre-existing hardlink or
host bind-mount aliases, and external host topology writers must coordinate;
this is not hostile same-UID multi-tenant fencing. Every setup descriptor is
closed by the existing target-release boundary before workload execution.

Native mount classification reuses Lillux's shared regular-file, directory and
filesystem-Unix-socket classes. An admitted callback socket is pinned and
reproved like other exact filesystem sources, then attached non-recursively
over a file-shaped target. This neither exposes arbitrary sockets nor grants
callback authority to opaque Tools. Replaced socket inodes, anonymous socket
handles, symlinks and unsupported special files cannot become mount authority.

Fully sealed anonymous regular files have immutable bytes but no bindable
filesystem mount. Lillux streams exactly their admitted length into a fresh
private staging tmpfs, preserving only read/execute mode bits and leaving the
sender's offset unchanged. It closes every write handle, attaches read-only
descriptor mounts, proves staging ownership, detaches that filesystem, then
removes only its empty underlying mountpoint before any workload starts.
Materialized files stay linked on the detached filesystem; unlinking them would
make executable self lookup report a deleted path even through the admitted
mount. The target retains neither staging paths nor authority descriptors.
The native backend refuses recursive directory sources equal to or above its
private construction mountpoint (`/tmp`). Such a source would also clone
backend-private setup mounts, which are not admitted workload content. Normal
project directories beneath that mountpoint remain supported. The kernel
descriptor locator is used only for this refusal; exact inode/type reproof
still owns source admission. The capability probe temporarily clones its
ancestor to test inherited descriptor attachment, then retires that probe-only
alias before exact staging cleanup; production does not suppress cleanup errors.
Unsealed bytes
cannot use this materialization path, and sealed sources cannot grant writable
mounts. These are backend mechanics, not a new realization, host-path fallback,
or permission to weaken the node policy.

Concurrent verification of a retained artifact does not mutate it. Protection
is established at creation; reuse checks exact protected permissions and hashes
the retained inode without reapplying chmod or repairing an unexpected mode.
Exact byte reads and digests use positional I/O, not seek/read on duplicated
descriptors: duplicated descriptors share a cursor. Size, metadata and growth
checks remain mandatory. This preserves parallel parser/service validation
without serializing all consumers behind another global lock.

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
  the direct target hold without an isolation backend. When a finalized program
  retains an admitted source closure or external realization, RyeOS gives that
  process a daemon-private sparse input root and materializes the exact retained
  bytes there by descriptor-safe reflink or bounded copy. A live launch with no
  retained filesystem bindings keeps the ordinary live project path. This input
  delivery is not a substitute for OS isolation: disabled mode still provides no
  confinement from other host paths visible to the process.
  A signed captured-filesystem or isolated-network requirement refuses in
  this mode. Execution-runtime-root realizations also refuse: only project-
  root realizations support the private-copy input delivery described above.
- `mode: enforce` applies the complete policy and refuses the launch if any
  requested control cannot be enforced.

## Explicit disabled policy member

```yaml
schema: 1
policy:
  version: 5
  mode: disabled
  backend: null
  process_scopes:
    mode: unconfigured
  filesystem:
    proc_filesystem: empty
    live_project:
      mode: fixed_parents
      limits:
        max_entries: 2048
        max_depth: 64
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
    runtime_files: []
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

This is an explicit policy choice, not a Rust fallback. To opt in, install a
signed backend bundle, select its bundle and implementation in the replacement
policy generation, change the mode to `enforce`, run `ryeos node doctor`, and
restart the node.

`--pin-project` remains the explicit complete-project snapshot/COW workflow. It
does not control ordinary admitted source or external-content delivery, and is
not required merely to run a definition that already retains those exact bytes.
Sparse admitted-input roots are process inputs and scratch, not project
publication. Their writes are discarded at process cleanup and never fold back
to the live project. Durable outputs use structured results or explicit
daemon-owned authoring, vault, and bundle-event callbacks; generic opaque file
ingest is not currently a runtime callback contract.
The signed node execution config also sets
`node.max_private_materialization_copy_bytes`. It is one aggregate allowance
per private materialization transaction when filesystem reflinks are
unavailable; zero, missing, malformed, or untrusted values refuse node startup.
The `ryeos.metrics` summary reports reflink/copy counts, copied bytes, allowance
remaining, and materialization time without exposing host paths.
The daemon loads one immutable policy generation at startup; editing the file
does not change a running daemon. The daemon-backed `ryeos daemon status`
surface (`service:node/status`) reports the loaded mode, version, source, and
source digest together with the exact backend selection, signed bundle-manifest
digest, signer fingerprint, adapter content digest and build, declared and
effective capabilities, and inspected artifact versions and digests. Backend
status is the typed value `disabled`, `available`, `unavailable`, or
`incompatible`. `ryeos node status` is the narrower local lifecycle probe.
Doctor derives the same facts from the shared immutable runtime snapshot.

When the adapter refuses target setup, Lillux retains its bounded refusal
document separately from workload stderr. Ordinary execution propagates that
diagnostic just as parser/composer and launch-preparer execution do; it must
not replace the cause with a generic supervised-launcher failure. A launch
refusal is not an executed workload's nonzero exit result.

## Strict schema

- The outer member `schema` is `1`; `policy.version` must be `5`. Other
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
- A retained immutable-input or live-project authority intersects an explicit
  writable `{project}` grant into an exact read-only project mount. The bare
  read-only access ceiling does not grant project visibility. Immutable input
  mounts require the state-issued materialization proof for the execution input
  (not the definition generation), recheck its complete contents, and retain its
  descriptor. Unproved project paths cannot expose protected node storage.
  Explicit readable project grants retain the same protected-root floor.
  Code-only launches can instead rely on admitted code/bundle mounts.
  A writable node ceiling
  does not make an immutable child input or evaluator candidate writable, and
  does not erase its read access. Missing project grants still refuse execution;
  unrelated writable paths are not converted to readable mounts. Under captured
  execution, this exact project mount survives alongside verified code and
  separately admitted dependencies, without ambient node-state access.
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

## Per-execution network ceiling

The node policy remains the maximum network authority. A signed kind schema may
also declare a mechanical composed-value projection whose closed result is
`node_policy` or `isolated`. Plan compilation freezes that result into the
serialized execution plan. Launch then intersects the plan ceiling with the
independently admitted parent/engine ceiling; `isolated` is absorbing, so no
later runtime, wrapper, provider, or caller can widen it.

The Tool kind projects its optional `network_authority` field and deliberately
declares `node_policy` as the omission result. Development build and test items
author `isolated`; ordinary tools keep their existing node ceiling. Generic
dispatch reads only the signed projection declaration and closed vocabulary—it
does not branch on a tool name, compiler, worker, or provider.

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
Lillux owns its process-scoped flat generation, advisory lifetime/cleanup
locks, host-clock/process naming, stale-generation collection, permissions,
and exact descriptor-relative teardown; the engine owns only artifact meaning,
hash verification, and policy quotas.

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

The native Lillux backend uses a private root and new user, mount, PID, IPC, and
UTS namespaces. An isolated network plan also creates a network namespace; host
mode deliberately keeps host networking. The sandbox target is PID 1 inside
its namespace, while the adapter reports its exact host PID to daemon lifecycle
ownership. Signed `filesystem.proc_filesystem` explicitly selects `empty`,
`pid_namespace`, or the separately admitted `pid_namespace_nested` ceiling;
omission refuses admission. The task-only `pid_namespace` mode requires the inspected
`filesystem.pid_namespace_proc` capability and an isolated PID namespace.
It mounts a new procfs with `subset=pid` and read-only, nosuid, nodev, noexec
flags. No host procfs is bound: that would let a same-UID workload traverse
another host process's `root`, `cwd`, or `fd` links and bypass the selected
filesystem surface. Ordinary signal syscalls remain available for sandbox
descendants; namespace PID translation prevents them from naming host
processes.

The actual PID-1 child mounts procfs before pivoting into the prepared private
root. It then detaches the old root, closes filesystem authority descriptors,
installs confinement, and only then reports readiness and accepts release.
Merely unsharing `CLONE_NEWPID` in the adapter parent does not move that parent
into the new PID namespace. Mounting after old-root detachment can also fail
Linux's unprivileged filesystem-visibility check. This construction exposes
only namespace tasks in `pid_namespace` mode, not `/proc/sys` or other non-task top-level entries.
Positive mounts and workspaces cannot replace the selected proc surface.
No workload-specific executable wrapper or host-path extension is involved.
The kernel's [PID namespace documentation](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html)
and [mount visibility check](https://github.com/torvalds/linux/blob/master/fs/namespace.c)
describe these mounting constraints; actual backend qualification remains
required on the selected host.

### Scope-backed nested sandboxes

The node must explicitly configure `process_scopes.nested_sandbox: true` and
`filesystem.proc_filesystem: pid_namespace_nested` together. A signed backend
declaration alone is insufficient: the selected generation must qualify
`process.nested_sandbox` and whole-execution scope control on the actual host.
Unconfigured nodes refuse scope-required launch; there is no weaker fallback.

Only a concrete scope retained by the attachment-required launch can compile
the nested plan. Its allocation is journaled before resource creation and its
exact recovery identity is bound before held spawn. Ordinary unscoped Tools
and preparers still receive strict process-group confinement and read-only,
task-only proc, even on a node whose ceiling permits nested sandboxes.

Nested mode permits child user, mount and PID namespace construction after
the outer target has irreversibly dropped setup capabilities. Its proc mount
contains only the isolated namespace's processes, but deliberately includes
non-task kernel metadata and writable task UID/GID maps. It is **not** the
task-only privacy surface. The native backend refuses a root launch identity;
dropping capabilities alone does not remove root's file-owner permissions.
Before release and during generation qualification it checks that selected
global proc control files cannot be opened for writing. No control value is
written by these checks.

The Lillux Linux minimal-device surface pins and verifies the exact kernel
character devices for null, zero, full, random and urandom; it does not expose
the host device directory. Nested mode additionally supplies the tty device
needed by namespace constructors. Before release, its freshly forked target
detaches its controlling terminal without changing its inherited session or
process group, then proves the terminal is unavailable. The held-launch
attachment boundary still verifies the retained launcher's exact process group;
whole-execution scope ownership is not an exemption from that initial proof.
Ordinary process-group-contained targets do not receive this tty device or
change process session. Qualification exercises the device surface after
private-root exec, including full-device write refusal and nested terminal
detachment; construction-time mount success alone is insufficient evidence.

Fresh child namespaces cannot promote inherited read-only mounts or reveal
masked lower mounts. Targets receive no scope-control descriptors or mounts;
scope namespace creation and process-migration escape interfaces remain
restricted by Lillux. Process groups may change inside this mode because the
retained whole-execution scope, not the original group, owns quiescence and
termination. OS implementation details stay in Lillux; RyeOS retains the
semantic policy, exact lifetime evidence and workspace/recovery obligations.

Host delegation is installation/supervision authority, never an interactive
sudo step during a worker turn. The node controller must already have a
qualified unprivileged placement. A container image cannot manufacture a
writable host delegation where the platform supplies only a read-only one.

Prospective bundle validation is not controller placement. It preserves the
complete signed policy and adapter checks and runs definition validators under
enforced ordinary subprocess isolation, but does not open, qualify or allocate
the node's process scopes from the installer/CLI process. This prospective
snapshot advertises no scope capabilities and refuses scoped launch. Execution
admission resolves a separate fully qualified generation from its supervised
placement; an execution failure never retries through the validation-only
constructor. Initialization therefore does not require a running daemon or
pretend that the installer's process placement proves daemon readiness.

Lillux configuration v3 selects the stable delegation location, not its
reboot-ephemeral inode. Generation admission captures the exact live directory;
allocation v2 and recovery v4 retain that incarnation and boot. Allocation,
cleanup and compilation reject a different incarnation at the same path.

## Supported host supervision

Most nodes remain ordinary account-owned direct nodes. An app root may live
under the account's home directory; RyeOS does not require an administrator-owned
app-root parent, a workload-named root, or a host-local node registry.

When a node is to host dedicated workers whose selected Lillux backend needs
host delegation, the operator performs one explicit local setup after that node
has been initialized:

```sh
ryeos node host setup --confirm [--app-root <existing-node-root>] \
  [--bind <addr>] [--uds-path <path>]
```

This is an administrator-maintenance transition, not a worker operation. The
CLI selects the already initialized app root and its current account; Lillux
performs the host-specific elevation, native service provisioning, executable
pinning and selected process-scope delegation. RyeOS receives only the opaque
Lillux scope configuration and keeps its generic app-root, node-identity,
account, desired-lifecycle and upgrade testimony. Neither node policy nor
worker input can select a privileged executable, service manager, host account
or scope parent.

The node service receives an empty process environment. Its exact app root is
the sole launch argument, and the node-account daemon resolves its already
persisted bootstrap endpoints after credential drop. It never inherits the
administrator's `HOME`, `PATH`, XDG state or login-session environment. Native
interpreter, credential-drop and scope-launch failures are retained as bounded
root-owned attempt testimony in the existing host association; they do not
become node policy or process-signalling authority.

The configured association is deliberately fail-closed. After setup, ordinary
`ryeos start`, `ryeos stop`, and `ryeos node status` use the installed native
service through Lillux. A missing, changed or unhealthy configured service is
an error; RyeOS never silently starts a direct replacement daemon. Nodes with
no association retain the ordinary direct lifecycle. Setup leaves a new service
down, so provisioning does not unexpectedly start a node. That native boot
disposition remains inert after a host reboot; an operator explicitly uses
`ryeos start` when the node should run again.

The native adapter is a Lillux concern. Its first qualified implementation is
the local Artix/runit adapter; systemd, launchd and Windows require their own
Lillux adapter and host qualification before RyeOS claims support. Manager
files, manager controls and OS-specific delegation paths do not appear in
RyeOS policy, bundles, worker environments or project configuration. Workloads
never receive host-service control or elevation authority.

The protected association detects an app-root replacement, but it does not
pretend that a user-owned home is immutable against that account. The worker
security boundary remains the admitted filesystem, process, network and
workload-client authority. Installation stages and validates a new package
generation before it inhibits a configured service, retains prior up/down
intent through replacement, and verifies the installed generation before
restoring that intent.

The node's runtime store retains one independent host-lifetime reset fence
before scope allocation. Exact last retirement clears it. If execution schemas
have changed while that fence remains unsettled, reset requires proof that the
original host lifetime ended; daemon restart alone is insufficient. Stop and
settle work before upgrading rather than discarding live recovery authority.
Startup uses that same Lillux lifetime evidence to settle a former daemon's
unattached attempt after its host lifetime ended. Same-host scope emptiness
does not independently prove that a pre-attachment launcher was reaped, so
that case retains its credential and workspace fences.
These are local recovery facts under the node's exclusive state ownership,
not transferable evidence that another machine stopped. Copying a live
runtime database or its lifetime witness to another host is not a supported
handoff. Cross-site execution uses its existing source-settlement and target-
admission authorities; it must not import local process coordinates as cleanup
proof.

Native host qualification is available as the isolated
`.github/workflows/lillux-native.yml` job in the RyeOS source repository.
It provisions only a disposable test VM and runs exact Lillux lifecycle and
namespace fixtures; it is neither a release job nor evidence that another
deployment host supplies the same capabilities. Worker turns never invoke
this provisioning entry or request administrator credentials.

Process attachment is orthogonal to this isolation mode. Every daemon-owned
launch is created awaiting attachment, its exact target identity is persisted,
and only then is the target released to execute. In disabled mode Lillux holds
the direct target in its native pre-exec boundary. In enforce mode the adapter
holds its actual target and reports that target's host PID through the strict
isolation protocol. A supervised request without the requested target hold is
rejected; there is no fallback to wrapper identity or direct execution.

Workspace freeze uses the same ownership split. RyeOS retains the durable
execution coordinate, while Lillux exclusively performs exact-group stop,
procfs membership enumeration, birth verification, pidfd retention, resume or
termination, and bounded settle waits. No workspace-freeze executor or daemon
caller supplies raw signal numbers or treats a numeric PID/PGID as process
authority.

For ordinary strict-group launches, Lillux creates a new session before it executes the adapter, and the target
inherits the retained wrapper's process group. The wrapper remains unreaped
while Lillux terminates that group, which keeps the PGID reserved even if the
initial target exits while descendants are still running. Timeout,
cancellation, output overflow, attachment failure, release failure, and wait
failure all use that stable group ownership. The strict native sandbox denies
group/session escape; it does not silently relax this restriction to accommodate
a nested sandbox. Scope-backed launches use the separate containment contract
above. Resource accounting limits still require their own admitted capabilities;
whole-execution termination alone is not proof of aggregate resource limiting.

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
surface is pinned separately, `/dev` is backend-provided, `/proc` follows the
explicit node-owned process-filesystem choice,
and `/tmp` is private tmpfs.

`limits.open_files` becomes `RLIMIT_NOFILE` before exec. Output is retained only
up to `limits.stdout_bytes` and `limits.stderr_bytes`; pipes continue to be
drained so the child cannot deadlock, and overflow terminates the supervised
workload with an explicit truncated-output result. When a request already has
a lower cap, the lower value wins. Any validation, mount, backend, or limit
failure refuses the spawn.

Lillux's existing bounded stdout capture supports one byte-preserving live
reader after process release. Protocol code must decode binary framing through
that reader, never through the ordinary text result's lossy UTF-8 conversion.
The reader shares the capture and drainer, has no independent process authority
or unbounded output queue, and retains unread bytes across process settlement.
Reading must run concurrently with the existing wait/abort owner so deadlines
and output-limit supervision continue to advance. Capture closure, a protocol
terminal frame, and actual process settlement are separate facts; cleanup can
close capture without natural pipe EOF. Neither EOF nor a claimed successful
terminal frame overrides cancellation, overflow, timeout or a failing exit.

## Launch coverage

Workspace membership is process-lifetime authority, not an inference from a
thread's terminal status. A failed child may retire its exact membership only
while its launch claim remains active and its preparation boundary proves no
process contact, or its held-process owner proves abort and reap. The caller's
launch owner must match the binding; settlement also refuses live descendants.
Attached identity and membership settle atomically using the existing reaped
process owner. Never clear the attachment first and then infer the borrower is
safe because its PID is absent.

Direct and managed launch paths preserve this distinction through failure.
Task panic, cancellation, a generic engine error, and unproved abort/release
cleanup retain quarantine; unconditional resource destruction is not settlement
evidence. Shutdown retains the coordinator's recovery obligations. These rules
apply equally to fresh, detached and resumed child execution and do not give a
child authority to close or publish its parent's workspace.

Unknown command delivery is distinct from unknown process cleanup. When the
exact placement/capsule has retained reaped-worker testimony, no current epoch,
and no unsettled worker or conflicting pool ownership, workspace capture may
proceed without settling or replaying the unknown command. A newer pending
boot defeats older cleanup evidence. An absent PID alone proves neither case.

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
Setuid, setgid, and file-capability adapter or external artifact executables are
refused, and verified bytes execute from sealed private captures.
The supported Docker profile adds `SYS_ADMIN` and uses unconfined seccomp and
AppArmor profiles for the required namespace and mount operations. A
purpose-built AppArmor profile may replace `apparmor=unconfined` when it grants
the same operations.

This boundary limits filesystem visibility and writes, network namespace
access, the target environment, and open file descriptors. It is not a virtual
machine, does not defend against kernel vulnerabilities, and does not yet set
CPU, memory, or per-isolation process quotas. Do not model a process quota with
`RLIMIT_NPROC`: it is scoped to the daemon's real UID rather than one isolation.
The native backend does not yet claim aggregate cgroup resource containment;
requests for aggregate CPU, memory, or process ceilings fail closed until
Lillux supplies a separately qualified aggregate-quota authority. The scoped
lifecycle implementation above establishes freeze/termination/recovery, not
these resource ceilings; no OS controller mechanics move into RyeOS policy
or execution code merely because a scope exists. Transitive imports,
libraries, and assets remain live read-only unless separately content-pinned.
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

## Target-local network inputs

Policy v4 separates permission to use the node network from general host
filesystem access. `network.runtime_files` explicitly selects bounded regular
files, each with `source`, `destination`, and `max_bytes`. An empty list supplies
none. There are no implicit resolver or certificate-directory mounts.

The resolved node generation uses Lillux to read exact regular descriptors
under their bounds and seal the bytes. Operator-selected system symlinks are
resolved once before descriptor capture. Missing, oversized, special-file,
overlapping-destination and node-private inputs fail closed. Subsequent source
replacement cannot alter the sealed generation. Refresh the node generation
when resolver settings or trust roots intentionally change.

Only an effective host-network launch receives those read-only file mounts.
An isolated-network child receives none, even when its node permits networking
for other workloads. File destinations cannot override its workspace, code,
realizations, private state, devices or proc surface. The exact input digests
belong to the local isolation admission class and launch provenance, not the
portable program identity. A different target supplies its own admitted inputs.

The development profile selects node resolver/hosts files and the node's CA
bundle explicitly. It does not turn that bundle into a portable build product
or give a workload permission to choose different trust roots. Immutable
workload-specific runtime/trust artifacts still use ordinary signed environment
and external-content declarations. Changing transport inputs does not authorize
new endpoints, worker operations, signing, credentials or publication.

For an existing node, `ryeos node policy-apply isolation <source.yaml>` validates
and atomically replaces that one signed member while retaining the other policy
values. The daemon must be stopped. An obsolete schema in the replaced member
is verified as signed predecessor bytes, not interpreted as current authority;
every member of the resulting generation must compile. There is no implicit
profile reset, migration, or missing-field fallback.
