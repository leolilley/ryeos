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
2. **User message context** — injected into the first user message before and after the directive body (environment info, directive instruction, directive-declared knowledge)

Together, these give the agent a complete working context on its first turn — no discovery required.

## System Messages

System messages are assembled from `build_system_prompt` hooks and delivered via the API's system message parameter. They contain foundational instructions that apply to every turn:

- **Identity** (`rye/agent/core/Identity`) — who the agent is, its role
- **Behavior** (`rye/agent/core/Behavior`) — operational rules, safety constraints
- **Tool protocol** (`rye/agent/core/ToolProtocol`) — how to call Rye tools

These are loaded by built-in hooks defined in `hook_conditions.yaml`:

```yaml
- id: "system_identity"
  event: "build_system_prompt"
  layer: 2
  position: "before"
  action:
    primary: "load"
    item_type: "knowledge"
    item_id: "rye/agent/core/Identity"

- id: "system_behavior"
  event: "build_system_prompt"
  layer: 2
  position: "before"
  action:
    primary: "load"
    item_type: "knowledge"
    item_id: "rye/agent/core/Behavior"

- id: "system_tool_protocol"
  event: "build_system_prompt"
  layer: 2
  position: "before"
  action:
    primary: "load"
    item_type: "knowledge"
    item_id: "rye/agent/core/ToolProtocol"
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

| Provider  | How system messages are sent                   |
| --------- | ---------------------------------------------- |
| Anthropic | `system` parameter on the messages API call    |
| OpenAI    | `{"role": "system", "content": "..."}` message |
| Gemini    | `system_instruction` parameter                 |

The runner passes the assembled string to the provider — each provider adapter maps it to the correct API field.

## User Message Context

User message context is injected into the first user message via `thread_started` hooks (or `thread_continued` for resumed threads). Two built-in hooks inject before the directive body:

| Position | Built-in Hook              | Knowledge Item                        | `wrap` | Purpose                           |
| -------- | -------------------------- | ------------------------------------- | ------ | --------------------------------- |
| `before` | `ctx_environment`          | `rye/agent/core/Environment`          | `true` | Runtime environment, project info |
| `before` | `ctx_directive_instruction`| `rye/agent/core/DirectiveInstruction` | `false`| How to interpret directive bodies  |

Hooks can set `wrap: false` to inject content without XML wrapping. By default, injected knowledge is wrapped in an XML tag derived from the knowledge item's `name` field (e.g. `<Environment id="rye/agent/core/Environment" type="knowledge">...</Environment>`). With `wrap: false`, the raw content is injected directly — this is used for `DirectiveInstruction` which needs to appear as plain instructions before the directive body, not as a tagged context block.

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
[environment context]            ← before (wrapped)
[directive instruction]          ← before (raw, wrap: false)
[directive body + inputs]        ← the actual task
[directive after-context]        ← after (if any)
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
      <suppress>tool-protocol</suppress>
    </context>
    ...
  </metadata>
</directive>
```

These items are loaded at thread startup and merged with hook-injected context:

| Tag          | Destination                                              |
| ------------ | -------------------------------------------------------- |
| `<system>`   | Appended to the system message (after hook layers)       |
| `<before>`   | Injected between hook before-context and directive body  |
| `<after>`    | Injected after directive body                            |
| `<suppress>` | Skips a named hook-driven context layer                  |

Directive-declared context items are resolved via the `load` tool (same as knowledge items loaded by hooks), so they follow the standard three-tier resolution: project → user → system.

### Suppressing Context Layers

The `<suppress>` tag skips a specific hook-driven context layer. It matches against:

- The hook's `id` field (e.g. `system_tool_protocol`)
- The action's full knowledge `item_id` (e.g. `rye/agent/core/ToolProtocol`)

Basename matching (e.g. just `tool-protocol`) is intentionally not supported to avoid ambiguous clashes across namespaces.

This is useful when a directive needs to replace a default layer with something custom:

```xml
<context>
  <suppress>system_tool_protocol</suppress>
  <before>project/custom-tool-protocol</before>
</context>
```

Suppressions apply to both `build_system_prompt` and `thread_started` hooks. They are also composed through `extends` chains — if any directive in the chain suppresses a layer, it stays suppressed.

### Message Assembly Order

With both hook context and directive context, the first user message is assembled as:

```
hook before-context (environment)     ← from thread_started hooks (wrapped)
hook before-context (directive instr) ← from thread_started hooks (raw, wrap: false)
directive before-context              ← from <before> knowledge items
directive prompt (body + outputs)     ← from _build_prompt()
directive after-context               ← from <after> knowledge items
```

## The `extends` Chain

When a directive uses `extends`, context items from the entire inheritance chain are composed root-first:

