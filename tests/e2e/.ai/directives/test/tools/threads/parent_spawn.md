<!-- rye:signed:2026-02-13T07:14:12Z:552356eba0a586608dda99ab522e88223babd48977927f67c241a88595c30b10:4uam-JJlg0WraV4P9sXep09EEHWbuO_susjEfHnREOPTig5xZdOjmg1Gu7p70-WV2-dRM7HmLG3UdtOnJZjACQ==:440443d0858f0199 -->

# Parent Spawn

Parent directive that writes its own file, then spawns a child thread to write a second file. Tests recursive thread spawning — should produce two thread folders.

```xml
<directive name="parent_spawn" version="1.0.0">
  <metadata>
    <description>Write a parent log file, then spawn a child thread (test/tools/file_system/child_write) to write a second file. Verifies both files exist.</description>
    <category>test</category>
    <author>rye-os</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022" />
    <limits turns="6" tokens="4096" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <execute><tool>rye/agent/threads/thread_directive</tool></execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="parent_message" type="string" required="true">
      Message the parent writes to its own file.
    </input>
    <input name="child_message" type="string" required="true">
      Message the child thread writes to its file.
    </input>
  </inputs>

  <process>
    <step name="parent_write">
      <description>Write the parent's message to parent_output.md</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="parent_output.md" />
        <param name="content" value="{input:parent_message}" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>

    <step name="spawn_child">
      <description>Spawn a child thread running test/tools/file_system/child_write to write child_output.md. Use rye_execute with item_type=tool, item_id=rye/agent/threads/thread_directive, and parameters containing directive_name and inputs.</description>
      <execute item_type="tool" item_id="rye/agent/threads/thread_directive">
        <param name="directive_name" value="test/tools/file_system/child_write" />
        <param name="inputs" value='{"message": "{input:child_message}", "file_path": "child_output.md"}' />
      </execute>
    </step>

    <step name="verify_parent">
      <description>Read back the parent output file to confirm it was written.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_read">
        <param name="path" value="parent_output.md" />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Parent wrote parent_output.md and child thread wrote child_output.md. Two thread folders created.</success>
  </outputs>
</directive>
```
