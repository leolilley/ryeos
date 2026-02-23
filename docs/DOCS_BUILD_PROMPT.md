# Rye OS Documentation Build — Fresh Analysis Prompt

You are starting from scratch. There is an empty `docs/` folder. Old documentation exists in `docs-dep/` for reference only — do NOT copy its structure or opinions. Analyze the actual codebase and design documentation from first principles.

## What Rye OS Is

Rye OS is an MCP server that gives AI agents a portable `.ai/` directory system. Agents interact with it through 4 MCP tools. The `.ai/` directory contains three item types (directives, tools, knowledge) that the agent can search, load, execute, and sign.

The key idea: **everything is data**. Directives are markdown files with XML metadata. Tools are Python/YAML/JS/shell scripts with metadata headers. Knowledge is markdown with frontmatter. The MCP server resolves, validates, and executes these items — the agent never touches the filesystem directly.

## The Codebase — What Actually Exists

### Layer 1: Lilux (microkernel)
`lilux/lilux/` — the lowest layer. Provides primitives that everything else builds on.

```
lilux/lilux/
├── primitives/
│   ├── subprocess.py      # Run shell commands
│   ├── http_client.py     # Make HTTP requests
│   ├── signing.py         # Ed25519 sign/verify
│   ├── integrity.py       # Content hashing
│   ├── lockfile.py        # Integrity lockfiles
│   └── errors.py
├── runtime/
│   ├── env_resolver.py    # Resolve env vars, secrets
│   └── auth.py            # Authentication
└── schemas/
    └── schema_validator.py
```

### Layer 2: Rye (MCP server)
`rye/rye/` — the MCP server that agents talk to.

```
rye/rye/
├── tools/                    # The 4 MCP tools (agent-facing surface)
│   ├── execute.py            # rye_execute — run tools and directives
│   ├── load.py               # rye_load — read item content
│   ├── search.py             # rye_search — find items
│   └── sign.py               # rye_sign — validate and sign items
├── executor/                 # Execution engine
│   ├── primitive_executor.py # Resolves items → builds chain → runs
│   ├── chain_validator.py    # Validates execution chain integrity
│   └── lockfile_resolver.py  # Pins tool versions via lockfiles
├── handlers/                 # Item type handlers
│   ├── directive/            # Parse + load directives
│   ├── tool/                 # Parse + load tools
│   └── knowledge/            # Parse + load knowledge
├── utils/                    # Shared utilities
│   ├── metadata_manager.py   # Read/write item metadata
│   ├── resolvers.py          # Resolve item_id → file path (project > user > system)
│   ├── parser_router.py      # Route files to correct parser by extension
│   ├── validators.py         # Input validation
│   ├── integrity.py          # Integrity verification
│   ├── trust_store.py        # Ed25519 key management
│   ├── signature_formats.py  # Comment-prefix formats per file type
│   ├── path_utils.py         # Path utilities
│   ├── errors.py             # Error types
│   └── logger.py
├── protocols/
│   └── jsonrpc_handler.py    # MCP JSON-RPC protocol
├── bundle_entrypoints.py     # Package entrypoints
└── constants.py
```

### Layer 3: The `.ai/` Data Bundle
`rye/rye/.ai/` — ships inside the rye package. This is the "standard library" of items available to every agent.

**Directives** (`rye/rye/.ai/directives/`):
```
rye/core/create_directive.md
rye/core/create_knowledge.md
rye/core/create_threaded_directive.md
rye/core/create_tool.md
rye/agent/threads/thread_summary.md
```

**Knowledge** (`rye/rye/.ai/knowledge/`):
```
rye/core/directive-metadata-reference.md
rye/core/knowledge-metadata-reference.md
rye/core/tool-metadata-reference.md
```

