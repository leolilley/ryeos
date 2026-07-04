```yaml
category: ryeos/future
name: hosted-node-trust-boundaries
title: Hosted-Node Trust Boundaries
entry_type: implementation_guide
version: "0.1.0"
description: The local-trust assumptions that must harden before hosting other principals — sandboxing, MCP network auth, multi-principal resolution, remote-state GC — each requirement-shaped, none buildable ahead of a deployment decision.
tags:
  - hosted-node
  - federation
  - sandboxing
  - security
```

# Future: Hosted-Node Trust Boundaries

## Status

Deferred as a group, on purpose. The substrate currently assumes one local
operator who owns the machine; these four boundaries only become real when
a node hosts OTHER principals or federates. They are requirement-shaped,
not backlog-shaped: designing them against imagined deployments produces
the wrong designs. When a hosted/federation decision lands, each needs its
own spec pass — this doc is the index of what that decision activates,
not one work item.

## The four boundaries

1. **Process sandboxing.** `sandbox_wrap()` is an identity wrapper today:
   spawned runtimes and tools run with the daemon user's full ambient
   authority. Fine when the operator owns everything the process could
   touch; not fine the moment a hosted principal's item executes on shared
   hardware. Activation means choosing an isolation mechanism (namespaces,
   seccomp, microVM) per threat model — which is exactly why it cannot be
   designed ahead of one.

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
