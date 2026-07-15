<!-- ryeos:signed:2026-07-14T10:12:37Z:25cfbf6f5e862669a92315504d9cf5f2a64bc2238072dd8e63896964c79fe6f1:K0aGS96nCkegloTp8otnafOhJMlBeWKnFalfjJgLx58ZxvJel17YGcbpOok+nRlqb7rXSV9ty/pwi7OFeKz6BA==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
```yaml
category: ryeos/future
name: distributed-substrate-deferred-advanced
title: Distributed Substrate Deferred Advanced Implementation
entry_type: implementation_guide
version: "0.4.0"
author: amp
created_at: 2026-05-30T00:00:00Z
description: Future implementation notes intentionally left out of the immediate distributed substrate hardening path, with triggers for when to pull them forward.
tags:
  - distributed-substrate
  - federation
  - cloud-nodes
  - remote-execution
  - durable-jobs
  - future-work
```

# Distributed Substrate Deferred Advanced Implementation

## Purpose

This note records the advanced implementation work that should stay out of the immediate substrate hardening pass, while preserving the conditions that should pull each item forward.

The current implementation path is:

```text
bounded closure transfer
  → staged imports
  → durable jobs
  → generic signed heads
  → admission
  → federation
  → cloud/cluster storage
```

The items below are not rejected. They are deferred until they support that sequence rather than distracting from it.

## Pull forward soon as substrate foundations

### Durable sync / remote jobs

Pull forward early, but as a generic job spine rather than as old remote-execute polling.

Durable job operation types should cover:

- closure push;
- closure pull;
- admission submit;
- remote execute;
- mirror peer head;
- follow namespace;
- federation repair.

Each job should record:

- job id;
- operation_type;
- peer / remote;
- caller principal;
- root hashes or head refs;
- base head / expected head when relevant;
- uploaded / fetched hashes;
- current phase;
- retry metadata;
- limits used;
- result hash or attestation hash;
- failure reason.

Durable jobs are the bridge from synchronous operator commands to cloud/federation operation. They unlock retry, crash recovery, observability, async remote execution, and cluster repair.

### Hosted execution isolation handoff

Pull forward the handoff contract when remote jobs begin executing code for a
principal outside the node owner's trust boundary. Signed admission establishes
who requested work and what object closure was admitted; it does not by itself
make that code safe to co-locate with other principals.

The current optional Linux Bubblewrap boundary is one inner execution backend
and the integration seam for this future work. It must first be expressed as a
typed backend-neutral isolation plan rather than promoted into the whole hosted
architecture. A hostile-workload scheduler must add, per principal or job:

- CPU, memory, and process-count cgroup limits, accounting, and authoritative
  whole-workload teardown across descendant process groups and sessions;
- a VM, microVM, or dedicated outer worker selected by the deployment threat
  model;
- preservation of the current bounded stdout/stderr supervision, plus bounded
  event capture and optional private spooling that cannot be exhausted by
  guest output rate;
- principal-scoped workspace, cache, secret, and egress authority;
- durable worker identity, audit, cancellation, cleanup, and retry semantics;
  and
- an admitted immutable image/snapshot or closure policy when live read-only
  transitive imports and assets are not acceptable.

Host PIDs and same-UID signals are not isolated by the current inner sandbox.
The outer worker design must remove that shared authority rather than claiming
Bubblewrap alone solved it. Durable job records should therefore carry an
explicit isolation class and worker/cgroup identity once this path is activated.
That outer identity also closes the spawn-to-durable-attachment crash window:
if a node dies after creating a process but before committing its exact birth
tuple, the worker/cgroup remains an independently nameable teardown boundary.
The backend, capability matching, and multi-platform contract is specified in
`ryeos/future/data-driven-execution-isolation-backends`.

### CAS attribution and staging metadata

Pull forward early in a lightweight projection form.

The system needs to know why an object is present before it can safely operate as a cloud or federation node.

Suggested projection:

