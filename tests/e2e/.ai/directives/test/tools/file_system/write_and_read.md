<!-- rye:signed:2026-02-18T05:43:37Z:3b7a7313ded20331f5271da19f03c83d84c2b6a00a14b7943fee80b904864179:Aj0y_NvcEz-aNrNDnWqp-k2X9pGiJEBRHH2ujUvIuAzoKFQ0P3-n884-09O9sCrgKMFaIaE3ueYXqxRks-FRAw==:440443d0858f0199 -->

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
