<!-- ryeos:signed:2026-08-27T04:21:31Z:3db6d1ce55c5946df0f5e02d8195222162f87389717fb6365fd10e24b54b219c:YeF/vgHETq99ZA0wPheVu0eKHr0/iLTWmpZKRz4wGO+hg4Q/KLZlZNRiEUhhdyqymrlgB+B2gHZhk9Q6EHshBQ==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: ryeos/future
name: local-execution-roadmap
title: Local Execution Roadmap
description: Current recorded local-model foundation and the scheduled path through serious remote inference, traces, training, sealed qualification, generation capsules, and offline export
entry_type: reference
version: "0.3.0"
```

# Local execution roadmap

This note is the ownership and sequencing map for local execution. The current
operating contract is `knowledge:local-inference/activation`; implementation
details are planned separately. Local execution remains one consumer of generic
RyeOS execution, content, evidence, and placement authority rather than a
model-specific engine subsystem.

## Landed foundation

RyeOS has:

- a generic signed `worker` kind and fixed persistent-session lifecycle;
- an `admitted_local_worker` provider transport;
- exact source closure and content/large-content realizations;
- a daemon-owned private realization view for trusted disabled-isolation
  execution, with optional enforced isolation as separate node hardening;
- stable node identity plus admitted and optional observed execution
  realizations;
- daemon-owned local observations, immutable provider-call records, crash
  repair, and zero-contact replay;
- bounded persistent pools, cancellation, restart recovery, target-channel
  ownership, and resource admission; and
- the local-inference bundle's hermetic Qwen3-0.6B CPU recorded route.

The fixture proves the contract. It does not define the serious model, device,
context, trace, training, or offline deployment architecture.

## Scheduled local-inference path

An offline-model requirement and an available capable remote execution site
now satisfy the old pull-forward trigger. The next local-execution work is:

1. **Ordinary-node activation.** Prove the recorded fixture under default
   disabled isolation using exact daemon-private input delivery. Bubblewrap is
   optional and its absence is not a degraded provider class.
2. **Profile composition.** Separate reusable tinygrad worker implementation
   from a concrete, signed model/runtime/target policy without adding model
   vocabulary to engine code or caller-selectable mutable profiles.
3. **Serious remote model.** Admit one explicit model and hardware profile on
   the intended remote target. Keep CPU Qwen3-0.6B as a cheap fixture.
4. **Recorded production execution.** Bank and replay a real model request,
   retain cold/warm/prefill/decode/resource evidence, and expose it through the
   execution field.
5. **Trace and corpus evidence.** Retain bounded agent trajectory, exposed
   reasoning, token, kernel, and generation-state references under explicit
   privacy and corpus-admission policy.
6. **Tinygrad training.** Consume exact corpus/base-model/program realizations,
   publish immutable candidate weights, evaluate separately, and promote only
   through explicit authority.
7. **Sealed qualification.** Promote an observed closed artifact/numerics set
   only after two clean processes reproduce canonical bytes under one exact
   execution identity.
8. **Generation-state capsules.** Recorded capsules may provide honest prefix,
   park/resume, and fork behavior before sealed qualification; only a qualified
   capsule may claim equivalence to uninterrupted generation.
9. **Offline export.** Rehydrate and verify the complete selected
   project/model/runtime closure in a network-independent environment.

Recorded inference, useful solve work, trace collection, and recorded training
do not wait for sealed qualification. Sealed remains an earned stronger claim,
not a deadline or prerequisite imposed on the useful path.

## Worker lifecycles

The current fixed local-provider session and any future leased general runtime
remain the same `worker` kind but different admitted protocol/lifecycle classes:

- **Fixed persistent provider session — current.** Program, realizations,
  provider role, protocol, resources, and target are fixed before boot. It
  accepts bounded provider requests and receives no project resolver, general
  invocation capability, callback token, secret set, or mutable item lookup.
- **Leased managed runtime — deferred.** A reusable vessel receives one
  separately admitted invocation lease with bounded authority and must prove
  reset before reuse.

Do not widen the fixed local-model worker merely to obtain general warm-runtime
latency reuse. `knowledge:ryeos/future/content-addressed-managed-runtime-workers`
owns that separate measured boundary.

## Trace and training boundary

RyeOS core owns content refs, events, artifacts, identities, lineage,
retention, effects, and comparisons. The local-inference bundle or project owns
tokens, logits, model reasoning, datasets, adapters, optimizer state, metrics,
and promotion semantics.

Observable tool trajectories and explicit model/provider outputs may become
training examples only through an admitted corpus builder. Hidden frontier
chain-of-thought is not inferred or claimed. Credentials, private profile
state, withheld evaluation inputs, and sources without permitted-use policy do
not enter a training realization.

## Pull-forward order

1. Pass ordinary-node fixture bank/replay acceptance without Bubblewrap.
2. Land profile composition and one serious remote target/model.
3. Complete recorded production inference and bounded trace evidence.
4. Shift hosted project work onto that route once one network-independent
   end-to-end probe passes.
5. Add corpus building and recorded tinygrad training when retained traces show
   a concrete learning opportunity.
6. Qualify one sealed inference scope when observed artifacts and numerics can
   be closed.
7. Pull recorded generation capsules when prefix/recovery/search economics are
   material; add exact-resume claims only after qualification.
8. Export a complete offline verification closure for independent acceptance.

General scheduling, broad federation, sealed training, and hostile multi-tenant
containment retain their own evidence gates.

## Owners

- Sealed qualification: `knowledge:ryeos/future/sealed-local-inference`.
- Generation state: `knowledge:ryeos/future/generation-state-capsules`.
- Execution scope: `knowledge:ryeos/future/execution-identity`.
- Large artifacts: `knowledge:ryeos/future/large-content-realization-follow-ons`.
- Provider evidence/export: `knowledge:ryeos/future/provider-call-effect-records`.
- General leased workers:
  `knowledge:ryeos/future/content-addressed-managed-runtime-workers`.

## Non-goals

- no second model/worker/federation substrate;
- no multi-backend fallback around tinygrad;
- no model, tokenizer, weights, dataset, LoRA, or KV variants in generic state;
- no automatic model/device selection;
- no requirement that normal local inference use Bubblewrap; and
- no claim that local or sealed model output is correct merely because it is
  reproducible.
