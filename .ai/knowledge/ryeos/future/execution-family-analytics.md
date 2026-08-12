<!-- ryeos:signed:2026-08-12T07:28:15Z:4679ad0c8308a2c386a4389835707670a9515cdd0a47c4ef9bb9d33705a6904a:vOnLzK6B7Ui1uPGJPerB1dkaASZ+tFp0EBqYPbQdq+jDzNF3nfrIyEln5DKcIYXjEM50SiFbTOjHwLCunlwDBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [future, analytics, execution-family, effective-definition, cost]
version: "0.3.0"
status: deferred
description: >
  Behavioral diff over the effective-definition digest seed and
  cost-per-digest series across execution families.
---

# Execution family analytics

Effective-definition digests name behavior; family entities group lineage by
canonical ref. The exact authorized two-run comparison surface is now landed:
it verifies retained operands, reports bounded structural definition and
realization changes, and emits authoritative per-run cost samples without
disclosing arbitrary composed values. Its remaining current gate is live web
and terminal acceptance against a complete cost-bearing pair.

This note now owns what remains beyond that pair: attribution and cost series
over an execution family. Project-owned bank/pending/replay/digest-moved
classification remains acceptance instrumentation, not generic analytics: the
project interprets its outcomes, while RyeOS supplies identity and provenance.

## 1. Behavioral diff — "what changed between these two runs"

Decomposes mechanically:

- **Same effective digest** → the composed program is pinned. First compare the
  admitted realization/target scope and action/effect identity; only when
  those also match may remaining differences be attributed to a recorded
  stochastic boundary (and, once determinism classes land, to a specific
  divergence proof).
- **Different digest** → walk the two digest seeds and name the contributor
  that changed: root, a specific ancestor, a hook-plan layer, a grant, a
  config key. The seed is a versioned structure, so the diff is structural,
  not textual.

The pairwise structural seed diff is landed. The deferred family deliverable is
to aggregate those typed change records across more than two authorized
members, preserving contributor coordinates and allowlisted identity sides.
Arbitrary composed values and their hashes remain undisclosed. A bare
definition digest is not a lookup capability, and the existing artifact-grid
comparison contract remains unchanged. The result should answer, for example,
"this family moved when the operator layer gained a hook," not merely "the
outputs differ."

## 2. Cost series — computation with a receipt

Accounting scopes are sealed into capsules, so spend is already attributable
to nameable behavior. Exact per-run pair samples are landed. The deferred
deliverable is a bounded series grouped by effective digest across a family —
cost regressions become detectable without inventing a second accounting
total. Direct costs and rollups retain their basis and are never summed
together or with their children. Relationship to
`directive-provider-accounting-and-hard-budgets.md`: budgets act at
admission/dispatch time; this reads sealed scopes after the fact. Separate
concerns, one denominator.

## Boundaries

- The generic layer diffs seeds and reports sealed cost samples. Domain outcomes
  (win rates, quality) remain project evidence mapped by project projections —
  the field's standing ownership line.
- No new store: seed diffs compute from capsules already retained; series
  compute from run summaries already projected.
- A first bank and a replay are distinct. Campaign reports may aggregate the
  distinction, but generic analytics never infer replay from record existence
  or from equal result bytes.
- Provider-turn evidence and outer graph-dispatch evidence are separate rows;
  an aggregate must not collapse them into one replay claim. First-class
  provider publication rows in the execution field remain a prerequisite of
  the next operator acceptance slice, not landed analytics substrate today.

## Triggers to revisit

- the solver family exceeds a handful of members and "which change caused
  this" is asked more than once a week;
- a project proof view prompts "why did this digest move" questions its
  project-owned classification cannot answer (that is this note's cue, not a
  defect in the campaign slice);
- cost anomalies get investigated by reading run lists manually.
