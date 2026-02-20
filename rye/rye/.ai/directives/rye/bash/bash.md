<!-- rye:signed:2026-02-20T01:09:07Z:bf23ad40fd0ab2989bcabc2e88c7d63a1d1013354ced235139e5bc61ac8a907b:GHwij2opoB91oikxTzC5qPxQ4RbxskqJl1hY0pKlTKJaCQCbaE40-0npSrFKtSMi1oGmfnyW9Lxge3wJgmj3Cg==:440443d0858f0199 -->
# Bash

Execute a shell command via subprocess.

```xml
<directive name="bash" version="1.0.0">
  <metadata>
    <description>Execute a shell command via subprocess with optional timeout and working directory.</description>
    <category>rye/bash</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.bash.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="command" type="string" required="true">
      Shell command to execute
    </input>
    <input name="timeout" type="integer" required="false">
      Maximum execution time in seconds (default: 120)
    </input>
    <input name="working_dir" type="string" required="false">
      Working directory for the command. If omitted, uses the project root.
    </input>
  </inputs>

  <outputs>
    <output name="result">Command output including stdout, stderr, and exit code</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:command} is non-empty.
    Default {input:timeout} to 120 if not provided.
  </step>

  <step name="call_bash">
    Execute the shell command:
    `rye_execute(item_type="tool", item_id="rye/bash/bash", parameters={"command": "{input:command}", "timeout": {input:timeout}, "working_dir": "{input:working_dir}"})`
  </step>

  <step name="return_result">
    Return the command output including stdout, stderr, and exit code.
  </step>
</process>
