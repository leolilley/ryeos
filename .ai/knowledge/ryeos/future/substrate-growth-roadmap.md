<!-- ryeos:signed:2026-08-12T07:28:15Z:4d1b9effb1491583a5e45aafef2346280a6b6785085da129d4e7ed6299fd382a:5M+LLB1HqmiHy3M1VZmjolmC5ob+8KA77ZVqIh+k3tv1NOS4eXPDMLLDk6L7wuPQ86oXKrM/BuCiXqMb5GL/Aw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: substrate-growth-roadmap
title: RyeOS Substrate Growth Roadmap
description: Boundary map from current single-node execution acceptance through managed workers, portable evidence, hosted nodes, and federation
entry_type: reference
version: "0.1.0"
```

# RyeOS substrate growth roadmap

This note connects the active execution work to the larger deferred RyeOS
directions. It is a sequencing and ownership map, not an implementation plan.
The linked owner documents remain authoritative for each deferred boundary.

## Current horizon: complete one-node truth

The current acceptance workloads exercise two complementary shapes:

- deep, cost-bearing agent execution: provider evidence and replay, pinned
  continuation, project observations, execution identity, comparison, and
  explanation of a run as it unfolds;
- broad execution: parallel follow cohorts, recorded subprocess effects,
  cumulative collection, restart recovery, cache reuse, and state-lock
  contention.

Together they establish whether one RyeOS node can execute real projects while
remaining exact, recoverable, observable, and efficient. The immediate loop is
complete only when:

1. both workloads finish representative runs without project-side substrate
   workarounds;
2. first execution and replay retain their distinct authoritative evidence;
3. remaining RyeOS-owned contention and cache misses are measured before being
   optimized;
4. a complete cost-bearing run pair passes the landed execution-comparison
   contract; and
5. the web and terminal execution field show the same project, run, definition,
   cost, decision, and replay facts from durable evidence.

This is the shared foundation for every direction below. Managed workers and
federation must extend it rather than creating parallel execution systems.

## Growth map

```text
representative deep + broad workloads
    |
    v
single-node execution truth
  identity + realization + capsules + effects + continuation + field
    |
    +---------------- local execution ----------------+
    |                                                 |
    |  fixed persistent worker (landed)               |
    |    -> sealed local qualification                 |
    |    -> generation-state capsules, when needed    |
    |    -> leased warm workers, when measured         |
    |                                                 |
    +---------------- portability --------------------+
    |                                                 |
    |  verification-only evidence export              |
    |    -> hosted principal/job boundary              |
    |    -> durable remote jobs and staged transfer    |
    |    -> cross-node continuation and federation     |
    +-------------------------------------------------+
