<!-- ryeos:signed:2026-09-29T00:19:35Z:ecd22eaf8ab8927bfe9ad74fa5afce215d5c20ffa328d3adc94fb564c2314c67:DOG5AAveRXJkXCsF4C3wm0/Z+YZLr6eUuAdDdO4ZFvxFcNqSbJVq0ylWFQHsEOVxrTNO39uxDHxBjq+COrRXAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: data-driven-runtime-verification
title: Data-Driven Runtime Verification
description: Future separation of declarative verification policy, reusable evidence transport, and runtime-specific protocol interpreters
entry_type: design
version: "0.2.0"
```

# Data-Driven Runtime Verification

RyeOS already has a generic product qualification framework. Pinned GNU Python
qualification uses that framework with specialized signed Tool content; it does
not require Python-specific daemon logic. This direction also applies to the
broader development and bundle-release workflow and restored guest evidence.
Generalization remains separate from completing current execution proofs.
Temporary execution plans belong in the repository's `.tmp/` workspace;
durable knowledge should describe implemented contracts and explicitly deferred
designs without depending on a temporary plan's continued presence.

## Status and scope

Deferred design, recorded on 2026-09-25. This note captures the longer-term
direction discussed while implementing the independent Codex runtime
qualification. It does not change the active qualification contract, enable a
qualification claim, or require the current Codex verifier to become a
universal verifier.

The active qualification is intentionally Codex-specific. Its signed policy
selects a verifier and scenario; Codex app-server code interprets the runtime
protocol. The current branch is still integrating daemon-owned interactive
execution. The direct target, signed inputs, network route, evidence joins and
end-to-end acceptance remain owned by that implementation.

The future direction is to make scenario selection and expected behavior
declarative where that is safe, while keeping protocol interpretation in small,
auditable runtime adapters. Generalize from repeated concrete workloads, not
speculation.

## Goal

RyeOS should be able to describe verification intent as admitted data and run
it through reusable execution and evidence machinery:

```text
signed verification policy
  -> admitted subject and execution recipe
  -> bounded interaction through the actual process owner
  -> runtime-specific protocol interpretation
  -> policy checks over observations
  -> daemon join of interpretation with launch, settlement and output evidence
