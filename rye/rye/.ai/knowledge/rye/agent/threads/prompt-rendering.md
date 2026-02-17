<!-- rye:signed:2026-02-17T23:54:02Z:21da3173cf06923fb0740485d6332a23dd4059171190d549b86b866ec1c24768:Mau78SUcFxrKd6xkdRg06bW-_ndmR-d5XId1eqem2IgYnR_5Vbb0cjNDZZh0Gb4Mz1EbYbLvU_7wnihW1ZsGDw==:440443d0858f0199 -->

```yaml
id: prompt-rendering
title: Prompt Rendering
entry_type: reference
category: rye/agent/threads
version: "1.0.0"
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
5. <returns>                    (deterministic from <outputs>)
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
| Returns               | `directive["outputs"]` → `<returns>` | What structured output to produce     |

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

## The `<outputs>` → `<returns>` Transformation

The `<outputs>` block from the XML fence is **not** sent as-is. It's deterministically transformed into a `<returns>` block appended to the prompt body.

### List Format

When `outputs` is a list of `{name, description}` dicts:

```python
# Input
outputs = [
    {"name": "directive_path", "description": "Path to the created file"},
    {"name": "signed", "description": "Whether signing succeeded"},
]

# Output in prompt
<returns>
  <output name="directive_path">Path to the created file</output>
  <output name="signed">Whether signing succeeded</output>
</returns>
```

If an output has no description, it becomes self-closing:

```xml
<output name="count" />
```

### Dict Format

When `outputs` is a dict of `{key: value}` pairs:

```python
# Input
outputs = {"score": "Numeric score 0-100", "tier": "hot, warm, cold"}

# Output in prompt
<returns>
  <output name="score">Numeric score 0-100</output>
  <output name="tier">hot, warm, cold</output>
</returns>
```

## Why This Matters

- **Parent-child contract:** Parent threads match these output keys when consuming child results. Names must be consistent between `<outputs>` declaration and parent expectations.
- **Structured output:** The `<returns>` block tells the LLM exactly what keys to produce, preventing freeform responses when structured output is needed.
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

    # Returns (from outputs)
    outputs = directive.get("outputs", [])
    if outputs:
        output_lines = ["<returns>"]
        if isinstance(outputs, list):
            for o in outputs:
                oname = o.get("name", "")
                odesc = o.get("description", "")
                if odesc:
                    output_lines.append(f'  <output name="{oname}">{odesc}</output>')
                else:
                    output_lines.append(f'  <output name="{oname}" />')
        elif isinstance(outputs, dict):
            for k, v in outputs.items():
                output_lines.append(f'  <output name="{k}">{v}</output>')
        output_lines.append("</returns>")
        parts.append("\n".join(output_lines))

    # Close directive tag (if any opening tag was emitted)
    if name or desc:
        parts.append("</directive>")

    return "\n\n".join(parts)
```
