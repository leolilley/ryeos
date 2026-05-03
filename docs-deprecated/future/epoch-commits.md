```yaml
id: epoch-commits
title: "Epoch Commits: Signed Hash Chain over Knowledge Graph State"
description: A tamper-evident developmental history for an agent's knowledge graph. Periodic signed commitments over graph state and structured diffs, chained by hash, stored in CAS. No blockchain, no consensus — uses the existing Ed25519 signing key and content-addressed store.
category: future
tags:
  [
    knowledge,
    graph,
    signing,
    cas,
    history,
    audit,
    transparency-log,
  ]
version: "0.1.0"
status: planned
```

# Epoch Commits: Signed Hash Chain over Knowledge Graph State

> **Status:** Planned. Builds on the knowledge runtime ([knowledge-runtime.md](knowledge-runtime.md)) and the existing CAS + Ed25519 signing infrastructure.

> **Scope:** A verifiable, append-only record of how an agent's knowledge graph evolved over time, signed by that agent's identity key. Not a blockchain. Not consensus. Not "proof of learning" — proof that *what the agent's key claimed about the graph* at time T was X.

---

## 1. Identity = Key (Ryeos Premise)

In Ryeos, **the agent is the signing key.** There is no separate "user account," no "agent ID" beyond the Ed25519 public key. Every signed item — knowledge, tool, directive, config — is bound to a key, and that key *is* the authoring identity. The trust store is a set of public keys; trust is not a database row, it is the cryptographic ability to verify a signature.

Epoch commits inherit this premise without exception:

- An "epoch chain" is per-key, not per-user-account.
- The chain is a sequence of statements **the key has made about its own knowledge graph**.
- A key is never recovered, merged, or transferred. Lose the key, lose the chain. A new key starts a new chain — a different agent, by definition.
- Verifying an epoch commit is the same operation as verifying any other signed CAS object.

This is the only consistent posture given the rest of the system. It also constrains scope: epoch commits cannot say anything about cognition, only about what a key signed.

## 2. Closing the Trust Loop

The knowledge runtime design ([knowledge-runtime-arc-agi-3-design.md](knowledge-runtime-arc-agi-3-design.md) §2) defines the trust loop:

```
sign → compose → verify → inject into prompt → reason → act → learn → sign → ...
```

Each cycle adds signed knowledge items to the graph. The loop is closed at the *item* level — every leaf is signed and verifiable. But the loop has no record of itself. There is no signed statement that says "as of this turn, the graph looks like this." Two agents (or one agent across time) can produce the same final graph by different paths, and the path is unrecoverable.

Epoch commits add one more arrow:

```
sign → compose → verify → inject → reason → act → learn → sign → COMMIT EPOCH → ...
```

The commit is the agent's signed assertion about the graph state it has reached. It is the same key that signs items, signing a hash over those items. Nothing new about identity; just another signed object kind, used to pin state.

## 3. Problem

Ryeos has signed knowledge items in CAS. Each item is verifiable in isolation. But there is no record of:

- What the knowledge graph looked like at a given point in time
- What changed between two points in time
- Which key authored those changes as a coherent transition

Logs and snapshots are not enough. Logs are mutable. Snapshots are large. Neither is signed. Neither chains. An auditor reviewing an agent's behavior at some past moment cannot reconstruct the knowledge state the agent's key was operating from in any tamper-evident way.

We want: a thin, append-only chain of signed commitments that pins the knowledge graph state at chosen points and records the structural delta between them. The chain itself lives in CAS like everything else.

## 4. Non-goals

- **Not a blockchain.** No consensus, no tokens, no gas. A single agent's key signs its own chain. Multi-agent federation is a separate problem.
- **Not proof of learning.** The chain proves what the agent's key signed about the graph at time T. It cannot distinguish genuine intellectual progress from any other graph mutation. The honesty assumption is the same as for any signed item.
- **Not continuous.** Epochs are explicit boundaries, not every write. Commits happen on operator/tool action, not on every knowledge mutation.
- **Not retroactive.** You cannot commit an epoch about the past. Each commit pins state as of the moment it is signed.
- **Not a replacement for CAS.** The chain references CAS objects; it does not replace them.

## 5. Primitives

### 3.1 EpochCommit

A CAS object signed by the agent's key.

