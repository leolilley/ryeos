<!-- rye:signed:2026-02-10T02:00:00Z:placeholder:unsigned:unsigned -->
# Write

Write content to a file, creating directories as needed.

```xml
<directive name="write" version="1.0.0">
  <metadata>
    <description>Write content to a file on disk, creating parent directories if they do not exist.</description>
    <category>rye/file-system</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="2048" />
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
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"file_path": "{input:file_path}", "content": "{input:content}"})`
  </step>

  <step name="return_result">
    Return the path of the written file.
  </step>
</process>
