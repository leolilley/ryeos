<!-- ryeos:signed:2026-03-11T07:13:35Z:2907a3c2d43f3425a914ac3ea8969985ade508e14693f0eb614e9282c06bf87e:L57L7t3o9AavPpk9ab6YZ6XnigvUeWgogO7HyVE_HIAyoHxzXhzqR2kuVt1nDtMl7H7VeHUcg3wpfcLkwt_lCQ==:4b987fd4e40303ac -->
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
      <fetch>*</fetch>
    </permissions>
  </metadata>

  <outputs>
    <success>Should be escalated due to spend limit.</success>
  </outputs>
</directive>
```

<process>
  <step name="write_file">
    Write "Should hit spend limit" to `spend_test.txt`.
  </step>
</process>