```rust
struct EpochCommit {
    /// Agent identity (Ed25519 public key, hex)
    agent: String,
    /// Monotonic per-agent counter, starts at 0
    epoch: u64,
    /// CAS hash of the previous EpochCommit. None for epoch 0.
    prev_hash: Option<String>,
    /// Merkle root over all knowledge items in the graph at commit time.
    /// Defined in §6.
    graph_root: String,
    /// CAS hash of an EpochDiff object describing what changed since prev.
    /// None for epoch 0.
    diff_root: Option<String>,
    /// Wall-clock time of commit (informational, not consensus)
    timestamp_unix: u64,
    /// Free-form operator/tool note. Bounded size.
    note: Option<String>,
    /// Ed25519 signature over canonical CBOR encoding of the above fields
    sig: String,
}
```

Stored as a normal signed CAS object with kind `epoch_commit`. Verifiable by any party with the agent's public key.

### 3.2 EpochDiff

A CAS object describing structural change between two epochs. Not signed itself — its integrity comes from being referenced by the signed `EpochCommit.diff_root`.

```rust
struct EpochDiff {
    from_epoch: u64,
    to_epoch: u64,
    from_graph_root: String,
    to_graph_root: String,

    /// Item-level changes. Each entry is a CAS hash of a knowledge item.
    items_added: Vec<String>,
    items_removed: Vec<String>,
    items_modified: Vec<ItemChange>,

    /// Graph-level changes (computed deterministically from item set).
    edges_added: u32,
    edges_removed: u32,

    /// Optional structured fingerprints for downstream tooling.
    /// All optional, all deterministic functions of the graph snapshots.
    extras: BTreeMap<String, serde_cbor::Value>,
}

struct ItemChange {
    item_id: String,
    from_hash: String,
    to_hash: String,
}
```

`extras` is an extension point. v1 puts nothing there. Later: cluster fingerprints, embedding centroid shifts, custom analytics. Any extras must be deterministic functions of the two snapshots so the diff is reproducible.

### 3.3 GraphSnapshot (in-memory only, not stored)

The intermediate representation used to compute `graph_root`. Materialised on demand by the knowledge runtime; never persisted.

```rust
struct GraphSnapshot {
    /// Sorted by item_id for canonical ordering
    items: Vec<(ItemId, ItemHash)>,
    /// Sorted by (from, to) for canonical ordering
    edges: Vec<(ItemId, ItemId, EdgeKind)>,
}
```

## 6. `graph_root` Definition

`graph_root` must be a deterministic function of "the knowledge graph as of commit time" so two implementations on the same snapshot produce the same root.

**v1 definition:**

1. Collect every knowledge item resolved through the standard 3-tier system (project → user → system) at the moment of commit.
2. For each item, take its CAS content hash (already computed during signing).
3. Sort items by `item_id` lexicographically.
4. Sort edges (from `extends` / `references` frontmatter) canonically: `(from_id, to_id, kind)` lex sorted.
5. Build a Merkle tree:
   - leaves = `sha256("item:" || item_id || ":" || content_hash)`
   - leaves += `sha256("edge:" || from || ":" || to || ":" || kind)`
   - sort all leaves lex
   - standard binary Merkle, sha256, duplicate last leaf if odd
6. `graph_root = root hash, hex`

This makes `graph_root` a single hash that commits to *the entire knowledge graph state* (item contents and edges) without storing the graph itself in the commit.

A verifier given a `GraphSnapshot` can recompute `graph_root` and check it matches the commit.

## 7. Chain Semantics

- Per agent (per Ed25519 public key), epochs are monotonically increasing integers starting at 0.
- Epoch 0 has `prev_hash = None` and `diff_root = None`.
- Epoch N>0 has `prev_hash = sha256(canonical_cbor(EpochCommit_{N-1}))` and `diff_root = Some(...)`.
- A break in the chain (missing prev, hash mismatch, signature failure, non-monotonic epoch) makes the chain invalid from that point. No automatic repair.
- An agent that loses its key cannot continue its chain. A new key starts a new chain. There is no "merge."

## 8. Storage Layout

Chains live in CAS like everything else. Two indexing surfaces:

```
.ai/state/epochs/
  <agent_pubkey_hex>/
    head            ← cas_hash of current head EpochCommit
    log             ← append-only file, one line per epoch:
                       <epoch_number>\t<cas_hash>\t<timestamp>
```

