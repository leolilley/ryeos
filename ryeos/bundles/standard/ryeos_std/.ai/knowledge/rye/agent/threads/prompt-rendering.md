<!-- rye:signed:2026-04-09T00:11:21Z:7d41f7f3f3b6985cd58a190185ac23fa6a8ac7873b2216732804b798ef8598e4:cnYbTWOOEzo4JYnUMJo54UUX22m6qhlZq4qaxa-ef9O1rHxRkU0qBn05RrdNdxtdSe7Gb2d48g6cXEOBZGN_Bg:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: prompt-rendering
title: Prompt Rendering
entry_type: reference
category: rye/agent/threads
version: "1.2.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - prompt
  - rendering
  - threads
  - returns
  - outputs
  - system-message
  - context-injection
  - extends
references:
  - thread-lifecycle
  - directive-extends
  - "docs/authoring/directives.md"
```

# Prompt Rendering

How `_build_prompt()` transforms a directive into the LLM prompt. Located in `thread_directive.py` (invoked internally by `execute directive`).

## Prompt Structure

The prompt is built by `_build_prompt()` concatenating these parts with `\n`:

```
1. <directive name="..." >      (name + description tag)
2. <permissions>...</permissions> (raw XML from directive metadata)
3. Body                         (process steps — the actual instructions)
4. directive_return instruction  (from <outputs>, via rye_execute)
5. </directive>                 (closing tag)
```

`DirectiveInstruction` (the "STOP. You are now the executor…" preamble) is **not** part of `_build_prompt()`. It is injected via the `ctx_directive_instruction` context hook at `thread_started` time (see hook_conditions.yaml). The hook uses `wrap: false` to inject raw text without XML wrapping. For in-thread mode (non-threaded `execute`), the constant `DIRECTIVE_INSTRUCTION` is returned via `your_directions` in `execute.py`.

## What's INCLUDED in the Prompt

| Component             | Source                           | Purpose                                    |
|-----------------------|----------------------------------|--------------------------------------------|
| Directive name        | `directive["name"]`              | Context: which directive is running        |
| Description           | `directive["description"]`       | Context: what this directive does          |
| Permissions           | `directive["content"]` (regex)   | Raw `<permissions>` XML block              |
| Body                  | `directive["body"]`              | Process steps — the actual LLM instructions|
| Returns               | `directive["outputs"]` → `directive_return` call | Instructs the LLM to call `directive_return` via `rye_execute` |

## What's EXCLUDED from the Prompt

The LLM does **not** receive:

- Metadata XML (`<metadata>`, `<permissions>`, `<limits>`, `<model>`, `<hooks>`)
- Signature comments (`<!-- rye:signed:... -->`)
- Raw XML fences (the ` ```xml ` wrapper)
- Permission declarations
- Limit values
- Model configuration
- Hook definitions

These are consumed by infrastructure (`thread_directive.py`, `SafetyHarness`, `provider_resolver`).

## The `<outputs>` → `directive_return` Transformation

The `<outputs>` block from the XML fence is **not** sent as-is. It's transformed into an instruction telling the LLM to call `directive_return` via `rye_execute` with the declared output fields as parameters.

### List Format

When `outputs` is a list of `{name, description}` dicts:

```python
# Input
outputs = [
    {"name": "directive_path", "description": "Path to the created file"},
    {"name": "signed", "description": "Whether signing succeeded"},
]

# Output in prompt
When you have completed all steps, return structured results:
`rye_execute(item_id="rye/agent/threads/directive_return", parameters={"directive_path": "<Path to the created file>", "signed": "<Whether signing succeeded>"})`
```

If an output has no description, the field name is used as the placeholder:

```
parameters={"count": "<count>"}
```

### Dict Format

When `outputs` is a dict of `{key: value}` pairs:

```python
# Input
outputs = {"score": "Numeric score 0-100", "tier": "hot, warm, cold"}

# Output in prompt
When you have completed all steps, return structured results:
`rye_execute(item_id="rye/agent/threads/directive_return", parameters={"score": "<Numeric score 0-100>", "tier": "<hot, warm, cold>"})`
```

## Why This Matters

- **Parent-child contract:** Parent threads match these output keys when consuming child results. Names must be consistent between `<outputs>` declaration and parent expectations.
- **Structured output via tool call:** Instead of a passive XML block, the LLM is instructed to actively call `directive_return` with the declared output fields. This produces structured results that the thread infrastructure can reliably parse.
- **Separation of concerns:** Infrastructure metadata stays in the XML fence for the parser. Only execution-relevant content reaches the LLM.

## Code Reference

```python
def _build_prompt(directive: Dict) -> str:
    import re as _re
    parts = []

    # Directive name + description
    name = directive.get("name", "")
    desc = directive.get("description", "")
    if name and desc:
        parts.append(f'<directive name="{name}">\n<description>{desc}</description>')
    elif name:
        parts.append(f'<directive name="{name}">')
    elif desc:
        parts.append(f'<directive>\n<description>{desc}</description>')

    # Permissions — extract raw XML from directive content as-is
    content = directive.get("content", "")
    if content:
        m = _re.search(r"(<permissions>.*?</permissions>)", content, _re.DOTALL)
        if m:
            parts.append(m.group(1))

    # Body (process steps — the actual instructions, already pseudo-XML)
    body = directive.get("body", "").strip()
    if body:
        parts.append(body)

    # Returns (from outputs) — directive_return call instruction
    outputs = directive.get("outputs", [])
    if outputs:
        output_fields = {}
        if isinstance(outputs, list):
            for o in outputs:
                oname = o.get("name", "")
                if oname:
                    otype = o.get("type", "string")
                    required = o.get("required", False)
                    desc = o.get("description", "")
                    label = f"{desc} ({otype})" if desc else otype
                    if required:
                        label += " [required]"
                    output_fields[oname] = label
        elif isinstance(outputs, dict):
            output_fields = dict(outputs)

        if output_fields:
            params_obj = ", ".join(f'"{k}": "<{v or k}>"' for k, v in output_fields.items())
            parts.append(
                "When you have completed all steps, return structured results:\n"
                f'`rye_execute(item_id="rye/agent/threads/directive_return", '
                f"parameters={{{params_obj}}})`"
            )

    # Close directive tag if opened
    if name or desc:
        parts.append("</directive>")

    return "\n".join(parts)
```

## System Message Assembly

System messages are assembled from `<system>` context items declared in the directive's extends chain. This content is delivered via the provider's native system message field — it is **not** stuffed into a user message.

### How It Works

1. The extends chain is walked root-first, collecting `<context><system>` knowledge items
2. Each knowledge item is loaded and its content extracted
3. Non-empty content blocks are concatenated to form the final system prompt
4. The assembled prompt is sent as the API's system message

### Provider-Specific Delivery

| Provider        | Delivery Mechanism                                    |
|-----------------|-------------------------------------------------------|
| Anthropic       | Top-level `system` field in the API request           |
| Gemini          | `systemInstruction` field                             |
| OpenAI-compat   | Message with `role: "system"` at the start of messages|

This ensures each provider receives the system prompt in its idiomatic format.

## Context Hook XML Wrapping

By default, context hooks wrap their loaded content in PascalCase XML tags with a `type` attribute derived from the knowledge item's name and type:

```xml
<Identity id="rye/agent/core/Identity" type="knowledge">
...content...
</Identity>
```

The tag name comes from the item's `name` field in its YAML frontmatter (PascalCase). The `type` attribute reflects the `item_type` from the hook action.

### The `wrap: false` Option

Hooks can set `wrap: false` to inject raw content without XML wrapping. This is used by the `ctx_directive_instruction` hook so `DirectiveInstruction` content appears as bare text (not inside XML tags):

```yaml
- id: "ctx_directive_instruction"
  event: "thread_started"
  layer: 2
  position: "before"
  wrap: false
  action:
    primary: "execute"
    item_type: "knowledge"
    item_id: "rye/agent/core/DirectiveInstruction"