**Tools** (`rye/rye/.ai/tools/`) — organized by category:
```
rye/agent/
  permissions/capability_tokens/    # Capability token system
  providers/anthropic.yaml, openai.yaml  # LLM provider configs
  threads/                          # Thread system (biggest subsystem)
    thread_directive.py             # Main thread execution loop
    orchestrator.py                 # Multi-thread orchestration
    runner.py                       # Thread runner
    safety_harness.py               # Cost/limit enforcement
    errors.py
    adapters/                       # LLM provider adapters
      provider_adapter.py, http_provider.py, provider_resolver.py, tool_dispatcher.py
    events/                         # Event system
      event_emitter.py, streaming_tool_parser.py
    internal/                       # Internal helpers
      budget_ops.py, cancel_checker.py, classifier.py, control.py,
      cost_tracker.py, emitter.py, limit_checker.py, state_persister.py,
      text_tool_parser.py, thread_chain_search.py, tool_result_guard.py
    loaders/                        # Config loaders (data-driven)
      config_loader.py, coordination_loader.py, error_loader.py,
      events_loader.py, hooks_loader.py, resilience_loader.py,
      condition_evaluator.py, interpolation.py
    persistence/                    # State persistence
      thread_registry.py, transcript.py, state_store.py,
      artifact_store.py, budgets.py
    security/
      security.py
    config/                         # YAML configs for thread behavior
      events.yaml, error_classification.yaml, hook_conditions.yaml,
      coordination.yaml, resilience.yaml, budget_ledger_schema.yaml

rye/bash/
  bash.py                           # Shell command execution

rye/core/
  bundler/bundler.py, collect.yaml  # Bundle creation/verification
  extractors/                       # Metadata extractors per item type
    directive/directive_extractor.yaml
    knowledge/knowledge_extractor.yaml
    tool/tool_extractor.yaml
  parsers/                          # File parsers
    markdown/frontmatter.py, markdown/xml.py, python/ast.py, yaml/yaml.py, javascript/javascript.py
  primitives/                       # Primitive configs
    subprocess.yaml, http_client.yaml
  runtimes/                         # Execution runtimes
    python/script.yaml, python/function.yaml,
    node/node.yaml, bash/bash.yaml,
    mcp/stdio.yaml, mcp/http.yaml
  sinks/                            # Output sinks
    file_sink.py, null_sink.py, websocket_sink.py
  system/system.py                  # System info tool
  telemetry/mcp_logs.py             # Logging

rye/file-system/
  read.py, write.py, edit_lines.py, glob.py, grep.py, ls.py

rye/lsp/
  lsp.py                            # Language Server Protocol integration

rye/mcp/
  connect.py, discover.py, manager.py  # MCP client (connect to other MCP servers)

rye/primary/                        # Primary tool wrappers (used inside threads)
  rye_execute.py, rye_load.py, rye_search.py, rye_sign.py

rye/core/registry/
  registry.py                       # Registry push/pull/search

rye/web/
  webfetch.py, websearch.py         # Web scraping and search
```

**Bundles** (`rye/rye/.ai/bundles/`):
```
ryeos-core/manifest.yaml           # Bundle manifest for the core bundle
```

**Lockfiles** (`rye/rye/.ai/lockfiles/`): integrity pinning files.

### Layer 4: Registry API
`services/registry-api/` — a separate service for sharing items.

```
services/registry-api/registry_api/
├── main.py, config.py, models.py, auth.py, validation.py
```

### Layer 5: Agency-Kiwi (real-world orchestration example)
`/home/leo/projects/agency-kiwi/.ai/directives/` — a separate project that demonstrates the orchestration pattern. Use this as a reference for how orchestration works in practice, NOT as something to document.

```
agency-kiwi/orchestrator/
  run_lead_pipeline.md    # Master orchestrator — spawns children per phase
  qualify_leads.md        # Sub-orchestrator — spawns scrape + score children
  prepare_outreach.md     # Sub-orchestrator — spawns enrich + email children
agency-kiwi/execution/
  discover_leads.md       # Leaf — calls scrape tool, saves output
  scrape_website.md       # Leaf — calls web scraper
  score_lead.md           # Leaf — calls scoring tool
  enrich_contact.md       # Leaf — finds contact email
  generate_email.md       # Leaf — generates outreach email
  update_pipeline_state.md # Leaf — writes state
agency-kiwi/strategy/
  select_targets.md       # Loads state + niche list, picks next batch
```

## How Orchestration Works

This is the most important system to document well.

### The Pattern

1. **Orchestrator directive** (sonnet-tier model, high budget) loads state, decides what to do, spawns child threads
2. **Child directives** (haiku-tier model, low budget) do the actual work — call tools, write files, return results
3. **Sub-orchestrators** sit in between — they orchestrate their own children
4. Each child thread runs in its own SafetyHarness with its own limits and transcript
5. Parents wait for children via `wait_threads` (asyncio.Event, zero-polling)
6. Capability tokens attenuate down the tree (children can only do what parents allow)
7. Budget is hierarchical — parent's budget is the ceiling for the entire tree

### How It's Invoked

All orchestration happens through ONE tool: `rye/agent/threads/thread_directive`. The agent calls:

```
rye_execute(
  item_type="tool",
  item_id="rye/agent/threads/thread_directive",
  parameters={
    "directive_name": "my-project/orchestrator/run_pipeline",
    "inputs": {"location": "Dunedin", "batch_size": 5},
    "limit_overrides": {"turns": 30, "spend": 3.00}
  }
)
```

