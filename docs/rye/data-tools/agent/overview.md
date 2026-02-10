    this doc is incorrect around how each of the primary tools handle each of the primary items.
    specifically around the diffeernt between what is returned from executing directives and knowledge vs load iwith no destination.

# Agent Architecture

> How Rye agents execute directives using the 4 primary tools and
> the thread execution system.

## Core Concept: In-Thread Directive Execution

When an agent calls `rye_execute` with `item_type="directive"`, the directive's
parsed content — its description, steps, inputs, permissions, and body — is
returned **into the current thread's context**. The agent now has the
instructions and follows them by calling tools.

This is the fundamental pattern. Directives are instructions that get loaded
into a running conversation, not programs that spawn new processes.

```
Agent thread is running
    │
    ├─ Agent calls: rye_execute(item_type="directive", item_id="deploy/staging")
    │
    ├─ Result: directive metadata + steps returned to agent
    │
    ├─ Agent reads the steps and follows them:
    │   ├─ Step 1: rye_search(item_type="tool", query="deploy")
    │   ├─ Step 2: rye_execute(item_type="tool", item_id="rye/file-system/fs_write", ...)
    │   ├─ Step 3: rye_execute(item_type="tool", item_id="rye/file-system/fs_read", ...)
    │   └─ Step 4: rye_sign(item_type="knowledge", item_id="deploy-log")
    │
    └─ Agent reports completion
```

The agent stays on the same thread the entire time. No new threads are spawned.
The directive provides _what to do_; the agent decides _how to do it_ using
the 4 primary tools.

## The 4 Primary Tools

Every agent thread has access to exactly 4 tools. These map to the 4 MCP
tools exposed by the Rye server.

| Tool          | Purpose                                         | Key Parameters                       |
| ------------- | ----------------------------------------------- | ------------------------------------ |
| `rye_execute` | Run a tool, directive or knowledge into context | `item_type`, `item_id`, `parameters` |
| `rye_search`  | Find items across the 3 spaces                  | `item_type`, `query`                 |
| `rye_load`    | Read an item's raw content                      | `item_type`, `item_id`               |
| `rye_sign`    | Validate and sign an item                       | `item_type`, `item_id`               |

### `rye_execute` — The Dual-Purpose Tool

`rye_execute` behaves differently based on `item_type`:

**`item_type="tool"`** — Executes the tool and returns its result:

```json
{
  "item_type": "tool",
  "item_id": "rye/file-system/fs_write",
  "parameters": {"path": "output.md", "content": "hello"}
}
→ returns: {"success": true, "path": "/project/output.md", "bytes_written": 5}
```

**`item_type="directive"`** — Loads the directive and returns its parsed
content into the thread. The agent then follows the steps:

```json
{
  "item_type": "directive",
  "item_id": "test/write_and_read",
  "parameters": {"directive_name": "test/write_and_read", "inputs": {"path": "/tmp/x"}}
}
→ returns: {name, description, steps, inputs, permissions, body, ...}
```

**`item_type="knowledge"`** — Loads the knowledge entry content:

```json
{
  "item_type": "knowledge",
  "item_id": "rye-architecture"
}
→ returns: {content, metadata, ...}
```

### `rye_search` — Discovery

Search across tools, directives, or knowledge in all 3 spaces
(project > user > system):

```json
{"item_type": "knowledge", "query": "authentication"}
→ returns: [{id, title, score, preview, ...}, ...]
```

### `rye_load` — Inspect

Load the raw content of any item:

```json
{"item_type": "directive", "item_id": "test/01_write_file"}
→ returns: {content: "<!-- rye:validated:... -->\n# Write File\n...", metadata: {...}}
```

### `rye_sign` — Validate

Validate and cryptographically sign an item:

```json
{"item_type": "knowledge", "item_id": "capability-tokens-learnings"}
→ returns: {status: "signed", signature: {timestamp, hash}}
```

## Thread Execution

A thread is the runtime context where an agent executes a directive
autonomously. The `thread_directive` tool spawns a thread.

### What a Thread Provides

