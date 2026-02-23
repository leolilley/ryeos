```yaml
id: building-a-pipeline
title: "Building a Pipeline"
description: Step-by-step guide to building a multi-phase orchestrated pipeline
category: orchestration
tags: [pipeline, tutorial, orchestration, practical]
version: "1.1.0"
```

# Building a Pipeline

This tutorial walks through building a multi-phase orchestrated pipeline using the agency-kiwi lead generation pipeline as a concrete example. By the end, you'll understand how to design, structure, and run an orchestration tree.

## Step 1: Design the Directive Hierarchy

Start by mapping out the tree. Each node is a directive with a clear responsibility:

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

### How to Decide What Goes Where

**Make it a root orchestrator when** it coordinates the full workflow end-to-end, manages state across phases, and needs to make high-level decisions (which niches to target, whether to retry failures).

**Make it a sub-orchestrator when** a phase is complex enough to need its own spawn/wait/aggregate cycle. `qualify_leads` needs to scrape websites, score leads, filter, and save — that's a multi-step coordination task.

**Make it an execution leaf when** it calls one tool and returns. `discover_leads` calls the Google Maps scraper. `score_lead` calls the scoring tool. No coordination, no children.

**Rule of thumb:** If a step requires spawning children, it's an orchestrator. If it calls a tool and returns, it's a leaf. If you're unsure, start as a leaf and promote to sub-orchestrator when complexity grows.

## Step 2: Choose Models and Limits

Match the model to the task complexity. Match the budget to the expected cost.

| Role | Directive | Model | Turns | Spend | Why |
|------|-----------|-------|-------|-------|-----|
| Orchestrator | `run_lead_pipeline` | sonnet | 30 | $3.00 | Multi-phase coordination, state reasoning |
| Sub-orchestrator | `qualify_leads` | sonnet | 20 | $1.00 | Spawn/wait/aggregate cycle |
| Sub-orchestrator | `prepare_outreach` | sonnet | 15 | $0.80 | Similar coordination |
| Execution leaf | `discover_leads` | haiku | 10 | $0.10 | Call scraper, save results |
| Execution leaf | `scrape_website` | haiku | 8 | $0.05 | Call scraper, return HTML |
| Execution leaf | `score_lead` | haiku | 4 | $0.05 | Call scorer, return number |
| Execution leaf | `enrich_contact` | haiku | 6 | $0.05 | Look up contact info |
| Execution leaf | `generate_email` | haiku | 6 | $0.05 | Generate personalized email |
| Execution leaf | `update_pipeline_state` | haiku | 4 | $0.05 | Write state file |

**Why orchestrators use sonnet:** They need to reason about state (which niches are done? which failed?), make coordination decisions (should I retry? how many to batch?), and handle complex data flows between phases.

**Why leaves use haiku:** They execute a single tool call. No reasoning about state, no coordination decisions. Haiku is fast and cheap — a leaf that runs in 4 turns costs ~$0.01-0.03.

**Budget math:** The root at $3.00 needs to cover itself (~$0.30 for 30 sonnet turns) plus all children. If it spawns 5 discover_leads ($0.50), 1 qualify_leads ($1.00), 1 prepare_outreach ($0.80), and 1 update_state ($0.05) = ~$2.65. The $3.00 ceiling provides margin.

## Step 3: Define Permissions

Each directive gets the minimum capabilities it needs.

### Root Orchestrator Permissions

```xml
<permissions>
  <execute>
    <tool>rye.agent.threads.thread_directive</tool>
    <tool>rye.agent.threads.orchestrator</tool>
  </execute>
  <search>
    <directive>agency-kiwi.*</directive>
    <knowledge>agency-kiwi.*</knowledge>
  </search>
  <load>
    <knowledge>agency-kiwi.*</knowledge>
  </load>
</permissions>
```

The root needs:
- `thread_directive` — to spawn children
- `orchestrator` — to wait for children, aggregate results, read transcripts
- `search/load` agency-kiwi — to find and load state/knowledge

### Sub-Orchestrator Permissions

```xml
<permissions>
  <execute>
    <tool>rye.agent.threads.thread_directive</tool>
    <tool>rye.agent.threads.orchestrator</tool>
  </execute>
  <load>
    <knowledge>agency-kiwi.*</knowledge>
  </load>
</permissions>
```

