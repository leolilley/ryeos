<!-- ryeos:signed:2026-08-10T10:35:12Z:f83a1fa5444c11e6662789300eaa869b709fa6fbdff93d95a86913e567256920:RKdz4FGQh9w4Ej5wshg6CerT5BpStrIDR7ghqkuK+udKkI0JktcWwtKS2UtWGWMpileBFjDfHzy6GblMpNTgDA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
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
classification from daemon-supplied `dispatch` and `run` proof rows. That is
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

Deliverable: a seed-diff projection — given two exact, authorized run subjects,
emit typed change records (`contributor`, `coordinate`, `change`, plus
allowlisted identity sides) through a dedicated execution-comparison field
source. Arbitrary composed values and their hashes remain undisclosed. A bare
definition digest is not a lookup capability, and the existing artifact-grid
comparison contract remains unchanged. The output is "the output differs
*because* the operator layer gained a hook," not "the output differs."

## 2. Cost series — computation with a receipt

Accounting scopes are sealed into capsules, so spend is already attributable
to nameable behavior. Deliverable: exact per-run cost samples grouped by
effective digest across a family — cost regressions become detectable without
inventing a second accounting total. Direct costs and rollups retain their
basis and are never summed together or with their children. Relationship to
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
  the next ARC operator slice, not landed analytics substrate today.

## Triggers to revisit

- the solver family exceeds a handful of members and "which change caused
  this" is asked more than once a week;
- the ARC campaign proof view prompts "why did this digest move" questions its
  project-owned classification cannot answer (that is this note's cue, not a
  defect in the campaign slice);
- cost anomalies get investigated by reading run lists manually.
