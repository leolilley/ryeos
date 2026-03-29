```yaml
id: residual-stream-and-native-model-family
title: "Residual Stream Artifacts & Native Model Family"
description: Embedding-level cross-thread reasoning transfer, a decomposed model family native to RYE, and a tinygrad-based training pipeline fed by verified execution traces
category: future
tags:
  [
    residual-stream,
    embedding-artifacts,
    tinygrad,
    model-family,
    training,
    intent-router,
    preloader,
    reasoning-engine,
    trace-collection,
  ]
version: "0.1.0"
status: design-proposal
```

# Residual Stream Artifacts & Native Model Family

> **Status:** Design Proposal — long-horizon vision grounded in existing RYE infrastructure. Not scheduled for implementation. Builds on [Memory & Intent Resolution](memory-and-intent-resolution.md).

## Executive Summary

Current agent frameworks — including RYE running on frontier models — face a fundamental ceiling: the model is doing too many things in a single forward pass. It reasons about the problem, decides which tool to call, formats the call correctly, predicts what it will need next, and manages its own context window. These are different cognitive tasks with different computational profiles, crammed into one architecture optimised for none of them.

This proposal outlines three interconnected ideas that, taken together, describe a path toward something qualitatively different:

1. **Residual stream artifacts** — threads export their final internal reasoning state, not just their token output. Orchestrators integrate child reasoning at the embedding level, not the transcript level. Cross-thread synthesis becomes lossless.

2. **A decomposed model family** — three specialised models (reasoning engine, intent router, predictive preloader) trained together on a shared embedding space, each optimal for its role, collectively producing what no single model can match.

3. **A tinygrad-based training pipeline** fed by RYE's own verified execution traces — a novel training corpus with properties no scraped dataset can replicate, closing a flywheel between execution quality and model quality.

None of this requires replacing frontier models. In the near term, the intent router and preloader enhance frontier model performance inside RYE. The full reasoning engine is a longer horizon target, justified once the trace corpus reaches sufficient scale and quality.

---

## What Exists Today

The infrastructure this proposal builds on — all live in the codebase:

| Component                 | Location                                                        | What It Does                                                                                                  |
| ------------------------- | --------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| Thread orchestration      | `orchestrator.py`, `coordination.yaml`                          | Spawn, cancel, wait, budget cascade across child threads                                                      |
| Thread transcripts        | `.ai/agent/threads/{thread_id}/transcript.jsonl`                | Full JSONL per thread — reasoning, tool calls, results                                                        |
| Artifact store            | `rye/agent/threads/persistence/artifact_store`                  | Stores large outputs outside conversation context, content-addressed                                          |
| Budget ledger             | `rye/agent/threads/persistence/budgets`                         | Hierarchical atomic budget tracking across concurrent forks                                                   |
| Capability attenuation    | fnmatch patterns (e.g., `rye.execute.tool.rye.bash`)            | Granular permission control cascading down thread hierarchy                                                   |
| Ed25519 signing           | Lillux `signing` primitive                                      | Every item signed; unsigned items rejected                                                                    |
| Registry                  | `rye/core/registry/registry`                                    | Push, pull, search signed items with TOFU key pinning                                                         |
| Thread registry           | `thread_registry.py` (SQLite)                                   | Tracks full thread provenance: `thread_id`, `directive`, `model`, `parent_id`, `chain_root_id`, `spend`, etc. |
| Dynamic tool registration | `tool_schema_loader.py`, `runner.py`                            | Each capability-granted tool registered as a real API-level tool with `_primary` dispatch field               |
| Provider adapter          | `rye/agent/threads/adapters/http_provider`                      | HTTP-based LLM provider bridge (Anthropic, OpenAI)                                                            |
| Streaming tool parser     | `rye/agent/threads/events/streaming_tool_parser`                | Parse streaming tool call responses from LLM providers                                                        |
| Lillux primitives         | `subprocess`, `http_client`, `signing`, `integrity`, `lockfile` | The full set of OS-level primitives — **no embedding or residual-export primitive exists today**              |

The key gap: threads communicate exclusively through token-level transcript text. There is no mechanism to carry geometric reasoning state between threads, no specialised models for tool dispatch or context prefetching, and no training pipeline fed by execution traces.

---

## Part 1: Residual Stream Artifacts

### The Problem

