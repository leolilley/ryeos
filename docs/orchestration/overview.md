```yaml
id: orchestration-overview
title: "Orchestration Overview"
description: How Rye OS enables AI agents to coordinate multi-step workflows through thread-based orchestration
category: orchestration
tags: [orchestration, threads, overview, architecture]
version: "1.0.0"
```

# Orchestration Overview

Rye OS orchestration lets AI agents coordinate multi-step workflows by spawning child threads — each running its own LLM loop with independent limits, budget, model selection, and transcript. A parent directive spawns children via `execute directive`, waits for results, and coordinates the next phase.

## The Core Pattern

Every orchestrated workflow follows the same structure:

1. **Orchestrator directive** loads state and decides what work to do
2. It **spawns child threads** — each child runs a directive in its own LLM loop
3. It **waits** for children to complete
4. It **aggregates results** and moves to the next phase
5. Repeat until done

```
┌─────────────────────────────────────────────────────┐
│  Root Orchestrator (sonnet, 30 turns, $3.00)        │
│                                                      │
│  Phase 1: Load state                                 │
│  Phase 2: Select targets                             │
│  Phase 3: Spawn discover_leads children ─────────┐   │
│  Phase 4: Wait for discovery                     │   │
│  Phase 5: Spawn qualify_leads sub-orchestrator   │   │
│  Phase 6: Wait for qualification                 │   │
│  Phase 7: Report results                         │   │
└──────────────────────────────────────────────────┘   │
                                                       │
    ┌──────────────────────────────────────────────────┘
    │
    ▼
┌───────────────┐  ┌───────────────┐  ┌───────────────┐
│ discover_leads│  │ discover_leads│  │ discover_leads│
│ (haiku, 10t)  │  │ (haiku, 10t)  │  │ (haiku, 10t)  │
│ $0.10 budget  │  │ $0.10 budget  │  │ $0.10 budget  │
└───────────────┘  └───────────────┘  └───────────────┘

    ┌─────────────────────────────────────────────────┐
    │  Sub-Orchestrator: qualify_leads                 │
    │  (sonnet, 20 turns, $1.00)                      │
    │                                                  │
    │  Step 1: Load knowledge                          │
    │  Step 2: Read leads                              │
    │  Step 3: Spawn scrape_website children ──────┐   │
    │  Step 4: Spawn score_lead children           │   │
    │  Step 5: Filter and classify                 │   │
    │  Step 6: Save results                        │   │
    └──────────────────────────────────────────────┘   │
                                                       │
        ┌──────────────────────────────────────────────┘
        │
        ▼
    ┌──────────────┐  ┌──────────────┐
    │ scrape_website│  │ score_lead   │
    │ (haiku, 8t)  │  │ (haiku, 4t)  │
    │ $0.05 budget │  │ $0.05 budget │
    └──────────────┘  └──────────────┘
```

## Single Entry Point: `execute directive`

All orchestration happens through `rye_execute` with `item_type="directive"`. An agent spawns a child by calling:

```python
rye_execute(
    item_type="directive",
    item_id="my-project/orchestrator/run_pipeline",
    parameters={"location": "Dunedin", "batch_size": 5},
    limit_overrides={"turns": 30, "spend": 3.00}
)
```

Internally, this delegates to `thread_directive` — but the LLM never calls that tool directly.

**Parameters:**

| Parameter         | Type   | Required | Description |
|-------------------|--------|----------|-------------|
| `item_id`         | string | yes      | Directive item_id to execute |
| `parameters`      | object | no       | Input parameters for the directive |
| `async`           | bool   | no       | Return immediately with `thread_id` (default: `false`) |
| `model`           | string | no       | Override LLM model |
| `limit_overrides` | object | no       | Override limits: `turns`, `tokens`, `spend`, `spawns`, `duration_seconds`, `depth` |

Synchronous execution blocks until the child completes and returns its result. Asynchronous execution (`async: true`) forks the process and returns a `thread_id` immediately — the parent can then continue spawning more children and wait for all of them later.

## The Hierarchy

Orchestration uses a tree of directives at three levels:

### Orchestrator

