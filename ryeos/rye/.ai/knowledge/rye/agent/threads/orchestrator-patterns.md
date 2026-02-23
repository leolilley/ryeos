<!-- rye:signed:2026-02-23T00:43:10Z:be6cf4cbb868dd2dd114d3b5997062ce4a701ab4baee4836800948c518bf8170:qLiGVKYFfcpDoldyIPtMC7cVw4_eDLeI3rl_G0KTaq7qEY9a5OCne4AT8metI8LyHTjdwTeoFfDmf5sIi_-JAQ==:9fbfabe975fa5a7f -->

```yaml
id: orchestrator-patterns
title: Orchestrator Patterns
entry_type: pattern
category: rye/agent/threads
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - orchestrator
  - patterns
  - pipeline
  - design
references:
  - spawning-patterns
  - thread-lifecycle
  - "docs/orchestration/building-a-pipeline.md"
```

# Orchestrator Patterns

When to use each directive role, how to design hierarchies, and real patterns for building pipelines.

## Three Roles

| Role             | Model    | Purpose                                  | Spawns Children? |
|------------------|----------|------------------------------------------|-------------------|
| Orchestrator     | sonnet   | Coordinate full workflow, manage state   | Yes               |
| Sub-orchestrator | sonnet   | Coordinate a subset (phase/domain)       | Yes               |
| Execution leaf   | haiku    | Call one tool and return                 | No                |

### When to Use Each

**Root orchestrator** when:
- Coordinates full end-to-end workflow
- Manages state across phases
- Makes high-level decisions (which targets, whether to retry)

**Sub-orchestrator** when:
- A phase needs its own spawn/wait/aggregate cycle
- Complex enough to warrant independent coordination
- E.g., `qualify_leads` needs scrape → score → filter → save

**Execution leaf** when:
- Calls one tool and returns
- No coordination, no children
- E.g., `discover_leads` calls scraper, `score_lead` calls scorer

**Rule of thumb:** If it spawns children → orchestrator. If it calls a tool and returns → leaf. Unsure → start as leaf, promote when complexity grows.

## Directive Hierarchy Design

Map out the tree first. Each node is a directive with clear responsibility:

```
run_lead_pipeline (root orchestrator)
├── discover_leads (execution leaf) × N niches
├── qualify_leads (sub-orchestrator)
│   ├── scrape_website (execution leaf) × N leads
│   └── score_lead (execution leaf) × N leads
├── prepare_outreach (sub-orchestrator)
│   ├── enrich_contact (execution leaf) × N leads
│   └── generate_email (execution leaf) × N leads
└── update_pipeline_state (execution leaf)
```

## Model + Budget Table

| Role             | Directive              | Model  | Turns | Spend  |
|------------------|------------------------|--------|-------|--------|
| Root orchestrator| `run_lead_pipeline`    | sonnet | 30    | $3.00  |
| Sub-orchestrator | `qualify_leads`        | sonnet | 20    | $1.00  |
| Sub-orchestrator | `prepare_outreach`     | sonnet | 15    | $0.80  |
| Execution leaf   | `discover_leads`       | haiku  | 10    | $0.10  |
| Execution leaf   | `scrape_website`       | haiku  | 8     | $0.05  |
| Execution leaf   | `score_lead`           | haiku  | 4     | $0.05  |
| Execution leaf   | `update_pipeline_state`| haiku  | 4     | $0.05  |

### Budget Math

Root at $3.00 covers itself (~$0.30 for 30 sonnet turns) + all children:
- 5× discover_leads = $0.50
- 1× qualify_leads = $1.00
- 1× prepare_outreach = $0.80
- 1× update_state = $0.05
- **Total ≈ $2.65**, $3.00 ceiling provides margin

## Standard Patterns

### Execution Leaf Pattern

```
check state → call tool → save output → report
```

```xml
<model tier="haiku" />
<limits max_turns="4–10" />
<permissions>
  <execute><tool>specific.tool.name</tool></execute>
</permissions>
```

Process steps:
1. Check if work already done (load state)
2. Call the tool
3. Save result to `.ai/data/`
4. Report outcome

### Sub-Orchestrator Pattern

```
load knowledge → read input → spawn children → wait → aggregate → process → save
```

```xml
<model tier="sonnet" />
<limits max_turns="15–20" />
<permissions>
  <execute>
    <tool>rye.agent.threads.thread_directive</tool>
    <tool>rye.agent.threads.orchestrator</tool>
  </execute>
  <load><knowledge>domain.*</knowledge></load>
</permissions>
```

Process steps:
1. Load domain knowledge and scoring frameworks
2. Read input data file
3. Spawn children (async) for each item
4. Wait for all children
5. Aggregate and process results
6. Save qualified/enriched output

### Root Orchestrator Pattern

```
load state → select targets → spawn discovery → qualify → outreach → update state → report → record learnings
```

```xml
<model tier="sonnet" />
<limits max_turns="30" />
<permissions>
  <execute>
    <tool>rye.agent.threads.thread_directive</tool>
    <tool>rye.agent.threads.orchestrator</tool>
  </execute>
  <search>
    <directive>domain.*</directive>
    <knowledge>domain.*</knowledge>
  </search>
  <load><knowledge>domain.*</knowledge></load>
</permissions>
```

