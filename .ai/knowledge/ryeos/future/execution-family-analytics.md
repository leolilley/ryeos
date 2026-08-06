<!-- ryeos:signed:2026-08-06T03:37:09Z:c6264bdbea9ca991b58f3e4a0cccf21d5619ba632d4656b12c159ac3f92a6c97:cKtBKLlpOy/zlqLssptobVeHZxRaMv/Ru8If9ELKagXnKlQBKQb4ZjgJK1FZ7FS1q/WYtiZOZ/pOZg5TGVhFDQ==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, analytics, execution-family, effective-definition, cost]
version: "0.1.0"
status: deferred
description: >
  Behavioral diff over the effective-definition digest seed and
  cost-per-digest series across execution families.
---

# Execution family analytics

Effective-definition digests name behavior; family entities group lineage by
canonical ref. That makes two questions answerable that no other system can
answer honestly, and neither has a query surface yet.

**A minimal slice ships ahead of this note** (per-version run/status/cost
aggregates on existing field entities — designed in the post-activation
implementation package). This note owns the rest.

## 1. Behavioral diff — "what changed between these two runs"

Decomposes mechanically:

- **Same effective digest** → behavior is pinned; attribute differences to the
  recorded stochastic boundary (and, once determinism classes land, to a
  specific divergence proof).
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

## Triggers to revisit

- the solver family exceeds a handful of members and "which change caused
  this" is asked more than once a week;
- the minimal slice's aggregates prompt "why" questions the aggregates cannot
  answer (that is this note's cue, not a defect in the slice);
- cost anomalies get investigated by reading run lists manually.
