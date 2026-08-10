<!-- ryeos:signed:2026-08-10T03:16:08Z:023e88fc345b526a621aec7ee271357f97c169d60a6b0a9aafce6d6765539308:hHrg/lD7nSJHfxJopKY8rDRTY7jAzEqE1NnwqVKDUT1fIIB+tjiyFdyOQ1ZBnVqQ+YTBFDAfIwo9SWDwImP5AA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, analytics, execution-family, effective-definition, cost]
version: "0.2.0"
status: deferred
description: >
  Behavioral diff over the effective-definition digest seed and
  cost-per-digest series across execution families.
---

# Execution family analytics

Effective-definition digests name behavior; family entities group lineage by
canonical ref. That makes two questions answerable that no other system can
answer honestly, and neither has a query surface yet.

The field already projects effective-definition families and bounded run facts.
The next ARC campaign slice adds project-owned bank/pending/replay/digest-moved
classification from daemon-supplied `_dispatch` and `_run` proof rows. That is
acceptance instrumentation, not the generic seed-diff/cost-series query owned
here: ARC interprets its outcomes, while RyeOS supplies identity and provenance.

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

Deliverable: a seed-diff projection — given two `definition:@digest` entities,
emit typed change records (`contributor`, `coordinate`, `from`, `to`) the
field renders in its existing compare machinery. The output is "the output
differs *because* the operator layer gained a hook," not "the output differs."

## 2. Cost series — computation with a receipt

Accounting scopes are sealed into capsules, so spend is already attributable
to nameable behavior. Deliverable: cost-per-effective-digest as a series
across a family — cost regressions become detectable the same way behavior
regressions are. Relationship to
`directive-provider-accounting-and-hard-budgets.md`: budgets act at
admission/dispatch time; this reads sealed scopes after the fact. Separate
concerns, one denominator.

## Boundaries

- The generic layer diffs seeds and sums sealed costs. Domain outcomes
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
  the next ARC operator slice, not landed analytics substrate today.

## Triggers to revisit

- the solver family exceeds a handful of members and "which change caused
  this" is asked more than once a week;
- the ARC campaign proof view prompts "why did this digest move" questions its
  project-owned classification cannot answer (that is this note's cue, not a
  defect in the campaign slice);
- cost anomalies get investigated by reading run lists manually.
