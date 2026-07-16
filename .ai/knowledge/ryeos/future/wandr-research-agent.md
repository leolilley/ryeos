<!-- ryeos:signed:2026-07-16T03:25:48Z:a8c823d036fe883789322e3ce733b46e9f8bb88dc0f502d01be1707153717c74:0H/Jzw1V/0lCGfY8Mv8V1Xk32UQpSaD9gOTbQVcvnriKMl9jzMiXO5hq+2/u0K3nxu7jy981Qgx4Bl6SxSB+BQ==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
---
name: ryeos/future/wandr-research-agent
version: "0.1.0"
category: ryeos/future
tags: [wandr, research-agent, graph, directive, tool, knowledge, evidence, benchmark]
description: Future implementation design for a RyeOS-native WANDR wide-and-deep research agent with hierarchical coverage, parallel entity enrichment, evidence admission, durable dossiers, and benchmark-compatible JSONL output.
references: [ryeos/development/architecture, ryeos/development/signing]
---

# RyeOS-native WANDR research agent

## Status

Future implementation design. This document defines the intended RyeOS architecture and development
sequence; it does not describe an existing implementation.

## Objective

Build a RyeOS-native solver for WANDR (Wide ANd Deep Research), a benchmark of high-volume research
tasks that require both broad entity discovery and deep, source-backed enrichment.

The solver must:

- discover a large, sometimes open-ended set of qualifying entities;
- complete every required enrichment branch for each entity;
- keep identifiers consistent across related task nodes;
- attach a fetchable source URL and faithful page excerpts to every submitted record;
- reject weak or incomplete evidence before submission;
- maintain explicit coverage state instead of relying on a model to remember the run;
- resume safely after interruption;
- produce the exact JSONL files declared by each benchmark task;
- expose useful diagnostics for discovery, enrichment, evidence, identity, and coverage failures.

The implementation is a RyeOS program:

- directives perform bounded semantic decisions;
- tools perform search, fetching, extraction, validation, persistence, and other mechanical work;
- graphs own all control flow, fan-out, joins, retry paths, and stopping conditions;
- signed knowledge stores accepted dossiers and reusable research strategy;
- graph checkpoints and content-addressed artifacts hold operational run state and large intermediate
  data.

## Benchmark shape

A WANDR task is a hierarchy of qualification keys. A representative shape is:

```text
company(n) -> employee(m) -> url(k)
```

Every path ending in a URL is an independently judged record. A task may also contain sibling
subtasks that share identifiers with the root task. For example:

```text
company
├── appointment
│   └── authoritative appointment URL
└── listing status
    └── recognized listing-authority URL
```

The scorer gives partial credit for valid records and complete credit only when the required subtree
under a member is satisfied. This makes the top-level entity subtree the primary unit of work. A
solver that finds many entities but leaves their side branches incomplete can have reasonable soft
precision and poor hard recall.

The implementation must therefore optimize for complete member subtrees, not merely for a large flat
collection of plausible records.

## Core architecture

The solver has two coordinated scales:

1. **Discovery scale:** search lanes find and canonicalize a surplus pool of candidate top-level
   entities.
2. **Member scale:** one followed child execution owns one candidate and attempts to complete its
   entire required subtree.

The root graph owns global coverage and decides where to spend the next unit of work.

```text
load contract
    |
    v
compile task
    |
    v
plan discovery
    |
    v
search lanes -- parallel follow --> query workers
    |
    v
merge and canonicalize candidates
    |
    v
select enrichment batch
    |
    v
member fan-out -- parallel follow --> one child per top-level entity
    |                                  |
    |                                  +--> complete all branches
    |                                  +--> validate every record
    |                                  +--> persist accepted dossier
    v
merge member results
    |
    v
measure hierarchical coverage
    |---------------- discovery deficit ----> plan discovery
    |---------------- branch deficit --------> select enrichment batch
    |---------------- target reached --------> export
    +---------------- budget exhausted ------> export best admissible result
```

This separation prevents three common errors:

- using enrichment calls to rediscover the same candidates repeatedly;
- assigning related branches of one entity to unrelated workers;
- stopping because a narrative appears complete while the required hierarchy still contains holes.

## Primary invariants

### One member, one owner

A member child owns every task branch attached to one top-level entity. It may perform multiple
searches and fetches, but it returns a single dossier with a consistent canonical identifier.

