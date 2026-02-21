```yaml
id: ryeos-cli
title: "ryeos-cli — Terminal-Native Interface"
description: A CLI wrapper around ryeos that maps shell commands to deterministic RYE invocations — shorthand verbs for the four primitives, a parameter parser for correct tool call construction, and a thread verb that makes directive execution a first-class shell operation
category: future
tags: [cli, terminal, interface, shell, parser]
version: "0.1.0"
status: exploratory
```

# ryeos-cli — Terminal-Native Interface

> **Status:** Exploratory — architecturally straightforward, not scheduled for implementation.

## The Idea

RYE's agent-facing surface is four MCP tools: `search`, `load`, `execute`, `sign`. Today, invoking these requires an MCP client. `ryeos-cli` is a CLI that maps shell verbs directly to these primitives — no MCP transport, no JSON-RPC, no agent session.

The core mapping:

```
rye {verb} {item_type} {item_id} [params...]
```

But the interesting problem is that RYE's execution model has a distinction the CLI needs to collapse. `execute directive` loads and returns a directive's content — it doesn't run it. Actually running a directive means `execute tool rye/agent/threads/thread_directive` with the directive name as a parameter. The CLI needs a shortcut that makes this natural.

---

## Verb Mapping

The CLI exposes verbs that map to the four primitives, plus a `thread` verb that shortcuts directive execution:

| CLI                                          | What It Calls                                                                                                     | Notes                                               |
| -------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- | --------------------------------------------------- |
| `rye search <scope> <query>`                 | `rye_search(scope=..., query=...)`                                                                                | Registry search                                     |
| `rye load <item_type> <item_id>`             | `rye_load(item_type=..., item_id=...)`                                                                            | Read item content to stdout                         |
| `rye execute <item_type> <item_id> [params]` | `rye_execute(item_type=..., item_id=..., parameters=...)`                                                         | Execute a tool or return directive content          |
| `rye sign <item_type> <item_id>`             | `rye_sign(item_type=..., item_id=...)`                                                                            | Ed25519 sign an item                                |
| `rye thread <directive_id> [params]`         | `rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive", parameters={"directive": ..., ...})` | **Shortcut** — spawn a thread that runs a directive |

### The `thread` Verb

This is the key shortcut. In an MCP client, running a directive autonomously requires:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive": "rye/outreach/email_campaign",
        "inputs": {"target": "tech companies", "limit": 50},
        "model": "sonnet",
        "max_spend": 1.00
    }
)
```

From the CLI:

```bash
rye thread rye/outreach/email_campaign \
    --target "tech companies" \
    --limit 50 \
    --model sonnet \
    --max-spend 1.00
```

The CLI parser needs to distinguish between thread-level parameters (`--model`, `--max-spend`, `--async`) and directive input parameters (`--target`, `--limit`). More on this below.

---

## Parameter Parsing

This is the hard part. RYE tool calls take a `parameters` dict with arbitrary keys. The CLI needs to parse shell arguments into the correct structure deterministically — no LLM in the loop.

### The Problem

Different tools and directives expect different parameter shapes. A bash tool expects `{"command": "..."}`. A directive expects `{"inputs": {"key": "value"}}`. The thread verb needs to separate its own flags from directive inputs. The CLI parser needs to construct the right call without ambiguity.

### Approach: Schema-Driven Parsing

Every RYE tool and directive already has a parseable schema — tools declare their parameters in YAML frontmatter, directives declare `<inputs>` in their XML metadata block. The CLI parser can load the schema for the target item and use it to construct the correct parameter dict.

```
rye thread rye/outreach/email_campaign --target "tech" --limit 50 --model sonnet
    │
    ▼
1. Parse verb: "thread" → execute tool thread_directive
2. Resolve item: rye/outreach/email_campaign → load directive metadata
3. Extract input schema: {target: string, limit: integer}
4. Separate params:
   - Thread params (known set): --model sonnet → model: "sonnet"
   - Directive inputs (from schema): --target "tech", --limit 50 → inputs: {target: "tech", limit: 50}
