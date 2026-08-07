<!-- ryeos:signed:2026-08-07T10:40:48Z:2b52b6c614b6f58f573d799d12a432b07714a0de32b677fa2562b7e9fd18c644:o8AXqzv0ZoE/yXMGk5IIn8ptMGPky+c8QNMR2/gRCgV96wEyTQPVBcvNHJZLy5mPUHp0aeNnIJcakm1zX+jTCQ==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, realization, weights, storage, inference, tinygrad]
version: "0.3.0"
status: draft
description: >
  A semantically-blind large-content tier for external realizations, with
  model weights as its motivating first consumer: pin-only ingest,
  contiguous mmap-ready storage under the pinned authority, streaming
  verification — identity layer AND wire vocabulary unchanged; the tier is
  named by manifest-object kind and permitted by kind-schema contract data,
  never by a new enum variant, and the substrate vocabulary never says
  "weights".
---

# Weights-tier realizations

Sealed local inference needs weights as admitted content, and the numbers
say the existing realization machinery cannot carry them: the content tier
bounds files at 32 MiB and a launch's realization set at 256 MiB, while a
quantized 7B model is ~4 GB and full-precision or larger models run to
tens of GB — three orders past the bounds, and past the *mechanisms*, not
just the numbers. Capture copies bytes into CAS at launch admission;
materialization copies them back out per cache generation; blob reads load
whole payloads into memory for verification. Every one of those is wrong
at weights scale. This note designs the tier that is right at that scale
while changing nothing above the storage layer.

## The invariant: identity is already done — and so is the vocabulary

A weights realization slots into the existing wire shape —
`{id, kind, mode, manifest_hash, entry_count, total_bytes, mount}` — so
sealing into the effective digest, descendant inheritance, capsule
reading, effect-record scoping, and divergence attribution all work
untouched. And `kind` stays what it is: **shape** (`file` | `tree`), a
closed mechanical vocabulary where each variant is a distinct capture and
mount implementation. A sharded model is a tree; a single safetensors
file is a file. Weights-ness is not a shape.

Where the distinction actually lives — as data, following the kind
pattern the substrate already runs on:

- **The manifest object names its own tier.** Bind and admission fetch
  the pinned manifest from CAS regardless; a large realization's
  `manifest_hash` resolves to an `external_large_content_manifest`
  object instead of the content manifest. Routing to the large-object
  store is decided by what the digest-pinned manifest says it is —
  sealed by identity, no wire discriminator, no ambient decision.
- **The kind-schema contract grants the tier.** `execution.
  external_content` already carries `allowed_roots` and
  `max_declarations` as signed schema values; permission to pin
  large-tier content and its bounds land beside them. An item kind that
  never declared the capability cannot bind a large-content manifest —
  refused at admission, from data, not from Rust.

And the tier itself is **semantically blind**. "Weights" appears nowhere
in substrate vocabulary — not in the manifest kind, not in the store,
not in the bounds — because the mechanism (contiguous immutable large
objects, chunked streaming ingest, mmap binding, lease/budget sweep,
scrub) carries model weights exactly as it will carry generation-state
checkpoints, training-data sets, or an oversized runtime tree. Weights
is what an *author* names their realization in item-space, the way one
is named `simulator_runtime` today. The state layer's charter — it
knows nothing about what content means — holds all the way down.

An earlier draft of this note put `kind: weights` in the shared
realization enum, and a second draft named the manifest kind and store
after weights. Same mistake at two depths: letting the first consumer
leak into mechanism vocabulary — as closed wire policy the schema-data
layer should express, then as meaning the state layer is chartered not
to hold. Storage changes; identity, vocabulary, and meaning-blindness
do not.

## Decisions

1. **Pin-only, by construction rather than by rule.** The only producer
   of weights manifests is the explicit ingest action; launch capture
   only ever emits content manifests, under content bounds. Capturing
   tens of GB of ambient bytes inside launch admission — the
   anti-pattern this tier exists to avoid — is therefore not *refused*,
   it is unreachable: there is no path from `mode: captured` to a
   weights manifest at all. A launch proves the pinned closure is
   present; ingest is where weights enter.