```xml
<!-- rye/agent/core/base declares: -->
<context>
  <system>rye/agent/core/Identity</system>
  <system>rye/agent/core/Behavior</system>
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

## Project-Level Context Customization

Projects can customize what context threads receive without modifying system files or individual directives.

### Override via Knowledge Items

The simplest approach: create a project-level knowledge item that shadows the system default. `LoadTool` cascades project → user → system, so a project file wins:

```
.ai/knowledge/rye/agent/core/Identity.md    ← project override
```

All threads in the project will load this instead of the system identity. No hook changes needed.

### Additive Hooks via `hooks.yaml`

Add extra context for specific directive categories using `.ai/config/agent/hooks.yaml`:

```yaml
hooks:
  - id: "project_deploy_rules"
    event: "thread_started"
    layer: 2
    position: "before"
    condition:
      path: "directive"
      op: "contains"
      value: "deploy"
    action:
      primary: "load"
      item_type: "knowledge"
      item_id: "project/deploy/rules"
```

This adds deploy-specific context alongside the default layers — it doesn't replace anything.

### Conditional Context via `hook_conditions.yaml`

For dynamic identity switching — different contexts for different directive types — override the system hooks in `.ai/config/hook_conditions.yaml`. The `ConfigLoader` merge-by-id system replaces hooks with the same ID:

```yaml
context_hooks:
  # Replace default identity with a conditional version
  - id: "system_identity"
    event: "build_system_prompt"
    layer: 2
    position: "before"
    condition:
      not:
        path: "directive"
        op: "contains"
        value: "web"
    action:
      primary: "load"
      item_type: "knowledge"
      item_id: "rye/agent/core/Identity"

  # Add: web directives get a different identity
  - id: "project_web_identity"
    event: "build_system_prompt"
    layer: 2
    position: "before"
    condition:
      path: "directive"
      op: "contains"
      value: "web"
    action:
      primary: "load"
      item_type: "knowledge"
      item_id: "project/identities/web-agent"
```

This gives web directives a specialized identity while all other directives keep the default. The condition evaluator supports:

| Operator   | Example                                                       | Matches when       |
| ---------- | ------------------------------------------------------------- | ------------------ |
| `eq`       | `{path: "directive", op: "eq", value: "init"}`                | Exact match        |
| `contains` | `{path: "directive", op: "contains", value: "web"}`           | Substring match    |
| `regex`    | `{path: "directive", op: "regex", value: "^project/deploy/"}` | Regex match        |
| `in`       | `{path: "model", op: "in", value: ["gemini", "claude"]}`      | Value in list      |
| `not`      | `{not: {path: "directive", op: "contains", value: "web"}}`    | Inverts child      |
| `any`      | `{any: [{...}, {...}]}`                                       | Any child matches  |
| `all`      | `{all: [{...}, {...}]}`                                       | All children match |

The context dict available to conditions includes: `directive`, `directive_body`, `model`, `limits`, `inputs`.

### Precedence Summary

| Mechanism                                 | Scope                          | Effect                                        |
| ----------------------------------------- | ------------------------------ | --------------------------------------------- |
| Project knowledge item override           | All threads in project         | Shadows system knowledge via LoadTool cascade |
| Project `hooks.yaml`                      | All threads matching condition | Adds extra context hooks                      |
| Project `hook_conditions.yaml`            | All threads matching condition | Replaces/adds system hooks by ID              |
| Directive `<suppress>`                    | Single directive               | Skips specific hook layers                    |
| Directive `<before>`/`<after>`/`<system>` | Single directive               | Adds extra knowledge items                    |

## Hooks vs `<context>`

Both hooks and `<context>` inject knowledge, but they serve different purposes:

| Aspect          | Hooks                                               | `<context>`                                     |
| --------------- | --------------------------------------------------- | ----------------------------------------------- |
| **Definition**  | `hook_conditions.yaml` or `hooks.yaml`              | Directive `<context>` metadata section          |
| **When**        | Dynamic — evaluated at runtime with conditions      | Static — declared at authoring time             |
| **Conditional** | Yes — `condition` field with full evaluator         | No — always loaded if declared                  |
| **Scope**       | System-wide, project-wide, or per-directive         | Per-directive (composed through `extends`)      |
| **Use case**    | Infrastructure concerns, dynamic identity switching | Domain knowledge specific to a directive's task |

In practice, hooks handle foundational context (identity, behavior, tool protocol, environment) and dynamic switching (different identity per directive category), while `<context>` handles directive-specific domain knowledge that varies by task.

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
  "before": ["ctx_environment", "ctx_directive_instruction"],
  "after": []
}
```

These events are useful for debugging — they show exactly what the LLM received and where it came from.

## Token Budget

Context injection adds approximately **~1,100 tokens** of overhead to the first turn (identity + behavior + tool protocol + environment + directive instruction). This is a fixed cost that pays for itself on the first turn by:

- Eliminating "what tools do I have?" discovery loops (saves 2–3 turns)
- Preventing tool call format errors (saves retry turns)
- Providing directive instruction so the agent knows how to interpret directive bodies

For a haiku-tier thread at ~$0.001/turn, the context overhead costs less than $0.001 and saves $0.002–0.003 in avoided discovery turns.

## What's Next

- [Authoring Directives — Context Injection](../authoring/directives.md#context-injection-with-context) — How to declare `<context>` and `<suppress>` in directives
- [Authoring Knowledge](../authoring/knowledge.md) — How to create knowledge items for context injection
- [Permissions and Capabilities](./permissions-and-capabilities.md) — How capabilities control thread access
- [Safety and Limits](./safety-and-limits.md) — Cost controls and the SafetyHarness
