# Future: Hardening-Program Deferred Follow-ups

## Status

Deferred, deliberately. These six items are the complete remainder of the
2026-07 hardening + front-end program (waves 0-3 plus the 07-04 closeout):
everything else that program ever parked or deferred has been built. Each
entry records what it is, where the seam lives, and the concrete trigger
that should cause someone to build it. Do not build ahead of the trigger —
every one of these was cut because the need is hypothetical until proven.

## 1. As-launched resolution digest (explain view v3)

The item explain view (`view:ryeos/item/explain`) re-resolves the extends
chain at inspection time and renders a "resolved now — may differ from
launch" caveat, because launch-time `ResolutionOutput` is not persisted.

- **Design:** persist a slim digest at launch — ancestor refs + content
  digests + composed `policy_facts` — as a launch event on the braid (the
  same seam the follow-lineage facts use). The explain view then renders
  launch truth with an optional "re-resolve now" comparison.
- **Seam:** the launch event emission in the executor launch path; the
  explain view YAML gains a second source.
- **Trigger:** the first time the caveat actually misleads — i.e. an
  operator debugs against the current chain while the launched chain
  differed. Until drift bites in practice, the caveat is honest enough.

## 2. Declared shadows (`shadows:` manifest intent)

Downstream bundles legitimately shadow another bundle's runtime config by
exact ref (e.g. arc shipping `config:ryeos-runtime/execution` overrides —
project-first resolution is the mechanism, so the foreign namespace is
required). Today publish surfaces these as an INFO-level note in the
namespace lint; the shadow is inferred, not declared.

- **Design:** a `shadows:` list in `manifest.source.yaml` — signed intent
  that this bundle overrides named foreign refs. The lint then verifies
  declared-vs-observed instead of inferring, and an undeclared shadow can
  warn loudly.
- **Seam:** `BundleManifestSource` (crates/daemon/ryeos-bundle/src/
  manifest.rs) + `lint_item_namespaces`
  (crates/tools/core-tools/src/actions/publish.rs).
- **Trigger:** shadow-related confusion recurring downstream. One incident
  produced the INFO note; a second means inference isn't enough.

## 3. Manifest re-sign audit

A `manifest audit` action showing family/operation deltas between two
signed manifests (runtime_authority additions/removals, kind changes), so
a re-signing campaign is reviewable as a diff of granted authority rather
than a YAML diff.

- **Seam:** core-tools action over two manifest.yaml files (or a manifest
  vs its bundle's installed predecessor).
- **Trigger:** a republish campaign that is painful to review by hand. The
  7-bundle republishes so far have not been — the manifests are small and
  the authority deltas obvious.

## 4. Per-row affordance gating

The threads-list "Watch child" affordance renders on every row but is
meaningful only on a suspended-parent row; elsewhere its merge fields
(`{record.follow.child_thread_id}`) resolve to null and activation no-ops.
Harmless today, noise as per-row affordance counts grow.

- **Design:** an affordance-level `when:` predicate over record fields
  (presence/equality only — reuse the projection tone-map vocabulary, do
  not invent an expression language), evaluated at view-model projection
  so both renderers get gated affordances for free.
- **Seam:** affordance projection in `crates/clients/base` view_model +
  the view YAML vocabulary; the deferral is noted inline in
  `bundles/studio/.ai/views/ryeos/threads/list.yaml` at the watch-child
  affordance.
- **Trigger:** cosmetic until either a third conditional affordance
  appears on a list view, or a no-op activation confuses an operator.

## 5. Braid-level double-terminal guard

The event chain physically accepted `thread_failed` followed by
`graph_completed` in one braid (the 2026-07-04 sync-abandon incident). The
status state machine correctly refused the failed→completed transition, so
the projection held — but both terminal events remain in the chain. The
root cause (route timeouts cancelling handler futures and firing
finalize-on-drop guards) is fixed at the dispatcher; a chain-level guard
rejecting a second contradictory terminal event is defense-in-depth with
no known remaining producer.

- **Seam:** the daemon-side event append path (`runtime.append_event` /
  finalize handling in the UDS server) — reject or quarantine a terminal
  event on an already-terminal thread, loudly.
- **Trigger:** natural fit for the run-stability lane's cancel/finalize
  plumbing (B-waves), which is already reworking terminal-state handling;
  build it there rather than as a standalone pass. Escalate to "now" if a
  second contradictory-terminal producer is ever observed.

## 6. Hosted/federation cluster

Deferred as a group since before the program, unchanged by it:

- GC sweeps for remote/federated state (07 scoped GC to local state).
- `sandbox_wrap()` becoming real sandboxing (today an identity wrapper).
- MCP network authentication.
- Multi-principal resolution.

- **Trigger:** an actual hosted or federation deployment decision. These
  are requirement-shaped, not backlog-shaped: building them against
  imagined deployments produces the wrong designs. When the decision
  lands, each needs its own spec pass — do not treat this section as one.

## Provenance

Extracted 2026-07-04 from the `.tmp/implementation/` program closeout
(README + OPERATOR-RUNBOOK carry the full landed record and commit ids).
Items 1-4 carry their deferral notes at the code/YAML seams named above;
this doc is the index, the seams are the truth.
