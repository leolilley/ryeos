```yaml
category: ryeos/future
name: nested-execution-ownership-and-evidence
title: Nested Execution Ownership and Evidence
description: Deferred composition of existing child execution and scoped process ownership, including independently observed launchers, product-selected parents, and interactive subordinate execution
entry_type: design
version: "0.1.0"
```

# Nested Execution Ownership and Evidence

## Status and scope

Deferred design, recorded on 2026-09-25. This note captures the ownership
discussion around the independent Codex runtime verifier and identifies when
future workloads may require additional composition of existing mechanisms.
It does not establish a new execution kind, process service, or qualification
authority, and does not claim that the external execution backend is qualified.

RyeOS already supports managed child executions, authority derivation, durable
process attachment, cancellation and recovery. The deferred work is in specific
combinations of those capabilities. Implement a combination only when a concrete
workload demonstrates that the existing contracts cannot express it.

The scoped-producer and interactive-verifier work observed in
`feature/external-candidate-execution` is implementation in progress. References
to that work below describe the design boundary, not a landed or supported API.
Current operational contracts and acceptance evidence remain with their owning
implementation and core knowledge.

## Existing ownership mechanisms

| Mechanism | Responsibility | Boundary |
| --- | --- | --- |
| Ordinary managed child execution | Admit a child item under derived authority; retain its relationship, execution identity and terminal result | A logical child need not be an OS process spawned by its requesting parent |
| Follow execution | Suspend an eligible parent and resume it after the child's continuation chain settles | Requires the existing continuation and lifecycle eligibility; suspension cannot drive an ongoing interactive conversation |
| Detached execution | Admit a child that runs concurrently while preserving lineage and launch authority | Concurrency alone does not provide raw interactive process I/O or permission to outlive any cancellation policy |
| OS descendants within an execution scope | Run implementation subprocesses and account for their containment and cleanup | An individual descendant does not automatically receive independent execution authority or qualification evidence |
| Scoped subordinate process | Retain a process and its scope under an admitted root, with exact launch and settlement evidence | The verifier branch is extending this mechanism; it is not a replacement for ordinary child Threads |

The daemon's attachment-before-execution boundary records the actual target's
identity before releasing target code. Lillux owns process mechanics; RyeOS owns
admission, durable attachment, stop policy, recovery and terminal settlement.
That split applies regardless of whether the requester is a root or child item.

A compiler spawning helpers often needs only enclosing scope ownership. A tool
invoking another independently authorized RyeOS item normally belongs on the
existing child-execution path. Independently attesting an internal helper's
launch and behavior is an additional requirement, not an automatic consequence
of its position in the OS process tree.

## Authority and evidence rules

1. **Declared work is data.** Signed recipes and admitted definitions select
   executable identities, arguments, environment sources, resource bounds and
   allowed interaction. They remain subject to node policy, caller authority
   and the executing parent's delegation ceiling. Parentage is not a grant.
2. **Retained content determines the executable.** Selection must resolve to
   the exact admitted realization and member. Keep the necessary leases and
   descriptor authority through launch. A callback-supplied path or an ambient
   executable lookup cannot substitute for that authority.
3. **The process owner retains lifecycle responsibility.** Launch identity,
   applied isolation, stream ownership, termination and scope settlement must
   remain attributable to the component that actually owns those operations.
   Failure to acknowledge or observe an operation does not transfer ownership.
4. **The verifier interprets observations.** Behavioral checks belong to the
   admitted verifier and its signed scenario. Process ownership does not prove
   semantic correctness. A signature identifies the source and integrity of a
   statement; the qualification policy still determines what it can establish.
5. **Evidence authority is explicit.** A harness report may be admissible under
   a policy that expressly trusts that harness as the observer. It cannot
   silently replace execution facts that the policy requires the daemon to
   corroborate. A process library linked into the reporting harness does not,
   by itself, create an independent daemon observation.
6. **Scope settlement and candidate freeze are joined.** Direct target exit
   does not prove that descendants, inherited pipes or other workspace borrowers
   have settled. Frozen output requires the applicable writer exclusion and
   capture contract, including writers outside the target's immediate family.
7. **Qualification joins identities.** The accepted result must bind the
   admitted recipe/source, selected subject, actual execution attempt,
   observations, settlement and candidate identity. Runtime qualification and
   candidate evaluation remain distinct records with their existing owners.

These rules do not require a particular OS parent-child topology. They require
that the selected topology supply the evidence its qualification policy accepts.

## Boundary for the current verifier

The agreed direction is for the exact admitted Codex executable to be the direct
target of the existing daemon-owned scoped attempt. The enclosing verifier owns
the scripted peer, protocol orchestration and semantic checks. An intermediate
scenario driver whose only role is to launch and drive Codex can be removed.

This still represents a subordinate operation of the verifier. It neither bans
ordinary subprocesses nor requires general managed nesting before qualification
can proceed. The generic execution owner manages an admitted executable and
bounded byte streams. Codex app-server protocol semantics stay in the verifier
and its signed definitions.

