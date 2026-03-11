<!-- rye:signed:2026-03-11T07:13:35Z:87e1fd43f73a0f0a8e8933c7a1e436512c6039026beb53eaffb81573e3cd654b:n4IQ2qteaUdvMFHaJrm9ucbSAVD8ihmpqVHmYZeawuicwFG9Duc2sFs3D5CQsDmb-puOE1SQV6IIWfC3mhLxCA==:4b987fd4e40303ac -->
# Limit Inheritance Test

Test that parent limits constrain child threads. Parent sets turns=3, spend=0.20 — child directive declares turns=25 but should be capped to parent's turns=3.

```xml
<directive name="limit_inheritance_test" version="1.0.0">
  <metadata>
    <description>Test: parent limits (turns=3, spend=0.20) constrain child. Child declares turns=25 but should be capped to 3.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" spend="0.20" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <execute><tool>rye/agent/threads/thread_directive</tool></execute>
    </permissions>
  </metadata>

  <outputs>
    <success>Child ran within parent's limit bounds. Check thread.json for child's resolved limits.</success>
  </outputs>
</directive>
```

<process>
  <step name="spawn_child_with_high_limits">
    <description>Spawn test/tools/file_system/write_file which has turns=3 in its own limits. Parent's turns=3 and spend=0.20 should be the upper bound.</description>
    <execute item_type="tool" item_id="rye/agent/threads/thread_directive">
      <param name="directive_name" value="test/tools/file_system/write_file" />
      <param name="inputs" value='{"message": "Child with inherited limits", "output_path": "limit_inherited.txt"}' />
    </execute>
  </step>
</process>
