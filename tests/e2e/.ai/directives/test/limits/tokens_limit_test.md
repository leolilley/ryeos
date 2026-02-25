<!-- rye:signed:2026-02-24T23:52:30Z:25c332af547bd9b20bc005b007fe17fd34cd27cfffdd18efd17fad0957c225f3:k0Bz-y4Hkpl7dfZ5rMDdbMihIWClF8nqsOxUH_s3l4gGwPoa2Zg-_lQLU9EqZAMVIi-UVtLtQfAiLc5pum-sAA==:9fbfabe975fa5a7f -->
# Tokens Limit Test

Test that the tokens limit triggers escalation. Set tokens=500 so the first LLM response exceeds it.

```xml
<directive name="tokens_limit_test" version="1.0.0">
  <metadata>
    <description>Test: exceed tokens limit (500) — should trigger escalation hook after first response.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="10" tokens="500" spend="1.0" />
    <permissions>
      <execute><tool>rye.file-system.*</tool></execute>
      <search>*</search>
    </permissions>
  </metadata>

  <outputs>
    <success>Should be escalated due to tokens limit.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_file">
    <description>Write a file. The LLM prompt + response tokens will exceed 500 total.</description>
    <execute item_type="tool" item_id="rye/file-system/fs_write">
      <param name="path" value="tokens_test.txt" />
      <param name="content" value="Should hit token limit" />
      <param name="mode" value="overwrite" />
    </execute>
  </step>
</process>
