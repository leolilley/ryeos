<!-- rye:signed:2026-04-19T09:49:53Z:af70f74ba10be342135b6b1abda2c39719398621002e9840063f7872745a6b0d:ozoUYbEAYAIAA+/MXWGsXyIf7D2yFBe5otJZ1nz4QtJvpfE/4qzG5LNG4j/ydc3JTV4FDlbxVCcubE8nyfdqAw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
    `rye_execute(item_id="rye/file-system/write", parameters={"path": "{input:file_path}", "content": "{input:content}"})`
  </step>

  <step name="return_result">
    Return the path of the written file.
  </step>
</process>
