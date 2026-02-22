<!-- rye:signed:2026-02-22T02:41:03Z:785ec8e349cefab3c9af15981bc2a816d6025a5749588b48b85452054a966266:wZL2iSXNBM5dVt1lzGoB3YEzpmAHugZ5xCRem4bBBuJBg4vGGJxdlnI0FYy1CM1YxF8AHrr3YLIYDfiB6SdvBQ==:9fbfabe975fa5a7f -->

```yaml
id: prompt-rendering
title: Prompt Rendering
entry_type: reference
category: rye/agent/threads
version: "1.1.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - prompt
  - rendering
  - threads
  - returns
  - outputs
references:
  - thread-lifecycle
  - "docs/authoring/directives.md"
```

# Prompt Rendering

How `_build_prompt()` transforms a directive into the LLM prompt. Located in `thread_directive.py`.

## Prompt Structure

The prompt is built by concatenating these parts with `\n\n`:

```
1. DIRECTIVE_INSTRUCTION        (constant from rye.constants)
2. <directive name="..." >      (name + description tag)
3. Preamble                     (cleaned markdown before XML fence)
4. Body                         (process steps — the actual instructions)
5. directive_return instruction  (from <outputs>, via rye_execute)
6. </directive>                 (closing tag)
```

## What's INCLUDED in the Prompt

| Component             | Source                           | Purpose                                    |
|-----------------------|----------------------------------|--------------------------------------------|
| `DIRECTIVE_INSTRUCTION` | `rye.constants`                | System-level execution instruction         |
| Directive name        | `directive["name"]`              | Context: which directive is running        |
| Description           | `directive["description"]`       | Context: what this directive does          |
| Preamble              | `directive["preamble"]`          | Summary text (markdown before XML fence)   |
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

## Preamble Cleaning

The preamble (markdown text before the XML fence) is cleaned:

```python
preamble_lines = [
    l for l in preamble.split("\n")
    if not l.strip().startswith(("<!-- rye:signed:", "# "))
]
```

Removes:
- Signature comments (`<!-- rye:signed:...`)
- Markdown headings (`# Title`)

Keeps: description paragraphs and context text.

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
`rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", parameters={"directive_path": "<Path to the created file>", "signed": "<Whether signing succeeded>"})`
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
`rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", parameters={"score": "<Numeric score 0-100>", "tier": "<hot, warm, cold>"})`
```

## Why This Matters

- **Parent-child contract:** Parent threads match these output keys when consuming child results. Names must be consistent between `<outputs>` declaration and parent expectations.
- **Structured output via tool call:** Instead of a passive XML block, the LLM is instructed to actively call `directive_return` with the declared output fields. This produces structured results that the thread infrastructure can reliably parse.
- **Separation of concerns:** Infrastructure metadata stays in the XML fence for the parser. Only execution-relevant content reaches the LLM.

## Code Reference

```python
def _build_prompt(directive: Dict) -> str:
    from rye.constants import DIRECTIVE_INSTRUCTION
    parts = [DIRECTIVE_INSTRUCTION]

    # Name + description (handles partial presence)
    name = directive.get("name", "")
    desc = directive.get("description", "")
    if name and desc:
        parts.append(f'<directive name="{name}">\n<description>{desc}</description>')
    elif name:
        parts.append(f'<directive name="{name}">')
    elif desc:
        parts.append(f'<directive>\n<description>{desc}</description>')

    # Preamble (cleaned)
    preamble = directive.get("preamble", "").strip()
    if preamble:
        preamble_lines = [l for l in preamble.split("\n")
                          if not l.strip().startswith(("<!-- rye:signed:", "# "))]
        preamble_clean = "\n".join(preamble_lines).strip()
        if preamble_clean:
            parts.append(preamble_clean)

    # Body (process steps)
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
                    output_fields[oname] = o.get("description", "")
        elif isinstance(outputs, dict):
            output_fields = dict(outputs)

        if output_fields:
            params_obj = ", ".join(f'"{k}": "<{v or k}>"' for k, v in output_fields.items())
            parts.append(
                "When you have completed all steps, return structured results:\n"
                f'`rye_execute(item_type="tool", item_id="rye/agent/threads/directive_return", '
                f"parameters={{{params_obj}}})`"
            )

    # Close directive tag (if any opening tag was emitted)
    if name or desc:
        parts.append("</directive>")

    return "\n\n".join(parts)
```
