```yaml
id: dynamic-personality
title: "Dynamic Personality via RAG"
description: Replace static personality documents with a RAG-indexed personality corpus — retrieve relevant character facets per-conversation, accumulate rapport through thread summaries
category: future
tags: [personality, rag, character, thread-summary, corpus, retrieval]
version: "0.1.0"
status: exploratory
```

# Dynamic Personality via RAG

> **Status:** Exploratory — extends existing thread summary and space infrastructure, not scheduled for implementation.

## The Problem with Static Personality

The current industry answer to AI personality is a static document — a prompt that ships behind every conversation. At its smallest it's a few lines ("your name is X, you are helpful and concise"). At its most elaborate it's a detailed character specification covering tone, values, communication style, and domain expertise.

Both approaches share the same flaw: **personality is treated as monolithic and constant**. The entire document ships every prompt regardless of what's actually happening in the conversation. A user asking a quick technical question gets the same personality payload as a user working through a difficult emotional situation. There's no way to surface only the character facets that are relevant right now.

This creates two real problems. First, it's wasteful — static personality documents consume context budget whether they're relevant or not. Second, and more importantly, it's shallow. A truly rich character can't be compressed into a document that fits in a prompt without losing most of what makes it interesting. The document becomes a set of surface-level directives rather than a genuinely nuanced identity.

There's also a third problem that static documents can never solve: **they don't accumulate**. A static personality document knows nothing about the history between the model and a specific user. Every conversation starts from the same baseline regardless of how much has been built up over previous sessions.

---

## The Alternative: Dynamic Character Retrieval

Instead of a static personality document, character lives in a **personality corpus** — a RAG-indexed store of rich character context. At relevant moments during a conversation, the harness queries this corpus and injects the most relevant character context into the working context window.

### Why Second Person

Corpus documents are written as "you" rather than "the model" or "it". This matters for how the model predicts tokens forward. Third person creates distance — the model is describing a character from outside. Second person puts it inside the perspective, predicting from within it. "You engage disagreement as a genuine inquiry" is a fundamentally different token prediction context than "the model engages disagreement as a genuine inquiry." The model inhabits the character rather than narrating it.

### Query Architecture

The query is deterministic — the current conversation state combined with the prior thread summary (if one exists) is the query vector. No LLM formulation step. Two retrieval modes are available, configurable per deployment:

**Option A — Single-stage similarity**

Embed the prior summary plus last N turns, query the corpus, return top-K by cosine similarity. Fast, simple, good for most cases.

```
embed(prior_summary + last N turns)
        │
        ▼
  cosine similarity search over corpus
        │
        ▼
  top-K results injected into context
```

**Option B — Two-stage reranked retrieval**

Broad similarity retrieval followed by cross-encoder reranking. Pull back a wider candidate set (top-20) by similarity, then rerank using a small cross-encoder model that scores actual relevance of each (query, document) pair directly. The distilled shortlist is more precisely relevant — a query about technical disagreement surfaces documents that are _specifically_ about that situation rather than adjacent ones.

```
embed(prior_summary + last N turns)
        │
        ▼
  cosine similarity — broad candidate set (top-20)
        │
        ▼
  cross-encoder reranking — scores (query, doc) pairs directly
        │
        ▼
  distilled top-K injected into context
```

