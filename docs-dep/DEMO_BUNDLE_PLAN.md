# RYE OS Demo Bundle Plan

**Date:** 2026-02-17
**Status:** Planned (post-v0.1.0)
**Bundle ID:** `rye-demos`
**Distribution:** Registry only — NOT bundled with rye-os package

---

## Purpose

Demos are full-depth showcase projects that demonstrate what RYE OS can do. They are NOT toy examples or basic tool-call tutorials — those are covered by the bundled guides.

Demos show:

- Multi-phase pipelines with state management
- Child thread spawning and orchestration
- Knowledge-driven decision making
- Self-improving directive loops
- Real-world tool chaining across categories

Users pull demos from the registry:

```
User: "show me what rye can do"
LLM: rye_search(scope="directive", query="demo", space="registry")
     → pulls demo directives + supporting tools + knowledge
```

---

## Bundle Structure

Each demo is a self-contained mini-project with its own directives, tools, and knowledge. They are grouped by what they showcase.

```
rye-demos/
├── .ai/
│   ├── directives/
│   │   └── rye-demos/
│   │       ├── pipelines/
│   │       │   ├── research_pipeline/
│   │       │   │   ├── run_research.md
│   │       │   │   ├── gather_sources.md
│   │       │   │   ├── analyze_source.md
│   │       │   │   ├── synthesize_findings.md
│   │       │   │   └── generate_report.md
│   │       │   │
│   │       │   ├── code_review_pipeline/
│   │       │   │   ├── run_review.md
│   │       │   │   ├── analyze_file.md
│   │       │   │   ├── check_patterns.md
│   │       │   │   ├── score_quality.md
│   │       │   │   └── generate_review.md
│   │       │   │
│   │       │   └── data_processing_pipeline/
│   │       │       ├── run_etl.md
│   │       │       ├── extract_data.md
│   │       │       ├── transform_data.md
│   │       │       ├── validate_output.md
│   │       │       └── load_results.md
│   │       │
│   │       ├── evolution/
│   │       │   ├── self_building_tool/
│   │       │   │   ├── run_self_build.md
│   │       │   │   ├── analyze_need.md
│   │       │   │   ├── generate_tool.md
│   │       │   │   ├── test_tool.md
│   │       │   │   └── iterate_tool.md
│   │       │   │
│   │       │   ├── directive_evolution/
│   │       │   │   ├── run_evolution.md
│   │       │   │   ├── execute_directive.md
│   │       │   │   ├── analyze_failure.md
│   │       │   │   ├── mutate_directive.md
│   │       │   │   └── evaluate_improvement.md
│   │       │   │
│   │       │   └── knowledge_growth/
│   │       │       ├── run_knowledge_loop.md
│   │       │       ├── execute_task.md
│   │       │       ├── extract_learnings.md
│   │       │       ├── store_knowledge.md
│   │       │       └── apply_knowledge.md
│   │       │
│   │       ├── integration/
│   │       │   ├── mcp_discovery/
│   │       │   │   ├── run_mcp_integration.md
│   │       │   │   ├── discover_server.md
│   │       │   │   ├── register_tools.md
│   │       │   │   ├── test_tools.md
│   │       │   │   └── build_workflow.md
│   │       │   │
│   │       │   └── registry_roundtrip/
│   │       │       ├── run_roundtrip.md
│   │       │       ├── create_items.md
│   │       │       ├── push_to_registry.md
│   │       │       ├── pull_from_registry.md
│   │       │       └── execute_from_registry.md
│   │       │
│   │       └── orchestration/
│   │           ├── parallel_research/
│   │           │   ├── run_parallel_research.md
│   │           │   ├── spawn_researchers.md
│   │           │   ├── research_topic.md
│   │           │   ├── aggregate_findings.md
│   │           │   └── generate_synthesis.md
│   │           │
│   │           ├── multi_agent_build/
│   │           │   ├── run_build.md
│   │           │   ├── plan_architecture.md
│   │           │   ├── spawn_builders.md
│   │           │   ├── build_component.md
│   │           │   ├── integrate_components.md
│   │           │   └── verify_build.md
│   │           │
│   │           └── self_governing/
│   │               ├── run_governance.md
│   │               ├── propose_change.md
│   │               ├── evaluate_proposal.md
│   │               ├── apply_change.md
│   │               └── audit_system.md
│   │
│   ├── tools/
│   │   └── rye-demos/
│   │       ├── research/
│   │       │   ├── source_ranker.py
│   │       │   └── citation_formatter.py
│   │       ├── code-review/
│   │       │   ├── complexity_scorer.py
│   │       │   └── pattern_matcher.py
│   │       └── data/
│   │           ├── csv_parser.py
│   │           └── json_transformer.py
│   │
│   └── knowledge/
│       └── rye-demos/
│           ├── research/
│           │   ├── research_methodology.md
│           │   └── source_evaluation_criteria.md
│           ├── code-review/
│           │   ├── review_checklist.md
│           │   └── quality_rubric.md
│           └── evolution/
│               ├── mutation_strategies.md
│               └── fitness_criteria.md
```

---

## Demo Descriptions

### Pipelines

#### Research Pipeline

**What it shows:** Multi-phase orchestration with web search, content fetching, analysis, and synthesis. Each phase spawns child threads. Knowledge entries guide research methodology. Final output is a structured report.

