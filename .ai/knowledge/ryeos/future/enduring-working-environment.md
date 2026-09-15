---
category: ryeos/future
name: enduring-working-environment
title: Enduring Working Environment — Vision and Open Questions
description: Discussion synthesis connecting the papers, principal-centered UI, portable execution, model leverage and unresolved product boundaries.
entry_type: design
version: "0.1.0"
status: discussion
---

# Enduring working environment — vision and open questions

Discussion recorded: 2026-09-07. Unsigned working note, not an implementation authorization, release claim or proof of the research programme. Its purpose is to resume the reasoning without reconstructing the conversation.

## Start here when resuming

The central interpretation is that RyeOS aims at an enduring personal and organizational working environment. People retain their projects, capabilities, delegated authority and inspectable history while compatible models, processes, clients and execution sites can change.

Portable execution is the systems foundation. The personal UI is its human-facing expression. Agents, software factories, research, messaging and shared worlds are applications, not definitions of the substrate.

The next unresolved connection is how experience becomes useful judgment without becoming ambient authority. Read [From experience to reusable knowledge](experience-to-reusable-knowledge.md), then follow the decision questions below. Do not begin by proposing another memory service or re-deriving the execution object model.

## Conversation context and user clarifications

- The comparison with OpenClaw was about the core, not line count or current connector breadth. RyeOS can acquire integrations; differences in man-hours and product breadth do not settle the architectural question.
- The software factory is intended to implement foundation work. What is deferred is direct intake of OpenClaw-inspired messaging and other integrations, not use of the factory.
- The current priority is qualifying and hardening the existing foundations, not setting up remote workers in this planning thread.
- Backward-compatible migration and release-channel engineering were explicitly deprioritized while the user is the sole operator. Current-generation recovery, bootstrap, confinement and authority correctness still matter.
- The intended UI reference is the principal-centered personal-AI end-state design dated September 5, not the narrower hosted-development UI handoff.
- This note preserves reasoning outside temporary implementation documents. It is not a request to expand that implementation backlog indefinitely.

The specific UI design is currently at `.tmp/personal-ai-ui-and-software-factory-end-state.md`. That temporary location is not a durable publication: the essential interpretation is captured below, but a future permanent UI owner should adopt the full design deliberately rather than silently treating this summary as its replacement.

## What the papers contribute

Read the [papers index](../papers/README.md) and [series map](../papers/series-map.md) for the full programme. These are working research notes.

- [White paper](../papers/portable-execution-white-paper-thesis.md): executable capability and runtime history travel together. Identity, trust, authorization, isolation and semantic correctness remain separate.
- [Execution Is an Object](../papers/execution-is-an-object.md): work is distinct from its current interpreter. This motivates durable continuity; it does not eliminate the engineering needed for recovery or effect reconciliation.
- [Testimony, Not Determinism](../papers/testimony-not-determinism.md): attributable claims and local trust are central when judgment cannot be established by simply rerunning an executor. A signature does not establish external truth.
- [The Corporate Agent](../papers/the-corporate-agent.md): commitments and delegated roles can persist through executor succession. The corporate analogy is useful without implying consciousness, legal personhood or a new implemented agent-key hierarchy.
- [Semantics of Forgetting](../papers/semantics-of-forgetting.md): preserving accountability has storage and privacy costs. Retained hashes alone do not automatically preserve every verification or authorization predicate.
- [Measurement](../papers/measurement-not-benchmarking.md): judgments should identify their procedure, subject, evidence and scope. Different evaluators may legitimately disagree.
- [Sincerity](../papers/sincerity-under-open-frames.md): diverse evaluation alone did not establish the motivating conjecture. Conditional toy-model findings are not deployed-agent safety guarantees.

The strongest novelty, impossibility and theorem language in these notes remains research material requiring separate argument and comparison. The product vision need not rely on those absolute claims.

## Synthesis: what becomes enduring, what becomes replaceable

The person should not have to rebuild their working life whenever a provider, model, laptop or remote process changes. Projects, work, accepted capabilities, commitments and evidence supply continuity. Machines provide execution resources, models supply some of the judgment, and clients expose the environment.

Replaceability is bounded: preserved logical identity does not establish compatible runtimes, equivalent model behavior, available credentials or authority to continue on a new site. Model changes may require a new admission or linked work rather than alteration of a frozen execution.

The useful model-leverage claim is larger than tool restriction: better models can strengthen a working environment whose accumulated value and standards already belong to the principal. The model does not award itself permission, establish success through prose, or decide which history is authoritative.