```text
cas_entries
  hash
  entry_kind: object | blob
  bytes
  first_seen_at
  source_principal
  source_peer
  job_id
  state: local | staged | accepted | mirrored | rejected
```

This is not a billing system yet. It is the minimal accounting needed for:

- staged imports;
- per-principal quotas later;
- GC decisions;
- debugging object provenance;
- rejecting unadmitted network content;
- distinguishing local content from peer content.

### Typed handler context

Pull forward before many more distributed handlers are added.

Object closure, admission, head listing, and federation APIs must be principal-aware. Passing caller identity through loose JSON fields or ad hoc scope injection will become a bug source.

Handlers should receive typed context containing at least:

```rust
HandlerContext {
    principal_id,
    fingerprint,
    scopes,
    verified,
    audience,
}
```

This supports:

- ACL-filtered closure fetches;
- head namespace authorization;
- admission policy;
- per-principal staging and quota;
- cloud multi-tenancy;
- audit logging.

### Bundle sync through closure transfer

Do not build a separate advanced bundle replication system yet.

Instead, keep the design constraint:

> A bundle should become another signed object graph with heads and admission, not a special remote-copy path.

Existing bundle export/install can remain operational tooling, but federation should eventually move bundles through the same object-closure, staging, admission, and signed-head substrate as everything else.

## Keep deferred until triggered

### Chunked object transfer

Defer until large blobs or unreliable links make whole-entry transfer fail in practice.

Trigger:

- blobs regularly exceed practical request/response limits;
- operators report push/pull failures on unstable links;
- cloud storage requires resumable upload/download.

When triggered, add chunked CAS blob upload/download with resume. Do not let chunking complicate the initial closure/admission semantics.

### mTLS / TLS pinning

Defer until compliance, persistent TOFU failures, or deployment policy requires transport-level certificate identity.

RyeOS identity remains Ed25519 request signing. mTLS may harden transport, but it should not replace signed principal identity.

### Request-scoped trust overlays

Defer until CI-style signing-key churn makes persistent trust pinning painful.

This requires trust policy to become request-scoped instead of only boot-time or state-store scoped. It is useful, but premature before admission and namespace policy exist.

### Per-principal vault partitioning

Defer until real multi-tenant secret hosting or per-principal secret rotation becomes a product requirement.

This changes the vault trust boundary and on-disk layout. It is not needed for object graph federation unless remote jobs start storing user secrets on shared nodes.

### Remote seal / vault public key API

Defer until the server must never see plaintext secrets.

When triggered, `/public-key` should expose the vault public key again, `RemoteClient` should seal client-side, and vault handlers should accept pre-sealed blobs.

### Persistent remote workspaces

Defer until checkout/materialization cost dominates remote execution runtime.

This is an execution-performance feature, not a substrate prerequisite. It should come after durable jobs can safely track long-lived workspace state.

### Registry with namespace claims

Defer until multi-publisher bundle distribution requires formal namespace ownership.

This belongs to the registry/admission layer, not the immediate closure-transfer layer.

## Explicitly superseded old position

Older remote-execution notes treated daemon-to-daemon forwarding as architecturally excluded. That decision has been superseded.

Long-term RyeOS direction includes:

- daemon-to-daemon object graph transfer;
- remote execution;
- cloud nodes;
- node admission;
- cluster federation;
- mirrored signed heads;
- durable sync jobs.

The correct constraint is not "no daemon-to-daemon." The constraint is:

> Daemon-to-daemon behavior must be signed, bounded, staged, policy-admitted, observable, and recoverable.

## Immediate implementation boundary

The next implementation pass should focus on hardening the committed substrate slice:

1. object and response byte caps for closure transfer;
2. per-object read and link caps in closure traversal;
3. incomplete closure rejection for `objects/closure/get` by default;
4. array-only roots, no scalar compatibility;
5. remote client transfer options and response validation;
6. reachability using verified/signed refs where appropriate.

After that, pull forward:

1. durable jobs;
2. staged import / CAS attribution;
3. typed handler context;
4. generic signed heads.
