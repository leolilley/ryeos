<!-- ryeos:signed:2026-05-17T21:44:37Z:80b9eaa778505c3f4f1d65238cfa339ff1277a7018d2b9a83fe3ab8c1b81f98d:dYxoWXI/iDIVEFDcKdJDr/SRUKmnzbL8vwqwbXFKJ2ugRFitxK1k53gbHmCmomDcRrd8HDWMgmz24IqH/wcsDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Execute a shell command via subprocess with optional timeout and working directory."
version: "1.0.0"
model_tier: fast
limits:
  turns: 3
  tokens: 4096
permissions:
  execute:
    - tool:rye.bash.*
---

# Bash

Execute a shell command via subprocess.

<process>
  <step name="validate_inputs">
    Validate that {input:command} is non-empty.
    Default {input:timeout} to 120 if not provided.
  </step>

  <step name="call_bash">
    Execute the shell command:
    `rye_execute(item_id="rye/bash/bash", parameters={"command": "{input:command}", "timeout": {input:timeout}, "working_dir": "{input:working_dir}"})`
  </step>

  <step name="return_result">
    Return the command output including stdout, stderr, and exit code.
  </step>
</process>
