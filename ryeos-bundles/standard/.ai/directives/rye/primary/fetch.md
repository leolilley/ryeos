<!-- ryeos:signed:2026-05-17T21:44:36Z:abfd8aef73379049ef99fc5561cc17024c76bcc5f75af943f50a18425e2ceacc:MBhObHFaI8Zkrr/gpcUg6QVtHejl0NXvxK9X3QIUnWmzGj0/Prfk1AVZkpfKrjHhgeEOXdNl8cwm5NtEEEvVAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Resolve a name to items. Two modes: ID mode (item_id) returns content, query mode (query+scope) discovers matches."
version: "1.0.0"
model_tier: fast
limits:
  turns: 4
  tokens: 4096
permissions:
  fetch:
    - tool:*
    - directive:*
    - knowledge:*
---

# Fetch

Resolve items by ID or discover by query.

<process>
  <step name="detect_mode">
    If {input:item_id} is provided, use ID mode.
    If {input:query} is provided, use query mode.
    If both are provided, return an error.
  </step>

  <step name="resolve">
    ID mode: `rye_fetch(item_id="{input:item_id}", item_type="{input:item_type}", source="{input:source}", destination="{input:destination}")`
    Query mode: `rye_fetch(query="{input:query}", scope="{input:scope}", source="{input:source}", limit={input:limit})`
  </step>

  <step name="return_result">
    Return the resolved item(s) to the caller.
  </step>
</process>
