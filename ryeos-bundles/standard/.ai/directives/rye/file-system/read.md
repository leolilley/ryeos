<!-- ryeos:signed:2026-05-17T21:44:37Z:6839c2a5353c023c1b26da3375cc4b134ac2fb3576187a6a309b823818572032:GMf6N44J94emaI3IG9MHCNCC4FDqd80DdsPC9SW353BUgX+ewlxYU4UWpxWGNFYjv0ZfFmJpgAREScOtL9HyAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Read file contents from disk with optional offset and line limit for large files."
version: "1.0.0"
model_tier: fast
limits:
  turns: 3
  tokens: 2048
permissions:
  execute:
    - tool:rye.file-system.read
---

# Read

Read file contents with optional offset and line limit.

<process>
  <step name="validate_inputs">
    Validate that {input:file_path} is non-empty.
    Default {input:offset} to 1 and {input:limit} to 2000 if not provided.
  </step>

  <step name="call_read">
    Read the file:
    `rye_execute(item_id="rye/file-system/read", parameters={"path": "{input:file_path}", "offset": {input:offset}, "limit": {input:limit}})`
  </step>

  <step name="return_result">
    Return the file contents with line numbers.
  </step>
</process>
