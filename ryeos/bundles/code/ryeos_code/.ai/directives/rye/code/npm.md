<!-- rye:signed:2026-02-26T03:49:41Z:5b690a6704b241e12076949b9049a1f1b08a7c966dabc2421667691b47e180fc:t6gKVLx_lhQBwkTVIlbLfuHlq_lXIN6llRUzjC6KIpkVtm7PYzU_vdYOkt8YeaTi29IzhDqdmUcCPFuPV8oWAQ==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->
# NPM

Run NPM operations — install packages, run scripts, build, test, init, and exec via npx.

```xml
<directive name="npm" version="1.0.0">
  <metadata>
    <description>Run NPM operations — install packages, run scripts, build, test, init, and exec via npx.</description>
    <category>rye/code</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.code.npm.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="action" type="string" required="true">NPM action: install, run, build, test, init, exec</input>
    <input name="args" type="array" required="false">Arguments (package names for install, script name for run, command for exec)</input>
    <input name="flags" type="object" required="false">Flags to pass (e.g. save_dev: true, force: true)</input>
    <input name="working_dir" type="string" required="false">Working directory relative to project root</input>
    <input name="timeout" type="integer" required="false">Timeout in seconds (default: 120)</input>
  </inputs>

  <outputs>
    <output name="result">Command output with exit code</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:action} is non-empty. If not, halt with an error.
  </step>

  <step name="run_npm">
    Call the npm tool with the provided parameters.
    `rye_execute(item_type="tool", item_id="rye/code/npm/npm", parameters={"action": "{input:action}", "args": "{input:args}", "flags": "{input:flags}", "working_dir": "{input:working_dir}", "timeout": "{input:timeout}"})`
  </step>

  <step name="return_result">
    Return the output as {output:result}.
  </step>
</process>