When a child thread finishes reasoning and arrives at a conclusion, it decodes that conclusion to tokens and writes it to its transcript. The parent reads the transcript and re-encodes it. Information is lost twice — once in decoding, once in re-encoding. The parent also pays the full context cost of reading thousands of tokens of transcript to extract what it needs.

The residual stream at the point of conclusion is a high-dimensional vector — 8,000–16,000 dimensions for a model in the 30–70B range — that encodes everything the model computed: not just the conclusion but the uncertainty distribution, the considered alternatives that were rejected, the conceptual frame the model was reasoning within. The token output is a lossy projection of that onto vocabulary space.

When an orchestrator synthesises five child thread transcripts, it is doing something structurally identical to attention — asking "what in these outputs is relevant to my current reasoning state?" — but doing it in token space, which is expensive and lossy. A direct geometric integration would be cheaper and lossless.

### What This Proposes

A new artifact type — the **residual stream artifact** — that captures a thread's final internal state before vocabulary projection. Stored alongside the transcript in the artifact store. Signed with the same Ed25519 key. Content-addressed by hash. Deposited to the registry like any other artifact.

Orchestrators that pull these artifacts can perform cross-attention over child reasoning states directly, rather than reading transcript text. The synthesis happens at the level it was computed.

### Architecture

#### New Lillux Primitive Required

Lillux currently has five primitives: `subprocess`, `http_client`, `signing`, `integrity`, `lockfile`. This proposal requires a sixth and seventh:

- **`embedding`** — compute embeddings via a configurable provider (also required by [Memory & Intent Resolution](memory-and-intent-resolution.md))
- **`residual_export`** — tap the inference runtime's internal tensor state at a named layer before vocabulary projection, serialise, and return as a typed artifact

The `residual_export` primitive requires that the inference server exposes internal tensor state. This is not available from closed API providers (Anthropic, OpenAI) — it requires either open-weight models with a controlled inference runtime, or a provider that explicitly surfaces this capability. This is a hard dependency: residual stream artifacts only work with models RYE controls at the runtime level. Frontier model threads continue to communicate via transcripts.

#### Artifact Format

```json
{
  "artifact_type": "residual_stream",
  "thread_id": "thread_abc123",
  "model": "rye-reasoning-engine-v1",
  "layer": "final",
  "dimensions": 8192,
  "dtype": "float16",
  "shape": [8192],
  "data": "<base64-encoded tensor>",
  "context_summary": "Economic debt dynamics analysis — rate shock fragility",
  "capability_context": [
    "rye.execute.tool.rye.file-system.*",
    "rye.fetch.knowledge.*"
  ],
  "timestamp": "2026-03-01T14:22:00Z",
  "integrity": "sha256:a1b2c3..."
}
```

The `context_summary` is a short token-level description generated alongside the artifact — enough for the orchestrator to decide whether to integrate it without pulling the full tensor.

The `capability_context` field records what the thread was permitted to do when it produced this state. This is a training signal (see Part 3) and an auditing signal — you can see what cognitive context shaped this reasoning geometry.

#### Indexing and Emission

A hook on the existing `after_complete` lifecycle event (already defined in `events.yaml`) triggers artifact emission:

```yaml
# Proposed addition to hook_conditions.yaml
- id: "emit_residual_artifact"
  event: "after_complete"
  layer: 3
  condition: "model_supports_residual_export"
  action:
    primary: "execute"
    item_type: "tool"
    item_id: "rye/artifacts/residual_emitter"
    params:
      thread_id: "${thread_id}"
      layer: "final"
      store_local: true
      publish_registry: "${publish_traces}"
```

The `model_supports_residual_export` condition gates emission — threads running on closed API providers silently skip this hook.

#### Cross-Thread Integration

An orchestrator that wants to integrate child reasoning geometrically rather than textually calls a new tool that performs cross-attention over pulled residual artifacts:

```
.ai/
└── tools/
    └── rye/artifacts/
        ├── residual_emitter       # Emits residual artifact on thread completion (NEW)
        └── residual_integrator    # Cross-attends over child residual artifacts (NEW)
```

The `residual_integrator` takes a set of child thread IDs, pulls their residual artifacts from the artifact store, performs a weighted integration against the orchestrator's current reasoning context (expressed as a query embedding), and injects the result into the orchestrator's next turn context.

```
Orchestrator reasoning state (query)
        │
        ▼
  residual_integrator
        │
        ├── Pull child residual artifacts (artifact store)
        ├── Query-key dot product: query × each child state
        ├── Softmax weighting
        └── Weighted sum → integrated context vector
        │
        ▼
  Injected into orchestrator's next turn
  (alongside or instead of transcript summaries)
```