2. **Contiguous storage, chunked verification.** Large objects live as
   contiguous read-only files under the pinned state authority, named by
   content hash — mmap-ready for tinygrad's safetensors path, zero copies
   at bind time. A chunk-hash sidecar (fixed-size chunks, e.g. 64 MiB)
   makes verification and ingest streaming and resumable: the file hash
   commits to the chunk list, so a scrub or a resumed ingest never holds
   more than one chunk in memory. Chunking is a verification structure,
   not a storage layout — the file stays whole.
3. **No materialize-copy.** The realization mount binds the store's file
   (or shard directory) read-only into the runtime, through the same
   `IsolationReadOnlyMountAuthority` and lease discipline the content
   tier uses. There is no per-generation copy and no 2 GiB cache budget
   involvement; the large-object store has its own budget and its sweep
   honors leases exactly as the materialization cache does.
4. **Verification moves to ingest + scrub.** Blob reads verify on every
   load today; an mmap has no read-through hook, so the sealed claim
   rests on ingest-time streaming verification, immutable store files
   (0444 under the authority directory), and an explicit scrub that
   re-walks chunk hashes. State this honestly in evidence: launch
   admission proves manifest presence, size, and store residency — byte
   verification is an ingest/scrub fact, not a per-launch fact.
5. **Dedup by composition, not chunking.** Identical weights pinned by
   many programs dedup whole-file by content address — the dominant case,
   free. Fine-tuned full dumps differ everywhere, so content-defined
   chunking would buy little; the real lever is **layered composition**:
   base weights as one pinned weights realization, adapters (LoRA and
   friends, MBs not GBs) as ordinary content-tier realizations mounted
   beside them. The flywheel's distilled checkpoints should prefer
   adapter form whenever the training method allows it.
6. **Bounds move to where the tier is known.** The 32 MiB / 256 MiB
   byte caps are per-tier policy, and the wire-level realization-set
   validator is tier-blind — it cannot apply them soundly once two
   manifest kinds exist. So: capture keeps enforcing content bounds in
   the walk (unchanged in effect — every captured set is content), and
   admission enforces per-tier byte bounds after fetching manifests,
   when it knows what each one is. The large tier is bounded by the
   node's large-object budget and a per-file ceiling sized for real
   shards (hundreds of GB node budget, tens of GB per file), enforced
   at ingest where the cost is paid knowingly. The wire validator
   retains what is structural — ordering, uniqueness, canonical paths,
   hash formats, the 10k entry cap, overflow-checked sums — plus a
   generous absolute ceiling as pure DoS sanity, not policy.

## GC and retention

Weights manifests are CAS objects; the large objects they name are roots
exactly as realization blobs are today — reachable while any admitted
capsule's realization set names the manifest, swept when nothing does.
Retention within budget follows the standing lanes: leased (in-use)
generations are untouchable, and eviction is honest loss — a re-pin
re-ingests. KV and prefix caches are explicitly NOT this tier: they are
derived, regenerable artifacts owned by the generation-state capsule
design, never realization content.

## Increments

There is no wire-contract increment: the vocabulary does not change, and
until the large-content manifest object exists there is nothing to
validate differently — the tier's first code is the store that gives the
manifest something to name.

1. Large-object store + the `external_large_content_manifest` object
   under the pinned authority: streaming resumable ingest
   (hash-while-write, chunk sidecar), immutable publication, lease +
   budget sweep. The bounds relocation (capture-side content caps,
   admission-side per-tier caps, wire validator reduced to structure +
   sanity ceiling) lands here, in the same change that creates the
   second manifest kind.
2. Operator ingest surface (`ryeos content ingest <path>` or a signed
   tool — tier-named, never consumer-named): stream a file or shard
   directory in, emit the manifest object, print the pin line an author
   pastes into a declaration.
3. Bind path: route on manifest-object kind, mount weights realizations
   straight from the store; launch admission proves closure presence and
   sizes, and refuses weights manifests on item kinds whose contract
   never granted the tier.
4. Scrub: streaming chunk re-verification as a maintenance action, with
   findings as typed integrity evidence.
5. First consumer: the tinygrad runtime realization pins base weights,
   adapters layer as content-tier realizations — the sealed-local ladder's
   increment 2 satisfied.

## Triggers to revisit

- Local inference work begins in earnest (this gates it);
- ingest of a real model shows the chunk size or budget defaults were
  guessed wrong;
- the distillation flywheel produces full-dump checkpoints faster than
  storage grows, making content-defined chunking worth its complexity.
