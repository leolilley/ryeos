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