```

When `wrap: false`, the content string is injected as-is into the prompt position.

## Context Injection from `<context>` Directive Metadata

Directives can declare a `<context>` metadata section that specifies knowledge items to load and inject at specific positions in the prompt, or suppress hook-driven context layers.

### Positions

| Position      | Where Injected                                           |
|---------------|----------------------------------------------------------|
| `<system>`    | Appended to the system message (after hook-driven layers)|
| `<before>`    | Injected between hook before-context and directive body  |
| `<after>`     | Injected between directive body and hook after-context   |
| `<suppress>`  | Skips the named inherited context layer from extends chain |

### Knowledge Item Loading

Context entries reference knowledge items by ID. These are loaded via `FetchTool` (which cascades project → user → system) and injected at the declared position.

```xml
<context>
  <system>project/custom-identity</system>
  <before>project/coding-standards</before>
  <after>project/completion-rules</after>
</context>
```

### Suppressing Inherited Context Layers

Directives can suppress specific inherited context layers using `<suppress>`. The value matches against the knowledge `item_id` in the extends chain:

Basename matching is intentionally not supported to avoid ambiguous clashes (e.g. `Identity` matching both `rye/agent/core/Identity` and `project/auth/Identity`).

```xml
<context>
  <suppress>rye/agent/core/Identity</suppress>
  <system>project/custom-identity</system>
</context>
```

This prevents the inherited `Identity` from being injected and replaces it with a project-specific one.

### Message Assembly Order

The first user message is assembled in this order:

```
hook before-context (environment)     ← from thread_started hooks
directive before-context              ← from <before> knowledge items
directive prompt (body + outputs)     ← from _build_prompt()
directive after-context               ← from <after> knowledge items
```

Suppressions apply to context items from the extends chain and `thread_started` hooks.

## Directive Extends and Context Composition

When a directive uses `extends`, the context is composed **root-first** along the inheritance chain. This means the base directive's context appears first, then each child's context layers on top.

### Composition Order

```
base directive context (root)
  → parent directive context
    → leaf directive context (current)
```

- System-position content is concatenated root-first into the system message
- Before/after content follows the same root-first ordering
- Duplicate knowledge items are deduplicated (first occurrence wins)

See the `directive-extends` knowledge item for the full inheritance model.

## Per-Project Context Customization

Projects can customize the thread context without modifying system-level knowledge items.

### Override via Knowledge Items

`FetchTool` cascades project → user → system. To override the default identity:

1. Create `.ai/knowledge/rye/agent/core/Identity.md` in your project
2. The project-level file will be loaded instead of the system default
3. No directive changes needed — extends chains automatically pick up the override

This works for any core knowledge item: `Identity`, `Behavior`, `Environment`, and the decomposed protocol items (`protocol/execute`, `protocol/fetch`, `protocol/sign`).

### Override via Directive `<context>`

For per-directive customization (not project-wide), use `<context>` metadata:

```xml
<context>
  <suppress>rye/agent/core/Identity</suppress>
  <system>project/custom-identity</system>
</context>
```

### Precedence

| Mechanism | Scope | Applies To |
|-----------|-------|------------|
| Project knowledge item override | All threads in project | Extends chains loading that item |
| Directive `<suppress>` | Single directive | Named inherited context layers |
| Directive `<before>`/`<after>` | Single directive | Additional context items |
| Directive `<system>` | Single directive | System message extensions |

## Transcript Events

The rendering pipeline records events for observability:

| Event               | When Recorded                                          |
|---------------------|--------------------------------------------------------|
| `system_prompt`     | After system message assembly completes                |
| `context_injected`  | After each context layer is injected into the prompt   |

These events appear in the thread transcript and can be used for debugging prompt construction.