- **LLM loop**: System prompt → LLM → tool calls → results → LLM → ... → done
- **Safety harness**: Cost tracking, turn/token limits, permission enforcement
- **Capability tokens**: Ed25519-signed permission tokens scoped to the directive
- **Transcript**: JSONL event stream + auto-generated markdown
- **Thread metadata**: `thread.json` with status, cost, model info

### How a Thread Is Spawned

The `thread_directive` tool at `rye/agent/threads/thread_directive` is the
entry point. It:

1. Loads and parses the directive from `.ai/directives/`
2. Validates metadata: `<model>`, `<limits>`, `<permissions>` must be declared
3. Mints a `CapabilityToken` from the permissions
4. Maps permissions to the primary tools the agent can call
5. Loads the provider config (e.g., `anthropic_messages.yaml`)
6. Builds a system prompt from the provider's `prompts.system_template`
7. Runs the LLM tool-use loop until completion or limit breach
8. Writes transcript events, thread metadata, and cost data

### System Prompt: Data-Driven

The system prompt is **not hardcoded**. It's rendered from the provider YAML's
`prompts.system_template` section, with placeholders resolved at build time:

| Placeholder               | Resolved To                        |
| ------------------------- | ---------------------------------- |
| `{directive_name}`        | Directive name                     |
| `{directive_description}` | Directive description              |
| `{directive_steps}`       | Formatted steps with action blocks |
| `{directive_inputs}`      | JSON-serialized inputs             |
| `{tool_names}`            | Comma-separated tool names         |

If a directive step contains structured action tags (`<execute>`, `<search>`,
`<load>`, `<sign>`), they're rendered as canonical tool-call JSON blocks in the
steps section. This shows the LLM exactly which tool to call with what
parameters — using `{input:name}` refs resolved to actual values.

### Tool-Use Loop

```
┌─────────────────────────────────────────────┐
│  System prompt (from provider template)     │
│  + User prompt (directive body)             │
│                    │                        │
│                    ▼                        │
│  ┌──────────────────────┐                   │
│  │     LLM Response     │◄──────────────┐   │
│  └──────────┬───────────┘               │   │
│             │                           │   │
│     has tool_use blocks?                │   │
│      ╱            ╲                     │   │
│    yes             no                   │   │
│     │               │                   │   │
│     ▼               ▼                   │   │
│  Execute         Thread                 │   │
│  tool calls      complete               │   │
│     │                                   │   │
│     ▼                                   │   │
│  Append results ────────────────────────┘   │
│  to messages                                │
│                                             │
│  Check limits (turns, tokens, spend)        │
│  Fire hooks if triggered                    │
└─────────────────────────────────────────────┘
```

The loop runs up to `MAX_TOOL_ROUNDTRIPS` (10) turns. Each turn:

1. Sends messages to the LLM via the provider
2. Parses the response using provider config field mappings
3. Updates cost tracking
4. If tool calls: executes each, appends results, continues
5. If no tool calls: thread complete

## In-Thread vs New-Thread: When to Use Each

### In-Thread (rye_execute directive) — Default

The agent calls `rye_execute(item_type="directive")` and gets the directive's
instructions returned into its current context. The agent then executes the
steps itself.

**Use when:**

- The current agent needs the directive's instructions to guide its work
- The directive is a sub-task of a larger workflow
- You want sequential execution within one conversation
- The directive's steps should run with the current thread's tools and context

**Example:** An agent executing a `deploy_pipeline` directive loads
`validate_config` as a sub-directive. The validation steps run in the
same thread, and the agent sees the results immediately.

### New Thread (thread_directive tool) — Explicit

The agent calls `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive")`
to spawn a completely separate thread with its own LLM loop, safety harness,
and transcript.

**Use when:**

- The directive should run autonomously with its own LLM
- You need cost isolation (separate budget tracking)
- The directive requires different permissions or a different model
- You want parallel execution (fire-and-forget)
- The parent doesn't need to follow the child's steps — just its result

**Example:** An orchestrator directive spawns a `research_topic` thread and a
`write_report` thread. Each runs independently with its own model and limits.

### The Future Pattern: spawn_thread Directive

To make child-thread spawning easy for agents, a directive like
`spawn_thread` would:

1. Be loaded via `rye_execute(item_type="directive", item_id="spawn_thread")`
2. Return instructions telling the agent _how_ to call the `thread_directive`
   tool with the right parameters
