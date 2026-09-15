---
category: ryeos/future
name: governed-adaptation-and-memory-evidence
title: Governed Adaptation and Evidence of Remembering
description: Discussion review separating durable memory, continuation, adaptive updates, causal experiments and cryptographic claims.
entry_type: design
version: "0.1.0"
status: discussion
---

# Governed adaptation and evidence of remembering

Discussion recorded: 2026-09-07, following a user-supplied shared conversation about frozen models, local inference, identity, caching and machine memory. Unsigned discussion note, not implementation authorization or empirical results.

Read alongside [the enduring environment synthesis](enduring-working-environment.md) and [experience to reusable knowledge](experience-to-reusable-knowledge.md). This note owns the evidence distinctions and proposed experiment; it does not replace the existing inference, capsule or training owners.

## What the shared conversation adds

Its strongest proposal is governed plasticity: changes to knowledge, tools, retrieval procedures or adaptive model state become attributable candidates with scoped evaluation and explicit adoption.

Its strongest demonstration is an intervention: fresh execution with retained experience versus matched execution with that experience withheld. That distinguishes a record claiming memory use from evidence of a behavioral effect.

Its strongest caution is that durable state can preserve poisoning and false beliefs as readily as useful knowledge. Cryptographic integrity is not epistemic quality.

Do not inherit its early claims that a key proves personal experience, hash ancestry proves physical causation, local inference is automatically verified, or a signed cache establishes learning. The later corrections improve the conversation but do not settle every boundary.

## Distinctions to retain

| Question | Appropriate evidence | What it does not establish |
|---|---|---|
| Was this content retained? | Exact retained bytes and integrity checks against a trusted commitment | Truth, usefulness or use by a model |
| Who endorsed a claim? | Valid signature and issuer binding under explicit key assumptions | Human identity, exclusive custody or honest testimony |
| How are records related? | Verified parent links and selected-head/branch evidence | A complete unique history or physical causation |
| Was context supplied? | Exact admitted/rendered input and observation evidence | That the model relied on it |
| Did retained state affect behavior? | Controlled intervention with confounds addressed | General competence or improvement |
| Did the system learn usefully? | Durable adaptation plus held-out benefit under a named procedure | Universal intelligence or safety |
| Did computation continue equivalently? | Matching capsule/profile and qualified interrupted-versus-uninterrupted tests | Knowledge acquisition or semantic correctness |

These are separate claims, not one ladder where accumulating signatures eventually proves meaning.

## Functional memory is not conditional on cryptography

Stored information can affect future behavior without signatures, and reconstruction from a durable trace can constitute functional remembering. RyeOS's opportunity is to make scope, provenance, selection, custody and governance more explicit and portable—not to make all other memory unreal.

Use working experimental definitions, not universal definitions of mind:

- Retention: an experience-derived representation remains available after the originating process.
- Remembering: later behavior demonstrably depends on retained information under the tested conditions.
- Adaptation: experience changes persistent state or a processing procedure.
- Useful learning: that adaptation improves a declared task measure, with the intended generalization tested separately from recall.
- Authorized adoption: policy permits a candidate change to become active for a named use.

Authorization is not part of the definition of learning. Unwanted, harmful or unauthorized adaptation can occur. Otherwise the definition hides exactly the poisoning problem governance must address. Recall, transfer and improvement are distinct results; learning need not be universally beneficial.

## Learning and prioritization share a boundary, not an identity

Both raise the question of which inputs may influence future action. But admission cannot decide every semantic question, and a capability ceiling cannot ensure the best action among allowed actions.

Distinguish:

1. Enforcement priority: independent cancellation, authorization and process controls.
2. Task priority: scheduling and budget allocation among admitted goals.
3. Epistemic judgment: what evidence should change a belief or strategy.

A model may help with the latter two without becoming the authority for the first. “Confirm before deleting” needs an actual action gate; placing that sentence in context is not equivalent.

