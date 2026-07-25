<!-- ryeos:signed:2026-07-23T10:04:37Z:6964cfb91b845545f4e58f48cada771d156d7c1350cc43ae3e41706473d315d6:E2ukYs9aEyxYVoS6SoOUWw1QA0fN7Gz2Jp3FX24WO38ozkSF1MN3XNwl686su6ISgMvzdnuQw7n4e1541t4NCg==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
---
category: ryeos/papers
tags: [papers, ontology, execution, durability, process-model]
version: "0.1.0"
description: >
  Working notes for paper 1, "Execution Is an Object": the process
  abstraction conflates a computation with its interpreter; separating them
  turns durability, resume, replay, and migration into corollaries.
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

> For fifty years the process has been the primitive of computing, and the
> process conflates two things: the computation and its interpreter. An
> execution under the process model is an event — located on one machine,
> mortal, witnessed only by that machine, surviving as at most an exit code
> and side effects. RyeOS separates computation from interpreter: the run is
> a signed, content-addressed, durable object, and the process is a
> disposable projection of it. Once separated, durability, resume, replay,
> migration, and cancellation stop being subsystems and become corollaries.

## The one claim

The process abstraction conflates computation with interpreter; separating
them (run = record, process = frontier interpreter) dissolves an entire
category of infrastructure into corollaries of the ontology.

## Argument skeleton

1. **The conflation.** Under the process model, the identity, authority,
   and existence of a computation are all properties of its interpreter:
   PID, uid, address space, machine. Kill the interpreter and the
   computation ceases to exist — not because the work is logically gone,
   but because the substrate had nowhere else to keep it.
2. **The separation.** Represent the run as a durable object (event log,
   snapshots, lineage, continuation chain) with its own identity. The
   process becomes a projection: process : execution :: SQLite projection
   : CAS. RyeOS already states this tiering for state; the radical move is
   applying it one level up, to the process itself.
3. **The corollaries.** Resume after process death, replay of history,
   continuation across a segment cut, and cross-node migration are all the
   same operation — reading the execution back in from its own record.
   Systems on the process model build each of these as a separate
   subsystem; systems on the object model get them as theorems.
4. **The time inversion.** The process model treats present state as truth
   and history as optional decoration (logs). The object model inverts the
   arrow: the past is the truth, the present is a cache. "Now" is the
   growing edge of the record. This matches how courts, science, and
   accounting already work — authority derives from the record, not from
   the current state of anyone's RAM.

## Definitions owned by this paper

- **record / projection** — the durable object is truth; process, database,
  and client views are rebuildable projections of it.
- **frontier** — the not-yet-executed edge of the record, where the run
  touches the world. Nondeterminism exists only here; completed steps are
  consumed as data and never re-crossed.

## Evidence in the implementation

Present these as demonstrations, not aspirations:

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
  of re-dispatched — completed steps are data, never re-crossed.
- CAS-first writes with rebuildable SQLite projection — the tiering this
  paper generalizes.

## Objections and current answers

- **"This is just event sourcing / durable execution (Temporal)."** Three
  differences. (a) Those systems record the execution; here the record *is*
  the execution — the ontological claim, not an implementation pattern.
  (b) The object is self-certifying (signed, content-addressed), so it
  survives leaving its trust domain — see paper 2 and the white paper's
  necessity argument. (c) The executor is out of the definition, so the
  model generalizes to interpreters Temporal cannot host (see paper 2's
  executor class).
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
- Death of the process is a cache eviction, not an ending.
- Systems on the process model build subsystems; systems on the object
  model prove theorems.

## Guardrails

- This is the theory paper. The white paper deliberately does not lead with
  this framing ("everything is data" is mechanism, not headline there); the
  series can afford the ontology paper because papers argue theses, not
  products.
- Do not claim determinism, safety, or semantic correctness — ontology
  only.
- Do not mention agents. The executor is not this paper's subject; that
  discipline is what lets papers 2 and 3 inherit cleanly.
