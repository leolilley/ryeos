<!-- rye:signed:2026-02-22T02:31:19Z:9d08128d12cec8c21422edf820c8330dffa70f4e47e949b069d3f1e7a2db5f7d:Bf0MVazZaDaQS0ziQXpQW5E4NFcAGpfvwd3MFtmq0c30iB1J9uuRjmbmjdHHep2tdYYyZS-KeZ6ksaMq3H3sAw==:9fbfabe975fa5a7f -->
# LSP Diagnostics

Run LSP linters on a file and return diagnostics.

```xml
<directive name="lsp" version="1.0.0">
  <metadata>
    <description>Run LSP-based linters on a file and return diagnostics (errors, warnings, hints).</description>
    <category>rye/lsp</category>
    <author>rye-os</author>
    <model tier="fast" />
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
