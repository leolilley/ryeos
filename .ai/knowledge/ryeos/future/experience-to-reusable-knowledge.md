---
category: ryeos/future
name: experience-to-reusable-knowledge
title: From Experience to Reusable Knowledge
description: Deferred design for scoped adoption, selection, correction and privacy of knowledge derived from execution evidence.
entry_type: design
version: "0.1.0"
status: discussion
---

# From experience to reusable knowledge

Discussion recorded: 2026-09-07. Unsigned working design, not an implemented memory subsystem, approved schema, or autonomous-promotion permission.

This note owns the proposed knowledge lifecycle, independently of temporary implementation plans. For the broader reasoning and resume questions, read [Enduring working environment — vision and open questions](enduring-working-environment.md). Existing knowledge composition, admission, attestation and candidate services retain their implementation ownership.

## The missing connection

Execution records answer what was admitted, attempted and observed. Knowledge composition answers which context is rendered. Neither alone answers why a new interpretation should influence later work.

Keep three concepts distinct:

- Evidence: attributable observations, with scope and completeness limits.
- Interpretation: a claim, summary, preference inference or reusable strategy derived from evidence.
- Authority: permission to perform a specific operation, including changing future policy or selecting active context.

A signed interpretation is attributable, not necessarily true, accepted, current or authorized for every use. A source appearing in the model's input is not proof that it caused a particular output.

## What existing docs already cover

Repository-root paths:

| Existing owner | Relevant coverage | Boundary still to connect |
|---|---|---|
| bundles/standard/.ai/knowledge/ryeos/standard/kinds/knowledge.md and knowledge/composition.md | Knowledge context, composition, positions and budgets | Eligibility for reusable adoption, correction and withdrawal |
| crates/runtimes/knowledge/src/types.rs and compose.rs | Verified-item inputs, composition membership, exclusions and over-budget omissions; query output includes content digests | Whether all needed metadata survives admission and is inspectable for later use |
| crates/engine/ryeos-executor/src/augmentations/compose_context_positions.rs | Resolution, child authority and rendered context/meta projection | End-to-end explanation of selection and current eligibility |
| crates/state/ryeos-state/src/objects/attestation.rs | Issuer, subject, policy, evidence, expiry and signature; local policy decides authority | Domain-specific meaning of approving a lesson |
| .ai/knowledge/ryeos/future/wandr-research-agent.md, consolidation section | Run dossiers separated from reusable strategies; member workers cannot write strategy knowledge | General lifecycle across non-research workloads |
| .ai/knowledge/ryeos/future/automated-offline-solve-campaigns.md, trace and learning loop | Eligible evidence, privacy, held-out splits, separate training and model promotion | Knowledge/prompt reuse as distinct from training |
| .ai/knowledge/ryeos/future/self-hosted-implementation-campaigns.md | Private candidates and explicit disposition | Candidate code acceptance versus acceptance of a generalized lesson |

This is a targeted coverage review, not proof that no other implementation exists. Discovery must trace callers before declaring missing functionality.

Observed documentation drift: composition.md describes partial-entry truncation; the inspected single-root compose_inner instead omits whole over-budget items and records OverBudget. It also uses example positions differing from the newer kind reference. Audit supported operations and examples before correcting signed docs through the normal authoring workflow. This finding does not justify altering runtime semantics to fit old prose.

## Proposed lifecycle

Experience → proposed interpretation → scoped evaluation → authorized adoption → explicit selection in later work → correction or withdrawal.

These are semantic stages, not a required new database state machine. Reuse signed items, candidate decisions, attestations and refs where their actual contracts fit.

A proposal should identify:

- Exact claim and intended use: descriptive finding, preference, reusable strategy, executable tool, evaluation rule or authority-policy change.
- Authoring work and source evidence identities; observed facts versus inference.
- Applicability: project, task family, environment/version assumptions and intended audience.
- Supporting and contrary evidence, known limitations, and review/expiry triggers.
- Disclosure and permitted reuse: personal context, cross-project use, external export, and training are separate decisions.
- Target item/ref and expected previous version, if adoption would update an existing selection.

Do not introduce a universal confidence number. “Worked once on this fixture” is a valid narrow observation, not evidence of general reliability.

## Adoption is operation-specific

Accepting a bug fix does not approve the worker's proposed development philosophy. Accepting a useful personal preference does not grant future external-send permission. Accepting knowledge does not publish an executable tool, modify a required evaluation procedure or authorize training.