Sub-orchestrators need the same spawn/wait tools. They load knowledge but don't need to search for directives (the root tells them what to do).

### Execution Leaf Permissions

```xml
<!-- discover_leads -->
<permissions>
  <execute>
    <tool>scraping.gmaps.scrape_gmaps</tool>
  </execute>
  <load>
    <knowledge>agency-kiwi.*</knowledge>
  </load>
</permissions>

<!-- score_lead — tightest possible -->
<permissions>
  <execute>
    <tool>analysis.score_ghl_opportunity</tool>
  </execute>
</permissions>
```

Leaves get exactly the tool they call. `score_lead` doesn't even load knowledge — it just calls the scoring tool and returns.

## Step 4: Write Execution Leaves First

Build from the bottom up. Leaves are the simplest directives and the foundation of your pipeline.

### `discover_leads.md` — Full Example

````markdown
<!-- rye:signed:... -->
# Discover Leads

Scrape Google Maps for businesses in a specific niche and city. Save raw leads to the pipeline data directory.

```xml
<directive name="discover_leads" version="1.0.0">
  <metadata>
    <description>Scrape Google Maps for leads in a niche/city, save results</description>
    <category>agency-kiwi/leads</category>
    <author>agency-kiwi</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022" />
    <limits max_turns="10" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>scraping.gmaps.scrape_gmaps</tool>
      </execute>
      <load>
        <knowledge>agency-kiwi.*</knowledge>
      </load>
    </permissions>
  </metadata>

  <inputs>
    <input name="niche" type="string" required="true">
      Business niche to search (e.g., "plumbers", "dentists")
    </input>
    <input name="city" type="string" required="true">
      City to search in (e.g., "Dunedin")
    </input>
    <input name="max_results" type="integer" required="false" default="20">
      Maximum number of leads to scrape
    </input>
  </inputs>

  <outputs>
    <output name="leads_file">Path to the saved leads JSON file</output>
    <output name="lead_count">Number of leads discovered</output>
  </outputs>
</directive>
```

<process>
  <step name="check_existing">
    Check if leads have already been scraped for this niche/city combination.
    Load the pipeline state to see if {input:niche} in {input:city} has been processed.

    `rye_load(item_type="knowledge", item_id="agency-kiwi/state/pipeline_state")`

    If already scraped, report "already done" and return the existing file path.
  </step>

  <step name="scrape">
    Run the Google Maps scraper for {input:niche} businesses in {input:city}.

    `rye_execute(item_type="tool", item_id="scraping/gmaps/scrape_gmaps", parameters={"query": "{input:niche} in {input:city}", "max_results": {input:max_results}})`
  </step>

  <step name="save_results">
    Save the scraped leads to `.ai/data/agency-kiwi/leads/{input:niche}_{input:city}.json`.

    `rye_execute(item_type="tool", item_id="rye/file-system/fs_write", parameters={"path": ".ai/data/agency-kiwi/leads/{input:niche}_{input:city}.json", "content": "<JSON of scraped leads>"})`
  </step>

  <step name="report">
    Report:
    - Number of leads found
    - Path to the saved file
    - Any issues encountered (no results, API errors, etc.)
  </step>
</process>

<success_criteria>
  <criterion>Leads scraped and saved to .ai/data/agency-kiwi/leads/</criterion>
  <criterion>Lead count reported</criterion>
</success_criteria>
````

**Pattern:** check state → call tool → save output → report. This is the standard leaf pattern.

### Structured Returns with `directive_return`

When a directive declares `<outputs>`, the thread prompt instructs the LLM to call `directive_return` via `rye_execute` when all steps are complete. This provides structured key-value outputs that parent orchestrators can consume programmatically.

For example, the `discover_leads` directive declares:

```xml
<outputs>
  <output name="leads_file">Path to the saved leads JSON file</output>
  <output name="lead_count">Number of leads discovered</output>
</outputs>
```

When the LLM finishes, it calls:

```python
rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return",
    parameters={"leads_file": ".ai/data/leads.json", "lead_count": "15"})
```

The parent orchestrator receives these as `result["outputs"]` when waiting on the thread, enabling reliable data flow between pipeline stages.

