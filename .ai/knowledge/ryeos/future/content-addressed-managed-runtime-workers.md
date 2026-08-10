<!-- ryeos:signed:2026-08-10T03:16:08Z:50835abfef5d066fb79574bf8a30db4fd659c54634a69d8f21977c21f4caacf5:n1SYxz7aeG08WSVEEnsGCaDrV37D3m9RJSW8aj2bzx4johwyFV8/mumcU+er/WNrRtD0kAcfhzxzjQiVY8JvAQ==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: ryeos/future
name: content-addressed-managed-runtime-workers
title: Content-Addressed Managed Runtime Workers
description: Deferred leased-invocation class for the existing worker kind, reusing warm managed runtimes without reusing invocation authority, secrets, accounting, or recovery state
entry_type: design
version: "0.2.0"
```

# Content-Addressed Managed Runtime Workers

## Status

This is the deferred **leased-invocation worker class**, not a proposal for a
second RyeOS kind. The generic signed `worker` kind, exact external-content
realizations, admitted persistent-session capsules, daemon persistent-session
pool, target-channel isolation, cancellation, and the fixed local-provider
worker have landed. Their current operating contract is documented at
`knowledge:ryeos/core/execution/local-model-workers`.

The current worker is deliberately narrower than this design: its executable
closure, realizations, provider role, isolation ceiling, and session protocol
are fixed before boot. It accepts bounded local-provider requests but never
receives a general RyeOS invocation capability, callback token, project handle,
secret set, or mutable resolver. That is a complete and useful worker class,
not a temporary workaround.

This note owns the remaining step: admitting a separately authorized RyeOS
invocation into an already warm worker vessel. Pull it forward only when
measurements still show material time between durable planning and provider
request submission. Progressive provider output must also use bounded ordered
callback batching first; a warm worker cannot fix per-delta daemon round-trip
backpressure inside an already-running provider call.

The first leased implementation is deliberately narrower than a general
distributed worker system:

- local-node managed runtimes only;
- directive runtime first;
- one active invocation per worker;
- exact content-addressed worker classes;
- no cross-authority fallback;
- no persistent invocation secrets, conversation state, or spend authority;
- Lillux owns process creation, isolation, attachment, and teardown.

Distributed placement, hostile multi-tenant outer isolation, and remote worker
leases remain separate work.

### Measurement checkpoint: 2 August 2026

A controlled chat-latency profile measured warm daemon launch to runtime-ready
at about 0.23 seconds. One first provider call recorded about 25 ms of DNS and
68 ms of aggregate connection establishment; later provider calls in the same
invocation recorded neither, confirming existing connection reuse. Multi-round
provider/tool execution remained the dominant latency term.

Those measurements do not pass this document's pull-forward gate. Workers
remain a valid cold-tail and throughput design, but they are not the next fix
for current multi-second first text or long serial tool loops. Re-evaluate with
a distribution, not a single sample, after signed workflow bounds and provider
reasoning/model policy have been benchmarked. See
`knowledge:ryeos/future/chat-latency-investigation` for the measurement model.

This document's future leased worker is a reusable trusted managed-runtime
process inside one RyeOS node. It is the same item kind as the fixed local
worker and a different admitted lifecycle/protocol class. It is not the
hostile-workload outer worker/VM described by
`knowledge:ryeos/future/hosted-node-trust-boundaries`. A hosted deployment may
eventually place these runtime workers inside that stronger boundary, but the
two lifecycles and threat models must not be conflated.

## Decision

**Kind decision:** both fixed persistent sessions and future leased managed
runtimes are `worker` items. A kind names the mechanical execution vessel and
its signed compilation contract. Lease capability is protocol/config data and
admitted authority, not a consumer-specific kind. The leased class extends the
shared process/session mechanics without widening or replacing the fixed class.

A warm worker is an **execution vessel**, not an admitted execution and not an
invocation-authority holder. It retains only its boot-time isolation/domain
ceiling while ready.

Every invocation still receives its own normal RyeOS admission, durable thread
birth, admitted launch capsule, accounting authority, cancellation identity,
callback authority, and terminal settlement. The only reused material is the
exact runtime substrate whose content class and isolation-domain identity match
the invocation's admitted worker-pool key.

This applies the RyeOS thesis at two levels:

> A worker class is a content-addressed object. A pool partition binds that
> class to one authority domain. An invocation lease is a separately admitted
> object that temporarily extends one matching worker.

The worker must never turn "this process was trusted once" into "anything sent
to this process may execute."

## Why leased workers may exist

Ordinary managed-runtime invocation still starts a fresh process for every root
invocation. The fixed local-provider worker is an explicit exception under its
own narrow persistent-session contract; it does not accelerate ordinary
directive execution. Fresh processes give a strong, simple authority boundary,
but ordinary invocations repeatedly pay for:

- process creation and isolation setup;
- runtime bootstrap and static configuration parsing;
- allocator and async-runtime initialization;
- DNS resolver initialization;
- TCP, proxy, TLS, and HTTP/2 connection establishment;
- provider adapter and stable prompt/tool-schema preparation.

Within one directive invocation the current `reqwest::Client` already reuses
connections across provider turns. Workers primarily improve the first provider
call of later invocations; they do not remove model inference time or the second
provider turn after tool execution.

## Pull-forward gate

Do not implement workers from total chat latency alone. Use the existing daemon
and child timing records. Pull this design forward only when, after the
pre-worker work, both conditions are true:

1. at least one RyeOS-owned cost remains material:
   - warm `execution_planning` to `stream_started` p50 is above 1 second;
   - child process entry to provider request submission is above 500 ms; or
   - first-call DNS/connection establishment is a repeatable material part of
     time to first provider event; and
2. the measured residual is not explained by provider inference or downstream
   context construction.

If warm local admission and runtime handoff are already subsecond, provider and
workflow changes have better leverage than a worker pool. Likewise, high
`progressive_callback_total_us` belongs to the runtime callback path and must
not be counted as evidence for process reuse.

## Non-goals

The first implementation does not:

- cache LLM answers;
- keep a conversation authoritative in worker memory;
- let workers resolve mutable item refs;
- let workers mint or widen capabilities;
- let workers own the provider-spend ledger;
- reuse a worker across unrelated project/principal isolation domains;
- multiplex multiple jobs inside one process;
- silently fall back from an admitted pooled launch to a different cold
  execution contract;
- treat a worker heartbeat as durable execution state;
- add remote scheduling or cross-node leases.

## Vocabulary

### Worker class

`WorkerClass` is the complete static content identity required for runtime
reuse. Its canonical serialization produces `worker_class_hash`.

The initial class binds at least:

- managed runtime canonical ref and signed descriptor digest;
- exact verified executor-chain attestation digest, including executable blob,
  manifest, signer, host triple, bundle/trust generation, and root-ambiguity
  proof;
- exact verified boot-artifact closure for every host object the warm process
  consumes before or while declaring `ready`: interpreter/runtime executable,
  dynamic loader, shared libraries, CA material, resolver/proxy inputs, and
  every other non-invocation host dependency opened or retained for the worker
  lifetime. The closure may instead bind one immutable, verified host/image
  generation containing all of those objects. An unsealed live dependency
  makes the launch worker-ineligible; a changed generation drains the old pool
  rather than revalidating it in place;
- runtime protocol canonical ref and signed descriptor digest;
- canonical digest of the exact retained tool inventory and stable serialized
  tool schemas available inside the worker class;
- host platform and architecture;
- Lillux isolation backend and semantic policy identity;
- RyeOS daemon/runtime protocol generation;
- permitted callback protocol generation;
- runtime configuration schema and full resolved static-config dependency
  proof, including positive sources, negative precedence probes,
  project/node-local config generations, and node-policy generation.

The class contains no secret values and no invocation principal.

### Worker isolation domain

`WorkerIsolationDomain` is the authority partition within which one worker may
be reused. It binds:

- pinned project authority and exact immutable generation identity;
- acting-principal isolation identity;
- the canonical digest of the complete admitted boot-time authority closure:
  `ExecutionProjectAuthority`, final effective capabilities, environment
  bindings, `IsolationProjectAuthority`/live confinement, resolved
  `IsolationPolicy` network/environment/filesystem limits, callback-protocol
  ceiling, and every contributing node-policy generation;
- node isolation-policy generation and Lillux policy identity;
- node-local secret namespace and opaque credential generation;
- provider transport identity;
- egress/isolation policy partition;
- any hosted-tenant or outer-worker identity when those systems exist.

If transport identity depends on a client certificate, proxy credential, or
other connection-level secret, the domain binds its opaque
credential-generation identity so a connection is never reused across
credential authority.

Bearer API tokens sent as request headers remain invocation-scoped, but the
initial conservative implementation should still partition by node-local secret
namespace/credential generation. That can be relaxed only after explicit
cross-credential transport review. In particular, HTTP/2 header-compression
state may retain representations of authorization headers inside a live
connection even after the request object is dropped. A credential-generation
change therefore drains the old transport partition rather than merely
replacing the next request header.

`WorkerPoolKey` is the canonical digest of
`worker_class_hash + WorkerIsolationDomain`. The class answers "which exact
runtime substrate?" The domain answers "under whose exact isolation authority
may that substrate remain alive?"

### Worker instance

`WorkerInstance` is one Lillux-managed process admitted for exactly one
`WorkerPoolKey`. It has:

- a node-minted instance id;
- a boot epoch and random challenge;
- the `worker_class_hash` and worker isolation-domain digest;
- the attached Lillux process identity;
- a control-channel identity;
- a lifecycle state;
- no standing invocation capability.

Worker-instance state is operational projection state. It can be rebuilt by
reattestation or discarded. It is never the source of truth for an invocation.

### Worker process ownership

A persistent process cannot be attached successively to ordinary thread-owned
`pid`/`pgid` fields. The process belongs to the worker instance for its entire
lifetime.

Add a node-owned `WorkerProcessRecord` containing:

- worker instance id, pool key, boot epoch, and lifecycle generation;
- the exact Lillux process identity and attachment record;
- process/descendant lifecycle state;
- active lease id/sequence, if any;
- the daemon generation that owns reconciliation.

The thread stores only an `ActiveWorkerLeaseProjection`: thread id, lease hash,
worker instance id/epoch, and lease sequence. It does not claim ownership of
the worker PID or process group.

Process-bearing execution state must be typed, for example as
`ThreadProcess` versus `WorkerLease`, so cancellation, reconciliation, status,
and allowed-action derivation cannot accidentally interpret a shared worker
PID as thread-owned. Lillux remains the sole authority for process identity,
signals, descendants, and teardown.

### Invocation lease

`InvocationLease` is the durable, one-use authority that binds one already
admitted invocation to one matching worker instance.

It contains, or content-addresses:

- lease id and monotonic lease sequence;
- worker instance id, boot epoch, and `WorkerPoolKey`;
- thread id and launch id;
- admitted launch capsule hash;
- exact program/input/ref-binding hash;
- project authority and project-generation identity;
- acting principal and effective capability digest;
- execution and directive budget identities;
- deadline and cancellation identity;
- callback protocol/token identity;
- secret-binding names and opaque credential generations, never values;
- expected output/settlement protocol;
- retry/attempt coordinate.

The canonical lease is a node-signed, content-addressed object whose hash is
stored by the durable lease-binding transition. The daemon sends that exact
object over the boot-authenticated worker control channel, and the worker
verifies its digest and node signature. A worker accepts it only when every
class, domain, and instance field matches its boot identity and its previous
lease has settled and reset.

### Provider-attempt authority

The invocation lease does not grant an unbounded right to spend. Before every
provider request, the runtime uses the existing daemon accounting surface to
reserve one exact provider attempt. The daemon remains authoritative for:

- maximum-cost proof;
- reserve/issued/settled transitions;
- retry and ambiguity handling;
- token and money settlement;
- cancellation after issue.

Worker death cannot erase an issued provider attempt or make it safe to replay
blindly.

### Reset acknowledgement

`WorkerResetAck` is a protocol acknowledgement that the trusted runtime has:

- dropped invocation messages and rendered prompt buffers;
- dropped callback, thread-auth, and secret handles;
- closed invocation-scoped files and child handles;
- drained or cancelled invocation tasks;
- returned its tool/provider state machine to idle;
- advanced its lease sequence.

This is a correctness assertion by the signed trusted runtime, not a claim that
ordinary process memory provides a hostile-code zeroization proof. The first
pool therefore remains partitioned by project/principal/credential isolation
domain.

## Authority model

### Static material may persist

A ready worker may retain only material whose identity is included in, or
safely subordinate to, its worker class and isolation domain:

- runtime code and parsed static descriptors;
- compiled Rye expression/templates;
- provider adapter code;
- stable tool-schema serialization;
- DNS resolver and connection pools partitioned by transport identity;
- immutable prompt-prefix material identified by content digest;
- read-only immutable project material permitted by the class.

### Invocation material must not persist

The following is per-lease and must be revoked/dropped before `Ready`:

- input parameters and conversation history;
- model output and reasoning buffers;
- principal capabilities and callback tokens;
- secret values;
- tool arguments/results;
- live-input queue state;
- provider-attempt authority;
- writable project/state handles;
- thread checkpoint ownership;
- cancellation handles.

Durable conversation and continuation state stays in RyeOS thread
events/checkpoints/CAS. A worker restart must not change what can be resumed.

### Dynamic policy remains dynamic

Content addressing permits reuse of static proofs; it does not freeze current
node policy forever. Before issuing a lease, the daemon still applies:

- current signer revocation/trust gates;
- current isolation policy, allowing only equal or stricter posture;
- acting-principal authorization;
- current budget authority;
- current secret availability and authority;
- current cancellation/stop state;
- current node lifecycle/write barriers.

A class or isolation-domain mismatch starts or selects another exact pool. It
never mutates an existing worker into a different authority.

Because the OS sandbox is fixed at process birth, every invocation lease must
be a subset of the domain's full boot-time authority ceiling. Matching only
project/principal labels is insufficient. The lease validator proves subset
containment for capabilities, environment bindings, filesystem roots and
modes, callbacks, network/egress, secret namespace, and any platform-specific
isolation legs before dispatch.

## Initial eligibility boundary

The first worker-capable runtime must explicitly declare a signed worker
protocol and reset contract. Absence of that declaration means cold
single-invocation execution.

Initial eligibility should require:

- managed runtime execution;
- signed protocol support for leased multi-invocation framing;
- one job at a time;
- no unmanaged descendants after lease settlement;
- no runtime-owned durable state that exists only in process memory;
- daemon-mediated accounting and callback events;
- no per-invocation writable `state_root`, checkpoint ownership, or native
  resume authority;
- a pinned read-only project generation and stable
  principal/credential isolation partition;
- an isolation backend that can preserve the class boundary for the process
  lifetime.

Writable per-invocation COW workspaces are not worker-eligible until the worker
protocol can receive and revoke an exact per-lease workspace authority without
widening its process sandbox. Such executions remain cold. A read-only pinned
snapshot is the only initial project-bearing surface. `LiveFs` is not
content-addressed and is not initially eligible. A separately specified
immutable hosted-deployment generation may become eligible later, but there is
no undefined "stable hosted project" exception.

Initial worker leases may consume daemon-mediated durable state only through
authenticated callbacks whose effects are committed by RyeOS. Writable
state/workspace handles, checkpoint-bearing execution, and native resume remain
cold until Lillux exposes a separately specified, revocable per-lease authority
that can be granted and withdrawn without widening the worker's process
sandbox.

## Lifecycle

The explicit lifecycle is:

```text
absent
  → starting
  → attached
  → ready
  → leased
  → settling
  → resetting
  → ready