```

This follows the broader RyeOS pattern: data selects and composes authorized
work; code implements the finite semantics of each operation. A scenario
definition may select a protocol adapter and declare expected outcomes. It
must not supply executable authority, isolation privileges, arbitrary code, or
its own evidence trust.

## Separate the three responsibilities

### Declarative verification policy

Signed policy should describe the scenario's bounded intent, for example:

- which admitted subject/runtime realization is under test;
- the named protocol adapter and supported version;
- the conversation steps or finite scenario identifier;
- scripted, credential-free peer responses and their bounds;
- required observations and expected identities or outcomes;
- deadlines, frame and total-byte limits, retry rules, and failure conditions;
- which evidence sources the qualification accepts.

Policy is an input to admission and verification. It does not create authority
merely by naming a path, environment variable, endpoint, process, or desired
claim. Executable selection, environment, mounts, network access, callbacks,
resource ceilings and process settlement still come from their existing
admitted recipes and daemon-owned evidence paths.

Prefer finite typed scenario operations over embedded scripts or arbitrary
expressions. A policy language that can silently become a programming language
would enlarge the verifier's attack surface and make review harder.

### Reusable verification and evidence machinery

Common machinery can handle concerns whose semantics are genuinely shared:

- bounded, ordered request and response transport;
- exact attempt binding and acknowledgement validation;
- explicit offsets, framing bounds and overflow refusal;
- deadlines, cancellation and uncertain-operation handling;
- capture of raw observations without treating them as proof by themselves;
- consistent reporting of which checks ran and which evidence remains absent.

The transport must use the actual process owner. In the current direct-target
qualification, daemon callbacks deliver input to and capture output from the
daemon-owned Codex attempt. The verifier orchestrates that exchange; it cannot
turn its own report into daemon-authored launch or settlement evidence.

Reusable machinery should live at a RyeOS boundary only after multiple real
scenarios establish the shared contract. Do not move product-specific parsing
into a generic crate simply because two modules currently share code.

### Runtime-specific protocol interpretation

An adapter owns the finite semantics of its protocol: framing rules, message
correlation, valid state transitions, and extraction of observations. For the
current scenario, that is Codex app-server interpretation, including thread,
turn and notification correlation. Other runtimes may need different
adapters.

An adapter should be selected by signed, admitted policy and have a bounded,
versioned input/output contract. It interprets observations; it does not decide
which executable was launched or whether the daemon's process scope settled.
Those facts remain with the daemon. The qualification policy determines which
adapter observations, combined with which independently authored execution
facts, support a claim.

This does not imply one universal verifier binary. A small verifier per runtime
or scenario may be clearer to admit and audit. Shared transport and evidence
code can be reused behind those focused entry points.

## Evidence and trust boundary

For the current daemon-corroborated Codex profile, keep the distinction explicit:

| Evidence | Authoritative owner |
| --- | --- |
| Selected recipe, exact executable and applied launch | Admission and daemon execution owner |
| Input delivery, captured bytes, cancellation and process/scope settlement | Daemon process owner and its durable records |
| Protocol validity and scenario interpretation | Admitted runtime-specific verifier/adapter |
| Qualification decision | Qualification policy joining the required evidence sources |

A signed verifier is trusted code for its stated interpretation. Its signature
does not make its process report an independent witness of a child launch. If a
future qualification intentionally trusts a verifier to launch and describe a
child, it must state that weaker/different evidence claim. If it requires
daemon corroboration of the exact child, the child must use an owner and
evidence contract that supplies it.

For the current strong Codex claim, qualification must join the admitted
scenario and subject identity, exact attempt, runtime observations, relevant
provider evidence, whole-scope settlement, writer exclusion and frozen output
under their respective owners. A transcript or peer request log alone does not
establish executable identity or complete settlement. Other claims select their
required evidence owners and guarantees explicitly.
The strong Codex requirements are not universal prerequisites for trusted-child
checks or provider-backed point measurements, and those other contracts cannot
be silently substituted for the active Codex claim.

## Codex as the first concrete scenario

The current Codex verifier is a useful first instance, not proof that the
future framework already exists. Its policy can eventually describe the
selected Codex realization, app-server scenario, scripted provider responses,
expected thread/turn behavior and observation bounds. A Codex adapter would
interpret the app-server messages. Reusable transport could drive the
daemon-owned scoped attempt and capture its output. The daemon would still
corroborate exact launch and settlement before the qualification policy accepts
the result.

The current work must first prove this concrete path end to end. Future
configuration must not be used to paper over missing launch authority, signed
input bindings, isolated network reachability, lifecycle arbitration, restart
behavior, or evidence joins.

## When to implement the broader design

Bring this forward when at least one concrete second runtime or materially
different scenario demonstrates repeated verification structure that cannot
be expressed cleanly through the existing signed policy and focused verifier.
Before extracting a shared framework, document:

1. Which policy fields are common and which belong to one runtime.
2. Which protocol semantics require executable adapter code.
3. How adapter identity and version are admitted and bound to the scenario.
4. Which authority is supplied by admission versus merely described by data.
5. How raw observations map to qualified evidence, with an explicit owner for
   each fact.
6. How failures, cancellation, uncertain I/O, restart and partial observation
   remain fail-closed.

## Acceptance criteria

A future implementation should demonstrate:

- signed scenario data is canonical, bounded, versioned and rejects unknown or
  ambiguous fields;
- data can select only already admitted subjects, adapters, operations and
  evidence requirements;
- unsupported adapter versions and unsupported operations refuse before
  target contact;
- runtime-specific parsers enforce framing, state-machine, correlation and
  resource bounds;
- retries cannot duplicate non-idempotent input, and missing acknowledgements
  remain uncertain until reconciled;
- each required execution fact comes from its admitted owner; for the current
  strong Codex profile, the daemon supplies launch, isolation, capture and
  settlement evidence, which verifier-authored statements cannot replace;
- evidence is bound to the same signed scenario, subject and attempt, including
  the terminal, frozen output and writer-exclusion result when the selected
  claim requires them; all remain mandatory for the current strong Codex claim;
- negative tests reject substituted executables, changed policy, fabricated or
  mismatched observations, late provider contacts, incomplete settlement and
  restart ambiguity; and
- a second real runtime demonstrates reuse before shared abstractions are
  promoted as platform contracts.

## Related knowledge

- [Nested execution ownership and evidence](nested-execution-ownership-and-evidence.md)
  records the current boundary between daemon-owned execution facts and
  verifier-owned interpretation.
- [Data-driven scope profiles](data-driven-scope-profiles.md) describes a
  related RyeOS pattern: declarative policy expands only into explicit,
  validated authority.
- [Attachment before execution](../../../../bundles/standard/.ai/knowledge/ryeos/core/execution/attachment-before-execution.md)
  defines the daemon's durable launch-ownership boundary.