If the LLM omits required output fields, the runner returns an error asking it to retry — ensuring the contract between parent and child is enforced.

### `score_lead.md` — Minimal Leaf

````markdown
<!-- rye:signed:... -->
# Score Lead

Score a single lead using the GHL opportunity analysis tool. Returns the score directly.

```xml
<directive name="score_lead" version="1.0.0">
  <metadata>
    <description>Score a lead using GHL opportunity analysis</description>
    <category>agency-kiwi/leads</category>
    <author>agency-kiwi</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022" />
    <limits max_turns="4" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>analysis.score_ghl_opportunity</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="lead_data" type="string" required="true">
      JSON string of lead data (name, website, category, location, etc.)
    </input>
  </inputs>

  <outputs>
    <output name="score">Numeric score 0-100</output>
    <output name="tier">Classification: hot, warm, cold</output>
  </outputs>
</directive>
```

<process>
  <step name="score">
    Call the scoring tool with the lead data. Return the result directly.

    `rye_execute(item_type="tool", item_id="analysis/score_ghl_opportunity", parameters={"lead_data": {input:lead_data}})`

    Return the score and tier. Do not add commentary or reasoning — just the tool result.
  </step>
</process>
````

**Pattern:** call tool → return result. The simplest possible leaf. No state checking, no knowledge loading, no saving. 4 turns maximum: the LLM calls the tool, gets the result, and returns it.

## Step 5: Write Sub-Orchestrators

Sub-orchestrators coordinate a subset of work. They spawn leaves, wait, and aggregate.

### `qualify_leads.md` — Full Example

````markdown
<!-- rye:signed:... -->
# Qualify Leads

Take raw discovered leads, scrape their websites, score each one, and produce a qualified leads file sorted by opportunity score.

```xml
<directive name="qualify_leads" version="1.0.0">
  <metadata>
    <description>Scrape websites and score leads to produce qualified leads list</description>
    <category>agency-kiwi/leads</category>
    <author>agency-kiwi</author>
    <model tier="sonnet" />
    <limits max_turns="20" max_tokens="200000" />
    <permissions>
      <execute>
        <tool>rye.agent.threads.thread_directive</tool>
        <tool>rye.agent.threads.orchestrator</tool>
      </execute>
      <load>
        <knowledge>agency-kiwi.*</knowledge>
      </load>
    </permissions>
  </metadata>

  <inputs>
    <input name="leads_file" type="string" required="true">
      Path to the raw leads JSON file from discovery phase
    </input>
    <input name="output_file" type="string" required="true">
      Path to write qualified leads JSON
    </input>
  </inputs>

  <outputs>
    <output name="qualified_count">Number of qualified leads</output>
    <output name="output_file">Path to qualified leads file</output>
  </outputs>
</directive>
```