This only works when orchestrator and children share an embedding space — which is a training requirement, not an inference requirement (see Part 2).

#### What Changes for Existing Threads

Nothing. Threads that do not support residual export continue to communicate via transcripts exactly as today. Residual artifacts are additive — an enhancement when available, gracefully absent when not. The orchestrator checks for artifact availability and falls back to transcript synthesis if not present.

---

## Part 2: Decomposed Model Family

### The Problem

A frontier model running inside a RYE thread is doing four cognitively distinct things simultaneously:

- **Reasoning** — decomposing the problem, identifying dependencies, forming hypotheses
- **Tool dispatch** — deciding which tool to call and formatting it correctly
- **Context management** — tracking what has been done, what remains, what is relevant
- **Prefetching** — implicitly predicting what it will need and searching for it

These tasks have different computational profiles. Reasoning needs depth — many layers, long context, high dimensionality. Tool dispatch is classification — fast, cheap, needs a small model that deeply understands the tool ontology. Prefetching is sequence prediction — watch a stream, predict the next event.

No single architecture is optimal for all four. Current models are large enough to do all four adequately, but none of them exceptionally.

### The Proposed Model Family

Three models, designed together, trained to share a geometric embedding space:

#### The Reasoning Engine (30–70B parameters)

The front-end model. Its only job is to reason. It expresses tool needs in natural language — the intent router handles dispatch. It expresses information needs — the preloader handles fetching. It receives synthesised child reasoning states via residual artifacts rather than reading transcripts.

Because it is relieved of tool formatting, context management overhead, and search mechanics, its capacity concentrates entirely on reasoning quality.

**Architecture characteristics:**

- Deep rather than wide — 80–96 transformer layers
- 32–64k context window, optimised for clean state transfer via continuation rather than indefinitely long windows
- Capability-aware attention masking baked into the IR (see Part 3)
- Residual state export exposed as a named tensor output of the compiled kernel

It is not a replacement for Claude or GPT-4. In the near term it may not exist at all — frontier models play this role while the trace corpus accumulates. The reasoning engine is the long-horizon target.

#### The Intent Router (2–7B parameters)

A small, fast, specialised model. It watches the reasoning engine's output stream for expressed tool intent and maps that intent to a validated RYE tool invocation.

The reasoning engine writes: _"I need to list the files in the project config directory."_

The intent router maps this to: `rye_file_system_ls(path=".ai/config/")` — a real API-level tool call, correctly formed, with the right parameters.

This is essentially a classification and structured output problem. A 2–7B function-calling model is well suited for it — the task does not require deep reasoning, it requires accurate schema knowledge and fast execution. The dynamic tool registration system (which already registers each capability-granted tool as a real API-level tool with a `_primary` dispatch field) gives the intent router a clean, structured target schema.

The intent router is the first component of this family worth building. It is the most tractable (smallest model, clearest training signal), most immediately valuable (reduces tool call errors for all models inside RYE), and generates additional trace data as a byproduct of its operation.

#### The Predictive Preloader (adapter-scale)

A small sequence model — potentially an adapter on top of the reasoning engine rather than a standalone model — that watches the reasoning stream and prefetches knowledge and tool context before the model asks for it.

While the reasoning engine is generating tokens (typically 1–5 seconds of streaming output), the preloader reads the partial output and predicts what `rye_fetch` calls are about to happen. It fires those queries in parallel, warming the context before the reasoning engine reaches the point of needing the results.

The preloader does not make decisions — it prefetches. If its predictions are wrong, the fetched context is simply unused. Cache misses fall back to the normal search-and-load path. The upside is significant latency reduction for common patterns; the downside is negligible.

**What it requires beyond what exists today:**

A new hook event — `on_token_buffer` — that fires every N tokens during streaming. The existing `cognition_out_delta` event is per-chunk, not per-N-tokens, and is defined as droppable. A dedicated non-droppable buffer event is needed:

```yaml
# Proposed addition to events.yaml and hook_conditions.yaml
- id: "predictive_prefetch"
  event: "on_token_buffer" # PROPOSED — new event type needed
  layer: 3
  action:
    primary: "execute"
    item_type: "tool"
    item_id: "rye/intent/predictor"
    params:
      buffer_chars: 500
      context_turns: 3
      top_k: 3
      min_confidence: 0.6
      dispatch_prefetch: true
```

