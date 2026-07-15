<!-- ryeos:signed:2026-07-15T08:13:19Z:ecb3e521d34401c89f9ed043c10b5e93b348176a6aee346b4a16f96e9e5e7874:16gBYMRFQU74FOG1MGFHjjBDnIeBpTHTClaVLgV20/i7ZSDQKHhFcNRRoEbE5CP8hkW9T016utIRvJpi2DhIDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, sandbox, bubblewrap, security, subprocess, node-policy]
version: "1.2.0"
description: >
  Node contract for the node-owned subprocess sandbox: strict policy
  schema, startup pickup, enforcement behavior, diagnostics, and limits.
---

# Execution Sandbox

RyeOS can launch executable tools and runtimes through a node-owned Bubblewrap
policy on Linux. The only policy source is
`<app-root>/.ai/node/sandbox.yaml`. `ryeos init` creates it once; later edits
belong to the node owner. Items, bundles, requests, and environment variables
cannot activate the sandbox or weaken its controls.

This is the current Linux implementation, not a portable sandbox abstraction.
Its policy is structured, but backend inspection, capture, filesystem setup,
and launch compilation are Bubblewrap-specific. A later multi-platform design
must resolve typed isolation requirements against node-owned backend descriptors
and fail closed when a platform cannot provide the required capabilities. That
deferred architecture is recorded in
`ryeos/future/data-driven-execution-isolation-backends`.

The engine also keeps node trust separate from project/request trust. The
`node_trust_store` is loaded only from persistent node configuration and is the
authority for installed bundle schemas, parsers, handlers, protocols,
runtimes, and native executor manifests. Project keys and caller-scoped trust
overlays may authorize project items, but they cannot make a new host binary or
installed runtime node-trusted.

The policy has two modes:

- `mode: disabled` does not wrap the subprocess in Bubblewrap. This is the
  default and does not require Bubblewrap. Node-owned stdout/stderr retention
  caps, signature, trust, authorization, and capability checks still apply,
  but there is no OS confinement or verification-to-exec path pinning.
- `mode: enforce` applies the complete policy and refuses the launch if any
  requested control cannot be enforced.

## Default policy

