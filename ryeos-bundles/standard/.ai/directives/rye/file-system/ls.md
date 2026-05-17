<!-- ryeos:signed:2026-05-17T21:44:37Z:43b3810cb9932084704eb9e7e86ce7f3073f1f8d412ed8ec9c9373c6fbbe7e93:eq0Sc3/taMhFfW2CoOUWJLH1KJjH2J+l3vDnai5Bjy9lw+KHJolu4lKA35/QIA0jD2VZnXmsr8/O9FXZdueAAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