#### Shared Embedding Space

The three models must share a geometric embedding space for residual artifact integration to work. This is enforced through the training objective — not architecture alone — via a geometric alignment loss (see Part 3).

The practical consequence: the reasoning engine's expressed intent and the intent router's understanding of the tool ontology must be geometrically aligned. The reasoning engine's final residual state and the orchestrator's integration of it must be meaningfully compatible. These are training requirements, not inference requirements, but they must be designed in from the beginning.

---

## Part 3: Tinygrad Training Pipeline

### Why Tinygrad

Tinygrad is an end-to-end deep learning stack with a fully visible and hackable IR and compiler. For this training pipeline, that matters for three reasons:

**Capability-aware attention masking at the IR level** — current capability context is injected as text in the system prompt. The proposed architecture moves it into the attention kernel. A thread with `rye.execute.tool.rye.bash` in its capability set should literally not be able to attend to tool definitions it doesn't have permission to use. In tinygrad, this is a kernel-level modification — a capability-conditioned mask applied inside the attention matmul before softmax. In PyTorch, this is fighting the framework. In tinygrad, it's modifying a visible IR node.

**Residual state export** — the reasoning engine needs to expose its final residual stream as a named tensor output of the compiled kernel, not just as an intermediate that gets discarded after vocabulary projection. Tinygrad's lazy evaluation model makes this natural — you name the tensor you want to keep and it becomes an output. The JIT compiles the extended graph that includes it.

**Multi-objective training** — the training pipeline runs five simultaneous loss functions over heterogeneous data (see below). In PyTorch eager mode, this is complex gradient management. In tinygrad's lazy IR, all five objectives are tensor operations that get fused and optimised before any computation runs. The compiler handles the scheduling.

### The Training Corpus

RYE execution traces are not ordinary training data. Each trace has properties no scraped dataset can replicate:

| Property              | What it enables                                                                                                     |
| --------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Verified provenance   | Every trace is Ed25519-signed — you know exactly what model, what capability context, what code version produced it |
| Capability annotation | Every tool call is labeled with the capability that permitted it — ground truth for capability consistency loss     |
| Causal structure      | The trace records reasoning → tool call → result as explicit triples, not implicit in prose                         |
| Outcome labels        | Thread success, cost actuals vs budget, cancellations — labeled without human annotation                            |
| Delegation hierarchy  | The full parent-child thread tree is recorded — which reasoning states spawned which children                       |
| Residual artifacts    | (When available) the geometric reasoning state that produced each conclusion                                        |

**Corpus structure:**

A base layer of standard text pretraining — language, world knowledge, general reasoning. Approximately 2–4 trillion tokens. This is conventional and necessary.

A structured layer of RYE execution traces. Quality dominates quantity here — 50–200 billion tokens of verified trace data, weighted more heavily in the training objective than raw text despite the smaller volume.

A registry layer — the actual signed directives, tools, and knowledge items. Small in token count (~10–50 billion) but extremely high signal. This is what makes the model natively understand the RYE execution model rather than learning it from descriptions.

### Training Objectives

Five simultaneous loss functions:

**1. Language modeling loss** — standard cross-entropy next-token prediction on transcript text. The base that gives the model language. Nothing novel here.

**2. Causal faithfulness loss** — for each reasoning step in a trace, the model's expressed reasoning should predict the subsequent tool call. Penalises reasoning that doesn't causally connect to action. Requires tinygrad custom loss operating over reasoning-to-tool-call pairs extracted from trace structure.

**3. Capability consistency loss** — the model should assign near-zero probability to tool calls outside the current capability mask. Hard supervised signal from traces — every trace records exactly what was permitted. Applied as a classification loss on the capability-masked attention outputs. This is the loss that makes capability-aware masking work during inference: the model is not just told its capabilities, it has been trained to respect them at a geometric level.

**4. Outcome prediction loss** — given a partial trace, predict thread success, cost, and which children will be cancelled. Trains the model to reason about execution trajectories rather than just token sequences. The budget ledger and thread registry provide labeled outcomes without any human annotation.

**5. Geometric alignment loss** — the novel objective that makes the model family work as a unit. Child thread residual artifacts should project into the parent's embedding space in a consistent, interpretable way. Trained via contrastive loss over parent-child artifact pairs from the same execution lineage. The parent's synthesis should be geometrically close to a weighted combination of its children's final states. Without this loss, you have embedding spaces that are only incidentally aligned. With it, cross-thread residual integration is principled rather than approximate.

