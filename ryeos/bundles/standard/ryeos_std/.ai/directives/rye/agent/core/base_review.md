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
