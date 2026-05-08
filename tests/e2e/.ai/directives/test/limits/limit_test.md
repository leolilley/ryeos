<!-- ryeos:signed:2026-03-11T07:13:35Z:3e59e048e26719e1abc1167159bfdc3c9065f853e1c0ba5f5a4be99a44aa6a18:4E7UAhzejcXv3uv80aPRWHiS8ye5IawVmacFGhKQjbbJLQ-nwyUrzq7pEs2Uwk7QbwjDSeL6o56WBSBLXQJGAQ==:4b987fd4e40303ac -->
# Limit Test: Turns Limit

Test that the default_escalate_limit hook fires when turns limit is exceeded.

```xml
<directive name="limit_test" version="1.0.0">
  <metadata>
    <description>Test: exceed turns limit — should trigger escalation hook.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="1" tokens="4096" spend="1.0" />
  </metadata>
  <permissions>
    <execute><tool>rye.file-system.*</tool></execute>
    <execute><tool>rye.primary-actions.*</tool></execute>
  </permissions>
  <outputs>
    <success>Should be escalated due to turns limit.</success>
  </outputs>
</directive>
```

<process>
  <step name="search_tools">
    <description>Search for available tools — this will use the 1 turn, next iteration should hit limit.</description>
    <fetch scope="tool" query="file system" />
  </step>
  <step name="search_again">
    <description>Search again — should never reach this due to limit.</description>
    <fetch scope="tool" query="knowledge" />
  </step>
</process>