The root coordinator. Loads state, decides what to do, spawns children, aggregates results. Uses a reasoning-capable model (sonnet) because it needs to manage complex state and make coordination decisions.

### Sub-orchestrator

An intermediate coordinator that manages a subset of work. Spawned by the root orchestrator when a phase is complex enough to warrant its own coordination loop. Also uses sonnet.

### Execution Leaf

A single-purpose worker that calls one tool and returns its result. Uses a fast/cheap model (haiku) because it doesn't need to reason — just execute a tool call and report back.

## Directive Role Table

| Role             | Model  | Turns | Spend  | Responsibility |
|------------------|--------|-------|--------|----------------|
| Orchestrator     | sonnet | 30    | $3.00  | Load state, spawn children, coordinate, report |
| Sub-orchestrator | sonnet | 20    | $1.00  | Coordinate subset of work, spawn grandchildren |
| Strategy         | haiku  | 6     | $0.05  | Load knowledge + state, decide what to do |
| Execution leaf   | haiku  | 4–10  | $0.10  | Call one tool, save output, update state |

## Why Thread-Based Orchestration

**Isolation.** Each thread gets its own LLM conversation, transcript, and error boundary. A child that crashes or hits a limit doesn't take down the parent.

**Cost control.** Every thread has a spend limit. The hierarchical budget ledger ensures children can't spend more than the parent allocated. Budgets cascade up — the parent always knows the true cost of its subtree.

**Model selection per task.** Orchestrators use expensive reasoning models. Leaf workers use cheap fast models. A pipeline that spawns 20 leaf workers at $0.05 each costs $1.00 in leaf work instead of $3.00 if everything ran in one big sonnet conversation.

**Parallelism.** Async children run concurrently. The parent spawns multiple children with `async: true`, then waits for all of them with `wait_threads`. Wall-clock time drops to the slowest child instead of the sum of all children.

**Auditability.** Every thread writes a `transcript.md` and `thread.json`. You can inspect exactly what each thread did, what it spent, and what tools it called.

## Concrete Example: Agency-Kiwi Lead Pipeline

The agency-kiwi project uses orchestration to run a multi-phase lead generation pipeline:

**`run_lead_pipeline`** (root orchestrator, sonnet, 30 turns, $3.00):
1. Load pipeline state, niche list, city data, and learnings from knowledge
2. Select a batch of niches from the remaining queue
3. Spawn `discover_leads` children — one per niche, each with `{turns: 10, spend: 0.10}`
4. Wait for all discovery threads, aggregate results
5. Spawn `qualify_leads` sub-orchestrator with `{turns: 20, spend: 1.00}`
6. Spawn `prepare_outreach` sub-orchestrator
7. Spawn `update_pipeline_state` to persist results
8. Report summary and record learnings

**`qualify_leads`** (sub-orchestrator, sonnet, 20 turns, $1.00):
1. Load GHL sales framework and service tier knowledge
2. Read the leads file, separate leads by has-website / no-website
3. Spawn `scrape_website` children — one per lead with website, `{turns: 8, spend: 0.05}`
4. Wait, then spawn `score_lead` children per lead, `{turns: 6, spend: 0.05}`
5. Filter and sort by score, classify into tiers
6. Save qualified leads JSON

**`discover_leads`** (execution leaf, haiku, 10 turns, $0.10):
Check if niche already scraped → run Google Maps scraping tool → update state → report results

**`score_lead`** (execution leaf, haiku, 4 turns, $0.05):
Call the scoring tool → return score. No reasoning, no knowledge loading.

## What's Next

- [State Graphs](./state-graphs.md) — Declarative, code-free workflow definitions as YAML graph tools
- [Thread Lifecycle](./thread-lifecycle.md) — How threads are created, executed, and finalized
- [Spawning Children](./spawning-children.md) — How to spawn, coordinate, and collect results
- [Safety and Limits](./safety-and-limits.md) — Cost controls and the SafetyHarness
- [Permissions and Capabilities](./permissions-and-capabilities.md) — Capability tokens and fail-closed security
- [Continuation and Resumption](./continuation-and-resumption.md) — Context limits and thread handoffs
- [Building a Pipeline](./building-a-pipeline.md) — Step-by-step tutorial
