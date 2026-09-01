<!-- ryeos:signed:2026-09-01T22:50:22Z:3ca2f399de684a3797f41ae34e26f218da756e1cd16800a03a2cfc9414260dc1:jduSaBndl2Gu8j4ZFbsz3GIOX0NS4aUCsas7inR1X7wZgtK4UKXHHiofAdjjq5VlkSTo/sGkE0aUVUVzTg6hAw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: ryeos/future
name: automated-offline-solve-campaigns
title: Automated Offline Solve Campaigns
description: >-
  End-state design for hosted implementation, directive-native local
  reasoning, graph-managed search, evidence-driven learning, and
  network-independent solve execution
entry_type: design
version: "0.1.0"
```

# Automated offline solve campaigns

## Status

This is a future end-state and sequencing document. It connects the landed
hosted-worker, portable-placement, managed-content, persistent-session, graph,
directive, effect-record, and execution-evidence substrates to the active
local-inference direction.

It does not claim that directive-native local reasoning, a serious remote
model profile, trace-to-training automation, or final offline qualification is
already complete. Those capabilities remain separately admitted increments.

## Purpose

RyeOS should eventually execute a complex solve campaign from an admitted goal
to an authoritative result without requiring an initiating client, hosted
frontier worker, or operator to remain attached.

The complete system combines three related but independently authorized loops:

```text
implementation loop
  hosted frontier worker
    -> improve RyeOS, project programs, tools, and evaluations
    -> build and test a private candidate
    -> return a frozen candidate for explicit disposition

solve loop
  campaign graph
    -> durable directives
    -> admitted local inference
    -> separately admitted RyeOS tools
    -> evaluation, branching, comparison, and stopping
    -> result, exhaustion, or typed failure

learning loop
  admitted execution evidence
    -> provenance-complete corpus
    -> recorded tinygrad training
    -> held-out comparison
    -> explicitly promoted model generation
```

The implementation loop bootstraps and improves the other two. The solve loop
is the independently operable product. The learning loop progressively moves
repeatable work from frontier implementation workers into exact local
execution.

## End-state topology

The first useful topology uses explicit placement rather than a general
scheduler:

```text
operator/client node
  |-- admits goals, budgets, policy, and publication decisions
  |-- attaches to stable execution chains and evidence
  `-- may disconnect after accepted launch

capable execution node
  |-- hosted implementation worker in a private project candidate
  |-- exact local-inference persistent session
  |-- graph/directive/tool execution
  `-- trace, comparison, and training artifact retention

offline qualification node
  `-- exact exported solver/model/runtime closure with no network dependency
```

Later federation may place inference, training, evaluation, and implementation
work on different admitted sites. Placement changes must preserve the same
program, content, lineage, consequence, and evidence contracts. It must not
introduce a second agent, model, checkpoint, or execution substrate.

## Campaign admission

An automated campaign begins from signed project meaning and explicit node
policy. Before thread birth, RyeOS must know the complete admitted closure that
can affect the run:

- project generation and problem or environment inputs;
- campaign, solver-graph, directive, and tool programs;
- selected local-model route and exact inference profile;
- tinygrad worker, runtime, compiler, tokenizer, template, and model content;
- simulator, dataset, verifier, or evaluator content where required;
- target execution realization and resource requirements;
- search, evaluation, retry, approval, and stopping policy; and
- bounded turns, tokens, tool calls, wall time, retries, children,
  concurrency, response/artifact bytes, and retained workspace growth.

An undeclared model, tool, content realization, target, or policy refuses
before execution contact. Caller parameters cannot select a different model,
worker environment, executable, or authority.

Once the launch is durably accepted, the root identity is returned before a
gateway timeout can hide it. The campaign continues when every initiating
client disconnects.

## Directive-native reasoning

The local model is a model execution service, not an agent workstation. It has
no project shell, developer-tool search path, signing authority, daemon socket,
operator credential, or arbitrary RyeOS command surface.

An ordinary directive owns the durable reasoning loop:

```text
existing directive continuation/event boundary
  -> canonical admitted model request
  -> executed or replayed model response
  -> structurally validated proposed tool call
  -> separately admitted RyeOS tool child
  -> bounded tool observation
  -> next continuation/event boundary or terminal
```

The directive owns context construction, model-response interpretation,
reasoning state, tool proposal validation, usage, and terminal meaning. The
inference worker owns model-family execution, tokenization, sampling, and
model diagnostics. Concrete reads, mutations, builds, tests, simulations, and
evaluations remain ordinary RyeOS tool effects.