Sibling task nodes must not independently invent identifiers for the same entity. The canonical
identifier is assigned before enrichment and passed into every branch.

### No concurrent writes to final output

Member children return structured results or persist isolated dossiers. They never append to shared
JSONL files. After the join, one deterministic exporter writes all required output files atomically.

### Evidence is admitted, not merely generated

A model-produced record is a proposal. It becomes accepted only after deterministic and semantic
admission checks pass.

### Control flow belongs to graphs

No directive owns the research loop. Directives receive bounded inputs, make one semantic decision,
and return typed outputs. Graphs decide whether to search again, enrich another batch, repair a
record, backfill a failed member, or stop.

### Mechanical repetition belongs to tools

Search calls, page retrieval, visible-text extraction, excerpt matching, URL normalization,
deduplication, coverage arithmetic, ledger updates, and JSONL serialization must not consume model
turns.

### Operational state and reusable knowledge are distinct

Run queues, fetched pages, attempt counters, and large candidate ledgers are operational state.
Accepted entity dossiers and generalizable research strategies are knowledge. A task worker may
write its isolated dossier, but it may not update shared strategy knowledge.

## Proposed item layout

```text
.ai/
├── graphs/wandr/
│   ├── solve.yaml
│   ├── discover_round.yaml
│   ├── search_lane.yaml
│   ├── member.yaml
│   ├── branch.yaml
│   ├── evidence_record.yaml
│   ├── consolidate.yaml
│   └── smoke.yaml
├── directives/wandr/
│   ├── compile_task.md
│   ├── plan_discovery.md
│   ├── extract_candidates.md
│   ├── qualify_candidate.md
│   ├── plan_member.md
│   ├── extract_evidence.md
│   ├── audit_record.md
│   ├── diagnose_gap.md
│   └── consolidate.md
├── tools/wandr/
│   ├── contract.py
│   ├── ledger.py
│   ├── search.py
│   ├── fetch.py
│   ├── visible_text.py
│   ├── excerpts.py
│   ├── canonicalize.py
│   ├── deduplicate.py
│   ├── coverage.py
│   ├── dossier.py
│   ├── persist.py
│   └── export.py
└── knowledge/wandr/
    ├── task-families/
    ├── source-strategies/
    └── runs/<run-id>/members/<entity-id>
```

The exact namespace may change during implementation, but the ownership boundaries should remain.

## Root solve graph

`graph:wandr/solve` is the sole top-level entry point. Its input should identify the task workspace
and provide or locate:

- the rendered task instruction;
- the task manifest;
- the ordered task-node names;
- the required output paths;
- run budgets and concurrency limits;
- an optional resume identifier.

Suggested input shape:

```json
{
  "workspace": "/workspace",
  "instruction_path": "instruction.md",
  "manifest_path": "tests/manifest.json",
  "required_file_paths": [
    "results_example.jsonl",
    "results_example.sidecar.jsonl"
  ],
  "run_id": "optional-stable-id",
  "limits": {
    "max_discovery_rounds": 8,
    "max_member_attempts": 200,
    "max_concurrency": 16
  }
}
```

The graph stages are:

1. load and validate the public task contract;
2. compile the contract into a typed research plan;
3. recall an existing run ledger when resuming;
4. plan and execute discovery rounds;
5. canonicalize and rank candidate entities;
6. fan out member enrichment in bounded batches;
7. merge completed, partial, and rejected dossiers;
8. compute hierarchy-aware coverage;
9. choose discovery, enrichment, repair, or termination;
10. export benchmark-compatible JSONL;
11. return a scorecard and diagnostic summary.

The root state should contain references to large artifacts rather than embedding all fetched text or
records directly in graph state.

## Task compiler

`directive:wandr/compile_task` converts the task instruction and manifest into a typed plan. It runs
once per task unless the compiled plan has already been persisted and remains valid for the same task
content hash.

Its outputs should include:

```json
{
  "task_nodes": [
    {
      "name": "example",
      "output_path": "results_example.jsonl",
      "key_path": ["company", "employee", "url"],
      "required_counts": {"company": 50, "employee": 2, "url": 1},
      "shared_keys": [],
      "requirements": [],
      "authority_requirements": [],
      "date_constraints": []
    }
  ],
  "root_key": "company",
  "canonical_identifier_policy": "...",
  "discovery_dimensions": [],
  "admission_requirements": [],
  "target_members": 50
}
```