ready|leased|settling|resetting
  → draining
  → dead
```

Invalid transitions fail closed. `Ready` means the worker control channel is
live, the exact Lillux identity is attached, no lease is active, and the latest
reset sequence is acknowledged.

### Boot

1. RyeOS resolves and verifies the exact runtime/protocol/executor closure.
2. It computes `worker_class_hash`, the isolation-domain digest, and the exact
   `WorkerPoolKey`.
3. Lillux creates the target in the selected isolation class, initially held at
   the existing attachment-before-execution fence.
4. RyeOS durably records the worker instance and exact Lillux identity.
5. Lillux releases the process.
6. The runtime opens the authenticated worker control channel and answers the
   daemon challenge with its class, isolation-domain digest, instance id, boot
   epoch, protocol version, and executable identity.
7. RyeOS marks it `Ready`.

The daemon must not implement process permissions, namespace mutation, cgroup
teardown, or signal handling directly where Lillux owns that substrate.
The durable worker process record, rather than a thread row, owns the
attachment-before-execution transition and remains the reconciliation target
across sequential leases.

### Lease

1. Normal root admission produces the exact secret-free admitted launch
   capsule.
2. RyeOS computes the required worker class, isolation domain, and pool key.
3. The pool selects one exact `Ready` instance or starts that pool key and
   waits for it.
4. In one durable transition, RyeOS binds the thread, lease, worker
   instance/epoch, and attempt coordinate.
5. Only after that commit does the daemon send the invocation envelope and
   ephemeral secret handles.
6. The worker validates the lease and acknowledges acceptance.
7. RyeOS marks the invocation running and streams ordinary callback events.

There is no hidden cold fallback. If pooled execution is selected and the exact
worker cannot become ready, the launch fails with a typed worker-start or
worker-capacity error.

#### Durable lease commit model

The authoritative lease is the node-signed CAS object plus the thread's
durable lease-binding event/checkpoint. SQLite worker/pool rows are rebuildable
operational projections; they do not authorize execution.

The ordering fence is:

1. the admitted capsule and thread birth already exist durably;
2. RyeOS writes/signs the exact lease object;
3. one durable binding transition commits its lease hash, worker
   instance/epoch, sequence, thread, capsule hash, and attempt coordinate;
4. only then may the envelope be sent;
5. worker acceptance is a later idempotent durable transition;
6. running is a later idempotent durable transition after acceptance.

Crash reconciliation distinguishes:

- **bound, acceptance absent or unknown**: RyeOS cannot mechanically know
  whether an unrecorded send occurred. Query/re-attest the boot-authenticated
  worker's exact active lease hash/sequence. Resend the identical lease only
  after positive proof that the worker is idle at the immediately prior
  sequence; otherwise record the matching acceptance, or revoke/kill and apply
  exact-capsule/accounting recovery;
- **accepted, running transition missing**: reconcile the same lease
  idempotently; never mint a replacement attempt;
- **projection present without authoritative binding**: discard the projection
  and refuse execution.

The worker persists only its current accepted lease hash/sequence strongly
enough to answer reconciliation. That record does not replace RyeOS thread,
capsule, lease, or accounting authority.

### Settlement and reset

1. The runtime reports its exact terminal or continuation result.
2. RyeOS validates and durably settles the thread/accounting state using the
   existing authoritative path.
3. Callback, thread-auth, provider-attempt, secret, and workspace authorities
   are revoked.
4. The daemon sends `reset(lease_id, next_sequence)`.
5. The runtime drains invocation tasks and returns `WorkerResetAck`.
6. RyeOS marks the instance `Ready`.

Missing, contradictory, or timed-out reset acknowledgement drains and kills the
worker. It never returns to the pool optimistically.

## Cancellation and kill

- A stop before durable lease binding prevents dispatch.
- A stop after lease binding revokes the lease and sends cooperative cancel.
- The worker must stop issuing provider attempts as soon as revocation is
  observed.
- If the worker does not settle within the configured grace, Lillux tears down
  the whole worker process and descendants.
- One active job per worker ensures a hard kill cannot destroy an unrelated
  invocation.

The durable stop remains authoritative if completion races cancellation, using
the existing RyeOS terminal-dominance rules.

Cancellation and allowed-action checks resolve the thread's active lease
projection to the worker process record. A hard kill targets that Lillux-owned
worker record and terminally affects only its one active lease. Status may
report that the thread has an active worker lease, but it must not copy the
worker PID into the thread and pretend the thread owns it.

## Failure and recovery

### Worker failure

If the process or control channel dies:

1. reconcile/terminate through the Lillux-owned worker process record, mark the
   instance dead, and revoke its active lease;
2. preserve the thread's admitted capsule and provider-attempt transitions;
3. determine whether the runtime kind is restart-recoverable;
4. if recoverable, lease the exact capsule/continuation to another worker with
   the same pool key or a newly started instance;
5. if provider issue state is ambiguous, apply the existing accounting/retry
   contract rather than replaying blindly;
6. otherwise settle an honest terminal failure.

Recovery consumes the admitted capsule described by
`knowledge:ryeos/development/admitted-execution-recovery`. It never asks a
surviving worker what the invocation used to mean.

### Daemon failure

The first implementation should kill or drain all previous-daemon workers at
startup and rebuild pools. This is simpler and preserves the current daemon
generation fence.

A later implementation may adopt surviving workers only when they reattest:

- exact Lillux process identity;
- exact worker class and isolation domain;
- boot epoch and daemon challenge;
- no active unowned lease;
- current policy compatibility.

Stale process metadata is never enough.

### Upgrade

Any executable, runtime descriptor, protocol, isolation, or static-config
change produces another worker class hash. A project/principal/credential or
transport-domain change produces another pool key even when runtime content is
unchanged.

- new launches select the new class;
- old ready workers enter `Draining`;
- leased old workers finish only under their already admitted capsules;
- old workers are removed after settlement or a bounded drain deadline.

There is no cache invalidation race: changed content is a different class.

## Pool scheduler

Pool state is bounded per worker-pool key:

- minimum ready workers;
- maximum instances;
- maximum queued leases;
- idle TTL;
- start timeout;
- reset timeout;
- drain timeout;
- fair queue/priority policy.

`minimum ready workers` may prewarm only an already admitted exact pool domain.
Node capacity configuration cannot mint or infer a new project, principal,
credential, transport, or isolation domain merely to keep a process warm.

Semantic ownership is split:

- signed runtime/protocol descriptors declare worker capability and framing;
- effective signed `ryeos-runtime/execution` config declares runtime behavior
  and may request pooled execution, but cannot weaken node isolation;
- node-owned execution/isolation policy is the final eligibility and
  worker-domain authority;
- node operational configuration sets capacity and timeouts but cannot widen
  execution authority or make an ineligible runtime poolable.

Pool exhaustion is a typed, observable state. The queue must be bounded and
cancellable; it is not an unbounded delay hidden inside `stream_started`.

## Protocol shape

The current one-envelope stdin/stdout runtime protocol is insufficient for a
persistent worker. Add one signed, versioned managed-worker protocol with
bounded frames:

```text
worker_hello
worker_challenge
worker_ready
worker_reconcile_request
worker_reconcile_state