```yaml
version: 1
mode: disabled
backend:
  kind: bubblewrap
  executable: /usr/bin/bwrap
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

The default is deliberately inert. To opt in, install unprivileged Bubblewrap
0.11.0 or newer, change the node-owned mode to `enforce`, run
`ryeos node doctor`, and restart the node.
The daemon loads one immutable policy generation at startup; editing the file
does not change a running daemon. The daemon-backed `ryeos daemon status`
surface (`service:node/status`) reports the loaded mode, version, source, and
source digest. `ryeos node status` is the narrower local lifecycle probe.

## Strict schema

- `version` must be `1`, the first published strict-policy schema. Other
  versions and unknown fields are rejected without aliases or translation.
- `backend.kind` is `bubblewrap`.
- `backend.executable` must be absolute in both modes. Enforce mode also
  resolves it, requires an unprivileged executable regular file (no setuid,
  setgid, or Linux file capabilities), and captures its exact bytes in the
  node-private verified-artifact store at policy-load time. Every launch uses
  the captured inode rather than reopening the configured path; inspection
  reports its captured SHA-256 and version. Startup runs that exact capture's
  `--version` and `--help` probes and requires Bubblewrap 0.11.0+ with
  `--args`, `--bind-fd`, `--json-status-fd`, `--ro-bind-fd`, and `--argv0`.
  The executable selected in this node-owned file is node-approved host
  execution authority; these probes establish interface compatibility, not
  the provenance of arbitrary bytes.
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
  caller but does not install the sandbox policy's `RLIMIT_NOFILE`.
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
rejected before Bubblewrap can reinterpret it inside the new root. Enforce mode
then pins system, policy-selected, identity, socket, bundle, writable, and code
mount sources with `O_PATH` descriptors and gives Bubblewrap `--bind-fd` or
`--ro-bind-fd` references. Caller-supplied inherited descriptors are refused.
The descriptors remain close-on-exec in the
multithreaded daemon and are made inheritable only in the forked child, so a
pathname replacement cannot redirect a validated mount and concurrent spawns
cannot inherit another launch's authority.

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
Bubblewrap executable, or the daemon socket. The resolved host home itself and
its ancestors are also rejected. The only app-root exceptions are a
daemon-provenance execution workspace directly beneath
`.ai/state/cache/executions` and the exact daemon-owned checkpoint directory.
Absolute paths in this file are node-owner authority and should be kept narrow.

## Process boundary

Enforce mode canonicalizes the command, project, working directory, mount
sources, and complete constructed environment before spawn. Bubblewrap itself
receives an empty environment; target variables are installed inside the
namespace with `--setenv`, so loader controls such as `LD_PRELOAD` do not run
before isolation. The complete Bubblewrap option vector, target environment,
and target arguments are NUL-separated in an immutable sealed anonymous file;
the host-visible Bubblewrap command line contains only `--args <fd>`. `TMPDIR`
is always `/tmp`, backed by a private tmpfs.

The namespace uses a private root and new user, IPC, and UTS namespaces. An
isolated network policy also creates a network namespace; host mode deliberately
keeps host networking. RyeOS retains the host PID namespace because managed
runtimes attach their PID to the daemon and host-side PGID cancellation and
restart reconciliation must address the same process identifiers. The namespace
contains an empty `/proc` directory rather than the host procfs: exposing the
host mount would let a same-UID workload traverse another process's `root`,
`cwd`, or `fd` links and bypass the selected filesystem surface. PID syscalls
still use host identifiers, and the sandbox does not claim PID or same-UID
signal isolation.

Bubblewrap reports the target's host PID through a private
`--json-status-fd` pipe for accounting. Lillux creates a new session before it
executes Bubblewrap, and the target inherits the retained wrapper's process
group. The wrapper remains unreaped while Lillux terminates that group, which
keeps the PGID reserved even if the initial target exits while descendants are
still running. Timeout, cancellation, output overflow, and wait failure all use
that stable group ownership. A descendant that deliberately creates another
session remains outside this local process-group guarantee; hostile hosted
workers additionally use `cgroup.kill` at the outer worker boundary.

Managed launches pin the exact target and retained group leader before durable
attachment. The version-1 process identity records the boot ID, numeric target
and leader IDs, and both `/proc` start-time ticks. Every later target or group
signal first opens a pidfd, proves the stored incarnation, and uses pidfd
signalling; RyeOS never turns a stored PID/PGID into raw `kill` authority.
Self-attaching runtimes must match their accepted UDS `SO_PEERCRED` PID and are
pinned through that socket's `SO_PEERPIDFD`.

Normal shutdown closes authoring and tears down attached identities within a
shared node-owned grace bound. After an unclean daemon exit, the exclusive
state lock proves that any still-live, exactly matched attachment belongs to the
previous daemon; startup kills that group before recovery launches a
replacement. A same-boot attachment whose leader birth identity can no longer
be proven is quarantined rather than cleared or signalled.

One unavoidable local crash window remains between kernel process creation and
publication of the durable attachment. An immediate daemon `SIGKILL` in that
window can leave an untracked process group that state-based recovery cannot
name. A hosted hostile-workload boundary must place the daemon/launch handoff
inside an outer cgroup, VM, microVM, or dedicated worker that owns whole-workload
teardown independently of the attachment row.

Configured writable binds are installed first. Read-only policy mounts, the
verified-code authority mirror and exact artifact, and any non-system command
are installed afterward, so a broad writable ancestor cannot hide them.
Verified node or bundle commands without a content identity are refused;
project and node-selected commands are copied from one opened byte sequence and
run from the same synthetic read-only namespace. `/usr`, `/bin`, `/lib`, and
`/lib64` are descriptor-pinned read-only; the small required `/etc` runtime
surface is pinned separately, `/dev` is created by Bubblewrap, `/proc` is empty,
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
healthy inactive opt-out without requiring Bubblewrap. Enforce mode verifies
backend availability and capture digest, and reports filesystem, network,
environment, open-file, captured-output, and verified-artifact-limit posture.

The published container runs the default disabled profile without extra
capabilities. Enforce mode requires unprivileged Bubblewrap 0.11.0 or newer
and usable user namespaces; setuid or file-capability backends are refused.
The supported Docker profile adds `SYS_ADMIN` and uses unconfined seccomp and
AppArmor profiles for the required namespace and mount operations. A
purpose-built AppArmor profile may replace `apparmor=unconfined` when it grants
the same operations.

This boundary limits filesystem visibility and writes, network namespace
access, the target environment, and open file descriptors. It is not a virtual
machine, does not defend against kernel vulnerabilities, and does not yet set
CPU, memory, or per-sandbox process quotas. Do not model a process quota with
`RLIMIT_NPROC`: it is scoped to the daemon's real UID rather than one sandbox.
Host PIDs remain visible to syscalls, same-UID signal isolation is not claimed,
and transitive imports, libraries, and assets remain live read-only rather than
content-pinned.
Disabled means no OS sandbox, not unverified execution; resolution, signature,
authorization, and capability checks remain active.

## Local use, hosted nodes, and Docker

For a trusted, single-user local node, keeping the default disabled mode is a
reasonable choice. It avoids namespace requirements while retaining RyeOS's
normal signed-item and capability model. Enforcement becomes important when a
node executes bundles for another person, accepts remotely supplied projects,
or shares one worker among workloads with different trust.

Docker and the RyeOS sandbox protect different boundaries. A container isolates
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

Do not generalize this policy by adding more backend-specific fields to the
current schema. When another operating system or isolation backend is actually
needed, extract the backend-neutral plan and typed capability model described in
`ryeos/future/data-driven-execution-isolation-backends`, then keep Bubblewrap as
one Linux adapter.
