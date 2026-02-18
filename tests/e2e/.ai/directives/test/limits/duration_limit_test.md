<!-- rye:signed:2026-02-18T05:43:37Z:08c5145c8127f3c0c695d2b1c1d04307de3e19192621b170407c45866495636a:n_1xAK7gDO5vvw4RgYRG_hd7FdnrBXftNiEp1JEK8WiQ4H5TK0nxoeru8In9aYfJ0-RXCA_p1dMkk2tdLF_6AA==:440443d0858f0199 -->
# Duration Limit Test

Test that the duration_seconds limit triggers escalation. Set duration_seconds=1 so the thread exceeds it during its first LLM call.

```xml
<directive name="duration_limit_test" version="1.0.0">
  <metadata>
    <description>Test: exceed duration limit (1 second) — should trigger escalation hook when elapsed time exceeds 1s.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="10" tokens="100000" spend="1.0" duration_seconds="1" />
    <permissions>
      <cap>rye.execute.tool.rye.file-system.*</cap>
      <cap>rye.search.*</cap>
    </permissions>
  </metadata>

  <process>
    <step name="write_file">
      <description>Write a file. The LLM call will take more than 1 second, triggering the duration limit.</description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value="duration_test.txt" />
        <param name="content" value="Should hit duration limit" />
        <param name="mode" value="overwrite" />
      </execute>
    </step>
  </process>

  <outputs>
    <success>Should be escalated due to duration_seconds limit.</success>
  </outputs>
</directive>
```
