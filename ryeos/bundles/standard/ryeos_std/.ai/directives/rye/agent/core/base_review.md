<!-- rye:signed:2026-03-16T11:23:45Z:755b18aee31900a06930f2200ce66e5e64a7bafeb5ccbb7f5d5759b6732e2ada:fX3lNDEyE8tVpuhUcIrnMVZUD6CC1-bgOFS3SPUMpLbr2vjXXrhjKjjdCYxQ9AgsVs_HJZsy_fcipLPvJ4uuBg==:4b987fd4e40303ac -->
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
