<!-- rye:signed:2026-02-26T03:49:41Z:b1569f1d53127f25326f90ef2ad2db5bd9c453586c6ab9d044384832df726cdb:d8CBsgRKGP2AsYuxdIVm7DUI9pEf9PmcqeQYiOJH9TGCd85Ql3CGh1CPTUjnsvwE9r5EQQAC7ykCYgXuVY0CCQ==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->
# Code Diagnostics

Run linters and type checkers on a file — ruff, mypy, eslint, tsc, and more.

```xml
<directive name="diagnostics" version="1.0.0">
  <metadata>
    <description>Run linters and type checkers on a file — ruff, mypy, eslint, tsc, and more.</description>
    <category>rye/code</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.code.diagnostics.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="file_path" type="string" required="true">Path to the file to check</input>
    <input name="linters" type="array" required="false">Linters to run (auto-detected from file type if not specified)</input>
    <input name="timeout" type="integer" required="false">Timeout per linter in seconds (default: 30)</input>
  </inputs>

  <outputs>
    <output name="diagnostics">Diagnostic messages with severity, line, column, message, and code</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:file_path} is non-empty. If not, halt with an error.
  </step>

  <step name="run_diagnostics">
    Call the diagnostics tool with the provided parameters.
    `rye_execute(item_type="tool", item_id="rye/code/diagnostics/diagnostics", parameters={"file_path": "{input:file_path}", "linters": "{input:linters}", "timeout": "{input:timeout}"})`
  </step>

  <step name="return_diagnostics">
    Return the output as {output:diagnostics}.
  </step>
</process>
