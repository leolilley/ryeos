<!-- rye:signed:2026-03-03T22:32:56Z:955ed75045fac2d54bf1bb4a8e6252b5814a95a96c0cc00f82513fe392d295e4:0eq_ifu7ciFl6iS0iZQJC3lEp2Mg0mvOtg-YcN8HazD0qzMoTq6B0UuNRwg4ep4VMx9kyHvfVO3l0GBcpXTeCw==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

# Base Review

Operating context for review and analysis threads with read-only file access.

```xml
<directive name="base_review" version="1.0.0">
  <metadata>
    <description>Review operating context — search, load, and read-only file access</description>
    <category>rye/agent/core</category>
    <author>rye-os</author>
    <context>
      <system>rye/agent/core/Identity</system>
      <system>rye/agent/core/Behavior</system>
      <before>rye/agent/core/protocol/execute</before>
      <before>rye/agent/core/protocol/search</before>
      <before>rye/agent/core/protocol/load</before>
    </context>
    <permissions>
      <search>*</search>
      <load>*</load>
      <execute>
        <tool>rye.file-system.read</tool>
        <tool>rye.file-system.glob</tool>
        <tool>rye.file-system.grep</tool>
        <knowledge>*</knowledge>
      </execute>
    </permissions>
  </metadata>
</directive>
```
