<!-- rye:signed:2026-02-13T08:38:46Z:72817ea89cded6c2674744adbb2d33066d23650d79abdbfbe94a6ecb7046ad81:SawrkMPgnzo9IF-lzaskSpTON-QueqwcRcz7eZVZZoGIlnksjuUJStddztred5HMT8_jvCLlRbq88HNpgyHRAg==:440443d0858f0199 -->
# Limit Inheritance Test

Test that parent limits constrain child threads. Parent sets turns=3, spend=0.20 — child directive declares turns=25 but should be capped to parent's turns=3.

```xml
<directive name="limit_inheritance_test" version="1.0.0">
  <metadata>
    <description>Test: parent limits (turns=3, spend=0.20) constrain child. Child declares turns=25 but should be capped to 3.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="3" tokens="4096" spend="0.20" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <execute><tool>rye/agent/threads/thread_directive</tool></execute>
    </permissions>
  </metadata>

  <process>
    <step name="spawn_child_with_high_limits">
      <description>Spawn test/tools/file_system/write_file which has turns=3 in its own limits. Parent's turns=3 and spend=0.20 should be the upper bound.</description>
      <execute item_type="tool" item_id="rye/agent/threads/thread_directive">
        <param name="directive_name" value="test/tools/file_system/write_file" />
        <param name="inputs" value='{"message": "Child with inherited limits", "output_path": "limit_inherited.txt"}' />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Child ran within parent's limit bounds. Check thread.json for child's resolved limits.</success>
  </outputs>
</directive>
```
