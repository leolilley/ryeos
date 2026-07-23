---
category: ryeos/papers
tags: [papers, series-map, derivation, vocabulary]
version: "0.1.0"
description: >
  The spine of the RyeOS paper series: the core forcing derivation, the four
  strain points, the altitude map, citation discipline, and the shared
  vocabulary with definition ownership.
---

# Series map

This file holds everything that belongs to no single paper: the derivation
all four papers walk, the strain points that generated them, and the shared
vocabulary. If a claim gets restated in two paper files, it moves here and
gets referenced.

## The core derivation

One ontological substitution, then four forced steps. Nothing after step 0
is a design choice.

**Step 0 (the substitution).** An execution is an object, not an event.
Since 1970 an execution has been something that *happens* — on a machine,
once, witnessed only by that machine, leaving an exit code and side effects.
RyeOS makes it something that *exists*: content-addressed, signed, durable.
"Running" is redefined as extending the object at its frontier. The process
is not the computation; it is the interpreter currently advancing it.

**Step 1 (durability is a corollary).** If the run is an object, the process
is a disposable projection of it (process : execution :: SQLite projection :
CAS). Resume, replay, continuation chains, and cancellation stop being
subsystems and become corollaries of the ontology.

**Step 2 (portability forces proof).** If the object can outlive and leave
its machine, the machine can no longer be the authority for what it is or
who authorized it. Authority must travel inside the data: content addressing
for identity, signatures for authorship. Self-certification is not a
security feature — it is the minimal machinery portability forces. You
cannot unbind execution from location without making execution
self-certifying.

**Step 3 (the executor is fungible).** If history and authority live in the
data, any sufficiently trusted interpreter can advance the object — this
node or that one, a CPU or a model. The substrate is right for AI *because*
it does not care what the executor is, and the same indifference makes the
model the least special part of the agent.

**Step 4 (federation falls out).** If trust travels as data, machines that
do not trust each other can advance each other's work. Work carries its
warrant; delegation is a signed conversation between keys.

## The strain points

The derivation is strongest where it strains. Each strain resolves into a
load-bearing insight, and each insight owns a paper.

| # | Strain                                | Resolution                                                                 | Owner  |
| - | ------------------------------------- | -------------------------------------------------------------------------- | ------ |
| 1 | The world is not content-addressed.   | The signature attests testimony, not truth. RyeOS is an attribution system: trust is never eliminated, it is localized into revocable decisions about keys. The system is juridical, not mechanical. | Paper 2 |
| 2 | The executor is stochastic.           | Replay means re-witness, not recompute. Deeper: verification-by-recomputation requires an external spec; authored-output executors have none, so testimony is the only defined relation. | Paper 2 |
| 3 | The agent does not hold its own key.  | Agent identity is custodial — a corporate person acting through officers. The model is retained counsel; succession is officer succession, not death. | Paper 3 |
| 4 | Permanence collides with finitude and privacy. | History-as-truth owes a theory of meaning-preserving forgetting: which parts of the past can be dropped without changing what the surviving record means. | Paper 4 |

## Altitude map

| Paper | File                           | Changes what           | The one claim                                                                                     |
| ----- | ------------------------------ | ---------------------- | ------------------------------------------------------------------------------------------------- |
| 1     | `execution-is-an-object.md`    | what execution *is*    | Separating computation from interpreter turns durability, migration, replay into corollaries.     |
| 2     | `testimony-not-determinism.md` | what verification *means* | Recomputation verifies executors that have specs; testimony governs executors that are their own spec. |
| 3     | `the-corporate-agent.md`       | what agents *are and owe* | On an accountable substrate, agents acquire standing — and pay for it in unretractability.        |
| 4     | `semantics-of-forgetting.md`   | what permanence *costs* | A system whose truth is its history owes a semantics of what may be forgotten.                    |

The white paper sits beside, not above: its thesis is portable verified
execution as a general systems property, agents demoted to applications. See
`white-paper-relation.md` for the contract.

## Citation discipline

Strictly downstream. Papers 3 and 4 cite papers 1 and 2 and add no new
primitives — that is their strength, not a limitation. Papers 1 and 2 cite
only the white paper and external literature. No sideways citation between
3 and 4.

## Shared vocabulary and definition ownership

Each term is defined in exactly one file. Reference, never redefine.

| Term                        | Meaning (compressed)                                                                  | Owner   |
| --------------------------- | ------------------------------------------------------------------------------------- | ------- |
| record / projection         | The durable object is truth; process, database, and client views are rebuildable projections of it. | Paper 1 |
| frontier                    | The not-yet-executed edge of the record, where the run touches the world. "Now" is the growing edge of the record. | Paper 1 |
| authored-output executor    | An executor whose acts are authored rather than specified — it is its own spec; no external correctness predicate exists. Operators are the founding members; models are the second. Consciousness-free boundary. | Paper 2 |
| specification gap           | The essential (not practical) reason recompute-verification fails for authored-output executors: there is nothing to compare against. | Paper 2 |
| re-witness vs recompute     | What replay means on the testimony branch: reading the record back, never re-running the world. | Paper 2 |
| trust localization          | Trust is never eliminated, it is given an address: a discrete, inspectable, revocable decision about a key. Verification is objective; trust is local policy over the same evidence. | Paper 2 |
| custodial identity          | The agent's key is held and exercised by the node on its behalf; the agent is the juridical person, the model an authorized officer. | Paper 3 |
| standing                    | The ability to be a party to commitments. Chain: proof → accountability → trust → authority → agency. | Paper 3 |
| warrant chain               | act → agent key → grant → grantor key → operator. Responsibility as a walk, not a metaphysical puzzle. Ultra vires is detectable. | Paper 3 |
| meaning-preserving forgetting | Deletion that leaves every surviving object's verification and every warrant chain intact. | Paper 4 |
| capability and consequence  | What can be done and what was done, joined by one identity. Owned by the white paper; papers reference it as the mechanism of testimony. | White paper |

## Decisions log

- 2026-07-23: Reputation/agent-economy material is a **section** of paper 3,
  not a fifth paper. It is the economic face of standing, not a separate
  result.
- 2026-07-23: The white paper keeps its own thesis and guardrails; the
  series does not replace it and it is not "paper zero."
- 2026-07-23: Paper 2 is the flagship. The earlier candidate ("portability
  forces proof") is real but is setup; it now lives in the white paper's
  necessity argument and step 2 of the derivation above.