The compiler does not search. It gives later stages an executable description of the public task.

The plan must preserve:

- task-node ordering;
- parent/subtask relationships;
- shared-key bindings;
- required multiplicities at every hierarchy level;
- universal WANDR rules;
- task-specific validity and evidence requirements;
- required output filenames.

The deterministic contract tool validates that every task node maps to exactly one output path and
that all required paths remain inside the task workspace.

## Discovery system

Discovery aims for a surplus candidate pool rather than the exact requested count. An entity that
looks promising in search results may later fail identity, scope, authority, date, fetchability, or
evidence checks.

A reasonable initial policy is:

```text
candidate target = max(requested members * 1.75, requested members + 20)
```

This is a starting heuristic, not a fixed benchmark rule. The coverage controller should adapt it
using observed qualification and completion rates.

### Discovery planning

`directive:wandr/plan_discovery` receives:

- the compiled task plan;
- prior query attempts and yields;
- known candidates and canonical aliases;
- measured discovery and branch deficits;
- remaining budgets.

It returns bounded query lanes. A lane is a coherent search strategy, not a single unstructured
request:

```json
{
  "lane_id": "issuer-newswire-cfo-april",
  "purpose": "discover appointment candidates from issuer-controlled announcements",
  "queries": ["...", "..."],
  "domain_constraints": [],
  "date_constraints": {},
  "expected_entity_type": "company",
  "stop_after_new_candidates": 25
}
```

Lanes should deliberately diversify by:

- terminology and synonyms;
- authority surface;
- geography;
- date interval;
- alphabetic or categorical partition;
- industry or source index;
- known long-tail discovery routes;
- unresolved coverage gaps.

### Search-lane children

`graph:wandr/search_lane` owns one lane. It performs query calls, normalizes results, extracts
candidate identities, and returns candidate observations with provenance.

It must not declare an entity qualified from a snippet alone. Search results are discovery evidence,
not final record evidence.

Suggested candidate observation:

```json
{
  "display_name": "Example Corporation",
  "candidate_key": "example corporation",
  "aliases": ["Example Corp."],
  "discovery_urls": ["https://example.test/page"],
  "discovery_query": "...",
  "qualification_hints": ["possible CFO appointment"],
  "source_rank": 3
}
```

### Candidate merging

`tool:wandr/canonicalize` performs deterministic normalization first. Ambiguous near-duplicates are
sent to a narrowly scoped directive that must choose merge or separate and provide a canonical
identifier. Its decision is recorded in the ledger so later rounds do not reconsider the same pair.

Canonicalization must never change identifiers after records have been exported. Before export, a
rename must update the entire member subtree atomically.

## Member enrichment

`graph:wandr/member` receives one canonical candidate plus the complete task plan. It attempts all
branches required beneath that entity.

The member graph should:

1. recall an existing dossier for the candidate;
2. perform a cheap candidate qualification pass;
3. instantiate every required branch and evidence slot;
4. plan searches for missing branches;
5. search and fetch sources;
6. propose records from visible page text;
7. run each proposal through the evidence admission graph;
8. retry specific failed requirements when useful;
9. compute member completeness;
10. persist and return the dossier.

The member graph returns explicit status:

```json
{
  "entity_id": "example corporation",
  "status": "complete",
  "records_by_task": {},
  "missing_slots": [],
  "rejected_records": [],
  "attempt_summary": {},
  "dossier_ref": "knowledge:wandr/runs/<run>/members/<id>"
}
```

Allowed terminal statuses should include:

- `complete`: the required subtree is satisfied;
- `partial`: at least one admitted record exists but required slots remain;
- `rejected`: the top-level entity is out of scope or invalid;
- `exhausted`: the worker could not complete the member within its budget;
- `error`: execution failed in a way that should be distinguished from research failure.

## Branch planning

Within a member, a branch is the smallest independently fulfillable segment of the task tree. The
branch planner receives only the member dossier and its missing slots, not the entire global run.

It should prioritize:

1. branches necessary for hard completion;
2. high-confidence authoritative sources;
3. reuse of a page that legitimately supports multiple requirements;
4. cheap deterministic checks before further model or network work;
5. repairable evidence failures before entirely new searches.

A branch search result may create several proposed records, but the admission gate evaluates each
record separately.

## Evidence record contract