Cancellation has a linearization point and physical limits. Record request, dispatch fence, acknowledgement, process termination and unresolved effects separately. STOP can prevent subsequent dispatch without undoing a remote mutation already accepted. Inability to reach a remote node is not proof it stopped.

## Local inference and four different kinds of retained state

Local inference can expose exact model/runtime/artifact inputs and make interventions more controllable. It does not automatically close every output-selecting dependency, enforce isolation or prove a recorded computation ran.

The inspected [sealed local inference design](sealed-local-inference.md) explicitly retains a recorded baseline and requires separate exact-scope qualification. Its status is a document assertion, not a fresh qualification result from this review.

Keep four mechanisms distinct:

- Provider-effect replay: return the recorded outcome at its exact admitted coordinate, without fresh model contact.
- Generation capsule reuse: restore provider-compatible computational state, recorded or separately qualified.
- Knowledge retrieval: select artifacts as context for a new execution.
- Adaptive update: change weights, an adapter, learned memory, retrieval policy or other behavior-bearing state for later processing.

The [provider-record owner](provider-call-effect-records.md) explicitly forbids semantic and cross-coordinate reuse. Equal-looking prompts do not justify transferring results across changed authority, route, profile or realization. New evidence or a correction can require a new request; replay answers what was previously recorded, not what is true now.

The [capsule owner](generation-state-capsules.md) already distinguishes recorded continuation from byte-equivalent qualified continuation. Do not add a parallel InferenceCheckpoint schema because the discussion proposes that name. Generation forks are also not identity succession or permission to run two writers in one custody lineage.

## Opaque state is an information channel

A checkpoint, adapter or neural memory can retain private information or adversarial influence that is absent from a textual summary. Restoring it must not restore expired capabilities or silently revive withdrawn context.

After a policy or evidence correction, identify all known behavior-bearing derivatives: rendered prompts, retrieval indexes, capsules, adapters and model generations. Changing a ref does not cleanse an already-loaded tensor.

The provider owns compatibility and state interpretation; generic storage owns closure, identity, scope and retention. On an incident, possible dispositions include refusing restore, reduced-authority execution, or fresh reconstruction from eligible artifacts. Which is valid must be explicit; none guarantees semantic unlearning.

Candidate weight rollback is not reversal of external effects, removal of leaked secrets, or proof of forgetting. Do not call the whole system reversible because an artifact ref can move back.

## Epistemic provenance without a universal truth engine

Keep source origin, inference and disposition distinguishable:

- sensor bytes versus an interpretation of those bytes;
- another party's report versus corroborated evidence;
- a simulated result versus a physical observation;
- an intention, dispatch, external acceptance and confirmed effect;
- a source record, a derived summary and a currently endorsed claim.

These are not necessarily exclusive enum values. One claim can be an inference over reported sensor observations. A sensor signature also does not prove calibration, absence of spoofing or the interpretation's truth.

Use domain-owned schemas and linked evidence where needed. Do not put a universal observed/reported/inferred ontology or an uncalibrated confidence scalar into the generic kernel merely to make the distinction structural.

An agent may say “this claim is attributed to this execution in the retained record.” It must not turn that into “I experienced it” solely because it possesses a matching key. Copies, compromise, delegations and different branches affect what that statement means.

## Proposed experiment: retained experience and behavioral dependence

Start with ordinary recorded execution, not a dependency on sealed inference or an external transparency service.

### Stage A: recall after process replacement

1. An independent fixture generates random task-specific mappings after the model artifact is fixed. Keep answer material out of common prompts, filenames, metadata and evaluator feedback.
2. Prepare a shared baseline and record treatment assignments before outcomes are inspected.
3. Create separately authorized experimental runs: one receives an admitted experience artifact; one receives a matched neutral artifact; a third may receive a deliberately wrong mapping to test sensitivity.
4. Stop original processes and start fresh ones. Clear or isolate all candidate leak channels: live context, provider sessions, caches, shared homes, indexes and network access, according to the actual threat model.
5. Use the same model/runtime/profile and equivalent challenge conditions within each comparison. Supply new questions without including the answers.
6. Run many independent mappings, randomize order and retain all declared trials, failures and retries. Score using an independent deterministic fixture, not the tested model's claim of remembering.
7. Report success rates, paired differences and uncertainty under the declared sampling procedure. A single lucky answer is not evidence of reliable memory.

