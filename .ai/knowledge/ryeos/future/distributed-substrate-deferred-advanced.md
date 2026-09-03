<!-- ryeos:signed:2026-09-01T00:15:48Z:2edfbe049458a379e00cc796125d1ffc80c541b701746eeff26f502db156acb4:Z68m/4pTQrhlW3I7NZ9ki8JHX30IWvwOn3WagcBm/SKqndwO/mYjXPthOujGzLbwt2OwS16ePfsyXgtZ8RPgBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: distributed-substrate-deferred-advanced
title: Distributed Substrate and Hosted-Cluster Storage
entry_type: implementation_guide
version: "0.5.1"
author: amp
created_at: 2026-05-30T00:00:00Z
description: Current distributed-substrate baseline and the measured path from explicit cross-site placement through hosted-cluster storage, automatic placement, and broader federation.
tags:
  - distributed-substrate
  - federation
  - hosted-cluster
  - cloud-storage
  - remote-execution
  - durable-jobs
  - future-work
```

# Distributed Substrate and Hosted-Cluster Storage

## Purpose

This note owns the advanced distributed implementation boundary after the
landed explicit cross-site worker-placement path. It records:

- what the current substrate already implements;
- what must be qualified before broadening it;
- how a hosted RyeOS cluster may make durable state location-independent while
  keeping local realizations disposable;
- where automatic placement belongs; and
- why hosted clustering and federation remain different trust models.

The cross-horizon ordering is owned by
`knowledge:ryeos/future/substrate-growth-roadmap`. Distribution starts from
the exact capsule, effect, evidence, project-authority, and chain-lineage model.
It transports and re-admits those objects and consequences; it does not replace
them with a looser distributed identity.

This document defines dependencies and triggers inside the distributed and
hosted-storage branch. It does not override the global pull-forward order in the
substrate growth roadmap. In particular, a mechanism described here does not
become the next RyeOS implementation merely because its prerequisites exist.

The target property is:

> An execution is not permanently tied to one machine. An admitted,
> capability-matching site may become its next placement only after the exact
> closure is available and continuation plus chain-writer authority have been
> explicitly transferred. Exactly one placement may extend the frontier.

This is not a design in which any process that can see a head races to claim
it. Storage visibility, verification, admission, placement, and continuation
authority remain separate facts.

## Current landed baseline

The former version of this note described several substrate foundations as
future work. They are now implemented for the bounded explicit-placement path.
This is a boundary summary, not the owner of their current contracts:

- bounded object-closure description and transfer with response, entry, link,
  and total-byte limits;
- complete-closure validation and fail-closed handling of unsupported or
  missing content;
- durable staged imports whose roots survive interruption until publication or
  recovery cleanup;
- CAS entry attribution including source principal, source peer, durable job,
  byte count, and local/staged/accepted/mirrored/rejected state;
- typed verified handler context for authenticated distributed services;
- durable sync jobs and attempts with restart reconciliation and retryable
  phases;
- generic signed heads and admission heads;
- bounded head discovery and admitted-head mirroring;
- hosted structured-worker private environments and portable checkpoints;
- placement-transfer manifests, remote admission, target adoption, and
  recoverable source/target durable handoff jobs;
- a stable `chain_root_id`, a distinct successor `placement_thread_id`, worker
  boot-epoch fencing, and explicit source/target chain-writer transition
  evidence; and
- recovery checks that allow a chain to return to a node with an older valid
  mirror while rejecting forks, rollback, and unrelated local advancement.

This remains a narrow substrate. It is not yet:

- a general placement scheduler;
- an object-store-authoritative hosted cluster;
- a hostile multi-tenant cloud boundary;
- a public federation fabric;
- a distributed retention and repair policy; or
- an independently complete verification/certification export.

## Deferred activation gates

This document does not own the concrete next implementation campaign. Deferred
mechanisms below activate only through the triggers in the substrate growth
roadmap and the branch-specific gates at the end of this note.

### Verification-only export dependency

The read-only export/profile is owned by
`knowledge:ryeos/future/portable-execution-graph-advanced-path`. Its portable
attestation claim depends on the signer succession and historical-validation
design owned by `knowledge:ryeos/future/key-lifecycle`.

A future bounded export contains the exact launch capsule, program and project
closure, chain history, checkpoints, effects, receipts, artifacts, accounting
and publication evidence, retained signer evidence, and a signed head-through-
sequence statement. It explicitly declares excluded secret, vault, host-path,
and node-private material.

The acceptance property is:

> A clean node with no prior RyeOS state can authenticate and verify the
> execution through the attested head, detect any omitted required object, and
> receive no authority to continue it.

Before key succession is defined, an initial export may prove only a narrower
same-era claim under explicitly pinned trust roots. It must label that
limitation and must not claim durable authentication across signer rotation,
revocation, or succession.

### Measurement gate

No deferred transport packing, checkpoint, cache, persistent-workspace,
hosted-storage, or automatic-placement mechanism activates from intuition
alone. Its proposal must carry retained, workload-identified measurements and
a named operational profile showing which bound or requirement the current
explicit path fails to meet. Remote content-addressed graph traversal must not
be allowed to degrade into one network round trip per edge.

## Hosted-cluster storage direction

### Truth and realization model

A future hosted cluster may use this tiering:

```text
durable immutable CAS entries + linearizable signed heads
                         |
                         v
       verified local CAS and realization caches
                         |
                         v
        materialized projects, checkpoints, workers
