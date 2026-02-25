<!-- rye:signed:2026-02-24T23:52:30Z:8c2f436cdc2416826fb17ad5313f11a6f9ecfea4915a27061f02eb3ceae42f28:fgriEvHzH8493K1x2TmsrbsCgpcGoESZNPPUvLEe8IC8IiQZdQqR--blTLwen-GZpyzk5rofK5ihxOo_vM9dBA==:9fbfabe975fa5a7f -->
# Duration Limit Test

Test that the duration_seconds limit triggers escalation. Set duration_seconds=1 so the thread exceeds it during its first LLM call.

```xml
<directive name="duration_limit_test" version="1.0.0">
  <metadata>
    <description>Test: exceed duration limit (1 second) — should trigger escalation hook when elapsed time exceeds 1s.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="10" tokens="100000" spend="1.0" duration_seconds="1" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <search>*</search>
    </permissions>
  </metadata>

  <outputs>
    <success>Should be escalated due to duration_seconds limit.</success>
  </outputs>
</directive>
```

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