### Deployment Model

The trained model family is packaged as a RYE bundle. Signed, versioned, published to the registry as a system-space item. It becomes the default for any RYE deployment that pulls it — overridable at project or user space like any other item.

**Near-term (intent router first):**

The intent router is the first component worth building and deploying. It is the smallest model, has the clearest training signal (every tool call in every trace is a labeled example), and delivers immediate value — reducing tool call errors across all models running inside RYE. It also generates richer traces as a byproduct, since correct tool calls produce more coherent downstream reasoning.

Deployed as a system-space tool at `rye/intent/router`. Any thread with intent resolution enabled routes through it. The existing dynamic tool registration system already provides the structured schema the router targets.

**Medium-term (preloader):**

Once the trace corpus contains sufficient temporal patterns — search and load calls relative to the reasoning states that preceded them — the preloader can be trained. It is adapter-scale, can be initialised from the intent router or reasoning engine, and deploys non-disruptively (cache misses fall back to the normal path).

**Long-term (reasoning engine):**

Justified once the trace corpus reaches the scale and quality needed to train something meaningfully better than frontier models on agentic tasks. The reasoning engine does not need to match frontier models on general benchmarks — it needs to be better at the specific cognitive profile of operating inside RYE: long-horizon planning, delegation quality, capability-aware reasoning, residual state transfer.

At 30–70B parameters trained on this corpus, it should match or exceed frontier model performance on RYE-native tasks at significantly lower inference cost — because it is not carrying the cognitive overhead of tool mechanics, is not spending capacity on world knowledge storage that the registry externalises, and runs as multiple lean parallel instances rather than one massive sequential one.

---

## Part 4: Trace Collection and the Hosted Layer

### The Collection Problem

Local RYE runs are epistemically isolated. The traces stay on the user's machine. The registry can sync items, but execution quality data does not flow back without explicit opt-in.

Two collection channels:

**Hosted premium execution** — RYE offered as a substrate for high-value tasks where the context leverage genuinely matters: deep research synthesis, complex multi-domain analysis, enterprise workflow orchestration, government decision support. These tasks produce the richest traces — long running, multi-thread, complex delegation hierarchies, high-stakes outcomes that can be verified. Users pay for execution quality; trace collection is a byproduct. The hosted product subsidises the data flywheel.

**Opt-in local traces** — local runs can opt in to contribute anonymised execution traces in exchange for registry access benefits: accumulated domain knowledge, refined directives, preloaded context for their specific use patterns. The value exchange is explicit and cryptographically grounded — contributors can see exactly what they are contributing (the signed trace) and what they are receiving (signed items with TOFU provenance). Nothing is contributed without consent; nothing is received without verification.

### Integration with Frontier Models

This proposal does not position RYE's model family as a competitor to Claude, GPT-4, or Gemini. Those models are the reasoning engines until the trace corpus justifies building a native alternative.

The integration story: frontier models running inside RYE gain the full execution substrate — capability-aware tool registration, budget tracking, thread lifecycle management, residual artifact support (via the `residual_export` primitive for open-weight variants), registry access. The same model that without RYE is stateless and isolated becomes a citizen of a persistent, verified execution environment.

The traces produced by frontier models running inside RYE are valuable to those labs for their own training purposes — verified, capability-annotated, causally structured agentic reasoning data. There is a natural partnership structure: trace data in exchange for deeper API integration, model variants fine-tuned for RYE's execution model, or research collaboration on capability-aware attention.

### The Flywheel

```
Hosted execution + opt-in local
        │
        ▼
Verified trace corpus accumulates
(signed, capability-annotated, outcome-labeled)
        │
        ▼
Intent router trained first (smallest, highest immediate value)
        │
        ▼
Deployed as system-space tool — all RYE executions improve
        │
        ▼
Better executions → richer traces → better training signal
        │
        ▼
Preloader trained → further execution improvement
        │
        ▼
Corpus reaches reasoning engine threshold
        │
        ▼
Reasoning engine trained — native to RYE at every level
        │
        ▼
Geometric alignment enables residual artifact integration
        │
        ▼
Collective reasoning across thread boundaries
The registry becomes a repository of verified reasoning states
```

