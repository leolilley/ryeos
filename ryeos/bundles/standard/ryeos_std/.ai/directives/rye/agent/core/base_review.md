<!-- rye:unsigned -->

# Base Review

Operating context for review and analysis threads with read-only file access.

```xml
<directive name="base_review" version="2.0.0" extends="agent/core/base">
  <metadata>
    <description>Rye review context — extends general agent base, read-only file access</description>
    <category>rye/agent/core</category>
    <author>rye-os</author>
    <context>
      <system>rye/agent/core/Identity</system>
      <system>rye/agent/core/Behavior</system>
      <suppress>agent/core/Behavior</suppress>
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