<process>
  <step name="load_knowledge">
    Load the GHL sales framework and service tier definitions for scoring context.

    `rye_load(item_type="knowledge", item_id="agency-kiwi/frameworks/ghl_sales")`
    `rye_load(item_type="knowledge", item_id="agency-kiwi/frameworks/service_tiers")`
  </step>

  <step name="read_leads">
    Read the leads file at {input:leads_file}. Parse the JSON.
    Separate leads into two groups:
    - Leads WITH a website URL
    - Leads WITHOUT a website URL
  </step>

  <step name="scrape_websites">
    For each lead WITH a website URL, spawn a scrape_website child thread:

    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "agency-kiwi/leads/scrape_website", "inputs": {"url": "<lead_website_url>", "lead_id": "<lead_id>"}, "limit_overrides": {"turns": 8, "spend": 0.05}, "async": true})`

    Collect all thread_ids. Then wait for all:

    `rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator", parameters={"operation": "wait_threads", "thread_ids": ["<thread_ids>"], "timeout": 120})`

    Aggregate results:

    `rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator", parameters={"operation": "aggregate_results", "thread_ids": ["<thread_ids>"]})`
  </step>

  <step name="score_leads">
    For each lead (with or without website data), spawn a score_lead child thread:

    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "agency-kiwi/leads/score_lead", "inputs": {"lead_data": "<lead_json_with_website_data>"}, "limit_overrides": {"turns": 6, "spend": 0.05}, "async": true})`

    Wait for all scoring threads, aggregate results.
  </step>

  <step name="filter_and_classify">
    Combine scraping results with scoring results for each lead.
    Sort by score descending.
    Classify into tiers:
    - Hot: score >= 80
    - Warm: score >= 50
    - Cold: score < 50

    Filter out leads with score < 30 (not worth pursuing).
  </step>

  <step name="save_qualified">
    Save the qualified leads (with scores and tiers) to {input:output_file}.
    Report:
    - Total leads processed
    - Qualified count per tier (hot/warm/cold)
    - Leads filtered out
  </step>
</process>

<success_criteria>
  <criterion>All leads scored with tier classification</criterion>
  <criterion>Qualified leads saved to {input:output_file}</criterion>
  <criterion>Lead count per tier reported</criterion>
</success_criteria>
````

**Pattern:** load knowledge → read input → spawn children → wait → aggregate → process → save. This is the standard sub-orchestrator pattern.

## Step 6: Write the Root Orchestrator

The root orchestrator manages the full pipeline lifecycle.

### `run_lead_pipeline.md` — Full Example

````markdown
<!-- rye:signed:... -->
# Run Lead Pipeline

Execute the full lead generation pipeline for a city: discover leads across niches, qualify them, prepare outreach, and update pipeline state.

```xml
<directive name="run_lead_pipeline" version="1.0.0">
  <metadata>
    <description>Full lead generation pipeline: discover → qualify → outreach → report</description>
    <category>agency-kiwi/orchestrator</category>
    <author>agency-kiwi</author>
    <model tier="sonnet" />
    <limits max_turns="30" max_tokens="200000" />
    <permissions>
      <execute>
        <tool>rye.agent.threads.thread_directive</tool>
        <tool>rye.agent.threads.orchestrator</tool>
      </execute>
      <search>
        <directive>agency-kiwi.*</directive>
        <knowledge>agency-kiwi.*</knowledge>
      </search>
      <load>
        <knowledge>agency-kiwi.*</knowledge>
      </load>
    </permissions>
  </metadata>

  <inputs>
    <input name="city" type="string" required="true">
      Target city for lead generation (e.g., "Dunedin")
    </input>
    <input name="batch_size" type="integer" required="false" default="5">
      Number of niches to process per run
    </input>
  </inputs>

  <outputs>
    <output name="leads_discovered">Total leads discovered across all niches</output>
    <output name="leads_qualified">Total qualified leads</output>
    <output name="pipeline_status">Summary of pipeline state after this run</output>
  </outputs>
</directive>
```

<process>
  <step name="load_state">
    Load pipeline state and configuration:

    `rye_load(item_type="knowledge", item_id="agency-kiwi/state/pipeline_state")`
    `rye_load(item_type="knowledge", item_id="agency-kiwi/config/niche_list")`
    `rye_load(item_type="knowledge", item_id="agency-kiwi/config/city_data")`
    `rye_load(item_type="knowledge", item_id="agency-kiwi/learnings/pipeline_learnings")`

    Determine which niches have been processed for {input:city} and which remain.
  </step>

  <step name="select_batch">
    From the remaining unprocessed niches for {input:city}, select up to {input:batch_size} niches.
    Prioritize niches that historically have higher lead counts (use pipeline_learnings data).
    If no niches remain, report "pipeline complete for {input:city}" and stop.
  </step>

  <step name="discover">
    For each selected niche, spawn a discover_leads child thread:

    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "agency-kiwi/leads/discover_leads", "inputs": {"niche": "<niche>", "city": "{input:city}"}, "limit_overrides": {"turns": 10, "spend": 0.10}, "async": true})`

    Collect all thread_ids.

    Wait for all discovery threads:
    `rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator", parameters={"operation": "wait_threads", "thread_ids": ["<discovery_thread_ids>"], "timeout": 300})`

    Note any failures — log the niche and error but continue with successful results.
  </step>

  <step name="qualify">
    Spawn the qualify_leads sub-orchestrator with the combined leads files:

    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "agency-kiwi/leads/qualify_leads", "inputs": {"leads_file": "<combined_leads_path>", "output_file": ".ai/data/agency-kiwi/qualified/{input:city}_qualified.json"}, "limit_overrides": {"turns": 20, "spend": 1.00}})`

    This runs synchronously — wait for qualification to complete before outreach.
  </step>

  <step name="outreach">
    Spawn the prepare_outreach sub-orchestrator:

    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "agency-kiwi/outreach/prepare_outreach", "inputs": {"qualified_file": ".ai/data/agency-kiwi/qualified/{input:city}_qualified.json", "output_dir": ".ai/data/agency-kiwi/outreach/{input:city}/"}, "limit_overrides": {"turns": 15, "spend": 0.80}})`
  </step>

  <step name="update_state">
    Spawn update_pipeline_state to persist results:

    `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive_name": "agency-kiwi/state/update_pipeline_state", "inputs": {"city": "{input:city}", "niches_processed": "<list>", "leads_discovered": "<count>", "leads_qualified": "<count>"}, "limit_overrides": {"turns": 4, "spend": 0.05}})`
  </step>

  <step name="report">
    Summarize the pipeline run:
    - Niches processed and their lead counts
    - Total leads discovered vs qualified
    - Tier breakdown (hot/warm/cold)
    - Any failures or issues
    - Remaining niches for {input:city}
  </step>

  <step name="record_learnings">
    Record insights from this run as pipeline learnings:
    - Which niches yielded the most/fewest leads
    - Common failure patterns
    - Score distribution observations

    `rye_execute(item_type="tool", item_id="rye/file-system/fs_write", parameters={"path": ".ai/knowledge/agency-kiwi/learnings/pipeline_learnings.md", "content": "<updated learnings>"})`
  </step>
