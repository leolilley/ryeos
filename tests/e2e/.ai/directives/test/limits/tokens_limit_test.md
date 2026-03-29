<!-- rye:signed:2026-03-11T07:13:35Z:25c332af547bd9b20bc005b007fe17fd34cd27cfffdd18efd17fad0957c225f3:0UNsQOVfUhpLEBVoObjo-K8zXdBQ_ULDQyUCTLyNFl-tn2ImuIvS0UHwT4F-Vun2nLJZm9TSSX94JqrRxfe3Bw==:4b987fd4e40303ac -->
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
      <fetch>*</fetch>
    </permissions>
  </metadata>

  <outputs>
    <success>Should be escalated due to tokens limit.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_file">
    Write "Should hit token limit" to `tokens_test.txt`.
  </step>
</process>
