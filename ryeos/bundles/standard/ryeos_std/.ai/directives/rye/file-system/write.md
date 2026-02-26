<!-- rye:signed:2026-02-26T05:02:40Z:db51589dc0801e8aab588ee14dfa9533de7eb1d5d91065b10f8035a4c82bc194:6v2mioL0q0JlenpMPzVoY7eOgt68KG857XVrXZEM8M6ngyjJTnEWes0-3GWPHPrWpG1ep8rsz_8VB_HKwrEMDg==:4b987fd4e40303ac -->
# Write

Write content to a file, creating directories as needed.

```xml
<directive name="write" version="1.0.0">
  <metadata>
    <description>Write content to a file on disk, creating parent directories if they do not exist.</description>
    <category>rye/file-system</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.file-system.write</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="file_path" type="string" required="true">
      Path to the file to write (absolute or relative to project root)
    </input>
    <input name="content" type="string" required="true">
      Content to write to the file
    </input>
  </inputs>

  <outputs>
    <output name="path">Path to the written file</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:file_path} and {input:content} are non-empty.
  </step>

  <step name="call_write">
    Write the file:
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:file_path}", "content": "{input:content}"})`
  </step>

  <step name="return_result">
    Return the path of the written file.
  </step>
</process>