lease_offer
lease_accept | lease_refuse
lease_cancel
lease_terminal

worker_reset
worker_reset_ack
worker_drain
worker_goodbye
```

`worker_reconcile_state` reports the boot epoch, lifecycle state, last settled
sequence, and exact active lease hash/sequence if one exists. It is
challenge-bound and permits the daemon to distinguish positive
idle-at-prior-sequence proof from an accepted/active lease. Silence or
contradictory state is never treated as idle.

Invocation events may continue through the existing authenticated daemon
callback surface, but every callback must bind the active lease id/sequence as
well as the thread token. Frames have explicit byte bounds and reject unknown
fields. Protocol version is part of the worker class.

The current managed-runtime protocol injects callback/thread-auth credentials
and declared secrets through per-process environment. A persistent worker
cannot rotate process environment safely. Its boot environment therefore
contains only worker-instance authority; every invocation callback token,
thread-auth token, opaque secret handle, read-only admitted project identity,
and deadline arrives in the lease channel and is held only in that lease's
state. The initial protocol carries no writable project/state handle. The
worker protocol must not emulate environment rotation with global process
variables.

## Observability

Record without secrets or prompt content:

- worker class hash, isolation-domain digest, pool key, and runtime ref;
- instance id, lifecycle state, boot epoch, and Lillux identity;
- pool ready/leased/starting/draining counts;
- queue wait, cold worker start, lease handoff, reset, and teardown durations;
- connection reuse state and provider transport partition;
- lease/thread correlation;
- typed refusal, death, reset, and recovery reasons;
- warm-worker versus cold-process latency comparison during acceptance.

Worker ids and thread ids belong in traces/audit, not unbounded metric labels.

## Security and correctness invariants

1. A worker executes only a durable lease whose pool key equals its boot class
   and isolation domain.
2. Every invocation has its own admitted capsule; worker identity never
   substitutes for invocation admission.
3. Secret values are neither content-addressed nor durable in worker records.
4. Principal, project, credential, and transport partitions cannot be crossed
   by an initial implementation.
5. The daemon remains authoritative for cancellation and provider accounting.
6. One worker runs at most one lease at a time.
7. A worker returns to `Ready` only after terminal settlement, authority
   revocation, and reset acknowledgement.
8. Recovery uses the admitted capsule, not worker memory or mutable registries.
9. Lillux owns OS process/isolation operations.
10. Pool capacity configuration cannot widen runtime execution authority.
11. Changed content produces a different worker class, and changed authority
    produces a different pool key, rather than mutating a live worker's
    meaning.
12. No legacy runtime protocol or implicit fallback is introduced.
13. A worker process identity is owned once by its worker record; thread state
    projects an active lease and never re-parents the PID across invocations.
14. Every lease is a subset of the worker's exact boot-time authority ceiling.
15. Signed CAS lease binding precedes send; acceptance/running are later
    idempotent transitions, and SQLite remains projection only.

## Landed substrate

The following parts of the earlier implementation ladder now exist and are no
longer owned by this future note:

- the signed `worker` kind and `persistent_session` protocol;
- exact source/realization admission and path-free retained capsules;
- a bounded daemon-owned pool keyed by admitted session identity;
- one active request per fixed local-provider session;
- enforced-isolation target-channel plumbing, cancellation, teardown, restart,
  and no-contact terminal replay; and
- the standard Tinygrad/Qwen recorded local-provider worker.

These mechanics are reusable by the leased class, but none of them constitute
an invocation lease or authorize a worker to execute arbitrary admitted items.

## Remaining implementation increments

### Increment 1 — Lease contracts and state machine

- Define `WorkerClass`, `WorkerIsolationDomain`, `WorkerPoolKey`,
  `InvocationLease`, lifecycle enums, refusal enums, and canonical hashes.
- Define the signed managed-worker protocol.
- Add pure lifecycle/lease validation and adversarial tests.
- Do not spawn a persistent process yet.

### Increment 2 — Durable worker-instance ownership

- Reuse the landed persistent-target ownership and add the durable
  `WorkerProcessRecord`, typed thread process/lease projection, daemon worker
  registry, and leased pool state.
- Route cancellation, reconciliation, status, and allowed actions through the
  active lease projection without copying process ownership into the thread.
- Start a fixture worker, complete challenge/ready, drain, and kill.

### Increment 3 — One generic invocation lease

- Add leased framing to one eligible runtime without changing the fixed-session
  framing.
- Execute exactly one admitted fixture lease through the normal callback and
  accounting paths.
- Reset and execute a second lease in the same process.
- Prove invocation state and credentials do not cross the boundary.

### Increment 4 — Cancellation, death, and recovery

- Cover cancel before lease, cancel after lease, hard kill, daemon shutdown,
  provider-issued ambiguity, reset failure, and exact-capsule recovery.

### Increment 5 — Measured consumer acceptance

- For a directive consumer, reuse a provider connection across sequential
  invocations in one exact isolation domain.
- Compare cold and warm paths using the existing stage timings.
- Confirm provider/model/accounting facts and no secret-value logging.

### Increment 6 — Operational pool controls

- Add bounded capacity, queueing, idle drain, upgrade drain, status, and
  metrics.
- Publish a node operations runbook and rollback procedure.

## Acceptance gate

The worker path is ready only when:

- cold and warm executions have byte/semantic parity in their admitted
  envelopes and terminal records;
- cancellation and recovery tests cover every lease lifecycle boundary;
- provider attempt accounting remains exact across worker death;
- worker upgrade drains by class hash and authority changes partition by pool
  key;
- no secret or previous invocation payload is observable to the next lease;
- Lillux performs all process/isolation teardown;
- real warm measurements show a material improvement over the completed
  pre-worker path.

Passing this gate activates the leased class only. Failure leaves the existing
fixed local-provider worker untouched and correct.

## Related contracts

- `knowledge:ryeos/development/admitted-execution-recovery`
- `knowledge:ryeos/development/steering-graph-interrupt-and-cancel-path`
- `knowledge:ryeos/development/filesystem-durability`
- `knowledge:ryeos/future/distributed-substrate-deferred-advanced`
- `knowledge:ryeos/future/hosted-node-trust-boundaries`