</process>

<success_criteria>
  <criterion>At least one niche successfully discovered</criterion>
  <criterion>Qualification completed for discovered leads</criterion>
  <criterion>Pipeline state updated with progress</criterion>
  <criterion>Summary report generated</criterion>
</success_criteria>
````

**Pattern:** load state → select targets → spawn discovery (parallel) → spawn qualification (sequential) → spawn outreach (sequential) → update state → report → record learnings. This is the full orchestrator lifecycle.

## Step 7: Run It

### Start the Pipeline

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_name": "agency-kiwi/orchestrator/run_lead_pipeline",
        "inputs": {"city": "Dunedin", "batch_size": 5},
        "limit_overrides": {"turns": 30, "spend": 3.00}
    }
)
```

This starts the root orchestrator synchronously. It will spawn children as needed.

### Run Asynchronously (Background)

```python
result = rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_name": "agency-kiwi/orchestrator/run_lead_pipeline",
        "inputs": {"city": "Dunedin", "batch_size": 5},
        "limit_overrides": {"turns": 30, "spend": 3.00},
        "async": True
    }
)
# result = {"thread_id": "agency-kiwi/orchestrator/run_lead_pipeline-1739820456", "status": "running"}
```

### Check Status

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "get_status",
        "thread_id": "agency-kiwi/orchestrator/run_lead_pipeline-1739820456"
    }
)
```

### Read the Transcript

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "read_transcript",
        "thread_id": "agency-kiwi/orchestrator/run_lead_pipeline-1739820456",
        "tail_lines": 100
    }
)
```

### Resume After Failure

If the pipeline errored (API key expired, network issue), fix the problem and resume:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "resume_thread",
        "thread_id": "agency-kiwi/orchestrator/run_lead_pipeline-1739820456",
        "message": "API key has been rotated. Resume from where you left off."
    }
)
```

## Design Principles

1. **Build bottom-up.** Write and test leaves before orchestrators. A broken leaf is easy to debug. A broken orchestrator with broken leaves is not.

2. **One tool per leaf.** If a leaf needs two tools, consider whether it should be a sub-orchestrator or if the tools can be combined.

3. **State in files, not in memory.** Orchestrators save state to `.ai/data/` so pipelines can be resumed. Don't rely on conversation context for state persistence.

4. **Budget with margin.** Set the root budget 10-20% above the expected total. Unexpected retries, longer conversations, and context handoffs all consume budget.

5. **Fail forward.** Orchestrators should handle child failures gracefully — log them and continue with partial results rather than failing the entire pipeline.

6. **Test with `async: false` first.** Debug the pipeline synchronously before switching to async. Synchronous execution gives you the full result inline.
