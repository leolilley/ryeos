<!-- rye:signed:2026-02-22T02:31:19Z:8051ba10f78ba72757633167554d5730d91ee1e41726b52a6181fac53683a6f3:tNzkS2DKLnSPB7ziIgL_PvLOjYPYy8C6P42htU2kxjfqE_eHPdYILEiA8JoFF1O_dE_Ae7OMUH4mKOWqXeDyAA==:9fbfabe975fa5a7f -->
# Child Write

Simple child directive — writes a message to a file, then reads it back to confirm.

```xml
<directive name="child_write" version="1.0.0">
  <metadata>
    <description>Write a message to a file and read it back to verify. Used as a child thread target.</description>
    <category>test/tools/file_system</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="16000" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="message" type="string" required="true">
      The message to write.
    </input>
    <input name="file_path" type="string" required="true">
      Relative file path to write to (within the project workspace).
    </input>
  </inputs>

  <outputs>
    <output name="path">Path to the written file</output>
    <output name="content">The content that was verified from reading the file back</output>
  </outputs>
</directive>
```

<process>
  <step name="write">
    Write the message to the file:
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": "{input:file_path}", "content": "{input:message}"})`
  </step>

  <step name="verify">
    Read the file back to confirm it was written correctly:
    `rye_execute(item_type="tool", item_id="rye/file-system/read", parameters={"path": "{input:file_path}"})`
  </step>

  <step name="return_result">
    Return the path and the verified content using directive_return.
  </step>
</process>