## Parallel Fan-Out

Spawn N children async, wait, collect:

```python
# Spawn phase
for niche in niches:
    rye_execute(item_id="rye/agent/threads/thread_directive",
        parameters={
            "directive_name": "domain/discover",
            "inputs": {"niche": niche},
            "limit_overrides": {"turns": 10, "spend": 0.10},
            "async": True
        })
# → collect thread_ids

# Wait phase
rye_execute(item_id="rye/agent/threads/orchestrator",
    parameters={"operation": "wait_threads", "thread_ids": [...], "timeout": 300})

# Collect phase
rye_execute(item_id="rye/agent/threads/orchestrator",
    parameters={"operation": "aggregate_results", "thread_ids": [...]})
```

## Cross-Thread Context Wiring

Orchestrators pass dependency thread IDs as `inputs` when spawning child directives. The child directive's hooks reference them via interpolation, resolved by `interpolate_action()` which expands `${...}` in both `item_id` and `params` fields.

**Example:** Orchestrator completes Wave 0 (scaffold), gets `thread_id` back, then spawns Wave 1:

```python
rye_execute(item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_name": "project/implement_feature",
        "inputs": {"scaffold_thread_id": "scaffold_project/scaffold_project-1740200000"}
    })
```

In the child directive's `thread_started` hook:

```xml
<hooks>
  <hook id="inject_scaffold" event="thread_started">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.scaffold_thread_id}" />
  </hook>
</hooks>
```

The hook loads the knowledge entry from the scaffold thread — deterministic, zero API calls for context wiring. Any thread ID passed as an input can be referenced this way to pull context from a prior phase.

## Graph Hooks vs Directive Hooks

When combining state graphs with thread orchestration, two hook systems operate at different layers:

**Graph hooks** (YAML in `config.hooks`) handle pipeline-level events — progress logging, error handling, pipeline completion. The state graph defines the deterministic wave structure.

**Directive hooks** (XML in each directive's `<metadata>`) handle thread-level events — knowledge injection at startup (`thread_started`), summarization on completion (`after_complete`), context re-injection after handoff (`thread_continued`).

Example — a graph node spawns a thread directive, and the directive's hooks wire in knowledge:

```yaml
# Graph node (YAML) — spawns the thread
implement_api:
  action:
    primary: execute
    item_type: tool
    item_id: rye/agent/threads/thread_directive
    params:
      directive_name: project/implement_api
      inputs:
        scaffold_thread_id: "${state.scaffold_thread_id}"
        database_thread_id: "${state.database_thread_id}"
  assign:
    api_thread_id: "${result.thread_id}"
  next: implement_dashboard
```

```xml
<!-- Directive hooks (XML) — wire knowledge into the thread -->
<hooks>
  <hook id="inject_scaffold" event="thread_started">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.scaffold_thread_id}" />
  </hook>
  <hook id="inject_database" event="thread_started">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.database_thread_id}" />
  </hook>
</hooks>
```

The graph decides **what** runs and **when** (deterministic routing). The directive hooks decide **what context each thread sees** (knowledge injection). Both use the same underlying infrastructure (`condition_evaluator`, `interpolation`), but serve different purposes at different layers.

| Concern                    | Hook System    | Format              | Events                                        |
| -------------------------- | -------------- | ------------------- | --------------------------------------------- |
| Pipeline progress/errors   | Graph hook     | YAML `config.hooks` | `graph_started`, `after_step`, `error`        |
| Thread knowledge injection | Directive hook | XML `<hooks>`       | `thread_started`, `thread_continued`          |
| Thread summarization       | Directive hook | XML `<hooks>`       | `after_complete`                              |

## Sequential Phases

Some phases must run in order (qualification before outreach):

```python
# Phase 1: qualify (synchronous — blocks until done)
qualify_result = rye_execute(item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_name": "domain/qualify",
        "inputs": {"leads_file": "..."},
        "limit_overrides": {"turns": 20, "spend": 1.00}
    })

# Phase 2: outreach (only after qualification completes)
outreach_result = rye_execute(item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_name": "domain/outreach",
        "inputs": {"qualified_file": qualify_result["output_file"]},
        "limit_overrides": {"turns": 15, "spend": 0.80}
    })
```

## Error Handling in Orchestrators

```
After wait_threads:
  - Check each result individually
  - Log failures with context (which item, what error)
  - Continue with partial results
  - Report failure count in summary
  - Do NOT fail the entire pipeline for partial failures
```

## Design Principles

1. **Build bottom-up** — write and test leaves before orchestrators
2. **One tool per leaf** — if it needs two tools, consider sub-orchestrator
3. **State in files, not memory** — save to `.ai/data/` for resumability
4. **Budget with margin** — 10–20% above expected total for retries/handoffs
5. **Fail forward** — handle child failures gracefully, continue with partial results
6. **Test sync first** — debug with `async: false` before switching to async