The internal canonical record should be stricter than the benchmark's minimal JSON shape:

```json
{
  "task_node": "example.sidecar",
  "item": {"company": "Example Corporation"},
  "url": "https://example.test/page",
  "excerpts": ["Faithful text from the visible page."],
  "answer": {"status": "..."},
  "source": {
    "fetched_at": "2026-07-16T00:00:00Z",
    "final_url": "https://example.test/page",
    "content_hash": "...",
    "fetch_method": "http",
    "visible_text_artifact": "cas:..."
  },
  "admission": {
    "deterministic": {},
    "semantic": {},
    "accepted": true
  }
}
```

Provenance fields remain in the dossier and diagnostics. The exporter emits only the task-requested
fields.

## Evidence admission graph

`graph:wandr/evidence_record` is the most important quality boundary. It receives a proposed record,
the applicable task-node plan, and the fetched visible text.

### Deterministic gates

`tool:wandr/excerpts` and related tools must check:

1. required item keys exist and contain non-empty strings;
2. the URL is syntactically valid HTTP(S);
3. the page fetched successfully;
4. redirects resolve to a permitted public URL;
5. the extracted document contains usable visible main text;
6. every excerpt occurs in that text under conservative whitespace and punctuation normalization;
7. excerpts are non-empty and are not obvious navigation or metadata fragments;
8. the record conforms to the output schema;
9. the URL and key path do not duplicate an accepted record;
10. shared identifiers exactly match the member's canonical values.

If these checks fail, a directive must not be asked to excuse them.

### Semantic audit

`directive:wandr/audit_record` receives:

- the precise task-node requirements;
- the canonical item identity;
- the proposed answer;
- the proposed excerpts;
- bounded surrounding visible text;
- source and authority hints derived from the fetched page.

It returns flags, not rewritten evidence:

```json
{
  "entity_supported": true,
  "authority_supported": true,
  "scope_supported": true,
  "requirements_all_satisfied": false,
  "requirements_all_supported": false,
  "excerpt_semantics_faithful": true,
  "unsupported_requirements": ["announcement date"],
  "contradictions": [],
  "repairable": true
}
```

The audit directive must not invent excerpts, alter the entity, or promote information found outside
the submitted page. A repairable failure returns to the branch planner with the missing requirement.
An unrepairable failure rejects the proposal.

### Admission decision

`tool:wandr/dossier` admits a record only when all universal, deterministic, and task-specific gates
pass. The admission result and failed predicates are persisted for diagnostics and future gap
planning.

## Coverage model

Coverage is computed from the task plan and accepted dossiers, never inferred by a directive.

For every key node, the coverage tool should track:

- required child count;
- admitted child count;
- missing slots;
- surplus children;
- duplicate collapses;
- partial completeness;
- hard completeness;
- rejected and exhausted candidates;
- attempts and marginal yield.

Example summary:

```json
{
  "requested_members": 70,
  "discovered_unique": 126,
  "attempted_members": 91,
  "hard_complete_members": 68,
  "partial_members": 17,
  "rejected_members": 6,
  "remaining_member_deficit": 2,
  "missing_by_branch": {
    "appointment": 4,
    "listing_status": 11
  },
  "last_discovery_round": {
    "queries": 12,
    "new_unique_candidates": 7,
    "estimated_cost_per_candidate": 0.18
  }
}
```

The graph branches on these fields. The model may diagnose a deficit, but it cannot declare coverage
complete.

## Selection and stopping policy

The controller should favor hard-complete members while retaining useful soft-credit records.

Suggested policy:

1. enrich the candidates with the highest expected full-subtree completion probability;
2. maintain a surplus candidate queue to backfill failures;
3. repair nearly complete members before opening low-confidence candidates;
4. continue discovery when the queue cannot cover the remaining deficit at the observed completion
   rate;
5. stop when the requested hard-complete volume plus configured surplus is reached;
6. otherwise stop when time, spend, token, query, or attempt budgets are exhausted;
7. on budget exhaustion, export complete members first and then the best admitted partial members.

“More is better” does not justify exporting weak records. Additional records help only when their
expected correctness outweighs their precision risk.

The stopping tool should expose its reason explicitly:

```json
{
  "decision": "continue_enrichment",
  "reason": "two hard-complete members missing; candidate queue has six viable members",
  "next_batch_size": 6
}
```

## Persistence and resume

### Run ledger