```

The durable store owns location-independent persistence. Local NVMe remains the
fast path for graph traversal and materialization. Local CAS entries,
workspaces, indexes, and processes may be discarded and reconstructed without
changing execution identity.

The required operating principle is:

> Always correct when degraded; fast when healthy.

Cache placement, notifications, head gossip, and prefetch are performance
hints. Before claiming that a mutable namespace view is current, or before
acquiring authority to extend a mutable frontier, a node performs an
authoritative linearizable head read. It then serves or acts on the exact
hash-bound closure returned by that read. A historical read or realization
pinned to an exact admitted hash remains valid without rebinding to the latest
project or chain head. A lost notification may cause a slower conditional
read; it must not weaken these semantics.

### Hosted cluster is not federation

Keep these deployment shapes distinct:

```text
hosted cluster
  shared deployment authority and storage policy
  interchangeable cache/compute nodes
  possible shared durable object store

federation
  independently authoritative sites and policies
  signed closure exchange
  staging, verification, and re-admission
  explicit continuation and chain-writer transfer
```

One hosted cluster may use a shared durable backend. Independent federated
sites must not silently acquire ambient write authority merely because they can
read the same object store. Federation retains peer identity, admission,
audience, delegation, and source/target transition evidence.

### Durable storage semantic contract

Specify and test semantics before introducing a broad storage abstraction. A
candidate hosted backend must provide the observable equivalent of:

```text
immutable_put(kind, hash, bytes)
  create once, or verify the exact bytes already stored

immutable_get(kind, hash)
  return the exact addressed bytes or absence

head_get(namespace, name)
  return a signed RyeOS ref plus an opaque backend version token

publication_begin(namespace, name, expected_target, proposed_target, roots)
  durably register a bounded, renewable publication intent and root set

head_compare_and_swap(
  namespace,
  name,
  expected_backend_version,
  expected_target,
  admitted_signed_transition
)
  validate and atomically publish the exact authorized successor, or conflict

head_conditional_get(namespace, name, known_version)
  report unchanged, or return the newest signed ref and version

publication_settle(intent, outcome)
  idempotently complete or abort the publication and release/quarantine roots
