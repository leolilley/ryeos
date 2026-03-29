<!-- rye:signed:2026-03-29T06:39:14Z:794b11b1c52ba6548ce4a8b1cb9a65133bc05d7d0abae7da6f058474b942ad63:oZ5nfiPYjgDO1SoZATaF1yeM-jn8OJbMZLSFFtBrTKk_gafIa5uwxq8VIyXNr_E1gzc9cqz1zHUJfNsJ9D93BQ==:4b987fd4e40303ac -->
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
      <fetch>*</fetch>
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