The current implementation must finish its interactive ownership contract:
ordered bounded input, stable bounded output reads, explicit input closure,
concurrent lifecycle supervision, cancellation and exact terminal observation.
Lost acknowledgments must not cause blind input replay. Recovery of durable
attempt records must not pretend to recreate live pipe handles. These are current
implementation obligations, not work deferred by this note.

Removing the driver must preserve the execution conditions relevant to the
qualification claims: executable and configuration identity, environment,
mounts, network route and guest execution. A changed arrangement must not be
accepted as evidence for materially different runtime use.

At the discussion checkpoint, ordinary follow and detach explicitly refused
some product-selected parent compositions; follow also suspends the parent.
Those concrete restrictions, together with the interactive I/O requirements,
explain why an unchanged ordinary child path was not sufficient. Recheck the
current source before treating these restrictions as permanent.

## Deferred work and the conditions that justify it

### Independently observed nested launchers

Bring this forward when the launcher or supervisor's own child-creation behavior
is part of the subject being tested. Replacing such a launcher with a direct
target would change the experiment.

First specify which facts need independent observation and which harness is
trusted to report which facts. If daemon corroboration is required, determine
whether ordinary child dispatch or the existing scoped owner can supply it.
Any additional observation or delegated launch interface must retain exact
source, target, attempt and enclosing-scope identities under its actual owner.
Relaying a producer-authored success record through a protected channel does not
make it a daemon-authored launch observation.

Qualification must also distinguish a launcher that truly created its child
from a launcher that merely requested daemon-managed execution. Those are
different behaviors even when they run the same executable.

### Product-selected parents using ordinary child execution

Bring this forward when a real workflow requires a product-selected parent to
use follow or detach. Extend the existing admission and continuation contracts
after establishing how exact product selections, realization leases, project
authority and delegation survive the handoff and recovery.

Removing a refusal check is not the implementation. Fresh admission and recovery
must preserve the same authorized selections, and parent/child results must
retain their existing publication owners. Follow and detach keep their distinct
parent-lifetime semantics.

### Multiple supervised subjects and delegated process operations

Bring this forward when one admitted operation needs several independently
controlled subjects, distinct child budgets, or different child lifetime
policies. First identify whether ordinary item children already express the work.

If scoped-process composition is needed, define parent and child attempt
identity, allowed launch requests, aggregate resource ceilings, I/O access,
cancellation propagation and cleanup responsibility. Distinguish permission to
request work from ownership of its process. Define explicit behavior for parent
death, daemon death, partial launch and unresolved settlement.

A durable child allowed to outlive its requester needs an admitted lifetime and
an owner for its retained resources and result. Neither an open pipe nor an OS
reparenting event provides that authority. Reuse existing accounting, workspace,
execution and qualification owners rather than duplicating their records in a
second process hierarchy.

## Acceptance for a future extension

Each extension needs a concrete unmet workload, the existing contract that
refuses or cannot express it, and an explicit account of new authority. Compare
reuse of ordinary child execution, enclosing-scope descendants and scoped
subprocesses before adding an interface.

The joined tests should exercise the boundaries the extension changes:

- Exact admitted executable and recipe selection, including substitution and
  stale-attempt refusal; retained content must survive the required handoff.
- Parent and child authority, isolation and resource ceilings, with no implicit
  credential, callback or publication-authority inheritance.
- Ambiguous launch/input acknowledgments and repeated observation without
  duplicated execution or blind byte replay; mismatched requests must refuse.
- Parent cancellation or death, daemon restart, and descendants that retain
  pipes or workspace write access; incomplete cleanup must remain unresolved.
- Binding of observations and terminal evidence to the correct attempt, plus
  refusal of fabricated, incomplete or differently scoped evidence.

Tests that establish a component's mechanics do not establish the composed
workflow or qualify an installed provider. State both the passing boundary and
the remaining acceptance work.

## Related knowledge and implementation anchors

- [Tool runtime authority](tool-runtime-authority.md): authority derivation and
  callback enforcement, including deferred delegation.
- [Managed runtime worker leases](content-addressed-managed-runtime-workers.md):
  separately admitted invocation lifetime within a persistent runtime.
- [Attachment before execution](../../../../bundles/standard/.ai/knowledge/ryeos/core/execution/attachment-before-execution.md):
  durable process ownership, held release and recovery.
- [Hosted worker execution](../../../../bundles/standard/.ai/knowledge/ryeos/core/execution/worker-hosted-execution.md):
  child observation, workspace ownership and terminal evidence.
- [Follow child execution](../../../../crates/engine/ryeos-executor/src/execution/spawn_follow_child.rs)
  and [detached child execution](../../../../crates/engine/ryeos-executor/src/execution/spawn_detached_child.rs):
  existing managed execution relationships.

Discussion provenance: the `external-candidate-execution` worktree's verifier
handoff, runtime-verifier gap note and implementation ledger under `.tmp/`
record the September 24–25 implementation sequence. They include uncommitted
branch work and changing acceptance boundaries. This note preserves the deferred
design independently of those temporary progress records.