```

Contract rules:

1. List operations are never correctness or reachability authority.
2. Object and blob are distinct logical namespaces even if a backend shares
   their physical layout; conformance must reject kind confusion.
3. Backend version tokens are concurrency facts, not RyeOS identity or trust.
4. Backend CAS provides atomic serialization. RyeOS namespace policy decides
   whether a proposed transition is an authorized logical successor.
5. Every published ref binds its namespace, name, target, signer, and logical
   generation. The admitted transition binds the expected predecessor and the
   exact authority/audience required by that namespace.
6. General trust-store membership, a valid signature, or backend write
   credentials grant neither namespace mutation nor chain-writer authority.
7. Initial creation uses an explicit expected-absent state. Removal publishes a
   signed tombstone/generation transition; raw deletion and resurrection of an
   older signed ref are forbidden.
8. Before backend CAS, publication verifies namespace-scoped signer/admission
   authority, predecessor or ancestry rules, monotonic generation/fencing, and
   the complete durable closure.
9. A head must not publish before its complete required closure is durable and
   protected by a discoverable publication intent/root.
10. Storage access does not imply trust admission or continuation authority.
11. A linearizable read returns a definite `(signed head, backend version)`;
    the reader serves the closure bound to that head. A later head advance does
    not retroactively make that pinned read inconsistent.
12. A mutation or continuation commit conditions on the observed backend
    version, expected logical target, admitted transition, and current fencing
    authority.
13. A failed or ambiguously acknowledged compare-and-swap never manufactures a
    merge, retry identity, or replacement execution attempt. Recovery rereads
    and verifies the authoritative head before deciding whether to retry.
14. Local and hosted implementations must pass one publication, conflict,
    replay, rollback, corruption, ambiguous-response, and recovery conformance
    suite.

Publication intent and root registry requirements:

- intent creation is durable before any uploaded entry can become a GC
  candidate;
- the registry is strongly enumerable without treating bucket/filesystem list
  results as authority;
- an intent binds namespace, head name, expected target, proposed target,
  bounded root set, owner/job identity, creation time, expiry, and renewal
  generation;
- recovery idempotently completes or aborts an interrupted intent;
- expired intents enter quarantine for a declared grace period rather than
  becoming immediately deletable; and
- settlement occurs only after authoritative head outcome is known.

The backend capability profile replaces a generic durability promise. It must
declare and test:

- atomic create-if-absent for immutable entries;
- strong GET behavior after successful immutable publication;
- per-head-key linearizable conditional update behavior;
- multipart visibility and incomplete-upload behavior;
- acknowledged-write survival and replication failure domain;
- configured RPO and RTO;
- ambiguous response recovery; and
- the absence of assumed cross-key transactions or ordering.

Each publication records the durability profile it actually achieved. It must
not inherit a stronger label merely because a backend can be configured for
one elsewhere.

Do not introduce a generic `CasStore` trait solely in anticipation of this
backend. First prove the contract against the filesystem implementation and one
concrete experimental hosted implementation; extract the shared interface from
observed common semantics.

### Phase A: non-authoritative durable mirror

The first object-store integration is disaster-recovery shaped:

```text
authoritative local CAS and signed heads
              |
              v durable sync job
      non-authoritative hosted mirror
              |
              v
   clean-node restore and verification
```

Start with completed verification exports after their trust/succession claim is
defined. The mirror stores a signed, content-addressed mirror manifest naming
the exact export roots and mirror cut; restoration never discovers authority by
listing backend entries. Do not begin with active execution frontiers, vault
material, credentials, or node-local policy.

Pull this phase forward for a concrete hosted disaster-recovery requirement
with a named failure domain and target RPO/RTO, not merely because an object
store is available.

The mirror phase must prove:

- interrupted upload resumes or retries without changing object identity;
- local loss can be restored on a clean node;
- corrupt, missing, or substituted mirror content is detected;
- restored closures and signed heads match byte-for-byte;
- the signed mirror manifest, rather than backend listing, supplies the exact
  restore and retention roots;
- mirror roots participate explicitly in retention policy;
- mirror lag and last durable head are observable; and
- no local write is acknowledged under a stronger mirror guarantee than was
  actually established.

The phase acceptance artifact records the tested failure domain, fault cases,
repetition count, achieved RPO/RTO, largest verified restore, and any excluded
content. Every declared fault case must pass before the mirror is described as
a recovery mechanism.

### Phase B: one object-store-authoritative namespace

After the mirror meets its declared recovery profile, invert authority for the
completed verification-export catalog/head only. A different first namespace
requires an explicit design amendment rather than opportunistic reuse of the
pilot.

```text
publish complete immutable closure to durable store
              |
              v
compare-and-swap the signed authoritative head
              |
              v
