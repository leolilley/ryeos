<!-- rye:signed:2026-02-22T02:31:19Z:2301333b029fc812202637f24b0307f9981a32c061274223e246b800e6b5cbbc:NNktllXVUYavjkMjtXGjyj7bB_CogsO6c2TCuBpkF2u-ZGA_OKt-7QOK0EU0I1TFEhVp-HlU2RLz2Yqf94sFDQ==:9fbfabe975fa5a7f -->
# Spend Limit Test

Test that the spend limit triggers escalation. Set spend=$0.001 — even a single haiku turn costs more than that.

```xml
<directive name="spend_limit_test" version="1.0.0">
  <metadata>
    <description>Test: exceed spend limit ($0.001) — should trigger escalation hook after first LLM call.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="10" tokens="100000" spend="0.001" />
    <permissions>
      <cap>rye.execute.tool.rye.file-system.*</cap>
      <cap>rye.search.*</cap>
    </permissions>
  </metadata>

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

  <outputs>
    <success>Should be escalated due to spend limit.</success>
  </outputs>
</directive>
```
