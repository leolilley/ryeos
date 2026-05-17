<!-- ryeos:signed:2026-05-17T21:44:37Z:7863164df0a1f85a965e68e5fec4f42b7120174ac8cf2a84631eb9ec56330de2:LGcS5UpB6mly5jbGtxMspEI/zXcLonSIzyOvgHOPiVJkC1nXM4Qto+tKqBhzncXv3bq8xDzc6bf30FdiJWqmAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Find files matching a glob pattern, optionally scoped to a base directory."
version: "1.0.0"
model_tier: fast
limits:
  turns: 3
  tokens: 2048
permissions:
  execute:
    - tool:rye.file-system.glob
---

# Glob

Find files matching a glob pattern.

<process>
  <step name="validate_inputs">
    Validate that {input:pattern} is non-empty.
  </step>

  <step name="call_glob">
    Find matching files:
    `rye_execute(item_id="rye/file-system/glob", parameters={"pattern": "{input:pattern}", "path": "{input:path}"})`
  </step>

  <step name="return_result">
    Return the list of matching file paths.
  </step>
</process>