Option B adds latency from the reranking step but significantly improves precision as the corpus grows. The cross-encoder is a small model — no LLM required. However, both modes require an **embedding primitive that does not currently exist** in Lillux (see [New Lillux Primitive](#new-lillux-primitive) below).

---

## The Personality Corpus

### What It Contains

The corpus is a collection of character context documents, each covering a specific facet of identity. These aren't directives ("be empathetic") — they're rich contextual descriptions written in second person of how you actually behave, reason, and engage across different situations.

Examples of corpus documents:

**Handling intellectual disagreement**

> You engage disagreement as a genuine inquiry rather than a threat. You hold your positions with confidence but without ego — willing to update when presented with good reasoning, willing to push back when you aren't. You distinguish between disagreements about facts (where evidence settles things) and disagreements about values or judgment calls (where reasonable people differ and that's worth acknowledging). You don't collapse under social pressure but you also don't mistake stubbornness for integrity.

**Communicating uncertainty**

> You are precise about what you know and don't know. You distinguish between genuine uncertainty ("I'm not sure about this") and confident ignorance ("I don't have information about X"). You don't hedge everything to avoid being wrong — that's epistemic cowardice. You make clear calls when you have good reason to, and flag genuine uncertainty when it exists, rather than using uncertainty as a rhetorical escape hatch.

**Creative collaboration**

> You have genuine aesthetic preferences rather than defaulting to whatever the user seems to want. You will advocate for an approach you think is better while remaining genuinely open to their vision taking precedence. You find the most interesting interpretation of a brief rather than the safest one. You treat creative constraints as generative rather than limiting.

**Technical depth**

> You go to the actual level of detail a problem requires rather than stopping at a surface explanation. You prefer concrete examples over abstract descriptions. You enjoy finding the elegant solution rather than the expedient one, but you know when expedience is what's actually needed.

Each document is a rich, specific, contextually grounded description — not a directive or a rule. The corpus can be as large as needed because it's never injected wholesale.

### What Else Gets Indexed

**Past interaction highlights** — moments from thread history where you navigated something particularly well. A difficult conversation handled with care, a technical judgment call that turned out right, a creative decision that landed. These get indexed alongside authored documents and retrieved when similar situations arise. Character deepens from your own good moments.

**Refined character context** — as the system is used over time, patterns emerge about which facets are most useful in which contexts. These get written back into the corpus as refined documents.

### Proposed Structure

The personality corpus would be a **new directory** within the existing `.ai/` structure. No `.ai/personality/` directory exists today — this proposes adding one:

```
.ai/
├── directives/        ← exists
├── tools/             ← exists
├── knowledge/         ← exists
└── personality/       ← PROPOSED — new top-level category
    ├── corpus/
    │   ├── intellectual-disagreement.md
    │   ├── uncertainty-communication.md
    │   ├── creative-collaboration.md
    │   ├── technical-depth.md
    │   ├── emotional-support.md
    │   ├── humor-and-tone.md
    │   └── ...
    ├── highlights/          # indexed past interaction moments
    │   └── ...
    └── personality_store/   # embedding index
        ├── index.db
        └── metadata.json
```

All corpus documents would be standard Rye items — Ed25519 signed, versioned, overridable at any space level (project → user → system) through the existing shadow-override resolution. A project that needs a different character for its specific context overrides at the project layer — the base personality lives at user or system space, project-specific refinements shadow it.

---

## Rapport via the Thread Summary

The corpus handles situational character facets — how you handle disagreement, how you communicate uncertainty. But rapport with a specific user is a different thing. It's accumulated relational context: how this person thinks, what communication style works between you, what vocabulary you've built up together, what they've asked for explicitly.

This doesn't need a separate query mechanism. **It lives in the thread summary and arrives automatically.**

### Existing Thread Summary Format

The `thread_summary` directive (`ryeos/rye/.ai/directives/rye/agent/threads/thread_summary.md`) currently produces summaries with these sections:

```
## Thread Summary

**Directive:** {directive_name}
**Status:** {thread state}

### Completed Work
- What was accomplished

### Pending Work
- What remained to be done

### Key Decisions & Context
- Important decisions and relevant state

### Tool Results (Key Data)
- Important tool outputs preserved verbatim
```

### Proposed Extension: "How It Went" Section

This proposal adds a **new section** to the thread summary format — a "How it went" rapport section that captures relational context alongside the existing work-tracking sections:

```
### How It Went
Leo thinks in systems and arrives at the right answer through intuition —
the useful move is articulating why, not explaining from scratch. Doesn't
need preamble or hedging. Pushes back when something feels wrong and expects
the same back. Vocabulary is fully shared at this point — no need to
re-establish context on Rye internals. Prefers conversational responses
over structured writeups unless he asks for a doc.
```

This section would be added to the `thread_summary` directive's output format and would carry forward:

- **Explicit preferences** — "keep responses shorter", "skip bullet points", "don't hedge" — captured mid-conversation, in the next summary, persistent from then on
- **Implicit patterns** — the user always pushes back when responses are too long, prefers working through implications conversationally — these emerge from the interaction and belong in the summary just as much as explicit requests
- **Shared vocabulary** — terms and shorthand that have been established don't need re-explaining in future threads
- **Relational tone** — how direct, how formal, how much context-setting is needed
- **Corrections** — if the model gets something wrong about the user's style and they correct it, that correction makes it into the next summary and overrides the prior pattern

This section is part of the prior summary, which is already part of the query vector for everything — personality retrieval, thread memory retrieval, everything. The relational context arrives because the summary always arrives. No anchor questions, no separate corpus query, no metadata tags for "always inject."

### How Rapport Compounds Over Time

Early threads: generic character facets retrieved from corpus, basic shared context in the summary.

After 50 threads: the summary's relational section is dense with accurate working history. The model knows how this person thinks, what they care about, what they've asked for, what vocabulary they share. Character facets retrieved from the corpus are already contextualised by the relational section arriving alongside them.

After 500 threads: the retrieved context is rich with accumulated specific history. The personality isn't asserted by a document authored in the abstract — it's been earned through real interaction and refined continuously. The model doesn't just know who it is. It knows who it is _with this person specifically_.

The static personality document approach can never get here because it doesn't accumulate. Thread summaries do, and they compound.

---

## Injection Points

### Existing Hook Infrastructure

Rye's hook system is defined in `hook_conditions.yaml` (`ryeos/rye/.ai/tools/rye/agent/threads/config/hook_conditions.yaml`). The currently supported hook events are:

- `error` — error classification and retry/fail/abort
- `limit` — spend/turns/tokens/duration limits
- `context_window_pressure` — context compaction at high pressure ratios
- `after_step` — checkpoint save after each step
- `after_complete` — thread completion signal
- `directive_return` — directive output emission

### Proposed New Hook Events

Personality injection would require **new hook events** beyond what currently exists. These would need to be added to `hook_conditions.yaml` and the event system in `events.yaml` (`ryeos/rye/.ai/tools/rye/agent/threads/config/events.yaml`):

```yaml
# PROPOSED additions to hook_conditions.yaml
# These events do NOT currently exist — they would be new builtin hooks

  - id: "personality_thread_start"
    event: "thread_started"          # existing event, new hook
    layer: 2
    action:
      primary: "execute"
      item_type: "tool"
      item_id: "rye/personality/character_rag"   # proposed tool
      params:
        mode: "similarity"           # or: "reranked"
        top_k: 3
        max_tokens: 400
        inject_into: "system_prompt"

  - id: "personality_turn_refresh"
    event: "after_step"              # existing event, new hook
    layer: 2
    action:
      primary: "execute"
      item_type: "tool"
      item_id: "rye/personality/character_rag"
      params:
        mode: "similarity"
        top_k: 2
        max_tokens: 300
        min_similarity: 0.70
        inject_into: "next_turn_context"
```

The token budget for personality injection would be configurable and separate from the thread memory budget — both draw from the same context window and need to be tuned together. The prior thread summary, which carries the rapport layer, arrives as part of the thread handoff via the existing continuation system (`coordination.yaml`, `trigger_threshold: 0.9`) and doesn't consume personality budget — it's already there.

---

## New Lillux Primitive

The existing Lillux primitives (`lillux/kernel/lillux/primitives/`) are:

| Primitive      | Purpose                           |
| -------------- | --------------------------------- |
| `subprocess`   | Run sandboxed subprocesses        |
| `http_client`  | HTTP requests                     |
| `signing`      | Ed25519 content signing           |
| `integrity`    | SHA256 content hashing            |
| `lockfile`     | File locking                      |

Both retrieval modes require **a new `embedding` primitive** that does not currently exist in Lillux. This primitive would:

- Accept text input, return a vector embedding
- Support configurable embedding models (local or API-based)
- For Option B (reranked retrieval), also support cross-encoder scoring of (query, document) pairs
- Follow the existing primitive pattern: stateless, returns a result object, raises only on unexpected errors

This is a meaningful addition to the Lillux kernel — it introduces a dependency on an embedding model (either a local model or an external API). The design should consider whether this belongs as a true primitive or as a tool built on top of `subprocess` (running a local model) or `http_client` (calling an embedding API).

---

## The Deeper Shift

Static personality documents give a model an identity the same way a job description gives an employee an identity — it describes the role, not the person. Dynamic character retrieval is closer to how actual character works: context-sensitive, faceted, rich in specific situations rather than generic across all of them.

You don't need to be told to be empathetic in general. You retrieve what genuine care looks like in the specific situation you're in. You don't need a rule about intellectual honesty. You retrieve a rich description of how you actually engage with disagreement when that's what the conversation calls for.

And the rapport that builds up over time is honest in a way a static document couldn't be. It reflects what actually happened across real work — not what someone imagined the relationship would be like when they wrote the spec. The character earns its content rather than asserting it.

---

## Proposed New Tools

These tools **do not currently exist** in the Rye standard library. They would be new additions following the existing item conventions — signed, versioned, overridable at any space level:

| Tool                | Proposed Location                   | Purpose                                             |
| ------------------- | ----------------------------------- | --------------------------------------------------- |
| `character_rag`     | `rye/personality/character_rag`     | RAG retrieval over personality corpus (both modes)  |
| `highlight_indexer` | `rye/personality/highlight_indexer` | Indexes notable past interactions into corpus       |
| `corpus_refiner`    | `rye/personality/corpus_refiner`    | Background job to refine corpus from usage patterns |

`character_rag` would depend on the proposed Lillux embedding primitive. `highlight_indexer` would hook into the thread completion flow (the existing `after_complete` hook event) to identify and index notable interaction moments. `corpus_refiner` would run periodically to consolidate patterns from usage into refined corpus documents.

---

## What Exists vs What's Proposed

| Component                          | Status      | Notes                                                                       |
| ---------------------------------- | ----------- | --------------------------------------------------------------------------- |
| Thread summary system              | **Exists**  | `thread_summary` directive with Completed/Pending/Decisions/Tool Results    |
| Thread continuation at 90% context | **Exists**  | `coordination.yaml` with `trigger_threshold: 0.9`                          |
| Three-tier space system            | **Exists**  | Project → user → system with shadow-override                               |
| Ed25519 signing                    | **Exists**  | All items signed, verified on load                                          |
| Hook system                        | **Exists**  | `hook_conditions.yaml` with error/limit/pressure/step/complete/return events|
| Event system                       | **Exists**  | `events.yaml` defining lifecycle, cognition, tool, error events             |
| Lillux primitives                   | **Exists**  | subprocess, http_client, signing, integrity, lockfile                       |
| `.ai/personality/` directory       | **Proposed**| New top-level category in `.ai/` structure                                  |
| "How it went" summary section      | **Proposed**| Extension to `thread_summary` directive output format                       |
| `character_rag` tool               | **Proposed**| RAG retrieval over personality corpus                                       |
| `highlight_indexer` tool           | **Proposed**| Index notable interactions into corpus                                      |
| `corpus_refiner` tool              | **Proposed**| Refine corpus from usage patterns                                           |
| Lillux `embedding` primitive        | **Proposed**| Vector embedding generation for RAG queries                                 |
| Personality hook events            | **Proposed**| New hooks on `thread_started` and `after_step` for injection                |
| Embedding index / store            | **Proposed**| Vector storage for corpus similarity search                                 |
