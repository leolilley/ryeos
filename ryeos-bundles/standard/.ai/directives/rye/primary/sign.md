<!-- ryeos:signed:2026-05-17T21:44:36Z:ba62000765885d167a2d3bc314b704de10f638f6550974b592d2268803d85f48:w9eUYrv1YlpCpD2hWUDyl6U5EtpIDoqNqakyPm6jwe5JwhQE2uV+w9ZMKMauvWpcswtHpHJW2VOtLzLQzaLoDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