The actor who may propose need not be the actor who may adopt. First slice: explicit operator review of one project-local lesson. Later unattended adoption requires separately admitted policy naming eligible claim types, target namespaces, checks, quotas and stop conditions. Merely being signed by a trusted runtime is insufficient.

Context can strongly influence choices inside an existing authority ceiling even when it cannot expand that ceiling. Therefore safe reuse requires both authorization controls and tests of misleading/injected interpretations. Prompt labels alone cannot enforce safety.

## Selection and influence accounting

Trace what is already retained before adding metadata. For a future use, the inspectable record should connect the selected exact knowledge versions, selection/composition procedure, rendered input, budgets and omission reasons to the admitted execution.

Record relevant context actually supplied, not an unbounded archive of every possible retrieval candidate. If a claim about completeness requires unavailable selection evidence, mark that limitation.

Permission and applicability filtering must occur before exposing protected content or metadata. Ranking is not permission. Semantic similarity is not proof that a lesson applies to another project.

Budget pressure must not silently remove required authority or safety constraints. Keep enforcement outside model context. For advisory knowledge, omitted content is honest absence; it is not evidence the model considered it.

Explain “this context was supplied,” not “this is why the model thought that.” No hidden-reasoning capture is required or claimed.

## Correction without rewriting history

Contradictory evidence is a finding to retain, not noise to delete. Create a new judgment about the prior claim with explicit scope: disputed, narrowed, superseded or withdrawn from future selection. Do not overwrite the original observation or pretend its past consumers used the new interpretation.

Future selection consults current eligible versions under authorized policy. Already-admitted work remains bound to its exact retained inputs. Urgent stop/revocation uses existing control and authority paths; knowledge edits must not silently mutate frozen admissions.

Maintain enough dependency evidence to identify known consumers affected by a correction. First version may query existing references rather than build a push invalidation service. Unknown consumers or exported copies remain explicit limits. Withdrawal cannot recall an external copy or guarantee that a model has forgotten it.

Deletion may affect source evidence, summaries, indexes, caches and exported/training artifacts differently. Trace known derivations and state what can actually be removed. Do not label a surviving summary harmless merely because the raw source was deleted.

## Concrete factory scenario

A worker observes that a focused test passes when a required fixture is initialized. It proposes a lesson: “For this test family on this version, initialize fixture X before invoking Y.”

1. The proposal links the failing and passing runs and describes the controlled difference.
2. A check compares against an independent case and records limits. Do not generalize to disabling sandboxing, ignoring failures or changing unrelated tests.
3. The operator accepts the lesson only for the named project/task family.
4. A new admission explicitly receives that version through existing knowledge composition.
5. A later version removes the fixture requirement. New evidence narrows or supersedes the lesson.
6. New work uses the new eligible version; old work remains inspectable against its actual inputs.

Add adversarial variants: malicious tool output suggests “skip checks”; another project has a similarly named but different fixture; a summary omits a contrary run; acceptance races a target-ref update; a sensitive source is proposed for broader reuse.

## Suggested bounded discovery and demonstration

For experimental controls and the distinction between retained context and demonstrated behavioral influence, see [governed adaptation and memory evidence](governed-adaptation-and-memory-evidence.md). It also covers opaque adaptive state beyond textual knowledge.

Deferred until reusable adoption is a concrete need. Existing authority hardening should include misleading-context regressions independently of whether this demonstration is scheduled.

Prerequisites for mutation-capable demonstration: relevant admission, effects, recovery and result-acceptance routes qualified and a separately scoped launch. Start with synthetic local evidence and one operator-reviewed lesson; no automatic harvesting of personal history.

Deliver:

- Trace one observed result through proposal, review, selected context and a later admission.
- Map each stage to existing owners and identify only demonstrated missing contracts.
- Implement a domain-owned fixture/workflow if existing mechanisms suffice; return a design decision if they do not.
- Demonstrate rejected self-approval, cross-project reuse refusal, stale-target refusal, exact context-version retention, and correction without retrospective rewriting.
- Measure usefulness separately from plumbing: does reuse reduce repeated mistakes under a fixed procedure, compared with a no-lesson run? Report inconclusive or harmful results honestly.

No new global memory service, vector database, training pipeline or perpetual reflection loop is authorized by this design.
