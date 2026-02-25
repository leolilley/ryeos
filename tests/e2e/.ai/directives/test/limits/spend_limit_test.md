<!-- rye:signed:2026-02-24T23:52:30Z:2907a3c2d43f3425a914ac3ea8969985ade508e14693f0eb614e9282c06bf87e:krX190SpEuo8pwFe3YBZHpF5lNMWnislLuxYT6EtfrivpFHMVfTeV_hbeEfZ5eFBeGGzkm31T09t-C8TjQ1YDQ==:9fbfabe975fa5a7f -->
# Spend Limit Test

Test that the spend limit triggers escalation. Set spend=$0.001 — even a single haiku turn costs more than that.

```xml
<directive name="spend_limit_test" version="1.0.0">
  <metadata>
    <description>Test: exceed spend limit ($0.001) — should trigger escalation hook after first LLM call.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="10" tokens="100000" spend="0.001" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <search>*</search>
    </permissions>
  </metadata>

  <outputs>
    <success>Should be escalated due to spend limit.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_file">
    <description>Write a file. The LLM call cost will exceed $0.001.</description>
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value="spend_test.txt" />
      <param name="content" value="Should hit spend limit" />
      <param name="mode" value="overwrite" />
    </execute>
  </step>
</process>
