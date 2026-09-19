<!-- ryeos:signed:2026-09-18T23:27:55Z:5477990fecaa79e0798cfd88d0952647f4fa7006d977ef47f7f92daef94733ab:YUsOPcYr4l3R97eTzDns2otJlMsMmpNSDgzgQKhWR/3F4xNfYmPQSwjFCSUpBgb/an8pukvtoiAoxxz4ej3xCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [future, deployment, activation, self-hosting, evidence]
version: "0.3.0"
status: deferred
description: >
  Activation sets as admitted programs: RyeOS sealing and evidencing its own
  upgrades.
---

# Reflexive deployment

The development process that built the 2026-08 packages — design docs,
adversarial review gates, activation sets, epoch drain/cut operations — is
conspicuously graph-shaped, and it is currently executed by a human acting as
its own orchestrator. That is the one remaining place the
no-external-orchestration rule is not enforced: the system that accounts for
computation does not yet account for its own change.

## End state

- An **activation set is an admitted program**: the merge, epoch drain,
  bundle install, boot validation, and narrow acceptance steps are nodes in a
  signed deployment graph, admitted through the same finalizer as any launch.
- The **epoch cutover leaves the same evidence trail as any solve**: which
  schemas advanced, what was drained, what boot validation proved — durable,
  field-visible facts instead of terminal scrollback.
- **Review gates are observer hooks** on the deployment chain: the
  adversarial-review outcome is admitted evidence the deployment graph
  consumes, not a chat transcript.

## Release input boundary

Reflexive deployment begins with an already-published exact release coordinate.
For the native bundle path that coordinate is the substrate image digest, an
exact deployment-authorized node-bundle selection (referencing publisher-
authorized generations and any curated set evidence), consuming bundle-
publication policy-section digest, and whole node-policy generation described in
[Native bundle publication and node composition](native-bundle-publication-and-node-composition.md).

The deployment Graph may verify publisher and qualification evidence, fetch
the closure, drain an epoch, stage it, activate it, and validate the new boot.
It does not build the candidate, mint its publisher evidence, resolve a mutable
channel during activation, or infer deployment permission from successful
checks. Release production and activation remain separate admitted programs.

A bundle-source node serving bytes is not deployment authority. A development node
that built the candidate is not deployment authority. A publisher approving
the bundle set is not permission to alter a particular running node. The
deployment decision names the exact target node and exact release coordinate.

## Why deferred

1. Bootstrap asymmetry: the deployment program must survive the very cutover
   it performs (the runtime executing it changes under it). Requires the
   drain/handoff design to treat "deployer" as a special continuation
   boundary — genuinely new ground.
2. Prerequisites: admission evidence supplies the refusal/decision
   vocabulary; determinism classes make deployment steps' effects
   classifiable; epoch cadence must first stabilize (see the standing
   disciplines in `effective-programs.md` — epochs rare, boring,
   identity-only) so there is a steady shape to encode.

## Lessons already banked (2026-08-06 activation)

The first epoch activation surfaced exactly the failure modes a deployment
graph would prevent: drain executed under the outgoing binary left the store
at the old epoch; sudo prompts swallowed by progress UI; a publish phase that
reported success against a hollowed tree with no durable record to disprove
it. Each is an argument for steps-as-admitted-programs with evidence.

## What can be done early

- Write activation runbooks as `.ai` content (checklists as signed knowledge)
  so the eventual graph has authored source to compose from.
- Emit epoch-activation events from the existing cutover command (a small
  slice of admission evidence's pattern) — the court record can precede the
  court.

## Triggers to revisit

- the third manually-executed activation (two is a coincidence; three is a
  process);
- a deployment step is forgotten or misordered once (the evidence of the gap
  is the mandate);
- admission evidence and determinism classes are both live.
