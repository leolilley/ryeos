<!-- ryeos:signed:2026-07-23T10:04:31Z:9210f18d9c8f7d1644f5275acb7f21d926d2492b6aa924bafae58bed2bfed8fb:I2Z2ELCpgsZZmNYfMIGLbE8x7gWQIKDh2WsAFvCFuCkMZ5q1o7TiJppxJfqAGj6ydfQxB6iu7xhpYsJVzPyRDg==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
---
category: ryeos/papers
tags: [papers, research-program, index]
version: "0.1.0"
description: >
  Index and conventions for the RyeOS papers folder: the white paper thesis
  plus working notes for a four-paper research program on portable verified
  execution and its downstream consequences.
---

# RyeOS papers

Working notes for a research program. One white paper and four papers, each
defending exactly one claim, arranged so that every downstream claim is
inherited from an upstream result rather than asserted fresh.

None of these files are drafts. They are reference material for later
analysis, positioning, and writing — same register as the original white
paper notes.

## The program in one paragraph

RyeOS changes what an execution is: an object (signed, content-addressed,
durable) rather than an event (located, mortal, witnessed only by its
machine). From that one substitution the rest is forced — portability forces
proof, proof makes the executor fungible, fungible executors include
authored-output executors (operators, models), and verification for that
class is testimony, not recomputation. The papers walk that chain; the white
paper carries the general systems thesis; the agent paper cashes the
consequences; the forgetting paper keeps the program honest about
permanence.

## Files

| File                                       | Role                                                            |
| ------------------------------------------ | --------------------------------------------------------------- |
| `series-map.md`                            | The spine: shared derivation, strain points, vocabulary, rules. |
| `portable-execution-white-paper-thesis.md` | The white paper thesis (moved from `future/`, v0.3.0).          |
| `execution-is-an-object.md`                | Paper 1 — the ontology. What execution *is*.                    |
| `testimony-not-determinism.md`             | Paper 2 — the flagship. What verification *means*.              |
| `the-corporate-agent.md`                   | Paper 3 — downstream. What agents *are and owe*.                |
| `semantics-of-forgetting.md`               | Paper 4 — the open-theory paper. What permanence *costs*.       |
| `white-paper-relation.md`                  | The contract between the series and the white paper.            |

## Reading order

First time: `series-map.md`, then the white paper thesis, then papers 1–4 in
order. Papers 3 and 4 assume papers 1 and 2; the white paper assumes none of
them and is independently publishable.

## Conventions

- **One claim per paper.** If a file starts defending two claims, one of
  them is either upstream material (move it to `series-map.md`) or a new
  strain point (record it in the map, decide ownership there).
- **Definitions have owners.** Each shared term is defined in exactly one
  file; everything else references it. The ownership table lives in
  `series-map.md`. Do not redefine — drift between copies is how a series
  rots.
- **Citation is strictly downstream.** Papers 3 and 4 cite 1 and 2. Papers
  1 and 2 cite only the white paper and external literature. No sideways
  citation between 3 and 4.
- **Promotion rule.** A section becomes a paper only by decision recorded in
  `series-map.md`. (Standing precedent: reputation/agent-economy material is
  a section of paper 3, not a fifth paper.)
- **Signing.** Files stay unsigned while actively churning; sign when a file
  stabilizes. The white paper thesis was previously signed and needs
  re-signing after its move and 0.3.0 update.
- **Versioning.** Notes start at `0.1.0`; bump minor for substantive
  additions, patch for wording.
