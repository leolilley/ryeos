```yaml
id: pre-execution-context-assembly
title: "Pre-Execution Context Assembly: What We Already Have"
description: How RyeOS already solves the pre-execution context assembly problem that Skills' !`cmd` syntax addresses, via the layered context injection system, and where the authoring surface could be improved
category: ideas
tags: [ideas, directives, context, hooks, layered-context, authoring]
version: "1.0.0"
```

# Pre-Execution Context Assembly: What We Already Have

## The Problem Skills Tried to Solve

Anthropic's Skills system has a mechanism called `!`cmd`` syntax. In a skill file:

```markdown
## Pull request context
- PR diff: !`gh pr diff`
- Changed files: !`gh pr diff --name-only`
```

The shell commands execute before the agent sees the prompt. Their output replaces the placeholders. The agent starts with the context already assembled — zero turns consumed on gathering live state.

For directives that need to reason about environmental state (git diff, PR data, current branch, recent logs) before doing useful work, assembling that context before turn 1 is better than spending the opening turn on tool calls.

## RyeOS Already Solves This

The layered context injection system assembles context through three independent layers before the agent loop starts. All three run during thread setup, before turn 1.

### Layer 1 — Tool Schema Preload

After capability resolution, the tool schema loader scans `harness._capabilities`, resolves all granted tools across the 3-tier space, and injects their `CONFIG_SCHEMA` + `__tool_description__` into the before-context. The agent starts knowing the parameter shapes for every tool it's permitted to call — no `rye_fetch` discovery loops.

This is automatic. No authoring required. Declare permissions, get schemas.

### Layer 2 — Context Hooks (`thread_started`)

The `run_hooks_context()` path in `safety_harness.py` fires on `thread_started` and can execute any RyeOS item — tool, knowledge, or directive. The result content is extracted and injected at the `before` or `after` position relative to the directive body. All of this runs before the agent processes its first message.

The hook context passed to `run_hooks_context()` includes:

```python
{
    "directive": harness.directive_name,
    "directive_body": directive_body,
    "model": provider.model,
    "limits": harness.limits,
    "inputs": inputs or {},          # ← directive's typed inputs available here
    "project_path": str(project_path),
    "depth": depth,
    "parent_thread_id": ...,
    "capabilities_summary": ...,
}
```

`inputs` is present. A directive that receives `pr_number` as an input can declare a hook that passes `${inputs.pr_number}` to a bash tool call:

```yaml
# project hooks.yaml
hooks:
  - id: "pr_diff_context"
    event: "thread_started"
    position: "before"
    action:
      primary: "execute"
      item_type: "tool"
      item_id: "rye/bash"
      params:
        command: "gh pr diff ${inputs.pr_number}"
```

Directives can also declare their own hooks directly in the `<hooks>` block in their XML, which merge at layer 1 priority (above builtin hooks). The assembled output is XML-wrapped by default (or raw with `wrap: false`) and placed before the directive body.

This is the Skills `!`cmd`` pattern, but with:
- Ed25519 signing and integrity verification on the tool being called
- Full runtime chain execution (tool → runtime → primitive → Lillux)
- Permission check against the directive's `<permissions>` before execution
- `context_injected` event emitted to the transcript — the injection is on record
- Any RyeOS tool available, not just shell one-liners

### Layer 3 — Extends Chain Context

The `<context>` positions — `<system>`, `<before>`, `<after>` — load named knowledge and directive items into the system prompt and first message assembly, also before turn 1. This is for stable background knowledge (identity, behavior rules, domain conventions) rather than live state.

## The Actual Authoring Gap

The capability is there. The authoring surface is more verbose than Skills' `!`cmd``.

In Skills:
```markdown
- PR diff: !`gh pr diff $ARGUMENTS`
```

In RyeOS, you declare a hook. The hook references a tool. The tool runs through the execution chain. The output gets injected. More moving parts, more authoring ceremony, but each part is doing real work.

The question worth asking: is there a lighter authoring path for the common case of "run this tool before starting and put the output in my context"? The machinery for doing this with full integrity and permissions already exists. The question is whether a more concise directive-level declaration syntax makes sense — something like a `<pre_context>` shorthand that compiles down to the same hook execution path.

That would be a syntax convenience on top of a working system, not a capability addition. Whether it is worth the complexity of a new directive syntax element depends on how often directive authors need parametrized startup tool execution versus getting it for free through project-level hooks.

## What's Worth Documenting

The `thread_started` context hook pattern for dynamic context injection is underexplored. The existing `ctx_directive_instruction` hook demonstrates the mechanism (execute a knowledge item at startup, inject after the directive body), but there are no examples of hooks that execute tools with input interpolation.

Adding an example to the authoring docs — showing a `thread_started` hook that runs `rye/bash` with `${inputs.field}` and injects the result — would make this pattern discoverable without adding any new machinery.
