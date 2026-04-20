<!-- rye:signed:2026-04-19T09:49:53Z:ec9bef55d3a15031f2acd50ef433587574c97e85347781f13a7a792a26da23ce:qifpBgH9YR1fSVnqUU32WMXr7tCGb5o03OZ+97ADz6L2yl/3jVl9hrdjFvI8yq1OiEOtbUQgRjK7Ga7EOevmCg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
<!-- rye:unsigned -->

# Base Execute Only

Narrow operating context for threads that only need to execute tools.

```xml
<directive name="base_execute_only" version="2.0.0" extends="agent/core/base">
  <metadata>
    <description>Narrow Rye agent context — extends general agent base, execute only</description>
    <category>rye/agent/core</category>
    <author>rye-os</author>
    <context>
      <system>rye/agent/core/Identity</system>
      <system>rye/agent/core/Behavior</system>
      <suppress>agent/core/Behavior</suppress>
    </context>
    <permissions>
      <execute>*</execute>
    </permissions>
  </metadata>
</directive>
```