3. The agent follows those instructions and calls the tool

This keeps the pattern consistent: directives are always instructions loaded
into context, never magical spawners. The agent always decides what to do.

## Directive Metadata for Threads

Every directive that runs in a thread must declare:

```xml
<metadata>
  <model tier="haiku" id="claude-3-5-haiku-20241022" />
  <limits max_turns="5" max_tokens="2048" />
  <permissions>
    <execute>
      <tool>rye.file-system.*</tool>
    </execute>
    <search>
      <knowledge>*</knowledge>
    </search>
  </permissions>
</metadata>
```

**`<model>`** — Which LLM to use. Attributes: `tier` (haiku/sonnet/opus),
`id` (full model identifier).

**`<limits>`** — Resource bounds. Attributes: `max_turns`, `max_tokens`,
`max_spend`, `spend_currency`.

**`<permissions>`** — Hierarchical capability declarations. Primary verbs
(`execute`, `search`, `load`, `sign`) contain item types (`tool`, `directive`,
`knowledge`) with specifics:

```xml
<permissions>
  <execute>
    <tool>rye.file-system.*</tool>
    <directive>test/*</directive>
  </execute>
  <search>*</search>
  <load>
    <knowledge>*</knowledge>
  </load>
  <sign>
    <knowledge>*</knowledge>
  </sign>
</permissions>
```

### Structured Action Tags in Steps

Directive steps can include action tags that get rendered as canonical
tool-call blocks in the system prompt:

```xml
<process>
  <step name="find_entries">
    <description>Search for relevant knowledge</description>
    <search item_type="knowledge" query="{input:topic}" />
  </step>
  <step name="write_report">
    <description>Write findings to disk</description>
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value="{input:output_path}" />
      <param name="content" value="Report content" />
    </execute>
  </step>
</process>
```

The `{input:name}` placeholders are resolved to actual input values at
prompt-build time, giving the LLM concrete tool calls to make.

## Transcript & Telemetry

Every thread produces:

| File               | Format       | Purpose                               |
| ------------------ | ------------ | ------------------------------------- |
| `transcript.jsonl` | JSONL events | Machine-readable execution log        |
| `transcript.md`    | Markdown     | Human-readable, `tail -f` friendly    |
| `thread.json`      | JSON         | Thread metadata (status, cost, model) |

Event types: `thread_start`, `user_message`, `step_start`, `assistant_text`,
`assistant_reasoning`, `tool_call_start`, `tool_call_result`, `step_finish`,
`thread_complete`, `thread_error`, `hook_triggered`, `spawn_child`.

All events include a `directive` field identifying which instruction
produced them.

## File Layout

```
.ai/
├── directives/          # Task specifications (markdown + XML)
│   └── test/
│       ├── 01_write_file.md
│       └── 09_self_evolving_researcher.md
├── tools/               # Executable tools
│   └── rye/
│       ├── agent/
│       │   ├── providers/
│       │   │   └── anthropic_messages.yaml    # LLM provider config
│       │   └── threads/
│       │       ├── thread_directive.py        # Thread spawner tool
│       │       ├── thread_registry.py         # Registry + transcript writer
│       │       └── safety_harness.py          # Cost/limit/hook enforcement
│       ├── file-system/
│       │   ├── fs_read.py
│       │   └── fs_write.py
│       └── primary-tools/
│           ├── rye_execute.py                 # Execute items
│           ├── rye_search.py                  # Search items
│           ├── rye_load.py                    # Load item content
│           └── rye_sign.py                    # Validate & sign
├── knowledge/           # Domain knowledge entries
│   └── rye-architecture.md
└── threads/             # Thread execution data (runtime)
    ├── registry.db
    └── {thread_id}/
        ├── thread.json
        ├── transcript.jsonl
        └── transcript.md
```

## See Also

- [agent-transcript-telemetry.md](../../design/agent-transcript-telemetry.md) — Transcript event schema and telemetry
- [agent-threads-future.md](../../design/agent-threads-future.md) — Multi-turn conversations, async/await, channels
- [capabilities.md](capabilities.md) — Permission enforcement
