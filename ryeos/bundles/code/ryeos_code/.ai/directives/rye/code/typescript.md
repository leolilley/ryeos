<!-- rye:signed:2026-02-26T05:52:24Z:c2a9e4e385dc08ff8eb6672237444c44fd0177c33b8426059be6119e74086e4a:VMPs8SdWhx85Chr7a4zQMSvkobdpzMBWEZhe0pTk4SodvxJbNLbswUPDymhvP9Ez31KTfUWpeIJ_DMuVcFBHBg==:4b987fd4e40303ac -->
<!-- rye:unsigned -->
# TypeScript Type Check

Type check TypeScript code — run tsc --noEmit on a project or single file.

```xml
<directive name="typescript" version="1.0.0">
  <metadata>
    <description>Type check TypeScript code — run tsc --noEmit on a project or single file.</description>
    <category>rye/code</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.code.typescript.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="action" type="string" required="true">check (whole project) or check-file (single file)</input>
    <input name="file_path" type="string" required="false">File to check (required for check-file action)</input>
    <input name="working_dir" type="string" required="false">Directory containing tsconfig.json</input>
    <input name="strict" type="boolean" required="false">Enable strict mode (default: false)</input>
    <input name="timeout" type="integer" required="false">Timeout in seconds (default: 60)</input>
  </inputs>

  <outputs>
    <output name="result">Type check results with diagnostics</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:action} is non-empty. If check-file, validate file_path is provided.
  </step>

  <step name="run_tsc">
    Call the TypeScript type check tool with the provided parameters.
    `rye_execute(item_type="tool", item_id="rye/code/typescript/typescript", parameters={"action": "{input:action}", "file_path": "{input:file_path}", "working_dir": "{input:working_dir}", "strict": "{input:strict}", "timeout": "{input:timeout}"})`
  </step>

  <step name="return_result">
    Return output as {output:result}.
  </step>
</process>