The thread_directive tool:
1. Loads the directive, parses XML metadata
2. Creates a SafetyHarness with limits from metadata + overrides
3. Starts an LLM loop — the model reads the directive and follows its steps
4. Each step calls rye_execute/rye_load/rye_search via the primary tools
5. Steps that spawn children call thread_directive recursively
6. Transcript records everything; state persists for resumption

### Directive Format (what the agent reads)

Directives use this structure:
- Line 1: Signature comment (`<!-- rye:signed:... -->`)
- Markdown title + description
- Single ```xml fence with metadata (model, limits, permissions, inputs, outputs)
- Process steps in natural language + pseudo-code that the LLM follows

### Directive Roles

| Role             | Responsibility                                          | Model  | Turns | Spend |
| ---------------- | ------------------------------------------------------- | ------ | ----- | ----- |
| Orchestrator     | Load state, spawn children, coordinate waves, report    | sonnet | 30    | $3.00 |
| Sub-orchestrator | Coordinate a subset (e.g., qualify a batch of leads)    | sonnet | 20    | $1.00 |
| Strategy         | Load knowledge + state, decide what to do next          | haiku  | 6     | $0.05 |
| Execution        | Call one tool, save output, update state                 | haiku  | 4-10  | $0.10 |

## What the Documentation Needs to Cover

Think about THREE distinct audiences:

### Audience 1: Users building workflows
- How to install and configure Rye OS
- How to use the 4 MCP tools (execute, load, search, sign)
- How to author directives, tools, and knowledge
- How to build orchestrated pipelines (the orchestration pattern above)
- How to use threads (spawn, wait, resume, cancel)
- How to use the registry to share items
- Troubleshooting and error codes

### Audience 2: AI agents consuming items at runtime
- This is what `.ai/knowledge/` entries serve
- Metadata references, format specs, capability schemas
- The docs should be written so they CAN be copied into `.ai/knowledge/` with frontmatter

### Audience 3: Contributors working on the codebase
- Rye MCP server internals (executor, handlers, utils)
- Lilux primitives
- Registry API
- How to add new tool types, parsers, runtimes
- Testing

## Constraints

- DO NOT reuse the old docs structure. Analyze the codebase and design from scratch.
- The folder structure under `docs/` is entirely your decision.
- Every page should have YAML frontmatter (id, title, description, category, tags, version).
- Use real item IDs and real code examples from the codebase.
- Orchestration documentation is the highest priority — it's the thing that makes Rye OS unique.
- "Concepts" is too generic as a section name for what orchestration represents. Think harder about the information architecture.

## Files to Read for Context

These are the key source files. Read them before designing the structure:

**MCP tools (the agent-facing surface):**
- `rye/rye/tools/execute.py`
- `rye/rye/tools/load.py`
- `rye/rye/tools/search.py`
- `rye/rye/tools/sign.py`

**Thread system (orchestration engine):**
- `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py`
- `rye/rye/.ai/tools/rye/agent/threads/orchestrator.py`
- `rye/rye/.ai/tools/rye/agent/threads/safety_harness.py`
- `rye/rye/.ai/tools/rye/agent/threads/runner.py`

**Bundled directives (the "stdlib"):**
- `rye/rye/.ai/directives/rye/core/create_directive.md`
- `rye/rye/.ai/directives/rye/core/create_tool.md`
- `rye/rye/.ai/directives/rye/core/create_threaded_directive.md`
- `rye/rye/.ai/directives/rye/agent/threads/thread_summary.md`

**Executor (how items are resolved and run):**
- `rye/rye/executor/primitive_executor.py`
- `rye/rye/executor/chain_validator.py`
- `rye/rye/utils/resolvers.py`

**Real orchestration example (separate project, for understanding the pattern):**
- `/home/leo/projects/agency-kiwi/.ai/directives/agency-kiwi/orchestrator/run_lead_pipeline.md`
- `/home/leo/projects/agency-kiwi/.ai/directives/agency-kiwi/orchestrator/qualify_leads.md`
- `/home/leo/projects/agency-kiwi/.ai/directives/agency-kiwi/execution/discover_leads.md`
- `/home/leo/projects/agency-kiwi/.ai/directives/agency-kiwi/execution/score_lead.md`

## Deliverable

1. Propose the complete `docs/` folder structure with rationale
2. Write every page (not stubs — real content grounded in the codebase)
3. Start with orchestration — it's the flagship capability
4. Then MCP tool reference, then getting started, then internals
