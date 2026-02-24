```yaml
id: context-injection
title: "Context Injection"
description: How system messages, directive context, and hooks deliver knowledge to threads
category: orchestration
tags: [context, system-prompt, hooks, extends]
version: "1.0.0"
```

# Context Injection

Context injection is the system that loads knowledge into threads before the LLM starts reasoning. It prevents thinking loops ("what tools do I have?"), tool avoidance ("I don't know how to use that"), and identity confusion ("who am I?") by front-loading the information the agent needs.

## Overview

Every thread receives injected context through two channels:

1. **System messages** — delivered via the LLM provider's system message field (identity, behavior rules, tool protocol)
2. **User message context** — injected into the first user message before and after the directive body (environment info, completion protocol, directive-declared knowledge)

Together, these give the agent a complete working context on its first turn — no discovery required.

## System Messages

System messages are assembled from `build_system_prompt` hooks and delivered via the API's system message parameter. They contain foundational instructions that apply to every turn:

- **Identity** (`rye/agent/core/identity`) — who the agent is, its role
- **Behavior** (`rye/agent/core/behavior`) — operational rules, safety constraints
- **Tool protocol** (`rye/agent/core/tool-protocol`) — how to call Rye tools

These are loaded by built-in hooks defined in `hook_conditions.yaml`:

```yaml
- id: "system_identity"
  event: "build_system_prompt"
  layer: 2
  position: "before"
  action:
    primary: "load"
    item_type: "knowledge"
    item_id: "rye/agent/core/identity"

- id: "system_behavior"
  event: "build_system_prompt"
  layer: 2
  position: "before"
  action:
    primary: "load"
    item_type: "knowledge"
    item_id: "rye/agent/core/behavior"

- id: "system_tool_protocol"
  event: "build_system_prompt"
  layer: 2
  position: "before"
  action:
    primary: "load"
    item_type: "knowledge"
    item_id: "rye/agent/core/tool-protocol"
```

The runner assembles system messages by calling `harness.run_hooks_context()` with `event="build_system_prompt"`, then concatenates the `before` and `after` blocks:

```python
system_ctx = await harness.run_hooks_context(
    {"directive": harness.directive_name, ...},
    dispatcher,
    event="build_system_prompt",
)
hook_system = "\n\n".join(filter(None, [system_ctx["before"], system_ctx["after"]]))
```

The assembled system prompt is emitted as a `system_prompt` transcript event with the list of contributing hook layers.

### Provider-Specific Delivery

System messages are passed to the LLM via each provider's native system message mechanism:

| Provider   | How system messages are sent                     |
| ---------- | ------------------------------------------------ |
| Anthropic  | `system` parameter on the messages API call      |
| OpenAI     | `{"role": "system", "content": "..."}` message   |
| Gemini     | `system_instruction` parameter                   |

The runner passes the assembled string to the provider — each provider adapter maps it to the correct API field.

## User Message Context

User message context is injected into the first user message via `thread_started` hooks (or `thread_continued` for resumed threads). Two positions are available:

| Position | Built-in Hook         | Knowledge Item                    | Purpose                        |
| -------- | --------------------- | --------------------------------- | ------------------------------ |
| `before` | `ctx_environment`     | `rye/agent/core/environment`      | Runtime environment, project info |
| `after`  | `ctx_completion`      | `rye/agent/core/completion`       | How to signal completion        |

The runner constructs the first message by sandwiching the directive body:

```python
first_message_parts = []
if hook_ctx["before"]:
    first_message_parts.append(hook_ctx["before"])
first_message_parts.append(user_prompt)       # the directive body
if hook_ctx["after"]:
    first_message_parts.append(hook_ctx["after"])
messages.append({"role": "user", "content": "\n\n".join(first_message_parts)})
```

This produces a first message structured as:

```
[environment context]        ← before
[directive body + inputs]    ← the actual task
[completion protocol]        ← after
```

A `context_injected` event is emitted to the transcript recording which hooks contributed.

## `<context>` in Directives

Directives can declare additional knowledge items to inject using the `<context>` metadata section:

```xml
<directive name="deploy_staging" version="1.0.0">
  <metadata>
    <context>
      <system>project/deploy/system-rules</system>
      <before>project/deploy/environment-rules</before>
      <after>project/deploy/completion-checklist</after>
    </context>
    ...
  </metadata>
</directive>
```

These items are loaded at thread startup and placed into the same three positions as hook-injected context:

| Tag        | Destination                              |
| ---------- | ---------------------------------------- |
| `<system>` | Appended to the system message           |
| `<before>` | Prepended before the directive body      |
| `<after>`  | Appended after the directive body        |

Directive-declared context items are resolved via the `load` tool (same as knowledge items loaded by hooks), so they follow the standard three-tier resolution: project → user → system.

## The `extends` Chain

When a directive uses `extends`, context items from the entire inheritance chain are composed root-first:

```xml
<!-- rye/agent/core/base declares: -->
<context>
  <system>rye/agent/core/identity</system>
  <system>rye/agent/core/behavior</system>
</context>

<!-- project/deploy/base declares: -->
<context>
  <before>project/deploy/environment-rules</before>
</context>

<!-- deploy_staging (leaf) declares: -->
<context>
  <after>project/deploy/completion-checklist</after>
</context>
```

Resolution walks the chain leaf → parent → root, then reverses to compose root-first:

```
Chain: rye/agent/core/base → project/deploy/base → deploy_staging

System:  [identity, behavior]          ← from root
Before:  [environment-rules]           ← from middle
After:   [completion-checklist]        ← from leaf
```

Duplicates are deduplicated — if both parent and child declare the same knowledge item, it appears only once. Circular `extends` chains are detected and rejected.

See [Authoring Directives — Directive Inheritance](../authoring/directives.md#directive-inheritance-with-extends) for the directive-side syntax.

## Hooks vs `<context>`

Both hooks and `<context>` inject knowledge, but they serve different purposes:

| Aspect          | Hooks                                      | `<context>`                              |
| --------------- | ------------------------------------------ | ---------------------------------------- |
| **Definition**  | `hook_conditions.yaml` or directive `<hooks>` | Directive `<context>` metadata section   |
| **When**        | Dynamic — evaluated at runtime by event    | Static — declared at authoring time      |
| **Conditional** | Yes — hooks can have conditions            | No — always loaded if declared           |
| **Scope**       | System-wide, project-wide, or per-directive | Per-directive (composed through `extends`) |
| **Use case**    | Infrastructure concerns (identity, environment, error handling) | Domain knowledge specific to a directive's task |

In practice, hooks handle the "always-on" foundational context (identity, behavior, tool protocol, environment), while `<context>` handles directive-specific domain knowledge that varies by task.

## Transcript Rendering

Context injection produces two transcript events:

### `system_prompt`

Emitted after system message assembly. Contains the full system prompt text and the list of contributing hook layers:

```json
{
  "event": "system_prompt",
  "text": "You are a Rye agent...",
  "layers": ["system_identity", "system_behavior", "system_tool_protocol"]
}
```

### `context_injected`

Emitted after user message context is assembled. Records which hooks contributed before/after content:

```json
{
  "event": "context_injected",
  "before": ["ctx_environment"],
  "after": ["ctx_completion"]
}
```

These events are useful for debugging — they show exactly what the LLM received and where it came from.

## Token Budget

Context injection adds approximately **~1,150 tokens** of overhead to the first turn (identity + behavior + tool protocol + environment + completion). This is a fixed cost that pays for itself on the first turn by:

- Eliminating "what tools do I have?" discovery loops (saves 2–3 turns)
- Preventing tool call format errors (saves retry turns)
- Providing completion protocol so the agent knows when and how to finish

For a haiku-tier thread at ~$0.001/turn, the context overhead costs less than $0.001 and saves $0.002–0.003 in avoided discovery turns.

## What's Next

- [Authoring Directives — Context Injection](../authoring/directives.md#context-injection-with-context) — How to declare `<context>` in directives
- [Permissions and Capabilities](./permissions-and-capabilities.md) — How capabilities control thread access
- [Safety and Limits](./safety-and-limits.md) — Cost controls and the SafetyHarness
