<!-- ryeos:signed:2026-05-17T21:44:37Z:1c306ecfaa921e3768a9abdf81e03b2494fc233a0fbd619c61f9951bcfef42aa:xpLO89LitaInTVH74nKEVJkw+k9JullEZCj99cZlrlXS9JWtnSYGCZIWQwEaGzzQv+xgemNqqsfP2B9xhhejAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Search file contents for a text or regex pattern, optionally filtered by path and file glob."
version: "1.0.0"
model_tier: fast
limits:
  turns: 3
  tokens: 2048
permissions:
  execute:
    - tool:rye.file-system.grep
---

# Grep

Search file contents for a text or regex pattern.

<process>
  <step name="validate_inputs">
    Validate that {input:pattern} is non-empty.
  </step>

  <step name="call_grep">
    Search for the pattern:
    `rye_execute(item_id="rye/file-system/grep", parameters={"pattern": "{input:pattern}", "path": "{input:path}", "include": "{input:include}"})`
  </step>

  <step name="return_result">
    Return the list of matching lines with file paths, line numbers, and content.
  </step>
</process>
