---
description: "List files and directories at a given path, defaulting to the project root."
version: "1.0.0"
model_tier: fast
limits:
  turns: 3
  tokens: 2048
permissions:
  execute:
    - tool:rye.file-system.ls
---

# List Directory

List files and directories at a given path.

<process>
  <step name="validate_inputs">
    Default {input:path} to the project root if not provided.
  </step>

  <step name="call_ls">
    List the directory:
    `rye_execute(item_id="rye/file-system/ls", parameters={"path": "{input:path}"})`
  </step>

  <step name="return_result">
    Return the list of files and directories.
  </step>
</process>