The distributed destination is cooperation between independently governed environments, not just remote compute. A bounded assignment can carry inspectable authority and return attributable evidence. Each recipient still applies local policy; possession of evidence does not confer continuation authority.

The human interface should express this through recognizable projects, work, correspondence, evidence and decisions. Home/Projects/Work/Messages/Sites are views into that environment. Workers and placements are execution details, not the enduring people or entities the UI revolves around.

## Why the factory is a strong first consumer

Developing RyeOS exercises admission, private workspaces, bounded tools, remote custody, restart recovery, checks and explicit disposition together.

But self-hosting must not become self-authorization. The installed host governs the candidate that proposes its replacement. Candidate success does not authorize deployment or alteration of the acceptance procedure.

An additional boundary: accepting the code does not accept every lesson the worker inferred. A reusable technique, prompt instruction, evaluator change and capability-policy change are separate proposals with separate scope and disposition.

Existing owner: [self-hosted implementation campaigns](self-hosted-implementation-campaigns.md). Deployment remains with [reflexive deployment](reflexive-deployment.md).

## What was found, rather than merely proposed

The documentation already contains significant pieces of a learning lifecycle:

- Knowledge kinds and runtime docs cover bounded composition; the runtime has selection/omission metadata and verified inputs.
- [WANDR](wandr-research-agent.md) separates run/member dossiers from reusable strategies and forbids member workers from authoring strategy knowledge directly.
- [Offline solve campaigns](automated-offline-solve-campaigns.md) separate observed evidence, permitted corpus use, training, held-out evaluation and explicit model-profile promotion.
- The attestation object distinguishes a signed claim from the local policy that makes it authoritative.

The gap identified in this review is a shared account of adoption, eligibility, correction and reuse across those pieces—not proof that all relevant machinery is missing. Source pointers and a concrete composition-doc drift finding are preserved in [the knowledge lifecycle note](experience-to-reusable-knowledge.md).

No runtime qualification was run for this synthesis. Historical status assertions in future docs must be re-audited before implementation.

## Decision questions to carry forward

### 1. Ownership after loss or compromise

“This remains mine” needs a concrete recovery story for lost keys, devices and nodes. Old evidence verification, current authority to resume and successor identity are different questions.

Owner: [key lifecycle](key-lifecycle.md), with current core identity and hosted trust contracts. Next step: a sole-operator recovery table identifying surviving keys/backups, recoverable content, permissible actions and irrecoverable cases. Do not auto-regenerate identity or invent recovery authority. Pull forward before making loss/rotation continuity claims, not as a broad PKI programme.

### 2. Human identity versus signing and placement coordinates

The paper's uniform key abstraction is not the entire current runtime identity model. Operator, node, publisher and vault roles are distinct. Show who is acting and under what role without asking the user to constantly reason in fingerprints.

Next step: trace one local action, one forwarded action and one delegated child through the UI. Identify enduring principal/project/work identity separately from browser session, model and placement. Do not add a new agent identity service to satisfy the analogy.

### 3. Intent survives longer than permission

A retained goal can remain unfinished after its budget, deadline or mandate expires. It should become dormant rather than authorize infinite retries.

Owner: campaign and scheduler contracts. Next step: specify cancellation, dormancy, renewal and changed acceptance criteria for one workflow. Reconcile in-flight effects separately from stopping future work. Pull forward before persistent assistants or recurring autonomous work renew their own authority.

### 4. Experience must not silently rewrite the rules

Evidence, interpretation and authority must connect without collapsing. A model's “Leo usually approves” is an inference, not permission.

Owner of the proposed connection: [experience to reusable knowledge](experience-to-reusable-knowledge.md). First example: a factory lesson about a test fixture, explicitly adopted for one task family, later corrected without rewriting old admissions. No perpetual reflection loop is required.

### 5. Evaluation has provenance and interests

Different signatures do not establish independent review. Reviewers can share a model, flawed test, selected evidence or incentive. Signed reports can omit unfavorable attempts.

Next step: bind required procedures before implementation, identify evaluator dependence and declared evidence completeness, and preserve conflicting judgments about the same candidate. Do not promote a universal score or claim that signed evidence makes gaming impossible. Pull forward when selecting executors or claiming improvement.

### 6. Derived knowledge can remain private

Read permission is not permission to publish a summary, reuse it across projects, send it to a model provider or train on it. Derivation does not automatically remove sensitivity.

Owners: corpus policy, retention/export and hosted boundaries. Next step: trace one synthetic private source through summary, index, export and deletion. Identify what remains inferable and what copies cannot be recalled. Do not turn accountability into a requirement to collect all personal activity forever.