5. Construct call:
   rye_execute(
       item_type="tool",
       item_id="rye/agent/threads/thread_directive",
       parameters={
           "directive": "rye/outreach/email_campaign",
           "inputs": {"target": "tech", "limit": 50},
           "model": "sonnet"
       }
   )
```

Thread-level params are a known fixed set (`--model`, `--max-spend`, `--max-turns`, `--async`, `--budget`). Everything else gets routed to `inputs` based on the directive's declared schema. Unknown flags are rejected with a clear error.

### Direct Tool Execution

For `rye execute tool`, the same schema-driven approach applies:

```bash
rye execute tool rye/bash/bash --command "find . -name '*.py' | wc -l"
```

```
1. Parse: execute tool rye/bash/bash
2. Load tool metadata → params schema: {command: string}
3. Map --command → parameters.command
4. Construct: rye_execute(item_type="tool", item_id="rye/bash/bash", parameters={"command": "find ..."})
```

### Positional Shorthand

For tools with a single primary parameter, allow positional:

```bash
# These are equivalent:
rye execute tool rye/bash/bash --command "ls -la"
rye execute tool rye/bash/bash "ls -la"
```

The schema marks which parameter is `primary` (if any), and a bare positional argument maps to it.

---

## Architecture

```
rye thread my/directive --target foo --model sonnet
    │
    ▼
ryeos-cli
  ├── Verb parser (thread/execute/search/load/sign)
  ├── Schema loader (resolve item → extract param schema)
  ├── Param router (separate tool params from item params)
  └── Call constructor (build deterministic rye_execute call)
    │
    ▼
ryeos (executor, resolver, signing — the full stack)
    │
    ▼
lilux (subprocess, HTTP, signing, integrity primitives)
```

`ryeos-cli` imports `ryeos` directly as a Python library — no MCP transport. It's a synchronous call into the same executor chain that MCP clients use, minus the protocol overhead.

### Package Structure

```bash
pip install ryeos-cli
```

Dependencies: `ryeos` (which brings `lilux`). No MCP dependency.

---

## Full Verb Reference

```bash
# Search
rye search directive "lead generation"
rye search tool "bash"
rye search knowledge "api docs"
rye search tool.rye.core.* "*"         # scope + wildcard

# Load
rye load directive rye/outreach/email_campaign
rye load tool rye/bash/bash
rye load knowledge my-project/api-docs
rye load tool rye/bash/bash --destination project   # copy to project space

# Execute (stateless — returns content for directives, runs tools)
rye execute tool rye/bash/bash --command "pytest tests/"
rye execute directive my-project/onboarding         # returns parsed directive content
rye execute knowledge my-project/api-docs            # returns parsed knowledge content

# Thread (autonomous directive execution — the shortcut)
rye thread rye/outreach/email_campaign --target "tech companies" --limit 50
rye thread rye/outreach/email_campaign --target "tech" --async   # returns thread_id immediately
rye thread my-project/analyze --model haiku --max-spend 0.50

# Sign
rye sign tool my-project/scraper
rye sign directive my-project/onboarding
rye sign tool "my-project/*"                         # glob batch signing
```

---

## Output and Streaming

Thread execution can be long-running (the LLM loop runs until the directive completes or budget is exhausted). The CLI streams transcript events to stderr and writes the final result to stdout:

```bash
$ rye thread rye/outreach/email_campaign --target "tech" --limit 5
[thread_abc123] Starting: rye/outreach/email_campaign (sonnet, $1.00 budget)
[thread_abc123] Turn 1: Searching for tech companies...
[thread_abc123] Turn 2: Found 12 candidates, filtering to top 5...
[thread_abc123] Turn 3: Generating outreach emails...
[thread_abc123] Completed: 5 emails generated ($0.34 spent)
{"status": "completed", "result": {...}}
```

For `--async` mode, the CLI returns the thread_id and the user can check status:

```bash
$ rye thread my/directive --target "tech" --async
thread_abc123

