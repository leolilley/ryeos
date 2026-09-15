---
category: ryeos/papers
tags: [papers, ontology, execution, durability, process-model]
version: "0.2.0"
description: >
  Working notes for paper 1, "Execution Is an Object": the process
  separation of work from its interpreter gives durability, resume, replay
  and migration a shared identity model with explicit proof obligations.
---

# Paper 1 — Execution Is an Object

Working notes, not a draft. The theory paper: it establishes the ontology
that every other paper in the series assumes. See `series-map.md` for the
derivation this paper owns step 0 and step 1 of.

## Thesis

Short version:

> An execution is an object, not an event. "Running" is extending that
> object at its frontier; the process is the interpreter currently
> advancing it, not the computation itself.

Expanded version:

> RyeOS separates logical work from its current interpreter by retaining its
> admitted capability, authority and history as an object graph. This gives
> restart, inspection and transfer a shared identity model. Each operation still
> needs an explicit implementation and qualification contract.

## The one claim

Treating execution history and admitted authority as primary data gives
continuity a common foundation instead of tying work solely to process state.
The claim is about architectural coherence, not unique invention or automatic
recovery of arbitrary processes.

## Argument skeleton

1. **Separate work and executor.** A process can end while a logical task
   remains unfinished. The retained execution names what was admitted and
   which results or uncertainties were recorded.
2. **Retain sufficient state.** Runtime checkpoints, content closures and
   durable history support only the continuation operations their contracts
   define. A process may hold unrecorded state that cannot be reconstructed.
3. **Resume under authority.** Recovery must re-establish permissions,
   compatibility and exclusive writer ownership. It must not repeat an
   uncertain external effect merely because its response was lost.
4. **Keep projections subordinate.** UI and database projections should derive
   their authority from retained records. Their rebuilding depends on the
   actual retained closure, schema and storage durability guarantees.

## Definitions owned by this paper

- **record / projection** — retained records govern derived views; rebuilding
  and continuation depend on sufficient state and supported contracts.
- **frontier** — the not-yet-executed edge of the record, where the run
  touches the world. Retained outcomes are fixed data; fresh execution can
  introduce new nondeterminism. Completed steps are reusable under the declared
  replay contract; uncertain effects require reconciliation.

## Demonstrations to qualify

These are implementation-oriented examples, not fresh qualification results.
Record the tested revision, route and failure cuts before claiming acceptance:

- The thread event log as the execution: tail, replay, chain, children,
  cancel all act on the same object (`ryeos thread ...`, `ryeos events
  replay`).
- Dual hash-linked event braid: every event carries chain-previous and
  thread-previous hashes, so the trace is a woven structure in CAS, not a
  flat log braided by clients.
- Checkpoint plus event-log resume; project snapshot pinned at spawn.
- Reconciliation re-spawning resumable work under the same thread identity
  after a daemon restart — the process died, the execution did not.
- Completed child results spliced into a successor's resume state instead
  of re-dispatched, where the follow contract permits retained-result reuse.
- CAS-first writes with rebuildable SQLite projection — the tiering this
  paper generalizes.

## Objections and current answers

- **"This is event sourcing / durable execution."** There is real overlap.
  The proposed distinction is the integrated identity of executable content,
  authority and consequences across trust boundaries. It requires concrete
  comparison and demonstrations, not an ontological assertion that competitors
  cannot host a class of work.
- **"Side effects escape the record."** True, and out of scope here: the
  world is not content-addressed. The frontier is exactly where proof meets
  world; what the record can attest there is the subject of paper 2
  (testimony, not truth). This paper claims only that everything *about the
  run itself* is object, not event.
- **"Deterministic replay?"** Never claimed. Replay is reading the record
  back (re-witness); re-running the world was never the contract. Inherited
  guardrail from the white paper.

## Phrases worth preserving

- The process is a cache.
- The verb becomes a noun.
- An execution is something that exists, not something that happens.
- "Now" is the growing edge of the record.
- Process death need not end work whose continuation state was retained.
- Shared execution identity makes recovery contracts composable; it does not
  replace them.

## Guardrails

- This is the theory paper. The white paper deliberately does not lead with
  this framing ("everything is data" is mechanism, not headline there); the
  series can afford the ontology paper because papers argue theses, not
  products.
- Do not claim determinism, safety, or semantic correctness — ontology
  only.
- Do not mention agents. The executor is not this paper's subject; that
  discipline is what lets papers 2 and 3 inherit cleanly.