### 7. The person must be able to change

Keep the historical record honest while allowing present judgments to be revised. A withdrawn lesson, expired preference or changed role need not erase the past or bind the person forever to it.

Next step: distinguish historical attribution, current applicability and present authorization in one correction UI. Forgiveness and evaluation horizons are policy choices with tradeoffs, not solved by the sincerity toy model.

### 8. Review load can defeat governance

If every useful action requires deciphering many receipts, people may rubber-stamp decisions. That is a product failure, not permission to hide uncertainty.

Next step: observe a connected work/recovery/review task and measure decisions required, time to locate a failure, and ability to distinguish completed/checked/accepted. Use existing policy-backed batching only where scope permits. The UI goal is an understandable decision, not maximum evidence on screen.

### 9. Publish the portability envelope, not “runs anywhere”

Record what survives a move and what changes: execution identity, local trust, resources, credentials, evidence strength and compatible next actions.

Owners: [substrate growth roadmap](substrate-growth-roadmap.md), portable handoff and evidence-export designs. Qualify explicit routes before generalizing to independent federation.

## Where to resume the conversation

### ARC downstream grounding and the OpenClaw comparison

Follow-up inspection on 2026-09-07 found concrete domain versions of these
ideas already in ARC-2 and ARC-3: independently authorized solve, learning and
implementation lanes; fixed generations; evidence-gated promotion; and
project-owned evaluation. The general knowledge lifecycle should learn from
these contracts, not replace them.

ARC-3 already distinguishes GAP (missing decision-time knowledge) from DELEGATE
(a justified goal requiring controller computation), and curated transfer from
runtime game memory. ARC-2 explicitly allows bounded within-task adaptation
while forbidding cross-private-task leakage. These are complementary tests:
useful transfer where permitted, enforced non-transfer where required.

Downstream discussion/qualification owners:

- `/home/leo/projects/arc-agi-2/.ai/knowledge/arc2/future/governed-improvement-and-qualification.md`
- `/home/leo/projects/arc-agi-3/.ai/knowledge/arc3/implementation/future/governed-improvement-and-qualification.md`

The OpenClaw comparison must remain architectural and bounded. Its local
checkout at commit `5b9246ceec5` documents gateway-owned cloud-session
transcripts and inference/auth custody, disposable execution, immutable
workspace checkpoints, scoped credentials and owner epochs. The inspected
`src/worker/worker-connection-admission.ts` and worker/node contracts contain
owner-epoch checks; do not describe OpenClaw as lacking distributed control or
code-enforced policy.

Local sources: `../openclaw/docs/start/why-openclaw.md` and
`../openclaw/docs/gateway/cloud-workers.md`, relative to the RyeOS repository.
Those contracts do not themselves establish the full ARC experiment,
adaptation and promotion semantics. Building that layer around OpenClaw is
possible; this review does not prove the absence of every analogous feature
throughout its codebase.

The defensible RyeOS hypothesis is lower architectural mismatch for these
workloads: exact admitted programs, durable graph/effect identities, explicit
generation transitions and portable custody are already the intended common
substrate. ARC owns task meaning and scientific validity in either case.
Whether that fit produces a better working system remains an empirical
qualification question, not a conclusion from line count or this design review.

Follow-on discussion: [governed adaptation and memory evidence](governed-adaptation-and-memory-evidence.md) distinguishes recall, adaptive updates and continuation; proposes controlled experiments; and reviews stronger claims about keys, caching and causal history.

Start with the factory-learning example, not another abstract architecture discussion:

1. A bounded worker observes a useful pattern.
2. It proposes a narrow lesson with supporting and contrary evidence.
3. An authorized decision makes the lesson eligible for a named future use.
4. New work records the exact context supplied and keeps authority separate.
5. Later evidence corrects the lesson while earlier work remains interpretable.

Ask which existing owner handles each step, which decision is missing, and what the person needs to see. Then choose the smallest demonstration that answers those questions.

The overarching design test: preserve continuity without freezing judgment. History remains attributable; understanding can change; neither silently becomes permission.

## Ownership and scope

This note owns the discussion synthesis and resume questions, not the existing mechanisms it references. The knowledge lifecycle note owns the proposed cross-cutting adoption/correction semantics. Temporary hardening documents may consume these notes but must not become their sole storage location.

Do not silently rewrite the papers, signed current contracts or implementation owners to match this interpretation. Promote reviewed decisions through the normal authoring/signing workflow when their concrete trigger arrives.