$ rye execute tool rye/agent/threads/internal/get_status --thread_id thread_abc123
```

---

## Open Design Questions

### Parser Complexity

The schema-driven parser needs to handle: string, integer, float, boolean, and list types. Nested objects (like `inputs` in thread_directive) add complexity. Is a full argparse-style parser worth it, or should the CLI accept `--params '{json}'` as an escape hatch for complex cases?

```bash
# Escape hatch for complex params
rye execute tool my/complex-tool --params '{"nested": {"key": "value"}, "list": [1, 2, 3]}'
```

### Discoverability

`rye thread --help` should list available directives. `rye thread rye/outreach/email_campaign --help` should show the directive's input schema as CLI flags. This comes free from the schema-driven approach.

### Interactive Confirmation

Should `rye thread` confirm before spawning (costs money)? Interactive terminals get a confirmation prompt showing the model, budget, and directive. Piped/scripted usage (`--yes` flag or `isatty()` detection) skips it.

### Shell Completion

Schema-driven parsing enables shell completion — the CLI can generate completions for item IDs (from the registry) and parameter names (from the item schema). Bash/zsh/fish completion scripts generated from the registry state.

---

## CLI Syntax as Intermediate Representation

The CLI parser has a potential consequence beyond terminal usage. The [Intent Resolution](memory-and-intent-resolution.md) proposal needs a small model (Gemma 2B/7B) to translate natural-language intents into correct RYE invocations. The CLI parser could serve as a structuring layer between the model and the executor — the model generates a flat CLI string, and the parser handles the deterministic construction of the actual `rye_execute()` call.

### Two Output Formats

There are two plausible targets for the small model's output:

**Option A — CLI string (flat, shell-like):**

```
rye execute tool rye/bash/bash --command "find . -name '*.py' | wc -l"
```

The argument for this: it's a flat string with no nesting, no JSON escaping, closer to natural language. Shell commands are abundant in base model training data. The CLI parser handles the structuring deterministically.

**Option B — Structured function call (JSON):**

```json
{"name": "rye_execute", "parameters": {"item_type": "tool", "item_id": "rye/bash/bash", "parameters": {"command": "find . -name '*.py' | wc -l"}}}
```

The argument for this: function-calling models like Gemma are specifically fine-tuned to produce structured tool calls. This is literally what they're optimized for. Asking them to generate CLI strings instead may be going against their training rather than with it. The model already knows how to produce nested JSON with correct key names — that's the whole point of function-calling fine-tunes.

### What's Actually Unclear

The "CLI is easier for small models" intuition is plausible for base models but may not hold for function-calling fine-tunes. The real question is whether the model knows RYE's *specific* schema — but the intent resolver already passes candidate metadata in context, which is exactly how you'd prompt a function-calling model.

This needs empirical testing. The answer might also depend on whether the model is a generic function-calling fine-tune or one further fine-tuned on RYE's schema specifically.

### What's Clear Regardless

Independent of the model output format question, the CLI parser has value as a **shared parsing layer**:

- **Terminal entry point** — the primary purpose of `ryeos-cli`
- **Human-readable invocations** — useful for logging, debugging, documentation
- **MCP simplification** — the four MCP tools could optionally accept a CLI-style string alongside the current JSON parameters, routing through the same parser
- **Intent resolution (if Option A wins)** — the parser becomes the structuring layer between model output and executor

If the function-calling format (Option B) turns out to be better for the small model, the CLI parser still justifies itself as a terminal interface and human-readable format. It just wouldn't be in the intent resolution critical path.

---

## Relationship to Existing Infrastructure

| Existing Component                                   | How ryeos-cli Uses It                        |
| ---------------------------------------------------- | -------------------------------------------- |
| Four MCP tools (`search`, `load`, `execute`, `sign`) | Direct invocation without MCP transport      |
| `thread_directive` tool                              | The `thread` verb is a shortcut to this tool |
| `rye_search` (BM25 + fuzzy)                          | Powers `rye search` and completion           |
| Executor chain (tool → runtime → primitive)          | Same chain, invoked synchronously            |
| Three-tier spaces (project → user → system)          | Same resolution, same precedence             |
| Ed25519 signing                                      | Same keys, same verification                 |
| Item metadata schemas                                | Drive the parameter parser                   |
