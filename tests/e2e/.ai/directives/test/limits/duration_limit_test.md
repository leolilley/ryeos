<!-- ryeos:signed:2026-03-11T07:13:35Z:8c2f436cdc2416826fb17ad5313f11a6f9ecfea4915a27061f02eb3ceae42f28:a0lmqRtHbxg00XKkKJH67xtObZi10KGwy1YBrEolX3BksRmOLQcyAoINNzm_7aBnGadnjPzUboCDVuH_t-KeBQ==:4b987fd4e40303ac -->
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
      <fetch>*</fetch>
    </permissions>
  </metadata>

  <outputs>
    <success>Should be escalated due to duration_seconds limit.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_file">
    Write "Should hit duration limit" to `duration_test.txt`.
  </step>
</process>
