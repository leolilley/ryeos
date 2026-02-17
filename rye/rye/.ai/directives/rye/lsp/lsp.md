<!-- rye:signed:2026-02-10T02:00:00Z:placeholder:unsigned:unsigned -->
# LSP Diagnostics

Run LSP linters on a file and return diagnostics.

```xml
<directive name="lsp" version="1.0.0">
  <metadata>
    <description>Run LSP-based linters on a file and return diagnostics (errors, warnings, hints).</description>
    <category>rye/lsp</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.lsp.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="file_path" type="string" required="true">Path to the file to lint</input>
    <input name="linters" type="array" required="false">List of linter names to run (auto-detected if not specified)</input>
  </inputs>

  <outputs>
    <output name="diagnostics">List of diagnostic messages with severity, line, and message</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:file_path} is non-empty. If empty, halt with an error.
  </step>

  <step name="run_linters">
    Call the LSP tool with the provided parameters.
    `rye_execute(item_type="tool", item_id="rye/lsp/lsp", parameters={"file_path": "{input:file_path}", "linters": "{input:linters}"})`
  </step>

  <step name="return_diagnostics">
    Return the linter output as {output:diagnostics}.
  </step>
</process>