Fresh local inference must actually occur when the experiment claims it: distinguish provider-record replay from a model call using the retained artifact. Hashes or a receipt saying “used memory” are not the outcome measure.

No control can prove absolute absence from all possible prior information. Random generation, isolation and leak checks support a bounded empirical claim with residual assumptions.

### Stage B: interpreter replacement

Repeat the within-model treatment/control comparison with a second compatible model in new admissions. This tests portability of the retained knowledge across interpreters. Do not compare treatment on one model to control on another and attribute the difference solely to memory.

### Stage C: transfer and correction

Test an acquired strategy on held-out instances, with before/after and no-update controls. Then introduce valid contradictory evidence and test scoped correction without changing old records or leaking held-out answers into the update procedure.

### Stage D: adaptive machinery

Only if the task demonstrates value, use the existing corpus/training/promotion design to test an adapter or learned-state candidate. Separate replay, retrieval and parameter-update effects through matched ablations. Record regressions and resource cost as well as the target benefit.

Exact inference-continuation tests belong to the capsule qualification suite; they are complementary experiments, not prerequisites for functional memory.

## External anchoring: optional claim strengthening

A self-signed timestamp does not prove wall-clock age. An independently witnessed commitment can establish that particular committed material existed by the witnessed point, subject to the witness and protocol assumptions.

It does not establish truth, completeness, exclusive history, or that the committed material caused an answer. A local controlled experiment can begin without public anchoring; add independently verifiable ordering when making third-party non-retrofitting claims.

Public raw memory or low-entropy fact hashes can disclose information. Prefer a reviewed privacy-preserving commitment protocol if external publication is ever authorized. A peer signature is not automatically an independent witness.

Sigstore documents trusted timestamp authorities and Rekor inclusion time as timestamp mechanisms: [timestamp documentation](https://docs.sigstore.dev/cosign/verifying/timestamps/). This is a reference pattern, not a proposed RyeOS service dependency.

## Map proposed objects to current owners first

| Name proposed in shared chat | Start from | Decision still needed |
|---|---|---|
| LearningUpdate | Candidate artifacts, corpus/training lineage, checks and explicit promotion | Domain-specific update/provenance contract; no automatic new core kind |
| MemoryUseReceipt | Rendered-context metadata, admitted inputs and execution observations | What survives and what establishes supplied-versus-used evidence |
| InferenceCheckpoint | Generation-state capsules | Provider payload/compatibility and qualification |
| EpistemicClaim | Attestations plus domain evidence schemas | Claim semantics and admissible corroboration |
| StateAnchor | Evidence export, external witnessing and key-lifecycle designs | Need, privacy, witness assumptions and verification protocol |

Owner links: [offline solve campaigns](automated-offline-solve-campaigns.md), [execution identity](execution-identity.md), [provider records](provider-call-effect-records.md), [key lifecycle](key-lifecycle.md), and [knowledge adoption](experience-to-reusable-knowledge.md).

## The video claim and scope of review

The supplied video's claim that ordinary frozen inference does not update base weights is distinct from the assertion that generative systems cannot adapt. Research such as [Titans](https://arxiv.org/abs/2501.00663) explicitly studies neural memory that learns historical context at test time. That is evidence of an architectural research direction, not a solution to safe lifelong learning or a capability already implemented in RyeOS.

The quoted incident and broader historical claims were not independently verified here and are not implementation evidence. No claim of unique invention, consciousness or human-equivalent learning follows from the proposed experiment.

## Resume point

Ask: what is the smallest task on which an admitted experience changes later behavior, survives process replacement, can be withdrawn or corrected, and never expands action authority?

That experiment connects the papers' measurement programme, local inference, knowledge adoption and the factory without requiring a new theory of self or five speculative substrate objects.