populate or update local CAS and query projections
```

The actual publication sequence is:

1. durably register the publication intent and bounded root set;
2. publish typed immutable entries under that intent;
3. verify the complete proposed closure from the hosted backend;
4. validate the namespace-authorized logical successor transition;
5. compare-and-swap the exact expected backend version and logical target;
6. reread after an ambiguous response and determine the authoritative result;
7. settle the intent as completed or aborted; and
8. retain aborted/expired roots through the declared quarantine period.

The pilot must prove:

- two writes against the same observed predecessor and backend version yield at
  most one successful transition; retry requires reread, revalidation, and a
  newly authorized successor;
- acknowledged content survives loss of the writing node and its local disk;
- an authoritative linearizable read after publication returns the published
  head or a valid later successor;
- a node with no local content reconstructs the complete closure;
- local corruption is repaired from durable content;
- notification delay or loss cannot weaken latest-head or mutation semantics;
- local CAS and SQLite projections may be deleted and rebuilt; and
- interrupted and ambiguously acknowledged publication is recovered
  idempotently from its durable intent.

Deletion and reclamation are disabled during this pilot. Bounded safe leakage
is preferable to claiming distributed GC before its root registry, epochs,
reader/materialization pins, grace periods, quarantine, unavailable-peer
policy, and final reachability recheck are implemented and qualified.

Only after this succeeds should active chain heads be considered.

### Cross-cutting: closure bundles and disposable realizations

Hosted storage must not force a serial object-store request for every event or
graph edge. Introduce measured, content-addressed realization units such as:

- bounded closure archives with exact entry indexes;
- segmented thread-event histories;
- periodic execution checkpoints;
- project-tree shards when flat trees become a measured cost; and
- compacted cold-history archives with retained logical roots.

These are transport and realization projections of the canonical object graph,
not alternative identity systems. Every unpacked entry is verified against its
RyeOS address and schema contract.

This work is measurement-triggered rather than inherently later than the
authoritative pilot. Pull the minimum bounded bundle/batch/index mechanism
before Phase B if an empty-node restore would otherwise require serial remote
round trips or miss its declared RTO.

Compaction should be performed once for a generation and published as a
content-addressed result. Other cache nodes download and verify that result
rather than independently repeating expensive work. Its manifest binds the
input roots, schema/version, deterministic compactor identity, output roots,
and one explicit retention class:

- **lossless** — every canonical input remains retained;
- **archive-tier** — inputs remain retrievable under a declared availability
  and recovery profile; or
- **commitment-only** — earlier detail is intentionally unavailable and only
  signed commitments/checkpoints remain.

Hashes commit to unavailable inputs but do not make them independently
inspectable or replayable. Certification or complete-history claims require
lossless inputs or a self-contained archive capable of reproducing them
byte-for-byte. A commitment-only result must state the verification and replay
claims it relinquishes.

Local cache behavior must satisfy:

- cache presence never implies admission;
- active execution, staging, reader, and retention leases prevent eviction;
- materializations bind to the exact source head and closure identity;
- corruption discards and rematerializes the cache;
- an empty local node can make progress from durable state; and
- cache topology may change without changing execution or project identity.

Persistent remote workspaces remain deferred until measurements show that
exact checkout/materialization cost dominates useful execution. A retained
workspace is still a cache with explicit identity and revocation, not the
authoritative project state.

### Distributed reclamation gate

Authoritative hosted deletion remains disabled until a separate reclamation
profile implements and qualifies:

- a strongly enumerable registry of signed heads, mirror manifests,
  publication intents, retained exports, admission roots, and active leases;
- snapshot/epoch-based marking from one declared root cut;
- object inventory used only to propose deletion candidates, never to decide
  reachability;
- read/materialization pins or epochs covering the maximum unpinned operation
  lifetime;
- a minimum grace period longer than that lifetime;
- quarantine or versioned deletion before irreversible purge;
- idempotent retry and crash recovery for every sweep phase;
- explicit treatment of unavailable federation peers and lagging mirrors; and
- a final authoritative reachability and generation recheck immediately before
  purge.

Per-object reader leases are not required when a read pins an exact head/closure
under a qualified epoch. The design must state which mechanism protects each
read and materialization class. Until this gate passes, safe leakage is the
defined behavior.

## Independent track: automatic placement

Pull automatic selection forward only when explicit target choice is an
observed operational bottleneck. It does not depend on object-store-authoritative
storage: today's bounded transfer and explicit handoff may remain its data path.
Conversely, hosted disaster recovery may be needed without automatic
placement. The two tracks share qualification, measurement, and fencing
requirements but have independent triggers.

Use **placement-operation lease** for the scheduler's selection/failover fact.
This is distinct from a recurring schedule-owner lease owned by
`knowledge:ryeos/future/scheduler-deferred-advanced-work` and from a managed
runtime invocation lease. None of those leases is chain-writer authority.

The implementation order is:

1. signed node inventory and capability/policy publication;
2. placement requirements derived from the admitted launch capsule;
3. selection among eligible and trusted sites;
4. a durable placement-operation lease with owner identity, sequence, expiry,
   and fencing for that placement operation only;
5. exact target pre-admission and resource reservation;
6. closure/checkpoint availability or materialization;
7. a source cut that terminalizes the old placement and creates the exact
   successor `placement_thread_id` plus one-successor chain-writer grant;
8. target adoption that verifies the grant and atomically publishes the exact
   transferred successor head;
9. worker attachment under a fresh boot epoch; and
10. source settlement and cleanup after its authority has already ended.

Placement scoring may prefer a node that already retains the required closure,
model, or workspace cache, but locality is considered only after authority,
policy, isolation, resource, and capability eligibility:

```text
admitted and trusted
  -> satisfies isolation and resource requirements
  -> satisfies program/model/tool capabilities
  -> prefer useful verified cache locality