**Tools used:** web/websearch, web/webfetch, file-system/write, agent/threads/thread_directive
**Highlights:** Thread spawning per source, knowledge-driven analysis criteria, state tracking across phases

#### Code Review Pipeline

**What it shows:** Automated code review that reads files, checks against known patterns from knowledge base, scores quality, and generates a structured review with actionable feedback.

**Tools used:** file-system/read, file-system/glob, bash/bash (for git diff), agent/threads/thread_directive
**Highlights:** Per-file parallel analysis, knowledge-driven pattern matching, custom scoring tools

#### Data Processing Pipeline

**What it shows:** ETL pipeline that extracts data from files, transforms it using custom tools, validates output, and writes results. Demonstrates tool chaining and error handling.

**Tools used:** file-system/read, file-system/write, bash/bash, custom tools
**Highlights:** Sequential phase execution, validation gates between phases, custom tool integration

### Evolution

#### Self-Building Tool

**What it shows:** LLM analyzes a need, generates a Python tool with proper metadata and CONFIG_SCHEMA, signs it, tests it, and iterates based on test failures. The tool it creates is fully functional within RYE.

**Tools used:** authoring/create_tool, primary/sign, primary/execute, file-system/read
**Highlights:** Meta-programming — the system builds itself. Test-driven iteration loop.

#### Directive Evolution

**What it shows:** Run a directive → capture failure → analyze what went wrong → mutate the directive (adjust steps, permissions, limits) → re-run → evaluate improvement. Repeat until success.

**Tools used:** primary/execute, primary/load, file-system/write, primary/sign
**Highlights:** Self-annealing pattern. The directive improves itself across generations.

#### Knowledge Growth

**What it shows:** Execute a task → extract learnings from the execution → store as knowledge entries → use those entries to inform the next task. Knowledge base grows organically from work.

**Tools used:** primary/execute, authoring/create_knowledge, primary/search, primary/load
**Highlights:** Feedback loop between execution and knowledge. System gets smarter over time.

### Integration

#### MCP Discovery

**What it shows:** Point at an external MCP server → discover its tools → register them → test each tool → build a workflow that chains the discovered tools together.

**Tools used:** mcp/discover, mcp/add_server, mcp/connect, file-system/write
**Highlights:** Dynamic tool discovery. Builds workflows from unknown tool sets.

#### Registry Roundtrip

**What it shows:** Create a set of items (directive + tool + knowledge) → push to registry → pull from registry into a different project space → execute the pulled items. Full lifecycle.

**Tools used:** authoring/create\_\*, registry/push, registry/pull, primary/execute
**Highlights:** End-to-end registry workflow. Demonstrates portability.

### Orchestration

#### Parallel Research

**What it shows:** Spawn N parallel research threads, each investigating a different aspect of a topic. Aggregate findings. Generate a synthesis that combines all threads.

**Tools used:** agent/thread_directive, web/websearch, web/webfetch, file-system/write
**Highlights:** Parallel thread execution, result aggregation, synthesis across independent work.

#### Multi-Agent Build

**What it shows:** Plan an architecture → decompose into components → spawn builder threads per component → integrate → verify. A software build orchestrated entirely by directives.

**Tools used:** agent/thread_directive, agent/orchestrator, file-system/\*, bash/bash
**Highlights:** Hierarchical orchestration. Parent → child → grandchild thread spawning.

#### Self-Governing System

**What it shows:** A directive governance system where changes to directives must be proposed, evaluated against criteria, tested, and approved before being applied. The system governs its own evolution.

**Tools used:** primary/load, primary/execute, primary/sign, file-system/write
**Highlights:** Autonomous governance. Meta-level control over directive evolution.

---

## Installation

Demos are published to the registry as the `rye-demos` bundle:

```
# Pull all demos
User: "pull the rye demos"
LLM: rye_execute(item_type="directive", item_id="rye/registry/pull",
       parameters={"item_id": "rye-demos/...", "item_type": "directive"})

# Or browse what's available
User: "what demos are available?"
LLM: rye_search(scope="directive.rye-demos.*", query="demo pipeline",
       space="registry")
```

Each demo is self-contained — pulling a demo's orchestrator directive also pulls its child directives, tools, and knowledge via the bundle manifest.

---

## Implementation Priority

| Priority | Demo                     | Reason                                             |
| -------- | ------------------------ | -------------------------------------------------- |
| 1        | Self-Building Tool       | Most immediately impressive — system builds itself |
| 2        | Research Pipeline        | Practical, shows real value                        |
| 3        | Directive Evolution      | Core differentiator — self-annealing               |
| 4        | Knowledge Growth         | Shows the learning loop                            |
| 5        | Parallel Research        | Shows threading power                              |
| 6        | MCP Discovery            | Shows integration story                            |
| 7        | Code Review Pipeline     | Practical, relatable                               |
| 8        | Registry Roundtrip       | Shows the ecosystem                                |
| 9        | Multi-Agent Build        | Advanced orchestration                             |
| 10       | Data Processing Pipeline | Standard but solid                                 |
| 11       | Self-Governing System    | Experimental/research                              |

---

_Document Status: Planning_
_Last Updated: 2026-02-17_