The durable boundary is the existing predecessor and event chain, optionally
joined to an existing project state anchor or private project generation. This
design does not add a second directive checkpoint object or hidden agent-state
registry.

## Tool and mutation authority

Model output is a proposal, never an effect by itself. RyeOS validates the
typed proposal and admits the selected tool under its own program, workspace,
approval, accounting, and recording authority.

A directive may:

- read admitted project or environment state;
- stage a mutation inside its private project generation;
- dispatch deterministic search, build, test, simulator, or verifier tools;
- retain bounded observations;
- compare candidate results; and
- return a typed solution, candidate, exhaustion result, or failure.

It may not silently publish a project generation, install RyeOS, advance a
selectable model, or bypass an approval boundary. Candidate selection inside a
campaign remains distinct from active project or model promotion. Promotion is
a separate signed consequence unless a narrowly typed policy explicitly
pre-authorizes it.

An unattended approval-required action either has already admitted authority
or pauses durably for approval. It never hangs indefinitely and never converts
absence of approval into permission.

## Graph-managed search

A graph owns the larger campaign rather than hiding search inside the model
process:

```text
campaign root
  |-- enumerate due problems or states
  |-- create bounded solver attempts
  |-- fan out strategies, hypotheses, roles, seeds, or branches
  |-- follow authoritative child terminals
  |-- require complete evaluation cohorts
  |-- compare and select/report candidates
  |-- retry or branch under admitted policy
  `-- stop with solution, exhaustion, cancellation, or typed failure
```

Every child retains its exact project generation, model route, budgets, and
parent lineage. Parent collection remains bounded and references large child
evidence rather than embedding cumulative terminal state.

Failed, missing, cancelled, or malformed children cannot be treated as
successful empty results. Partial cohorts cannot author final evaluations,
classifications, reports, or promotion consequences.

## Recovery and unattended operation

The campaign must survive daemon, worker, client, and node interruption at
every durable boundary. Acceptance covers at least these crash windows:

- model effect published before directive continuation;
- proposed tool retained before child birth;
- tool child born before its identity is folded;
- tool settled before its observation is folded;
- continuation committed before the next model request;
- graph suspended before followed-child terminal delivery; and
- candidate/report staged before terminal publication.

Recovery preserves stable request, tool-child, follow, and consequence
identities. It neither duplicates a model call or tool mutation nor loses an
already settled result. A process restart reconstructs work from retained
authority rather than asking the model to recreate missing conversation state.

Budget exhaustion, cancellation, refusal, approval pause, and search
exhaustion are typed durable outcomes. They are not transport errors and are
not converted into automatic relaunches.

## Recording, replay, and branching

Every exact local-model request is an ordinary RyeOS effect. The first
execution contacts the admitted persistent worker and device, then publishes
one immutable record. An identical effect validates and replays that record
before worker reservation, process start, model load, or device contact.

The effect coordinate includes the effective caller and provider program,
exact model/profile/worker dependency program, canonical request, sampling and
trace policy, admitted execution realization, and exact accounting/effect
authority. It excludes unrelated project snapshot movement, host paths,
activation receipts, pool slots, process identities, leases, and cache
locations.

Recorded replay makes repeated experiments economical and comparable:

- identical model work is reused without hidden recomputation;
- new prompts, policies, tools, models, or execution realizations remain new
  effects;
- campaign restarts do not repay for settled model or simulator work; and
- execution comparisons can identify exactly which coordinate moved.

Campaigns may branch from shared admitted state. Initially each continuation
is an ordinary recorded execution. If measurements justify generation-state
capsules, recorded capsules may later provide exact-prefix reuse, bounded
park/resume, or explicit fork behavior. They prove retained-state integrity
and compatibility, not uninterrupted equivalence. A sealed-equivalence claim
requires separate positive qualification.

## Hosted implementation loop

Hosted frontier workers remain important during bootstrap and ongoing
improvement. They are RyeOS executions and may run as direct, followed, or
detached workers under graph control.

A hosted implementation worker may:

- inspect exact campaign evidence and generic failure boundaries;
- modify RyeOS or project programs in a private copy-on-write candidate;
- add or improve deterministic tools, directives, graphs, and evaluations;
- implement model-family or tinygrad support;
- run bounded builds, focused checks, reviews, and disposable-node probes; and
- return a frozen candidate with complete evidence.

The installed RyeOS generation remains authority over the campaign proposing
its successor. The worker cannot sign, install, publish, restart, reconfigure,
or replace its host. Operator or release authority explicitly disposes of the
candidate before another generation becomes active.

If a solve exposes a generic substrate defect, the solve freezes at the exact
evidence boundary. A separately project-bound implementation campaign fixes
and qualifies RyeOS, the operator installs the accepted generation, and the
solve resumes from admitted durable state without a project-side substrate
workaround.

## Evidence and execution field

The event and artifact braid must make long unattended work understandable
without global or sideways discovery. The execution field should expose:

- stable root and continuation lineage;
- current placement and recovery state;
- selected project, solver, model profile, and execution realization;
- executed versus replayed model and tool effects;
- graph fan-out, follow, cohort, retry, and stop state;
- turn, token, tool, time, storage, and concurrency budgets;
- approvals, cancellations, refusals, and exhaustion boundaries;
- bounded trace and artifact references;
- candidate and evaluation comparisons; and
- final result, report, or submission artifact.

Large traces and child state remain content-addressed and lazily inspectable.
Primary thread events carry bounded summaries and references, not cumulative
serialized execution history.

## Trace and learning loop

Learning uses only admitted observable evidence:

- canonical prompts and model responses;
- explicitly exposed model reasoning or summaries;
- proposed tool calls, actual tool effects, and observations;
- successful, failed, corrected, and preference trajectories;
- deterministic verifier and evaluation outcomes;
- model token/sampler diagnostics where explicitly enabled; and
- resource and performance evidence.

RyeOS does not infer or claim access to hidden frontier-model chain-of-thought.
Recorded provider or hosted-worker output does not automatically become
training data.

A project-owned corpus graph selects eligible evidence, applies permitted-use
and privacy policy, redacts secrets, deduplicates by task family and semantic
coordinate, assigns train/validation/test splits before transformation, and
publishes one immutable dataset realization. Blind or held-out evaluation data
must not enter the training closure.

A separate graph-orchestrated tinygrad training execution consumes exact base
weights, dataset, program, recipe, seed, target realization, and resource
policy. It stages immutable checkpoint/model artifacts, then a separate
held-out graph compares the candidate with its parent. Only explicit signed
project or operator authority creates a newly selectable model profile.

The intended flywheel is:

```text
frontier-guided implementation
  -> better local solve trajectories
  -> admitted distillation evidence
  -> stronger local model generation
  -> broader and cheaper local campaigns
  -> harder, better-localized failures
  -> next implementation or training improvement
