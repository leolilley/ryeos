<!-- rye:signed:2026-02-20T01:09:07Z:1a7df0b55bcaa4da3b371dcb84379ef0d70e3e8a58cff061a302c4e2e4b8696f:ZzzH7XWGvo_OH39lH5JO7KEcaTIlfdwblxssf4X2x3edaHI_KrOZrO1Sbz5RO1IDiz6YCs0mOB1SaQX8rFnICA==:440443d0858f0199 -->
# Write

Write content to a file, creating directories as needed.

```xml
<directive name="write" version="1.0.0">
  <metadata>
    <description>Write content to a file on disk, creating parent directories if they do not exist.</description>
    <category>rye/file-system</category>
    <author>rye-os</author>
    <model tier="fast" />
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
