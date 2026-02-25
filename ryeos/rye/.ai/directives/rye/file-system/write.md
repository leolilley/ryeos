<!-- rye:signed:2026-02-25T07:50:41Z:db51589dc0801e8aab588ee14dfa9533de7eb1d5d91065b10f8035a4c82bc194:Y74_VXJGej7JhEU4WySz1E7drysriU6-0YPDFFCBbbEAq6mGLyOwlL9EZfas5iZK8O1H9E6d5ZkTXBR8UjiqDQ==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:1a7df0b55bcaa4da3b371dcb84379ef0d70e3e8a58cff061a302c4e2e4b8696f:zU1Gypt7LtU5vsQsottmStN7HGdEPCmjqLFB_PhWU9ACvunN7sGmLzDBQxYY88qa8IwBPKK--qTqdXSMkNB4CA==:9fbfabe975fa5a7f -->
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