`head` is the only mutable file. `log` is append-only (never rewritten); it is a convenience index, not a source of truth. The source of truth is the chain itself, walkable from `head` via `prev_hash`.

The actual `EpochCommit` and `EpochDiff` objects live in the standard CAS store, addressed by content hash.

Operator can rebuild `.ai/state/epochs/<agent>/log` from `head` at any time by walking the chain.

## 9. Operations

All operations are tools under `tool:rye/knowledge/epoch/`. They go through the normal `POST /execute` path and the knowledge runtime — no daemon changes.

### 7.1 `epoch/snapshot`

Compute and return the current `graph_root` and a `GraphSnapshot` summary. No write.

```
in:  { project_path: string }
out: { graph_root: string, item_count: u32, edge_count: u32 }
```

Used by humans/tools to inspect current graph state without committing.

### 7.2 `epoch/commit`

Create a new EpochCommit pinning current graph state.

```
in:  {
       project_path: string,
       note: string?,            // operator note, bounded
       require_change: bool,     // if true, fail when graph_root == prev graph_root
     }
out: {
       epoch: u64,
       commit_hash: string,
       graph_root: string,
       diff_root: string?,
     }
```

Procedure:
1. Read `head` (if exists). Load prev `EpochCommit` from CAS.
2. Build current `GraphSnapshot`. Compute `graph_root`.
3. If `require_change` and `graph_root == prev.graph_root`: fail.
4. Compute `EpochDiff` between prev snapshot and current snapshot. Store in CAS, get `diff_root`.
5. Build `EpochCommit { agent, epoch=prev.epoch+1, prev_hash, graph_root, diff_root, timestamp, note, sig }`.
6. Sign with the agent's key. Store in CAS.
7. Atomically update `head`. Append to `log`.
8. Return result.

Crash recovery: if step 6 succeeds but step 7 does not, the next `commit` re-derives state from `head` (still pointing at the prior epoch). The orphaned commit object remains in CAS; harmless.

### 7.3 `epoch/diff`

Compute a structural diff between two epochs.

```
in:  { project_path: string, from: u64, to: u64 }
out: { diff: EpochDiff }
```

If `to == HEAD`, computes against current uncommitted graph state. Otherwise reads the stored diff for the range, or composes diffs across multiple epochs.

### 7.4 `epoch/verify`

Walk the chain from epoch 0 to `head`, verifying:

- Every signature is valid
- Every `prev_hash` matches
- Epoch numbers are monotonic
- Each `EpochCommit.graph_root` matches the recomputed `graph_root` for that snapshot **only if** the historical graph state is reconstructible (see §11)

```
in:  { project_path: string, agent: string?, deep: bool }
out: { ok: bool, head_epoch: u64, errors: [VerifyError] }
```

`deep: false` (default) checks signatures + chain integrity. `deep: true` additionally recomputes `graph_root` for every reachable historical state, which requires the items referenced by every historical snapshot to still be present in CAS.

### 7.5 `epoch/list`

List epochs with metadata.

```
in:  { project_path: string, agent: string?, limit: u32, before_epoch: u64? }
out: { epochs: [{ epoch, commit_hash, graph_root, timestamp, note }] }
```

## 10. Knowledge Runtime Integration

The knowledge runtime ([knowledge-runtime.md](knowledge-runtime.md)) gains the ability to:

1. **Materialise a GraphSnapshot.** Already implicit in resolution; this exposes it as an output.
2. **Compute `graph_root`.** Pure function of a `GraphSnapshot`.
3. **Compute an EpochDiff between two GraphSnapshots.** Pure function.

These three are the only knowledge-runtime additions. All commit / sign / store / chain logic lives in the `epoch/*` tools, not the runtime.

The knowledge runtime stays stateless. Epoch commits are the persistent layer above it.

## 11. Historical Reconstructibility

`graph_root` only commits to *content hashes* of items, not the items themselves. So a deep verification of a historical epoch requires that every item referenced by that historical snapshot is still in CAS.

Two retention policies:

- **Pin on commit (default).** When `epoch/commit` runs, it records every item content hash in the snapshot into a CAS pin set so GC cannot remove them. The commit becomes a CAS root. Storage cost grows with retained history.
- **Forget after N epochs (opt-in).** Pins are released after N epochs. Older epochs remain signature-verifiable (the chain is intact) but `deep: true` verification can no longer reconstruct their `graph_root`. The chain is still useful as evidence of *when* claims were made.

