<!-- ryeos:signed:2026-08-13T03:35:01Z:fc1aed0d2a10de23dbdc2b996918e4b7ccecb1ac0b9a38e5d7828b87a046ce98:rE7ahh88/sqU1GBOIhX5Wy+tvqjMmWXRUppdFjsvGnWlYTwH0rj2pwahSPzO27dnGF9Z9XCBN+rP9h2BsheXBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: local-execution-roadmap
title: Local Execution Roadmap
description: Landed local-execution foundation and the deferred boundaries for sealed inference, generation capsules, and leased latency workers
entry_type: reference
version: "0.2.0"
```

# Local execution roadmap

This note is the boundary map for the local-execution work. It is not another
implementation plan and it does not replace the current operating contract in
`knowledge:local-inference/activation`.

Its place in the larger RyeOS sequence is defined by
`knowledge:ryeos/future/substrate-growth-roadmap`. The local worker track is
one branch after single-node execution truth; it is not the route by which
hosted-node isolation or federation is implemented.

## Landed foundation

RyeOS now has the mechanical substrate required to run a local model without
making model meaning part of the engine:

- operator-owned external-content import and exact consumer binding;
- content and large-content realization manifests, closure traversal, scrub,
  retention, and fail-closed recovery from captured bytes;
- stable node execution identity and attestation, plus per-launch admitted and
  optional observed execution realizations;
- immutable provider-call records, daemon-owned local observations, publication
  proof, restart repair, and zero-contact replay;
- one generic signed `worker` item kind compiled through the persistent-session
  execution contract;
- an identity-keyed, bounded daemon persistent-session pool with cancellation,
  restart, target-channel isolation, and resource admission;
- the optional local-inference bundle's Tinygrad/Qwen worker and
  `admitted_local_worker` provider route;
  and
- a recorded-class local inference path. It is optional node policy, not a
  required default-node service.

The `worker` kind names a mechanical execution vessel. It is not synonymous
with model inference and it is not synonymous with latency reuse.

## One kind, one current lifecycle and one deferred lifecycle

The current local-provider worker and the future latency worker remain the same
signed `worker` kind. The fixed lifecycle exists today; the leased lifecycle is
deferred. They differ by admitted protocol and lifecycle:

1. **Fixed persistent session — current lifecycle.** The worker's executable closure,
   external realizations, isolation ceiling, provider role, and request
   protocol are fixed before it starts. It serves bounded local-provider
   requests, creates fresh request execution state, and returns daemon-observed
   terminal evidence. It receives no general invocation capability, project
   handle, callback token, secret set, or mutable item resolver.
2. **Leased managed runtime — deferred lifecycle.** A warm vessel accepts a separately
   admitted, single-use invocation lease containing one capsule, callback and
   accounting authority, deadline, cancellation identity, and bounded dynamic
   bindings. Settlement revokes those authorities and a reset acknowledgement
   is required before reuse.

The second class extends the worker protocol and durable lifecycle. It must not
be implemented by widening the fixed local worker or by introducing another
kind whose only difference is a consumer name.

## Deferred tracks

### Sealed local inference

The local route is `recorded`, not `sealed`. Promotion requires a node-signed
qualification over an exact admitted realization, deterministic request and
sampler contract, retained compiled artifacts/numerics policy, and two clean
processes producing byte-identical answers. The first execution that discovers
new JIT artifacts remains recorded; qualification names a later, closed
realization. If the target cannot meet that bar, recorded is the correct final
class.

Owner: `knowledge:ryeos/future/sealed-local-inference`.

### Generation-state capsules

KV state, tokens, sampler state, and model-specific resume validation remain
provider-owned. The shared substrate may own only opaque capsule coordinates,
payload and lineage hashes, staging, immutable indexes, tenant scopes, leases,
budgets, and retention. This begins only after a positive sealed
qualification; a recorded terminal replay is not a proof that in-flight state
can resume bit-identically.

Owner: `knowledge:ryeos/future/generation-state-capsules`.

### Leased latency workers

The process/session mechanics are landed, but general warm invocation reuse is
not. Remaining work is the durable invocation lease, boot-attested worker
instance, per-invocation authority handoff, authenticated callback binding,
reset acknowledgement, recovery reconciliation, and bounded operational pool.
The 2 August measurements still do not justify pulling this track ahead of
workflow/provider improvements for chat.

Owner: `knowledge:ryeos/future/content-addressed-managed-runtime-workers`.

### Certification, retention, and export

Provider and dispatch records replay locally today. Certification needs an
explicit retention lane and an independently verifiable export closure containing the
capsule, realization/effect closure, chain history, signer evidence, and a node
attestation of the exported head. Cross-node continuation remains separate.

Owners: `knowledge:ryeos/future/provider-call-effect-records` and
`knowledge:ryeos/future/portable-execution-graph-advanced-path`.

### Aggregate hostile-workload resource enforcement

Current pool ceilings are node admission budgets around trusted signed workers.
Per-process limits do not prove aggregate CPU, memory, or PID containment for a
hostile descendant tree. That stronger claim requires cgroup-backed ownership
or an equivalent kernel boundary and belongs with hosted-node trust work, not
inside the worker kind.

Owner: `knowledge:ryeos/future/hosted-node-trust-boundaries`.

## Pull-forward order

1. Complete representative deep and broad runs without project-side substrate
   workarounds, then make the web and terminal execution field show the same
   durable facts their project evidence exposes.
2. Complete the landed two-run comparison against a real cost-bearing pair
   and retain first-bank versus replay evidence distinctly.
3. Use that evidence to decide whether sealed local qualification has immediate
   workload value on the available hardware.
4. Pull generation capsules only after qualification is positive and a search
   workload needs prefix, park/resume, or fork.
5. Pull leased latency workers only when a new latency distribution passes the
   existing measurement gate.
6. Pull portable export when a solve must be independently verified outside
   the producing node.

This order is evidence-driven. Landing a generic mechanism does not, by itself,
make its most ambitious consumer the next implementation.
