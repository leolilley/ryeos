<!-- rye:signed:2026-02-21T05:56:40Z:b3b34121f1eabf2747d1003c6cab5df33c30c9079a8b344c77773f33aea41dd2:VSP3qU0nJNW_ZLHL438R15jhSMo16r-kVCDcPZbo1MboScZGUlXiLfc8BH383YFigzLeIYOcd5tKCvWgfN4ODw==:9fbfabe975fa5a7f -->

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
            "async_exec": True
        })
# → collect thread_ids

# Wait phase
rye_execute(item_id="rye/agent/threads/orchestrator",
    parameters={"operation": "wait_threads", "thread_ids": [...], "timeout": 300})

# Collect phase
rye_execute(item_id="rye/agent/threads/orchestrator",
    parameters={"operation": "aggregate_results", "thread_ids": [...]})
```

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
6. **Test sync first** — debug with `async_exec: false` before switching to async