```

"Any healthy worker" therefore means any admitted, capability-matching worker
selected by a fenced placement operation and subsequently attached under the
exact transferred launch and chain authority. It does not mean any healthy
process may acquire a frontier by observing shared storage.

The placement-operation lease design must define owner identity, acquisition
and renewal, expiry, failover, clock assumptions, stale-owner fencing,
split-brain protection, idempotent recovery, and operator-visible ownership.
It selects and fences orchestration; it never grants launch, effect,
credential, accounting, or chain-write authority.

The current chain-writer grant/placement generation remains the authoritative
fence for frontier extension. Every chain append and authoritative publication
must reject stale writer authority. An external effect class is eligible for
automatic takeover only when its commit boundary can reject stale authority or
is mediated through a fenced durable broker/outbox. The source becomes unable
to commit at the one-successor transition; the final cleanup step is not the
correctness-critical revocation.

A scheduler database or inventory projection may be useful, but it must be
rebuildable and must not become execution identity or grant continuation
authority by itself.

Do not replicate an active process. Move or restart the interpreter from the
durable record under one authoritative placement.

## Broader federation boundary

Federation adds independent-site concerns that a shared hosted cluster does not
solve:

- principal and node key succession, revocation, and delegation;
- audience binding and replay protection;
- public or third-party admission policy;
- independently administered storage and retention policy;
- mirrored signed heads and repair without ambient shared write authority;
- per-principal quotas, audit, cleanup, workspaces, caches, secrets, and egress;
- threat-model-selected outer containment for unrelated or hostile tenants;
- disclosure-scoped export and certification; and
- distributed GC that respects admission, retained evidence, leases, mirrors,
  and unavailable peers.

Federation may place specialized children independently:

```text
implementation placement
  |-- build children on high-core nodes
  |-- inference children on model-capable nodes
  |-- training children on accelerator nodes
  |-- candidate probes on clean qualification nodes
  `-- review children on independently admitted placements
```

Children retain their own chain identities. A parent placement does not
silently move them, and shared campaign membership does not grant authority to
extend another chain.

## Hosted execution isolation boundary

Signed admission proves who requested work and what closure was admitted. It
does not make hostile code safe to co-locate with another principal.

The current optional node-owned execution boundary remains the inner typed
launch boundary. A hostile-workload hosted scheduler must additionally provide,
per principal or job:

- CPU, memory, process-count, and eventually I/O cgroup limits;
- authoritative whole-workload teardown across descendant process groups and
  sessions;
- a dedicated worker, user, VM, or microVM selected by the threat model;
- bounded event capture and private spooling that guest output cannot use to
  exhaust the daemon;
- principal-scoped workspace, cache, storage, secret, and egress authority;
- durable outer-worker identity, audit, cancellation, cleanup, and retry; and
- an admitted immutable image/snapshot or complete closure policy.

The complete boundary and activation trigger remain owned by
`knowledge:ryeos/future/hosted-node-trust-boundaries`.

## Bundle distribution

Do not create an unrelated bundle replication substrate. A bundle should move
as another signed object graph through bounded closure transfer, staging,
admission, signed heads, durable jobs, retention, and repair. Existing bundle
export/install remains operational tooling until that common path is ready.

## Keep deferred until triggered

### Chunked and resumable entry transfer

Trigger when blobs exceed practical request limits, unreliable links cause
measured failures, or the selected hosted backend requires multipart/resumable
transfer to meet the named transfer profile. Preserve complete-entry hash
verification and staged publication.

### Persistent remote workspaces

Trigger when retained measurements show checkout/materialization dominates
the workload profile's declared latency or cost budget. The activating design
must name that threshold and the measurements that crossed it. A persistent
workspace needs an exact source-generation binding, lease, principal scope,
cleanup, and drift policy.

### mTLS and TLS pinning

Trigger for compliance or deployment transport policy. Ed25519 request and
object identity remains authoritative; transport certificates do not replace
signed principals.

### Request-scoped trust overlays

Trigger when CI-style signer churn makes persistent trust pins operationally
unworkable. Request-scoped trust must be explicit admission data, not an
unsigned transport hint.

