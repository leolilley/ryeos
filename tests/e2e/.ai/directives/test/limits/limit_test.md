<!-- rye:signed:2026-02-22T02:31:19Z:cdb00a0e36ef21b5e41f102d8f30d25eadb02a9ebcdecfb5d45a27a55b905fdb:KAaWdqRD5mrXgA5lqYrZJDMnFlXO3XUYEu62EYXY_aGWCZFpnHVua20dlDkSeUBpID1OwBOPc5PeuWz0qnaUCw==:9fbfabe975fa5a7f -->
# Limit Test: Turns Limit

Test that the default_escalate_limit hook fires when turns limit is exceeded.

```xml
<directive name="limit_test" version="1.0.0">
  <metadata>
    <description>Test: exceed turns limit — should trigger escalation hook.</description>
    <category>test/limits</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits turns="1" tokens="4096" spend="1.0" />
  </metadata>
  <permissions>
    <execute><tool>rye.file-system.*</tool></execute>
    <execute><tool>rye.primary-tools.*</tool></execute>
  </permissions>
  <process>
    <step name="search_tools">
      <description>Search for available tools — this will use the 1 turn, next iteration should hit limit.</description>
      <search item_type="tool" query="file system" />
    </step>
    <step name="search_again">
      <description>Search again — should never reach this due to limit.</description>
      <search item_type="tool" query="knowledge" />
    </step>
  </process>
  <outputs>
    <success>Should be escalated due to turns limit.</success>
  </outputs>
</directive>
```