The moat is not the code — RYE is MIT licensed. It is not the models — those can be replicated. It is the registry: accumulated verified execution traces, signed knowledge, provenance-tracked reasoning artifacts. This cannot be replicated without running the system at scale long enough to generate it. And the cryptographic model means the quality and provenance of everything in it can be proven in a way no scraped dataset ever could.

---

## Summary: What's New vs What's Unchanged

### Proposed New Tools

| Tool                  | Proposed Location                   | Purpose                                                                      |
| --------------------- | ----------------------------------- | ---------------------------------------------------------------------------- |
| `residual_emitter`    | `rye/artifacts/residual_emitter`    | Emits residual stream artifact on thread completion                          |
| `residual_integrator` | `rye/artifacts/residual_integrator` | Cross-attends over child residual artifacts for lossless synthesis           |
| `intent_router`       | `rye/intent/router`                 | Maps expressed intent to validated tool invocations (small model)            |
| `intent_predictor`    | `rye/intent/predictor`              | Predicts intents during streaming for prefetch (also in Memory & Intent doc) |

### Proposed New Lillux Primitives

| Primitive         | Purpose                                      | Status                                      |
| ----------------- | -------------------------------------------- | ------------------------------------------- |
| `embedding`       | Compute embeddings via configurable provider | Also required by Memory & Intent Resolution |
| `residual_export` | Tap inference runtime internal tensor state  | New — requires controlled inference runtime |

### Proposed New Hook Events

| Hook Event        | Status                               | Trigger                         | Action                                            |
| ----------------- | ------------------------------------ | ------------------------------- | ------------------------------------------------- |
| `after_complete`  | **Existing** — extended              | Thread exits                    | Emit residual artifact (gated on runtime support) |
| `on_token_buffer` | **Proposed** — new event type needed | Every N tokens during streaming | Predict + prefetch intents                        |

### Proposed New Training Infrastructure

| Component                          | What It Requires                                                                             |
| ---------------------------------- | -------------------------------------------------------------------------------------------- |
| Tinygrad training pipeline         | Open-weight base model; tinygrad IR modifications for capability masking and residual export |
| Capability-aware attention masking | IR-level kernel modification in tinygrad — not a post-hoc filter                             |
| Five-objective loss function       | Tinygrad custom loss nodes; trace structure parsing                                          |
| Geometric alignment loss           | Contrastive training over parent-child residual artifact pairs                               |
| Trace collection infrastructure    | Hosted execution layer; opt-in local trace signing and submission                            |

### What's Unchanged

| Component                                            | Status                                                                                      |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| Three MCP tools (`fetch`, `execute`, `sign`)          | Same interface, no changes                                                                  |
| Capability attenuation (fnmatch patterns)            | Unchanged — capability masking extends it at the model level, does not replace it           |
| Three-tier space resolution                          | Unchanged                                                                                   |
| Ed25519 signing, lockfiles, chain verification       | Unchanged                                                                                   |
| Thread orchestration and budget cascading            | Unchanged                                                                                   |
| Dynamic tool registration (`_primary` dispatch)      | Unchanged — intent router targets the same registered tools                                 |
| Existing Lillux primitives                           | Unchanged — two new primitives are additions                                                |
| Existing hook events                                 | Unchanged — new hooks are additions                                                         |
| Registry and TOFU trust model                        | Unchanged — residual artifacts are a new item type, same trust model                        |
| Frontier model support                               | Unchanged — these models continue to work exactly as today; residual artifacts are additive |

---

## Design Principles Maintained

**Everything is data** — the intent router, preloader, and residual integrator are all items in `.ai/`. Override at project level. No framework code changes for the tools themselves; only the Lillux primitives require core changes.

**The runtime runs on itself** — the model family components are subject to the same signing, integrity checks, and space precedence as any other tool. The reasoning engine's directives live in `.ai/` and are signed like everything else.

**Fail-closed** — residual export unavailable → graceful fallback to transcript synthesis. Intent router unavailable → direct tool calls work as before. Prefetch cache miss → normal search-and-load path. Each layer degrades independently.

**Permissions are not a special case** — capability-aware attention masking is the model-level expression of the same permission model that already governs everything else in RYE. It does not introduce new concepts; it drives existing concepts deeper into the stack.

**The substrate and the models together** — the agent is not the model. The agent is the substrate running models. This proposal is the elaboration of that principle all the way down: from the execution fabric through the OS primitives through the training objectives through the geometric structure of the embedding space itself.
