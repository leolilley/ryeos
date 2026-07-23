<!-- ryeos:signed:2026-07-23T10:04:44Z:fb2c304e77e638fe73306a539e5573bee9e026d46a6dc9900187d0901f168d6f:tOwkiabO0IAi1WJEsdONgRt/NkMDa8Kgq3rRV2xfzlh723bEkoALhHl/vzevaX8+mkX2i4cHnMHhTsFV1o6FAw==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
---
category: ryeos/papers
tags: [papers, forgetting, compaction, privacy, permanence, open-theory]
version: "0.1.0"
description: >
  Working notes for paper 4, "A Semantics of Forgetting": any system whose
  truth is its history owes a theory of which parts of the past may be
  deleted without changing what the surviving record means. The open-theory
  paper that keeps the program honest.
---

# Paper 4 — A Semantics of Forgetting

Working notes, not a draft. The open-theory paper: the mechanisms exist in
the implementation; the theory underneath them is unwritten, and this paper
owes it. Owns strain point 4 from `series-map.md`. Assumes papers 1 and 2.

## Thesis

Short version:

> A system whose source of truth is its history owes a semantics of
> forgetting: a principled account of which parts of the past can be
> deleted without changing what the surviving record means.

Expanded version:

> Permanence collides with two facts. Finitude: history-as-truth meets
> finite disks, hence GC and DAG compaction. Privacy: a total signed record
> of everything an agent or operator did is also a surveillance object,
> hence sealed envelopes. Both collisions are currently resolved by
> mechanism without theory. Stated properly, compaction is not cleanup — it
> is a claim that certain pasts are semantically inert, and that claim
> needs a definition, invariants, and proofs.

## The one claim

Meaning-preserving forgetting can be defined — deletion under which every
surviving object still verifies and every warrant chain remains walkable —
and content addressing makes it constructive: you can forget the bytes and
keep the commitment, because the hash remains as an unretractable
placeholder for the content it named.

## The core question

Which parts of the past can be forgotten without changing what the present
means? "The present" here is precise (paper 1): the frontier of a record
whose authority derives entirely from that record. Delete the wrong past
and the present does not merely lose detail — it loses justification.

## Sub-problems

### 1. Forgetting vs repudiation

The entire program (paper 3 especially) depends on unretractability: an
agent's standing is its inability to disown its record. So forgetting must
never become a retraction channel. The saving observation is that CAS
splits content from commitment: dropping a body while retaining its hash,
signature, and position in the braid forgets *what was said* while
preserving *that it was said, by whom, under what authority*. Forget the
bytes, keep the commitment. Deletion of content is permissible; deletion of
the fact of commitment never is.

### 2. Privacy and crypto-shredding

The sealed-envelope vault (AEAD data encryption, DEK-wrap to vault key,
per-remote pinning) already implies the second mechanism: destroying a key
renders sealed content permanently unreadable without touching the record's
structure. Crypto-shredding is forgetting-by-unreadability — the braid
stays intact, verification still passes, the content is gone from the
world. The paper should treat key destruction as a first-class forgetting
operation with its own semantics, not an operational accident.

### 3. The accountability floor

What must never be forgotten for the system to remain what it is: enough
for every surviving object to verify, and every warrant chain (paper 3) to
remain walkable — act to agent key to grant to grantor. Candidate
formulation: the minimum permanent record is the **skeleton** — hashes,
signatures, and links — while bodies are the forgettable flesh. The floor
is not a policy choice; it is derivable from what verification and
attribution structurally require.

### 4. Compaction as theorem, not hygiene

The implementation compacts snapshot DAGs by topological sort (Kahn's
algorithm) when pruning and rewriting history. The paper's job is the
correctness condition that makes such rewrites legitimate: a compaction is
**projection-safe** iff every query answerable from surviving refs answers
identically, every surviving object verifies, and no warrant chain breaks.
GC then stops being hygiene and becomes an inference: these objects are
unreachable from any commitment, therefore semantically inert, therefore
forgettable.

## Candidate principle (to be sharpened)

> Forgetting is legitimate exactly when it is invisible to verification and
> attribution. The permanent record is the skeleton of hashes, signatures,
> and links; content is forgettable flesh; and the two operations —
> body-dropping and key-destruction — are the only forgetting a
> history-as-truth system may perform.

Open question flagged honestly: whether ref rewriting during compaction can
always be made attestable (a signed claim that the rewrite was
projection-safe), so that even forgetting leaves testimony.

## Evidence in the implementation

- DAG versioning with parent hashes; GC compaction via topological sort.
- CAS/ref split: immutable facts under mutable, signed entry points —
  the structure that makes skeleton/flesh separable at all.
- Sealed-envelope vault; vault key rotation not affecting sealed secrets —
  the key-destruction mechanism's existing half.
- The white paper's limitation "revocation is hard for immutable objects" —
  this paper is that sentence taken seriously.

## Objections and current answers

- **"This is just GC."** GC decides reachability; this paper decides
  meaning. The claim is that reachability-from-commitments is the correct
  reachability relation for a system whose truth is its history — that is
  a semantic thesis, not a memory-management one.
- **"Regulation (right to erasure) will demand more than body-dropping."**
  Possibly — and the paper's value is stating precisely what *cannot* be
  granted (deletion of commitment) without the system ceasing to be
  accountable, so the negotiation happens with the invariant on the table.
- **"Why a whole paper?"** Because every other paper's strength
  (unretractability) is this paper's problem. A program that never prices
  its permanence is advocacy, not theory. This is the paper that keeps the
  series honest.

## Phrases worth preserving

- Forget the bytes, keep the commitment.
- The skeleton is permanent; the flesh is forgettable.
- Forgetting is legitimate exactly when it is invisible to verification
  and attribution.
- Compaction is a claim that a past is semantically inert — claims need
  proofs.
- Even forgetting should leave testimony.

## Guardrails

- Never propose a mechanism that enables retraction; forgetting and
  repudiation must remain provably distinct.
- Stay tethered to implemented mechanisms (compaction, GC, vault) — the
  paper theorizes what exists, it does not spec new features.
- Keep legal framing (erasure rights) as application, not foundation; the
  foundation is the verification/attribution invariant.
