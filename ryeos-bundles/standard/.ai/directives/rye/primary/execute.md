<!-- ryeos:signed:2026-05-17T21:44:36Z:a36c12df4bd5c548955956f5653023477be1288044a888108d87c25897e5ac5b:NO9ILSaoVMrsxG0V9QyLQ5UvXekOd54deWZ0KLTWzdTVWmvzUT2pr1nBDWkST78qGGzG6ZwfrecIFIvN7WiUAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Execute a directive, tool, or knowledge item. Supports dry_run mode for validation without side effects."
version: "1.0.0"
model_tier: fast
limits:
  turns: 4
  tokens: 4096
permissions:
  execute:
    - tool:*
    - directive:*
    - knowledge:*
---

# Execute

Execute a directive, tool, or knowledge item by id with optional parameters.

<process>
  <step name="validate_inputs">
    Validate that {input:item_type} is one of: directive, tool, knowledge.
    Validate that {input:item_id} is non-empty.
    Default {input:dry_run} to false if not provided.
  </step>

  <step name="call_execute">
    Execute the item:
    `rye_execute(item_id="{input:item_id}", parameters={input:parameters}, dry_run={input:dry_run})`
  </step>

  <step name="return_result">
    Return the execution result to the caller. If dry_run was true, return the validation result instead.
  </step>
</process>