```

## Remote placement and later federation

The initial production path selects one capable remote site explicitly. The
project workspace, serious local model, and hosted implementation worker may
co-reside there while retaining separate program and credential authority.

Later measured demand may place work by admitted capability:

```text
directive inference   -> model-capable site
recorded training     -> accelerator site
evaluation cohorts    -> bounded parallel sites
hosted implementation -> credential- and tool-capable site
offline qualification -> network-independent target
```

Federation must preserve stable execution lineage while each placement retains
its own node, thread, boot, policy, resource, credential, and activation
testimony. It transfers exact closure and continuation authority rather than
ambient directories or credential bytes.

A general scheduler, replicated active process, or public multitenant fabric
is not required for the first automated solve campaign.

## Final offline closure

The solve-ready export contains:

- exact project state and problem inputs;
- campaign, directive, graph, tool, verifier, and evaluator programs;
- selected local-inference profile and provider route;
- tinygrad worker, runtime, compiler, tokenizer, template, and model content;
- required simulator or dataset content;
- target execution requirements and managed-activation declarations;
- bounded qualification evidence and verification profile; and
- trust, signer, manifest, and program testimony.

It contains no hosted frontier worker, remote-provider credential, installed
assembler, mutable vendor path, live import root, or network dependency.

On a fresh offline target with no matching effect record, RyeOS activates the
exact cached artifacts and completes one solver-facing run with positive local
worker/device contact. Repeating the identical coordinate then returns from
the immutable effect record with zero worker/device contact. This proves both
fresh offline execution and exact replay; a replay-only acceptance is
insufficient.

Training, sealed inference, and generation-state capsules are not prerequisites
for solve-ready offline execution. They remain later improvement and stronger-
qualification tracks.

## Operator experience

The intended user experience is to admit a bounded campaign, disconnect, and
return later to authoritative progress rather than babysitting processes.

The operator should be able to see:

- which problems or environment states are pending, active, solved, exhausted,
  failed, cancelled, or awaiting approval;
- which strategies and model generations were attempted;
- which work executed and which replayed;
- current budgets, throughput, resource use, and estimated remaining work;
- exact failure, recovery, or divergence boundaries;
- candidate comparisons and complete cohort status; and
- final reports and output artifacts.

The same stable chain may be attached from the local node while execution
continues remotely. Client attachment changes observation and control, not
execution identity or ownership.

## Meaning of full automation

“Fully automated” means that, after an admitted campaign launch, RyeOS owns:

- validation and exact closure admission;
- placement and content activation;
- model execution and replay;
- typed tool dispatch;
- search fan-out and collection;
- budgets, retries, approvals, cancellation, and stopping;
- restart and placement recovery;
- evaluation and candidate reporting; and
- authoritative result and evidence publication.

It does not mean unrestricted self-modification. Installed RyeOS activation,
publisher and operator signing, credential enrollment, node policy changes,
project publication, and model promotion remain explicit authority boundaries.

The design seeks maximum unattended execution inside an admitted program while
preserving control over which programs, models, effects, and generations may
become authoritative.

## Delivery sequence

The shortest coherent route is:

1. finish installed qualification of managed local-inference activation;
2. prove one deterministic directive/model/tool continuation fixture;
3. prove a separate live bounded local-model directive trajectory;
4. establish reusable signed model-profile composition;
5. admit one serious tinygrad model on an explicitly selected capable site;
6. bank, restart, and zero-contact replay a real model request;
7. project bounded traces and execution evidence;
8. run one graph-managed unattended campaign with complete recovery and stop
   semantics;
9. export and freshly execute the recorded base-model solve closure offline;
10. add corpus, recorded training, model comparison, and explicit promotion;
11. add recorded generation-state reuse only when measurements justify it; and
12. qualify sealed inference or capsule equivalence only for exact scopes that
    positively prove those claims.

Useful solve work must not wait for a general scheduler, full federation,
sealed training, or a complete self-improvement loop.

## Relationship to adjacent designs

- `knowledge:ryeos/future/self-hosted-implementation-campaigns` owns the
  hosted worker producing a frozen implementation candidate.
- `knowledge:ryeos/future/local-execution-roadmap` owns the local tinygrad
  inference, recorded execution, training, and offline qualification sequence.
- `knowledge:ryeos/future/provider-call-effect-records` owns immutable model
  effect recording and replay evidence.
- `knowledge:ryeos/future/generation-state-capsules` owns optional recorded and
  sealed model-state continuation.
- `knowledge:ryeos/future/substrate-growth-roadmap` owns the broader sequencing
  across hosted execution, local inference, self-hosting, and federation.
- `knowledge:ryeos/future/distributed-substrate-deferred-advanced` owns later
  federation and scheduling work beyond explicit placement.

This document owns how those capabilities compose into one automated,
network-independent solve system.

## Explicit non-goals

- embedding downstream project names or domain semantics in generic RyeOS;
- giving the local inference worker a shell or developer-tool environment;
- executing model-proposed commands without typed RyeOS tool admission;
- making hosted frontier execution part of the final offline closure;
- treating hidden chain-of-thought as available evidence;
- automatically training on every retained execution;
- automatically promoting project or model generations without admitted
  authority;
- treating recorded execution as sealed or local output as correctness proof;
- requiring Bubblewrap for semantic activation, recording, or replay;
- claiming disabled isolation is hostile multitenant confinement;
- implementing a general scheduler before explicit placement is insufficient;
- running two active replicas of one stateful solve or worker; or
- introducing parallel agent, trace, cache, checkpoint, model, or federation
  substrates.

## Acceptance

The end state is accepted when:

1. a serious local tinygrad model runs from exact admitted content on the
   selected capable site and banks/replays across restart;
2. an ordinary directive completes a useful multi-turn model/tool trajectory
   across crash cuts without duplicate model or tool effects;
3. a bounded graph-managed campaign continues without clients, survives daemon
   and placement interruption, accepts only complete authoritative cohorts,
   and terminates with inspectable result or exhaustion evidence;
4. a hosted implementation worker can improve and qualify RyeOS or project
   candidates remotely without self-install or publication authority;
5. admitted trajectories can produce an immutable corpus, recorded tinygrad
   successor model, held-out comparison, and explicitly promoted profile;
6. the selected project/model execution closure performs a fresh run on a
   network-independent target and then proves zero-contact replay; and
7. the operator can attach locally or remotely and understand exact progress,
   budgets, effects, recovery, comparisons, and final artifacts through normal
   RyeOS execution surfaces.
