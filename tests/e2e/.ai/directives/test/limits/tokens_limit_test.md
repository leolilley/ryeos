<!-- rye:signed:2026-02-22T02:31:19Z:f31c06a5a6b81317e0257e906553073766df418f60564f0c7a5e841706f462e1:RtQwvnYk-ahDlWB_bwLVZWqLWECanjigiKE4b3TfQMM4t4okQhPfa0kbCp7M7_u3571OPw1ynlj6NR0KFf-wCA==:9fbfabe975fa5a7f -->
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
      <cap>rye.execute.tool.rye.file-system.*</cap>
      <cap>rye.search.*</cap>
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
