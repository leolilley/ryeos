<!-- rye:signed:2026-02-18T05:43:37Z:f31c06a5a6b81317e0257e906553073766df418f60564f0c7a5e841706f462e1:SHJ_V8xDS4aw-fW0WGV6RemWcvWDMZmK-3Jjipje973t2ttvx0MfKsdVwmVdz5mcXPl0lPtK8NORctH67NJ3Bg==:440443d0858f0199 -->
# Tokens Limit Test

Test that the tokens limit triggers escalation. Set tokens=500 so the first LLM response exceeds it.

```xml
<directive name="tokens_limit_test" version="1.0.0">
  <metadata>
    <description>Test: exceed tokens limit (500) — should trigger escalation hook after first response.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="10" tokens="500" spend="1.0" />
    <permissions>
      <cap>rye.execute.tool.rye.file-system.*</cap>
      <cap>rye.search.*</cap>
    </permissions>
  </metadata>

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

  <outputs>
    <success>Should be escalated due to tokens limit.</success>
  </outputs>
</directive>
```
