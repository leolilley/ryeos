<!-- ryeos:signed:2026-08-07T09:22:40Z:00e5e29cc565c9c344f87f79de27a2afb3c345f37617a40fd020e1fabba021a9:7LZ9RYqU6NOht9fFNTJh1b9OZPBbCQAqpfcF7yetnBcDKq1GKNkPLQEpWzI/90a1PB+b65jNT+jQfJPL5bXYBg==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, realization, weights, storage, inference, tinygrad]
version: "0.1.0"
status: draft
description: >
  A large-object tier for external-content realizations so model weights can
  be sealed content: pin-only ingest, contiguous mmap-ready storage under the
  pinned authority, streaming verification — identity layer unchanged.
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

## The invariant: identity is already done

A weights realization slots into the existing wire shape —
`{id, kind, mode, manifest_hash, entry_count, total_bytes, mount}` — so
sealing into the effective digest, descendant inheritance, capsule
reading, effect-record scoping, and divergence attribution all work
untouched. A new `kind: weights` changes how bytes are stored, ingested,
mounted, and verified. It does not change what identity means.

## Decisions

1. **Pin-only.** A weights declaration requires `mode: pinned` with its
   manifest digest; `captured` is refused at validation. Capturing tens of
   GB of ambient bytes inside launch admission is the anti-pattern this
   tier exists to avoid — weights enter through an explicit ingest action,
   and a launch only proves the pinned closure is present.
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
6. **Bounds become per-kind.** The 32 MiB / 256 MiB / 10k-entry bounds
   keep governing `tree` and `file` realizations unchanged. The weights
   kind is bounded by the node's large-object budget and a per-file
   ceiling sized for real shards (hundreds of GB node budget, tens of GB
   per file), enforced at ingest where the cost is paid knowingly, not at
   launch where it is too late.

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

0. Wire contract: `kind: weights` in the shared realization enum,
   pin-only validation, per-kind bounds carve-out. Every strict decoder
   that names realization kinds is taught in the same commit — the
   admit-the-field lesson is standing.
1. Large-object store under the pinned authority: streaming resumable
   ingest (hash-while-write, chunk sidecar), immutable publication,
   lease + budget sweep.
2. Operator ingest surface (`ryeos weights realize <path>` or a signed
   tool): stream a safetensors file or shard directory in, emit the
   manifest object, print the pin line an author pastes into a
   declaration.
3. Bind path: mount weights realizations straight from the store; launch
   admission proves closure presence and sizes.
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
