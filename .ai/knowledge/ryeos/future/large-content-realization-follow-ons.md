<!-- ryeos:signed:2026-08-10T03:16:08Z:3e4a70556b796331ce2caeee4fc9ca00eb260621c0fed83d02b47d8bd0cf860e:k5GygGaKt2ZyQiR0BEVMj7LT5oCfGDTi5aMMXFf5G3pe7BmW9sUTCsFCwfhAStMGH6E5/yN5PnYgnGYR8PHnDA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, realization, large-content, storage, inference, checkpoint]
version: "0.1.0"
status: deferred
description: >
  Deferred follow-ons for RyeOS's landed semantically blind large-content
  realization tier: measured composition, compiled-artifact reuse, operational
  policy, and future checkpoint consumers.
---

# Large-content realization follow-ons

## Current foundation

The large-content tier is implemented. It is not a weights kind and the shared
wire contains no model vocabulary. Operator-owned import produces current
content or large-content manifest objects; exact consumer binding grants mount
authority; closure traversal, scrub, staging, retention, and restart recovery
operate over those objects. The standard local worker uses it for model and
toolchain bytes.

The mechanical distinction is storage behavior:

- ordinary content is CAS material appropriate for bounded trees/files and
  materialization;
- large content is immutable contiguous storage with chunked verification,
  leases, budget sweep, and read-only binding suitable for very large files.

Consumer meaning remains item-authored data. Model weights, a runtime image, a
checkpoint tensor set, and a future training corpus do not create Rust variants
merely because they use the same storage mechanics.

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

## Deferred work

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

### Generation-state payloads

Generation-state capsules may reference large payload hashes, but the generic
store/index knows only bounded opaque payloads, lineage, tenant scope, leases,
and retention. KV layout, tokens, sampler state, and resume compatibility stay
provider-owned. Derived speculative payloads use cheaper retention than
evidence-bearing external realizations.

### Operational policy from measurements

Current file, launch, node-budget, and scrub defaults were chosen for the first
worker. Revisit them with retained measurements from larger models and capsule
payloads:

- ingest throughput and resumability;
- scrub duration and corruption detection;
- active lease pressure and free-space behavior;
- mmap/read patterns under inference;
- orphan/stage reclamation; and
- manifest entry/size distributions.

Node policy remains expressed as bounds and named authorities, never as a
model-specific exception.

## Triggers to revisit

- observed-artifact promotion begins for sealed local qualification;
- a second large-content consumer exposes an overly consumer-shaped contract;
- generation capsules need multi-gigabyte payload retention;
- real checkpoint/model churn shows whole-object deduplication is materially
  inadequate; or
- production measurements invalidate the current import/scrub/budget limits.