### Per-principal vault partitioning and remote sealing

Trigger with real multi-tenant secret hosting or a requirement that the remote
service never observe plaintext. This changes the vault and deployment trust
boundary and is not implied by object graph federation.

### Registry namespace claims

Trigger when multi-publisher distribution requires formal namespace ownership.
This belongs to registry and admission policy, not closure transport.

### Cache placement and notification mechanisms

Rendezvous hashing, queue notifications, gossip, or similar mechanisms are
optional fast paths after multiple cache nodes exist. No particular algorithm
or transport is part of the correctness contract. Every design must tolerate
lost, delayed, duplicated, and reordered notifications.

## Non-goals and guardrails

- Do not make hosted object storage mandatory for standalone or offline RyeOS
  nodes. Local filesystem authority remains a supported deployment mode.
- Do not treat a backend ETag, database row, cache location, host path, PID, or
  worker boot epoch as portable execution identity.
- Do not make list consistency, notification delivery, or cache routing part of
  correctness.
- Do not grant continuation authority from hashes or verification alone.
- Do not introduce three-phase commit or global cross-namespace transactions
  where immutable publication followed by one fenced head transition suffices.
- Do not manufacture merges or replacement executions after head conflicts.
- Do not allow two placements to extend one chain frontier.
- Do not let a successful implementation or candidate worker publish, install,
  or activate itself; promotion remains an explicit authority boundary.
- Do not build a separate model, worker, checkpoint, Git, or federation
  substrate to solve hosted storage.

## Intra-branch dependencies and triggers

This section is subordinate to the global sequence in
`knowledge:ryeos/future/substrate-growth-roadmap`. It answers what must precede
what *after that roadmap activates a consumer in this branch*.

Hosted-storage track, activated by a named hosted durability/recovery need:

1. Require retained reconstruction measurements and a concrete hosted
   durability/recovery profile.
2. Complete key-lifecycle succession design for any cross-generation portable
   authentication claim, then complete disclosure-scoped verification export
   before using that export as a mirror or certification root.
3. Define the backend capability, publication-intent, transition-authority,
   and no-deletion pilot profiles.
4. Add the non-authoritative completed-export mirror and prove clean-node
   recovery against its RPO/RTO profile.
5. Pull forward the minimum measured closure bundling/checkpointing needed to
   meet restore bounds.
6. Pilot the completed-export catalog/head as the one authoritative namespace.
7. Qualify publication conflict, ambiguous response, replay/rollback,
   corruption, and projection-rebuild behavior.
8. Design and qualify distributed reclamation before enabling deletion.
9. Consider active chain heads only after publication, freshness, repair,
   retention, fencing, and takeover semantics are proven.

Automatic-placement track, activated when explicit target choice is limiting:

1. Require the existing explicit one-successor handoff and its recovery path to
   be release-qualified under the applicable workload profile.
2. Require retained placement measurements showing explicit selection is the
   limiting operation.
3. Publish signed node inventory and placement requirements.
4. Add the placement-operation lease and selection policy without treating it
   as launch or chain authority.
5. Reuse the qualified exact one-successor handoff for every selected target.
6. Add cache locality only as a preference after authority and capability
   eligibility.

Federation and hosted-multitenancy track, activated when independent principals
or sites create the need:

1. Complete the hosted-principal isolation boundary and applicable key
   lifecycle, delegation, audience, and replay contracts.
2. Add distributed retention, repair, unavailable-peer, quota, and audit
   policy.
3. Add public/third-party admission only after those boundaries are explicit.

| Observed need | Pull forward |
|---|---|
| Explicit handoff has an unqualified failure cut | Handoff fault profile and recovery evidence |
| Cross-site reconstruction misses a declared bound | Measured closure bundling, checkpointing, or cache work |
| A hosted deployment requires recoverability from local-disk/node loss | Non-authoritative mirror with named RPO/RTO |
| Mirror recovery meets its profile and shared storage authority has measured value | One completed-export authoritative namespace |
| Explicit site choice is operationally limiting | Inventory-driven placement-operation leases |
| Hosted storage requires reclamation rather than bounded safe leakage | Distributed reclamation profile |
| Unrelated principals share infrastructure | Hosted isolation and principal partitioning |
| Authority/content moves among independent administrations | Broader federation, key lifecycle, retention, and repair |

Landing a mechanism is not evidence that its broadest consumer should be built
next. Correct explicit handoff, complete evidence, and measured reconstruction
remain the gates for every later layer.
