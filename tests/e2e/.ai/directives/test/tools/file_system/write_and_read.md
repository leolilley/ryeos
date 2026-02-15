<!-- rye:signed:2026-02-13T06:41:22Z:b1ec71f94d27746f2c156b22bd419939633c18caa7e6ab231b895b02fa702b72:PPg4zRV6bYjCPn31nxKQV8TuwCBGxwhExwFjpScXBD9kMGzDNdETnI-jK7ngL1o6b_F3f6Tg4WA1Vu-wD6cyCw==:440443d0858f0199 -->

# Write and Read

Two-step directive that writes content to a file, then reads it back to verify the contents match.

```xml
<directive name="write_and_read" version="1.0.0">
  <metadata>
    <description>Write content to a file then read it back to verify correctness.</description>
    <category>test/tools/file_system</category>
    <author>rye-os</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022" />
    <limits turns="5" tokens="2048" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="content" type="string" required="true">
      The content to write to the file.
    </input>
    <input name="file_path" type="string" required="true">
      The file path to write to and read from.
    </input>
  </inputs>

  <process>
    <step name="write_content">
      <description>Write the provided content to the target file.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="{input:file_path}" />
        <param name="content" value="{input:content}" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>

    <step name="read_and_verify">
      <description>Read the file back and confirm the contents match the original input.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_read">
        <param name="path" value="{input:file_path}" />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Content written to {input:file_path} and verified successfully.</success>
    <failure>Content mismatch detected after write — file contents do not match input.</failure>
  </outputs>
</directive>
```
