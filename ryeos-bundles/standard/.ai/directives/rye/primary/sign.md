---
description: "Validate an item's structure and metadata, then sign it with a cryptographic signature."
version: "1.0.0"
model_tier: fast
limits:
  turns: 3
  tokens: 2048
permissions:
  sign:
    - directive:*
    - tool:*
    - knowledge:*
---

# Sign

Validate and sign a directive, tool, or knowledge item.

<process>
  <step name="validate_inputs">
    Validate that {input:item_type} is one of: directive, tool, knowledge.
    Validate that {input:item_id} is non-empty.
    Default {input:source} to "project" if not provided.
  </step>

  <step name="call_sign">
    Validate and sign the item:
    `rye_sign(item_type="{input:item_type}", item_id="{input:item_id}", source="{input:source}")`
  </step>

  <step name="return_result">
    Return whether signing succeeded. If validation failed, return the validation errors.
  </step>
</process>