The operational ledger stores:

- task-plan content hash;
- query lanes and results;
- canonical candidates and aliases;
- candidate rank and status;
- fetched-page artifact references;
- member attempts and missing branches;
- admitted and rejected records;
- coverage snapshots;
- budget usage;
- exporter state.

Large values should live in content-addressed artifacts. The graph checkpoint holds stable references
and the minimal state needed to resume.

### Signed member dossiers

An accepted or partially accepted member is persisted under an isolated ref such as:

```text
knowledge:wandr/runs/<run-id>/members/<canonical-id>
```

The dossier contains the complete current document for the member. Upsert replaces the prior
document atomically; it must not append ambiguous fragments.

The manifest grants item-authoring authority only to the run/member namespace required by the
solver. Member workers cannot author task-family or source-strategy knowledge.

### Consolidation

`graph:wandr/consolidate` runs separately from benchmark solving. It may study completed run
dossiers to update reusable strategies such as:

- productive discovery partitions for a task family;
- reliable authority surfaces;
- source-specific fetch and extraction tactics;
- common identity aliases;
- recurring evidence failure patterns.

Consolidation claims should require support across multiple tasks or entities. Benchmark workers do
not write abstractions directly.

## Exporter

`tool:wandr/export` is deterministic and task-contract driven.

It must:

1. read the ordered task-node list;
2. select admitted records for each node;
3. keep shared identifiers byte-for-byte identical across files;
4. order records deterministically;
5. serialize one JSON object per line;
6. emit only `item`, `url`, `excerpts`, and `answer`, unless the task contract requires otherwise;
7. validate every output line by parsing it back;
8. write temporary files inside the workspace;
9. fsync and atomically rename each file into place;
10. confirm every declared output exists and is non-empty.

The exporter should produce a separate diagnostic artifact containing record provenance and rejected
records, but that artifact must not be substituted for the required benchmark files.

## Benchmark integration

The benchmark adapter should be deliberately thin. Its responsibilities are:

- create or connect to the trial's RyeOS project context;
- make the task workspace available to the solver;
- install or resolve the signed solver bundle;
- execute `graph:wandr/solve` with the task contract and limits;
- wait for terminal completion;
- confirm the graph produced every required file;
- expose the RyeOS thread and run identifiers in trial diagnostics.

The adapter must not contain research planning, search loops, evidence extraction, or fallback logic.
Those belong to signed RyeOS items and must be visible in the execution history.

The official benchmark evaluator remains unchanged and runs only after the graph has written the
submission files.

## Observability

Every run should make the following visible without reading model transcripts:

- current task-plan digest;
- discovery rounds and per-lane yields;
- unique candidate count and duplicate collapses;
- member status counts;
- hierarchy coverage tree;
- missing branches by frequency;
- deterministic admission failures;
- semantic audit failures;
- page-fetch and browser-fallback rates;
- token, spend, latency, and query totals;
- final stop reason;
- output paths and record counts.

Recommended graph facets include:

```text
benchmark=wandr
run=<run-id>
task=<task-name>
phase=discovery|member|evidence|export
entity=<canonical-id>
lane=<lane-id>
```

Member and search-lane fan-outs should be cohort follows so the parent has real join semantics and a
single terminal scorecard.

## Failure handling

The system must distinguish research failure from execution failure.

### Research failures

- candidate is out of scope;
- candidate identity is ambiguous;
- required authoritative source cannot be found;
- page does not support every claim;
- excerpt is incomplete;
- member remains partial after its budget.

These update the ledger and coverage model and ordinarily do not fail the root graph.

### Execution failures

- invalid task contract;
- unavailable required runtime or secret;
- corrupt checkpoint or dossier;
- unsafe workspace path;
- exporter cannot create required files;
- child result violates its declared output schema;
- authority does not permit required persistence.

These should fail the relevant execution clearly. A completed benchmark score of zero must remain
distinguishable from infrastructure failure.

### Retry boundaries

Retries belong at the narrowest safe level:

- transient network retry inside fetch/search tools;
- alternative page retrieval in the evidence graph;
- requirement-specific repair in a member branch;
- candidate replacement in the root graph;
- full task restart only when the run ledger cannot be resumed safely.

Do not repeat an entire member because one fetch timed out, and do not restart the task because one
candidate was invalid.

## Model routing

The architecture should allow separate tiers by decision shape:

- stronger reasoning for task compilation and difficult evidence sufficiency judgments;
- fast structured models for query diversification, candidate extraction, and gap classification;
- deterministic tools for all exact checks and repeated operations.

Routing is configured rather than embedded in graph logic. Directive output schemas remain stable
when a model route changes.

Model calls should receive bounded evidence windows and ledger summaries, never the entire run or
all fetched pages. The shrinking unresolved residue receives the most expensive reasoning.

## Security and authority

The solver processes untrusted public web content. Tool outputs and page text are evidence, not
instructions.

Required safeguards include:

- public HTTP(S) URL validation and private-network blocking;
- strict workspace containment for reads and writes;
- bounded response sizes and timeouts;
- no execution of page-provided scripts or shell fragments;
- browser sessions isolated per worker or explicitly pooled by policy;
- secrets injected only into tools that declare them;
- least-authority item authoring for member dossiers;
- no authority escalation through followed child graphs;
- signed solver items and inspectable execution history.

Directives must be told explicitly that fetched text can contain prompt injection and cannot change
the task, output contract, tool authority, or admission rules.

## Development sequence

Implementation should proceed by proving one cell at a time.

### Phase 1: contract and exporter

- Parse one generated WANDR task workspace.
- Produce the ordered task-node and output-path mapping.
- Export hand-constructed valid records to exact JSONL filenames.
- Verify path containment and atomic output behavior.

### Phase 2: one evidence record

- Search or accept one known URL.
- Fetch and extract visible main text.
- Propose excerpts and an answer.
- Enforce exact excerpt grounding.
- Run the semantic audit.
- Persist an accepted record with provenance.

This phase is the first quality gate. Broad search should not be built before it is reliable.

### Phase 3: one complete member

- Compile one task hierarchy.
- Enrich one known candidate across every branch.
- Keep shared identifiers identical.
- Persist a complete dossier.
- Export the member to all applicable task files.

### Phase 4: smoke task

- Run the benchmark smoke task end to end.
- Prove discovery, multiple evidence URLs per member, JSONL export, and evaluator compatibility.

### Phase 5: bounded breadth

- Discover and enrich 20–50 members.
- Add candidate surplus, deduplication, backfill, and bounded parallelism.
- Measure completion rate and marginal discovery yield.

### Phase 6: composite hierarchies

- Run tasks with shared identifiers and sibling subtasks.
- Prioritize hard-complete member subtrees.
- Diagnose branch-specific deficits.

### Phase 7: persistence and resume

- Interrupt runs during discovery and enrichment.
- Resume without duplicate searches or output rows.
- Verify signed dossiers and task-plan hash compatibility.

### Phase 8: validation set

- Run the official representative validation tasks.
- Inspect discovery, enrichment, identity, page, excerpt, and hard-completion diagnostics.
- Tune one component at a time.

### Phase 9: full benchmark

- Pin solver version, model routes, search configuration, and budgets.
- Run the full task set only after smoke and validation remain stable.
- Preserve all run identifiers, bundle identities, task-plan hashes, and final diagnostics.

## Initial implementation milestone

The first meaningful milestone is not a full benchmark run. It is:

> `graph:wandr/member` accepts one known candidate, completes every required branch beneath that
> candidate, admits only page-grounded evidence, persists an auditable dossier, and returns records
> that the deterministic exporter can write without interpretation.

Once that cell is trustworthy, discovery fan-out and member parallelism scale useful work. Before it
is trustworthy, added breadth only produces incomplete or unsupported records faster.

## Success criteria

The implementation is structurally complete when:

- task trees are compiled into typed hierarchy plans;
- global discovery and per-member enrichment are separate graph stages;
- member workers own complete subtrees;
- evidence admission combines deterministic grounding with semantic requirement checks;
- coverage and stopping are computed mechanically;
- final files are written once by a deterministic exporter;
- runs survive interruption without repeating completed work;
- accepted dossiers are signed and auditable;
- benchmark integration contains no hidden orchestration;
- diagnostics localize losses to discovery, enrichment, identity, fetch, page qualification,
  excerpt support, or missing hierarchy branches.

## External references

- WANDR benchmark article:
  `https://research.perplexity.ai/articles/wandr-benchmark-evaluating-research-agents-that-must-search-wide-and-deep`
- WANDR benchmark and evaluation harness:
  `https://github.com/perplexityai/wandr`
