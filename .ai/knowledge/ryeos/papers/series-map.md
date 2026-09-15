---
category: ryeos/papers
tags: [papers, series-map, derivation, vocabulary]
version: "0.2.0"
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

One architectural substitution, followed by four connected design questions.
The steps motivate one another; they are not a proof that implementation,
security or federation follows automatically.

**Step 0 (the substitution).** Represent execution as a durable object graph,
distinct from the process currently advancing it. Existing event-sourced and
durable-execution systems also separate logical work from processes; the RyeOS
hypothesis concerns the integration of executable content, authority and
consequence under cryptographic identity.

**Step 1 (recovery has an owner).** Retained history can support restart,
continuation and inspection. Recovery still requires durable commit boundaries,
compatible runtimes, effect reconciliation and available retained state.
Some executions cannot resume; some effects remain uncertain.

**Step 2 (portable evidence).** Work crossing trust domains needs a way to
establish identity and provenance under the recipient's policy. RyeOS chooses
hashes, signatures and explicit grants. Portability does not logically require
this single architecture, and a valid signature does not confer authority.

**Step 3 (bounded executor replacement).** A compatible, authorised executor
may advance work at a supported boundary. Changing models or runtimes can
require a new admission or linked execution. The record persists without
asserting that the replacement has equivalent behavior.

**Step 4 (federation is a further contract).** Independent sites can exchange
verifiable objects, but continuation additionally needs target admission,
explicit custody transfer, writer fencing, recovery and retention rules.
Evidence possession is not continuation authority.

## The strain points

The derivation is strongest where it strains. Each strain resolves into a
load-bearing insight, and each insight owns a paper.

| # | Strain                                | Resolution                                                                 | Owner  |
| - | ------------------------------------- | -------------------------------------------------------------------------- | ------ |
| 1 | The world is not content-addressed.   | Signatures attribute claims; trust policy, enforcement and independent evaluation remain necessary. | Paper 2 |
| 2 | Reproduction is not task success.    | Recorded replay, qualified recomputation and task evaluation answer different questions and can coexist. | Paper 2 |
| 3 | Mandates can outlive executors.      | Custodial authority and succession need explicit contracts; the corporate analogy is not legal personhood. | Paper 3 |
| 4 | Permanence collides with finitude and privacy. | History-as-truth owes a theory of meaning-preserving forgetting: which parts of the past can be dropped without changing what the surviving record means. | Paper 4 |

## Altitude map

| Paper | File                           | Changes what           | The one claim                                                                                     |
| ----- | ------------------------------ | ---------------------- | ------------------------------------------------------------------------------------------------- |
| 1     | `execution-is-an-object.md`    | what execution *is*    | Separating work from its executor gives recovery and transfer a shared identity model, with explicit qualification obligations. |
| 2     | `testimony-not-determinism.md` | what verification *means* | Reproduction establishes computational agreement; attributable testimony and task evaluation address different claims. |
| 3     | `the-corporate-agent.md`       | what agents *are and owe* | Durable mandates can survive executor changes, subject to custody, governance and retention policy.        |
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
| record / projection         | Retained records govern derived views; rebuilding and continuation depend on sufficient state and supported contracts. | Paper 1 |
| frontier                    | The not-yet-executed edge of the record, where the run touches the world. "Now" is the growing edge of the record. | Paper 1 |
| authored-output executor    | An executor making judgment-bearing choices not fully settled by recomputing its process. Humans and models can both perform such work; task-specific correctness predicates may still exist. | Paper 2 |
| specification gap           | Reproducing a decision does not establish that it satisfies the intended task or values; evaluation requires an independently stated criterion. | Paper 2 |
| re-witness vs recompute     | Reading retained evidence versus performing computation again; recorded-effect reuse and qualified reproduction have distinct contracts. | Paper 2 |
| trust localization          | Trust is never eliminated, it is given an address: a discrete, inspectable, revocable decision about a key. Verification is objective; trust is local policy over the same evidence. | Paper 2 |
| custodial identity          | A durable mandate can be exercised by authorised custodians; the corporate analogy does not define a new key hierarchy or legal person. | Paper 3 |
| standing                    | The design analogy of an entity bound to a mandate and attributable acts; not automatic legal standing or trust. | Paper 3 |
| warrant chain               | Links from an act to its invocation authority and granting principals, subject to retained evidence; not a complete causal or legal judgment. | Paper 3 |
| meaning-preserving forgetting | Retention change with explicitly preserved verification and attribution claims, and explicit loss of claims requiring deleted evidence. | Paper 4 |
| capability and consequence  | What can be done and what was done, joined by one identity. Owned by the white paper; papers reference it as the mechanism of testimony. | White paper |

## Decisions log

- 2026-09-10: Replaced the forced-derivation claim with explicit design and
  qualification obligations. Earlier entries below record historical reasoning,
  not an endorsement of superseded impossibility or unconditional-proof claims.

- 2026-07-23: Reputation/agent-economy material is a **section** of paper 3,
  not a fifth paper. It is the economic face of standing, not a separate
  result.
- 2026-07-23: The white paper keeps its own thesis and guardrails; the
  series does not replace it and it is not "paper zero."
- 2026-07-23: Paper 2 is the flagship. The earlier candidate ("portability
  forces proof") is real but is setup; it now lives in the white paper's
  necessity argument and step 2 of the derivation above.
- 2026-08-10: Measurement-of-executors material (observer frames, folds,
  invariants) is a **research note beside the program**
  (`measurement-not-benchmarking.md`), not paper 5. It identifies a
  candidate fifth strain point — step 3's "sufficiently trusted":
  fungibility forces comparison of authored-output executors, and the
  specification gap forbids a canonical benchmark. Its vocabulary (frame,
  fold, admissible observer, invariant candidate) is owned by the note and
  enters this map only on promotion. Promotion requires its demonstrations
  landing (the measurement fold; a frame-diff run) and a decision recorded
  here.
