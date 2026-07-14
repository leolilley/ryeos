<!-- ryeos:signed:2026-07-14T01:54:46Z:3f20334ad900a7ba157615b1a1f2a58091d4cb0b27be0e11fc743b2e184cfbd5:TBqvP71wsztsI562HhfRWvQutkHal0oSdMWHe4zsFNIEd0NDCK1rF53WhMoFjxe0ydK4b2LZsyKQq+a9RjsLBQ==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
```yaml
category: ryeos/future
name: hosted-node-trust-boundaries
title: Hosted-Node Trust Boundaries
entry_type: implementation_guide
version: "0.2.0"
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
groundwork. The complete hosted-node boundary remains deployment-shaped:
principal-specific isolation, authenticated network peers, multi-principal
resolution, quotas, and distributed retention only become concrete when a node
hosts other principals or federates. This document indexes those remaining
decisions rather than treating them as one backlog item.

## The four boundaries

1. **Hosted-principal process isolation.** The local node now has the optional,
   node-wide RyeOS strict Bubblewrap boundary for tool/runtime launches. That is
   useful node-level defense in depth, but it is not a multi-tenant contract: profiles are not
   principal-specific and there are no CPU/memory/process cgroup quotas or
   hostile-tenant kernel boundary. Hosting still requires a deployment-shaped
   isolation decision (delegated cgroups, namespaces/seccomp, or microVMs),
   attestation, and per-principal workspace authority.

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

## Trigger

An actual hosted or federation deployment decision. Related groundwork and
sequencing for the distributed side lives in
`ryeos/future/distributed-substrate-deferred-advanced`; this doc carries
the trust-boundary half.