```

The two branches interact but are not one feature. A managed runtime worker
changes how an admitted invocation is hosted inside a node. Portability and
federation change where authority, content, evidence, and continuation may
travel.

## Local execution branch

The generic signed `worker` kind and its fixed persistent-session lifecycle are
landed. A worker is a mechanical execution vessel, not a synonym for a model,
latency reuse, or hostile-workload isolation.

The deferred progression is deliberately gated:

1. **Sealed local inference.** Qualify one exact local realization only when
   its retained numerics, sampler, compiled artifacts, and independent runs
   justify a sealed claim. Recorded remains an honest final class when they do
   not.
2. **Generation-state capsules.** Add opaque, content-addressed KV/token/sampler
   lineage only after sealed qualification and a real workload needs exact
   park, resume, or fork.
3. **Leased managed runtimes.** Extend the same `worker` kind with a separately
   admitted single-use invocation lease only when measurements show material
   RyeOS-owned cold-start or connection cost after workflow and provider work.

Search and inference workloads may pull forward local hypothesis forks and
resumable generation. Evaluator or simulation workloads may justify warm
vessels only if their own measurements show setup cost rather than useful work
is the bottleneck. No project should be rewritten around workers merely
because the mechanism exists.

Owner: `knowledge:ryeos/future/local-execution-roadmap` and its linked worker,
sealed-inference, and generation-capsule documents.

## Portability branch

The four local layers are present: signed capability, admitted invocation,
durable consequence, and rebuildable projection. What remains portable is an
independently verifiable chain leaving its producing node.

The first step is verification-only export:

- the exact capsule and content-complete CAS/effect/evidence closure;
- a deterministic verification profile; and
- a node-signed statement binding the export to an exact chain head.

A completed execution proof intended for independent review is the preferred
first consumer because it makes the completeness and disclosure boundaries
concrete. Verification-only export does not grant continuation authority and
does not require federation.

Owner: `knowledge:ryeos/future/portable-execution-graph-advanced-path`.

## Hosted-node branch

Before unrelated principals share a node, RyeOS needs a deployment-shaped
trust boundary:

- principal and job identity carried from admission through execution;
- principal-scoped project state, caches, secrets, network policy, audit, and
  cleanup;
- cgroup-backed aggregate CPU, memory, PID, and workload-lifetime ownership;
- a VM, microVM, dedicated worker, or equivalent outer isolation boundary
  selected by the threat model; and
- principal-aware resolution, quota, retention, and UI/project registry.

The hosted outer worker is not the signed RyeOS `worker` kind. The signed item
is an application execution vessel. The outer worker/cgroup/VM is a kernel and
tenant containment boundary. A hosted deployment may place a signed worker
inside that boundary, but their identities and lifecycle claims remain
separate.

Owner: `knowledge:ryeos/future/hosted-node-trust-boundaries` and
`knowledge:ryeos/future/ryeos-ui-local-project-registry-and-multitenancy`.

## Federation branch

Federation is not daemon forwarding added to local execution. It begins only
after the portable evidence and hosted-principal boundaries are explicit. Its
initial substrate is:

1. bounded closure transfer and staged imports;
2. durable remote jobs and CAS attribution;
3. typed principal-aware handler context;
4. generic signed heads and policy admission;
5. key succession, revocation, delegation, audience binding, and replay
   protection; and then
6. remote execution, mirrored heads, cross-node continuation, repair, and
   distributed retention.

The local capsule, effect record, pinned generation, follow identity, and field
projection remain the model. Federation transports and re-admits those facts;
it must not invent a looser distributed identity system.

Owners: `knowledge:ryeos/future/distributed-substrate-deferred-advanced`,
`knowledge:ryeos/future/key-lifecycle`, and
`knowledge:ryeos/future/mcp-server-auth`.

## Pull-forward order

1. Finish representative deep and broad single-node workloads and
   execution-field acceptance.
2. Use the resulting evidence to decide whether sealed local inference has
   immediate workload value.
3. Implement verification-only execution-proof export when evidence must leave the
   node.
4. Establish hosted principal/job isolation before accepting untrusted remote
   execution.
5. Pull durable remote jobs, staged transfer, attribution, and signed heads as
   the first distributed substrate.
6. Design key lifecycle and delegation before cross-node continuation.
7. Activate federation only when admission, isolation, evidence, and retention
   compose across sites.

Generation capsules and leased workers stay on their own evidence gates; they
are not prerequisites for portability or federation. Reflexive deployment,
distributed scheduler leases, and broader family analytics should be pulled
forward only when the corresponding activation, placement, or analysis problem
is real.

## Decision rule

The next ambitious mechanism is chosen by observed need:

| Evidence from current work | Pull forward |
|---|---|
| Local inference can be independently reproduced byte-for-byte | sealed local qualification |
| An admitted search needs exact prefix park/resume/fork | generation-state capsules |
| RyeOS-owned warm-start cost remains materially high | leased managed runtimes |
| A completed execution must be verified outside its node | verification-only export |
| Another principal's work must run on the node | hosted principal/job boundary |
| Content, heads, or execution must move between nodes | durable jobs and federation substrate |

Landing a generic mechanism is not evidence that its most ambitious consumer
should be implemented next.
