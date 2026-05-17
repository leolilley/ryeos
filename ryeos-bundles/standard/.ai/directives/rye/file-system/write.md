<!-- ryeos:signed:2026-05-17T21:44:37Z:41f41d3fcce67b2f031f675454481c839b8bb6be1c582a726933cbe325b8de04:OS46HpIpESiBsu+jpDvxRCmvq8MhOF7xkpSgc00nW27v4HZsL2IeaRQfx5WeeASV0CPVXHAkI5dRkW4hjaetBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Write content to a file on disk, creating parent directories if they do not exist."
version: "1.0.0"
model_tier: fast
limits:
  turns: 3
  tokens: 2048
permissions:
  execute:
    - tool:rye.file-system.write
---

# Write

Write content to a file, creating directories as needed.

<process>
  <step name="validate_inputs">
    Validate that {input:file_path} and {input:content} are non-empty.
  </step>

  <step name="call_write">
    Write the file:
    `rye_execute(item_id="rye/file-system/write", parameters={"path": "{input:file_path}", "content": "{input:content}"})`
  </step>

  <step name="return_result">
    Return the path of the written file.
  </step>
</process>
