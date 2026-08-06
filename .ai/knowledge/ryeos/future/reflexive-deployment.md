<!-- ryeos:signed:2026-08-06T03:37:11Z:8b31f3fbd244deafce10f9dbf879e943a1089ac49cca2067592e2006dff2fc0b:poyLVTEQgsW/Yh/QyMToODYEap3aHOWheRmy2C30IWdo1s673yvUd4K0nFXfGrb8mtHCJmbvwb/a0Dhs0ZtvBw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, deployment, activation, self-hosting, evidence]
version: "0.1.0"
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
