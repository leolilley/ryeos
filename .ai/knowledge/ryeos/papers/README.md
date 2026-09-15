---
category: ryeos/papers
tags: [papers, research-program, index]
version: "0.2.0"
description: >
  Index and conventions for the RyeOS papers folder: the white paper thesis
  plus working notes for a four-paper research program on portable verified
  execution and its downstream consequences.
---

# RyeOS papers

Working notes for a research program. One white paper and four papers, each
defending exactly one claim, arranged so that downstream claims state their upstream assumptions and additional proof obligations.

None of these files are drafts. They are reference material for later
analysis, positioning, and writing — same register as the original white
paper notes.

## The program in one paragraph

RyeOS represents execution as signed, content-addressed, durable objects,
separating work from its current executor. This motivates portable authority,
recoverable histories and attributable judgment. These are connected design
obligations, not automatic consequences of serializing a run. The papers
develop that systems thesis, its implications for agents and measurement, and
the retention and privacy obligations it creates.

## Claim status and alignment

The September 2026 alignment distinguishes architectural hypotheses from
proved results and current release guarantees. Earlier impossibility claims
about other systems, automatic recovery/federation, and unconditional
permanence are withdrawn. Other systems can adopt similar contracts; the
question is architectural fit and demonstrated behavior.

Signatures establish attribution under key/trust assumptions, not external
truth, complete history or legal identity. Governance complements enforcement.
The newer [enduring-environment synthesis](../future/enduring-working-environment.md)
and [adaptation evidence note](../future/governed-adaptation-and-memory-evidence.md)
supply the product-facing boundaries. Changes here align the argument, not
authorize their implementation. Edited papers are unsigned pending review and
normal signing.

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
| `measurement-not-benchmarking.md`          | Research note beside the program: measurement of authored-output executors. Candidate paper 5 (see decisions log). |
| `sincerity-under-open-frames.md`           | Math companion to the measurement note: absorption theorem, corrected sincerity conjecture, toy models. |

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