This is an explicit tradeoff exposed via config:

```yaml
# .ai/config/epochs.yaml
retention:
  mode: pin_all              # | forget_after
  forget_after_epochs: null  # | N
```

## 12. Multi-Agent (Future)

Out of scope for v1. Worth noting the shape:

- Each agent runs its own chain.
- Inter-agent attestation: agent B can sign a commit referencing agent A's `commit_hash`, asserting "I observed A's epoch N at this time." This is a separate `EpochAttestation` object kind. Federation mechanics deferred.
- Optional mirror to an external transparency log (Trillian-style) for third-party tamper-evidence. Pure mirror; no consensus.

## 13. What This Replaces / Improves

| Today | With epoch commits |
|-------|--------------------|
| No structured record of graph evolution | Signed hash chain in CAS |
| "What did the agent know on date X?" → unanswerable | Walk chain, find epoch with `timestamp <= X`, read snapshot |
| Per-item signatures only | Per-item + per-graph-state commitments |
| Logs of writes (mutable, unsigned) | Optional, but no longer the primary record |

## 14. What It Does Not Solve

- **Proving learning happened.** A commit chain showing 50 epochs of structural change is consistent with learning, with random noise, or with deliberate fabrication by the key holder. Crypto can't tell the difference.
- **Adversarial introspection.** If an attacker has the key, they can produce any chain they want. Treat the chain as the agent's *own claim about itself*, not as ground truth about cognition.
- **Cross-agent comparison.** Different operators will choose different commit cadences and `extras` policies. Chains are not directly comparable without a normalised analysis layer (out of scope here).

## 15. Implementation Order

```
Phase 1: Plumbing
 1. Define EpochCommit, EpochDiff, GraphSnapshot types (ryeos-state or new crate)
 2. Canonical CBOR encoding + sha256 helpers
 3. Merkle root computation over GraphSnapshot
 4. CAS object kind: epoch_commit (signed), epoch_diff (unsigned)
 5. Storage layout under .ai/state/epochs/<agent>/

Phase 2: Tools
 6. tool:rye/knowledge/epoch/snapshot
 7. tool:rye/knowledge/epoch/commit (with require_change, atomic head update)
 8. tool:rye/knowledge/epoch/diff
 9. tool:rye/knowledge/epoch/verify (shallow + deep modes)
10. tool:rye/knowledge/epoch/list

Phase 3: Knowledge runtime hook
11. Knowledge runtime exposes GraphSnapshot materialisation
12. Knowledge runtime exposes graph_root computation

Phase 4: Retention
13. CAS pin integration (pin_all default)
14. .ai/config/epochs.yaml support
15. forget_after policy

Phase 5: Operator surface
16. CLI: rye epoch commit, rye epoch list, rye epoch verify
17. Crash-recovery test: commit object exists, head not updated, recover
```

## 16. Test Plan

### Determinism
1. Two runs of `epoch/snapshot` on the same graph produce identical `graph_root`
2. Two implementations of Merkle computation agree on a known fixture
3. Item order, edge order, leaf encoding all canonicalised

### Chain integrity
4. Genesis epoch: `prev_hash = None`, `diff_root = None`
5. Subsequent epoch: `prev_hash` matches CAS hash of previous commit
6. Tampered commit fails signature check
7. Inserted/missing epoch detected by `epoch/verify`
8. Non-monotonic epoch number rejected

### Diff correctness
9. Item added → appears in `items_added`
10. Item removed → appears in `items_removed`
11. Item content changed → appears in `items_modified` with correct from/to hashes
12. Edges follow item changes deterministically

### Commit operation
13. `require_change: true` and unchanged graph → commit fails, head unchanged
14. `require_change: false` and unchanged graph → commit succeeds, diff is empty
15. Crash between sign and head update → recoverable; orphaned commit harmless
16. Concurrent commits serialised (file lock or single-writer assumption)

### Verification
17. Shallow verify on a 100-epoch chain: passes
18. Deep verify with all items pinned: passes
19. Deep verify after forget_after retention: shallow passes, deep reports unrecoverable for pruned epochs
20. Verify across agent boundary: each agent's chain verifies independently

### Storage
21. `head` updated atomically (temp + rename)
22. `log` rebuildable from chain walk
23. CAS pin holds every item referenced by every retained epoch
