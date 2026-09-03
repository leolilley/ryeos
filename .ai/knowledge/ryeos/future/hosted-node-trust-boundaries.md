<!-- ryeos:signed:2026-08-27T04:21:36Z:900b03977a013ae3347dde0312a3e8041339a970bf8694b7787a139296bbded6:16cGcmzpsU8zG2F3nrN1JraKgboOxi2uR7XbhPZT1PU5kg3GmaMLY/XBs6pMcMXo33mgVZYwPMb92vqJveN3Aw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: ryeos/future
name: hosted-node-trust-boundaries
title: Hosted-Node Trust Boundaries
entry_type: implementation_guide
version: "0.7.0"
description: The remaining trust boundaries for hosting other principals, including deployment-grade isolation around typed signed backends.
tags:
  - hosted-node
  - federation
  - isolation
  - security
```

# Future: Hosted-Node Trust Boundaries

## Status

RyeOS now has a hosted structured-worker application boundary for trusted
owner-configured workloads, including private homes, credential generations,
typed approvals/effects, restart recovery, portable checkpoints, and explicit
cross-site placement. That application/session substrate is not the hostile
multi-tenant boundary owned by this note.

The node-owned RyeOS process-isolation boundary is implemented as optional
Linux hardening and remains disabled by default. When enabled, it gives RyeOS
one immutable, node-owned launch boundary where
verified code identity, descriptor-pinned filesystem authority, environment,
network posture, bounded stdout/stderr retention, target-process-group
supervision, and enforceable per-process limits meet. That is the right
foundation for extracting a backend-neutral isolation plan because later
isolation can wrap or further narrow one explicit boundary instead of finding
and replacing scattered spawn paths. When disabled, trusted signed launches may
still receive exact admitted inputs through a daemon-owned private workspace;
RyeOS reports honestly that kernel confinement was not enforced.

It is not yet a hostile multi-tenant boundary. The current policy is node-wide,
not principal-specific; CPU, memory, and process-count cgroup quotas are
deferred; host PIDs remain visible to syscalls; and same-UID signal isolation is
not claimed. A deployment that runs hostile
workloads must still add cgroups plus a VM, microVM, or dedicated outer worker.

Attachment-before-execution now closes the local creation-to-publication crash
window. Direct targets remain held until their exact identity is durable and
die with the daemon if it exits before attachment; isolated targets use the
same lifecycle transition at the backend's actual target boundary. After
attachment, startup reconciliation owns the exact persisted identity.

The future outer worker/cgroup remains necessary for a different reason: it
must own quotas and whole-workload teardown across descendants that escape the
local process group, hostile same-UID behavior, and worker/kernel failure. It is
not a substitute for the local durable attachment boundary.

The complete multi-principal hosted-node boundary remains deployment-shaped:
principal-specific identity and isolation, authenticated network peers,
multi-principal resolution, storage and secret partitioning, quotas, audit, and
distributed retention only become concrete when a node hosts other principals
or federates. This document indexes those remaining decisions rather than
treating them as one backlog item.

The sequencing relationship to local workers, portable evidence, and
federation is summarized by
`knowledge:ryeos/future/substrate-growth-roadmap`. In particular, this hosted
outer boundary is not a lifecycle of the signed `worker` item kind. It may host
such a worker, but it owns tenant and kernel containment rather than the
worker's admitted application protocol.

## The four boundaries

1. **Hosted-principal process isolation.** The local node now has an optional,
   node-wide backend-driven confinement boundary for tool/runtime launches.
   That is useful node-level defense in depth, but it is not a multi-tenant
   contract: profiles are not principal-specific and there is no hostile-tenant
   kernel boundary. Hosting still requires a deployment-shaped isolation
   decision, per-principal workspace authority, and attestation.

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

Hosted execution should resolve a typed isolation requirement and layer controls
rather than attempt to turn one process-isolation policy into the whole tenancy model:

```text
signed request + node admission
  -> principal/job execution authority
  -> selected RyeOS inner confinement backend
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

The outer worker owns the kernel-level containment decision. The selected inner
backend owns only the application-boundary capabilities it proves: which
verified executable is allowed to run and which resources are presented to it.
Cgroups own exhaustion, accounting, and
authoritative whole-workload teardown even when descendants create new process
groups or sessions. The current node launch supervisor owns bounded stdout and
stderr retention because guest memory limits do not cover daemon-owned buffers;
hosted event-stream limits and optional private output spooling remain future
work.
Principal storage, secret, and network layers own cross-tenant data authority.
None of those layers should be inferred from an item-authored isolation profile.

Backend selection and capability matching follow
`ryeos/core/node/execution-isolation`. In particular, a
hostile-workload requirement cannot fall back to direct execution or be marked
satisfied by a process-confinement backend. The selected backend kind,
capabilities, worker identity, and inspection/attestation evidence belong in
the durable job record.

The current shared launch-policy path intentionally makes this later work
additive. Its immutable startup snapshot can become an input to worker
provisioning; its launch context
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

An actual hosted deployment decision or the first remote job that would run
code for a principal outside the node owner's trust boundary. Hosting is the
principal/isolation stage before full federation, not a synonym for it.
Related groundwork and sequencing for the distributed side lives in
`ryeos/future/distributed-substrate-deferred-advanced`; this doc carries
the trust-boundary half.
