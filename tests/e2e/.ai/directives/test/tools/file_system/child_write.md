<!-- rye:signed:2026-02-13T07:14:13Z:e9902392d6bd947c18e46203c18e8578fef58d022c8b61bb05fedfb6f1d8962b:MM1rcFvg_WxtUw6y-zVdtlbGitEoF0WL1OfyBHtjoSTL_-CrvkU0-5OVyu6I9lf5cFvRbIrJmhMRPxptEseJCg==:440443d0858f0199 -->

# Child Write

Simple child directive — writes a message to a file, then reads it back to confirm.

```xml
<directive name="child_write" version="1.0.0">
  <metadata>
    <description>Write a message to a file and read it back to verify. Used as a child thread target.</description>
    <category>test/tools/file_system</category>
    <author>rye-os</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022" />
    <limits turns="4" tokens="2048" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="message" type="string" required="true">
      The message to write.
    </input>
    <input name="file_path" type="string" required="true">
      The file path to write to.
    </input>
  </inputs>

  <process>
    <step name="write">
      <description>Write the message to the file.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="{input:file_path}" />
        <param name="content" value="{input:message}" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>

    <step name="verify">
      <description>Read the file back to confirm it was written correctly.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_read">
        <param name="path" value="{input:file_path}" />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Message written to {input:file_path} and verified.</success>
  </outputs>
</directive>
```
