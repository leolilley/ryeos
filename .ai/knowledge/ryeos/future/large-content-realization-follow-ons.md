<!-- ryeos:signed:2026-08-27T04:21:33Z:c6e69e518a4dbd6c22682379ca8844afd6338446969fa694e499c23e27b61851:2CK0VG0xgMo2jvUtPhKHK+QrziAb48PKEGznzisZBOHLGa2kIVgGLrDcQvD2DX9CIJRlc/wJSEinbqOpP1OSDw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, realization, large-content, storage, inference, training, trace, checkpoint]
version: "0.2.0"
status: scheduled
description: >
  Deferred follow-ons for RyeOS's landed semantically blind large-content
  realization tier: measured composition, compiled artifacts, traces, corpora,
  training outputs, operational policy, and generation-state consumers.
---

# Large-content realization follow-ons

## Current foundation

The large-content tier is implemented. It is not a weights kind and the shared
wire contains no model vocabulary. Operator-owned import produces current
content or large-content manifest objects; exact consumer binding grants mount
authority; closure traversal, scrub, staging, retention, and restart recovery
operate over those objects. The local-inference fixture uses it for model and
toolchain bytes.

The mechanical distinction is storage behavior:

- ordinary content is CAS material appropriate for bounded trees/files and
  materialization;
- large content is immutable contiguous storage with chunked verification,
  leases, budget sweep, and read-only binding suitable for very large files.

Consumer meaning remains item-authored data. Model weights, token/logit/kernel
traces, a runtime image, a corpus, an adapter, optimizer state, a checkpoint
tensor set, and a generation capsule do not create Rust variants merely
because they use the same storage mechanics.

## Standing decisions

1. **Import and consumption remain separate authorities.** Only configured
   local operator import roots can ingest. A manifest hash does not grant mount
   authority; binding names the exact consumer ref and publisher.
2. **Captured and pinned both execute retained bytes.** `captured` describes
   expected-versus-observed digest policy at admission, not permission to fall
   back to the live filesystem during execution or recovery.
3. **Large content is pin/import only.** A launch cannot accidentally capture
   tens of gigabytes through the bounded ordinary-content walker.
4. **Verification is ingest/scrub evidence.** Large immutable files are
   streaming-verified on import and scrubbed explicitly; launch verifies the
   manifest, residency, identity, and binding rather than rereading an entire
   model.
5. **Self-contained realizations only.** Manifest symlinks must resolve within
   the realized tree. Absolute or lexically escaping links cannot smuggle an
   ambient runtime into an otherwise captured realization.
6. **No host paths in identity.** Logical root identity, policy identity,
   file identity, manifest bytes, and declared bounds may move a digest;
   absolute installation/import paths may not.

## Scheduled work

### Composition and deduplication

Whole-file content addressing already deduplicates identical objects. Do not
add content-defined chunk storage speculatively. Revisit composition only with
real evidence that repeated full checkpoints or model derivatives dominate
storage. Prefer explicit base-plus-overlay manifests when the consumer can
prove that mechanical composition reproduces the intended bytes; consumer
concepts such as LoRA stay outside state/CAS vocabulary.

### Verified compiled artifacts

Sealed local inference needs an observed kernel/compiler product set promoted
into a closed admitted realization. The large tier supplies storage and
retention but does not decide whether an artifact is a kernel, whether it is
complete, or whether its numerics are deterministic. Those proofs belong to
the provider qualification contract.

### Trace and corpus payloads

Bounded events carry hashes and summaries; large token, selected-logit,
compiler/kernel, agent-trajectory, and diagnostic payloads use ordinary or
large content according to their mechanical size and access pattern. Corpus
builders consume exact retained inputs under explicit privacy, permitted-use,
deduplication, split, and filtering policy, then publish an immutable corpus
manifest. The generic tier never decides that an event is a rationale, a
training example, or safe to disclose.

Credentials, private hosted-worker homes, withheld evaluation inputs, and
sources without admitted permitted-use policy must not enter a corpus closure.
Hidden provider chain-of-thought is not a capturable large-content class.

### Training outputs

Tinygrad training may publish immutable candidate weights, adapters, optimizer
state, and checkpoints through the same realization machinery. The training
program owns those meanings and the exact base-model/corpus/recipe coordinate.
An adapter or base-plus-overlay representation is permitted only when a signed
consumer contract proves its mechanical composition. The large-content store
does not promote a candidate model; evaluation and explicit promotion do.

### Generation-state payloads

Generation-state capsules may reference large payload hashes, but the generic
store/index knows only bounded opaque payloads, lineage, tenant scope, leases,
and retention. KV layout, tokens, sampler state, and resume compatibility stay
provider-owned. Derived speculative payloads use cheaper retention than
evidence-bearing external realizations.

### Operational policy from measurements

Current file, launch, node-budget, and scrub defaults were chosen for the
fixture worker. Revisit them with retained measurements from a serious remote
model, trace capture, recorded training, and capsule payloads:

- ingest throughput and resumability;
- scrub duration and corruption detection;
- active lease pressure and free-space behavior;
- mmap/read patterns under inference;
- orphan/stage reclamation;
- manifest entry/size distributions;
- corpus/checkpoint write amplification and retirement pressure; and
- training/inference concurrency under node storage budgets.

Node policy remains expressed as bounds and named authorities, never as a
model-specific exception.

## Triggers to revisit

- observed-artifact promotion begins for sealed local qualification;
- a serious remote model or recorded tinygrad training run is activated;
- retained trace/corpus payloads exercise certification and disclosure policy;
- a second large-content consumer exposes an overly consumer-shaped contract;
- generation capsules need multi-gigabyte payload retention;
- real checkpoint/model churn shows whole-object deduplication is materially
  inadequate; or
- production measurements invalidate the current import/scrub/budget limits.
