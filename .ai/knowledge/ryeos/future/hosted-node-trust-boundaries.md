<!-- ryeos:signed:2026-07-14T10:12:37Z:96742b8f0baed892c696bdef9a0ffc1cbcdf8170cf49e030dc549f4954b5fee7:xkLF6YFhpVXc1J3r4px7zxsDntBh899NbW0de+cJ+uoG1TpAv23G6uJTN5CA79ijq3FtOKEXZiRo/i24M3l2Aw==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
```yaml
category: ryeos/future
name: hosted-node-trust-boundaries
title: Hosted-Node Trust Boundaries
entry_type: implementation_guide
version: "0.5.0"
description: The remaining trust boundaries for hosting other principals, including deployment-grade isolation around the implemented node-owned process sandbox.
tags:
  - hosted-node
  - federation
  - sandboxing
  - security
```

# Future: Hosted-Node Trust Boundaries

## Status

The node-owned RyeOS strict process sandbox is implemented as optional
groundwork. It gives RyeOS one immutable, node-owned launch boundary where
verified code identity, descriptor-pinned filesystem authority, environment,
network posture, bounded stdout/stderr retention, target-process-group
supervision, and enforceable per-process limits meet. That is the right
foundation for hosted execution because later isolation can wrap or further
narrow one explicit boundary instead of finding and replacing scattered spawn
paths.

It is not yet a hostile multi-tenant boundary. The current policy is node-wide,
not principal-specific; CPU, memory, and process-count cgroup quotas are
deferred; host PIDs remain visible to syscalls; same-UID signal isolation is not
claimed; and transitive imports, libraries, and assets remain live read-only
views rather than content-pinned artifacts. A deployment that runs hostile
workloads must still add cgroups plus a VM, microVM, or dedicated outer worker.

Durable process attachment closes PID-reuse races after publication, but not
the crash window between kernel process creation and that publication. A daemon
`SIGKILL` in the window can leave an untracked local group. The future outer
worker/cgroup must own launch placement and whole-workload teardown so recovery
does not depend on a row the daemon may never have committed.

The complete hosted-node boundary remains deployment-shaped:
principal-specific identity and isolation, authenticated network peers,
multi-principal resolution, storage and secret partitioning, quotas, audit, and
distributed retention only become concrete when a node hosts other principals
or federates. This document indexes those remaining decisions rather than
treating them as one backlog item.

## The four boundaries

1. **Hosted-principal process isolation.** The local node now has the optional,
   node-wide RyeOS strict Bubblewrap boundary for tool/runtime launches. That is
   useful node-level defense in depth, but it is not a multi-tenant contract:
   profiles are not principal-specific and there is no hostile-tenant kernel
   boundary. Hosting still requires a deployment-shaped isolation decision,
   per-principal workspace authority, and attestation.

2. **MCP network authentication.** Local MCP integration trusts the local
   socket boundary. Networked MCP needs real peer authentication and an
   authorization story mapping MCP callers into principals.

3. **Multi-principal resolution.** Resolution, project spaces, and vault
   scoping assume the one operator identity. Hosting means per-principal
   resolution roots, quota, and isolation between principals' project
   state — a resolver-level design, not a permissions patch.

4. **Remote-state GC.** The GC profiles sweep local state only (CAS,
   caches, traces, runtime history, retention). Federated/remote object
   graphs, admitted heads, and synced project state have no retention
   story; distributed GC decisions interact with admission and cannot be
   local-only.

## Target hostile-workload stack

Hosted execution should layer controls rather than attempt to turn one
Bubblewrap policy into the whole tenancy model:

```text
signed request + node admission
  -> principal/job execution authority
  -> RyeOS strict inner sandbox
       exact entry bytes, fd-pinned mounts, narrow env/network/filesystem,
       bounded stdout/stderr, and target/wrapper process-group supervision
  -> per-principal or per-job cgroup v2
       CPU, memory, process count, workload-lifetime kill, and eventually I/O
       accounting/limits
  -> outer worker boundary selected by threat model
       dedicated worker process/user, VM, or microVM
  -> hosted event supervision and optional private output spooling
       event caps plus larger node-private output retention where required
  -> principal-scoped storage, secrets, network policy, audit, and GC
```

The outer worker owns the kernel-level containment decision. RyeOS strict owns
the inner application boundary: which verified executable is allowed to run and
which resources are presented to it. Cgroups own exhaustion, accounting, and
authoritative whole-workload teardown even when descendants create new process
groups or sessions. The current node launch supervisor owns bounded stdout and
stderr retention because guest memory limits do not cover daemon-owned buffers;
hosted event-stream limits and optional private output spooling remain future
work.
Principal storage, secret, and network layers own cross-tenant data authority.
None of those layers should be inferred from an item-authored sandbox profile.

The current sandbox intentionally makes this later work additive. Its immutable
startup snapshot can become an input to worker provisioning; its launch context
already carries execution provenance; its runtime-wide `apply` stage is the
single handoff where a cgroup or outer-worker assignment can be required; and
future per-tool or per-principal profiles can intersect with the node policy only
to narrow it.

## Hosted-isolation completion criteria

Do not describe a deployment as hostile multi-tenant until it has, at minimum:

- a distinct principal/job identity carried from admission into execution;
- CPU, memory, process-count, and workload-lifetime enforcement outside the
  child process's control;
- bounded stdout, stderr, and event capture or node-private spooling, with
  overflow behavior that cannot exhaust daemon memory or block teardown;
- an outer worker boundary appropriate to the accepted kernel threat model;
- cross-principal PID/signal isolation, or separate workers that make the
  same-UID signal issue inapplicable;
- principal-scoped workspaces, caches, secrets, network egress, accounting,
  audit, and cleanup;
- a decision on whether transitive code/assets must be closure-pinned or are
  acceptable as an admitted immutable image/snapshot; and
- failure semantics that tear down the cgroup/worker and reconcile durable job
  state without reusing leaked authority, including daemon death before durable
  process attachment.

## Trigger

An actual hosted or federation deployment decision. Related groundwork and
sequencing for the distributed side lives in
`ryeos/future/distributed-substrate-deferred-advanced`; this doc carries
the trust-boundary half.
